//! ddrescue-compatible mapfile for tracking rip progress.
//!
//! Records which byte ranges of a disc image are good, unreadable,
//! or not-yet-attempted. Written as plain text so it's greppable,
//! human-editable, and interoperates with ddrescue's own tools.
//!
//! Format:
//! ```text
//! # Rescue Logfile. Created by freemkv-engine vX.Y.Z
//! # Current pos / status / pass / pass_time (ddrescue state machine — we only populate pos)
//! 0x000000000  ?  1  0
//! #      pos        size  status
//! 0x000000000  0x12345678    +
//! 0x012345678  0x00001000    -
//! 0x012346678  0x01234500    ?
//! ```
//!
//! Status chars: `?` non-tried · `*` non-trimmed · `/` non-scraped · `-` unreadable · `+` finished.
//!
//! Persisted to disk in time-batched intervals; see [`FLUSH_INTERVAL`] and
//! [`Mapfile`] for the flush policy.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// Minimum interval between mapfile persists (else on `flush()`/`Drop`).
// Bounds atomic-rename RPC rate on NFS staging; worst-case crash loss is
// one interval's worth of records.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1000);

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
    /// Exhaustive on purpose: adding a sixth variant is a compile error here,
    /// not a silently-omitted entry in one of the hand-written arrays that
    /// used to be scattered across this crate.
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
    /// Largest gap among unreadable ranges in milliseconds. Computed as
    /// largest range size / bytes_per_sec * 1000. Set by caller (autorip)
    /// since bytes_per_sec is application-specific.
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

impl Mapfile {
    /// Create a new mapfile with one `NonTried` region covering the whole disc.
    /// Writes to disk immediately so a resume can pick up even if the caller
    /// never records anything.
    pub fn create(path: &Path, total_size: u64, version: &str) -> io::Result<Self> {
        let mut mf = Self {
            path: path.to_path_buf(),
            entries: vec![MapEntry {
                pos: 0,
                size: total_size,
                status: SectorStatus::NonTried,
            }],
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
        let text = std::fs::read_to_string(path)?;
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
                        let e: io::Error =
                            libfreemkv::error::Error::MapfileInvalid { kind: "scope" }.into();
                        return Err(e);
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
                let is_current_line = fields
                    .get(1)
                    .and_then(|f| single_char(f))
                    .and_then(SectorStatus::from_char)
                    .is_some();
                if is_current_line {
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
                let e: io::Error =
                    libfreemkv::error::Error::MapfileInvalid { kind: "range" }.into();
                return Err(e);
            }
            // A zero-size entry is degenerate: it contributes nothing to the
            // partition yet trips overlap/coalesce arithmetic (two entries can
            // share the same pos). Reject it rather than carry it through.
            if size == 0 {
                let e: io::Error =
                    libfreemkv::error::Error::MapfileInvalid { kind: "zero_size" }.into();
                return Err(e);
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
                let err: io::Error =
                    libfreemkv::error::Error::MapfileInvalid { kind: "overlap" }.into();
                return Err(err);
            }
            if e.pos > cursor {
                // Leading or internal gap — fill it as NonTried.
                filled.push(MapEntry {
                    pos: cursor,
                    size: e.pos - cursor,
                    status: SectorStatus::NonTried,
                });
            }
            cursor = e.pos.saturating_add(e.size);
            filled.push(e);
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
                // basis. Warn, don't fail — so `==` here is an equivalent mutant.
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
            let e: io::Error = libfreemkv::error::Error::MapfileInvalid { kind: "range" }.into();
            return Err(e);
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
            if let Some(last) = merged.last_mut()
                && last.pos.saturating_add(last.size) == e.pos
                && last.status == e.status
            {
                last.size = last.size.saturating_add(e.size);
                continue;
            }
            merged.push(e);
        }
        self.entries = merged;
        if !hit {
            return Ok(());
        }
        self.stats = Self::compute_stats(&self.entries, self.total_size);
        self.changed()
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

    /// Snapshot of the incrementally-maintained summary statistics.
    /// O(1) — returns the cached `MapStats`. On a scoped mapfile the never-read bytes
    /// outside the scope are not pending: they are not part of this image's job.
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
        let nontried: u64 = self
            .entries
            .iter()
            .filter(|e| e.status == SectorStatus::NonTried)
            .map(|e| e.size)
            .sum();
        let inside: u64 = self
            .ranges_with(&[SectorStatus::NonTried])
            .iter()
            .map(|&r| intersect(&[r], scope).iter().map(|&(_, n)| n).sum::<u64>())
            .sum();
        nontried - inside
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
            if let Some(last) = merged.last_mut()
                && run_end(last) == e.pos
                && last.status == e.status
            {
                last.size = last.size.saturating_add(e.size);
                continue;
            }
            merged.push(e);
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
                let file = std::fs::File::create(tmp)?;
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
        // Re-check at the commit point: the entry check above can be stale, since
        // writing/fsyncing the tmp hangs on the mount this mechanism exists for.
        // Narrows (not closes) the race; true fix needs an atomic check-and-rename.
        if self.disowned.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&tmp);
            return Ok(());
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // fsync the parent directory so the rename itself is durable — syncing the
        // tmp file's bytes alone isn't enough, since the new dirent lives only in the
        // page cache until synced. Best-effort: unsupported dirs aren't a failure.
        if let Some(parent) = self.path.parent() {
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
/// (KU §4.1, `# freemkv-vidfp:`); equal to libfreemkv's `ResolvedKeySet::vid_fingerprint`.
/// A fingerprint to compute or verify a `vidfp` with; the VID never leaves memory.
pub fn vid_fingerprint(vid: &[u8; 16]) -> [u8; 32] {
    sha256(b"freemkv-vid-fp-v1", vid)
}

/// `SHA-256("freemkv-key-fp-v1" ‖ key)[..8]`: a legacy key's identity (KU §4.1); equal to
/// libfreemkv's `ResolvedKeySet::proven_key_fingerprints` for the same key.
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
        let vid = parse_vid_hex(hex.trim()).ok_or_else(|| invalid("vid"))?;
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

// Parse a 32-char hex VID string. `None` on malformation, which `load()`
// turns into a hard `MapfileInvalid{kind:"vid"}` rather than "no identity" —
// else corruption here would re-open the cross-disc resume splice.
fn parse_vid_hex(s: &str) -> Option<[u8; 16]> {
    // The one workspace hex parser (accepts an optional `0x`/`0X` prefix,
    // byte-based so a multi-byte legacy VID comment rejects, never panics).
    libfreemkv::hex::parse_hex_fixed::<16>(s)
}

/// Parse a legacy key line's value `<cps>:<32hex>` into `(cps_unit, key)`; `None` on any
/// malformation, which [`parse_legacy_key_lines`] drops (KU §4.1).
fn parse_uk_line(s: &str) -> Option<(u32, [u8; 16])> {
    let (cps, hex) = s.split_once(':')?;
    let cps: u32 = cps.trim().parse().ok()?;
    let key = parse_vid_hex(hex.trim())?; // 32-hex → [u8; 16], shared parser
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
            Some((lp, ln)) if p <= *lp + *ln => *ln = (*ln).max(p + n - *lp),
            _ => out.push((p, n)),
        }
    }
    out
}

/// The parts of `ranges` that lie inside `scope` (both sorted, merged byte ranges).
pub(crate) fn intersect(ranges: &[(u64, u64)], scope: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for &(p, n) in ranges {
        let end = p + n;
        for &(sp, sn) in scope {
            let (a, b) = (p.max(sp), end.min(sp + sn));
            if a < b {
                out.push((a, b - a));
            }
        }
    }
    out
}

