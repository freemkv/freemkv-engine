//! ddrescue-compatible mapfile for tracking rip progress.
//!
//! Records which byte ranges of a disc image are good, unreadable,
//! or not-yet-attempted. Written as plain text so it's greppable,
//! human-editable, and interoperates with ddrescue's own tools.
//!
//! Format:
//! ```text
//! # Rescue Logfile. Created by freemkv-engine vX.Y.Z
//! # Current pos / status / pass / pass_time
//! 0x000000000  ?  1  0
//! #      pos        size  status
//! 0x000000000  0x12345678    +
//! 0x012345678  0x00001000    -
//! 0x012346678  0x01234500    ?
//! ```
//!
//! The current-position line is ddrescue's state line; freemkv writes it as a fixed
//! `0x000000000  ?  1  0` and ignores it on load.
//!
//! Status chars: `?` non-tried · `*` non-trimmed · `/` non-scraped · `-` unreadable · `+` finished.
//!
//! Persisted to disk in time-batched intervals; see [`FLUSH_INTERVAL`] and
//! [`Mapfile`] for the flush policy.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

// Minimum interval between mapfile persists (else on `flush()`/`Drop`).
// Bounds atomic-rename RPC rate on NFS staging; worst-case crash loss is
// one interval's worth of records.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1000);

// Load refuses a larger file (MapfileInvalid "too_large"): ~8M runs at ~32 bytes a line, far
// past any real damage map, so a wrong or planted file fails fast instead of filling memory.
const MAX_MAPFILE_BYTES: u64 = 256 << 20;

/// Mapfile path for a regular output file: appends `.mapfile` to the output
/// path.
///
/// The engine owns mapfiles now, so it owns the naming rule too. libfreemkv
/// keeps its own copy of this rule private (`pub(crate)`) purely to back
/// [`libfreemkv::Disc::mapfile_for`], which additionally special-cases
/// `/dev/null` (benchmark output) to a temp-dir path derived from the disc
/// title. Callers that hold a `Disc` should prefer `Disc::mapfile_for`; this
/// is the plain-path rule for callers that only have an output path.
pub fn mapfile_path_for(iso_path: &Path) -> PathBuf {
    let mut s = iso_path.as_os_str().to_os_string();
    s.push(".mapfile");
    PathBuf::from(s)
}

/// Status of a byte range in the mapfile. ddrescue-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectorStatus {
    /// `?` — not yet attempted. Initial state for a fresh mapfile.
    NonTried,
    /// `*` — fast-pass read failed; edges need trimming.
    NonTrimmed,
    /// `/` — trimmed; interior needs sector scrape.
    NonScraped,
    /// `-` — drive couldn't read it this session.
    Unreadable,
    /// `+` — good.
    Finished,
}

impl SectorStatus {
    /// THE definition of "these bytes are confirmed good".
    ///
    /// Exhaustive on purpose: adding a sixth variant is a compile error here. The status
    /// sets below are checked against it over every status character by a unit test.
    pub fn is_finished(self) -> bool {
        match self {
            SectorStatus::Finished => true,
            SectorStatus::NonTried
            | SectorStatus::NonTrimmed
            | SectorStatus::NonScraped
            | SectorStatus::Unreadable => false,
        }
    }
}

/// Every status that is NOT [`SectorStatus::Finished`] — "not confirmed good".
/// This is the convergence set: what the multipass loop treats as still
/// unfinished, and what a front-end's loss report must count as bad.
///
/// Sibling of [`damage_sector_statuses`]; the difference is `NonTried`, and it
/// matters. Both live here so the distinction is visible at every call site
/// instead of being re-derived from a comment.
pub fn bad_sector_statuses() -> [SectorStatus; 4] {
    [
        SectorStatus::NonTried,
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
    ]
}

/// The DAMAGE set: attempted and failed. Excludes `NonTried`, which is the
/// unread remainder rather than damage — counting it would report a whole
/// unswept disc as confirmed loss.
pub fn damage_sector_statuses() -> [SectorStatus; 3] {
    [
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
    ]
}

impl SectorStatus {
    /// The single ddrescue status character for this status
    /// (`?`/`*`/`/`/`-`/`+`).
    pub fn to_char(self) -> char {
        match self {
            Self::NonTried => '?',
            Self::NonTrimmed => '*',
            Self::NonScraped => '/',
            Self::Unreadable => '-',
            Self::Finished => '+',
        }
    }
    /// Parse a ddrescue status character into a `SectorStatus`. Returns
    /// `None` for any character that is not one of `?*/-+`.
    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => Self::NonTried,
            '*' => Self::NonTrimmed,
            '/' => Self::NonScraped,
            '-' => Self::Unreadable,
            '+' => Self::Finished,
            _ => return None,
        })
    }
}

/// One contiguous range of bytes with a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MapEntry {
    pub pos: u64,
    pub size: u64,
    pub status: SectorStatus,
}

/// Summary statistics over all entries.
///
/// `bytes_pending` aggregates `NonTried + NonTrimmed + NonScraped` for
/// back-compat. `bytes_nontried` and `bytes_retryable` (= NonTrimmed +
/// NonScraped) split that aggregate so UIs can distinguish *unread*
/// territory (still ahead of Pass 1's read head) from *needs-retry*
/// territory (Pass 1 already encountered, queued for Pass 2-N).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MapStats {
    pub bytes_total: u64,
    pub bytes_good: u64,
    pub bytes_unreadable: u64,
    pub bytes_pending: u64,
    /// Sectors Pass 1 hasn't reached yet (`NonTried`). Subset of
    /// `bytes_pending`.
    pub bytes_nontried: u64,
    /// Sectors flagged for Pass 2-N retry — `NonTrimmed` (multi-sector
    /// read failed; needs split) + `NonScraped` (small-block read
    /// partially recovered; remainder still pending). Subset of
    /// `bytes_pending`. This is the right signal for a "MAYBE / will
    /// retry" UI bucket; `bytes_pending` over-counts because it folds
    /// in `bytes_nontried`.
    pub bytes_retryable: u64,
    /// Number of distinct `Unreadable` ranges (for UI display).
    /// Computed by `compute_stats` (counts coalesced `-` entries).
    pub num_bad_ranges: u32,
    /// Always `0.0`: the engine never computes this (it needs a bitrate the map lacks).
    /// The lost-time figure is `MultipassResult::main_lost_ms`; kept for API compatibility.
    pub main_lost_ms: f64,
}

