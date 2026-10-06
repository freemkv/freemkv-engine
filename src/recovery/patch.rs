//! Producer / consumer split for `Disc::patch`.
//!
//! A consumer thread owns the [`libfreemkv::io::WritebackFile`] and the
//! [`super::mapfile::Mapfile`]. The producer thread (`Disc::patch`) keeps
//! the [`libfreemkv::sector::SectorSource`], the wedge streak and decrypt, so the channel carries clean cleartext bytes. It runs over
//! a depth-1 channel ([`libfreemkv::io::pipeline::WRITE_THROUGH_DEPTH`]) so
//! back-pressure kicks in immediately.

use std::io::{Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use libfreemkv::error::{Error, Result};
use libfreemkv::io::pipeline::{Flow, Sink};

use super::mapfile::{self, MapStats, Mapfile, SectorStatus};
use super::section_recover::{
    Bisect, CachePrime, Direction, HandlerCtx, HandlerOutcome, HandlerScoreboard, Jump, Linear,
    Oscillate, ReadParams, RecoverySink, SECTOR, SPEED_MAX_KBS, SectionHandler, SpeedPref,
    SpeedSweep, TimeoutPref, run_handlers,
};

// Wall-clock budget one handler gets before the chain tries the next idea
// (#55): what's left over becomes NonTrimmed residue instead of hanging.
// Replaces the old 1800 s/range + 3600 s/pass grind budgets.
const PER_HANDLER_BUDGET_SECS: u64 = 60;

/// Minimum interval between progress heartbeats pushed from inside a handler, so
/// the UI's bar/speed move continuously during a long section without flooding
/// the reporter (see the tick closure in `recover_section`).
const PROGRESS_TICK_MS: u64 = 250;

// Bridges [`RecoverySink`] onto the patch consumer pipe: each recovered span
// becomes a [`PatchItem::Recovered`]. A dead / stalled consumer is returned as
// `Err`, which ends the handler chain Fatal instead of reading on.
struct PatchRecoverySink<'a> {
    pipe: &'a Pipeline<PatchItem, PatchSummary>,
    /// Cancellation token for the send below — the caller's external Stop bit
    /// when there is one (see `recover_section`). Without it this send had no way
    /// to observe a Stop at all: `WRITE_THROUGH_DEPTH` is 1, so a consumer
    /// stalled inside its write parks the producer in `send` indefinitely.
    halt: &'a libfreemkv::halt::Halt,
}

