//! Producer / consumer split for `Disc::patch`.
//!
//! Background: pre-0.18 patch ran strictly serial — single-sector
//! recovery read → seek + write recovered bytes → mapfile.record →
//! next iteration. The drive sat idle while the previous block's
//! recovered bytes were committed. On a damaged disc with many bad
//! sectors that adds up: per-sector write + mapfile.record costs a
//! handful of milliseconds each, which the drive could be using to
//! issue the next per-sector retry.
//!
//! This module decouples them. A consumer thread owns the
//! [`libfreemkv::io::WritebackFile`] (the ISO file) and the
//! [`super::mapfile::Mapfile`]. The producer thread (`Disc::patch`)
//! keeps the [`libfreemkv::sector::SectorSource`], the wedge / damage-window
//! state, the per-range watchdog, decrypt — so what enters the channel
//! is already-clean cleartext bytes (or an "Unreadable" terminal mark).
//!
//! Producer and consumer run concurrently; the channel uses
//! [`libfreemkv::io::pipeline::WRITE_THROUGH_DEPTH`] (=1) so back-pressure
//! kicks in immediately. We want the drive's per-sector retry budget
//! to stay in lockstep with the writer — sweep's `DEFAULT_PIPELINE_DEPTH`
//! (4) would let several sectors of recovered bytes queue up between
//! the producer's retry decisions and the writer, and patch's recovery
//! loop reads stats (`bytes_good`, range progress) inline to drive its
//! skip / wedge decisions. WRITE_THROUGH_DEPTH gives "read N+1 while
//! writing N", no further pipelining — exactly the model the producer
//! logic was written against.
//!
//! Correctness invariants preserved:
//! - Mapfile is single-writer (consumer-only). No locking on it.
//! - All recovery state (damage window, consecutive_failures, skip
//!   escalation, range watchdog) stays on the producer thread.
//! - `set_speed` calls happen on the producer thread (same thread that
//!   owns the `SectorSource`). No new SCSI concurrency.
//! - Per-iteration ordering of file-write → mapfile-record is kept
//!   intact in the consumer (write before record), so the on-disk
//!   invariant "mapfile only marks Finished what the file has received"
//!   survives a crash mid-pass.
//! - The BU40N+Initio bridge wedge concern is unchanged: only one
//!   SCSI command in flight at a time, error-path timing identical,
//!   no new retry logic. The threading primitive only overlaps the
//!   *write* with the *next read*; the per-sector single-shot read
//!   budget that the bridge wedge concern was originally about is
//!   untouched.
//!
//! Per-range watchdog (`range_sectors × SECONDS_PER_SECTOR`, capped at `RANGE_BUDGET_CAP_SECS`)
//! checks `bytes_good` for forward progress. With work in flight on
//! the consumer, the producer would otherwise see stale values; the
//! sink publishes a [`SharedPatchState`] snapshot after every record
//! so the producer's stall guards observe consumer side-effects with
//! at most one item of lag (which is fine — the watchdog uses minute-
//! scale budgets, not single-record latency).

use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use libfreemkv::error::{Error, Result};
use libfreemkv::io::pipeline::{Flow, Sink};

use super::mapfile::{self, MapStats, Mapfile, SectorStatus};
use super::section_recover::{
    Bisect, CachePrime, Direction, HandlerCtx, HandlerOutcome, HandlerScoreboard, Jump, Linear,
    Oscillate, ReadParams, RecoverySink, SectionHandler, SpeedPref, SpeedSweep, TimeoutPref,
    run_handlers,
};

/// Wall-clock budget one recovery handler gets on a section before the chain
/// moves to the next idea (#55). Tight and bounded — this is what guarantees a
/// pass never hangs: a handler that can't shrink the still-bad set within this
/// window returns, the next handler tries a different idea, and whatever is
/// still bad becomes NonTrimmed residue so recovery advances to the next range.
/// Replaces the old 1800 s/range + 3600 s/pass grind budgets on the live path.
const PER_HANDLER_BUDGET_SECS: u64 = 60;

/// Minimum interval between progress heartbeats pushed from inside a handler, so
/// the UI's bar/speed move continuously during a long section without flooding
/// the reporter (see the tick closure in `recover_section`).
const PROGRESS_TICK_MS: u64 = 250;

/// Bridges the decoupled [`RecoverySink`] a handler writes to onto the live
/// patch consumer pipe: each recovered span becomes a [`PatchItem::Recovered`]
/// the consumer thread seeks + writes + records `Finished`. `recovered` can't
/// return an error (the trait is infallible so handlers stay simple), so a
/// pipe-closed / halt error is captured in `err` and surfaced by the caller
/// after `run_handlers` returns.
struct PatchRecoverySink<'a> {
    pipe: &'a Pipeline<PatchItem, PatchSummary>,
    err: Option<Error>,
}

impl RecoverySink for PatchRecoverySink<'_> {
    fn recovered(&mut self, pos: u64, buf: &[u8]) {
        if self.err.is_some() {
            return;
        }
        if let Err(e) = send_or_abort(
            self.pipe,
            PatchItem::Recovered {
                pos,
                buf: buf.to_vec(),
            },
        ) {
            self.err = Some(e);
        }
    }
}

/// Item the producer hands to the patch consumer. One per per-sector
/// recovery decision.
pub(super) enum PatchItem {
    /// Sector / small batch successfully recovered (and decrypted on the
    /// producer side if `opts.decrypt` was set). Consumer seeks to
    /// `pos`, writes `buf`, records the range as `Finished`.
    Recovered { pos: u64, buf: Vec<u8> },

    /// Producer exhausted retries on `[pos, pos+len)`. Consumer records
    /// the range as `Unreadable`. No file write — the existing zero-fill
    /// from sweep is preserved in place.
    ///
    /// Currently unused by `Disc::patch` itself (2026-05-11 design call:
    /// patch never marks `Unreadable` mid-multipass; bytes stay
    /// `NonTrimmed` so future passes get another shot at them). Kept
    /// in the enum for the orchestrator-side end-of-recovery promotion
    /// (autorip, after the final retry pass completes, promotes
    /// still-NonTrimmed bytes to Unreadable). The orchestrator (autorip)
    /// performs this promotion directly via `Mapfile::record()` after all
    /// retry passes complete, not by emitting to `PatchSink`. This variant
    /// remains unused by the library itself.
    #[allow(dead_code)]
    Unreadable { pos: u64, len: u64 },

    /// Producer marks `[pos, pos+len)` as `NonTrimmed`. Used for BOTH
    /// the per-range skip-limit case (remaining bytes never tried) AND
    /// individual sector failures (tried-but-failed within a pass).
    /// Both stay "hopeful" — a later pass retries them.
    ///
    /// CRITICAL: "NonTrimmed in pass N" does NOT mean "Unreadable
    /// forever." Drive reads are stochastic: the same sector that
    /// fails 10 times in Pass 2 may succeed on attempt 1 in Pass 3
    /// after temperature / bus state / prior-read patterns shift.
    /// Pre-2026-05-11 patch marked individual failures Unreadable,
    /// which gave up on sectors that subsequent passes could have
    /// recovered (historical: ~36% of patch-marked Unreadable
    /// sectors turned out to be readable in re-rip experiments).
    /// Promotion to true Unreadable is the orchestrator's job,
    /// applied once after all retry passes complete.
    NonTrimmed { pos: u64, len: u64 },
}

/// Mapfile snapshot the sink republishes after every record so the
/// producer can drive its stall / progress logic without holding the
/// mapfile lock for long. `bad_ranges` is the DAMAGE set
/// (`NonTrimmed + Unreadable + NonScraped`) — NOT NonTried, which is the unread
/// remainder, not damage. Including NonTried inflated the live located drilldown
/// (at-risk movie time + range count) with unread sectors; excluding it matches
/// the one-shot progress path.
pub(super) struct SharedPatchState {
    pub stats: MapStats,
    pub bad_ranges: Vec<(u64, u64)>,
}

impl SharedPatchState {
    /// Cap on the republished `bad_ranges` Vec. Consumers (progress display,
    /// scheduler) only sample the head of the list. NOTE: there is no mapfile
    /// entry cap — `Mapfile.entries` is unbounded — so this truncation is the
    /// only thing keeping a pathologically fragmented disc from making every
    /// per-record republish allocate without limit.
    const MAX_BAD_RANGES: usize = 8192;

    fn from_map(map: &Mapfile) -> Self {
        let mut bad_ranges = map.ranges_with(&mapfile::damage_sector_statuses());
        bad_ranges.truncate(Self::MAX_BAD_RANGES);
        Self {
            stats: map.stats(),
            bad_ranges,
        }
    }
}

/// Final summary returned by [`Sink::close`] when the consumer drains
/// cleanly. Mirrors what the pre-split patch loop computed at the end
/// of the function — final mapfile stats plus whether `sync_all`
/// failed on a regular file (the only kind of fsync error patch ever
/// surfaced; `/dev/null` and pipes always fail `sync_all`, that's not
/// a real error).
pub(super) struct PatchSummary {
    pub stats: MapStats,
}

/// Consumer-side of the patch pipeline. Owns the ISO writeback file
/// and the mapfile; publishes a shared snapshot after every record so
/// the producer can read `bytes_good` for stall detection and
/// progress reporting.
pub(super) struct PatchSink {
    file: libfreemkv::io::WritebackFile,
    map: Mapfile,
    /// Whether the output is a regular file (so a `sync_all` failure
    /// is real). `/dev/null` etc. always fail `sync_all`; ignore those.
    is_regular: bool,
    /// Snapshot the producer reads. Updated after every successful
    /// `record()` call. `Mutex` rather than separate atomics because
    /// the producer wants stats + bad_ranges as a coherent pair.
    shared: Arc<Mutex<SharedPatchState>>,
    /// Last time the shared snapshot was republished. `from_map` allocates
    /// O(bad_ranges) every call, so the per-record path throttles to a time
    /// cadence (`REPUBLISH_CADENCE`); the final close always forces a publish.
    last_republish: Option<std::time::Instant>,
}

/// Minimum interval between per-record snapshot republishes.
const REPUBLISH_CADENCE: std::time::Duration = std::time::Duration::from_millis(250);

impl PatchSink {
    /// Open `path` as a [`libfreemkv::io::WritebackFile`] and pair it with
    /// `map` for the consumer. The producer holds onto the returned
    /// `Arc<Mutex<SharedPatchState>>` so it can poll mapfile state
    /// while the consumer is mutating it.
    pub(super) fn new(
        path: &std::path::Path,
        map: Mapfile,
        is_regular: bool,
    ) -> Result<(Self, Arc<Mutex<SharedPatchState>>)> {
        let file =
            libfreemkv::io::WritebackFile::open(path).map_err(|e| Error::IoError { source: e })?;
        let shared = Arc::new(Mutex::new(SharedPatchState::from_map(&map)));
        let shared_clone = shared.clone();
        Ok((
            Self {
                file,
                map,
                is_regular,
                shared,
                last_republish: None,
            },
            shared_clone,
        ))
    }

    /// Republish the shared snapshot. When `force` is false the update is
    /// throttled to `REPUBLISH_CADENCE`; `force` (used at close) always
    /// publishes the final state.
    fn republish(&mut self, force: bool) {
        let now = std::time::Instant::now();
        if !force
            && let Some(prev) = self.last_republish
            && now.duration_since(prev) < REPUBLISH_CADENCE
        {
            return;
        }
        self.last_republish = Some(now);
        self.publish_now();
    }

