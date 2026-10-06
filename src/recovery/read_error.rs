//! Single source of truth for what to do when a sector read fails.
//!
//! Pass 1 (`recovery::sweep_linked`, this crate) calls into
//! `handle_read_error` after every failed `read_sectors` — the ONE
//! production call site. The handler classifies the error, updates the
//! in-flight context (counters, damage window, retry budgets), and
//! returns a `ReadAction` the caller dispatches on.
//!
//! Pass N does not route here.
//!
//! Adding a new error class = add one arm in `handle_read_error`.

use libfreemkv::error::Error;
use libfreemkv::scsi;
use libfreemkv::scsi::SenseFamily;

/// In-flight bookkeeping a read loop must keep across iterations. The
/// handler reads and mutates this. Caller owns the storage.
pub(crate) struct ReadCtx {
    /// Number of sectors per read attempt. Scales the damage-jump
    /// distance, so a jump clears a whole multiple of the read size.
    pub(crate) batch: u16,
    /// Successful reads since the last failure. Resets to 0 on failure.
    /// Used by callers to drive damage-zone exit / speed restoration.
    pub(crate) consecutive_good: u64,
    /// Failed reads since the last success. Resets to 0 on success.
    /// Drives long-pause escalation on persistent failure.
    pub(crate) consecutive_failures: u64,
    /// Failed batch reads since the last success. Drives the
    /// fast-entry damage-jump on Pass 1 (skip the disc-level grind
    /// once we're clearly in a damaged region; Pass N will recover
    /// the actual sectors). Reset on success.
    pub(crate) consecutive_outer_failures: u64,
    /// Sliding window of recent read outcomes (true=ok, false=fail).
    /// Capped at `damage_window_max`. Drives damage-jump decisions.
    pub(crate) damage_window: Vec<bool>,
    /// Maximum number of outcome entries kept in `damage_window`; the
    /// oldest is evicted once this is exceeded. A whole count (e.g. 16).
    pub(crate) damage_window_max: usize,
    /// Fraction of `damage_window` entries that must be failures before
    /// the window-based damage-jump fires, as a whole-number percentage
    /// (e.g. `12` = 12%).
    pub(crate) damage_threshold_pct: usize,
    /// Trigger a damage-jump after this many consecutive outer-batch
    /// failures, even when the damage_window isn't full yet. Pass 1
    /// uses a small value (1 — jump on the first outer failure; see
    /// the 2026-05-11 rewrite in `for_sweep`) so we don't spend ~40
    /// minutes grinding to fill a 16-block window before the first jump
    /// on a damage zone we entered cleanly. Pass N uses a larger value
    /// because Pass N's whole job IS to grind on the bad ranges.
    pub(crate) fast_jump_threshold: u64,
    /// Multiplier applied to damage-jump distance. Doubles each jump,
    /// resets to 1 after `damage_window_max` consecutive good reads.
    pub(crate) jump_multiplier: u64,
    /// NOT_READY retries used so far for the current LBA. Reset to 0 once the
    /// position moves on (a success, or a NOT_READY past its budget) and on any
    /// other response.
    pub(crate) not_ready_retries: u32,
    /// Bridge-degradation cooldowns used so far.
    pub(crate) bridge_degradation_count: u32,
    /// Which pass this context belongs to: `false` = Pass 1 sweep,
    /// `true` = a Pass N patch (`for_patch`, test-only today: the shipped Pass N
    /// reads through `section_recover`). It selects the wedge-skip distance
    /// (Pass 1 jumps `WEDGE_JUMP_SECTORS`; Pass N only
    /// `WEDGE_PASS_N_SKIP_SECTORS`, because it is already grinding a
    /// single known-bad range), exempts Pass N from the zone-entry
    /// cooldown (being inside damage is its normal state, not a
    /// transition worth a 30s pause), and labels the wedge logs.
    pub(crate) patch_pass: bool,
    /// Count of consecutive firmware-wedge responses (HARDWARE_ERROR
    /// or ILLEGAL_REQUEST sense keys) since the last successful read.
    /// Pass 1 uses this to drive the wedge-skip path: each wedge
    /// triggers a 1 GB jump + cooldown pause. Reaching
    /// `WEDGE_ABORT_THRESHOLD` consecutive wedges with no good read
    /// in between → real AbortPass.
    pub(crate) wedge_count: u64,
    // Diagnostic counters (added 2026-05-10): aggregate state for post-mortem
    // analysis, so an operator can tell from the logs whether a wedge was one
    // read at physically-damaged media vs. accumulated firmware-state buildup.
    /// `Instant` of the most recent successful read. Used to compute
    /// "time since last good" for the WARN log on each error. None
    /// before the first successful read.
    pub(crate) last_success_at: Option<std::time::Instant>,
    /// `Instant` of the most recent failed read. Used to compute
    /// "time since last error" for the WARN log. None before the
    /// first error.
    pub(crate) last_error_at: Option<std::time::Instant>,
    /// Last error's sense-key "family" (Medium / Hardware / IllegalRequest
    /// / NotReady / Other). Used to detect WEDGE TRANSITIONS — when
    /// the family changes from Medium → Hardware/IllegalRequest, the
    /// drive almost certainly just entered fast-fail mode. That
    /// transition gets its own WARN log so the trace is unambiguous.
    pub(crate) last_error_family: Option<SenseFamily>,
    /// Sum of all errors observed during this sweep. Reported in the
    /// end-of-pass summary.
    pub(crate) total_errors: u64,
    /// Sum of all successful reads during this sweep.
    pub(crate) total_reads_ok: u64,
    /// Count of damage zones entered (transitions from clean → in-damage).
    pub(crate) zones_entered: u64,
    /// Count of damage-jumps executed during this sweep.
    pub(crate) jumps_taken: u64,
    /// True between "first error after a clean period" and "16 consecutive
    /// good reads after the last error in the cluster." Used to count
    /// zone entries and to bound zone_reads accurately.
    pub(crate) in_damage_zone: bool,
    /// Count of failures this pass inside a long failure streak — the `consecutive_failures >=
    /// CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD` branch of the pause selection, whose pause is
    /// today the standard one. Reported in the pass summary so an operator can see how often
    /// the drive was in a long failure streak.
    pub(crate) long_pause_escalations: u64,
    /// Count of RECOVERED ERROR (marginal) reads the drive reported this pass
    /// (surfaced by the PER=1 mode-select at drive-prep). Each is distrusted and
    /// marked NonTrimmed for a Pass N re-read; the count is reported in the
    /// pass summary so an operator can see how much of a "clean" rip was actually
    /// marginal.
    pub(crate) marginal_recovered: u64,
}