impl RecoverySink for PatchRecoverySink<'_> {
    fn recovered(&mut self, pos: u64, buf: &[u8]) -> Result<()> {
        match super::send_bounded(
            self.pipe,
            PatchItem::Recovered {
                pos,
                buf: buf.to_vec(),
            },
            self.halt,
        ) {
            Ok(()) => Ok(()),
            // Ok on halt: not written, the mapfile keeps it bad (only the in-memory
            // set drops it); the chain ends Halted. An Err would fail a Stop.
            Err(super::SendStall::Halted) => Ok(()),
            Err(stall) => Err(stall.into_error()),
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

    /// Producer marks `[pos, pos+len)` as `NonTrimmed`: a range's residue after its final
    /// tier (tried, still failing). It stays "hopeful" — a later pass retries it; promotion
    /// to true `Unreadable` is the orchestrator's job, applied once after all retry passes.
    NonTrimmed { pos: u64, len: u64 },
}

// Mapfile snapshot the sink republishes after every record so the producer can drive stall /
// progress logic without holding the mapfile lock. Derived figures over the DAMAGE set.
pub(super) struct SharedPatchState {
    pub stats: MapStats,
    /// Damage bytes intersecting the main title's extents, over the COMPLETE
    /// damage set. `PassProgress::bytes_bad_in_main_title` verbatim.
    pub bad_bytes_in_title: u64,
    /// The rendered drilldown — located ranges, section count, "+N more" tail,
    /// at-risk movie time — computed over the COMPLETE damage set.
    pub located: libfreemkv::progress::LocatedProgress,
    /// Damage bytes inside the map's scope (all of it for an unscoped map): what is still
    /// bad of the pass's `work_total`.
    pub damage_in_scope: u64,
}

impl SharedPatchState {
    /// `title` is the main feature (`disc.titles.first()`); `None` on a disc
    /// with no titles, where there is nothing to locate damage against.
    fn from_map(map: &Mapfile, title: Option<&libfreemkv::DiscTitle>) -> Self {
        let bad_ranges = map.ranges_with(&mapfile::damage_sector_statuses());
        let (bad_bytes_in_title, located) = match title {
            Some(t) => (
                bytes_bad_in_title(t, &bad_ranges),
                libfreemkv::disc::locate_ranges(&bad_ranges, t),
            ),
            None => (0, libfreemkv::progress::LocatedProgress::default()),
        };
        let damage_in_scope = match map.scope() {
            Some(scope) => mapfile::intersect(&bad_ranges, scope),
            None => bad_ranges,
        }
        .iter()
        .map(|r| r.1)
        .sum();
        Self {
            stats: map.stats(),
            bad_bytes_in_title,
            located,
            damage_in_scope,
        }
    }
}

// A pass's result from its producer run and consumer teardown: the producer's error wins,
// unless the consumer's `apply` failed first (its write error is the cause).
fn settle(
    run_result: Result<()>,
    finish_result: Result<PatchSummary>,
    consumer_failed: bool,
) -> Result<PatchSummary> {
    let Err(e) = run_result else {
        return finish_result;
    };
    // Don't let a close() failure vanish on the both-failed path: it's the only signal
    // that the mapfile on disk is now untrustworthy.
    if let Err(close_err) = &finish_result {
        tracing::warn!(
            target: "freemkv::disc",
            phase = "patch.finish.dropped",
            pass_error = %e,
            close_error = %close_err,
            "patch: consumer close failed while the pass was already failing; the mapfile on disk may be incomplete"
        );
    }
    Err(super::pass_failure(e, finish_result, consumer_failed))
}

// Final summary from [`Sink::close`] on a clean drain: the final mapfile
// stats. A `sync_all` failure on a regular file short-circuits `close` with
// an `Err` before this is built, so it never carries a fsync-error field.
pub(super) struct PatchSummary {
    pub stats: MapStats,
}

// Consumer-side of the patch pipeline. Owns the ISO writeback file and the
// mapfile; publishes a shared snapshot after every record so the producer
// can read `bytes_good` for stall detection and progress reporting.
pub(super) struct PatchSink {
    file: libfreemkv::io::WritebackFile,
    map: Mapfile,
    /// Whether the output is a regular file (so a `sync_all` failure
    /// is real). `/dev/null` etc. always fail `sync_all`; ignore those.
    is_regular: bool,
    /// Snapshot the producer reads. Updated after every successful
    /// `record()` call. `Mutex` rather than separate atomics because
    /// the producer wants stats + drilldown as a coherent pair.
    shared: Arc<Mutex<SharedPatchState>>,
    /// Main feature, so the snapshot's drilldown can be derived here from the
    /// complete damage set rather than from a capped copy on the reader's side.
    /// Cloned once at construction; `patch` never mutates `disc.titles`.
    title: Option<libfreemkv::DiscTitle>,
    /// Last time the shared snapshot was republished. `from_map` walks the
    /// whole damage set every call, so the per-record path throttles to a time
    /// cadence (`REPUBLISH_CADENCE`); the final close always forces a publish.
    last_republish: Option<std::time::Instant>,
}

/// Minimum interval between per-record snapshot republishes.
const REPUBLISH_CADENCE: std::time::Duration = std::time::Duration::from_millis(250);

impl PatchSink {
    // Opens `path` as a [`libfreemkv::io::WritebackFile`] and pairs it with
    // `map` for the consumer. The producer holds onto the returned
    // `Arc<Mutex<SharedPatchState>>` to poll mapfile state concurrently.
    pub(super) fn new(
        path: &std::path::Path,
        map: Mapfile,
        is_regular: bool,
        title: Option<libfreemkv::DiscTitle>,
    ) -> Result<(Self, Arc<Mutex<SharedPatchState>>)> {
        let file = libfreemkv::io::WritebackFile::open(path).map_err(Error::from)?;
        let shared = Arc::new(Mutex::new(SharedPatchState::from_map(&map, title.as_ref())));
        let shared_clone = shared.clone();
        Ok((
            Self {
                file,
                map,
                is_regular,
                shared,
                title,
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
        // Build outside the lock (it walks the whole damage set), then swap.
        let next = SharedPatchState::from_map(&self.map, self.title.as_ref());
        *lock_snapshot(&self.shared) = next;
    }
}

// The snapshot is only ever replaced whole, so a poisoned lock still holds a
// complete (if stale) value: take it rather than cascade the panic.
fn lock_snapshot(m: &Mutex<SharedPatchState>) -> std::sync::MutexGuard<'_, SharedPatchState> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// No `close_stopped` override (the default `close`): this sink renames nothing, and a
// data-less mapfile flush would claim sectors not yet durable. T8: "halted: **Stopped**
// (not a failure)" after one 5 s grace; the disowned mapfile keeps the resumable record.
impl Sink<PatchItem> for PatchSink {
    type Output = PatchSummary;

    fn apply(&mut self, item: PatchItem) -> std::result::Result<Flow, Error> {
        match item {
            PatchItem::Recovered { pos, buf } => {
                let len = buf.len() as u64;
                let lost = |e| super::image_write_failed(&self.map, e);
                self.file.seek(SeekFrom::Start(pos)).map_err(lost)?;
                self.file.write_all(&buf).map_err(lost)?;
                if self.map.persist_due() && self.is_regular {
                    self.file.sync_all().map_err(lost)?;
                }
                self.map
                    .record(pos, len, SectorStatus::Finished)
                    .map_err(Error::from)?;
            }
            PatchItem::NonTrimmed { pos, len } => {
                self.map
                    .record(pos, len, SectorStatus::NonTrimmed)
                    .map_err(Error::from)?;
            }
        }
        self.republish(false);
        Ok(Flow::Continue)
    }

    fn close(mut self) -> std::result::Result<Self::Output, Error> {
        // Drain in-flight writeback then fsync; failure matters only on regular
        // files since pipes / `/dev/null` etc. always fail `sync_all`.
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
                // The data is not durable: the dropped map must not flush it as Finished.
                self.map.disown_handle().disown();
                return Err(Error::from(e));
            }
            tracing::debug!(
                target: "freemkv::disc",
                phase = "patch.sync.skipped",
                error = %e,
                "patch: sync_all failed for non-regular file; ignoring"
            );
        }
        self.map.flush().map_err(Error::from)?;
        // Final republish so anyone reading the shared snapshot after
        // `Pipeline::finish` sees the post-flush state; the snapshot's
        // contract is that it stays current through close.
        self.republish(true);
        Ok(PatchSummary {
            stats: self.map.stats(),
        })
    }
}

use super::{PatchOptions, PatchOutcome};
use crate::engine_halt::EngineHalt;
use libfreemkv::disc::bytes_bad_in_title;
use libfreemkv::io::pipeline::Pipeline;
use libfreemkv::sector::SectorSource;

// Breadth-first recovery tiers: 0 fast-sweeps every bad range, 1 deep-recovers
// the residual, 2 runs marginal specialists on the true hardened residual.
// See `PatchCtx::run` and `build_tier_handlers`.
const PATCH_TIERS: usize = 3;

// Phase A pre-snapshot: the fields the patch loop needs after the live
// `Mapfile` moves into the consumer thread (`map` is the object loaded).
pub(super) struct InitialState {
    pub map: Mapfile,
    pub stats: MapStats,
    pub total_bytes: u64,
    pub bad_ranges: Vec<(u64, u64)>,
    pub work_total: u64,
    pub is_regular: bool,
}

pub(super) fn compute_initial_state(
    path: &std::path::Path,
    mapfile_path: &std::path::Path,
) -> Result<InitialState> {
    let map = mapfile::Mapfile::load(mapfile_path).map_err(Error::from)?;
    let total_bytes = map.total_size();
    let initial_stats = map.stats();
    // Retry passes act on NonTrimmed/NonScraped/Unreadable (a failed sector gets
    // a fresh shot next pass); NonTried is excluded since a preceding sweep pass
    // covers it. Not reversed for `opts.reverse`: `PatchCtx::run` sorts this list.
    let mut bad_ranges = map.ranges_with(&mapfile::damage_sector_statuses());
    if let Some(scope) = map.scope() {
        bad_ranges = mapfile::intersect(&bad_ranges, scope);
    }
    let work_total: u64 = bad_ranges.iter().map(|(_, sz)| *sz).sum();
    // Fail safe when metadata is indeterminate: assume a regular file so a real
    // `sync_all` failure surfaces rather than gets swallowed. `/dev/null` and
    // pipes still map to `false`; only a genuine metadata error hits the default.
    let is_regular = super::output_is_regular(std::fs::metadata(path));
    Ok(InitialState {
        map,
        stats: initial_stats,
        total_bytes,
        bad_ranges,
        work_total,
        is_regular,
    })
}

// One recovery read of `[lba, lba+count)` into `buf[..count*2048]`. `recovery` selects the SCSI
// timeout (60 s deep vs fast); `fua` forces the drive to bypass readahead and re-fetch.
pub(super) fn recovery_read<R: SectorSource + ?Sized>(
    reader: &mut R,
    lba: u32,
    count: u16,
    buf: &mut [u8],
    recovery: bool,
    fua: bool,
) -> Result<usize> {
    // AACS unit widening happens inside the whole-disc reader (per-file grid). The
    // caller's buffer is reused, so a short transfer would leave the PREVIOUS span's
    // stale bytes behind: `require_full_read` turns it into a failed read.
    let bytes = count as usize * 2048;
    super::require_full_read(
        reader.read_sectors_fua(lba, count, &mut buf[..bytes], recovery, fua),
        bytes,
        lba,
    )
}

// The still-bad `[pos, len)` sub-ranges of one bad section (byte offsets,
// multiples of 2048), sorted and non-overlapping. Each recovery phase calls
// [`SubRanges::remove`] to shrink the set; whatever remains is NonTrimmed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct SubRanges {
    /// (pos, len) pairs, sorted by pos, non-overlapping, all non-zero len.
    ranges: Vec<(u64, u64)>,
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
    pub(super) fn contains(&self, pos: u64) -> bool {
        self.ranges.iter().any(|&(p, n)| pos >= p && pos - p < n)
    }

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

// Pre-loop diagnostic dump: emits `patch_mapfile_snapshot` plus the
// first/last 10 entries (info + per-entry debug). Pure logging, no state
// mutation. Keeps the operator's existing `[disc] patch_mapfile_*` grep patterns.
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
                status_char = %entry.status.to_char(),
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
                status_char = %entry.status.to_char(),
                "Mapfile entry"
            );
        }
    }
}