// Revokes a `Mapfile`'s right to write its path, so an ABANDONED writer thread (see
// `super::finish_bounded`) can't rewrite the file from a stale snapshot after a resume.
#[derive(Clone)]
pub(crate) struct MapfileDisown(Arc<AtomicBool>);

impl MapfileDisown {
    /// Revoke the mapfile's right to write. Idempotent, and safe to call
    /// from a thread other than the one that owns the `Mapfile` — that is
    /// the entire point.
    pub(crate) fn disown(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Time-batched mapfile. `record()` keeps in-memory state up-to-date on
/// every call; persists to disk at most once per `FLUSH_INTERVAL`.
/// Explicit `flush()` and `Drop` guarantee state is on disk after a sweep
/// or patch finishes. On hard crash the worst-case loss is one flush
/// interval of records — the file's payload bytes are unaffected.
pub struct Mapfile {
    path: PathBuf,
    /// The CANONICAL maximal-run partition of `[0, total_size)`: contiguous, gapless, and
    /// (after any `record()`) with no two adjacent entries sharing a status. Deliberately
    /// uncapped — that invariant IS the bound, and the length is exactly the number of status
    /// runs the disc's damage actually has (it shrinks as damage is recovered, not just grows).
    /// `record()` is O(log entries + touched runs), plus a tail shift when the run count changes.
    entries: Vec<MapEntry>,
    total_size: u64,
    version: String,
    /// Incrementally maintained stats — updated on every `record()` call
    /// so `stats()` is O(1) instead of O(n).
    stats: MapStats,
    /// True when in-memory state has changed but `write_to_disk` has not
    /// yet captured it.
    dirty: bool,
    /// Wall-clock timestamp of the last successful `write_to_disk` (or
    /// the moment the mapfile was constructed, whichever is later).
    last_flushed: Instant,
    /// SHA-1 of the disc's `Unit_Key_RO.inf` (40 lowercase hex), `# freemkv-disc:`: the
    /// disc's identity (KU §4.1). `None` for a non-AACS disc or an anonymous import.
    disc_hash: Option<String>,
    /// [`vid_fingerprint`] of the disc's Volume ID, `# freemkv-vidfp:`. The raw VID is
    /// never held or written (KU J6: memory only; it derives the keys, KS-16).
    vidfp: Option<[u8; 32]>,
    /// [`key_fingerprint`]s of the keys a pre-1.8 mapfile stored in its legacy key lines, kept as
    /// `# freemkv-legacy-keyfp:` so an old capture keeps an identity (KU §4.1, decision b).
    legacy_keyfps: Vec<[u8; 8]>,
    /// Byte ranges a SCOPED (MKV-staging) image was read over, persisted as a
    /// `# freemkv-scope:` header. Outside them nothing is read: the file is not a
    /// whole-disc image, and `stats()` leaves those unread bytes out of pending.
    scope: Option<Vec<(u64, u64)>>,
    /// Whether the image holds raw (`true`) or decrypted (`false`) sectors, `# freemkv-raw:`.
    /// `None` = unknown (written before the stamp existed): a resume cannot prove its mode.
    raw: Option<bool>,
    /// Raised through a [`MapfileDisown`] handle when this mapfile's owner
    /// has been abandoned; once set, no further write reaches the path. See
    /// [`MapfileDisown`].
    disowned: Arc<AtomicBool>,
}

impl std::fmt::Debug for Mapfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mapfile")
            .field("path", &self.path)
            .field("total_size", &self.total_size)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Mapfile {
    /// Create a new mapfile with one `NonTried` region covering the whole disc.
    /// Writes to disk immediately so a resume can pick up even if the caller
    /// never records anything.
    pub fn create(path: &Path, total_size: u64, version: &str) -> io::Result<Self> {
        let mut mf = Self {
            path: path.to_path_buf(),
            // No entry for an empty image: load() refuses a zero-size entry.
            entries: (total_size > 0)
                .then_some(MapEntry {
                    pos: 0,
                    size: total_size,
                    status: SectorStatus::NonTried,
                })
                .into_iter()
                .collect(),
            total_size,
            version: version.to_string(),
            stats: MapStats {
                bytes_total: total_size,
                bytes_pending: total_size,
                bytes_nontried: total_size,
                ..Default::default()
            },
            dirty: false,
            last_flushed: Instant::now(),
            disc_hash: None,
            vidfp: None,
            legacy_keyfps: Vec::new(),
            scope: None,
            raw: None,
            disowned: Arc::new(AtomicBool::new(false)),
        };
        // Eager initial persist so a resume can pick this up even if
        // `record()` is never called.
        mf.write_to_disk()?;
        mf.last_flushed = Instant::now();
        Ok(mf)
    }

    /// Load an existing mapfile from disk.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = read_capped(path)?;
        let mut entries = Vec::new();
        let mut saw_current_line = false;
        let mut version = String::from("unknown");
        let mut disc_hash: Option<String> = None;
        let mut vidfp: Option<[u8; 32]> = None;
        let mut legacy = LegacyIdentity::default();
        let mut scope: Option<Vec<(u64, u64)>> = None;
        let mut raw: Option<bool> = None;
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(rest) = t.strip_prefix('#') {
                let rest = rest.trim();
                if let Some(v) = rest.strip_prefix("Rescue Logfile. Created by ") {
                    version = v.to_string();
                }
                // The identity headers are not best-effort: dropping a malformed one
                // downgrades to "no identity", letting disc A's ranges apply to disc B.
                if let Some(h) = rest.strip_prefix("freemkv-disc:") {
                    disc_hash = Some(parse_disc_hash(h.trim()).ok_or_else(|| invalid("disc"))?);
                }
                if let Some(h) = rest.strip_prefix("freemkv-vidfp:") {
                    let fp = libfreemkv::hex::parse_hex_fixed::<32>(h.trim());
                    vidfp = Some(fp.ok_or_else(|| invalid("vidfp"))?);
                }
                if let Some(h) = rest.strip_prefix("freemkv-legacy-keyfp:") {
                    let fp = libfreemkv::hex::parse_hex_fixed::<8>(h.trim());
                    legacy.keyfps.push(fp.ok_or_else(|| invalid("keyfp"))?);
                }
                parse_legacy_key_lines(rest, &mut legacy)?;
                if let Some(r) = rest.strip_prefix("freemkv-raw:") {
                    raw = Some(match r.trim() {
                        "1" => true,
                        "0" => false,
                        _ => return Err(invalid("raw")),
                    });
                }
                // Like the identity headers, a malformed scope is refused: dropping it
                // would present a partial image as a whole-disc one.
                if let Some(sc) = rest.strip_prefix("freemkv-scope:") {
                    let Some(ranges) = parse_scope(sc.trim()) else {
                        return Err(invalid("scope"));
                    };
                    scope = Some(ranges);
                }
                continue;
            }
            // First non-comment line is the "current" state line
            // (`pos status [pass] [pass_time]`). We ignore its contents but
            // skip over it.
            if !saw_current_line {
                saw_current_line = true;
                // Discriminate by ddrescue's line shape, not a `0x`-prefix heuristic
                // (that dropped a data line whose size lacked `0x`). A current line's
                // 2nd field is a single status char; a data line's is the hex size.
                let fields: Vec<&str> = t.split_whitespace().collect();
                if is_current_line(&fields) {
                    continue;
                }
                // Otherwise it's a data line — fall through to entry parse.
            }
            // Entry: `pos size statuschar`
            let fields: Vec<&str> = t.split_whitespace().collect();
            if fields.len() < 3 {
                // A short data line is dropped coverage, not noise: skipping it deletes
                // its range from the gapless [0, total_size) partition, and skipping
                // the last one shrinks total_size itself. Reject rather than skip.
                return Err(invalid("short_line"));
            }
            if fields.len() > 3 {
                return Err(invalid("long_line"));
            }
            let pos = parse_hex(fields[0])?;
            let size = parse_hex(fields[1])?;
            // Reject pos+size overflow up front: downstream overlap/coalesce/next_with
            // code adds pos+size freely, and a crafted line would otherwise panic
            // (debug) or wrap to a tiny range (release), corrupting stats/resume.
            if pos.checked_add(size).is_none() {
                return Err(invalid("range"));
            }
            // A zero-size entry is degenerate: it contributes nothing to the
            // partition yet trips overlap/coalesce arithmetic (two entries can
            // share the same pos). Reject it rather than carry it through.
            if size == 0 {
                return Err(invalid("zero_size"));
            }
            // The whole token, not its first char: `+garbage` is damage, not Finished.
            let status = single_char(fields[2])
                .and_then(SectorStatus::from_char)
                .ok_or_else(|| invalid("status_char"))?;
            entries.push(MapEntry { pos, size, status });
        }
        entries.sort_by_key(|e| e.pos);
        // Reject overlapping ranges (would make compute_stats double-count and
        // inflate resume decisions), then coalesce-fill internal gaps as NonTried
        // so a holed mapfile can't pass as falsely "complete", without stranding partials.
        let mut filled: Vec<MapEntry> = Vec::with_capacity(entries.len() + 1);
        let mut cursor: u64 = 0;
        for e in entries {
            if e.pos < cursor {
                return Err(invalid("overlap"));
            }
            if e.pos > cursor {
                // Leading or internal gap — fill it as NonTried.
                push_coalesced(
                    &mut filled,
                    MapEntry {
                        pos: cursor,
                        size: e.pos - cursor,
                        status: SectorStatus::NonTried,
                    },
                );
            }
            cursor = e.pos.saturating_add(e.size);
            // Coalesced so a loaded map meets the maximal-run invariant `record()` relies on.
            push_coalesced(&mut filled, e);
        }
        let entries = filled;
        let total_size = entries
            .last()
            .map(|e| e.pos.saturating_add(e.size))
            .unwrap_or(0);
        let stats = Self::compute_stats(&entries, total_size);
        Ok(Self {
            path: path.to_path_buf(),
            entries,
            total_size,
            version,
            stats,
            dirty: false,
            last_flushed: Instant::now(),
            disc_hash,
            vidfp: vidfp.or(legacy.vidfp),
            legacy_keyfps: legacy.keyfps,
            scope,
            raw,
            disowned: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Load if the file exists, otherwise create a fresh mapfile.
    pub fn open_or_create(path: &Path, total_size: u64, version: &str) -> io::Result<Self> {
        match Self::load(path) {
            Ok(mf) => {
                // load() derives total_size from the last entry's pos+size; if that
                // disagrees with the caller's size, resume math keys off the wrong
                // basis. Warn only: sweep() already forces a fresh sweep on a mismatch.
                if mf.total_size != total_size {
                    tracing::warn!(
                        target: "freemkv::disc",
                        phase = "mapfile_total_size_mismatch",
                        loaded_total = mf.total_size,
                        supplied_total = total_size,
                        path = %path.display(),
                        "loaded mapfile coverage differs from supplied disc size"
                    );
                }
                Ok(mf)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Self::create(path, total_size, version)
            }
            Err(e) => Err(e),
        }
    }

    /// Mark a byte range as having the given status. Splits any overlapping
    /// existing entries, merges with adjacent same-status entries, and flushes
    /// to disk once `FLUSH_INTERVAL` has elapsed since the last persist (see
    /// `flush()`/`Drop` for guaranteed durability).
    pub fn record(&mut self, pos: u64, size: u64, status: SectorStatus) -> io::Result<()> {
        if size == 0 {
            return Ok(());
        }
        // Mirror load()'s overflow contract: reject a range that would wrap u64
        // rather than storing a saturated entry, which load() would then reject
        // on the next resume (making the mapfile unreadable).
        let Some(end) = pos.checked_add(size) else {
            return Err(invalid("range"));
        };
        self.splice(pos, end, status);
        self.changed()
    }

    /// Re-mark every run whose status is in `from` as `to`, in one O(entries) pass: the same
    /// map as calling [`Mapfile::record`] over each such range (the end-of-recovery promotion),
    /// without the per-range cost. Flushes like `record()`.
    pub fn promote(&mut self, from: &[SectorStatus], to: SectorStatus) -> io::Result<()> {
        let mut hit = false;
        let mut merged: Vec<MapEntry> = Vec::with_capacity(self.entries.len());
        for mut e in self.entries.drain(..) {
            if from.contains(&e.status) {
                e.status = to;
                hit = true;
            }
            push_coalesced(&mut merged, e);
        }
        self.entries = merged;
        if !hit {
            return Ok(());
        }
        self.stats = Self::compute_stats(&self.entries, self.total_size);
        self.changed()
    }

    /// Whether the next [`Mapfile::record`] persists the map: a consumer makes the image's
    /// data durable first, so the saved map never marks unsynced sectors Finished.
    pub(crate) fn persist_due(&self) -> bool {
        self.last_flushed.elapsed() >= FLUSH_INTERVAL
    }

    // Marks in-memory state dirty and persists it once `FLUSH_INTERVAL` has elapsed.
    fn changed(&mut self) -> io::Result<()> {
        self.dirty = true;
        if self.last_flushed.elapsed() >= FLUSH_INTERVAL {
            self.write_to_disk()?;
            self.dirty = false;
            self.last_flushed = Instant::now();
        }
        Ok(())
    }

    /// A handle that revokes this mapfile's right to write its path. Take
    /// one BEFORE the mapfile moves into a consumer thread; see
    /// [`MapfileDisown`] and [`super::finish_bounded_disowning`].
    pub(crate) fn disown_handle(&self) -> MapfileDisown {
        MapfileDisown(Arc::clone(&self.disowned))
    }

    /// Whether a [`MapfileDisown`] handle has revoked this mapfile: its owner was
    /// abandoned and must touch neither this file nor the image it describes.
    pub(crate) fn is_disowned(&self) -> bool {
        self.disowned.load(Ordering::Acquire)
    }

    /// Persist any pending in-memory changes to disk. No-op if clean.
    /// Callers (sweep/patch finalisation) invoke this after their last
    /// `record()` to guarantee state is durable before returning.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.dirty {
            self.write_to_disk()?;
            self.dirty = false;
            self.last_flushed = Instant::now();
        }
        Ok(())
    }

    /// Record the disc hash (SHA-1 of `Unit_Key_RO.inf`, `0x` optional), written as
    /// `# freemkv-disc:`. A value that is not 40 hex digits is ignored.
    pub fn set_disc_hash(&mut self, hash: &str) {
        let h = parse_disc_hash(hash);
        if h.is_some() && h != self.disc_hash {
            self.disc_hash = h;
            self.dirty = true;
        }
    }

    /// The disc hash this mapfile names (40 lowercase hex), if any.
    pub fn disc_hash(&self) -> Option<&str> {
        self.disc_hash.as_deref()
    }

    /// Record the disc's Volume ID fingerprint (`# freemkv-vidfp:`, KU §4.1).
    pub fn set_vid_fingerprint(&mut self, fp: [u8; 32]) {
        if self.vidfp != Some(fp) {
            self.vidfp = Some(fp);
            self.dirty = true;
        }
    }

    /// The disc's Volume ID fingerprint (a converted legacy raw VID line included).
    pub fn vid_fingerprint(&self) -> Option<[u8; 32]> {
        self.vidfp
    }

    /// Record whether the image holds raw or decrypted sectors (`# freemkv-raw:`).
    pub(crate) fn set_raw(&mut self, raw: bool) {
        if self.raw != Some(raw) {
            self.raw = Some(raw);
            self.dirty = true;
        }
    }

    /// Raw (`true`) or decrypted (`false`) image; `None` if the mapfile does not say.
    pub(crate) fn raw(&self) -> Option<bool> {
        self.raw
    }

    /// Fingerprints of the keys a pre-1.8 mapfile stored (KU §4.1, decision b).
    pub fn legacy_key_fingerprints(&self) -> &[[u8; 8]] {
        &self.legacy_keyfps
    }

    /// All map entries, sorted ascending by `pos` and (after load)
    /// guaranteed disjoint and non-overflowing.
    pub(crate) fn entries(&self) -> &[MapEntry] {
        &self.entries
    }

    /// Total image size in bytes, FIXED at construction: the size handed to
    /// [`Mapfile::create`], or the end byte of the last entry [`Mapfile::load`] parsed.
    ///
    /// It is NOT recomputed. [`Mapfile::record`] never touches it and never bounds a range
    /// against it, so this is "the coverage this mapfile was opened for", not "the end of the
    /// last entry as it stands now". It is also `stats().bytes_total`, so a caller recording
    /// past it would show a front-end ratio over 100%.
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// First range with a given status starting at or after `from`.
    pub fn next_with(&self, from: u64, status: SectorStatus) -> Option<(u64, u64)> {
        for e in &self.entries {
            if e.status != status {
                continue;
            }
            let e_end = e.pos.saturating_add(e.size);
            if e_end <= from {
                continue;
            }
            let start = e.pos.max(from);
            return Some((start, e_end - start));
        }
        None
    }

    /// All ranges matching one of the given statuses, in position order.
    pub fn ranges_with(&self, statuses: &[SectorStatus]) -> Vec<(u64, u64)> {
        self.entries
            .iter()
            .filter(|e| statuses.contains(&e.status))
            .map(|e| (e.pos, e.size))
            .collect()
    }

    /// Snapshot of the incrementally-maintained summary statistics: O(1) on a whole-disc map.
    /// On a scoped mapfile the never-read bytes outside the scope are not pending (not this
    /// image's job); that adjustment is one O(entries + scope ranges) walk, allocation-free.
    pub fn stats(&self) -> MapStats {
        let mut s = self.stats;
        if self.scope.is_some() {
            let outside = self.nontried_outside_scope();
            s.bytes_pending = s.bytes_pending.saturating_sub(outside);
            s.bytes_nontried = s.bytes_nontried.saturating_sub(outside);
        }
        s
    }

    fn nontried_outside_scope(&self) -> u64 {
        let Some(scope) = &self.scope else {
            return 0;
        };
        let end = |p: u64, n: u64| p.saturating_add(n);
        let mut outside = 0u64;
        let mut first = 0;
        for e in self
            .entries
            .iter()
            .filter(|e| e.status == SectorStatus::NonTried)
        {
            let e_end = end(e.pos, e.size);
            // Scope is sorted and merged: ranges ending at or before this run end before later ones.
            while scope.get(first).is_some_and(|&(p, n)| end(p, n) <= e.pos) {
                first += 1;
            }
            let inside: u64 = scope[first..]
                .iter()
                .take_while(|&&(p, _)| p < e_end)
                .map(|&(p, n)| end(p, n).min(e_end) - p.max(e.pos))
                .sum();
            outside = outside.saturating_add(e.size.saturating_sub(inside));
        }
        outside
    }

    /// The byte ranges a scoped (MKV-staging) image covers; `None` = the whole disc.
    pub fn scope(&self) -> Option<&[(u64, u64)]> {
        self.scope.as_deref()
    }

    /// Limit this image to `ranges` (bytes, sorted and merged here). Marks the mapfile dirty.
    pub fn set_scope(&mut self, mut ranges: Vec<(u64, u64)>) {
        self.scope = Some(merge_byte_ranges(&mut ranges));
        self.dirty = true;
    }

    /// Widen a scoped image back to the whole disc: its unread rest becomes pending.
    pub fn clear_scope(&mut self) {
        if self.scope.take().is_some() {
            self.dirty = true;
        }
    }

    // Replaces the entries overlapping or touching `[pos, end)` with their split remainders,
    // the new run and merged neighbours; stats move by the window's delta. Given the
    // maximal-run invariant nothing outside the window can merge, so this equals a full rebuild.
    fn splice(&mut self, pos: u64, end: u64, status: SectorStatus) {
        let run_end = |e: &MapEntry| e.pos.saturating_add(e.size);
        let lo = self.entries.partition_point(|e| run_end(e) < pos);
        let hi = self.entries.partition_point(|e| e.pos <= end);
        let window = &self.entries[lo..hi];
        let mut pieces: Vec<MapEntry> = Vec::with_capacity(window.len().min(4) + 1);
        let mut placed = false;
        let new_run = MapEntry {
            pos,
            size: end - pos,
            status,
        };
        for e in window {
            let e_end = run_end(e);
            if e_end <= pos {
                pieces.push(e.clone());
                continue;
            }
            if e.pos < pos {
                pieces.push(MapEntry {
                    pos: e.pos,
                    size: pos - e.pos,
                    status: e.status,
                });
            }
            if e_end > end && !placed {
                pieces.push(new_run.clone());
                placed = true;
            }
            if e.pos >= end {
                pieces.push(e.clone());
            } else if e_end > end {
                pieces.push(MapEntry {
                    pos: end,
                    size: e_end - end,
                    status: e.status,
                });
            }
        }
        if !placed {
            pieces.push(new_run);
        }
        let mut merged: Vec<MapEntry> = Vec::with_capacity(pieces.len());
        for e in pieces {
            push_coalesced(&mut merged, e);
        }
        for e in window {
            tally(&mut self.stats, e, false);
        }
        for e in &merged {
            tally(&mut self.stats, e, true);
        }
        if merged.len() == hi - lo {
            self.entries[lo..hi].clone_from_slice(&merged);
        } else {
            self.entries.splice(lo..hi, merged);
        }
    }

    fn compute_stats(entries: &[MapEntry], total_size: u64) -> MapStats {
        let mut s = MapStats {
            bytes_total: total_size,
            ..Default::default()
        };
        for e in entries {
            tally(&mut s, e, true);
        }
        s
    }

    fn write_to_disk(&self) -> io::Result<()> {
        with_commit_lock(&self.path, || self.write_locked())
    }

    // Only under `with_commit_lock`: the disowned checks, tmp write and rename are then one
    // step against any other writer of this path, so a stale snapshot can't land after a resume.
    fn write_locked(&self) -> io::Result<()> {
        // DISOWNED: owner was abandoned; someone else records this path now. Checked
        // here (the single commit point) so one check covers flush/record/Drop.
        // Reported as success: not writing is correct, and no caller remains.
        if self.disowned.load(Ordering::Acquire) {
            return Ok(());
        }
        // Write to a tempfile then rename for atomicity. Appending ".tmp"
        // rather than `with_extension` so we don't clobber the original
        // extension (which may already be ".mapfile").
        let tmp = {
            let mut s = self.path.clone().into_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };
        // Any `?` between creating the tmp and the final rename used to leave a
        // partially-written `<path>.tmp` behind forever. Written as a closure so
        // a single cleanup covers every early return.
        let write_tmp = |tmp: &std::path::Path| -> io::Result<()> {
            {
                // Unlink, then create exclusively: a symlink planted at the tmp name is
                // removed, never written through.
                let _ = std::fs::remove_file(tmp);
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(tmp)?;
                let mut w = std::io::BufWriter::new(file);
                writeln!(w, "# Rescue Logfile. Created by {}", self.version)?;
                // Identity lines (KU §4.1): a disc hash and fingerprints only, never a key
                // byte or a raw VID (J6); a legacy file is scrubbed on this first write.
                if let Some(h) = &self.disc_hash {
                    writeln!(w, "# freemkv-disc: {h}")?;
                }
                if let Some(fp) = &self.vidfp {
                    writeln!(w, "# freemkv-vidfp: {}", hex_lower(fp))?;
                }
                for fp in &self.legacy_keyfps {
                    writeln!(w, "# freemkv-legacy-keyfp: {}", hex_lower(fp))?;
                }
                if let Some(scope) = &self.scope {
                    let list: Vec<String> = scope
                        .iter()
                        .map(|(p, n)| format!("0x{p:x}+0x{n:x}"))
                        .collect();
                    writeln!(w, "# freemkv-scope: {}", list.join(","))?;
                }
                if let Some(raw) = self.raw {
                    writeln!(w, "# freemkv-raw: {}", u8::from(raw))?;
                }
                writeln!(w, "# Current pos / status / pass / pass_time")?;
                writeln!(w, "0x000000000  ?  1  0")?;
                writeln!(w, "#      pos        size  status")?;
                for e in &self.entries {
                    writeln!(
                        w,
                        "0x{:09x}  0x{:09x}    {}",
                        e.pos,
                        e.size,
                        e.status.to_char()
                    )?;
                }
                #[cfg(test)]
                fire_write_hook(&self.path, HookAt::TmpBuffered);
                w.flush()?;
                // fsync the tmp file before the rename so bytes are durable (notably on
                // NFS, where a rename can reach the server before the data does).
                // Recover the File from the BufWriter to call sync_all.
                let file = w.into_inner().map_err(|e| e.into_error())?;
                file.sync_all()?;
            }
            Ok(())
        };
        if let Err(e) = write_tmp(&tmp) {
            // Do not leave the half-written tmp behind. Best-effort: if the
            // failure was itself "cannot touch this directory", the remove will
            // fail too, and the write error is the one worth reporting.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // Re-check: the tmp write may have hung past a disown. A disown landing after this
        // check is still safe: the resumed owner's writes take the commit lock, so they land
        // after this rename, never before it.
        if self.disowned.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&tmp);
            return Ok(());
        }
        #[cfg(test)]
        fire_write_hook(&self.path, HookAt::PreRename);
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // fsync the parent directory so the rename itself is durable — syncing the
        // tmp file's bytes alone isn't enough, since the new dirent lives only in the
        // page cache until synced. Best-effort: unsupported dirs aren't a failure.
        if let Some(parent) = parent_dir(&self.path) {
            libfreemkv::io::fsync::dir(parent);
        }
        Ok(())
    }
}