impl ReadCtx {
    /// Initial context for a Pass 1 sweep: `batch` sectors per read. Tuned for "fast and
    /// accurate" — the damage-jump fast path triggers after just 1 outer-batch failure, so
    /// every failed batch jumps ahead (zero-filled NonTrimmed, left for Pass N to revisit)
    /// rather than grinding the same LBA. Transient errors still get a
    /// small bounded number of retries (`NOT_READY_MAX_RETRIES` /
    /// `BRIDGE_DEGRADATION_MAX_RETRIES`).
    pub(crate) fn for_sweep(batch: u16) -> Self {
        Self {
            batch,
            consecutive_good: 0,
            consecutive_failures: 0,
            consecutive_outer_failures: 0,
            damage_window: Vec::with_capacity(16),
            damage_window_max: 16,
            damage_threshold_pct: 12,
            fast_jump_threshold: 1,
            jump_multiplier: 1,
            not_ready_retries: 0,
            bridge_degradation_count: 0,
            patch_pass: false,
            wedge_count: 0,
            last_success_at: None,
            last_error_at: None,
            last_error_family: None,
            total_errors: 0,
            total_reads_ok: 0,
            zones_entered: 0,
            jumps_taken: 0,
            in_damage_zone: false,
            long_pause_escalations: 0,
            marginal_recovered: 0,
        }
    }

