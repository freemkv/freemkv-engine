//! Handler-chain recovery of a single bad section (Pass-N rework, #55).
//!
//! A coordinator runs a chain of time-bounded recovery *handlers* (read
//! backwards, forwards, fast, slow, bisect...) over one section's still-bad
//! sub-ranges. Each handler gets a wall-clock `deadline`, shrinks the shared
//! [`SubRanges`], and returns [`HandlerOutcome::Remaining`] for the NEXT
//! handler to try. Recovered bytes flow through [`RecoverySink`]. Wired
//! into `recover_section` (#55) via the live [`run_handlers`] engine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::patch::{SubRanges, recovery_read};
use libfreemkv::error::Error;
use libfreemkv::scsi::SenseFamily;
use libfreemkv::sector::SectorSource;

/// One 2048-byte sector.
pub(super) const SECTOR: u64 = 2048;
/// Batch size a linear handler reads at once (sectors). A failed batch is left
/// bad whole (no per-sector re-read); Bisect salvages readable islands in it.
const BATCH_SECTORS: u64 = 32;

/// `Jump` handler: after this many consecutive failed batches it jumps to the
/// middle of the remaining span (see the handler) to find where readable data
/// resumes rather than reading every dead sector.
const JUMP_AFTER_FAILS: u32 = 2;

/// Early-yield: after this many consecutive unproductive reads, a handler hands the still-bad
/// set to the next handler instead of grinding its whole budget on a dead zone.
const UNPRODUCTIVE_YIELD: u32 = 4;

/// Wedge abort: after this many CONSECUTIVE wedge-family senses, `read_span` escalates to
/// `Transport` and the whole pass aborts rather than hammering remaining sections.
const WEDGE_ABORT_STREAK: u32 = 16;

/// A wedge-family failure only counts toward WEDGE_ABORT_STREAK if it came back faster than
/// this, so slow genuine ECC recovery doesn't false-trip the wedge abort.
const WEDGE_FASTFAIL_MS: u64 = 500;

// DELIBERATE DIVERGENCE from read_error.rs's WEDGE_ABORT_THRESHOLD (also 16, no
// latency gate): Pass N is the last, targeted attempt, so it affords slow genuine
// ECC retries; Pass 1 sweeps cheaply. Confirmed deliberate — do NOT "unify".

/// Max read speed sentinel for `SET CD SPEED` (0xFFFF = "as fast as the drive
/// will go"). The default for every read; a handler that wants to slow the
/// spindle passes [`SpeedPref::Min`] and [`read_span`] restores this on exit.
pub(super) const SPEED_MAX_KBS: u16 = 0xFFFF;

/// Min read speed (~DVD 1x). Slower rotation gives the servo more dwell and
/// the ECC engine more integration time per sector (min-speed [`Linear`] and
/// [`SpeedSweep`]). Only needs to be well below max; the drive rounds to a supported step.
const SPEED_MIN_KBS: u16 = 1385;

/// Which spindle speed a read requests. `Max` is the streaming default; `Min`
/// slows the spindle for marginal-sector recovery (more servo dwell + ECC
/// integration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpeedPref {
    Max,
    Min,
}

impl SpeedPref {
    /// The `SET CD SPEED` value (KB/s) this preference maps to.
    fn kbs(self) -> u16 {
        match self {
            SpeedPref::Max => SPEED_MAX_KBS,
            SpeedPref::Min => SPEED_MIN_KBS,
        }
    }
}

/// Which SCSI read timeout a read requests. `Fast` is the 10 s single-attempt
/// budget (scouting); `Deep` is the 60 s ECC-recovery budget (deep recovery).
/// Maps onto `recovery_read`'s `recovery` bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeoutPref {
    Fast,
    Deep,
}

impl TimeoutPref {
    /// The `recovery` bool (true = 60 s deep) this timeout maps to.
    fn recovery(self) -> bool {
        matches!(self, TimeoutPref::Deep)
    }
}

/// The per-read knobs a handler hands to read_span: speed / cache(FUA) /
/// timeout (direction is the handler's own walk). A new technique is a new
/// parameterisation of the same read primitive, never a bypass of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadParams {
    pub speed: SpeedPref,
    pub fua: bool,
    pub timeout: TimeoutPref,
}

impl ReadParams {
    /// Tier-0 scout read: max speed, cache on, 10 s single-attempt.
    pub(super) fn fast() -> Self {
        Self {
            speed: SpeedPref::Max,
            fua: false,
            timeout: TimeoutPref::Fast,
        }
    }

    /// Tier-1 deep read: max speed, cache on, 60 s ECC-recovery budget.
    pub(super) fn deep() -> Self {
        Self {
            speed: SpeedPref::Max,
            fua: false,
            timeout: TimeoutPref::Deep,
        }
    }

    /// Scorecard tag for the speed / cache / timeout axes, e.g. `min:fua:deep`.
    /// The handler prepends its own name + direction (`linear:fwd:` + tag).
    fn tag(&self) -> String {
        let speed = match self.speed {
            SpeedPref::Max => "max",
            SpeedPref::Min => "min",
        };
        let timeout = match self.timeout {
            TimeoutPref::Fast => "fast",
            TimeoutPref::Deep => "deep",
        };
        if self.fua {
            format!("{speed}:fua:{timeout}")
        } else {
            format!("{speed}:{timeout}")
        }
    }
}