impl Drop for Mapfile {
    // Best-effort flush on drop so an early-return/unwind doesn't lose
    // in-memory state. Errors are swallowed (Drop can't surface them);
    // explicit `flush()` on the success path handles errors properly.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

// Test seam: each hook runs once, inside `write_to_disk` for its path at its `HookAt` point.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum HookAt {
    TmpBuffered,
    PreRename,
}
#[cfg(test)]
type WriteHook = (PathBuf, HookAt, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static WRITE_HOOKS: std::sync::Mutex<Vec<WriteHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn fire_write_hook(path: &Path, at: HookAt) {
    let hook = {
        let mut hooks = WRITE_HOOKS.lock().unwrap_or_else(PoisonError::into_inner);
        let i = hooks.iter().position(|(p, a, _)| p == path && *a == at);
        i.map(|i| hooks.remove(i).2)
    };
    if let Some(f) = hook {
        f();
    }
}

// The directory holding `path`: `.` for a bare relative name, whose `parent()` is `""`.
fn parent_dir(path: &Path) -> Option<&Path> {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Some(Path::new(".")),
        p => p,
    }
}

type CommitLocks = HashMap<PathBuf, Arc<Mutex<()>>>;

// Runs `f` holding this process's lock for `path` (keyed as spelled: every pass builds its
// mapfile path the same way). Per path, so one hung mount never stalls another rip's writes.
fn with_commit_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    static LOCKS: LazyLock<Mutex<CommitLocks>> = LazyLock::new(Default::default);
    let locks = || LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    let lock = Arc::clone(locks().entry(path.to_path_buf()).or_default());
    let out = {
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        f()
    };
    drop(lock);
    let mut map = locks();
    if map.get(path).is_some_and(|l| Arc::strong_count(l) == 1) {
        map.remove(path);
    }
    out
}