    /// Initial context for a Pass 2-N patch: `batch` sectors per read. The fast-jump threshold
    /// is loose (window-based jump only) since Pass N exists to recover scattered sectors Pass
    /// 1 skipped, and `damage_threshold_pct` uses [`PATCH_DAMAGE_THRESHOLD_PCT`] (6%, tighter
    /// than Pass 1's 12%) to converge faster on bad sub-zones. Tests only: Pass N does not
    /// route here (`section_recover` owns it), so this pins the `patch_pass` branches.
    #[cfg(test)]
    pub(crate) fn for_patch(batch: u16) -> Self {
        Self {
            batch,
            consecutive_good: 0,
            consecutive_failures: 0,
            consecutive_outer_failures: 0,
            damage_window: Vec::with_capacity(16),
            damage_window_max: 16,
            damage_threshold_pct: PATCH_DAMAGE_THRESHOLD_PCT,
            // Pass N is allowed to grind: window-based jump only,
            // matching the historical behaviour for patch passes.
            fast_jump_threshold: u64::MAX,
            jump_multiplier: 1,
            not_ready_retries: 0,
            bridge_degradation_count: 0,
            patch_pass: true,
            wedge_count: 0,
            last_success_at: None,
            last_error_at: None,
            last_error_family: None,
            total_errors: 0,
            total_reads_ok: 0,
            zones_entered: 0,
            jumps_taken: 0,
            in_damage_zone: false,
            long_pause_escalations: 0,
            marginal_recovered: 0,
        }
    }

    /// Caller calls this after every successful read.
    pub(crate) fn on_success(&mut self) {
        self.consecutive_good += 1;
        self.consecutive_failures = 0;
        self.not_ready_retries = 0;
        // Any successful read clears the wedge-skip counter — the
        // drive recovered, so further wedges should reset the skip
        // budget instead of accumulating toward a real abort.
        self.wedge_count = 0;
        // A successful read means the bridge recovered too, so its
        // 15s-cooldown retry budget must be freed — otherwise it saturates
        // permanently after 5 cumulative events and later degradations lose data.
        self.bridge_degradation_count = 0;
        self.consecutive_outer_failures = 0;
        self.damage_window.push(true);
        if self.damage_window.len() > self.damage_window_max {
            self.damage_window.remove(0);
        }
        // Diagnostic state.
        self.total_reads_ok += 1;
        self.last_success_at = Some(std::time::Instant::now());
        // If we were in a damage zone and accumulated enough good
        // reads to exit (damage_window now all-good), the zone is
        // over. Don't reset zones_entered — that's a sweep total.
        if self.in_damage_zone && self.consecutive_good >= self.damage_window_max as u64 {
            self.in_damage_zone = false;
            self.last_error_family = None;
            // Reset the jump multiplier so the NEXT zone starts at the base
            // distance — otherwise it carries over the prior zone's inflation
            // (up to MAX_JUMP_MULTIPLIER) and the next zone's first jump skips recoverable data.
            self.jump_multiplier = 1;
        }
    }

    /// Final per-pass summary suitable for an INFO log at the end of a
    /// Pass 1 `sweep`. Caller renders this to a single structured log line.
    pub(crate) fn pass_summary(&self) -> PassSummary {
        PassSummary {
            total_reads_ok: self.total_reads_ok,
            total_errors: self.total_errors,
            zones_entered: self.zones_entered,
            jumps_taken: self.jumps_taken,
            long_pause_escalations: self.long_pause_escalations,
            marginal_recovered: self.marginal_recovered,
        }
    }
}

/// End-of-pass stats logged at INFO for post-mortem analysis. Lets
/// an operator answer "how damaged is this disc?" from a single log
/// line per pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PassSummary {
    pub(crate) total_reads_ok: u64,
    pub(crate) total_errors: u64,
    pub(crate) zones_entered: u64,
    pub(crate) jumps_taken: u64,
    /// Long-streak pause escalations taken — see
    /// [`ReadCtx::long_pause_escalations`].
    pub(crate) long_pause_escalations: u64,
    /// RECOVERED ERROR (marginal) reads distrusted and re-queued for Pass N.
    pub(crate) marginal_recovered: u64,
}