// Bundles final mapfile stats + accumulated loop counters into the public
// `PatchOutcome` the caller consumes, also emitting the post-loop tracing
// (`patch_iso_size_end`, `patch_done`).
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

// Per-pass loop state inside `Disc::patch`, on the producer thread.
pub(super) struct PatchLoopState {
    // How the pass ended.
    pub halted: bool,
    pub wedged_exit: bool,
    // Clock seam: the handler chain reads wall time through this rather than
    // calling `Instant::now()` inline, so the per-handler deadline is driven by
    // an injectable clock and deterministic tests can wind it forward.
    pub now: fn() -> std::time::Instant,
    // Snapshot at construction — these stay constant for the whole pass.
    pub bytes_good_before: u64,
    pub initial_batch: u16,
    pub work_total: u64,
}

impl PatchLoopState {
    pub(super) fn new(bytes_good_before: u64, initial_batch: u16, work_total: u64) -> Self {
        // Production clock: the real monotonic wall clock.
        Self::new_with_clock(
            bytes_good_before,
            initial_batch,
            work_total,
            std::time::Instant::now,
        )
    }

    /// Like `new`, but with an injectable monotonic clock so a test can wind a
    /// fake clock forward. `new` passes `Instant::now`, production unchanged.
    pub(super) fn new_with_clock(
        bytes_good_before: u64,
        initial_batch: u16,
        work_total: u64,
        now: fn() -> std::time::Instant,
    ) -> Self {
        Self {
            halted: false,
            wedged_exit: false,
            now,
            bytes_good_before,
            initial_batch,
            work_total,
        }
    }
}