    fn publish_now(&self) {
        // Best-effort lock — only the producer reads, only the consumer
        // writes; contention is single-acquire so the lock is never
        // poisoned in practice. If it ever did get poisoned we'd want
        // the underlying error surfaced rather than silently swallowed,
        // so we propagate the poison panic rather than silently
        // continuing with stale shared state.
        let mut guard = self
            .shared
            .lock()
            .expect("PatchSink shared state mutex poisoned");
        *guard = SharedPatchState::from_map(&self.map);
    }
}

impl Sink<PatchItem> for PatchSink {
    type Output = PatchSummary;

    fn apply(&mut self, item: PatchItem) -> std::result::Result<Flow, Error> {
        match item {
            PatchItem::Recovered { pos, buf } => {
                let len = buf.len() as u64;
                self.file
                    .seek(SeekFrom::Start(pos))
                    .map_err(|e| Error::IoError { source: e })?;
                self.file
                    .write_all(&buf)
                    .map_err(|e| Error::IoError { source: e })?;
                self.map
                    .record(pos, len, SectorStatus::Finished)
                    .map_err(|e| Error::IoError { source: e })?;
            }
            PatchItem::Unreadable { pos, len } => {
                self.map
                    .record(pos, len, SectorStatus::Unreadable)
                    .map_err(|e| Error::IoError { source: e })?;
            }
            PatchItem::NonTrimmed { pos, len } => {
                self.map
                    .record(pos, len, SectorStatus::NonTrimmed)
                    .map_err(|e| Error::IoError { source: e })?;
            }
        }
        self.republish(false);
        Ok(Flow::Continue)
    }

    fn close(mut self) -> std::result::Result<Self::Output, Error> {
        // Drain in-flight writeback then issue a full fsync. A failure
        // here matters only on regular files — pipes / `/dev/null` etc.
        // always fail `sync_all`.
        if let Err(e) = self.file.sync_all() {
            if self.is_regular {
                tracing::warn!(
                    target: "freemkv::disc",
                    phase = "patch.sync.failed",
                    error = %e,
                    os_error = e.raw_os_error(),
                    error_kind = ?e.kind(),
                    "patch: sync_all failed"
                );
                return Err(Error::IoError { source: e });
            }
            tracing::debug!(
                target: "freemkv::disc",
                phase = "patch.sync.skipped",
                error = %e,
                "patch: sync_all failed for non-regular file; ignoring"
            );
        }
        self.map.flush().map_err(|e| Error::IoError { source: e })?;
        // Final republish so anyone reading the shared snapshot after
        // `Pipeline::finish` sees the post-flush state. (The producer
        // already has its own copy of the final `MapStats` in the
        // returned `PatchSummary`, but the snapshot is part of the
        // public-ish contract of the consumer: it stays current
        // through close.)
        self.republish(true);
        Ok(PatchSummary {
            stats: self.map.stats(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────
// Disc::patch + bytes_bad_in_title — extracted from disc/mod.rs in
// 0.20.1. Behavior unchanged; the move splits the 3,900-line mod.rs
// into a cleaner-to-read file.
// ─────────────────────────────────────────────────────────────────

use super::{PatchOptions, PatchOutcome};
use libfreemkv::disc::bytes_bad_in_title;
use libfreemkv::io::pipeline::Pipeline;
use libfreemkv::sector::SectorSource;

/// Breadth-first recovery tiers. Tier 0 fast-sweeps every bad range; tier 1
/// deep-recovers the residual; tier 2 runs the marginal specialists on whatever
/// tiers 0-1 leave (the true hardened residual). See `PatchCtx::run` and
/// `build_tier_handlers`.
const PATCH_TIERS: usize = 3;

/// Send a `PatchItem` and translate a `SendError` (consumer thread died
/// / panicked) into a library error so the caller propagates cleanly.
pub(super) fn send_or_abort(
    pipe: &Pipeline<PatchItem, PatchSummary>,
    item: PatchItem,
) -> Result<()> {
    pipe.send(item).map_err(|_| Error::PipelineConsumerGone)
}

/// Phase A pre-snapshot. Loads the mapfile, captures the fields the
/// patch loop needs after the live `Mapfile` moves into the consumer
/// thread (`bytes_good` baseline, total stats, entry snapshot for
/// the diagnostic dump, the initial bad-range work list, total work
/// in bytes, and the `is_regular` test that gates the post-pass
/// `sync_all` error policy). Returned `Mapfile` is the same object
/// that was loaded — caller passes ownership into `PatchSink::new`.
#[allow(clippy::type_complexity)]
pub(super) fn compute_initial_state(
    path: &std::path::Path,
    mapfile_path: &std::path::Path,
) -> Result<(
    Mapfile,
    MapStats,
    Vec<mapfile::MapEntry>,
    u64,
    Vec<(u64, u64)>,
    u64,
    bool,
)> {
    let map = mapfile::Mapfile::load(mapfile_path).map_err(|e| Error::IoError { source: e })?;
    let total_bytes = map.total_size();
    let initial_stats = map.stats();
    let initial_entries: Vec<_> = map.entries().to_vec();
    // Every retry pass acts on NonTrimmed, NonScraped, and Unreadable
    // ranges. Including Unreadable means a sector that failed in pass N
    // gets a fresh shot in pass N+1 — drive state evolves, the same
    // read can succeed later. Each pass owns its own jumps/skips; if
    // pass 5 jumps over the same zone as pass 2, fine. NonTried ranges
    // are intentionally excluded — they are covered by a preceding
    // sweep pass, not by patch.
    // NOT reversed for `opts.reverse`: `PatchCtx::run` sorts this list by
    // (size desc, pos asc) before walking it, and because `ranges_with` yields
    // disjoint runs every `pos` is unique — a total order. Any pre-ordering
    // here is therefore unobservable. A `bad_ranges.reverse()` lived here and
    // did nothing; `opts.reverse` now only labels the reported `PassKind`.
    let bad_ranges = map.ranges_with(&mapfile::damage_sector_statuses());
    let work_total: u64 = bad_ranges.iter().map(|(_, sz)| *sz).sum();
    // Fail SAFE when metadata is indeterminate: assume a regular file so a
    // real `sync_all` failure is surfaced, not swallowed. `/dev/null` and pipes
    // report success-with-non-file here (so they still correctly map to
    // `false`); only a genuine metadata error (e.g. transient NFS ESTALE) hits
    // the default, and for a data-integrity guard "surface the error" is the
    // right side to err on.
    let is_regular = super::output_is_regular(std::fs::metadata(path));
    Ok((
        map,
        initial_stats,
        initial_entries,
        total_bytes,
        bad_ranges,
        work_total,
        is_regular,
    ))
}

/// One recovery read of `[lba, lba+count)` into `buf[..count*2048]`.
///
/// On an AACS disc a mid-unit window (start or length not unit-aligned)
/// is widened to the enclosing aligned 3-sector unit, decrypted, and the
/// originally-requested window copied back out: the decrypting reader
/// rejects an unaligned read (`DecryptFailed`) and the sector would be
/// abandoned without the drive ever being asked. Units anchor at offset
/// 0, so the widened start is always unit-aligned. All recovery
/// accounting upstream (pos, block_bytes, dispatched lba/count) is
/// unchanged — only the physical read widens, so the cursor cannot
/// desync. `recovery` selects the SCSI timeout (true = 60 s deep recovery,
/// false = the fast path); `fua` forces the drive to bypass its readahead cache
/// and re-fetch
/// the medium (a Pass-N marginal-sector lever — see
/// [`libfreemkv::sector::SectorSource::read_sectors_fua`]).
pub(super) fn recovery_read<R: SectorSource + ?Sized>(
    reader: &mut R,
    decrypt_is_aacs: bool,
    lba: u32,
    count: u16,
    buf: &mut [u8],
    recovery: bool,
    fua: bool,
) -> Result<usize> {
    let bytes = count as usize * 2048;
    if decrypt_is_aacs && (!lba.is_multiple_of(3) || !count.is_multiple_of(3)) {
        const U: u32 = 3;
        let aligned_lba = lba - (lba % U);
        let head = (lba - aligned_lba) as usize; // lead-in sectors
        let span = head + count as usize;
        let aligned_count = span + ((U as usize - span % U as usize) % U as usize);
        let mut scratch = vec![0u8; aligned_count * 2048];
        reader.read_sectors_fua(
            aligned_lba,
            aligned_count as u16,
            &mut scratch,
            recovery,
            fua,
        )?;
        buf[..bytes].copy_from_slice(&scratch[head * 2048..head * 2048 + bytes]);
        Ok(bytes)
    } else {
        reader.read_sectors_fua(lba, count, &mut buf[..bytes], recovery, fua)
    }
}

/// The still-bad `[pos, len)` sub-ranges of one bad section, in byte offsets
/// (all multiples of 2048), kept sorted and non-overlapping. The per-section
/// recovery rework (#50) threads one of these through the recovery phase
/// helpers: each phase RECOVERS some bytes and calls [`SubRanges::remove`] to
/// shrink the set; whatever remains after all phases is the dead residue that
/// gets recorded NonTrimmed. Pure data structure — no I/O — so each phase
/// helper is unit-testable by asserting the residual `SubRanges`.
///
/// The residue tracker used by the phased `recover_section` orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SubRanges {
    /// (pos, len) pairs, sorted by pos, non-overlapping, all non-zero len.
    ranges: Vec<(u64, u64)>,
}

#[cfg_attr(not(test), allow(dead_code))]
/// Widen a mapfile byte-range outward to whole 2048-byte sectors.
///
/// This is the single ingress where mapfile byte-ranges become read requests,
/// and it is where the "all offsets are sector multiples" invariant that
/// `SubRanges` documents actually gets established. Nothing validated it
/// before: `Mapfile::load` has no alignment check, so an imported ddrescue
/// mapfile written with a 512-byte block size (`-b 512`) parses fine and
/// yields unaligned ranges.
///
/// Two things went wrong downstream without this:
///   * an unaligned `pos` — `read_span` does `lba = pos / SECTOR`, reads the
///     sector CONTAINING `pos`, then writes those 2048 real bytes at byte
///     offset `pos` and records them Finished. A shifted write of genuine
///     payload, marked good. Silent corruption.
///   * a sub-sector length — `count = (span / SECTOR) as u16` truncates to 0,
///     and a zero-sector read reports Good. (Harmless in the mapfile, since
///     `record` ignores a zero-size entry, but it credits the handler
///     scorecard for a recovery that never happened.)
///
/// Widening is strictly conservative: the extra bytes are re-read from the
/// disc and written with real data, so this recovers a 512-aligned mapfile
/// rather than condemning its fragments as permanently unreadable — which is
/// what rejecting them at load time, or failing the read here, would do.
fn snap_to_sectors(pos: u64, len: u64) -> (u64, u64) {
    super::snap_to_sectors(pos, len)
}

impl SubRanges {
    /// One whole bad section.
    pub(super) fn from_section(pos: u64, len: u64) -> Self {
        let ranges = if len == 0 {
            Vec::new()
        } else {
            vec![(pos, len)]
        };
        Self { ranges }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Total still-bad bytes across all sub-ranges.
    pub(super) fn total_len(&self) -> u64 {
        self.ranges.iter().map(|&(_, l)| l).sum()
    }

    pub(super) fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// Remove the recovered byte-range `[pos, pos+len)` from the bad set,
    /// splitting any sub-range it bisects and trimming any it overlaps. A
    /// range fully covered is dropped; a removal landing in a gap is a no-op.
    /// This is how a phase helper records "these bytes are no longer bad".
    pub(super) fn remove(&mut self, pos: u64, len: u64) {
        if len == 0 {
            return;
        }
        let rend = pos + len;
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.ranges.len() + 1);
        for &(rp, rl) in &self.ranges {
            let re = rp + rl;
            // Disjoint: keep whole.
            if rend <= rp || pos >= re {
                out.push((rp, rl));
                continue;
            }
            // Left remainder [rp, pos).
            if pos > rp {
                out.push((rp, pos - rp));
            }
            // Right remainder [rend, re).
            if rend < re {
                out.push((rend, re - rend));
            }
            // Otherwise the overlap consumed this whole sub-range.
        }
        self.ranges = out;
    }
}

/// Pre-loop diagnostic dump: emits `patch_mapfile_snapshot` plus the
/// first/last 10 entries (info + per-entry debug). Pure logging — no
/// state mutation. Pulled out of `Disc::patch` so the coordination
/// body stays compact; the operator's grep patterns for
/// `[disc] patch_mapfile_snapshot`, `patch_mapfile_entries_start`,
/// `patch_mapfile_entry_start`, `patch_mapfile_entries_end`,
/// `patch_mapfile_entry_end` are unchanged.
pub(super) fn log_patch_start_snapshot(
    initial_entries: &[mapfile::MapEntry],
    initial_stats: &mapfile::MapStats,
    bytes_good_before: u64,
) {
    tracing::info!(
        target: "freemkv::disc",
        phase = "patch.mapfile.snapshot",
        total_entries = initial_entries.len(),
        bytes_good_before,
        bytes_retryable = initial_stats.bytes_retryable,
        bytes_unreadable = initial_stats.bytes_unreadable,
        bytes_nontried = initial_stats.bytes_nontried,
        "Mapfile state snapshot at patch start"
    );

    if !initial_entries.is_empty() {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.mapfile.entries.start",
            num_to_log = (initial_entries.len().min(10)) as u32,
            "First 10 entries"
        );
        for entry in initial_entries.iter().take(10) {
            tracing::debug!(
                target: "freemkv::disc",
                phase = "patch.mapfile.entry.start",
                pos_hex = format!("0x{:09x}", entry.pos),
                size_mb = entry.size as f64 / 1_048_576.0,
                status_char = entry.status.to_char() as u8 as i32,
                "Mapfile entry"
            );
        }
    }
    if initial_entries.len() > 10 {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.mapfile.entries.end",
            num_to_log = (initial_entries.len().min(10)) as u32,
            "Last 10 entries"
        );
        for entry in initial_entries.iter().skip(initial_entries.len() - 10) {
            tracing::debug!(
                target: "freemkv::disc",
                phase = "patch.mapfile.entry.end",
                pos_hex = format!("0x{:09x}", entry.pos),
                size_mb = entry.size as f64 / 1_048_576.0,
                status_char = format!("{}", entry.status.to_char()),
                "Mapfile entry"
            );
        }
    }
}

/// Bundle final mapfile stats + accumulated loop counters into the
/// public `PatchOutcome` the caller consumes. The post-loop tracing
/// (`patch_iso_size_end`, `patch_done`) is also emitted here so the
/// coordination body has one less inline stanza.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_outcome(
    state: &PatchLoopState,
    summary: &PatchSummary,
    path: &std::path::Path,
    total_bytes: u64,
    num_ranges: usize,
    wedged_threshold: u64,
) -> PatchOutcome {
    let stats = summary.stats;

    if let Ok(metadata) = std::fs::metadata(path) {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.iso_size.end",
            iso_bytes = metadata.len(),
            bytes_recovered = stats.bytes_good.saturating_sub(state.bytes_good_before),
            "ISO file size at patch end"
        );
    }