/// What the caller should do after a read failure. The caller owns the
/// I/O side-effects (sleep, write zeros, advance pos) — the handler
/// only decides which side-effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadAction {
    /// Pause `pause_secs` then retry the same LBA / batch. Used for
    /// transient conditions (NOT_READY, bridge degradation) that the
    /// drive may recover from on its own.
    Retry { pause_secs: u64 },
    /// Mark the failed range NonTrimmed (zero-fill, retry in Pass N+),
    /// then pause `pause_secs` before resuming the next LBA.
    SkipBlock { pause_secs: u64 },
    /// Mark the failed range NonTrimmed AND advance position by
    /// `sectors` (zero-filling the gap as NonTrimmed). Then pause
    /// `pause_secs`. Used when the damage-window threshold is crossed.
    JumpAhead { sectors: u64, pause_secs: u64 },
    /// Unrecoverable at this layer. Caller propagates `Err` up to the
    /// outer pass loop / autorip, which can attempt USB re-enumeration,
    /// drop session, etc.
    AbortPass,
}

// Pause budget constants: give the drive and bridge time to settle after a failed read. Applied
// by Pass 1 sweep; Pass N's pauses live in section_recover.rs today.
const FAIL_PAUSE_SECS: u64 = 5;
// Long cooldown on the FIRST failure after a clean run, before retries can push the drive
// toward firmware fast-fail.
const ZONE_ENTRY_COOLDOWN_SECS: u64 = 30;
// Cooldown for a long failure streak; same value as FAIL_PAUSE_SECS,
// kept as a separate name so the escalation is explicit at call sites.
const CONSECUTIVE_FAIL_LONG_PAUSE_SECS: u64 = 5;
const CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD: u64 = 10;
const POST_JUMP_EXTRA_PAUSE_SECS: u64 = 2;
const NOT_READY_PAUSE_SECS: u64 = 3;
const NOT_READY_MAX_RETRIES: u32 = 3;
const BRIDGE_DEGRADATION_PAUSE_SECS: u64 = 15;
const BRIDGE_DEGRADATION_MAX_RETRIES: u32 = 5;

// Base of the damage-jump formula: jump_sectors = JUMP_BASE_SECTORS * batch * jump_multiplier.
// Sized so the first jump clears a whole damage cluster in ~2 doublings.
const JUMP_BASE_SECTORS: u64 = 1024;
// The jump multiplier's cap (a saturated one once produced a 56 GB jump).
const MAX_JUMP_MULTIPLIER: u64 = 64;

// Firmware-wedge skip policy: a damaged drive's firmware can latch into
// returning HARDWARE_ERROR/ILLEGAL_REQUEST for every later read; instead of
// aborting immediately we JumpAhead + cooldown, aborting only after N wedges.

/// One-gigabyte jump (1024 MiB) on each wedge. Big enough to clear
/// almost any single-cluster damage zone we've seen.
const WEDGE_JUMP_SECTORS: u64 = 524_288;
// Cooldown after each wedge; long enough to give the drive a chance
// to leave fast-fail without stalling forever on a stuck drive.
const WEDGE_PAUSE_SECS: u64 = 30;
// Bail after this many consecutive wedges with no good read between
// them — generous enough for real damage clusters, bounded enough to
// not loop forever on a permanently bricked drive.
const WEDGE_ABORT_THRESHOLD: u64 = 16;

// Pass-N wedge-skip distance: small, unlike WEDGE_JUMP_SECTORS, because
// Pass N targets specific NonTrimmed sectors and a 1 GB skip would blow
// past the current range and abandon recoverable sectors.
const WEDGE_PASS_N_SKIP_SECTORS: u64 = 64;

/// Single source of truth for the Pass-N damage-window threshold.
/// [`ReadCtx::for_patch`] reads this for the damage-skip threshold: with
/// a 16-entry sliding window, 6% fires once 1/16 recent reads failed.
/// Twice as eager as Pass 1's 12% (`damage_threshold_pct` on
/// `for_sweep`), since patch's job is to converge on bad sub-zones.
#[cfg(test)]
pub(crate) const PATCH_DAMAGE_THRESHOLD_PCT: usize = 6;