fn invalid(kind: &'static str) -> io::Error {
    libfreemkv::error::Error::MapfileInvalid { kind }.into()
}

// Adds (or removes) one entry's contribution to `s`. Saturating: `record()` only removes
// what an earlier tally added, so this never actually clamps.
fn tally(s: &mut MapStats, e: &MapEntry, add: bool) {
    let f = |v: &mut u64| {
        *v = if add {
            v.saturating_add(e.size)
        } else {
            v.saturating_sub(e.size)
        }
    };
    match e.status {
        SectorStatus::Finished => f(&mut s.bytes_good),
        SectorStatus::Unreadable => {
            f(&mut s.bytes_unreadable);
            s.num_bad_ranges = if add {
                s.num_bad_ranges.saturating_add(1)
            } else {
                s.num_bad_ranges.saturating_sub(1)
            };
        }
        SectorStatus::NonTried => {
            f(&mut s.bytes_pending);
            f(&mut s.bytes_nontried);
        }
        SectorStatus::NonTrimmed | SectorStatus::NonScraped => {
            f(&mut s.bytes_pending);
            f(&mut s.bytes_retryable);
        }
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// 40 hex digits (SHA-1), `0x` optional → lowercase without the prefix.
fn parse_disc_hash(s: &str) -> Option<String> {
    libfreemkv::hex::parse_hex_fixed::<20>(s).map(|b| hex_lower(&b))
}

fn sha256(tag: &[u8], bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(tag);
    h.update(bytes);
    h.finalize().into()
}

/// `SHA-256("freemkv-vid-fp-v1" ‖ VID)`: the only form of a Volume ID a mapfile may hold
/// (KU §4.1, `# freemkv-vidfp:`); equal to libfreemkv's `KeyRing::vid_fingerprint`.
/// A fingerprint to compute or verify a `vidfp` with; the VID never leaves memory.
pub fn vid_fingerprint(vid: &[u8; 16]) -> [u8; 32] {
    sha256(b"freemkv-vid-fp-v1", vid)
}

/// `SHA-256("freemkv-key-fp-v1" ‖ key)[..8]`: a legacy key's identity (KU §4.1); equal to
/// libfreemkv's `KeyRing::proven_key_fingerprints` for the same key.
pub(crate) fn key_fingerprint(key: &[u8; 16]) -> [u8; 8] {
    let d = sha256(b"freemkv-key-fp-v1", key);
    let mut fp = [0u8; 8];
    fp.copy_from_slice(&d[..8]);
    fp
}

// KU §4.1: legacy-keyfp is written for base keys only, "CPS id < 2^24" (forensic tags sit above).
const LEGACY_BASE_CPS_LIMIT: u32 = 1 << 24;

/// What the pre-1.8 identity lines of one mapfile become.
#[derive(Default)]
struct LegacyIdentity {
    vidfp: Option<[u8; 32]>,
    keyfps: Vec<[u8; 8]>,
}

/// THE read-only parser of the pre-1.8 key and raw-VID comment lines (KU §2.2
/// allow-path, §4.1): a key becomes its [`key_fingerprint`] (base keys only; a malformed
/// line is dropped, never failing the load) and a raw VID its [`vid_fingerprint`] (a
/// malformed VID still fails the load: it named a disc). Nothing raw is kept.
fn parse_legacy_key_lines(comment: &str, out: &mut LegacyIdentity) -> io::Result<()> {
    if let Some(hex) = comment.strip_prefix("freemkv-vid:") {
        let vid = parse_hex16(hex.trim()).ok_or_else(|| invalid("vid"))?;
        out.vidfp = Some(vid_fingerprint(&vid));
    } else if let Some(uk) = comment.strip_prefix("freemkv-uk:")
        && let Some((cps, key)) = parse_uk_line(uk.trim())
        && cps < LEGACY_BASE_CPS_LIMIT
    {
        let fp = key_fingerprint(&key);
        if !out.keyfps.contains(&fp) {
            out.keyfps.push(fp);
        }
    }
    Ok(())
}

// 32 hex digits (a legacy VID or key line) → 16 bytes; `None` on malformation. A bad VID
// fails the load (`vid`) rather than reading as "no identity" (cross-disc resume splice).
fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    // The one workspace hex parser (accepts an optional `0x`/`0X` prefix,
    // byte-based so a multi-byte legacy VID comment rejects, never panics).
    libfreemkv::hex::parse_hex_fixed::<16>(s)
}

/// Parse a legacy key line's value `<cps>:<32hex>` into `(cps_unit, key)`; `None` on any
/// malformation, which [`parse_legacy_key_lines`] drops (KU §4.1).
fn parse_uk_line(s: &str) -> Option<(u32, [u8; 16])> {
    let (cps, hex) = s.split_once(':')?;
    let cps: u32 = cps.trim().parse().ok()?;
    let key = parse_hex16(hex.trim())?;
    Some((cps, key))
}

// `0xPOS+0xSIZE,...` (possibly empty) → sorted, merged byte ranges; `None` if malformed.
fn parse_scope(s: &str) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (p, n) = part.split_once('+')?;
        let p = parse_hex(p.trim()).ok()?;
        let n = parse_hex(n.trim()).ok()?;
        p.checked_add(n)?;
        out.push((p, n));
    }
    Some(merge_byte_ranges(&mut out))
}