// Why [`PatchCtx::recover_section`] returned. [`PatchCtx::run`] advances to
// the next bad range on `Completed`, and ends the whole pass only on
// `Halted` or `TransportFault` (matching `state.*` flag already set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionOutcome {
    /// Section drained: recovered what was readable, left the rest NonTrimmed.
    Completed,
    /// Halt requested (the halt token, also checked at each progress report).
    /// `state.halted` is set.
    Halted,
    /// USB-bridge transport fault: a dead bus, not a bad sector.
    /// `state.wedged_exit` is set.
    TransportFault,
}

// Per-pass coordination state for one `Disc::patch` run. `state` carries
// ACROSS ranges; per-range scratch is reset at the top of `recover_section`.
struct PatchCtx<'a, 'o> {
    disc: &'a libfreemkv::Disc,
    reader: &'a mut dyn SectorSource,
    pipe: &'a Pipeline<PatchItem, PatchSummary>,
    shared: &'a Mutex<SharedPatchState>,
    opts: &'a PatchOptions<'o>,
    // The op's cancellation (with `opts.halt`) and the libfreemkv token that follows it.
    halt: &'a EngineHalt<'a>,
    lib: &'a libfreemkv::halt::Halt,
    total_bytes: u64,
    state: PatchLoopState,
    /// Per-rip handler scorecard: grades handlers by recovery rate so the
    /// coordinator runs the winners first and lets duds fall back. Reset per
    /// pass (ephemeral, no persistence).
    scoreboard: HandlerScoreboard,
    /// Consecutive wedge-family senses across the WHOLE pass. Seeded into each
    /// per-section `HandlerCtx` and read back after, so a drive fast-fail wedge is
    /// detected even when every bad sub-range is smaller than the abort streak.
    wedge_streak: u32,
    /// The read speed the drive was last set to, carried across ranges; `None` until set.
    drive_speed: Option<u16>,
}

// Build the handler chain for one breadth-first tier (0 fast scouts, 1 slow-deep, 2 marginal
// specialists). The scorecard re-orders WITHIN a tier.
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
        // Tier 2 — marginal specialists, run only on the hardened residual tiers
        // 0-1 leave; each targets one failure mode and self-deprioritises via the
        // scorecard if it doesn't fit this disc. Reads are wedge-safe; additive.
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
                // Slow spin: Linear fwd + rev at min speed.
                Box::new(Linear {
                    direction: Direction::Reverse,
                    params: min_deep,
                }),
                Box::new(Linear {
                    direction: Direction::Forward,
                    params: min_deep,
                }),
                // FUA retry: Linear fwd + rev + Bisect under FUA (multiple physical
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
                // Slow + FUA: the hardest sector — min speed AND FUA.
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

// The FLAT handler pool — every technique from all tiers in ONE chain (data-driven bandit, no
// tier gate). Enabled by `FREEMKV_PATCH_FLAT` (see `patch_flat_mode`).
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

// The pass label the front end renders: a single-sector batch is a SCRAPE
// pass, anything larger a TRIM pass; `reverse` decorates whichever it is.
// Shared by `report_patch_progress` so the rule has one implementation.
pub(super) fn pass_kind(initial_batch: u16, reverse: bool) -> libfreemkv::progress::PassKind {
    if initial_batch == 1 {
        libfreemkv::progress::PassKind::Scrape { reverse }
    } else {
        libfreemkv::progress::PassKind::Trim { reverse }
    }
}

// The scheduler knobs, as pure functions of the raw env value: lets tests pin parsing without
// WRITING to the process environment.
fn flat_mode_from_value(v: Option<&str>) -> bool {
    matches!(v, Some(v) if !v.is_empty() && v != "0")
}

// See [`flat_mode_from_value`]. Default 12 s, floored at 1. Deliberately NOT
// ceilinged; the absurd end is handled in [`handler_deadline`], which
// saturates instead of panicking.
fn flat_budget_from_value(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(12)
        .max(1)
}

// Test-only, thread-local overrides for the two knobs; `set_var`-ing the
// real keys is unsound (see [`flat_mode_from_value`]).
#[cfg(test)]
#[path = "patch_knob_override_tests.rs"]
mod knob_override;