/// Where a handler left the section after its bounded attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HandlerOutcome {
    /// The still-bad set is now empty — the section is fully recovered. The
    /// coordinator stops the chain.
    Complete,
    /// The handler finished or hit its deadline with bad sub-ranges remaining —
    /// the coordinator moves to the next handler.
    Remaining,
    /// The caller's halt token was observed set — abort the chain.
    Halted,
    /// A transport-layer fault (bridge wedge / dead bus) — the device never
    /// answered. The coordinator returns this so the caller can un-wedge
    /// (spin-cycle) before deciding whether to continue.
    TransportFault,
    /// A non-read error (a key stop, or a sink that can no longer write) ended
    /// the chain; the error is in [`HandlerCtx::fatal`]. Not disc damage and
    /// not a dead bus: the caller fails the pass with it.
    Fatal,
}

/// Receives sectors a handler successfully read back. Kept minimal and
/// decoupled from `PatchSink` so handlers are unit-testable in isolation; the
/// live wiring maps `recovered` onto the mapfile write + Finished mark.
pub(super) trait RecoverySink {
    /// `buf` holds the plaintext bytes for the byte-range `[pos, pos+buf.len())`
    /// (all multiples of [`SECTOR`]). `Err` means nothing more can be written:
    /// the span is not recovered and the chain ends [`HandlerOutcome::Fatal`].
    /// `Ok` is not a durability promise: on a halt the span may go unwritten
    /// (the mapfile keeps it bad) while the chain winds down Halted.
    fn recovered(&mut self, pos: u64, buf: &[u8]) -> Result<(), Error>;
}

/// Everything a handler needs, borrowed for the duration of one `recover` call.
/// The `deadline` is passed separately to `recover` (not stored here) so each
/// handler invocation is independently bounded.
pub(super) struct HandlerCtx<'a> {
    pub reader: &'a mut dyn SectorSource,
    pub sink: &'a mut dyn RecoverySink,
    /// Clock seam — handlers read wall time through this, never `Instant::now()`
    /// inline, so tests advance a fake clock deterministically.
    pub now: &'a dyn Fn() -> Instant,
    pub halt: Option<&'a AtomicBool>,
    /// Progress heartbeat. Handlers call [`HandlerCtx::progress`] frequently (it
    /// is internally throttled); this pushes a fresh progress snapshot to the
    /// caller's reporter DURING a handler, not just at range boundaries — so the
    /// bar and speed move as recovery happens instead of jumping once per
    /// section. `None` in tests (no reporter).
    pub tick: Option<&'a mut dyn FnMut()>,
    /// Consecutive reads that recovered nothing, updated by [`read_span`]. When
    /// it reaches [`UNPRODUCTIVE_YIELD`] the handler should yield to the next one
    /// (see [`HandlerCtx::stalled`]). Reset to 0 before each handler runs.
    pub unproductive: u32,
    /// Consecutive wedge-family senses (Hardware / IllegalRequest), updated by
    /// [`read_span`]. At [`WEDGE_ABORT_STREAK`] the drive is wedged and the read
    /// escalates to `Transport`. Seeded from and read back into the pass-level
    /// counter so the streak spans sections; a Good or non-wedge read resets it.
    pub wedge_streak: u32,
    /// The spindle speed (`SET CD SPEED` KB/s) currently programmed into the
    /// drive. [`read_span`] issues `SET CD SPEED` only when a read's requested
    /// speed DIFFERS from this (a `SET CD SPEED` per read would thrash the
    /// spindle), and [`run_handlers`] restores [`SPEED_MAX_KBS`] after each
    /// handler. Seeded to max — the caller resets the drive to max before the
    /// chain runs.
    pub cur_speed: u16,
    /// The error behind a [`HandlerOutcome::Fatal`] (a key stop, or the sink
    /// refusing a span); the caller surfaces it instead of recording damage.
    pub fatal: Option<Error>,
}

impl HandlerCtx<'_> {
    fn halted(&self) -> bool {
        self.halt.is_some_and(|h| h.load(Ordering::Relaxed))
    }

    // "Stop this handler now" check every handler loop calls between reads:
    // true when the deadline passed OR the early-yield dead streak was hit,
    // so every handler hands off on a dead zone with no per-handler edits.
    fn past(&self, deadline: Instant) -> bool {
        self.stalled() || (self.now)() >= deadline
    }

    /// True once the handler has made `UNPRODUCTIVE_YIELD` reads (of any span) in a
    /// row with no recovery — its cue to hand the baton to the next handler instead of
    /// grinding a dead zone for its whole budget.
    fn stalled(&self) -> bool {
        self.unproductive >= UNPRODUCTIVE_YIELD
    }

    /// Deadline-only stop check (ignores the early-yield stall streak). Used
    /// inside Bisect's boundary-probing loops, where a short run of failing
    /// reads is the *expected* way to home in on a dead edge — not a stall.
    fn timed_out(&self, deadline: Instant) -> bool {
        (self.now)() >= deadline
    }

    /// Emit a progress heartbeat (throttling lives in the tick closure).
    fn progress(&mut self) {
        if let Some(t) = self.tick.as_mut() {
            t();
        }
    }
}