/// Sort and merge `(pos, size)` byte ranges; zero-size ranges are dropped.
pub(crate) fn merge_byte_ranges(ranges: &mut [(u64, u64)]) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &(p, n) in ranges.iter().filter(|r| r.1 > 0) {
        match out.last_mut() {
            Some((lp, ln)) if p <= lp.saturating_add(*ln) => {
                *ln = (*ln).max(p.saturating_add(n) - *lp)
            }
            _ => out.push((p, n)),
        }
    }
    out
}

/// The parts of `ranges` that lie inside `scope` (both sorted, merged byte ranges): one
/// walk over the two lists together.
pub(crate) fn intersect(ranges: &[(u64, u64)], scope: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < ranges.len() && j < scope.len() {
        let (p, n) = ranges[i];
        let (sp, sn) = scope[j];
        let (end, send) = (p.saturating_add(n), sp.saturating_add(sn));
        let (a, b) = (p.max(sp), end.min(send));
        if a < b {
            out.push((a, b - a));
        }
        // Advance whichever ends first; the other may still overlap the next one.
        if end <= send {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

// Reads a mapfile's text, refusing one over `MAX_MAPFILE_BYTES` without reading it all.
fn read_capped(path: &Path) -> io::Result<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_MAPFILE_BYTES {
        return Err(invalid("too_large"));
    }
    let mut text = String::new();
    file.take(MAX_MAPFILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_MAPFILE_BYTES {
        return Err(invalid("too_large"));
    }
    Ok(text)
}

fn single_char(field: &str) -> Option<char> {
    let mut chars = field.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

// ddrescue's current line is `pos status [pass ...]`; its status adds `F` (filling) and `G`
// (generating) to the block alphabet. `F` is also a hex digit, so `0 F +` stays a data line.
fn is_current_line(fields: &[&str]) -> bool {
    let Some(c) = fields.get(1).and_then(|f| single_char(f)) else {
        return false;
    };
    match c {
        'G' => true,
        'F' => !fields
            .get(2)
            .and_then(|f| single_char(f))
            .is_some_and(|s| SectorStatus::from_char(s).is_some()),
        _ => SectorStatus::from_char(c).is_some(),
    }
}

fn push_coalesced(out: &mut Vec<MapEntry>, e: MapEntry) {
    if let Some(last) = out.last_mut()
        && last.pos.saturating_add(last.size) == e.pos
        && last.status == e.status
    {
        last.size = last.size.saturating_add(e.size);
        return;
    }
    out.push(e);
}

fn parse_hex(s: &str) -> io::Result<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    // The ParseIntError is dropped for the stable `hex` kind.
    u64::from_str_radix(s, 16).map_err(|_| invalid("hex"))
}

#[cfg(test)]
#[path = "mapfile_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mapfile_status_set_tests.rs"]
mod status_set_tests;

// Loads a mapfile, distinguishing "there isn't one" from "there is one and it is unreadable" —
// shared CLASSIFICATION, per-caller fail-safe VALUE.
pub(crate) fn load_if_present(path: &std::path::Path) -> io::Result<Option<Mapfile>> {
    match Mapfile::load(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            tracing::warn!(
                target: "freemkv::disc",
                path = %path.display(),
                error = %e,
                "mapfile exists but could not be read; treating damage as unknown",
            );
            Err(e)
        }
    }
}

#[cfg(test)]
#[path = "mapfile_write_to_disk_cleanup_tests.rs"]
mod write_to_disk_cleanup_tests;

#[cfg(test)]
#[path = "mapfile_load_if_present_tests.rs"]
mod load_if_present_tests;

/// Stamp `disc`'s identity on `map` (KU §4.1): its disc hash, and the fingerprint of its
/// Volume ID (the scanned disc's own, else the rip's key set's).
pub(crate) fn stamp_identity(
    map: &mut Mapfile,
    disc: &libfreemkv::Disc,
    keys: Option<&libfreemkv::keys::KeyRing>,
) {
    if let Some(a) = &disc.aacs {
        map.set_disc_hash(&a.disc_hash);
    }
    if let Some(fp) = disc_vid_fingerprint(disc, keys) {
        map.set_vid_fingerprint(fp);
    }
}

// The VID fingerprint of the disc in hand: its scanned VID, else the key set's in-memory one.
fn disc_vid_fingerprint(
    disc: &libfreemkv::Disc,
    keys: Option<&libfreemkv::keys::KeyRing>,
) -> Option<[u8; 32]> {
    disc.aacs
        .as_ref()
        .map(|a| a.volume_id)
        .filter(|v| *v != [0u8; 16])
        .map(|v| vid_fingerprint(&v))
        .or_else(|| keys.and_then(|k| k.vid_fingerprint()))
}

/// What the disc in hand offers the §4.4 identity check; `None` = unknown (not compared).
pub(crate) struct DiscIdentity {
    pub disc_hash: Option<String>,
    pub vidfp: Option<[u8; 32]>,
    /// Fingerprints of the proven base keys; empty = none proven (rule 3 cannot check).
    pub proven: Vec<[u8; 8]>,
}

impl DiscIdentity {
    /// `disc`'s identity with the rip's `keys` (their VID and proven keys). With no set nothing
    /// is proven: KU §4.4 rule 3 is "checked **only if** the set proved at least one base key".
    pub(crate) fn of(disc: &libfreemkv::Disc, keys: Option<&libfreemkv::keys::KeyRing>) -> Self {
        let proven = keys.map_or_else(Vec::new, |set| set.proven_key_fingerprints());
        DiscIdentity {
            disc_hash: disc
                .aacs
                .as_ref()
                .and_then(|a| parse_disc_hash(&a.disc_hash)),
            vidfp: disc_vid_fingerprint(disc, keys),
            proven,
        }
    }
}

/// Does `map` describe `disc`? [`check_identity`] over [`DiscIdentity::of`].
pub(crate) fn check_mapfile_identity(
    map: &Mapfile,
    disc: &libfreemkv::Disc,
    keys: Option<&libfreemkv::keys::KeyRing>,
) -> io::Result<()> {
    check_identity(map, &DiscIdentity::of(disc, keys))
}

/// KU §4.4, over what is known on both sides:
/// 1. disc hashes both known and different → mismatch;
/// 2. VID fingerprints both known and different → mismatch;
/// 3. legacy key fingerprints, only when base keys were proven: none matches → mismatch. A
///    set that proved no base key cannot check them, and that is not a mismatch.
pub(crate) fn check_identity(map: &Mapfile, disc: &DiscIdentity) -> io::Result<()> {
    let mismatch = |rule: &str| -> io::Error {
        tracing::warn!(
            target: "freemkv::disc",
            rule,
            "mapfile identity does not match the disc in hand — refusing to resume"
        );
        invalid("disc-mismatch")
    };
    if let (Some(m), Some(d)) = (map.disc_hash(), disc.disc_hash.as_deref())
        && m != d
    {
        return Err(mismatch("disc hash"));
    }
    if let (Some(m), Some(d)) = (map.vid_fingerprint(), disc.vidfp)
        && m != d
    {
        return Err(mismatch("vidfp"));
    }
    if !map.legacy_keyfps.is_empty()
        && !disc.proven.is_empty()
        && !map.legacy_keyfps.iter().any(|f| disc.proven.contains(f))
    {
        return Err(mismatch("legacy key fingerprints"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "mapfile_ku_identity_tests.rs"]
mod ku_identity_tests;