/// Set the flat-mode knob for the current thread; `None` restores the env-based
/// value. Test-only.
#[cfg(test)]
fn flat_mode_override(v: Option<bool>) {
    knob_override::FLAT_MODE.with(|c| c.set(v));
}

/// Set the per-handler budget for the current thread; `None` restores the
/// env-based value. Test-only.
#[cfg(test)]
fn flat_budget_override(v: Option<u64>) {
    knob_override::FLAT_BUDGET.with(|c| c.set(v));
}

/// True when the flat-pool bandit scheduler is requested (`FREEMKV_PATCH_FLAT`
/// set to anything but empty / `0`). Default (unset) keeps the tier ladder.
fn patch_flat_mode() -> bool {
    #[cfg(test)]
    if let Some(v) = knob_override::FLAT_MODE.with(|c| c.get()) {
        return v;
    }
    flat_mode_from_value(std::env::var("FREEMKV_PATCH_FLAT").ok().as_deref())
}

// The deadline `budget_secs` seconds from `now`, saturating instead of panicking on an
// unbounded env-sourced value.
fn handler_deadline(now: std::time::Instant, budget_secs: u64) -> std::time::Instant {
    let mut secs = budget_secs;
    loop {
        if let Some(t) = now.checked_add(std::time::Duration::from_secs(secs)) {
            return t;
        }
        // Halve until it fits. Terminates: `secs == 0` always fits.
        secs /= 2;
    }
}

/// Short per-handler EXPLORE budget for the flat bandit (seconds). Keeps any
/// one handler from hogging a range. `FREEMKV_PATCH_FLAT_BUDGET` overrides;
/// default 12 s, floored at 1.
fn flat_handler_budget_secs() -> u64 {
    #[cfg(test)]
    if let Some(v) = knob_override::FLAT_BUDGET.with(|c| c.get()) {
        return v;
    }
    flat_budget_from_value(std::env::var("FREEMKV_PATCH_FLAT_BUDGET").ok().as_deref())
}

impl PatchCtx<'_, '_> {
    /// Orchestrator (one pass): walk the ordered bad ranges, recovering
    /// each and stopping the whole pass on halt / wedge / transport-fault.
    fn run(&mut self, bad_ranges: &[(u64, u64)]) -> Result<()> {
        let num_ranges = bad_ranges.len();
        // Attack the LARGEST ranges first: big NonTrimmed regions are usually
        // sweep-jump over-marks that read straight back, so tier 0 recovers the
        // bulk of the disc early instead of grinding fragments (ties: low LBA).
        let mut ordered: Vec<(u64, u64)> = bad_ranges.to_vec();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        // Per-range still-bad sets, persisted ACROSS the breadth-first tiers so
        // tier N+1 works on exactly what tier N left behind.
        let mut sections: Vec<SubRanges> = ordered
            .iter()
            .map(|&(p, l)| {
                // The single ingress establishing "all offsets are sector multiples".
                let (p, l) = super::snap_to_sectors(p, l);
                SubRanges::from_section(p, l)
            })
            .collect();

        // Two schedulers pick the handler chain per range: FLAT bandit runs one
        // scorecard-ordered pass over the full pool (best for a hardened, late-
        // resume residual); TIER sweeps easy bulk first, then escalates (fresh rip).
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

    /// Run ONE breadth-first tier of the handler chain over one range's
    /// still-bad set. `final_tier` records the surviving residue as
    /// NonTrimmed. Cross-range scheduling lives in [`PatchCtx::run`].
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
        // A Stop before this range's first READ issues none (§5.4 ET5 "before the first READ").
        if self.halt.is_cancelled() {
            self.state.halted = true;
            return Ok(RegionOutcome::Halted);
        }
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.region.enter",
            tier,
            flat,
            range_index = range_idx,
            num_total_ranges = num_ranges,
            range_lba = range_pos / SECTOR,
            range_size_mb = range_size as f64 / 1_048_576.0,
            bad_bytes = bad.total_len(),
            "entering patch range"
        );

        // Enter at max read speed (handlers pick their own via `ReadParams`, and `read_span`
        // restores max after each). Programmed only when the drive's speed is unknown or
        // was left lower, not on every range.
        if self.drive_speed != Some(SPEED_MAX_KBS) {
            self.reader.set_speed(SPEED_MAX_KBS);
        }

        // Handler roster. FLAT mode: the whole pool in one chain, best-first by
        // the rip scorecard. TIER mode: just this tier's roster, likewise
        // scorecard-ordered within the tier. Either way the scorecard re-learns per disc.
        let mut handlers: Vec<Box<dyn SectionHandler>> = if flat {
            build_flat_pool()
        } else {
            build_tier_handlers(tier)
        };

        // Clock seam: handlers read wall time through this so tests can wind a
        // fake clock (the same seam the pass uses for its own timing).
        let now_ptr = self.state.now;
        let now_fn = move || now_ptr();