// Damage classifications on this thread (EK11: an on-arrival side read never lands here).
#[cfg(test)]
thread_local! {
    pub(crate) static CLASSIFIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// THE single error-handling entry point. Updates `ctx`, returns the
/// action the caller must apply.
///
/// New error class = add a new arm here. New logging on errors = add
/// it once at the top. New retry policy = adjust the constants. No
/// other read site needs to change.
pub(crate) fn handle_read_error(err: &Error, ctx: &mut ReadCtx) -> ReadAction {
    // 0. KU §2.4 on-arrival key stop (E7022/E7032): the unit WAS read, so it is not
    //    damage. Checked before any counter moves: no retry, skip, jump or zone entry.
    if super::is_key_stop(err) {
        return ReadAction::AbortPass;
    }
    #[cfg(test)]
    CLASSIFIED.with(|c| c.set(c.get() + 1));
    ctx.consecutive_failures += 1;
    ctx.consecutive_good = 0;
    ctx.consecutive_outer_failures += 1;

    // Diagnostic instrumentation — compute timing context BEFORE
    // mutating the timestamps so the log reflects the gap to the
    // PREVIOUS error / success, not zero.
    let now = std::time::Instant::now();
    let ms_since_last_error = ctx
        .last_error_at
        .map(|t| now.duration_since(t).as_millis() as u64);
    let ms_since_last_success = ctx
        .last_success_at
        .map(|t| now.duration_since(t).as_millis() as u64);

    let current_family = err
        .scsi_sense()
        .map(|s| SenseFamily::from_sense_key(s.sense_key))
        .unwrap_or(SenseFamily::Other);

    let is_recovered =
        err.scsi_sense().map(|s| s.sense_key) == Some(scsi::SENSE_KEY_RECOVERED_ERROR);

    ctx.total_errors += 1;
    ctx.last_error_at = Some(now);

    // Wedge transition: previous error was MEDIUM, this one HARDWARE or
    // ILLEGAL_REQUEST — the moment firmware flips into fast-fail mode.
    // Distinct WARN so logs make it unambiguous when the wedge "started."
    let is_wedge_transition = matches!(ctx.last_error_family, Some(prev) if !prev.is_wedge_family())
        && current_family.is_wedge_family();
    ctx.last_error_family = Some(current_family);

    tracing::warn!(
        target: "freemkv::disc",
        phase = "read_error",
        consecutive_failures = ctx.consecutive_failures,
        consecutive_outer_failures = ctx.consecutive_outer_failures,
        ms_since_last_error,
        ms_since_last_success,
        total_errors = ctx.total_errors,
        total_reads_ok = ctx.total_reads_ok,
        batch = ctx.batch,
        wedge_count = ctx.wedge_count,
        sense_family = ?current_family,
        sense_key = err.scsi_sense().map(|s| s.sense_key),
        asc = err.scsi_sense().map(|s| s.asc),
        ascq = err.scsi_sense().map(|s| s.ascq),
        error = %err,
        "read failed; classifying"
    );

    if is_wedge_transition {
        // NOTE: this is the FIRST escalation into hardware/illegal-request, NOT a
        // confirmed wedge — drives often recover after one such error, so calling
        // it a wedge here over-claims; a genuine wedge is PERSISTENT (see below).
        tracing::warn!(
            target: "freemkv::disc",
            phase = "fastfail_escalation",
            errors_in_zone = ctx.total_errors,
            ms_since_last_success,
            new_family = ?current_family,
            "drive escalated into the fast-fail sense family (was returning recoverable medium \
             errors before this) — often transient; only a PERSISTENT run is a real wedge"
        );
    }

    // 1. Transport failure: bridge crash / USB disconnect. The outer pass loop
    //    re-discovers the sg path / re-opens the drive; inline single-sector
    //    retry here was tried pre-v0.17.0 and observed to make wedges worse.
    if err.is_scsi_transport_failure() {
        return ReadAction::AbortPass;
    }

    // 1b. Medium change (UNIT ATTENTION): media/bus state changed under us, so
    //     resumed LBAs no longer match the mapped image — abort (not retry/skip)
    //     so the outer loop reacquires; kept above NOT_READY/wedge to avoid swallowing.
    if err.scsi_sense().is_some_and(|s| s.is_unit_attention()) {
        tracing::warn!(
            target: "freemkv::disc",
            phase = "medium_change",
            asc = err.scsi_sense().map(|s| s.asc),
            ascq = err.scsi_sense().map(|s| s.ascq),
            "UNIT ATTENTION (media/bus state changed) — aborting pass to reacquire"
        );
        return ReadAction::AbortPass;
    }

    // 2. Bridge degradation: a non-standard SCSI status byte (e.g. 0x04/0x05,
    //    not GOOD/CHECK CONDITION/TRANSPORT FAILURE) with empty sense — the USB
    //    bridge's semi-stuck state before a crash; retries then falls through.
    if err.is_bridge_degradation() && ctx.bridge_degradation_count < BRIDGE_DEGRADATION_MAX_RETRIES
    {
        ctx.bridge_degradation_count += 1;
        return ReadAction::Retry {
            pause_secs: BRIDGE_DEGRADATION_PAUSE_SECS,
        };
    }

    let sense_key = err.scsi_sense().map(|s| s.sense_key).unwrap_or(0);

    // 3. Generic NOT_READY (other ASC codes): drive's mechanical
    //    pickup may be moving. Pause and retry briefly.
    if sense_key == scsi::SENSE_KEY_NOT_READY && ctx.not_ready_retries < NOT_READY_MAX_RETRIES {
        ctx.not_ready_retries += 1;
        return ReadAction::Retry {
            pause_secs: NOT_READY_PAUSE_SECS,
        };
    }
    // Past here the block is skipped or jumped: the next LBA gets its own budget.
    ctx.not_ready_retries = 0;

    // Zone-entry tracking: latch the clean->damaged transition AFTER every
    // early-return branch (transport failure, bridge, NOT_READY), not before —
    // else a transient retry burns it, starving a real error of the 30s cooldown.
    let is_zone_entry_transition = !ctx.in_damage_zone && !is_recovered;
    if is_zone_entry_transition {
        ctx.in_damage_zone = true;
        ctx.zones_entered += 1;
    }

    // 4. Hardware error / illegal request — the firmware-wedge family; same
    //    pacing+skip response both passes. Pass 1 jumps 1 GB, Pass N jumps just
    //    past the sector (not abandoning its target range); both share the abort budget.
    if sense_key == scsi::SENSE_KEY_HARDWARE_ERROR || sense_key == scsi::SENSE_KEY_ILLEGAL_REQUEST {
        // Count every wedge — this is what carries the pass toward
        // WEDGE_ABORT_THRESHOLD instead of burning a 30s cooldown per read
        // forever on a firmware fast-fail state.
        ctx.wedge_count += 1;
        if ctx.wedge_count >= WEDGE_ABORT_THRESHOLD {
            tracing::warn!(
                target: "freemkv::disc",
                phase = "wedge_abort",
                wedge_count = ctx.wedge_count,
                threshold = WEDGE_ABORT_THRESHOLD,
                pass = if ctx.patch_pass { "N" } else { "1" },
                "wedge-skip exhausted — drive appears permanently stuck"
            );
            return ReadAction::AbortPass;
        }
        let jump_sectors = if ctx.patch_pass {
            WEDGE_PASS_N_SKIP_SECTORS
        } else {
            WEDGE_JUMP_SECTORS
        };
        tracing::warn!(
            target: "freemkv::disc",
            phase = "wedge_skip",
            pass = if ctx.patch_pass { "N" } else { "1" },
            wedge_count = ctx.wedge_count,
            jump_sectors,
            pause_secs = WEDGE_PAUSE_SECS,
            "wedge detected — skipping ahead and pausing for drive cooldown"
        );
        ctx.jumps_taken += 1;
        return ReadAction::JumpAhead {
            sectors: jump_sectors,
            pause_secs: WEDGE_PAUSE_SECS,
        };
    }

    // 4b. RECOVERED ERROR — the drive fought for this sector (ECC worked hard),
    //    which can be silently WRONG on marginal media, so distrust it: SkipBlock
    //    (not damage-jump — a single sector, not a cluster) for Pass N re-read.
    if sense_key == scsi::SENSE_KEY_RECOVERED_ERROR {
        ctx.marginal_recovered += 1;
        tracing::warn!(
            target: "freemkv::disc",
            phase = "recovered_error",
            marginal_recovered = ctx.marginal_recovered,
            asc = err.scsi_sense().map(|s| s.asc),
            ascq = err.scsi_sense().map(|s| s.ascq),
            "drive reported a recovered (marginal) read — distrusting; marking NonTrimmed for Pass N re-read"
        );
        return ReadAction::SkipBlock {
            pause_secs: FAIL_PAUSE_SECS,
        };
    }

    // 5. Read failure — record it in the damage window, then decide between
    //    skip-in-place and a damage-jump. Marginal media (MEDIUM_ERROR /
    //    ABORTED_COMMAND) lands here too: SkipBlock now, Pass N revisits later.
    ctx.damage_window.push(false);
    if ctx.damage_window.len() > ctx.damage_window_max {
        ctx.damage_window.remove(0);
    }

    let bad_count = ctx.damage_window.iter().filter(|&&b| !b).count();
    let bad_pct = if ctx.damage_window.is_empty() {
        0
    } else {
        bad_count * 100 / ctx.damage_window.len()
    };

    // Inter-error pause — wedge prevention via pacing. Zone entry (first error
    // after a clean run) gets the long ZONE_ENTRY_COOLDOWN_SECS pause (a
    // 2026-05-11 wedge hit after ~7 errors in 6.5s); others get the standard 5s.
    let is_zone_entry = is_zone_entry_transition && !ctx.patch_pass;
    let pause_secs = if is_zone_entry {
        ZONE_ENTRY_COOLDOWN_SECS
    } else if ctx.consecutive_failures >= CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD {
        ctx.long_pause_escalations += 1;
        CONSECUTIVE_FAIL_LONG_PAUSE_SECS
    } else {
        FAIL_PAUSE_SECS
    };

    // 6. Damage-jump: too many failures → skip ahead by an escalating gap,
    //    capped at MAX_JUMP_MULTIPLIER, sized to clear 100+ MB clusters in ~2
    //    jumps via fast-entry (Pass 1) or window (Pass N).
    let fast_trigger = ctx.consecutive_outer_failures >= ctx.fast_jump_threshold;
    let window_trigger =
        ctx.damage_window.len() >= ctx.damage_window_max && bad_pct >= ctx.damage_threshold_pct;
    if fast_trigger || window_trigger {
        let mult = ctx.jump_multiplier.min(MAX_JUMP_MULTIPLIER);
        let sectors = JUMP_BASE_SECTORS
            .saturating_mul(ctx.batch as u64)
            .saturating_mul(mult);
        ctx.jump_multiplier = (ctx.jump_multiplier.saturating_mul(2)).min(MAX_JUMP_MULTIPLIER);
        // Reset the outer-failure counter so a long damaged region
        // doesn't keep firing fast-jump every read after the initial
        // jump fired. The window-based trigger handles further jumps.
        ctx.consecutive_outer_failures = 0;
        ctx.jumps_taken += 1;
        return ReadAction::JumpAhead {
            sectors,
            pause_secs: pause_secs + POST_JUMP_EXTRA_PAUSE_SECS,
        };
    }

    // 7. Default: zero-fill the failed batch as NonTrimmed and pause
    //    before the next read.
    ReadAction::SkipBlock { pause_secs }
}

#[cfg(test)]
#[path = "read_error_tests.rs"]
mod tests;