    tracing::info!(
        target: "freemkv::disc",
        phase = "patch.done",
        wedged_exit = state.wedged_exit,
        halted = state.halted,
        bytes_recovered = stats.bytes_good.saturating_sub(state.bytes_good_before),
        final_bytes_good = stats.bytes_good,
        final_bytes_unreadable = stats.bytes_unreadable,
        final_bytes_pending = stats.bytes_pending,
        total_ranges_processed = num_ranges,
        "Disc::patch returning"
    );

    PatchOutcome {
        bytes_total: total_bytes,
        bytes_good: stats.bytes_good,
        bytes_unreadable: stats.bytes_unreadable,
        bytes_pending: stats.bytes_pending,
        bytes_recovered_this_pass: stats.bytes_good.saturating_sub(state.bytes_good_before),
        halted: state.halted,
        wedged_exit: state.wedged_exit,
        wedged_threshold,
    }
}

/// Per-pass loop state, accumulated across every range and every read
/// inside `Disc::patch`. Lives on the producer thread; helpers take
/// `&mut PatchLoopState` so they can mutate counters and per-range
/// scratch without an explosion of parameters at the call site.
pub(super) struct PatchLoopState {
    // Counters
    pub halted: bool,
    pub wedged_exit: bool,
    // Clock seam: the handler chain reads wall time through this rather than
    // calling `Instant::now()` inline, so the per-handler deadline is driven by
    // an injectable clock and deterministic tests can wind it forward.
    pub now: fn() -> std::time::Instant,
    // Snapshot at construction — these stay constant for the whole pass.
    pub bytes_good_before: u64,
    #[allow(dead_code)]
    pub total_bytes: u64,
    pub initial_batch: u16,
    pub work_total: u64,
}

impl PatchLoopState {
    pub(super) fn new(
        bytes_good_before: u64,
        total_bytes: u64,
        initial_batch: u16,
        work_total: u64,
    ) -> Self {
        // Production clock: the real monotonic wall clock.
        Self::new_with_clock(
            bytes_good_before,
            total_bytes,
            initial_batch,
            work_total,
            std::time::Instant::now,
        )
    }

    /// Like `new`, but with an injectable monotonic clock so a test can wind a
    /// fake clock forward to drive the per-handler deadline deterministically.
    /// `new` passes `Instant::now`, so the production path is unchanged.
    pub(super) fn new_with_clock(
        bytes_good_before: u64,
        total_bytes: u64,
        initial_batch: u16,
        work_total: u64,
        now: fn() -> std::time::Instant,
    ) -> Self {
        Self {
            halted: false,
            wedged_exit: false,
            now,
            bytes_good_before,
            total_bytes,
            initial_batch,
            work_total,
        }
    }
}

/// Why [`PatchCtx::patch_region`] returned. The orchestrator
/// ([`PatchCtx::run`]) advances to the next bad range on `Completed` (the
/// handler chain always drains a section to recovered-or-residue, so there is
/// no per-range abort), and ends the whole pass only on `Halted` or
/// `TransportFault` — for which the matching `state.halted` / `state.wedged_exit`
/// flag was already set, so `build_outcome` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionOutcome {
    /// Section drained: recovered what was readable, left the rest NonTrimmed.
    Completed,
    /// Halt requested — the halt token or the progress reporter.
    /// `state.halted` is set.
    Halted,
    /// USB-bridge transport fault: a dead bus, not a bad sector.
    /// `state.wedged_exit` is set.
    TransportFault,
}

/// Per-pass coordination state for one `Disc::patch` run: the decrypting
/// reader, the consumer pipe + its shared mapfile snapshot, the options,
/// and the accumulating [`PatchLoopState`]. Bundling these lets the
/// orchestrator ([`PatchCtx::run`]) and the focused per-range recovery
/// loop ([`PatchCtx::patch_region`]) be methods rather than free
/// functions threading a dozen arguments. `state` carries ACROSS ranges
/// (counters, stall timers, NOT_READY/last-skip cursors); the per-range
/// scratch inside it is reset at the top of each `patch_region`.
struct PatchCtx<'a, 'o> {
    disc: &'a libfreemkv::Disc,
    reader: &'a mut dyn SectorSource,
    pipe: &'a Pipeline<PatchItem, PatchSummary>,
    shared: &'a Mutex<SharedPatchState>,
    opts: &'a PatchOptions<'o>,
    total_bytes: u64,
    decrypt_is_aacs: bool,
    state: PatchLoopState,
    /// Per-rip handler scorecard: grades handlers by recovery rate so the
    /// coordinator runs the winners first and lets duds fall back. Reset per
    /// pass (ephemeral, no persistence).
    scoreboard: HandlerScoreboard,
    /// Consecutive wedge-family senses across the WHOLE pass. Seeded into each
    /// per-section `HandlerCtx` and read back after, so a drive fast-fail wedge is
    /// detected even when every bad sub-range is smaller than the abort streak.
    wedge_streak: u32,
}

/// Build the handler chain for one breadth-first tier. Each config is named by
/// its FULL parameterisation (`build_tier_handlers` picks the roster; the
/// scorecard re-orders WITHIN a tier per rip). The engine hardcodes no
/// conclusion: every technique is always present at its tier, and a technique
/// that doesn't fit this disc self-deprioritises (scores low, yields after 4
/// unproductive reads) rather than being removed.
///
/// - **Tier 0 — fast scouts** (`fast`: max speed, 10 s, cache on): grab the
///   readable bulk across every range.
/// - **Tier 1 — slow-deep** (`deep`: max speed, 60 s ECC budget): deep-recover
///   the easy residual.
/// - **Tier 2 — marginal specialists**: the physical-failure-mode matrix
///   (SlowSpin / FuaRetry / SlowFua / CachePrime / Oscillate / SpeedSweep), run
///   ONLY on what tiers 0-1 leave.
fn build_tier_handlers(tier: usize) -> Vec<Box<dyn SectionHandler>> {
    match tier {
        // Tier 0 — fast scouts. Bisect leads by default (probing a range's
        // MIDDLE finds a readable island in one read); Jump blows through large
        // dead runs; the fast linear sweeps mop up. The scorecard re-orders.
        0 => vec![
            Box::new(Bisect {
                params: ReadParams::fast(),
            }),
            Box::new(Jump {
                params: ReadParams::fast(),
            }),
            Box::new(Linear {
                direction: Direction::Reverse,
                params: ReadParams::fast(),
            }),
            Box::new(Linear {
                direction: Direction::Forward,
                params: ReadParams::fast(),
            }),
        ],
        // Tier 1 — slow deep recovery on the small residue tier 0 leaves.
        1 => vec![
            Box::new(Linear {
                direction: Direction::Reverse,
                params: ReadParams::deep(),
            }),
            Box::new(Linear {
                direction: Direction::Forward,
                params: ReadParams::deep(),
            }),
        ],
        // Tier 2 — marginal specialists, run ONLY on the hardened residual that
        // tiers 0-1 leave. Each targets ONE physical failure mode. They are all
        // NEW configs, so the scorecard calibrates each once then ranks by its
        // decayed rate — a specialist that doesn't fit THIS disc self-
        // deprioritises (scores low, yields after 4 unproductive reads) and one
        // that starts landing sectors climbs. Every read is a wedge-safe
        // `read_span`, so they inherit the wedge-abort / unproductive-yield /
        // deadline bounds for free. Additive: tiers 0-1 are untouched.
        _ => {
            // Slower spindle (more servo dwell + ECC integration per sector).
            let min_deep = ReadParams {
                speed: SpeedPref::Min,
                fua: false,
                timeout: TimeoutPref::Deep,
            };
            // Cache-bypass physical re-read (stochastic marginal sectors).
            let fua_deep = ReadParams {
                speed: SpeedPref::Max,
                fua: true,
                timeout: TimeoutPref::Deep,
            };
            // Both levers for the hardest sectors (min spindle AND cache-bypass).
            let slow_fua = ReadParams {
                speed: SpeedPref::Min,
                fua: true,
                timeout: TimeoutPref::Deep,
            };
            vec![
                // SlowSpin: Linear fwd + rev at min speed.
                Box::new(Linear {
                    direction: Direction::Reverse,
                    params: min_deep,
                }),
                Box::new(Linear {
                    direction: Direction::Forward,
                    params: min_deep,
                }),
                // FuaRetry: Linear fwd + rev + Bisect under FUA (multiple physical
                // attempts per marginal sector).
                Box::new(Linear {
                    direction: Direction::Forward,
                    params: fua_deep,
                }),
                Box::new(Linear {
                    direction: Direction::Reverse,
                    params: fua_deep,
                }),
                Box::new(Bisect { params: fua_deep }),
                // SlowFua: the hardest sector — min speed AND FUA.
                Box::new(Linear {
                    direction: Direction::Forward,
                    params: slow_fua,
                }),
                // CachePrime: warm the channel on the preceding good run first.
                Box::new(CachePrime {
                    params: ReadParams::deep(),
                }),
                // Oscillate: alternate approach direction, at max and at min.
                Box::new(Oscillate {
                    params: ReadParams::deep(),
                }),
                Box::new(Oscillate { params: min_deep }),
                // SpeedSweep: per-sector Max→Min speed search.
                Box::new(SpeedSweep {
                    params: ReadParams::deep(),
                }),
            ]
        }
    }
}