        // Pass-local cancel latch: the tick's `should_cancel()` answer used to be
        // discarded, so a caller with `opts.halt: None` waited out full per-handler
        // budgets before cancelling. `Arc` so it doubles as a `Halt` when unset.
        let pass_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Halt token for the sends below: the op's when wired, not the pass latch — a
        // producer parked in `send` can't be reached by the tick-flipped latch.
        let pass_halt = if self.halt.is_wired() {
            self.lib.clone()
        } else {
            libfreemkv::halt::Halt::from_arc(pass_cancel.clone())
        };

        let mut sink = PatchRecoverySink {
            pipe: self.pipe,
            halt: &pass_halt,
        };

        let bad_before = bad.total_len();
        let (outcome, wedge_after, fatal, speed_after) = {
            // Progress heartbeat: a throttled closure pushing a fresh snapshot to the
            // reporter on every read, so the bar/speed move during a handler, not just
            // at section end. Scoped here so its `self.state` borrow ends before below.
            let disc = self.disc;
            let opts = self.opts;
            let shared = self.shared;
            let total_bytes = self.total_bytes;
            let state = &self.state;
            // `None` = no tick yet, so the FIRST read ticks immediately instead of
            // waiting out PROGRESS_TICK_MS — makes a cancel observable on read 1
            // and gets the progress bar moving at section start, not a quarter-second later.
            let last_tick: std::cell::Cell<Option<std::time::Instant>> = std::cell::Cell::new(None);
            let cancel = &pass_cancel;
            let ext_halt = self.halt;
            let mut tick = move || {
                // Mirror the op's cancel on EVERY read, not just on a throttled tick, so a
                // wired caller is not made less responsive by routing through this latch.
                if ext_halt.is_cancelled() {
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
                    if report_patch_progress(disc, state, opts, total_bytes, shared, ext_halt) {
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
                halt: Some(pass_cancel.as_ref()),
                tick: Some(&mut tick),
                unproductive: 0,
                fatal: None,
                // Carry the pass-level wedge streak in so a fast-fail wedge is
                // caught across many small sections, not reset each one.
                wedge_streak: self.wedge_streak,
                // The drive is at max (above); read_span tracks changes.
                cur_speed: SPEED_MAX_KBS,
            };
            // Per-handler time budget. FLAT is EXPLORE-first: a short slice per handler
            // so all 16 get a quick turn and the scorecard learns which land bytes.
            // TIER keeps the full 60 s window. Both tunable via `FREEMKV_PATCH_FLAT_BUDGET`.
            let budget_secs = if flat {
                flat_handler_budget_secs()
            } else {
                PER_HANDLER_BUDGET_SECS
            };
            let o = run_handlers(&mut ctx, &mut handlers, bad, &mut self.scoreboard, |_bad| {
                handler_deadline(now_ptr(), budget_secs)
            });
            (o, ctx.wedge_streak, ctx.fatal.take(), ctx.cur_speed)
        };
        self.wedge_streak = wedge_after;
        self.drive_speed = Some(speed_after);
        // A non-read error (or a sink that can no longer write) ended the chain: fail
        // the pass with it, before any residue is recorded NonTrimmed as damage.
        if let Some(e) = fatal {
            return Err(e);
        }

        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.region.exit",
            tier,
            range_index = range_idx,
            range_lba = range_pos / SECTOR,
            outcome = ?outcome,
            bad_bytes_before = bad_before,
            bad_bytes_after = bad.total_len(),
            recovered = bad_before.saturating_sub(bad.total_len()),
            "region tier finished"
        );

        // On the FINAL tier, whatever is still bad is this pass's residue: record
        // NonTrimmed and account the range once. A later pass gets another shot;
        // the orchestrator promotes to Unreadable only after the final pass.
        if final_tier {
            for &(pos, len) in bad.ranges() {
                match super::send_bounded(self.pipe, PatchItem::NonTrimmed { pos, len }, &pass_halt)
                {
                    Ok(()) => {}
                    // Same reasoning as the sink above: a Stop that lands
                    // while this residue send is parked ends the pass halted,
                    // not failed.
                    Err(super::SendStall::Halted) => {
                        self.state.halted = true;
                        return Ok(RegionOutcome::Halted);
                    }
                    Err(stall) => return Err(stall.into_error()),
                }
            }
        }

        if report_patch_progress(
            self.disc,
            &self.state,
            self.opts,
            self.total_bytes,
            self.shared,
            self.halt,
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
            // spin-cycle the drive and resume from the mapfile next pass. `Fatal`
            // returned its error above; ending the pass is the safe reading if not.
            HandlerOutcome::TransportFault | HandlerOutcome::Fatal => {
                debug_assert!(
                    outcome != HandlerOutcome::Fatal,
                    "a Fatal chain end must carry its error in ctx.fatal"
                );
                self.state.wedged_exit = true;
                Ok(RegionOutcome::TransportFault)
            }
        }
    }
}