fn single_char(field: &str) -> Option<char> {
    let mut chars = field.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

fn parse_hex(s: &str) -> io::Result<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).map_err(|_| {
        // Underlying ParseIntError dropped — its Display is OS-locale text.
        // The typed variant carries `kind = "hex"` which is stable.
        let e: io::Error = libfreemkv::error::Error::MapfileInvalid { kind: "hex" }.into();
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins this crate's `mapfile_path_for` to libfreemkv's `Disc::mapfile_for` (duplicated by
    // necessity — libfreemkv can't depend back on this crate).
    #[test]
    fn agrees_with_libfreemkv_disc_mapfile_for() {
        let disc = libfreemkv::Disc {
            volume_id: "TEST_VOL".into(),
            meta_title: Some("TEST_VOL".into()),
            format: libfreemkv::DiscFormat::Uhd,
            capacity_sectors: 1024,
            capacity_bytes: 1024 * 2048,
            layers: 1,
            titles: Vec::new(),
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        for p in [
            "/tmp/movie.iso",
            "/staging/Some Disc (2024).iso",
            "relative.iso",
            "/tmp/no_extension",
            "/tmp/dots.in.name.iso",
        ] {
            let path = Path::new(p);
            assert_eq!(
                mapfile_path_for(path),
                disc.mapfile_for(path),
                "mapfile naming drifted from libfreemkv for {p}"
            );
        }
    }

    fn tmpfile(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "libfreemkv-mapfile-test-{}-{}-{}.mapfile",
            std::process::id(),
            tag,
            n
        );
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    // Three-pass patch-shaped workload; returns the entry count after each
    // pass. Pass 2 is the worst case for `record()`'s coalescing: alternate
    // sectors come back, so every recovered sector is its own bracketed run.
    fn fragmenting_multipass(mf: &mut Mapfile) -> Vec<usize> {
        const SEC: u64 = 2048;
        let mut counts = Vec::new();
        // Pass 1 (sweep): the readable bulk lands Finished, three regions of
        // 64 sectors each fail as NonTrimmed.
        mf.record(0, 1000 * SEC, SectorStatus::Finished).unwrap();
        for base in [100u64, 400, 700] {
            mf.record(base * SEC, 64 * SEC, SectorStatus::NonTrimmed)
                .unwrap();
        }
        counts.push(mf.entries().len());
        // Pass 2 (scrape): every other sector inside each bad region comes
        // back; the rest stay NonTrimmed. Worst-case interleave.
        for base in [100u64, 400, 700] {
            for i in 0..64u64 {
                if i % 2 == 0 {
                    mf.record((base + i) * SEC, SEC, SectorStatus::Finished)
                        .unwrap();
                }
            }
        }
        counts.push(mf.entries().len());
        // Pass 3: the remaining sectors come back too.
        for base in [100u64, 400, 700] {
            for i in 0..64u64 {
                if i % 2 == 1 {
                    mf.record((base + i) * SEC, SEC, SectorStatus::Finished)
                        .unwrap();
                }
            }
        }
        counts.push(mf.entries().len());
        counts
    }

    // `record()` leaves `entries` as the CANONICAL maximal-run partition of `[0, total_size)`
    // (contiguous, gapless, no two adjacent entries sharing a status).
    fn assert_canonical(mf: &Mapfile) {
        let es = mf.entries();
        assert!(!es.is_empty());
        assert_eq!(es[0].pos, 0, "partition must start at 0");
        let mut expect_pos = 0u64;
        for (i, e) in es.iter().enumerate() {
            assert_eq!(e.pos, expect_pos, "gap or overlap before entry {i}");
            assert!(e.size > 0, "zero-size entry {i}");
            if i > 0 {
                assert_ne!(
                    es[i - 1].status,
                    e.status,
                    "entries {} and {i} share a status and were not coalesced",
                    i - 1
                );
            }
            expect_pos += e.size;
        }
        assert_eq!(
            expect_pos,
            mf.total_size(),
            "partition must cover the whole image"
        );
    }

    // R14: a record that does not change the run count (a sweep extending its Finished run
    // into NonTried) edits `entries` in place instead of rebuilding the whole Vec.
    #[test]
    fn a_record_that_keeps_the_run_count_edits_in_place() {
        const SEC: u64 = 2048;
        let p = tmpfile("record_in_place");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 20_000 * SEC, "test").unwrap();
        // Runs `* + ?` per 4-sector group: `*` at 4i, `+` at 4i+1, `?` over 4i+2..4i+4.
        for i in 0..5_000u64 {
            mf.record(i * 4 * SEC, SEC, SectorStatus::NonTrimmed)
                .unwrap();
            mf.record((i * 4 + 1) * SEC, SEC, SectorStatus::Finished)
                .unwrap();
        }
        let (ptr, len) = (mf.entries().as_ptr(), mf.entries().len());
        for i in 0..5_000u64 {
            // Grows each `+` run by one sector into the `?` run after it.
            mf.record((i * 4 + 2) * SEC, SEC, SectorStatus::Finished)
                .unwrap();
            assert_eq!(mf.entries().as_ptr(), ptr, "record() rebuilt the entry Vec");
        }
        assert_eq!(mf.entries().len(), len);
        assert_canonical(&mf);
        let _ = std::fs::remove_file(&p);
    }

    // The pre-R14 `record()` (full rebuild, sort, global coalesce): the reference the
    // localized splice must agree with.
    fn reference_record(
        entries: &[MapEntry],
        pos: u64,
        size: u64,
        status: SectorStatus,
    ) -> Vec<MapEntry> {
        if size == 0 {
            return entries.to_vec();
        }
        let end = pos + size;
        let mut out = Vec::new();
        for e in entries.iter().cloned() {
            let e_end = e.pos + e.size;
            if e_end <= pos || e.pos >= end {
                out.push(e);
                continue;
            }
            if e.pos < pos {
                out.push(MapEntry {
                    pos: e.pos,
                    size: pos - e.pos,
                    status: e.status,
                });
            }
            if e_end > end {
                out.push(MapEntry {
                    pos: end,
                    size: e_end - end,
                    status: e.status,
                });
            }
        }
        out.push(MapEntry { pos, size, status });
        out.sort_by_key(|e| e.pos);
        let mut merged: Vec<MapEntry> = Vec::new();
        for e in out {
            if let Some(last) = merged.last_mut()
                && last.pos + last.size == e.pos
                && last.status == e.status
            {
                last.size += e.size;
                continue;
            }
            merged.push(e);
        }
        merged
    }

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const ALL_STATUSES: [SectorStatus; 5] = [
        SectorStatus::NonTried,
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
        SectorStatus::Finished,
    ];

    // R14 equivalence: random records (inside, straddling and past the end, leaving gaps;
    // zero-size too) give exactly the old algorithm's entries, and the delta-maintained
    // stats equal a full recount after every step.
    #[test]
    fn localized_record_matches_the_full_rebuild() {
        let p = tmpfile("record_equivalence");
        let _ = std::fs::remove_file(&p);
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for round in 0..200 {
            let total = 1 + rng.below(400);
            let mut mf = Mapfile::create(&p, total, "test").unwrap();
            let mut reference = mf.entries().to_vec();
            for step in 0..60 {
                let pos = rng.below(total + 40);
                let size = match rng.below(6) {
                    0 => 0,
                    1 => rng.below(total + 40) + 1,
                    _ => rng.below(12) + 1,
                };
                let status = ALL_STATUSES[rng.below(5) as usize];
                mf.record(pos, size, status).unwrap();
                reference = reference_record(&reference, pos, size, status);
                assert_eq!(
                    mf.entries(),
                    reference.as_slice(),
                    "round {round} step {step}: record({pos}, {size}, {status:?})"
                );
                assert_eq!(
                    mf.stats(),
                    Mapfile::compute_stats(&reference, total),
                    "round {round} step {step}: stats drifted"
                );
            }
            mf.dirty = false;
        }
        let _ = std::fs::remove_file(&p);
    }

    // P6: `promote()` is the per-range `record()` loop in one pass: same entries, same stats.
    #[test]
    fn promote_matches_recording_each_range() {
        let p = tmpfile("promote_equivalence");
        let q = tmpfile("promote_equivalence_ref");
        let from = [SectorStatus::NonTrimmed, SectorStatus::NonScraped];
        let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
        for round in 0..100 {
            let total = 1 + rng.below(300);
            let mut mf = Mapfile::create(&p, total, "test").unwrap();
            for _ in 0..40 {
                let pos = rng.below(total);
                let size = (rng.below(10) + 1).min(total - pos);
                mf.record(pos, size, ALL_STATUSES[rng.below(5) as usize])
                    .unwrap();
            }
            let to = ALL_STATUSES[rng.below(5) as usize];
            let mut looped = Mapfile::create(&q, total, "test").unwrap();
            looped.entries = mf.entries.clone();
            looped.stats = mf.stats;
            for (pos, size) in looped.ranges_with(&from) {
                looped.record(pos, size, to).unwrap();
            }
            mf.promote(&from, to).unwrap();
            assert_eq!(mf.entries(), looped.entries(), "round {round} to {to:?}");
            assert_eq!(mf.stats(), looped.stats(), "round {round} to {to:?}");
            assert_canonical(&mf);
        }
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&q);
    }

    // The `Mapfile.entries` bound, measured rather than asserted from a doc.
    #[test]
    fn fragmentation_peaks_then_collapses_as_damage_is_recovered() {
        let p = tmpfile("fragmentation_peaks_then_collapses");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000 * 2048, "test").unwrap();
        let counts = fragmenting_multipass(&mut mf);
        assert_canonical(&mf);
        let _ = std::fs::remove_file(&p);
        // Literals, not recomputed from the code under test: pass 1 alternates
        // +/*/+/*/+/*/+ = 7 runs; pass 2 is 3 regions x 64 alternating sectors with
        // the leading + merging into the bulk = 1 + 3*64 = 193; pass 3 collapses to 1.
        assert_eq!(counts, vec![7, 193, 1]);
        assert!(
            counts[2] < counts[1],
            "fragmentation must be reversible, not a ratchet: {counts:?}"
        );
    }

    /// A record that lands inside an existing run of the same status is free:
    /// the partition is unchanged, so repeated passes over already-known
    /// territory cannot fragment the list at all.
    #[test]
    fn repeat_records_inside_a_run_do_not_fragment() {
        let p = tmpfile("repeat_records_inside_a_run");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000 * 2048, "test").unwrap();
        mf.record(0, 1000 * 2048, SectorStatus::Finished).unwrap();
        mf.record(100 * 2048, 8 * 2048, SectorStatus::Unreadable)
            .unwrap();
        let before: Vec<MapEntry> = mf.entries().to_vec();
        assert_eq!(before.len(), 3);
        for _ in 0..500 {
            mf.record(100 * 2048, 8 * 2048, SectorStatus::Unreadable)
                .unwrap();
            mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        }
        assert_eq!(mf.entries(), before.as_slice());
        assert_canonical(&mf);
        let _ = std::fs::remove_file(&p);
    }

    // Pins the on-disk format against a literal mapfile written by an older
    // release (v0.14.0) for damaged UHD media: it must load, its 19 entries
    // must be understood exactly, and re-writing it must reproduce the bytes.
    #[test]
    fn real_shaped_mapfile_round_trips() {
        const ARCHIVED: &str = "\
# Rescue Logfile. Created by libfreemkv v0.14.0
# Current pos / status / pass / pass_time
0x000000000  ?  1  0
#      pos        size  status
0x000000000  0x34b630000    +
0x34b630000  0x000010000    *
0x34b640000  0x2b0d50000    +
0x5fc390000  0x007010000    *
0x6033a0000  0x000070000    +
0x603410000  0x008000000    *
0x60b410000  0x371030000    +
0x97c440000  0x000010000    *
0x97c450000  0x0001f0000    +
0x97c640000  0x000010000    *
0x97c650000  0x0000c0000    +
0x97c710000  0x001000000    *
0x97d710000  0x000090000    +
0x97d7a0000  0x002000000    *
0x97f7a0000  0x005480000    +
0x984c20000  0x003010000    *
0x987c30000  0x000080000    +
0x987cb0000  0x004000000    *
0x98bcb0000  0xa24550000    +
";
        let p = tmpfile("real_shaped_mapfile_round_trips");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, ARCHIVED).unwrap();
        let mut mf = Mapfile::load(&p).unwrap();
        // Nineteen entries — the real fragmentation ceiling this format has
        // been observed to reach, and unchanged between that disc's two passes.
        assert_eq!(mf.entries().len(), 19);
        assert_eq!(mf.total_size(), 0x1_3B0_200_000);
        assert_eq!(mf.entries()[1].pos, 0x34b630000);
        assert_eq!(mf.entries()[1].size, 0x10000);
        assert_eq!(mf.entries()[1].status, SectorStatus::NonTrimmed);
        // Re-write it: same build, same bytes.
        mf.record(0, 0x34b630000, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        let rewritten = std::fs::read_to_string(&p).unwrap();
        assert_eq!(rewritten, ARCHIVED);
        // And it still loads to the same entries.
        let reloaded = Mapfile::load(&p).unwrap();
        assert_eq!(reloaded.entries(), mf.entries());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_has_one_nontried_region() {
        let p = tmpfile("create_has_one_nontried_region");
        let _ = std::fs::remove_file(&p);
        let mf = Mapfile::create(&p, 1000, "test").unwrap();
        assert_eq!(mf.entries().len(), 1);
        assert_eq!(mf.entries()[0].pos, 0);
        assert_eq!(mf.entries()[0].size, 1000);
        assert_eq!(mf.entries()[0].status, SectorStatus::NonTried);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_splits_overlap() {
        let p = tmpfile("record_splits_overlap");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(200, 100, SectorStatus::Finished).unwrap();
        let es = mf.entries();
        assert_eq!(es.len(), 3);
        assert_eq!(
            (es[0].pos, es[0].size, es[0].status),
            (0, 200, SectorStatus::NonTried)
        );
        assert_eq!(
            (es[1].pos, es[1].size, es[1].status),
            (200, 100, SectorStatus::Finished)
        );
        assert_eq!(
            (es[2].pos, es[2].size, es[2].status),
            (300, 700, SectorStatus::NonTried)
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_coalesces_adjacent_same_status() {
        let p = tmpfile("record_coalesces_adjacent_same_status");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 100, SectorStatus::Finished).unwrap();
        mf.record(200, 100, SectorStatus::Finished).unwrap();
        // Entries: [0..100 NonTried, 100..300 Finished (merged), 300..1000 NonTried]
        let es = mf.entries();
        assert_eq!(es.len(), 3);
        assert_eq!(
            (es[1].pos, es[1].size, es[1].status),
            (100, 200, SectorStatus::Finished)
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_replaces_existing_status() {
        let p = tmpfile("record_replaces_existing_status");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(200, 100, SectorStatus::Unreadable).unwrap();
        mf.record(200, 100, SectorStatus::Finished).unwrap();
        let es = mf.entries();
        // The overwrite should result in all finished at 200..300, NonTried elsewhere — 3 entries.
        assert_eq!(es.len(), 3);
        assert_eq!(es[1].status, SectorStatus::Finished);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn round_trip_load() {
        let p = tmpfile("round_trip_load");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 200, SectorStatus::Finished).unwrap();
        mf.record(500, 100, SectorStatus::Unreadable).unwrap();
        // record() batches; explicit flush before reading back from disk.
        mf.flush().unwrap();
        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.entries(), mf.entries());
        // The entry list is the one part of state written verbatim; total_size and
        // stats are supplied on create and re-derived on load, so a writer that
        // dropped the trailing extent could round-trip "correctly" on entries alone.
        assert_eq!(
            loaded.total_size(),
            1000,
            "the extent must survive a reload"
        );
        assert_eq!(loaded.total_size(), mf.total_size());
        assert_eq!(loaded.stats(), mf.stats(), "in-memory and reloaded stats");
        let st = loaded.stats();
        assert_eq!(st.bytes_total, 1000);
        assert_eq!(st.bytes_good, 200, "record(100, 200, Finished)");
        assert_eq!(st.bytes_unreadable, 100, "record(500, 100, Unreadable)");
        assert_eq!(st.bytes_pending, 700, "1000 - 200 - 100 still outstanding");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_to_disk_fsyncs_and_leaves_no_tmp() {
        // Regression: write_to_disk must recover the File and sync_all() it before
        // rename (NFS durability). The .tmp file must not survive a successful
        // write, and the renamed mapfile must load back identically.
        let p = tmpfile("write_to_disk_fsyncs");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 200, SectorStatus::Finished).unwrap();
        mf.write_to_disk().unwrap();

        let mut tmp = p.clone().into_os_string();
        tmp.push(".tmp");
        assert!(
            !PathBuf::from(&tmp).exists(),
            "tmp file should be renamed away after a successful write"
        );

        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.entries(), mf.entries());
        // Same reason as `round_trip_load`: the derived state has to survive
        // too, or a durable write of a truncated extent still passes.
        assert_eq!(loaded.total_size(), 1000);
        assert_eq!(loaded.stats(), mf.stats());
        assert_eq!(loaded.stats().bytes_good, 200);
        assert_eq!(loaded.stats().bytes_pending, 800);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_to_disk_fsyncs_parent_dir() {
        // Regression: after rename(2), write_to_disk must fsync the parent dir so
        // the new dirent is durable. Can't observe power loss in a unit test, but
        // exercise the fsync branch and confirm it neither errors nor corrupts.
        let dir = tmpfile("write_to_disk_fsyncs_parent_dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("disc.mapfile");
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(0, 400, SectorStatus::Finished).unwrap();
        mf.record(400, 100, SectorStatus::Unreadable).unwrap();
        mf.write_to_disk().unwrap();

        // The directly-called dir fsync helper must be a no-op-on-error,
        // never a panic, even for a nonexistent directory.
        libfreemkv::io::fsync::dir(&dir.join("does-not-exist"));

        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.entries(), mf.entries());
        assert_eq!(loaded.total_size(), 1000);
        assert_eq!(loaded.stats(), mf.stats());
        assert_eq!(loaded.stats().bytes_good, 400);
        assert_eq!(loaded.stats().bytes_unreadable, 100);
        assert_eq!(loaded.stats().bytes_pending, 500);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_sum_correctly() {
        let p = tmpfile("stats_sum_correctly");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(0, 400, SectorStatus::Finished).unwrap();
        mf.record(400, 100, SectorStatus::Unreadable).unwrap();
        let s = mf.stats();
        assert_eq!(s.bytes_good, 400);
        assert_eq!(s.bytes_unreadable, 100);
        assert_eq!(s.bytes_pending, 500);
        assert_eq!(s.bytes_total, 1000);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn ranges_with_filters() {
        let p = tmpfile("ranges_with_filters");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 50, SectorStatus::Unreadable).unwrap();
        mf.record(300, 50, SectorStatus::Unreadable).unwrap();
        let bad = mf.ranges_with(&[SectorStatus::Unreadable]);
        assert_eq!(bad, vec![(100, 50), (300, 50)]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stats_consistent_after_overlapping_records() {
        let p = tmpfile("stats_consistent_after_overlapping");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        // Record some finished, some unreadable, some nontrimmed
        mf.record(0, 300, SectorStatus::Finished).unwrap();
        mf.record(300, 200, SectorStatus::NonTrimmed).unwrap();
        mf.record(500, 100, SectorStatus::Unreadable).unwrap();
        mf.record(600, 400, SectorStatus::Finished).unwrap();

        // Final entries: [0..300 Finished, 300..500 NonTrimmed, 500..600 Unreadable, 600..1000 Finished]
        let s = mf.stats();
        assert_eq!(s.bytes_good, 700); // 300 + 400
        assert_eq!(s.bytes_unreadable, 100); // 100
        assert_eq!(s.bytes_pending, 200); // NonTrimmed only (NonTried=0)
        assert_eq!(s.bytes_nontried, 0);
        assert_eq!(s.bytes_retryable, 200); // NonTrimmed
        assert_eq!(s.bytes_total, 1000);

        // Overwrite a NonTrimmed range with Finished
        mf.record(300, 100, SectorStatus::Finished).unwrap();
        // Entries: [0..400 Finished, 400..500 NonTrimmed, 500..600 Unreadable, 600..1000 Finished]
        let s2 = mf.stats();
        assert_eq!(s2.bytes_good, 800); // 400 + 400
        assert_eq!(s2.bytes_unreadable, 100);
        assert_eq!(s2.bytes_pending, 100); // NonTrimmed only
        assert_eq!(s2.bytes_retryable, 100);

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_rejects_entry_whose_range_overflows_u64() {
        let p = tmpfile("load_overflow");
        let _ = std::fs::remove_file(&p);
        // pos near u64::MAX with a nonzero size overflows pos+size.
        let body = format!("0x{:x} 0x10 +\n", u64::MAX - 4);
        std::fs::write(&p, body).unwrap();
        let kind = match Mapfile::load(&p) {
            Ok(_) => panic!("overflowing entry must be rejected"),
            Err(e) => e.kind(),
        };
        assert_eq!(kind, io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn record_rejects_range_overflowing_u64() {
        let p = tmpfile("record_overflow");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        let err = mf
            .record(u64::MAX - 4, 16, SectorStatus::Finished)
            .expect_err("overflowing record must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn parse_vid_hex_does_not_panic_on_multibyte_32_byte_input() {
        // A 32-BYTE comment containing a multi-byte char would make the
        // old `&s[i*2..i*2+2]` slice fall inside a char boundary and
        // panic. Must return None instead.
        let s = "中".to_string() + &"a".repeat(29); // 3 + 29 = 32 bytes
        assert_eq!(s.len(), 32);
        assert_eq!(parse_vid_hex(&s), None);
        // A valid 32-char ASCII hex string still parses.
        assert_eq!(
            parse_vid_hex("00112233445566778899aabbccddeeff"),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ])
        );
    }

    #[test]
    fn load_rejects_overflowing_pos_plus_size() {
        let p = tmpfile("load_rejects_overflow");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0xfffffffffffffff0  0x20    +\n",
        )
        .unwrap();
        assert!(
            Mapfile::load(&p).is_err(),
            "a pos+size that overflows u64 must be rejected, not wrap"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_rejects_overlapping_ranges() {
        let p = tmpfile("load_rejects_overlap");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000080  0x00000100    -\n",
        )
        .unwrap();
        assert!(
            Mapfile::load(&p).is_err(),
            "overlapping ranges must be rejected so stats can't double-count"
        );
        let _ = std::fs::remove_file(&p);
    }

    // Regression: an INTERNAL hole (byte range no entry covers) must load
    // filled as NonTried so it's visible to resume, else total_size still
    // equals the disc size and copy()'s complete-check misreports a hole.
    #[test]
    fn load_fills_internal_gap_as_nontried() {
        let p = tmpfile("load_fills_internal_gap");
        let _ = std::fs::remove_file(&p);
        // Two Finished entries: [0,0x100) and [0x200,0x300). The hole at
        // [0x100,0x200) is never covered.
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000200  0x00000100    +\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).expect("holed mapfile must load (gap filled, not rejected)");
        // The hole [0x100,0x200) must now be a NonTried entry.
        let hole = mf
            .entries()
            .iter()
            .find(|e| e.pos == 0x100)
            .expect("internal gap must be filled with a synthetic entry");
        assert_eq!(hole.size, 0x100, "filled gap covers the whole hole");
        assert_eq!(
            hole.status,
            SectorStatus::NonTried,
            "filled gap must be NonTried so resume reads it"
        );
        // total_size unchanged (last entry end), but the hole is now pending.
        assert_eq!(mf.total_size(), 0x300);
        // EXACTLY the hole, not "at least" it: doubling the NonTried contribution
        // in compute_stats left this assertion green (satisfied by `>=`) while
        // six other mapfile tests went red.
        assert_eq!(
            mf.stats().bytes_pending,
            0x100,
            "the hole must count as pending — exactly once — so copy() doesn't \
             report complete"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Regression: a LEADING gap (first entry doesn't start at 0) is filled
    /// as NonTried too, so resume reads the head of the disc.
    #[test]
    fn load_fills_leading_gap_as_nontried() {
        let p = tmpfile("load_fills_leading_gap");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000080  0x00000100    +\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).expect("leading-gap mapfile must load");
        let head = mf
            .entries()
            .first()
            .expect("must have a leading fill entry");
        assert_eq!(head.pos, 0, "fill must start at byte 0");
        assert_eq!(head.size, 0x80);
        assert_eq!(head.status, SectorStatus::NonTried);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn num_bad_ranges_counts_unreadable_entries() {
        let p = tmpfile("num_bad_ranges");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 50, SectorStatus::Unreadable).unwrap();
        mf.record(300, 50, SectorStatus::Unreadable).unwrap();
        assert_eq!(mf.stats().num_bad_ranges, 2);
        let _ = std::fs::remove_file(&p);
    }

    // ── status char round-trip (ddrescue alphabet ?*/-+) ──────────
    // Every SectorStatus must round-trip to_char/from_char with the exact
    // alphabet — a swapped mapping would silently misclassify resume state.
    #[test]
    fn status_char_round_trip_is_ddrescue_alphabet() {
        let pairs = [
            (SectorStatus::NonTried, '?'),
            (SectorStatus::NonTrimmed, '*'),
            (SectorStatus::NonScraped, '/'),
            (SectorStatus::Unreadable, '-'),
            (SectorStatus::Finished, '+'),
        ];
        for (st, ch) in pairs {
            assert_eq!(st.to_char(), ch, "{st:?} must map to '{ch}'");
            assert_eq!(SectorStatus::from_char(ch), Some(st));
        }
        // Any char outside the alphabet is rejected. This list used to end in
        // `'?'.to_ascii_uppercase()` (just `'?'`, a valid status char), asserting
        // nothing the `pairs` loop above hadn't already covered.
        for bad in ['x', ' ', '0', '#', '!'] {
            assert_eq!(
                SectorStatus::from_char(bad),
                None,
                "'{bad}' is not a status"
            );
        }
    }

    // ── parse_hex / parse_uk_line / parse_vid_hex error paths ─────

    /// parse_hex accepts both `0x`-prefixed and bare hex (ddrescue writes
    /// `0x`-prefixed). A non-hex field is a MapfileInvalid{kind:"hex"}.
    #[test]
    fn parse_hex_accepts_prefixed_and_bare_rejects_garbage() {
        assert_eq!(parse_hex("0x10").unwrap(), 16);
        assert_eq!(parse_hex("10").unwrap(), 16);
        assert_eq!(parse_hex("0xffffffff").unwrap(), 0xffff_ffff);
        let err = parse_hex("0xzz").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // The scope survives a flush/load as a ddrescue-safe comment, and on load a scoped
    // map's never-read rest is not pending (it is not this image's job).
    #[test]
    fn scope_round_trips_and_scopes_the_pending_stats() {
        let p = tmpfile("scope_round_trip");
        let mut mf = Mapfile::create(&p, 64 * 2048, "test").unwrap();
        mf.set_scope(vec![
            (32 * 2048, 8 * 2048),
            (0, 8 * 2048),
            (4 * 2048, 2 * 2048),
        ]);
        mf.record(0, 8 * 2048, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("# freemkv-scope: 0x0+0x4000,0x10000+0x4000"),
            "{text}"
        );
        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.scope(), Some(&[(0, 0x4000), (0x10000, 0x4000)][..]));
        let st = loaded.stats();
        assert_eq!((st.bytes_pending, st.bytes_nontried), (8 * 2048, 8 * 2048));
        assert_eq!(st.bytes_total, 64 * 2048);
        let mut widened = loaded;
        widened.clear_scope();
        assert_eq!(
            widened.stats().bytes_pending,
            56 * 2048,
            "the rest is pending again"
        );
        widened.flush().unwrap();
        assert!(
            !std::fs::read_to_string(&p)
                .unwrap()
                .contains("freemkv-scope")
        );
    }

    // Dropping a malformed scope would present a partial image as a whole one.
    #[test]
    fn a_malformed_scope_header_is_refused() {
        for bad in ["zz", "0x0", "0x0+", "0xffffffffffffffff+0x2"] {
            let p = tmpfile("scope_bad");
            std::fs::write(
                &p,
                format!("# freemkv-scope: {bad}\n0x0 ? 1\n0x0 0x800 ?\n"),
            )
            .unwrap();
            assert!(Mapfile::load(&p).is_err(), "{bad}");
        }
    }

    #[test]
    fn intersect_keeps_only_the_overlap() {
        assert_eq!(
            intersect(&[(0, 100)], &[(10, 5), (90, 20)]),
            [(10, 5), (90, 10)]
        );
        assert!(intersect(&[(0, 10)], &[(10, 5)]).is_empty());
        assert_eq!(merge_byte_ranges(&mut [(5, 5), (0, 5), (20, 0)]), [(0, 10)]);
    }

    #[test]
    fn parse_uk_line_rejects_malformed() {
        assert_eq!(parse_uk_line("no-colon"), None);
        assert_eq!(
            parse_uk_line("notanumber:11111111111111111111111111111111"),
            None
        );
        // 30 hex chars (15 bytes) — wrong length.
        assert_eq!(parse_uk_line("0:1111111111111111111111111111"), None);
        // Valid.
        assert_eq!(
            parse_uk_line("3:000102030405060708090a0b0c0d0e0f"),
            Some((3u32, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))
        );
    }

    /// parse_vid_hex tolerates an optional `0x` prefix and uppercase hex,
    /// but a 31- or 33-char string (not 32) is rejected — a VID is exactly
    /// 16 bytes = 32 hex chars.
    #[test]
    fn parse_vid_hex_length_and_case() {
        assert_eq!(
            parse_vid_hex("0xAABBCCDDEEFF00112233445566778899"),
            Some([
                0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
                0x88, 0x99
            ])
        );
        assert_eq!(parse_vid_hex(&"a".repeat(31)), None);
        assert_eq!(parse_vid_hex(&"a".repeat(33)), None);
    }

    // ── next_with / ranges_with semantics ─────────────────────────

    /// next_with returns the first matching range AT OR AFTER `from`,
    /// clipping the returned start to `from` when `from` lands inside a
    /// matching range (the patch loop relies on resuming mid-range).
    #[test]
    fn next_with_clips_start_to_from() {
        let p = tmpfile("next_with_clips");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(200, 300, SectorStatus::NonTrimmed).unwrap();
        // from inside the NonTrimmed range [200,500): start clips to 350,
        // size is 500-350 = 150.
        assert_eq!(
            mf.next_with(350, SectorStatus::NonTrimmed),
            Some((350, 150))
        );
        // from before the range: returns the whole range from its pos.
        assert_eq!(mf.next_with(0, SectorStatus::NonTrimmed), Some((200, 300)));
        // from at/after the range end: no match.
        assert_eq!(mf.next_with(500, SectorStatus::NonTrimmed), None);
        // status with no entries: None.
        assert_eq!(mf.next_with(0, SectorStatus::Unreadable), None);
        let _ = std::fs::remove_file(&p);
    }

    /// ranges_with matches ANY of the supplied statuses, preserving
    /// position order. Used to build the Pass-N retry queue (NonTrimmed +
    /// NonScraped together).
    #[test]
    fn ranges_with_multiple_statuses_in_order() {
        let p = tmpfile("ranges_with_multi");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(100, 100, SectorStatus::NonTrimmed).unwrap();
        mf.record(300, 100, SectorStatus::NonScraped).unwrap();
        mf.record(500, 100, SectorStatus::Unreadable).unwrap();
        let retry = mf.ranges_with(&[SectorStatus::NonTrimmed, SectorStatus::NonScraped]);
        assert_eq!(retry, vec![(100, 100), (300, 100)]);
        let _ = std::fs::remove_file(&p);
    }

    // ── record edge cases ─────────────────────────────────────────

    /// A zero-size record is a no-op (record() early-returns on size==0):
    /// entries and stats are unchanged.
    #[test]
    fn record_zero_size_is_noop() {
        let p = tmpfile("record_zero");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        let before = mf.entries().to_vec();
        mf.record(500, 0, SectorStatus::Finished).unwrap();
        assert_eq!(mf.entries(), before.as_slice());
        assert_eq!(mf.stats().bytes_good, 0);
        let _ = std::fs::remove_file(&p);
    }

    /// Recording the FULL disc with one status collapses to a single
    /// coalesced entry (record splits then merges adjacent same-status).
    #[test]
    fn record_full_span_coalesces_to_one_entry() {
        let p = tmpfile("record_full_span");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(0, 500, SectorStatus::Finished).unwrap();
        mf.record(500, 500, SectorStatus::Finished).unwrap();
        let es = mf.entries();
        assert_eq!(es.len(), 1, "two adjacent Finished must coalesce");
        assert_eq!((es[0].pos, es[0].size), (0, 1000));
        assert_eq!(mf.stats().bytes_good, 1000);
        let _ = std::fs::remove_file(&p);
    }

    /// A record that exactly overwrites the whole previous entry leaves the
    /// partition disjoint and total coverage invariant. bytes_total stays
    /// constant; good+pending+unreadable always sums to total.
    #[test]
    fn record_partition_invariant_total_coverage() {
        let p = tmpfile("record_invariant");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(0, 250, SectorStatus::Finished).unwrap();
        mf.record(250, 250, SectorStatus::Unreadable).unwrap();
        mf.record(500, 250, SectorStatus::NonTrimmed).unwrap();
        // NonTried (500..750? no) leftover is [750,1000).
        let s = mf.stats();
        assert_eq!(
            s.bytes_good + s.bytes_unreadable + s.bytes_pending,
            s.bytes_total,
            "coverage must partition the disc exactly"
        );
        // Entries must be disjoint and sorted.
        let es = mf.entries();
        for w in es.windows(2) {
            assert!(
                w[0].pos + w[0].size <= w[1].pos,
                "entries must stay disjoint and sorted"
            );
        }
        let _ = std::fs::remove_file(&p);
    }

    // ── load() current-line heuristic ─────────────────────────────

    /// load() skips the ddrescue "current pos" status line (2nd field is a
    /// status char, not a 0x size) and parses the data lines that follow.
    /// The header doc shows `0x000000000  ?  1  0` as the status line.
    #[test]
    fn load_skips_current_status_line() {
        let p = tmpfile("load_skips_current");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000100  0x00000100    -\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).unwrap();
        assert_eq!(mf.entries().len(), 2);
        assert_eq!(mf.entries()[0].status, SectorStatus::Finished);
        assert_eq!(mf.entries()[1].status, SectorStatus::Unreadable);
        let _ = std::fs::remove_file(&p);
    }

    /// A mapfile written WITHOUT a current-line (first non-comment line is
    /// already a data entry: 2nd field starts `0x`) must still parse that
    /// first line as an entry — the heuristic detects it and falls through.
    #[test]
    fn load_treats_leading_data_line_as_entry() {
        let p = tmpfile("load_leading_entry");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  0x00000200    +\n\
             0x000000200  0x00000100    ?\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).unwrap();
        // First line is NOT a status line; both lines are entries.
        assert_eq!(mf.entries().len(), 2);
        assert_eq!(mf.entries()[0].size, 0x200);
        let _ = std::fs::remove_file(&p);
    }

    // Regression: a leading DATA line with NO `0x` size prefix must still
    // parse as an entry, not get misclassified as the current-status line
    // and dropped (discriminator keys off field shape, not the prefix).
    #[test]
    fn load_treats_leading_data_line_without_0x_prefix_as_entry() {
        let p = tmpfile("load_leading_entry_no_0x");
        let _ = std::fs::remove_file(&p);
        // Note: sizes/positions written WITHOUT the `0x` prefix.
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             000000000  200    +\n\
             000000200  100    ?\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).unwrap();
        // The old `0x`-prefix heuristic would have skipped the first line as a
        // "current line" and lost a valid `+` entry. Both lines are entries.
        assert_eq!(mf.entries().len(), 2);
        assert_eq!(mf.entries()[0].size, 0x200);
        assert_eq!(mf.entries()[0].status, SectorStatus::Finished);
        assert_eq!(mf.entries()[1].status, SectorStatus::NonTried);
        let _ = std::fs::remove_file(&p);
    }

    /// load() parses the version from the `# Rescue Logfile. Created by`
    /// header and exposes it (round-trips through write_to_disk).
    #[test]
    fn load_parses_version_header() {
        let p = tmpfile("load_version");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by libfreemkv v9.9.9\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).unwrap();
        assert_eq!(mf.version, "libfreemkv v9.9.9");
        let _ = std::fs::remove_file(&p);
    }

    /// load() rejects an entry with a non-hex pos/size field
    /// (MapfileInvalid{kind:"hex"}) rather than silently skipping it —
    /// a corrupt data line must not be dropped, masking missing coverage.
    #[test]
    fn load_rejects_non_hex_field() {
        let p = tmpfile("load_nonhex");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0xZZZ  0x100    +\n",
        )
        .unwrap();
        assert!(Mapfile::load(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    // A truncated data line is REFUSED, not skipped (skipping would shrink total_size and hide
    // missing coverage).
    #[test]
    fn load_rejects_a_data_line_with_too_few_fields() {
        let p = tmpfile("load_shortline");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x0  ?  1  0\n\
             0x0        0x2800  +\n\
             0x2800     0x800\n",
        )
        .unwrap();
        let err = match Mapfile::load(&p) {
            Ok(map) => panic!(
                "a short line must not be skipped; loaded total_size={:#x} good={:#x}",
                map.total_size(),
                map.stats().bytes_good
            ),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    /// A single-field line is the same case one field shorter.
    #[test]
    fn load_rejects_a_data_line_with_one_field() {
        let p = tmpfile("load_onefield");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x0  ?  1  0\n\
             0x0        0x2800  +\n\
             0x2800\n",
        )
        .unwrap();
        let err = match Mapfile::load(&p) {
            Ok(_) => panic!("a one-field line must be refused, not skipped"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    // A malformed legacy VID header must FAIL the load, not drop
    // silently — that would turn "carries an identity" into "carries none",
    // which reopens the cross-disc resume splice the identity guard stops.
    #[test]
    fn load_rejects_a_malformed_vid_header() {
        let p = tmpfile("load_bad_vid");
        let _ = std::fs::remove_file(&p);
        // The legacy prefix is assembled so only `parse_legacy_key_lines` spells it (EK9).
        let vid_line = format!("# {}vid: 00112233445566778899aabbccddeezz", "freemkv-");
        std::fs::write(
            &p,
            format!(
                "# Rescue Logfile. Created by test\n{vid_line}\n0x0  ?  1  0\n0x0  0x800    +\n"
            ),
        )
        .unwrap();
        let err = match Mapfile::load(&p) {
            Ok(mf) => panic!(
                "a corrupt VID header must not load as 'no identity' (vidfp={:?})",
                mf.vid_fingerprint()
            ),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    /// load() rejects an unknown status char (MapfileInvalid{kind:
    /// "status_char"}). A `~` is not in the ddrescue alphabet.
    #[test]
    fn load_rejects_unknown_status_char() {
        let p = tmpfile("load_badstatus");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x100    ~\n",
        )
        .unwrap();
        let err = match Mapfile::load(&p) {
            Ok(_) => panic!("unknown status char must be rejected"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    /// An empty mapfile (only comments / blank lines) loads with zero
    /// entries and total_size 0 — never panics on the `entries.last()` None.
    #[test]
    fn load_empty_mapfile_is_zero_total() {
        let p = tmpfile("load_empty");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, "# Rescue Logfile. Created by test\n\n   \n").unwrap();
        let mf = Mapfile::load(&p).unwrap();
        assert!(mf.entries().is_empty());
        assert_eq!(mf.total_size(), 0);
        assert_eq!(mf.stats().bytes_total, 0);
        let _ = std::fs::remove_file(&p);
    }

    /// load() sorts entries by pos even when the file lists them out of
    /// order, and total_size derives from the highest end (entries are
    /// sorted then last().pos+size).
    #[test]
    fn load_sorts_out_of_order_entries() {
        let p = tmpfile("load_sort");
        let _ = std::fs::remove_file(&p);
        std::fs::write(
            &p,
            "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000200  0x00000100    -\n\
             0x000000000  0x00000200    +\n",
        )
        .unwrap();
        let mf = Mapfile::load(&p).unwrap();
        assert_eq!(mf.entries()[0].pos, 0);
        assert_eq!(mf.entries()[1].pos, 0x200);
        assert_eq!(mf.total_size(), 0x300);
        let _ = std::fs::remove_file(&p);
    }

    fn invalid_kind(e: &io::Error) -> Option<&'static str> {
        match e.get_ref()?.downcast_ref::<libfreemkv::error::Error>()? {
            libfreemkv::error::Error::MapfileInvalid { kind } => Some(kind),
            _ => None,
        }
    }

    fn load_text(tag: &str, text: &str) -> io::Result<Mapfile> {
        let p = tmpfile(tag);
        std::fs::write(&p, text).unwrap();
        let r = Mapfile::load(&p);
        let _ = std::fs::remove_file(&p);
        r
    }

    // M26: a data line is exactly `pos size status`, the status one character.
    #[test]
    fn load_refuses_a_malformed_status_field() {
        for (line, kind) in [
            ("0x0 0x800 +garbage", "status_char"),
            ("0x0 0x800 +?", "status_char"),
            ("0x0 0x800 + extra", "long_line"),
        ] {
            let e = load_text("load_status_field", &format!("0x0 ? 1\n{line}\n"))
                .map(|_| ())
                .unwrap_err();
            assert_eq!(invalid_kind(&e), Some(kind), "{line}: {e}");
        }
    }

    // ── write_to_disk format ──────────────────────────────────────
    // Entries round-trip through load() with the fixed header block
    // (Created by / Current pos / column header) intact for external tools.
    #[test]
    fn write_to_disk_format_round_trips_and_has_headers() {
        let p = tmpfile("write_format");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 0x1000, "vTEST").unwrap();
        mf.record(0x100, 0x200, SectorStatus::Finished).unwrap();
        mf.record(0x500, 0x100, SectorStatus::Unreadable).unwrap();
        mf.flush().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# Rescue Logfile. Created by vTEST"));
        assert!(text.contains("# Current pos / status / pass / pass_time"));
        assert!(text.contains("0x000000100  0x000000200    +"));
        assert!(text.contains("0x000000500  0x000000100    -"));
        let reloaded = Mapfile::load(&p).unwrap();
        assert_eq!(reloaded.entries(), mf.entries());
        let _ = std::fs::remove_file(&p);
    }

    /// create() persists immediately so a resume sees the fresh mapfile
    /// even if record() is never called (load right after create matches).
    #[test]
    fn create_persists_eagerly() {
        let p = tmpfile("create_eager");
        let _ = std::fs::remove_file(&p);
        let mf = Mapfile::create(&p, 4096, "test").unwrap();
        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.entries(), mf.entries());
        assert_eq!(loaded.total_size(), 4096);
        let _ = std::fs::remove_file(&p);
    }

    /// open_or_create returns a fresh NonTried mapfile when the path does
    /// not exist (NotFound → create), not an error.
    #[test]
    fn open_or_create_creates_when_absent() {
        let p = tmpfile("open_or_create_absent");
        let _ = std::fs::remove_file(&p);
        let mf = Mapfile::open_or_create(&p, 2048, "test").unwrap();
        assert_eq!(mf.entries().len(), 1);
        assert_eq!(mf.entries()[0].status, SectorStatus::NonTried);
        assert_eq!(mf.total_size(), 2048);
        let _ = std::fs::remove_file(&p);
    }

    /// open_or_create loads an existing file (and does NOT reset it to
    /// NonTried) even when the supplied total_size differs from the loaded
    /// coverage — the warn path must still return the loaded state.
    #[test]
    fn open_or_create_loads_existing_despite_size_mismatch() {
        let p = tmpfile("open_or_create_mismatch");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        mf.record(0, 500, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        // Supply a DIFFERENT total; must still load the existing entries.
        let reopened = Mapfile::open_or_create(&p, 999_999, "test").unwrap();
        assert_eq!(reopened.stats().bytes_good, 500);
        // Loaded total reflects the file, not the supplied arg.
        assert_eq!(reopened.total_size(), 1000);
        let _ = std::fs::remove_file(&p);
    }

    /// ONLY a missing file means "create a fresh one". A file that exists but
    /// does not parse is a real error and must propagate: creating over it
    /// silently DESTROYS the record of what was already read, and the rip
    /// restarts from sector 0 reporting a clean resume.
    #[test]
    fn open_or_create_propagates_a_corrupt_file_instead_of_overwriting_it() {
        let p = tmpfile("open_or_create_corrupt");
        let _ = std::fs::remove_file(&p);
        // Overlapping ranges — `load` rejects these (MapfileInvalid), which is
        // NOT io::ErrorKind::NotFound.
        let corrupt = "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000080  0x00000100    -\n";
        std::fs::write(&p, corrupt).unwrap();
        let err = Mapfile::open_or_create(&p, 0x100, "test")
            .err()
            .expect("a corrupt mapfile is an error, not a reason to start over");
        assert_ne!(
            err.kind(),
            io::ErrorKind::NotFound,
            "the file is present; only NotFound may route to create()"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            corrupt,
            "the unparseable mapfile was overwritten — the prior rip's progress is gone"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// Drop flushes pending in-memory state (a sweep that returns early
    /// must not lose records). After dropping a dirty Mapfile, a fresh
    /// load() sees the last record.
    #[test]
    fn drop_flushes_pending_state() {
        let p = tmpfile("drop_flush");
        let _ = std::fs::remove_file(&p);
        {
            let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
            // record may or may not flush (time-batched); ensure dirty.
            mf.record(0, 400, SectorStatus::Finished).unwrap();
            // Drop here flushes.
        }
        let loaded = Mapfile::load(&p).unwrap();
        assert_eq!(loaded.stats().bytes_good, 400);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stats_consistent_after_split_record() {
        let p = tmpfile("stats_consistent_after_split");
        let _ = std::fs::remove_file(&p);
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        // Mark middle as NonTrimmed
        mf.record(200, 400, SectorStatus::NonTrimmed).unwrap();
        // Entries: [0..200 NonTried, 200..600 NonTrimmed, 600..1000 NonTried]
        let s = mf.stats();
        assert_eq!(s.bytes_pending, 1000); // NonTried(600) + NonTrimmed(400)
        assert_eq!(s.bytes_retryable, 400); // NonTrimmed only
        assert_eq!(s.bytes_nontried, 600); // 200 + 400

        // Overwrite the NonTrimmed with Finished (splitting the remaining NonTried)
        mf.record(200, 400, SectorStatus::Finished).unwrap();
        // Entries: [0..200 NonTried, 200..600 Finished, 600..1000 NonTried]
        let s2 = mf.stats();
        assert_eq!(s2.bytes_good, 400);
        assert_eq!(s2.bytes_pending, 600); // NonTried(200 + 400)
        assert_eq!(s2.bytes_nontried, 600);
        assert_eq!(s2.bytes_retryable, 0);

        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod status_set_tests {
    use super::*;

    // The two sets differ by exactly `NonTried`, and neither may ever contain
    // `Finished`. Both used to be hand-written arrays scattered across five
    // call sites in three files — in different orders — this pins the relation.
    #[test]
    fn damage_set_is_the_bad_set_without_the_unread_remainder() {
        let bad = bad_sector_statuses();
        let damage = damage_sector_statuses();

        for s in damage {
            assert!(bad.contains(&s), "{s:?} is damage but not bad");
        }
        for s in bad {
            assert!(
                damage.contains(&s) || s == SectorStatus::NonTried,
                "{s:?} is bad but not damage, and is not the unread remainder"
            );
        }
        assert!(bad.contains(&SectorStatus::NonTried));
        assert!(!damage.contains(&SectorStatus::NonTried));
        for s in bad {
            assert!(!s.is_finished(), "{s:?} must not count as good");
        }
    }

    /// `is_finished` is the single arbiter of "confirmed good"; every other
    /// status must disagree with it.
    #[test]
    fn only_finished_is_finished() {
        assert!(SectorStatus::Finished.is_finished());
        for s in bad_sector_statuses() {
            assert!(!s.is_finished());
        }
    }
}

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
mod write_to_disk_cleanup_tests {
    use super::*;

    // A failed write must not leave `<path>.tmp` behind: every `?` between
    // `File::create(&tmp)` and the final `rename` used to orphan the tmp file.
    // Reachable via a directory sitting on the destination name (rename fails).
    #[test]
    fn a_failed_write_does_not_orphan_the_tmp_file() {
        let dir = std::env::temp_dir().join(format!("fmkv-tmpclean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Occupy the mapfile's own name with a non-empty DIRECTORY, so the
        // rename at the end of `write_to_disk` cannot succeed.
        let path = dir.join("m.mapfile");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        // `create` writes eagerly, so this exercises the failing path.
        let res = Mapfile::create(&path, 4096, "test");
        assert!(res.is_err(), "expected the rename onto a directory to fail");

        let tmp = {
            let mut s = path.clone().into_os_string();
            s.push(".tmp");
            std::path::PathBuf::from(s)
        };
        assert!(
            !tmp.exists(),
            "a partially-written tmp was left behind at {tmp:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod load_if_present_tests {
    use super::*;

    #[test]
    fn absent_is_none_not_an_error() {
        let dir = std::env::temp_dir().join(format!("fmkv-lip-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nope.map");
        let _ = std::fs::remove_file(&p);
        assert!(load_if_present(&p).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The distinction the three call sites kept re-deriving: a mapfile that
    /// EXISTS but cannot be parsed is an error, never an indistinguishable
    /// "nothing here".
    #[test]
    fn corrupt_is_an_error_not_none() {
        let dir = std::env::temp_dir().join(format!("fmkv-lip-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.map");
        std::fs::write(&p, b"# Rescue Logfile. Created by test\n0x00 0xZZZZ +\n").unwrap();
        match load_if_present(&p) {
            Err(e) => assert_ne!(
                e.kind(),
                io::ErrorKind::NotFound,
                "corruption must not masquerade as absence"
            ),
            Ok(_) => panic!("a corrupt mapfile must not load, nor read as absent"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Stamp `disc`'s identity on `map` (KU §4.1): its disc hash, and the fingerprint of its
/// Volume ID (the scanned disc's own, else the rip's key set's).
pub(crate) fn stamp_identity(
    map: &mut Mapfile,
    disc: &libfreemkv::Disc,
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
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
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
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
    pub(crate) fn of(
        disc: &libfreemkv::Disc,
        keys: Option<&libfreemkv::keys::ResolvedKeySet>,
    ) -> Self {
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
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
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
mod ku_identity_tests {
    //! KU-E1 mapfile rules (KU §4.1, §4.4, §4.5; J6): no key bytes and no raw VID on disk,
    //! legacy lines become fingerprints, and the identity check. The VID derives the keys
    //! (KS-16: "Kvu = AES-G(Km, IDv)"), so it is secret-class and only its hash is kept.

    use super::*;
    use crate::test_fixtures::{Answer, Calls, K1, K2, VID, bd_image, resolve};
    use libfreemkv::keys::{KeyScope, ResolveKeysOptions, ResolvedKeySet};

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn scratch(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(format!("{tag}.mapfile"));
        (dir, p)
    }

    /// The engine's fingerprints are exactly the set's (KU §4.1, J18): `SHA-256(
    /// "freemkv-vid-fp-v1" ‖ VID)` and `SHA-256("freemkv-key-fp-v1" ‖ key)[..8]`.
    #[test]
    fn fingerprints_match_the_key_set() {
        let fx = bd_image(&[Some(K1)], 1);
        let f = crate::test_fixtures::factory(&[(Answer::Keydb, &[K1])], &Calls::default());
        let set = ResolvedKeySet::resolve(
            &fx.disc,
            &mut fx.source(),
            KeyScope::Titles(vec![0]),
            &f,
            ResolveKeysOptions {
                vid: Some(VID),
                ..Default::default()
            },
        )
        .unwrap()
        .keys;
        assert_eq!(set.vid_fingerprint(), Some(vid_fingerprint(&VID)));
        assert_eq!(set.proven_key_fingerprints(), vec![key_fingerprint(&K1)]);
    }

    /// EK1 (KU §4.1, J6): "From KU-E1 the engine writes neither `# freemkv-uk:` nor a raw
    /// `# freemkv-vid:` line": only `# freemkv-disc:` and `# freemkv-vidfp:`, from a disc
    /// whose scan banked a key and whose VID is in hand.
    #[test]
    fn mapfile_never_writes_key_or_raw_vid_lines() {
        let mut fx = bd_image(&[Some(K1)], 1);
        let aacs = fx.disc.aacs.as_mut().unwrap();
        aacs.volume_id = VID;
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("disc.iso");
        let opts = crate::SweepOptions::default();
        crate::sweep(&fx.disc, &mut fx.source(), &iso, &opts).unwrap();
        let text = std::fs::read_to_string(mapfile_path_for(&iso)).unwrap();
        for banned in ["freemkv-uk", "freemkv-vid:", &hex(&K1), &hex(&VID)] {
            assert!(!text.contains(banned), "{banned} on disk:\n{text}");
        }
        let disc_hash =
            libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash)
                .to_ascii_lowercase();
        assert!(
            text.contains(&format!("# freemkv-disc: {disc_hash}\n")),
            "{text}"
        );
        let fp = hex(&vid_fingerprint(&VID));
        assert!(text.contains(&format!("# freemkv-vidfp: {fp}\n")), "{text}");
    }

    /// EK2 (KU §4.1, §4.5): legacy `# freemkv-uk:` / `# freemkv-vid:` lines load as
    /// fingerprints through `parse_legacy_key_lines`; a load alone writes nothing, and the
    /// first write scrubs them: no 32-hex key and no raw VID left in the file.
    #[test]
    fn legacy_lines_become_fingerprints_on_first_write() {
        let (_d, p) = scratch("legacy");
        let legacy = format!(
            "# Rescue Logfile. Created by freemkv 1.7\n\
             # freemkv-uk: 1:{}\n\
             # freemkv-uk: 2:{}\n\
             # freemkv-uk: 16777216:{}\n\
             # freemkv-uk: not-a-line\n\
             # freemkv-vid: {}\n\
             0x0  ?  1  0\n\
             0x0  0x1000    ?\n",
            hex(&K1),
            hex(&K2),
            hex(&[0x77; 16]),
            hex(&VID)
        );
        std::fs::write(&p, &legacy).unwrap();
        let mut mf = Mapfile::load(&p).expect("a malformed uk line never fails the load");
        assert_eq!(
            mf.legacy_key_fingerprints(),
            [key_fingerprint(&K1), key_fingerprint(&K2)],
            "base keys only (CPS id < 2^24); the malformed line is dropped"
        );
        assert_eq!(mf.vid_fingerprint(), Some(vid_fingerprint(&VID)));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            legacy,
            "load wrote nothing"
        );

        mf.record(0, 2048, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        for banned in [
            "freemkv-uk",
            "freemkv-vid:",
            &hex(&K1),
            &hex(&K2),
            &hex(&VID),
        ] {
            assert!(
                !text.contains(banned),
                "{banned} survived the scrub:\n{text}"
            );
        }
        for k in [K1, K2] {
            let line = format!("# freemkv-legacy-keyfp: {}\n", hex(&key_fingerprint(&k)));
            assert!(text.contains(&line), "{text}");
        }
        let again = Mapfile::load(&p).unwrap();
        assert_eq!(
            again.legacy_key_fingerprints(),
            mf.legacy_key_fingerprints()
        );
        assert_eq!(again.vid_fingerprint(), Some(vid_fingerprint(&VID)));
    }

    /// KU-X1, KU §4.4 rule 3: legacy key fingerprints are "checked **only if** the set proved
    /// at least one base key". With no set nothing is proven: disc-banked keys never stand in.
    /// Per spec; do not change without a spec citation proving otherwise.
    #[test]
    fn identity_without_a_set_reads_no_banked_keys() {
        let (_d, p) = scratch("no_set");
        let banked = disc_with(HASH_A, [0; 16]);
        assert!(DiscIdentity::of(&banked, None).proven.is_empty());
        let map = map_with(&p, None, None, &[K2]);
        assert!(check_mapfile_identity(&map, &banked, None).is_ok());
    }

    /// The new identity lines round-trip; a malformed one fails the load (dropping it would
    /// turn "names a disc" into "names none" and reopen the cross-disc resume splice).
    #[test]
    fn identity_lines_round_trip_and_a_malformed_one_is_refused() {
        let (_d, p) = scratch("identity");
        let mut mf = Mapfile::create(&p, 4096, "test").unwrap();
        mf.set_disc_hash("0xAABBCCDDEEFF00112233445566778899AABBCCDD");
        mf.set_vid_fingerprint(vid_fingerprint(&VID));
        mf.record(0, 2048, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        let back = Mapfile::load(&p).unwrap();
        assert_eq!(
            back.disc_hash(),
            Some("aabbccddeeff00112233445566778899aabbccdd")
        );
        assert_eq!(back.vid_fingerprint(), Some(vid_fingerprint(&VID)));
        assert_eq!(back.entries(), mf.entries());
        let text = std::fs::read_to_string(&p).unwrap();
        for (prefix, kind) in [("freemkv-disc: ", "disc"), ("freemkv-vidfp: ", "vidfp")] {
            let at = text.find(prefix).unwrap() + prefix.len();
            let mut bad = text.clone();
            bad.replace_range(at..at + 2, "zz");
            std::fs::write(&p, &bad).unwrap();
            let err = Mapfile::load(&p).map(|_| ()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{kind}");
        }
    }

    fn disc_with(hash: &str, vid: [u8; 16]) -> libfreemkv::Disc {
        let mut d = bd_image(&[Some(K1)], 1).disc;
        let a = d.aacs.as_mut().unwrap();
        a.disc_hash = hash.to_string();
        a.volume_id = vid;
        d
    }

    fn map_with(p: &Path, hash: Option<&str>, vid: Option<[u8; 16]>, keys: &[[u8; 16]]) -> Mapfile {
        let mut mf = Mapfile::create(p, 2048, "test").unwrap();
        if let Some(h) = hash {
            mf.set_disc_hash(h);
        }
        if let Some(v) = vid {
            mf.set_vid_fingerprint(vid_fingerprint(&v));
        }
        mf.legacy_keyfps = keys.iter().map(key_fingerprint).collect();
        mf
    }

    const HASH_A: &str = "0x1111111111111111111111111111111111111111";
    const HASH_B: &str = "0x2222222222222222222222222222222222222222";

    /// EK3 (KU §4.4): rule 1 disc hash, rule 2 `vidfp`, both only when both sides are known.
    #[test]
    fn identity_rules() {
        let (_d, p) = scratch("rules");
        let other_vid = [0x33; 16];
        let ok = |mf: &Mapfile, d: &libfreemkv::Disc| check_mapfile_identity(mf, d, None).is_ok();
        // Rule 1.
        assert!(ok(
            &map_with(&p, Some(HASH_A), None, &[]),
            &disc_with(HASH_A, [0; 16])
        ));
        assert!(!ok(
            &map_with(&p, Some(HASH_A), None, &[]),
            &disc_with(HASH_B, [0; 16])
        ));
        assert!(ok(
            &map_with(&p, None, None, &[]),
            &disc_with(HASH_B, [0; 16])
        ));
        // Rule 2: the disc's own VID, else the set's.
        assert!(ok(
            &map_with(&p, None, Some(VID), &[]),
            &disc_with(HASH_A, VID)
        ));
        assert!(!ok(
            &map_with(&p, None, Some(VID), &[]),
            &disc_with(HASH_A, other_vid)
        ));
        assert!(
            ok(
                &map_with(&p, None, Some(VID), &[]),
                &disc_with(HASH_A, [0; 16])
            ),
            "no VID in hand: cannot compare, not a mismatch"
        );
        // Neither side carries anything (a clear disc, a ddrescue import).
        let mut clear = disc_with(HASH_A, [0; 16]);
        clear.aacs = None;
        assert!(ok(&map_with(&p, None, None, &[]), &clear));
    }

    /// EK3 rule 3 (KU §4.4, coord 4): legacy key fingerprints are checked ONLY when the set
    /// proved a base key; a set that proved none (every piece Lazy or Clear) cannot check
    /// them, and that is not a mismatch.
    #[test]
    fn identity_rules_legacy_fingerprints_need_a_proven_key() {
        let (_d, p) = scratch("rule3");
        let fx = bd_image(&[Some(K1)], 1);
        let calls = Calls::default();
        let proved_k1 = resolve(
            &fx,
            KeyScope::Titles(vec![0]),
            &[(Answer::Keydb, &[K1])],
            &calls,
        )
        .unwrap();
        let clear = bd_image(&[None], 1);
        let proved_none = resolve(
            &clear,
            KeyScope::Titles(vec![0]),
            &[(Answer::Keydb, &[])],
            &calls,
        )
        .unwrap();
        assert!(proved_none.proven_key_fingerprints().is_empty());
        let d = &fx.disc;
        let check = |keys: &[[u8; 16]], set: Option<&ResolvedKeySet>| {
            check_mapfile_identity(&map_with(&p, None, None, keys), d, set).is_ok()
        };
        assert!(
            check(&[K1], Some(&proved_k1)),
            "a stored fingerprint matches"
        );
        assert!(check(&[K2, K1], Some(&proved_k1)));
        assert!(!check(&[K2], Some(&proved_k1)), "none matches a proven key");
        assert!(
            check(&[K2], Some(&proved_none)),
            "no proven key: cannot check"
        );
        assert!(check(&[K2], Some(&ResolvedKeySet::none())));
        assert!(
            check(&[], Some(&proved_k1)),
            "no stored fingerprint: nothing to check"
        );
    }
}