/// Outcome of one physical read attempt, before the caller decides what to do
/// with the still-bad set.
enum ReadHit {
    /// Bytes came back and were handed to the sink.
    Good,
    /// A recoverable bad-sector error (media / check-condition). Leave the span
    /// bad and move on.
    Bad,
    /// Transport-layer fault — the bus is gone. Abort now.
    Transport,
    /// A non-read error, recorded in [`HandlerCtx::fatal`]. End the chain now.
    Fatal,
}

/// Read `count` sectors at byte offset `pos` and, on success, hand them to the
/// sink. Does NOT touch the still-bad set — the caller removes recovered spans
/// so the read helper stays independent of `SubRanges`.
fn read_span(
    ctx: &mut HandlerCtx,
    buf: &mut [u8],
    pos: u64,
    count: u16,
    params: ReadParams,
) -> ReadHit {
    read_span_as(ctx, buf, pos, count, params, true)
}

// A prime read of the sector at `pos`: a still-bad one is a real read (recovered and counted);
// any other (already good, or outside this section) only warms the drive: nothing is written
// and it neither recovers nor breaks the dead streak.
fn prime_read(
    ctx: &mut HandlerCtx,
    buf: &mut [u8],
    pos: u64,
    params: ReadParams,
    bad: &SubRanges,
) -> ReadHit {
    read_span_as(ctx, buf, pos, 1, params, bad.contains(pos))
}

// [`read_span`]; `commit: false` reads without handing the bytes to the sink or resetting
// the dead streak.
fn read_span_as(
    ctx: &mut HandlerCtx,
    buf: &mut [u8],
    pos: u64,
    count: u16,
    params: ReadParams,
    commit: bool,
) -> ReadHit {
    let lba = (pos / SECTOR) as u32;
    let bytes = count as usize * SECTOR as usize;
    // Enforce sector-alignment at runtime, every build (not debug_assert!, which
    // compiles out under --release): a zero count would make `Ok(0) == bytes`
    // fire the Good path, falsely recording an unread span as recovered.
    if count == 0 || !pos.is_multiple_of(SECTOR) {
        tracing::error!(
            target: "freemkv::disc",
            pos,
            count,
            "read_span: refused a non-sector-aligned or empty span before reading; treating as a failed read so it is never falsely marked recovered"
        );
        return ReadHit::Bad;
    }
    // Program the spindle speed ONLY when it changes — a `SET CD SPEED` per read
    // would thrash the drive. `run_handlers` restores max after the handler.
    let want_speed = params.speed.kbs();
    if want_speed != ctx.cur_speed {
        ctx.reader.set_speed(want_speed);
        ctx.cur_speed = want_speed;
    }
    let recovery = params.timeout.recovery();
    let read_started = (ctx.now)();
    let hit = match recovery_read(ctx.reader, lba, count, buf, recovery, params.fua) {
        Ok(n) if n == bytes && !commit => ReadHit::Good,
        Ok(n) if n == bytes => match ctx.sink.recovered(pos, &buf[..bytes]) {
            Ok(()) => ReadHit::Good,
            // The span was read but cannot be written: stop reading into a dead sink.
            Err(e) => {
                ctx.fatal = Some(e);
                ReadHit::Fatal
            }
        },
        // A short transfer is a failed read, not partial recovery: `buf` is reused
        // across reads, so committing buf[..bytes] here would hand the sink a
        // previous span's tail. Should be unreachable; kept as a defensive check.
        Ok(_) => {
            ctx.wedge_streak = 0;
            ReadHit::Bad
        }
        Err(e) if e.is_scsi_transport_failure() => {
            tracing::warn!(
                target: "freemkv::disc",
                phase = "section_recover.transport",
                lba,
                code = e.code(),
                error = %e,
                "transport failure during recovery; ending the pass"
            );
            ReadHit::Transport
        }
        // Not disc damage: stop the chain now and hand the real error to the
        // caller via `ctx.fatal`.
        Err(e) if !super::is_damage_candidate(&e) => {
            tracing::warn!(
                target: "freemkv::disc",
                phase = "section_recover.fatal",
                lba,
                code = e.code(),
                "non-read error during recovery; aborting the pass with it"
            );
            ctx.fatal = Some(e);
            ReadHit::Fatal
        }
        Err(e) => {
            // Wedge watch: only a wedge-family sense AND a fast return (<
            // WEDGE_FASTFAIL_MS) count toward the streak. The latency gate keeps a
            // genuine slow ECC-recovery failure from false-tripping the abort.
            let sense_is_wedge = e
                .scsi_sense()
                .map(|s| SenseFamily::from_sense_key(s.sense_key).is_wedge_family())
                .unwrap_or(false);
            let elapsed = (ctx.now)().duration_since(read_started);
            let fast_fail = elapsed.as_millis() < WEDGE_FASTFAIL_MS as u128;
            if sense_is_wedge && fast_fail {
                ctx.wedge_streak = ctx.wedge_streak.saturating_add(1);
                if ctx.wedge_streak >= WEDGE_ABORT_STREAK {
                    tracing::warn!(
                        target: "freemkv::disc",
                        phase = "section_recover.wedge_abort",
                        lba,
                        streak = ctx.wedge_streak,
                        code = e.code(),
                        sense = ?e.scsi_sense(),
                        "consecutive fast-fail wedge senses; ending the pass as a transport fault"
                    );
                    ReadHit::Transport
                } else {
                    ReadHit::Bad
                }
            } else {
                ctx.wedge_streak = 0;
                ReadHit::Bad
            }
        }
    };
    // Track the dead streak for the early-yield hand-off: a recovering read
    // resets it, a fruitless one advances it toward UNPRODUCTIVE_YIELD.
    match hit {
        ReadHit::Good => {
            if commit {
                ctx.unproductive = 0;
            }
            ctx.wedge_streak = 0;
        }
        // A Bad read is unproductive grinding — advance the yield streak.
        ReadHit::Bad => ctx.unproductive = ctx.unproductive.saturating_add(1),
        // A Transport / Fatal hit aborts the handler immediately, so it is NOT
        // unproductive grinding — leave the streak untouched (it is never read
        // again after the chain ends).
        ReadHit::Transport | ReadHit::Fatal => {}
    }
    // Heartbeat after every read (the tick closure throttles to ~250 ms) so the
    // UI's bar/speed move DURING a handler, not just when the section finishes.
    ctx.progress();
    hit
}