/// The FLAT handler pool — every technique×parameterization from all tiers in
/// ONE chain, no tier gate. `run_handlers` sorts it best-first by the rip
/// scorecard on every range, so this is a data-driven bandit: the first ranges
/// try them all (explore), then the decayed-yield ranking floats whatever is
/// actually landing sectors to the front (exploit), re-measured per range. A
/// handler that doesn't fit stays last but is never dropped (floor → it can
/// still revive if the residual's character shifts). No fixed ordering, no
/// "start tier" — the data picks the order. Enabled by `FREEMKV_PATCH_FLAT`;
/// unset keeps the proven tier ladder.
fn build_flat_pool() -> Vec<Box<dyn SectionHandler>> {
    let mut pool = Vec::new();
    for tier in 0..PATCH_TIERS {
        pool.extend(build_tier_handlers(tier));
    }
    pool
}

/// The batch label carried through the pass. `block_sectors` no longer sizes
/// any read (the handler chain owns read sizing); it survives ONLY as this
/// label, and the clamp keeps `Some(0)` from reading as "scrape".
pub(super) fn initial_batch_of(opts: &PatchOptions) -> u16 {
    opts.block_sectors.unwrap_or(1).max(1)
}

/// The pass label the front end renders for a patch pass.
///
/// This is the one observable consequence `block_sectors` and `reverse` still
/// have: a single-sector batch is reported as a SCRAPE pass, anything larger as
/// a TRIM pass, and `reverse` decorates whichever one it is. Shared by
/// `report_patch_progress` (every progress tick) so the label cannot be
/// asserted against a re-implementation of the rule.
pub(super) fn pass_kind(initial_batch: u16, reverse: bool) -> libfreemkv::progress::PassKind {
    if initial_batch == 1 {
        libfreemkv::progress::PassKind::Scrape { reverse }
    } else {
        libfreemkv::progress::PassKind::Trim { reverse }
    }
}

/// True when the flat-pool bandit scheduler is requested (`FREEMKV_PATCH_FLAT`
/// set to anything but empty / `0`). Default (unset) keeps the tier ladder.
fn patch_flat_mode() -> bool {
    std::env::var("FREEMKV_PATCH_FLAT")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Short per-handler EXPLORE budget for the flat bandit (seconds). Keeps any one
/// handler from hogging a range so all 16 get a fast turn and the scorecard
/// learns quickly. `FREEMKV_PATCH_FLAT_BUDGET` overrides; default 12 s, floored
/// at 1.
fn flat_handler_budget_secs() -> u64 {
    std::env::var("FREEMKV_PATCH_FLAT_BUDGET")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(12)
        .max(1)
}

impl PatchCtx<'_, '_> {
    /// Orchestrator (one pass): walk the ordered bad ranges. Apply the
    /// inter-range cooldown only after a range that grinded, then recover
    /// the range; stop the whole pass the moment a range reports
    /// halt / wedge / transport-fault.
    fn run(&mut self, bad_ranges: &[(u64, u64)]) -> Result<()> {
        let num_ranges = bad_ranges.len();
        // Attack the LARGEST ranges first. The big NonTrimmed regions are usually
        // sweep-jump over-marks that read straight back, so ordering them ahead of
        // the many tiny dead fragments lets tier 0 recover the bulk of the disc in
        // its first minutes instead of grinding fragments first (ties: low LBA
        // first for a predictable, mostly-sequential walk).
        let mut ordered: Vec<(u64, u64)> = bad_ranges.to_vec();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        // Per-range still-bad sets, persisted ACROSS the breadth-first tiers so
        // tier N+1 works on exactly what tier N left behind.
        let mut sections: Vec<SubRanges> = ordered
            .iter()
            .map(|&(p, l)| {
                let (p, l) = snap_to_sectors(p, l);
                SubRanges::from_section(p, l)
            })
            .collect();

        // Two schedulers select the handler chain per range:
        //
        // FLAT bandit (`FREEMKV_PATCH_FLAT`): ONE range walk, the full flat pool
        // per range. `run_handlers` orders it best-first by the live scorecard,
        // so the data — not a fixed tier order — decides what runs first. Right
        // for a hardened residual (late resume): the specialists get a shot on
        // every range immediately instead of waiting out a full bucket→mug sweep.
        //
        // TIER ladder (default): tier 0 fast-sweeps EVERY range first — grabbing
        // the easily-readable bulk across the whole disc (sweep-jump over-marks a
        // big region NonTrimmed without testing each sector, so most reads back in
        // seconds) — BEFORE any slow per-sector grind; tiers 1-2 escalate onto the
        // residue. Right for a FRESH rip (flood still present). This is also the
        // fix for the OLD depth-first starvation bug (full chain per range burned
        // ~5 min on a front cluster and starved the big recoverable ranges) — but
        // the new handlers self-limit (yield after 4 dead reads), so the flat
        // scheduler no longer hits that.
        if patch_flat_mode() {
            for (range_idx, &(range_pos, range_size)) in ordered.iter().enumerate() {
                if sections[range_idx].is_empty() {
                    continue;
                }
                // Single flat pass: this IS the final (only) tier for the range,
                // so surviving residue is recorded NonTrimmed for the next pass.
                let outcome = self.recover_section(
                    0,
                    range_idx,
                    num_ranges,
                    range_pos,
                    range_size,
                    &mut sections[range_idx],
                    /* final_tier */ true,
                    /* flat */ true,
                )?;
                match outcome {
                    RegionOutcome::Completed => {}
                    RegionOutcome::Halted | RegionOutcome::TransportFault => return Ok(()),
                }
            }
            return Ok(());
        }
        for tier in 0..PATCH_TIERS {
            let final_tier = tier + 1 == PATCH_TIERS;
            for (range_idx, &(range_pos, range_size)) in ordered.iter().enumerate() {
                if sections[range_idx].is_empty() {
                    continue; // already fully recovered by an earlier tier
                }
                let outcome = self.recover_section(
                    tier,
                    range_idx,
                    num_ranges,
                    range_pos,
                    range_size,
                    &mut sections[range_idx],
                    final_tier,
                    /* flat */ false,
                )?;
                match outcome {
                    RegionOutcome::Completed => {}
                    RegionOutcome::Halted | RegionOutcome::TransportFault => return Ok(()),
                }
            }
        }
        Ok(())
    }

    /// Run ONE breadth-first tier of the handler chain over one range's still-bad
    /// set `bad`. Tier 0 = the fast breadth handlers (grab the readable bulk,
    /// fast-fail the rest); tier 1 = deep recovery (slow reads) + bisect on the
    /// residual. `final_tier` records the surviving residue as NonTrimmed and
    /// accounts the range toward progress exactly once. Cross-range scheduling
    /// lives in [`PatchCtx::run`]; this owns one (tier, range) unit of work.
    #[allow(clippy::too_many_arguments)]
    fn recover_section(
        &mut self,
        tier: usize,
        range_idx: usize,
        num_ranges: usize,
        range_pos: u64,
        range_size: u64,
        bad: &mut SubRanges,
        final_tier: bool,
        flat: bool,
    ) -> Result<RegionOutcome> {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.region.enter",
            tier,
            flat,
            range_index = range_idx,
            num_total_ranges = num_ranges,
            range_lba = range_pos / 2048,
            range_size_mb = range_size as f64 / 1_048_576.0,
            bad_bytes = bad.total_len(),
            "entering patch range"
        );

        // Enter at max read speed. A handler picks its own speed / FUA / timeout
        // via its `ReadParams`; `read_span` restores max after each handler, so
        // every tier starts from the streaming default.
        self.reader.set_speed(0xFFFF);

        // Handler roster. FLAT mode: the whole pool (all techniques) in one
        // chain — `run_handlers` orders it best-first by the rip scorecard, so
        // the data picks what runs first. TIER mode: just this tier's roster
        // (tier 0 fast scouts, 1 slow-deep, 2 marginal specialists), likewise
        // scorecard-ordered within the tier. Either way the scorecard re-learns
        // per disc.
        let mut handlers: Vec<Box<dyn SectionHandler>> = if flat {
            build_flat_pool()
        } else {
            build_tier_handlers(tier)
        };

        // Clock seam: handlers read wall time through this so tests can wind a
        // fake clock (the same seam the pass uses for its own timing).
        let now_ptr = self.state.now;
        let now_fn = move || now_ptr();

        let mut sink = PatchRecoverySink {
            pipe: self.pipe,
            err: None,
        };

        let bad_before = bad.total_len();
        // Pass-local cancel latch. The progress tick below already asks the
        // front-end `should_cancel()` every PROGRESS_TICK_MS, but its answer
        // used to be discarded (`let _ = ...`), so the ONLY halt check a
        // handler chain could see was `opts.halt` — which every caller in this
        // crate except `extract` leaves as None. A Stop therefore had to wait
        // out the remaining per-handler budgets (up to ~10 min on one bad
        // range) before `report_patch_progress` was consulted at the section
        // boundary. Latching the tick's answer here, and handing it to the
        // handler chain as its halt token, makes cancellation land at the very
        // next inter-read check instead.
        let pass_cancel = std::sync::atomic::AtomicBool::new(false);
        let (outcome, wedge_after) = {
            // Progress heartbeat: a throttled closure that pushes a fresh
            // snapshot to the reporter as recovery happens (called from every
            // read via `HandlerCtx::progress`), so the bar and speed move DURING
            // a handler instead of only when a section finishes. Scoped to this
            // block so its borrow of `self.state` ends before the post-tier
            // accounting below.
            let disc = self.disc;
            let opts = self.opts;
            let shared = self.shared;
            let total_bytes = self.total_bytes;
            let state = &self.state;
            // `None` = no tick yet, so the FIRST read ticks immediately
            // instead of waiting out PROGRESS_TICK_MS. That makes a cancel
            // observable on read 1 (rather than up to 250 ms in) and gets the
            // progress bar moving at the start of a section instead of a
            // quarter-second later.
            let last_tick: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
            let cancel = &pass_cancel;
            let ext_halt = self.opts.halt.as_deref();
            let mut tick = move || {
                // Mirror an externally-supplied halt on EVERY read, not just on
                // a throttled tick, so a caller that does wire `opts.halt` is
                // not made less responsive by routing through this latch.
                if ext_halt.is_some_and(|h| h.load(std::sync::atomic::Ordering::Relaxed)) {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                let t = now_ptr();
                let due = match last_tick.get() {
                    None => true,
                    Some(prev) => {
                        t.duration_since(prev) >= std::time::Duration::from_millis(PROGRESS_TICK_MS)
                    }
                };
                if due {
                    last_tick.set(Some(t));
                    if report_patch_progress(disc, state, opts, total_bytes, shared) {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            };
            let mut ctx = HandlerCtx {
                reader: &mut *self.reader,
                sink: &mut sink,
                now: &now_fn,
                // The latch, not `opts.halt` directly: it carries BOTH the
                // external token (mirrored above) and the front-end's
                // `should_cancel()` answer from the progress tick.
                halt: Some(&pass_cancel),
                decrypt_is_aacs: self.decrypt_is_aacs,
                tick: Some(&mut tick),
                unproductive: 0,
                // Carry the pass-level wedge streak in so a fast-fail wedge is
                // caught across many small sections, not reset each one.
                wedge_streak: self.wedge_streak,
                // Drive was just reset to max above; read_span tracks changes.
                cur_speed: 0xFFFF,
            };
            // Per-handler time budget. FLAT mode is EXPLORE-first: give each
            // handler only a short slice so all 16 get a turn on the range
            // quickly ("test all quick"), and the scorecard learns which land
            // bytes — a winner then earns more cumulative time across ranges and
            // passes. TIER mode keeps the full 60 s deep-recovery window. Both
            // env-tunable via `FREEMKV_PATCH_FLAT_BUDGET`.
            let budget_secs = if flat {
                flat_handler_budget_secs()
            } else {
                PER_HANDLER_BUDGET_SECS
            };
            let o = run_handlers(&mut ctx, &mut handlers, bad, &mut self.scoreboard, |_bad| {
                now_ptr() + std::time::Duration::from_secs(budget_secs)
            });
            (o, ctx.wedge_streak)
        };
        self.wedge_streak = wedge_after;

        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.region.exit",
            tier,
            range_index = range_idx,
            range_lba = range_pos / 2048,
            outcome = ?outcome,
            bad_bytes_before = bad_before,
            bad_bytes_after = bad.total_len(),
            recovered = bad_before.saturating_sub(bad.total_len()),
            "region tier finished"
        );

        // A pipe-closed / halt error captured while emitting recovered spans is
        // fatal to the pass.
        if let Some(e) = sink.err.take() {
            return Err(e);
        }

        // On the FINAL tier, whatever is still bad is this pass's residue: record
        // NonTrimmed and account the range toward progress (once). A later pass —
        // or a future handler — gets another shot; the orchestrator promotes
        // still-NonTrimmed to Unreadable only after the final pass completes.
        if final_tier {
            for &(pos, len) in bad.ranges() {
                send_or_abort(self.pipe, PatchItem::NonTrimmed { pos, len })?;
            }
        }

        if report_patch_progress(
            self.disc,
            &self.state,
            self.opts,
            self.total_bytes,
            self.shared,
        ) {
            self.state.halted = true;
            return Ok(RegionOutcome::Halted);
        }

        match outcome {
            // Whether the chain cleared the section or left residue, we always
            // advance to the next range — never hang, never abort mid-pass.
            HandlerOutcome::Complete | HandlerOutcome::Remaining => Ok(RegionOutcome::Completed),
            HandlerOutcome::Halted => {
                self.state.halted = true;
                Ok(RegionOutcome::Halted)
            }
            // Bridge/transport crash: end the pass so the orchestrator can
            // spin-cycle the drive and resume from the mapfile next pass.
            HandlerOutcome::TransportFault => {
                self.state.wedged_exit = true;
                Ok(RegionOutcome::TransportFault)
            }
        }
    }
}

/// Build + dispatch a `PassProgress` to the caller's reporter,
/// using the current pipeline-shared mapfile snapshot. Needs
/// `&self` for `disc.titles`. Returns `true` if the reporter
/// asked us to halt (i.e. the outer loop should set
/// `state.halted` and break).
pub(super) fn report_patch_progress(
    disc: &libfreemkv::Disc,
    state: &PatchLoopState,
    opts: &PatchOptions,
    total_bytes: u64,
    shared: &Mutex<SharedPatchState>,
) -> bool {
    let Some(reporter) = opts.progress else {
        return false;
    };
    let (s, bad_ranges_now) = {
        let g = shared
            .lock()
            .expect("PatchSink shared state mutex poisoned");
        (g.stats, g.bad_ranges.clone())
    };
    let kind = pass_kind(state.initial_batch, opts.reverse);
    let main_title_bad = disc
        .titles
        .first()
        .map(|t| bytes_bad_in_title(t, &bad_ranges_now))
        .unwrap_or(0);
    let main_title = disc.titles.first();
    // Progress = bytes RECOVERED so far (initial bad − still-bad), not a
    // per-range counter. With breadth-first tiers the readable bulk comes
    // back during tier 0 before any range is "finished", so a range-counter
    // sits at 0% while hundreds of MB are actually recovered. Deriving it
    // from the live still-bad count makes the bar (and the speed the client
    // computes from its delta) reflect real recovery the instant it happens.
    //
    // Compose the still-bad set to MATCH `work_total` (= the initial
    // NonTrimmed + NonScraped + Unreadable, no NonTried). `bytes_pending`
    // alone is the wrong denominator: it INCLUDES NonTried (so on a partially
    // swept disc it exceeds `work_total` and saturating_sub pins the bar at 0)
    // and EXCLUDES Unreadable (so the final-tier Unreadable→NonTrimmed relabel
    // would drive `recovered` backward). Subtract NonTried and add Unreadable
    // back so the two sets line up and progress stays monotonic.
    let still_bad_work = s
        .bytes_pending
        .saturating_sub(s.bytes_nontried)
        .saturating_add(s.bytes_unreadable);
    let recovered = state.work_total.saturating_sub(still_bad_work);
    let pp = libfreemkv::progress::PassProgress {
        kind,
        work_done: recovered,
        work_total: state.work_total,
        bytes_good_total: s.bytes_good,
        bytes_unreadable_total: s.bytes_unreadable,
        bytes_pending_total: s.bytes_pending,
        bytes_retryable_total: s.bytes_retryable,
        bytes_total_disc: total_bytes,
        disc_duration_secs: main_title.map(|t| t.duration_secs),
        bytes_bad_in_main_title: main_title_bad,
        main_title_duration_secs: main_title.map(|t| t.duration_secs),
        main_title_size_bytes: main_title.map(|t| t.size_bytes),
        // The rendered drilldown — located ranges + at-risk movie time —
        // computed here from the in-memory bad-range set + title so the
        // client renders it verbatim and never reads the mapfile.
        located: main_title
            .map(|t| libfreemkv::disc::locate_ranges(&bad_ranges_now, t))
            .unwrap_or_default(),
    };
    !reporter.report(&pp)
}

/// Bytes of bad/unreadable data in a title's extents, from a mapfile.
///
/// Consumers (CLI, autorip) call this after a rip pass to determine
/// how much damage affects a particular title — useful for showing
/// "42s lost (12s in main movie)" in the UI.
pub fn bytes_bad_in_title_from_mapfile(
    mapfile_path: &std::path::Path,
    title: &libfreemkv::DiscTitle,
) -> u64 {
    let map = match mapfile::Mapfile::load(mapfile_path) {
        Ok(m) => m,
        // A MISSING mapfile is legitimate (no damage was ever tracked — e.g. a
        // clean single-pass rip): 0 bad bytes is correct. Any OTHER load error
        // (corrupt / unreadable mapfile) means we CANNOT know the damage — and
        // a returned 0 reads to the caller as "clean." Logging alone is not
        // fail-safe: the RETURN VALUE drives the loss/abort accounting, not the
        // log. So fail safe by reporting the ENTIRE title as bad (its full
        // in-extent byte count) — a corrupt damage record must surface as
        // maximal loss, never as a clean rip.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::warn!(
                target: "freemkv::disc",
                path = %mapfile_path.display(),
                error = %e,
                "bytes_bad_in_title: mapfile load failed; reporting whole title bad (fail-safe: cannot confirm clean)"
            );
            return bytes_bad_in_title(title, &[(0, u64::MAX)]);
        }
    };
    // The CONVERGENCE set (includes NonTried), not the damage set: a
    // front-end asking "how much of the main title is still bad" must count
    // the unread remainder too, or an interrupted rip reports as clean.
    let bad_ranges = map.ranges_with(&mapfile::bad_sector_statuses());
    bytes_bad_in_title(title, &bad_ranges)
}