// Build + dispatch a `PassProgress` to the caller's reporter, using the current
// pipeline-shared mapfile snapshot. `true` once `halt` is cancelled at this report
// (outer loop should set `state.halted` and break).
pub(super) fn report_patch_progress(
    disc: &libfreemkv::Disc,
    state: &PatchLoopState,
    opts: &PatchOptions,
    total_bytes: u64,
    shared: &Mutex<SharedPatchState>,
    halt: &EngineHalt<'_>,
) -> bool {
    let Some(reporter) = opts.progress else {
        return false;
    };
    // Both figures come from the snapshot, derived over the COMPLETE damage set.
    // They used to be computed from a range list capped at 8192 entries, which
    // silently under-reported at-risk bytes on a disc fragmented past that cap.
    let (s, main_title_bad, located, still_bad_work) = {
        let g = lock_snapshot(shared);
        (
            g.stats,
            g.bad_bytes_in_title,
            g.located.clone(),
            g.damage_in_scope,
        )
    };
    let kind = pass_kind(state.initial_batch, opts.reverse);
    let main_title = disc.titles.first();
    // Progress = bytes RECOVERED, not a per-range counter, since breadth-first
    // tiers bring back readable bulk before any range "finishes". `still_bad_work` is the
    // damage inside the scope, as `work_total` is.
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
        // derived by the sink from the in-memory bad-range set + title so the
        // client renders it verbatim and never reads the mapfile.
        located,
    };
    reporter.event(&libfreemkv::Event::Pass(&pp));
    halt.is_cancelled()
}