/// One recovery idea, given a bounded shot at the section's still-bad set.
/// Contract: check `ctx.halted()` / `ctx.past(deadline)` between reads,
/// update `bad` on good/bad reads, and return `TransportFault` promptly.
pub(super) trait SectionHandler {
    /// Scorecard identity — the FULL config (technique + direction + speed +
    /// cache + timeout), e.g. `linear:fwd:min:fua:deep`. The scoreboard keys on
    /// this, so two instances of the same handler at different [`ReadParams`]
    /// score independently and can flip past each other.
    fn name(&self) -> String;
    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome;
}

/// Which end a [`Linear`] sweep walks from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// start→end (the front the reverse pass kept dying on).
    Forward,
    /// end→start (the disc sweep overshoots forward, so a NonTrimmed range's
    /// good data sits at its tail — reverse hits it first).
    Reverse,
}

impl Direction {
    fn is_reverse(self) -> bool {
        matches!(self, Direction::Reverse)
    }

    fn tag(self) -> &'static str {
        match self {
            Direction::Forward => "fwd",
            Direction::Reverse => "rev",
        }
    }
}

/// Linear batch sweep of each bad sub-range, in `direction`, at `params`.
/// The direction × [`ReadParams`] axes give every combination from one
/// handler — tier-0/1/2 specialists are all just `Linear` at different params.
pub(super) struct Linear {
    pub direction: Direction,
    pub params: ReadParams,
}