/// Pass 2..N of a multipass rip: re-read the bad ranges
/// recorded in the sidecar mapfile and try to recover them.
/// The walk is LARGEST-RANGE-FIRST (ties: lowest LBA first), not
/// positional: the big `NonTrimmed` regions are usually sweep
/// skip-ahead overshoot that reads straight back, so taking them
/// before the many tiny dead fragments recovers the bulk of the
/// disc in the first minutes. Returns a [`PatchOutcome`] with
/// recovered byte counts and wedge-detection signals.
///
/// Paired with [`Disc::sweep`] as the library's other flat
/// rip-phase verb. Caller drives the retry loop and the
/// sweep-vs-patch dispatch.
pub fn patch(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &PatchOptions,
) -> Result<PatchOutcome> {
    use libfreemkv::io::pipeline::{Pipeline, WRITE_THROUGH_DEPTH};
    use libfreemkv::sector::DecryptingSectorSource;

    // Pre-flight decrypt gate (also enforced in `copy`; re-checked here so a
    // direct `patch` caller can't bypass it). A decrypting patch pass of an
    // encrypted disc with no usable key would write ciphertext into the ISO's
    // recovered ranges; refuse before reading any sector. No-op for `--raw`
    // (`opts.decrypt == false`) and unencrypted discs.
    crate::resolve::ensure_decryptable_strict(disc, !opts.decrypt)?;

    let patch_t0 = std::time::Instant::now();
    let mapfile_path = disc.mapfile_for(path);
    let (map, initial_stats, initial_entries, total_bytes, bad_ranges, work_total, is_regular) =
        compute_initial_state(path, &mapfile_path)?;
    // Same reasoning as the decrypt gate above: `copy` and `sweep` both verify
    // the mapfile actually describes THIS disc before acting on it, and a
    // direct `patch` caller — the exposed sweep/patch resume pair — must not
    // be able to skip that. Without it, a resume that resolves to a leftover
    // mapfile from a different disc patches disc B's ranges into disc A's ISO
    // and records them Finished, which is silent corruption presented as a
    // successful recovery.
    mapfile::check_mapfile_identity(&map, disc).map_err(|e| Error::IoError { source: e })?;
    // Same argument as the identity gate above, and the gap it left open. A
    // patch pass walks only the mapfile's BAD ranges and takes `bytes_good`
    // from its stats, so it never looks at — and never re-reads — anything the
    // mapfile already calls Finished. If the image has since been truncated
    // (a full disk, an interrupted transfer, a remount), every Finished range
    // past the cut is a sparse hole that this pass will not touch, and the
    // outcome still reports the whole disc as good: a successful recovery over
    // data that was never written.
    //
    // `copy` and `sweep` both answer this by forcing a fresh sweep. `patch` has
    // no sweep to fall back to, so it refuses instead and leaves the choice to
    // the caller.
    //
    // REGULAR FILES ONLY. A length is only evidence of content for a regular
    // file: a character device (`/dev/null`, the discard destination used by
    // read-only verification passes) always stat's as 0 bytes no matter how
    // much has been written to it, so an unconditional gate refuses every such
    // pass. `sweep.rs::close` exempts non-regular outputs from its `sync_all`
    // check for the same reason; `is_regular` here is the same flag, read from
    // the open handle by `compute_initial_state`.
    let image = crate::recovery::image_state(path, total_bytes)?;
    if is_regular && image.is_short() {
        tracing::info!(
            target: "freemkv::scan",
            phase = "patch",
            have = image.len,
            want = image.want,
            "refusing: the image is shorter than the mapfile describes"
        );
        return Err(Error::ImageTruncated {
            have: image.len,
            want: image.want,
        });
    }
    tracing::info!(
        target: "freemkv::scan",
        phase = "patch",
        num_ranges = bad_ranges.len(),
        reverse = opts.reverse,
        "begin"
    );
    let bytes_good_before = initial_stats.bytes_good;
    let bytes_good_start = bytes_good_before;

    // Decrypt-aware read — symmetric with `Disc::sweep`. A decrypting patch
    // (`opts.decrypt`) decrypts in place (plaintext ISO); a NON-decrypting
    // patch (the multipass / `--raw --multipass` path) copies ciphertext
    // verbatim (keys = `None` → pass-through). Bad sectors are found by
    // PHYSICAL read success, not by decrypt structure: a re-read that returns
    // good bytes recovers the range; a read that errors leaves it NonTrimmed
    // for the next pass. (The old decrypt-VERIFY read gate was removed.)
    let mut keys = if opts.decrypt {
        disc.decrypt_keys()
    } else {
        libfreemkv::decrypt::DecryptKeys::None
    };
    let decrypt_is_aacs = matches!(keys, libfreemkv::decrypt::DecryptKeys::Aacs { .. });
    // AACS decrypting patch: resolve the whole-disc key map up front and decrypt
    // via the map (identical to `Disc::sweep`). CSS keeps the content-gated
    // self-descramble path. (Multipass patch is `--raw`, so decrypt is a no-op.)
    let key_map = if opts.decrypt && decrypt_is_aacs {
        let halt = opts.halt.clone().map(libfreemkv::halt::Halt::from_arc);
        Some(std::sync::Arc::new(disc.resolve_content_key_map(
            reader,
            &mut keys,
            opts.key_fetch.as_ref(),
            halt.as_ref(),
        )?))
    } else {
        None
    };
    let content_ranges = disc.encrypted_content_ranges();
    let can_gate = !content_ranges.is_empty();
    let mut reader = {
        let mut dec = DecryptingSectorSource::new(reader, keys);
        if let Some(map) = key_map {
            dec = dec.with_key_map(map);
        } else if opts.decrypt && can_gate {
            dec = dec.with_content_ranges(std::sync::Arc::from(content_ranges));
        }
        dec
    };
    let reader = &mut reader;

    // Spawn the consumer. The `WritebackFile` (same bounded-cache
    // wrapper sweep uses, so patch's recovery writes — sparse but
    // can be many across a damaged region — get the burst-flush
    // protection on slow / NFS-backed staging) and the `Mapfile`
    // both move into the sink. We hold an `Arc<Mutex<…>>` snapshot
    // the sink republishes after every record so producer-side
    // stall guards / progress callbacks can read consumer side-
    // effects.
    let (sink, shared) = PatchSink::new(path, map, is_regular)?;
    // Why: WRITE_THROUGH_DEPTH (=1) — patch reads ONE sector per
    // recovery decision and the producer's stall / damage-window
    // logic checks consumer-published stats inline. Sweep's
    // DEFAULT_PIPELINE_DEPTH (=4) would let several sectors of
    // recovered bytes queue up between producer decisions and
    // writes, which conflicts with the per-sector lockstep this
    // loop was written against.
    let pipe = Pipeline::<PatchItem, _>::spawn(WRITE_THROUGH_DEPTH, sink)?;

    // Log ISO file size at patch start for write monitoring
    if let Ok(metadata) = std::fs::metadata(path) {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.iso_size.start",
            iso_bytes = metadata.len(),
            "ISO file size at patch start"
        );
    }

    // Read sizing and fast-vs-deep recovery are owned by the handler chain
    // (`section_recover.rs`): it reads at a fixed `BATCH_SECTORS`, bisects to
    // isolate readable islands, and selects fast vs 60 s deep reads per
    // handler/tier. The old adaptive `current_batch` / halve-on-failure /
    // double-back loop that this comment used to describe no longer exists.
    // `block_sectors` and `full_recovery` therefore no longer drive behavior
    // — they survive only as the PassKind label and the diagnostics logged
    // below (informational-only; a caller can't change read sizing or the
    // recovery timeout through them). Clamp to ≥1 so the label math never
    // underflows on a `Some(0)`.
    let initial_batch = initial_batch_of(opts);
    let recovery = opts.full_recovery;
    log_patch_start_snapshot(&initial_entries, &initial_stats, bytes_good_before);

    tracing::info!(
        target: "freemkv::disc",
        phase = "patch.ranges",
        num_ranges = bad_ranges.len(),
        work_total,
        reverse_mode = opts.reverse,
        "Bad ranges for patch"
    );
    tracing::info!(
        target: "freemkv::disc",
        phase = "patch.start",
        block_sectors = initial_batch,
        recovery,
        reverse = opts.reverse,
        wedged_threshold = opts.wedged_threshold,
        num_ranges = bad_ranges.len(),
        work_total,
        bytes_good_start,
        "Disc::patch entered"
    );

    // Drive the recovery: build the per-pass context, then walk the
    // ordered bad ranges. `run` owns inter-range cooldown + the
    // pass-ending conditions; `patch_region` owns one range's loop.
    let mut ctx = PatchCtx {
        disc,
        reader,
        pipe: &pipe,
        shared: &shared,
        opts,
        total_bytes,
        decrypt_is_aacs,
        state: PatchLoopState::new(bytes_good_before, total_bytes, initial_batch, work_total),
        scoreboard: HandlerScoreboard::default(),
        wedge_streak: 0,
    };
    // Hold the pass result rather than `?`-ing it: `pipe.finish()` below is what
    // runs `PatchSink::close` (sync_all + mapfile.flush), and returning early
    // here skipped it on every error path — so a pass that died mid-write left
    // the on-disk damage record unflushed and disagreeing with what happened.
    let run_result = ctx.run(&bad_ranges);
    ctx.scoreboard.log();
    let PatchCtx { state, .. } = ctx;

    // Drain the consumer thread unconditionally: drop tx, wait for `close` to
    // run sync_all + mapfile.flush, then take the final stats from the sink's
    // summary. `close` failing on a regular-file sync_all is surfaced as
    // `Error::IoError`, matching pre-split behaviour.
    let finish_result = pipe.finish();

    // Producer-side error wins over consumer-side — the pass failure is what
    // motivated quitting; the flush error, if any, is downstream. Mirrors the
    // sweep path's documented precedence. But do not let a close() failure
    // vanish silently on the both-failed path: it is the only signal that the
    // mapfile on disk is now untrustworthy.
    if let Err(ref e) = run_result
        && let Err(close_err) = &finish_result
    {
        tracing::warn!(
            target: "freemkv::disc",
            phase = "patch.finish.dropped",
            pass_error = %e,
            close_error = %close_err,
            "patch: consumer close failed while the pass was already failing —              the mapfile on disk may be incomplete"
        );
    }
    run_result?;
    let summary = finish_result?;

    let outcome = build_outcome(
        &state,
        &summary,
        path,
        total_bytes,
        bad_ranges.len(),
        opts.wedged_threshold,
    );
    tracing::info!(
        target: "freemkv::scan",
        phase = "patch",
        recovered = outcome.bytes_recovered_this_pass,
        halted = outcome.halted,
        wedged_exit = outcome.wedged_exit,
        elapsed_ms = patch_t0.elapsed().as_millis() as u64,
        "end"
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal disc for the image-length gate. Only capacity matters here.
    fn guard_disc(sectors: u32) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "TESTDISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: sectors,
            capacity_bytes: sectors as u64 * 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    struct NoReader;
    impl libfreemkv::sector::SectorSource for NoReader {
        fn read_sectors(
            &mut self,
            _lba: u32,
            _count: u16,
            _buf: &mut [u8],
            _decrypt: bool,
        ) -> std::result::Result<usize, libfreemkv::Error> {
            panic!("the image-length gate must refuse BEFORE any sector is read");
        }
    }

    /// A patch pass walks only the mapfile's BAD ranges and takes `bytes_good`
    /// from its stats, so it never re-reads anything already marked Finished.
    /// If the image has been truncated since, every Finished range past the cut
    /// is a sparse hole — and the outcome still reported the whole disc as
    /// good: a successful recovery over data that was never written.
    ///
    /// `copy` and `sweep` both guarded this and `patch` did not. It must refuse,
    /// and it must refuse before reading a single sector (NoReader panics).
    #[test]
    fn patch_refuses_an_image_shorter_than_the_mapfile_describes() {
        let dir = std::env::temp_dir().join(format!("fmkv-patch-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("short.iso");

        let sectors = 64u32;
        let disc = guard_disc(sectors);
        let full = disc.capacity_bytes;

        // The mapfile describes the WHOLE disc, with one bad range to patch.
        let mapfile_path = disc.mapfile_for(&iso);
        let _ = std::fs::remove_file(&mapfile_path);
        let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
        mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
        mf.record(2048, 2048, mapfile::SectorStatus::NonTrimmed)
            .unwrap();
        mf.flush().unwrap();

        // …but the image on disk is half that long.
        std::fs::write(&iso, vec![0u8; (full / 2) as usize]).unwrap();

        let opts = PatchOptions::for_patch_pass(true, None, None, None);
        let err = match patch(&disc, &mut NoReader, &iso, &opts) {
            Err(e) => e,
            Ok(_) => panic!("a truncated image must not be patched and called good"),
        };
        match err {
            Error::ImageTruncated { have, want } => {
                assert_eq!(have, full / 2, "reports the length actually found");
                assert_eq!(want, full, "reports the length the mapfile describes");
            }
            other => panic!("expected ImageTruncated, got {other:?}"),
        }

        let _ = std::fs::remove_file(&mapfile_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gate must not fire on a healthy image — otherwise every normal
    /// resume would be refused.
    #[test]
    fn patch_accepts_an_image_of_the_length_the_mapfile_describes() {
        let dir = std::env::temp_dir().join(format!("fmkv-patch-intact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("full.iso");

        let sectors = 64u32;
        let disc = guard_disc(sectors);
        let full = disc.capacity_bytes;

        let mapfile_path = disc.mapfile_for(&iso);
        let _ = std::fs::remove_file(&mapfile_path);
        let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
        mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
        std::fs::write(&iso, vec![0u8; full as usize]).unwrap();

        // Nothing bad to patch, so this returns without reading a sector — the
        // point is only that it did NOT return ImageTruncated.
        let opts = PatchOptions::for_patch_pass(true, None, None, None);
        let r = patch(&disc, &mut NoReader, &iso, &opts);
        assert!(
            !matches!(r, Err(Error::ImageTruncated { .. })),
            "an image of exactly the right length must not be refused"
        );

        let _ = std::fs::remove_file(&mapfile_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A NON-REGULAR destination is exempt from the truncation gate.
    ///
    /// `/dev/null` is a real destination here (the discard sink used for
    /// read-only verification / benchmark passes), and a character device
    /// always stat's as 0 bytes no matter how much has been written to it. An
    /// unconditional length gate therefore reads "0 of N bytes" and refuses
    /// EVERY such pass with `ImageTruncated` — which is exactly the regression
    /// that shipped and that only an integration test caught. The length is
    /// only evidence for a regular file, so the gate must be `is_regular`-gated.
    #[test]
    #[cfg(unix)]
    fn patch_does_not_apply_the_truncation_gate_to_a_non_regular_destination() {
        let dev_null = std::path::Path::new("/dev/null");

        // Unique volume id → a unique temp mapfile name (`mapfile_for` derives
        // the /dev/null mapfile name from the disc), so this cannot collide
        // with a concurrently-running test's mapfile.
        let mut disc = guard_disc(64);
        disc.volume_id = format!("fmkv-devnull-gate-{}", std::process::id());
        let full = disc.capacity_bytes;

        let mapfile_path = disc.mapfile_for(dev_null);
        let _ = std::fs::remove_file(&mapfile_path);
        let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
        mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
        mf.flush().unwrap();

        // /dev/null reports len 0 while the mapfile describes `full` bytes:
        // the short-image condition is satisfied, and must NOT fire. Nothing is
        // bad, so the pass reads no sector (NoReader would panic).
        assert_eq!(
            std::fs::metadata(dev_null).unwrap().len(),
            0,
            "precondition: the character device measures as zero-length"
        );
        let opts = PatchOptions::for_patch_pass(false, None, None, None);
        let r = patch(&disc, &mut NoReader, dev_null, &opts);
        let _ = std::fs::remove_file(&mapfile_path);
        match r {
            Err(Error::ImageTruncated { have, want }) => panic!(
                "a /dev/null patch pass must not be refused as truncated \
                 (have={have}, want={want}) — a character device has no meaningful length"
            ),
            Err(other) => panic!("unexpected patch failure: {other:?}"),
            Ok(_) => {}
        }
    }

    /// The flat bandit pool must contain every handler from every tier, with a
    /// UNIQUE name per config — the scoreboard keys on the name, so any two
    /// handlers sharing a name would blur each other's decayed-yield ranking.
    #[test]
    fn flat_pool_is_all_tiers_with_unique_names() {
        let flat = build_flat_pool();
        let tiered: usize = (0..PATCH_TIERS).map(|t| build_tier_handlers(t).len()).sum();
        assert_eq!(
            flat.len(),
            tiered,
            "flat pool must equal the sum of all tier rosters"
        );
        let mut names: Vec<String> = flat.iter().map(|h| h.name()).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "every flat-pool handler must have a unique scoreboard name"
        );
    }

    // ----------------------------------------------------------------
    // Env-var scheduler knobs (`FREEMKV_PATCH_FLAT`,
    // `FREEMKV_PATCH_FLAT_BUDGET`).
    //
    // Process env is GLOBAL and cargo runs tests on many threads, so every
    // test that writes one of these keys must hold `ENV_LOCK` for as long as
    // it needs its value to stand, and must put the previous value back.
    // `EnvGuard` does both. `FREEMKV_PATCH_FLAT*` is read ONLY by this module
    // (`patch_flat_mode` / `flat_handler_budget_secs`) and the only tests in
    // the whole lib binary that reach that code are in this file, so this lock
    // is sufficient today — ANY future test that sets these keys, wherever it
    // lives, must take it too or it will race.
    // ----------------------------------------------------------------

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Holds `ENV_LOCK` and restores the captured keys on drop (including on
    /// panic, so one failing assertion cannot leak `FREEMKV_PATCH_FLAT` into
    /// the rest of the run).
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            // A poisoned lock only means some earlier test panicked while
            // holding it; the guard's Drop still restored the env, so the
            // mutex's data (`()`) is not actually corrupt.
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            Self { saved, _lock: lock }
        }

        fn set(&self, key: &str, val: Option<&str>) {
            // SAFETY: single-threaded with respect to these keys — `ENV_LOCK`
            // is held for the guard's lifetime and nothing else in the binary
            // touches them.
            unsafe {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                // SAFETY: as in `set` — the lock is still held here.
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    /// The flat-mode toggle, exercised through the real function: unset / empty
    /// / "0" → tier ladder; anything else → flat bandit.
    ///
    /// This used to assert against a locally-defined copy of the predicate, so
    /// the function under test was never called and any rewrite of it (e.g. to
    /// `!= "1"`, which flips `"true"` and every other value) left the test
    /// green. It now sets the real env var and calls `patch_flat_mode`.
    #[test]
    fn flat_mode_toggle_reads_the_env_var() {
        let g = EnvGuard::capture(&["FREEMKV_PATCH_FLAT"]);

        g.set("FREEMKV_PATCH_FLAT", None);
        assert!(!patch_flat_mode(), "unset must keep the proven tier ladder");
        g.set("FREEMKV_PATCH_FLAT", Some(""));
        assert!(!patch_flat_mode(), "empty is not an opt-in");
        g.set("FREEMKV_PATCH_FLAT", Some("0"));
        assert!(!patch_flat_mode(), "0 is the explicit off switch");

        g.set("FREEMKV_PATCH_FLAT", Some("1"));
        assert!(patch_flat_mode(), "1 selects the flat bandit");
        g.set("FREEMKV_PATCH_FLAT", Some("true"));
        assert!(
            patch_flat_mode(),
            "any non-empty, non-zero value opts in — not just \"1\""
        );
    }

    /// The flat per-handler EXPLORE budget, exercised through the real
    /// function. It had no test at all, and it is the whole reason the flat
    /// scheduler gives all 16 handlers a fast turn: a wrong default (or a
    /// missing floor) either starves the pool or spins forever on a handler
    /// with a zero-second deadline.
    #[test]
    fn flat_handler_budget_defaults_parses_and_floors() {
        let g = EnvGuard::capture(&["FREEMKV_PATCH_FLAT_BUDGET"]);

        g.set("FREEMKV_PATCH_FLAT_BUDGET", None);
        assert_eq!(flat_handler_budget_secs(), 12, "shipped default is 12 s");

        g.set("FREEMKV_PATCH_FLAT_BUDGET", Some("30"));
        assert_eq!(flat_handler_budget_secs(), 30, "an override is honoured");

        g.set("FREEMKV_PATCH_FLAT_BUDGET", Some("  7 "));
        assert_eq!(
            flat_handler_budget_secs(),
            7,
            "surrounding space is trimmed"
        );

        // A zero/negative budget would make every deadline already-expired, so
        // handlers would be entered and abandoned without a single read.
        g.set("FREEMKV_PATCH_FLAT_BUDGET", Some("0"));
        assert_eq!(flat_handler_budget_secs(), 1, "floored at 1 s");

        // Garbage must fall back to the default, not to 0 and not to a panic.
        for junk in ["", "abc", "-5", "12s"] {
            g.set("FREEMKV_PATCH_FLAT_BUDGET", Some(junk));
            assert_eq!(
                flat_handler_budget_secs(),
                12,
                "unparseable {junk:?} must fall back to the default"
            );
        }
    }

    /// A reader that fails every read and remembers the LBA order it was asked
    /// for. Every handler therefore yields on its own dead-read limit, and the
    /// recorded sequence is a direct trace of the SCHEDULER's walk.
    struct TraceReader {
        lbas: Vec<u32>,
    }
    impl libfreemkv::sector::SectorSource for TraceReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            _count: u16,
            _buf: &mut [u8],
            _decrypt: bool,
        ) -> std::result::Result<usize, libfreemkv::Error> {
            self.lbas.push(lba);
            // An ORDINARY bad sector (CHECK CONDITION / medium error): must not
            // be mistaken for a transport fault, which would end the pass early
            // and destroy the trace.
            Err(Error::DiscRead {
                sector: lba as u64,
                status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
                sense: Some(libfreemkv::scsi::ScsiSense {
                    sense_key: 0x03,
                    asc: 0x11,
                    ascq: 0x00,
                }),
            })
        }
    }

    /// Run one real patch pass over two bad ranges and return the LBA trace.
    /// The caller owns `FREEMKV_PATCH_FLAT` (via `EnvGuard`) around this.
    fn trace_two_range_pass(tag: &str, a: (u32, u32), b: (u32, u32)) -> Vec<u32> {
        let dir = std::env::temp_dir().join(format!("fmkv-flat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("trace.iso");

        let disc = guard_disc(2000);
        let full = disc.capacity_bytes;
        let mapfile_path = disc.mapfile_for(&iso);
        let _ = std::fs::remove_file(&mapfile_path);
        let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
        mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
        for &(lba, count) in &[a, b] {
            mf.record(
                lba as u64 * 2048,
                count as u64 * 2048,
                mapfile::SectorStatus::NonTrimmed,
            )
            .unwrap();
        }
        mf.flush().unwrap();
        std::fs::write(&iso, vec![0u8; full as usize]).unwrap();

        let mut reader = TraceReader { lbas: Vec::new() };
        let opts = PatchOptions::for_patch_pass(false, None, None, None);
        patch(&disc, &mut reader, &iso, &opts).expect("the pass itself must complete");

        let _ = std::fs::remove_dir_all(&dir);
        reader.lbas
    }

    /// END-TO-END through the FLAT scheduler: with `FREEMKV_PATCH_FLAT` set,
    /// `PatchCtx::run` does ONE walk over the ranges and gives each range the
    /// whole handler pool before moving on. The tier ladder does the opposite:
    /// it sweeps EVERY range at tier 0, then every range again at tier 1, so a
    /// range is revisited after later ranges have been touched.
    ///
    /// That difference is the entire behavioural content of the env flag, and
    /// nothing exercised it — `build_flat_pool` was only checked structurally,
    /// so the flat branch of `run` (and the per-handler budget it selects)
    /// could be deleted or inverted with the suite still green. The read trace
    /// makes the walk order observable: flat = range A finished before range B
    /// is ever touched; tiered = A is revisited after B.
    #[test]
    fn the_flat_scheduler_finishes_a_range_before_starting_the_next() {
        // Range A is the LARGER one, so the (size desc, pos asc) sort puts it
        // first in BOTH schedulers — the only variable left is the walk shape.
        let a = (100u32, 16u32);
        let b = (1000u32, 4u32);
        let in_a = |l: &u32| *l >= a.0 && *l < a.0 + a.1;
        let in_b = |l: &u32| *l >= b.0 && *l < b.0 + b.1;

        let g = EnvGuard::capture(&["FREEMKV_PATCH_FLAT", "FREEMKV_PATCH_FLAT_BUDGET"]);
        // A 1 s per-handler budget keeps the pass quick; the handlers all yield
        // on dead reads long before it, so it does not change the walk.
        g.set("FREEMKV_PATCH_FLAT_BUDGET", Some("1"));

        g.set("FREEMKV_PATCH_FLAT", Some("1"));
        let flat = trace_two_range_pass("flat", a, b);
        g.set("FREEMKV_PATCH_FLAT", None);
        let tiered = trace_two_range_pass("tier", a, b);
        drop(g);

        // Both schedulers must actually visit both ranges, or the ordering
        // assertions below would hold vacuously.
        for (name, trace) in [("flat", &flat), ("tiered", &tiered)] {
            assert!(
                trace.iter().any(in_a) && trace.iter().any(in_b),
                "{name}: both bad ranges must be attempted (trace: {trace:?})"
            );
        }

        let last_a = flat.iter().rposition(in_a).unwrap();
        let first_b = flat.iter().position(in_b).unwrap();
        assert!(
            last_a < first_b,
            "FLAT: the whole handler pool must finish range A before range B is \
             touched — range A was read again at trace index {last_a}, after \
             range B started at {first_b} (trace: {flat:?})"
        );

        // The contrast that proves the trace really observes the scheduler:
        // the default ladder DOES come back to range A after range B.
        let last_a_t = tiered.iter().rposition(in_a).unwrap();
        let first_b_t = tiered.iter().position(in_b).unwrap();
        assert!(
            last_a_t > first_b_t,
            "TIERED: the breadth-first ladder must revisit range A on a later \
             tier, after range B's tier-0 pass (trace: {tiered:?})"
        );
    }

    /// Transport failure (status=0xFF, USB-bridge crash) must be recognised and
    /// abort the pass, rather than being treated as an ordinary bad sector and
    /// hammering the crashed device for up to the per-range watchdog budget. The
    /// transport-failure classification predicate is not unit-testable in
    /// isolation, so this guards the predicate the production early-return keys
    /// off, and the contrast that an ordinary read error is NOT misclassified as
    /// a transport failure.
    #[test]
    fn transport_failure_is_recognised_for_patch_abort() {
        use libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE;

        // The exact shape Drive::read surfaces on a bridge crash.
        let tf = Error::DiscRead {
            sector: 1_392_314,
            status: Some(SCSI_STATUS_TRANSPORT_FAILURE),
            sense: None,
        };
        assert!(
            tf.is_scsi_transport_failure(),
            "a DiscRead with status=0xFF must classify as a transport failure so \
             patch aborts the pass"
        );

        // The raw ScsiError form (e.g. straight from the transport) too.
        let tf_raw = Error::ScsiError {
            opcode: 0x28,
            status: SCSI_STATUS_TRANSPORT_FAILURE,
            sense: None,
        };
        assert!(tf_raw.is_scsi_transport_failure());

        // An ordinary recoverable bad sector (CHECK CONDITION with sense) must
        // NOT trip the transport-failure abort — it should still be retried /
        // marked NonTrimmed, not abort the whole pass.
        let bad_sector = Error::DiscRead {
            sector: 1_392_314,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(libfreemkv::scsi::ScsiSense {
                sense_key: 0x03,
                asc: 0x11,
                ascq: 0x00,
            }),
        };
        assert!(
            !bad_sector.is_scsi_transport_failure(),
            "an ordinary bad-sector CHECK CONDITION must not be misclassified as \
             a transport failure"
        );
    }

    #[test]
    fn recovery_read_widens_unaligned_aacs_window() {
        // A mid-unit AACS read must widen to the enclosing 3-sector unit
        // (so the decrypting source accepts it) and copy back exactly the
        // requested sector. Each sector is filled with its own LBA's low
        // byte so we can prove which window came back.
        struct RecordReader {
            saw_lba: u32,
            saw_count: u16,
        }
        impl SectorSource for RecordReader {
            fn read_sectors(
                &mut self,
                lba: u32,
                count: u16,
                buf: &mut [u8],
                _recovery: bool,
            ) -> Result<usize> {
                self.saw_lba = lba;
                self.saw_count = count;
                for s in 0..count as usize {
                    buf[s * 2048..(s + 1) * 2048].fill((lba as usize + s) as u8);
                }
                Ok(count as usize * 2048)
            }
        }
        let mut rr = RecordReader {
            saw_lba: 0,
            saw_count: 0,
        };
        let mut buf = vec![0u8; 2048];
        // Request lba=4 (4 % 3 == 1, mid-unit), count=1.
        let n = recovery_read(&mut rr, true, 4, 1, &mut buf, true, false).unwrap();
        assert_eq!(n, 2048);
        assert_eq!(rr.saw_lba, 3, "widened down to the unit-aligned start");
        assert_eq!(rr.saw_count, 3, "widened to a whole 3-sector unit");
        assert_eq!(
            buf[0], 4u8,
            "copied back the requested sector (lba 4), not the unit head (lba 3)"
        );
    }

    // ----------------------------------------------------------------
    // SubRanges — the still-bad work-list the per-section recovery
    // phases (#50) shrink. Pure data structure; exhaustively tested so
    // each future phase helper can assert on its residual ranges.
    // ----------------------------------------------------------------

    #[test]
    fn subranges_from_section_and_basics() {
        let s = SubRanges::from_section(2048, 10 * 2048);
        assert!(!s.is_empty());
        assert_eq!(s.total_len(), 10 * 2048);
        assert_eq!(s.ranges(), &[(2048, 10 * 2048)]);
        assert!(SubRanges::from_section(2048, 0).is_empty());
        assert!(SubRanges::default().is_empty());
    }

    #[test]
    fn subranges_remove_middle_splits() {
        // [0,20k) minus [8k,12k) -> [0,8k) + [12k,20k)
        let mut s = SubRanges::from_section(0, 20 * 1024);
        s.remove(8 * 1024, 4 * 1024);
        assert_eq!(s.ranges(), &[(0, 8 * 1024), (12 * 1024, 8 * 1024)]);
        assert_eq!(s.total_len(), 16 * 1024);
    }

    #[test]
    fn subranges_remove_prefix_suffix_and_whole() {
        // prefix
        let mut s = SubRanges::from_section(1000, 1000);
        s.remove(900, 200); // [1000,1100) trimmed off the front
        assert_eq!(s.ranges(), &[(1100, 900)]);
        // suffix
        let mut s = SubRanges::from_section(1000, 1000);
        s.remove(1800, 500); // [1800,2000) trimmed off the back
        assert_eq!(s.ranges(), &[(1000, 800)]);
        // whole (exact + over-cover both clear it)
        let mut s = SubRanges::from_section(1000, 1000);
        s.remove(1000, 1000);
        assert!(s.is_empty());
        let mut s = SubRanges::from_section(1000, 1000);
        s.remove(0, 100_000);
        assert!(s.is_empty());
    }

    #[test]
    fn subranges_remove_gap_and_zero_are_noops() {
        let mut s = SubRanges::from_section(1000, 1000);
        s.remove(5000, 1000); // disjoint, after
        s.remove(0, 500); // disjoint, before
        s.remove(1200, 0); // zero-len
        assert_eq!(s.ranges(), &[(1000, 1000)]);
    }

    #[test]
    fn subranges_remove_spanning_two_ranges() {
        // two sub-ranges, removal straddling the gap trims the inner edges
        let mut s = SubRanges::from_section(0, 4096);
        s.remove(1024, 1024); // -> [0,1024) + [2048,4096)
        assert_eq!(s.ranges(), &[(0, 1024), (2048, 2048)]);
        s.remove(512, 2048); // covers tail of first + head of second
        assert_eq!(s.ranges(), &[(0, 512), (2560, 1536)]);
    }
}

/// `bytes_bad_in_title_from_mapfile` is what the CLI's damage report is made
/// of (`pipe.rs` scales it by the main title's own size and runtime to print
/// "N s lost"), and nothing constrained it: the mutation run replaced the whole
/// body with `0` and the suite stayed green — a damaged rip reported as clean.
///
/// Its three documented answers are genuinely different, so each gets a test:
/// a missing mapfile is clean, a corrupt one is maximally bad (fail-safe), and
/// a real one counts the CONVERGENCE set inside the title's extents.
#[cfg(test)]
mod bytes_bad_from_mapfile_tests {
    use super::*;
    use crate::recovery::mapfile::{Mapfile, SectorStatus};

    const SECTOR: u64 = 2048;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fmkv-bbt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A title occupying sectors [start_lba, start_lba + count).
    fn title_at(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.extents = vec![libfreemkv::disc::Extent {
            start_lba,
            sector_count,
        }];
        t
    }

    /// No mapfile means no damage was ever tracked — a clean single-pass rip.
    /// Zero is the right answer here, and only here.
    #[test]
    fn missing_mapfile_is_clean() {
        let d = tmpdir("missing");
        let bad = bytes_bad_in_title_from_mapfile(&d.join("nope.mapfile"), &title_at(0, 100));
        assert_eq!(bad, 0);
    }

    /// A mapfile that EXISTS but cannot be parsed means the damage record is
    /// gone. Returning 0 there would read to the caller as "clean", so the
    /// fail-safe reports the whole title bad. This is the arm that matters: it
    /// is the difference between "we know it is fine" and "we cannot tell".
    #[test]
    fn corrupt_mapfile_reports_the_whole_title_bad() {
        let d = tmpdir("corrupt");
        let p = d.join("broken.mapfile");
        std::fs::write(&p, b"this is not a rescue logfile\n\x00\xff garbage").unwrap();
        let title = title_at(10, 50);
        let bad = bytes_bad_in_title_from_mapfile(&p, &title);
        assert_eq!(
            bad,
            50 * SECTOR,
            "an unreadable damage record must surface as maximal loss, not as a clean rip"
        );
    }

    /// The normal path: only the bad bytes that actually fall inside the
    /// title's extents count. Damage elsewhere on the disc is not this title's.
    #[test]
    fn counts_only_damage_inside_the_title_extents() {
        let d = tmpdir("scoped");
        let p = d.join("scoped.mapfile");
        let total = 1000 * SECTOR;
        let mut mf = Mapfile::create(&p, total, "test").unwrap();
        // Everything good, then damage inside the title and damage outside it.
        mf.record(0, total, SectorStatus::Finished).unwrap();
        mf.record(100 * SECTOR, 10 * SECTOR, SectorStatus::Unreadable)
            .unwrap();
        mf.record(900 * SECTOR, 20 * SECTOR, SectorStatus::Unreadable)
            .unwrap();
        mf.flush().unwrap();

        // Title spans sectors 50..150, so it contains the first damage only.
        let bad = bytes_bad_in_title_from_mapfile(&p, &title_at(50, 100));
        assert_eq!(
            bad,
            10 * SECTOR,
            "damage outside the extents must not count"
        );
    }

    /// The CONVERGENCE set, not the damage set. An interrupted rip leaves
    /// `NonTried` sectors: they are unread, not proven good, and a front-end
    /// that ignores them reports a half-finished rip as clean.
    #[test]
    fn unread_sectors_count_as_bad() {
        let d = tmpdir("nontried");
        let p = d.join("nontried.mapfile");
        let total = 200 * SECTOR;
        let mut mf = Mapfile::create(&p, total, "test").unwrap();
        // First half read clean; second half never attempted.
        mf.record(0, 100 * SECTOR, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();

        let bad = bytes_bad_in_title_from_mapfile(&p, &title_at(0, 200));
        assert_eq!(
            bad,
            100 * SECTOR,
            "NonTried is not good — an interrupted rip must not report clean"
        );
    }

    /// A fully-good mapfile really is zero, so the fail-safes above are not
    /// just "always returns nonzero".
    #[test]
    fn a_fully_read_title_is_zero() {
        let d = tmpdir("clean");
        let p = d.join("clean.mapfile");
        let total = 200 * SECTOR;
        let mut mf = Mapfile::create(&p, total, "test").unwrap();
        mf.record(0, total, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();

        assert_eq!(
            bytes_bad_in_title_from_mapfile(&p, &title_at(0, 200)),
            0,
            "a fully-Finished mapfile must report no loss"
        );
    }
}

#[cfg(test)]
mod snap_tests {
    use super::snap_to_sectors;

    /// An already-aligned range is untouched.
    #[test]
    fn aligned_ranges_pass_through() {
        assert_eq!(snap_to_sectors(0, 2048), (0, 2048));
        assert_eq!(snap_to_sectors(4096, 8192), (4096, 8192));
    }

    /// A 512-block ddrescue range widens outward to whole sectors. Both edges
    /// move, and the result always covers the original span.
    #[test]
    fn unaligned_ranges_widen_outward_and_cover_the_original() {
        for &(pos, len) in &[
            (512u64, 1024u64),
            (2048 + 512, 512),
            (100 * 2048 + 1, 3000),
            (1, 1),
        ] {
            let (p, l) = snap_to_sectors(pos, len);
            assert_eq!(p % 2048, 0, "start {p} not sector-aligned");
            assert_eq!(l % 2048, 0, "len {l} not a sector multiple");
            assert!(p <= pos, "widening must not lose the head");
            assert!(p + l >= pos + len, "widening must not lose the tail");
            assert!(l >= 2048, "a non-empty span must be at least one sector");
        }
    }

    /// A sub-sector span must never yield a zero-sector read: `count =
    /// (span / SECTOR) as u16` would truncate to 0, and a zero-length read
    /// reports Good, crediting a recovery that never happened.
    #[test]
    fn sub_sector_spans_never_truncate_to_zero_sectors() {
        let (_, l) = snap_to_sectors(1000, 48);
        assert!(l / 2048 >= 1, "span {l} truncates to a zero-sector read");
    }
}