/// Bytes of bad/unreadable data in a title's extents, from a mapfile.
///
/// Front ends call this after a rip pass to determine
/// how much damage affects a particular title — useful for showing
/// "42s lost (12s in main movie)" in the UI.
pub fn bytes_bad_in_title_from_mapfile(
    mapfile_path: &std::path::Path,
    title: &libfreemkv::DiscTitle,
) -> u64 {
    let map = match mapfile::Mapfile::load(mapfile_path) {
        Ok(m) => m,
        // A MISSING mapfile is legitimate (no damage tracked): 0 is correct. Any
        // OTHER load error means we CANNOT know the damage, so fail safe by
        // reporting the ENTIRE title as bad rather than returning 0 ("clean").
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

/// [`patch()`] under the op token `op` (stop design v5 §4.2), OR'd with `opts.halt`; the
/// pass latch stays exempt. A Stop is [`crate::EngineOutcome::Stopped`].
pub fn patch_with(
    op: &libfreemkv::Halt,
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &PatchOptions,
) -> crate::EngineOutcome<PatchOutcome> {
    let halt = crate::EngineHalt::new(op, opts.halt.clone());
    let r = patch_in(disc, reader, path, opts, &halt);
    crate::EngineOutcome::from_result(r, &halt, |r| r.halted)
}

/// Pass 2..N of a multipass rip: re-read the bad ranges recorded in the
/// sidecar mapfile and try to recover them.
///
/// The walk is LARGEST-RANGE-FIRST (ties: lowest LBA first), not
/// positional: the big `NonTrimmed` regions are usually sweep skip-ahead
/// overshoot that reads straight back, so taking them first recovers the
/// bulk of the disc in the first minutes. Returns a [`PatchOutcome`] with
/// recovered byte counts and wedge-detection signals. Paired with
/// [`super::sweep()`]; caller drives the retry loop and pass dispatch.
pub fn patch(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &PatchOptions,
) -> Result<PatchOutcome> {
    patch_in(
        disc,
        reader,
        path,
        opts,
        &EngineHalt::legacy(opts.halt.clone()),
    )
}

// A patch pass under `halt`, which already holds `opts.halt`.
pub(crate) fn patch_in(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &PatchOptions,
    halt: &EngineHalt<'_>,
) -> Result<PatchOutcome> {
    halt.linked(|lib| patch_linked(disc, reader, path, opts, halt, lib))
}

// `patch_in` with `lib`, the libfreemkv token that follows `halt`.
fn patch_linked(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &PatchOptions,
    halt: &EngineHalt<'_>,
    lib: &libfreemkv::halt::Halt,
) -> Result<PatchOutcome> {
    use libfreemkv::io::pipeline::{Pipeline, WRITE_THROUGH_DEPTH};

    // Pre-flight decrypt gate (also enforced in `copy`; re-checked here so a
    // direct `patch` caller can't bypass it): a decrypting pass with no usable
    // key would write ciphertext into recovered ranges. No-op for `--raw`.
    crate::resolve::ensure_decryptable_with(disc, !opts.decrypt, opts.keys.as_ref())?;

    let patch_t0 = std::time::Instant::now();
    let mapfile_path = disc.mapfile_for(path);
    let InitialState {
        map,
        stats: initial_stats,
        total_bytes,
        bad_ranges,
        work_total,
        is_regular,
    } = compute_initial_state(path, &mapfile_path)?;
    // AACS BD Pre-recorded 0.953 §3.7 Note: "PC Host shall decrypt bus-encrypted Clip AV
    // stream file". A scoped (MKV-staging) map only re-reads its scope, where none is lost.
    if map.scope().is_none() {
        libfreemkv::sector::bus_removal::ensure_image_debussable(reader)?;
    }
    // Same reasoning as the decrypt gate: `copy`/`sweep` verify the mapfile
    // describes THIS disc, and `patch` must not skip that — otherwise a leftover
    // mapfile from disc B patches its ranges into disc A's ISO as "Finished".
    mapfile::check_mapfile_identity(&map, disc, opts.keys.as_ref()).map_err(Error::from)?;
    // COVERAGE. `total_bytes` (the denominator every reported figure derives
    // from) comes wholly from the untrusted mapfile, never checked against the
    // drive. `copy` forces a fresh sweep on mismatch; `patch` refuses instead.
    if total_bytes != disc.capacity_bytes {
        tracing::info!(
            target: "freemkv::scan",
            phase = "patch",
            mapfile_total = total_bytes,
            disc_capacity = disc.capacity_bytes,
            "refusing: the mapfile does not cover the disc in the drive"
        );
        return Err(Error::MapfileInvalid { kind: "coverage" });
    }
    // Same argument as the identity gate: patch never re-reads what the mapfile
    // calls Finished, so a since-truncated image reports success over data past
    // the cut. `copy`/`sweep` force a fresh sweep on this; `patch` refuses.
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

    // Decrypt-aware read, identical to `sweep` (`--raw` copies ciphertext verbatim); AACS
    // reads widen onto each file's unit grid. Bad sectors = PHYSICAL read failure.
    let mut reader = super::whole_disc::whole_disc_decrypting_reader(
        disc,
        reader,
        opts.decrypt,
        halt.is_wired().then_some(lib),
        opts.keys.as_ref(),
    )?;
    let reader = &mut reader;
    // Logged from the live map before it moves into the sink (no entry copy).
    log_patch_start_snapshot(map.entries(), &initial_stats, bytes_good_before);

    // Spawn the consumer: `WritebackFile`/`Mapfile` move into the sink; the shared
    // snapshot lets producer callbacks read consumer effects. Disown taken
    // BEFORE the move so abandoned teardown can still stop a stale mapfile write.
    let map_disown = map.disown_handle();
    let (sink, shared) = PatchSink::new(path, map, is_regular, disc.titles.first().cloned())?;
    // WRITE_THROUGH_DEPTH (=1): patch reads one sector per recovery decision and
    // checks consumer-published stats inline. Sweep's depth-4 default would let
    // recovered sectors queue between decisions, breaking this per-sector lockstep.
    let pipe = Pipeline::<PatchItem, _>::spawn(WRITE_THROUGH_DEPTH, sink)?;
    // Halt token for the teardown below: the op's, as `run`'s `pass_halt`; with none wired,
    // a never-cancelled token leaves the join's no-progress window as its only bound (T7).
    let finish_halt = lib.clone();

    // Log ISO file size at patch start for write monitoring
    if let Ok(metadata) = std::fs::metadata(path) {
        tracing::info!(
            target: "freemkv::disc",
            phase = "patch.iso_size.start",
            iso_bytes = metadata.len(),
            "ISO file size at patch start"
        );
    }

    // Read sizing / fast-vs-deep recovery are owned by the handler chain now, not
    // the old adaptive current_batch loop. `block_sectors`/`full_recovery` survive
    // only as informational PassKind labels; clamp to ≥1 to avoid underflow.
    let initial_batch = initial_batch_of(opts);
    let recovery = opts.full_recovery;

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
        bytes_good_start = bytes_good_before,
        "Disc::patch entered"
    );

    // Drive the recovery: build the per-pass context, then walk the ordered bad
    // ranges. `run` owns the tier / range walk and the pass-ending conditions;
    // `recover_section` runs one tier's handler chain over one range.
    let mut ctx = PatchCtx {
        disc,
        reader,
        pipe: &pipe,
        shared: &shared,
        opts,
        halt,
        lib,
        total_bytes,
        state: PatchLoopState::new(bytes_good_before, initial_batch, work_total),
        scoreboard: HandlerScoreboard::default(),
        wedge_streak: 0,
        drive_speed: None,
    };
    // Hold the pass result rather than `?`-ing it: the teardown below runs
    // `PatchSink::close` (sync_all + mapfile.flush), and returning early here
    // skipped it, leaving the on-disk damage record unflushed on error paths.
    let run_result = ctx.run(&bad_ranges);
    ctx.scoreboard.log();
    let PatchCtx { state, .. } = ctx;

    // Drain the consumer unconditionally: drop tx, wait for `close`, take final
    // stats. Bounded by the SAME Stop bit `run`'s sends use — a plain
    // `Pipeline::finish` once hung joining a wedged consumer and swallowed a halt.
    let consumer_failed = pipe.consumer_failed();
    let finish_result = super::finish_bounded_disowning(pipe, &finish_halt, &map_disown);
    let summary = settle(run_result, finish_result, consumer_failed)?;

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
#[path = "patch_tests.rs"]
mod tests;

// The live progress drilldown is built from `SharedPatchState` over the COMPLETE
// damage set; the totals `report_patch_progress` derives must not shrink when a
// disc fragments past libfreemkv's display cap (or the old 8192-entry one).
#[cfg(test)]
#[path = "patch_truncated_range_reporting_tests.rs"]
mod truncated_range_reporting_tests;

// `bytes_bad_in_title_from_mapfile` feeds the CLI's damage report; nothing
// constrained it before (a mutation run replaced the body with `0` and the
// suite stayed green). Each of its three documented answers gets a test.
#[cfg(test)]
#[path = "patch_bytes_bad_from_mapfile_tests.rs"]
mod bytes_bad_from_mapfile_tests;

#[cfg(test)]
#[path = "patch_snap_tests.rs"]
mod snap_tests;