impl SectionHandler for Linear {
    fn name(&self) -> String {
        format!("linear:{}:{}", self.direction.tag(), self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        let reverse = self.direction.is_reverse();
        let batch_bytes = BATCH_SECTORS * SECTOR;
        let mut buf = vec![0u8; batch_bytes as usize];
        // Snapshot the sub-ranges: we mutate `bad` via remove() as we recover,
        // and iterating the snapshot keeps that from disturbing the walk.
        let mut snapshot: Vec<(u64, u64)> = bad.ranges().to_vec();
        if reverse {
            snapshot.reverse();
        }

        for (rp, rl) in snapshot {
            // Position within the range, in bytes, walked from whichever end.
            let mut done = 0u64;
            while done < rl {
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let span = batch_bytes.min(rl - done);
                let pos = if reverse {
                    rp + (rl - done - span)
                } else {
                    rp + done
                };
                let count = (span / SECTOR) as u16;
                match read_span(ctx, &mut buf, pos, count, self.params) {
                    ReadHit::Good => bad.remove(pos, span),
                    // Keep reads at the full batch — no per-sector grind (proven
                    // worse on the BU40N, and stalled a handler on a dead front).
                    // Leave the failed batch bad; Bisect salvages islands within it.
                    ReadHit::Bad => {}
                    ReadHit::Transport => return HandlerOutcome::TransportFault,
                    ReadHit::Fatal => return HandlerOutcome::Fatal,
                }
                done += span;
            }
        }

        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// Bisect + expand: probe the middle of a bad sub-range, and on a good read expand outward
/// until a read fails, recovering the readable island in large batches. Failing ends re-bisect.
pub(super) struct Bisect {
    pub params: ReadParams,
}

impl SectionHandler for Bisect {
    fn name(&self) -> String {
        format!("bisect:{}", self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        let batch = BATCH_SECTORS * SECTOR;
        let mut buf = vec![0u8; batch as usize];
        let mut probe = [0u8; SECTOR as usize];
        // Work stack of still-bad chunks. A good probe recovers the readable island
        // and pushes the two smaller failing ends; a dead probe pushes the two
        // halves. Either way the stack shrinks toward small clusters, draining bounded.
        let mut stack: Vec<(u64, u64)> = bad.ranges().to_vec();
        while let Some((rp, rl)) = stack.pop() {
            if rl == 0 {
                continue;
            }
            if ctx.halted() {
                return HandlerOutcome::Halted;
            }
            if ctx.past(deadline) {
                return HandlerOutcome::Remaining;
            }
            let end = rp + rl;
            let mid = rp + (rl / SECTOR / 2) * SECTOR;
            match read_span(ctx, &mut probe, mid, 1, self.params) {
                ReadHit::Good => {
                    bad.remove(mid, SECTOR);
                    // Expand FORWARD from mid+1 in batches until a read fails.
                    let mut fwd = mid + SECTOR;
                    let mut step = batch;
                    while fwd < end {
                        if ctx.halted() {
                            return HandlerOutcome::Halted;
                        }
                        if ctx.timed_out(deadline) {
                            return HandlerOutcome::Remaining;
                        }
                        let span = step.min(end - fwd);
                        let count = (span / SECTOR) as u16;
                        match read_span(ctx, &mut buf[..span as usize], fwd, count, self.params) {
                            ReadHit::Good => {
                                bad.remove(fwd, span);
                                fwd += span;
                                step = batch;
                            }
                            // Halve at the dead boundary instead of giving up, so
                            // the readable sectors right up to the dead one are
                            // recovered in ~log2(batch) reads (no per-sector grind).
                            ReadHit::Bad => {
                                if span > SECTOR {
                                    step = ((span / SECTOR) / 2).max(1) * SECTOR;
                                } else {
                                    break;
                                }
                            }
                            ReadHit::Transport => return HandlerOutcome::TransportFault,
                            ReadHit::Fatal => return HandlerOutcome::Fatal,
                        }
                    }
                    // Expand BACKWARD from mid toward rp until a read fails.
                    let mut bwd = mid;
                    let mut step = batch;
                    while bwd > rp {
                        if ctx.halted() {
                            return HandlerOutcome::Halted;
                        }
                        if ctx.timed_out(deadline) {
                            return HandlerOutcome::Remaining;
                        }
                        let span = step.min(bwd - rp);
                        let pos = bwd - span;
                        let count = (span / SECTOR) as u16;
                        match read_span(ctx, &mut buf[..span as usize], pos, count, self.params) {
                            ReadHit::Good => {
                                bad.remove(pos, span);
                                bwd = pos;
                                step = batch;
                            }
                            ReadHit::Bad => {
                                if span > SECTOR {
                                    step = ((span / SECTOR) / 2).max(1) * SECTOR;
                                } else {
                                    break;
                                }
                            }
                            ReadHit::Transport => return HandlerOutcome::TransportFault,
                            ReadHit::Fatal => return HandlerOutcome::Fatal,
                        }
                    }
                    // Locating this readable island was productive work; the failed
                    // reads pinning its dead edges are boundary probes, not a stall.
                    // Clear the streak so the re-bisect starts fresh.
                    ctx.unproductive = 0;
                    // The two failing ends stay bad — bisect them again to pin
                    // the exact dead sectors.
                    if bwd > rp {
                        stack.push((rp, bwd - rp));
                    }
                    if fwd < end {
                        stack.push((fwd, end - fwd));
                    }
                }
                ReadHit::Bad => {
                    // Dead middle: split and keep hunting for a good centre.
                    if mid > rp {
                        stack.push((rp, mid - rp));
                    }
                    let right = mid + SECTOR;
                    if right < end {
                        stack.push((right, end - right));
                    }
                }
                ReadHit::Transport => return HandlerOutcome::TransportFault,
                ReadHit::Fatal => return HandlerOutcome::Fatal,
            }
        }

        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// Blow through a LARGE dead run fast: after [`JUMP_AFTER_FAILS`] failed batches, skip ahead to
/// the middle of what remains (halving, no fixed cap), leaving the skipped span bad.
pub(super) struct Jump {
    pub params: ReadParams,
}

impl SectionHandler for Jump {
    fn name(&self) -> String {
        format!("jump:{}", self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        let batch = BATCH_SECTORS * SECTOR;
        let mut buf = vec![0u8; batch as usize];
        let snapshot: Vec<(u64, u64)> = bad.ranges().to_vec();
        for (rp, rl) in snapshot {
            let mut off = 0u64;
            let mut consec_fail = 0u32;
            while off < rl {
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let span = batch.min(rl - off);
                let pos = rp + off;
                let count = (span / SECTOR) as u16;
                match read_span(ctx, &mut buf[..span as usize], pos, count, self.params) {
                    ReadHit::Good => {
                        bad.remove(pos, span);
                        consec_fail = 0;
                        off += span;
                    }
                    ReadHit::Bad => {
                        consec_fail += 1;
                        if consec_fail >= JUMP_AFTER_FAILS {
                            // Sustained dead run — jump to the middle of the remaining
                            // span. Halving adapts to any size (the old fixed 8 MiB
                            // jump leapt over smaller ranges); skipped span stays bad.
                            let remaining = rl - off;
                            let step = ((remaining / 2) / SECTOR).max(1) * SECTOR;
                            off += step;
                            consec_fail = 0;
                        } else {
                            off += span;
                        }
                    }
                    ReadHit::Transport => return HandlerOutcome::TransportFault,
                    ReadHit::Fatal => return HandlerOutcome::Fatal,
                }
            }
        }
        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// SpeedSweep: per residual sector, try Max→Min spindle speeds until one reads (speed resonance
/// means the sweet spot isn't always the slowest). Single-sector.
pub(super) struct SpeedSweep {
    pub params: ReadParams,
}

impl SectionHandler for SpeedSweep {
    fn name(&self) -> String {
        format!("speedsweep:{}", self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        // Fastest first — resonance means the sweet spot isn't always the
        // slowest, and the fast read costs least when it happens to work.
        const SWEEP: [SpeedPref; 2] = [SpeedPref::Max, SpeedPref::Min];
        let mut probe = [0u8; SECTOR as usize];
        let snapshot: Vec<(u64, u64)> = bad.ranges().to_vec();
        for (rp, rl) in snapshot {
            let mut off = 0u64;
            while off < rl {
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let pos = rp + off;
                for speed in SWEEP {
                    let params = ReadParams {
                        speed,
                        fua: self.params.fua,
                        timeout: self.params.timeout,
                    };
                    match read_span(ctx, &mut probe, pos, 1, params) {
                        ReadHit::Good => {
                            bad.remove(pos, SECTOR);
                            break;
                        }
                        // Next speed, checking for a stop between (each read is up to 60s).
                        // `timed_out`, not `past`: a failed fast read is the technique.
                        // Between sectors the dead streak still yields (`past` above).
                        ReadHit::Bad => {
                            if ctx.halted() {
                                return HandlerOutcome::Halted;
                            }
                            if ctx.timed_out(deadline) {
                                return HandlerOutcome::Remaining;
                            }
                            continue;
                        }
                        ReadHit::Transport => return HandlerOutcome::TransportFault,
                        ReadHit::Fatal => return HandlerOutcome::Fatal,
                    }
                }
                off += SECTOR;
            }
        }
        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// CachePrime: read the good run immediately preceding a residual island to lock the drive's
/// PLL/servo, then read the island while the channel is warm.
pub(super) struct CachePrime {
    pub params: ReadParams,
}

impl SectionHandler for CachePrime {
    fn name(&self) -> String {
        format!("cacheprime:{}", self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        let batch_bytes = BATCH_SECTORS * SECTOR;
        let mut buf = vec![0u8; batch_bytes as usize];
        let mut prime = [0u8; SECTOR as usize];
        let snapshot: Vec<(u64, u64)> = bad.ranges().to_vec();
        for (rp, rl) in snapshot {
            if ctx.halted() {
                return HandlerOutcome::Halted;
            }
            if ctx.past(deadline) {
                return HandlerOutcome::Remaining;
            }
            // Prime: read the good sector immediately before the island to lock
            // the servo/PLL, so the boundary sector is read warm, not cold-seeked.
            if rp >= SECTOR {
                // A bad/absent preceding sector just means no prime — read cold.
                match prime_read(ctx, &mut prime, rp - SECTOR, self.params, bad) {
                    ReadHit::Transport => return HandlerOutcome::TransportFault,
                    ReadHit::Fatal => return HandlerOutcome::Fatal,
                    ReadHit::Good | ReadHit::Bad => {}
                }
            }
            // Now walk the island forward while warm; contiguous reads keep the
            // channel primed across it (each sector's predecessor was just read).
            let mut done = 0u64;
            while done < rl {
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let span = batch_bytes.min(rl - done);
                let pos = rp + done;
                let count = (span / SECTOR) as u16;
                match read_span(ctx, &mut buf[..span as usize], pos, count, self.params) {
                    ReadHit::Good => bad.remove(pos, span),
                    ReadHit::Bad => {}
                    ReadHit::Transport => return HandlerOutcome::TransportFault,
                    ReadHit::Fatal => return HandlerOutcome::Fatal,
                }
                done += span;
            }
        }
        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// Is the sector ABOVE `pos` still inside the disc? `capacity_sectors` is
/// `0` when the source does not know its own size (the trait default); the
/// prime then goes ahead exactly as it always did.
fn prime_above_is_in_range(pos: u64, capacity_sectors: u32) -> bool {
    if capacity_sectors == 0 {
        return true;
    }
    (pos / SECTOR) + 1 < capacity_sectors as u64
}

/// Oscillate: read each residual sector by ALTERNATING approach (forward-into vs reverse-into
/// priming), since a sector's servo lock can differ by approach direction.
pub(super) struct Oscillate {
    pub params: ReadParams,
}

impl SectionHandler for Oscillate {
    fn name(&self) -> String {
        format!("oscillate:{}", self.params.tag())
    }

    fn recover(
        &mut self,
        ctx: &mut HandlerCtx,
        bad: &mut SubRanges,
        deadline: Instant,
    ) -> HandlerOutcome {
        let mut probe = [0u8; SECTOR as usize];
        let snapshot: Vec<(u64, u64)> = bad.ranges().to_vec();
        for (rp, rl) in snapshot {
            let mut off = 0u64;
            while off < rl {
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let pos = rp + off;
                // Forward-into: prime from the sector below, then read the target. A
                // successful prime must be claimed via bad.remove — read_span already
                // handed the bytes to the sink; left in `bad` it reports false loss.
                if pos >= SECTOR {
                    match prime_read(ctx, &mut probe, pos - SECTOR, self.params, bad) {
                        ReadHit::Transport => return HandlerOutcome::TransportFault,
                        ReadHit::Fatal => return HandlerOutcome::Fatal,
                        ReadHit::Good => bad.remove(pos - SECTOR, SECTOR),
                        ReadHit::Bad => {}
                    }
                }
                // Any prime-below recovery above is already committed via
                // `bad.remove` — safe to check and return here.
                if ctx.halted() {
                    return HandlerOutcome::Halted;
                }
                if ctx.past(deadline) {
                    return HandlerOutcome::Remaining;
                }
                let recovered = match read_span(ctx, &mut probe, pos, 1, self.params) {
                    ReadHit::Good => {
                        bad.remove(pos, SECTOR);
                        true
                    }
                    ReadHit::Transport => return HandlerOutcome::TransportFault,
                    ReadHit::Fatal => return HandlerOutcome::Fatal,
                    ReadHit::Bad => false,
                };
                // Reverse-into: prime from the sector above (head from higher LBA).
                // Bounded at the top, mirroring the forward-into guard: unbounded, priming
                // one-past-end draws a wedge sense / IO error and aborts the pass.
                if !recovered && prime_above_is_in_range(pos, ctx.reader.capacity_sectors()) {
                    // The target-forward read above already committed any
                    // recovery via `bad.remove` — safe to check and return here.
                    if ctx.halted() {
                        return HandlerOutcome::Halted;
                    }
                    if ctx.past(deadline) {
                        return HandlerOutcome::Remaining;
                    }
                    // Claim a successful prime — see the prime-below comment. Above
                    // `pos`, `pos + SECTOR` is the next still-bad sector for ranges
                    // two+ sectors long, so a landed prime is a real recovery.
                    match prime_read(ctx, &mut probe, pos + SECTOR, self.params, bad) {
                        ReadHit::Transport => return HandlerOutcome::TransportFault,
                        ReadHit::Fatal => return HandlerOutcome::Fatal,
                        ReadHit::Good => bad.remove(pos + SECTOR, SECTOR),
                        ReadHit::Bad => {}
                    }
                    // The prime-above recovery above is already committed —
                    // safe to check and return before the final target read.
                    if ctx.halted() {
                        return HandlerOutcome::Halted;
                    }
                    if ctx.past(deadline) {
                        return HandlerOutcome::Remaining;
                    }
                    match read_span(ctx, &mut probe, pos, 1, self.params) {
                        ReadHit::Good => bad.remove(pos, SECTOR),
                        ReadHit::Transport => return HandlerOutcome::TransportFault,
                        ReadHit::Fatal => return HandlerOutcome::Fatal,
                        ReadHit::Bad => {}
                    }
                }
                off += SECTOR;
            }
        }
        if bad.is_empty() {
            HandlerOutcome::Complete
        } else {
            HandlerOutcome::Remaining
        }
    }
}

/// EWMA smoothing factor for the decayed recovery rate: higher = more reactive (leadership flips
/// sooner), lower = steadier.
const SCORE_EWMA_ALPHA: f64 = 0.5;

/// Per-rip handler scorecard: decayed recovery rate ([`SCORE_EWMA_ALPHA`] EWMA of bytes/second)
/// so the coordinator runs whoever is winning *now* first. Ephemeral.
#[derive(Default)]
pub(super) struct HandlerScoreboard {
    stats: std::collections::HashMap<String, ScoreStat>,
}

#[derive(Default, Clone, Copy)]
struct ScoreStat {
    /// Decayed recovery rate (bytes/second), the ranking signal. `None` until
    /// the first attempt that spent measurable time (a zero-elapsed call proves
    /// no rate). Seeded to the first timed sample, then EWMA'd.
    ewma_rate: Option<f64>,
    // Cumulative totals — for the operator log line only, NOT for ranking.
    recovered: u64,
    nanos: u128,
    attempts: u64,
}

impl HandlerScoreboard {
    /// Fold one timed sample (bytes/second) into the decayed rate.
    fn decay(prev: Option<f64>, sample: f64) -> f64 {
        match prev {
            None => sample,
            Some(p) => SCORE_EWMA_ALPHA * sample + (1.0 - SCORE_EWMA_ALPHA) * p,
        }
    }

    // Record one attempt: `recovered` bytes over `elapsed`. A barren attempt decays the score
    // DOWN, letting an exhausted early winner lose its lead.
    fn record(&mut self, name: &str, recovered: u64, elapsed: std::time::Duration) {
        let e = self.stats.entry(name.to_string()).or_default();
        e.recovered = e.recovered.saturating_add(recovered);
        e.nanos = e.nanos.saturating_add(elapsed.as_nanos());
        e.attempts += 1;
        let secs = elapsed.as_secs_f64();
        if secs > 0.0 {
            let sample = recovered as f64 / secs;
            e.ewma_rate = Some(Self::decay(e.ewma_rate, sample));
        }
    }

    /// Ranking key (higher runs earlier). Untried → top, so it gets calibrated.
    fn rank(&self, name: &str) -> u64 {
        match self.stats.get(name) {
            // Never attempted → top, so every handler is calibrated once.
            None => u64::MAX,
            // Attempted but no timed sample yet (e.g. returned Halted immediately).
            // Ranked at the bottom (0), not the top: otherwise a called-but-idle
            // handler perpetually crowds out proven performers.
            Some(s) => match s.ewma_rate {
                None => 0,
                // `as` saturates: NaN / negative rank 0, overflow ranks u64::MAX.
                Some(r) => r as u64,
            },
        }
    }

    /// Emit the scorecard to the log so the operator can see, per rip, which
    /// handler is pulling the weight and which is a dud on this drive/disc.
    pub(super) fn log(&self) {
        let mut rows: Vec<_> = self.stats.iter().collect();
        // Rank by the decayed rate (the live signal), highest first.
        rows.sort_by_key(|(name, _)| std::cmp::Reverse(self.rank(name)));
        for (name, s) in rows {
            let mbps = s.recovered as f64 / (s.nanos as f64 / 1e9).max(1e-9) / 1_048_576.0;
            tracing::info!(
                target: "freemkv::disc",
                phase = "scorecard",
                handler = name.as_str(),
                recovered_mb = s.recovered as f64 / 1_048_576.0,
                attempts = s.attempts,
                decayed_bytes_per_s = s.ewma_rate.unwrap_or(0.0),
                mb_per_s = mbps,
                "handler scorecard (this rip)"
            );
        }
    }
}

/// Run the handler chain over one section's still-bad set, ordered
/// best-first by the rip scorecard. Never-hang: each handler is
/// deadline-bounded, and `Halted` / `TransportFault` short-circuit.
pub(super) fn run_handlers(
    ctx: &mut HandlerCtx,
    handlers: &mut [Box<dyn SectionHandler>],
    bad: &mut SubRanges,
    scoreboard: &mut HandlerScoreboard,
    section_deadline_for: impl Fn(&SubRanges) -> Instant,
) -> HandlerOutcome {
    // Best-first by recovery rate so far; untried handlers rank top (calibrate).
    handlers.sort_by_key(|h| std::cmp::Reverse(scoreboard.rank(&h.name())));
    for handler in handlers.iter_mut() {
        if bad.is_empty() {
            return HandlerOutcome::Complete;
        }
        let name = handler.name();
        let before = bad.total_len();
        let deadline = section_deadline_for(bad);
        let started = (ctx.now)();
        // Fresh dead-streak budget per handler: each gets its own chance before
        // the early-yield trips.
        ctx.unproductive = 0;
        let outcome = handler.recover(ctx, bad, deadline);
        // A handler may have dropped the spindle (min-speed Linear / SpeedSweep) or
        // set FUA; restore max speed before the next handler so it starts from the
        // streaming default (FUA is a per-read param, so nothing to unwind there).
        if ctx.cur_speed != SPEED_MAX_KBS {
            ctx.reader.set_speed(SPEED_MAX_KBS);
            ctx.cur_speed = SPEED_MAX_KBS;
        }
        let elapsed = (ctx.now)().duration_since(started);
        let after = bad.total_len();
        scoreboard.record(&name, before.saturating_sub(after), elapsed);
        tracing::info!(
            target: "freemkv::disc",
            phase = "section_recover.handler",
            handler = name.as_str(),
            bad_bytes_before = before,
            bad_bytes_after = after,
            recovered = before.saturating_sub(after),
            outcome = ?outcome,
            // Set when `outcome` is Fatal: the non-read error that ended the chain.
            fatal_code = ctx.fatal.as_ref().map(|e| e.code()),
            "handler finished; remaining bad bytes carry to the next handler"
        );
        match outcome {
            HandlerOutcome::Complete => return HandlerOutcome::Complete,
            HandlerOutcome::Remaining => continue,
            HandlerOutcome::Halted => return HandlerOutcome::Halted,
            HandlerOutcome::TransportFault => return HandlerOutcome::TransportFault,
            HandlerOutcome::Fatal => return HandlerOutcome::Fatal,
        }
    }
    if bad.is_empty() {
        HandlerOutcome::Complete
    } else {
        HandlerOutcome::Remaining
    }
}

#[cfg(test)]
#[path = "section_recover_tests.rs"]
mod tests;
