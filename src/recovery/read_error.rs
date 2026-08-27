//! Single source of truth for what to do when a sector read fails.
//!
//! Pass 1 (`recovery::sweep_internal`, this crate) calls into
//! `handle_read_error` after every failed `read_sectors` — the ONE production
//! call site. The handler classifies the error, updates the in-flight context
//! (counters, damage window, retry budgets), and returns a `ReadAction` the
//! caller dispatches on.
//!
//! Pass N does not route here: its recovery is the time-bounded handler chain
//! in `section_recover.rs`, driven by `patch.rs`. (`ReadCtx::for_patch` and the
//! Pass-N constants below are the relocated Pass-N tuning, exercised by this
//! module's own tests; the named `handle_read_failure` in `disc/patch.rs` that
//! this doc used to point at belongs to a module layout that no longer exists —
//! there is no `src/disc/` in this crate, and no function of that name.)
//!
//! Adding a new error class = add one arm in `handle_read_error`.
//! Adding new logging on errors = one place.

use libfreemkv::error::Error;
use libfreemkv::scsi;
use libfreemkv::scsi::SenseFamily;

/// In-flight bookkeeping a read loop must keep across iterations. The
/// handler reads and mutates this. Caller owns the storage.
pub struct ReadCtx {
    /// Number of sectors per read attempt. Scales the damage-jump
    /// distance, so a jump clears a whole multiple of the read size.
    pub batch: u16,
    /// Successful reads since the last failure. Resets to 0 on failure.
    /// Used by callers to drive damage-zone exit / speed restoration.
    pub consecutive_good: u64,
    /// Failed reads since the last success. Resets to 0 on success.
    /// Drives long-pause escalation on persistent failure.
    pub consecutive_failures: u64,
    /// Failed batch reads since the last success. Drives the
    /// fast-entry damage-jump on Pass 1 (skip the disc-level grind
    /// once we're clearly in a damaged region; Pass N will recover
    /// the actual sectors). Reset on success.
    pub consecutive_outer_failures: u64,
    /// Sliding window of recent read outcomes (true=ok, false=fail).
    /// Capped at `damage_window_max`. Drives damage-jump decisions.
    pub damage_window: Vec<bool>,
    /// Maximum number of outcome entries kept in `damage_window`; the
    /// oldest is evicted once this is exceeded. A whole count (e.g. 16).
    pub damage_window_max: usize,
    /// Fraction of `damage_window` entries that must be failures before
    /// the window-based damage-jump fires, as a whole-number percentage
    /// (e.g. `12` = 12%).
    pub damage_threshold_pct: usize,
    /// Trigger a damage-jump after this many consecutive outer-batch
    /// failures, even when the damage_window isn't full yet. Pass 1
    /// uses a small value (1 — jump on the first outer failure; see
    /// the 2026-05-11 rewrite in `for_sweep`) so we don't spend ~40
    /// minutes grinding to fill a 16-block window before the first jump
    /// on a damage zone we entered cleanly. Pass N uses a larger value
    /// because Pass N's whole job IS to grind on the bad ranges.
    pub fast_jump_threshold: u64,
    /// Multiplier applied to damage-jump distance. Doubles each jump,
    /// resets to 1 after `damage_window_max` consecutive good reads.
    pub jump_multiplier: u64,
    /// NOT_READY retries used so far for the current LBA. Reset to 0
    /// on any non-NOT_READY response.
    pub not_ready_retries: u32,
    /// Bridge-degradation cooldowns used so far.
    pub bridge_degradation_count: u32,
    /// Which pass this context belongs to: `false` = Pass 1 sweep,
    /// `true` = a Pass N patch. It selects the wedge-skip distance
    /// (Pass 1 jumps `WEDGE_JUMP_SECTORS`; Pass N only
    /// `WEDGE_PASS_N_SKIP_SECTORS`, because it is already grinding a
    /// single known-bad range), exempts Pass N from the zone-entry
    /// cooldown (being inside damage is its normal state, not a
    /// transition worth a 30s pause), and labels the wedge logs.
    pub patch_pass: bool,
    /// Count of consecutive firmware-wedge responses (HARDWARE_ERROR
    /// or ILLEGAL_REQUEST sense keys) since the last successful read.
    /// Pass 1 uses this to drive the wedge-skip path: each wedge
    /// triggers a 1 GB jump + cooldown pause. Reaching
    /// `WEDGE_ABORT_THRESHOLD` consecutive wedges with no good read
    /// in between → real AbortPass.
    pub wedge_count: u64,
    // Diagnostic counters (added 2026-05-10): aggregate state for post-mortem
    // analysis, so an operator can tell from the logs whether a wedge was one
    // read at physically-damaged media vs. accumulated firmware-state buildup.
    /// `Instant` of the most recent successful read. Used to compute
    /// "time since last good" for the WARN log on each error. None
    /// before the first successful read.
    pub last_success_at: Option<std::time::Instant>,
    /// `Instant` of the most recent failed read. Used to compute
    /// "time since last error" for the WARN log. None before the
    /// first error.
    pub last_error_at: Option<std::time::Instant>,
    /// Last error's sense-key "family" (Medium / Hardware / IllegalRequest
    /// / NotReady / Other). Used to detect WEDGE TRANSITIONS — when
    /// the family changes from Medium → Hardware/IllegalRequest, the
    /// drive almost certainly just entered fast-fail mode. That
    /// transition gets its own WARN log so the trace is unambiguous.
    pub last_error_family: Option<SenseFamily>,
    /// Sum of all errors observed during this sweep. Reported in the
    /// end-of-pass summary.
    pub total_errors: u64,
    /// Sum of all successful reads during this sweep.
    pub total_reads_ok: u64,
    /// Count of damage zones entered (transitions from clean → in-damage).
    pub zones_entered: u64,
    /// Count of damage-jumps executed during this sweep.
    pub jumps_taken: u64,
    /// True between "first error after a clean period" and "16 consecutive
    /// good reads after the last error in the cluster." Used to count
    /// zone entries and to bound zone_reads accurately.
    pub in_damage_zone: bool,
    /// Count of long-streak pause escalations taken this pass — the
    /// `consecutive_failures >= CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD`
    /// branch of the pause selection.
    ///
    /// The escalation currently resolves to the same number of seconds as
    /// the ordinary inter-error pause (see
    /// `CONSECUTIVE_FAIL_LONG_PAUSE_SECS`), so it has NO effect a caller
    /// could observe from the returned `ReadAction` — deleting the branch
    /// outright changed nothing any test could see. Counting it makes the
    /// policy observable in its own right: the pass summary can say how
    /// often the drive was in a long failure streak, and the branch cannot
    /// be removed without a test noticing.
    pub long_pause_escalations: u64,
    /// Count of RECOVERED ERROR (marginal) reads the drive reported this pass
    /// (surfaced by the PER=1 mode-select at drive-prep). Each is distrusted and
    /// marked NonTrimmed for a Pass N re-read; the count is reported in the
    /// pass summary so an operator can see how much of a "clean" rip was actually
    /// marginal.
    pub marginal_recovered: u64,
}

impl ReadCtx {
    /// Initial context for a Pass 1 sweep. The job is "fast and
    /// accurate, get the most data in the shortest time" — Pass N
    /// is the one that grinds on the bad ranges. So a failed batch
    /// becomes SkipBlock (whole 32-sector blocks marked NonTrimmed
    /// for Pass N to revisit), and
    /// the damage-jump fast-path triggers after just 1 consecutive
    /// outer-batch failure — the user's wedge-prevention principle
    /// (2026-05-11): once the drive returns ANY recoverable error,
    /// retrying the same LBA quickly is what triggers the firmware
    /// fast-fail transition. On the damage-jump and marginal paths Pass 1
    /// jumps immediately rather than grinding the same LBA. Transient errors
    /// (NOT_READY, bridge degradation) are still retried a small bounded
    /// number of times (`NOT_READY_MAX_RETRIES` / `BRIDGE_DEGRADATION_MAX_RETRIES`)
    /// in both passes before falling through to the skip path.
    /// Pass N owns the heavy retries — it gets per-sector timeouts that don't
    /// hammer the firmware the same way.
    pub fn for_sweep(batch: u16) -> Self {
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

    /// Initial context for a Pass 2-N patch. Pass N's whole reason to
    /// exist is to recover sectors Pass 1 skipped, so the fast-jump
    /// threshold is loose: we don't bail too early on a range that
    /// has scattered good sectors mixed in.
    ///
    /// No production caller: Pass N's read effort is owned by the
    /// handler chain in `section_recover.rs`, which does its own read
    /// sizing and its own error classification and never routes
    /// through [`handle_read_error`]. This constructor is kept as the
    /// Pass-N half of this module's tuning table — `for_sweep`'s
    /// values only mean anything next to it, and the wedge and
    /// zone-entry branches below still read `patch_pass`.
    ///
    /// `damage_threshold_pct = 6` is looser than Pass 1 (12%): Pass N triggers
    /// the damage-skip at half Pass 1 density because the patch loop exists to chip
    /// away at bad ranges, so being more eager to skip clustered bad sectors
    /// converges faster on the recoverable good sectors inside a range.
    pub fn for_patch(batch: u16) -> Self {
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
    pub fn on_success(&mut self) {
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
            // (up to 64x) and the next zone's first jump skips recoverable data.
            self.jump_multiplier = 1;
        }
    }

    /// Final per-pass summary suitable for an INFO log at the end of a
    /// Pass 1 `sweep`. Caller renders this to a single structured log line.
    pub fn pass_summary(&self) -> PassSummary {
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
pub struct PassSummary {
    pub total_reads_ok: u64,
    pub total_errors: u64,
    pub zones_entered: u64,
    pub jumps_taken: u64,
    /// Long-streak pause escalations taken — see
    /// [`ReadCtx::long_pause_escalations`].
    pub long_pause_escalations: u64,
    /// RECOVERED ERROR (marginal) reads distrusted and re-queued for Pass N.
    pub marginal_recovered: u64,
}

/// What the caller should do after a read failure. The caller owns the
/// I/O side-effects (sleep, write zeros, advance pos) — the handler
/// only decides which side-effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadAction {
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

// Pause budget constants. Tuned from 2026-05-07 BU40N traces showing
// bridge wedges 524 ms after a 5.4-second internal ECC retry. The
// post-failure pauses give the drive — and the bridge — time to settle.
/// Pause between a failed read and the next read attempt — applied
/// by Pass 1 sweep via `handle_read_error`. (Pass N used to carry a separate
/// `POST_FAILURE_PAUSE_SECS` in the old `disc/patch.rs`; neither the constant
/// nor that module exists any more — see the reframe below, and
/// `section_recover.rs` for the pauses Pass N applies today.)
///
/// 2026-05-11 reframe: a failed read is a failed read, regardless of
/// which pass is running. The prior split (1s for Pass N, 5s for Pass
/// 1 via `PASS_1_FAIL_PAUSE_SECS`) was solving an imaginary cost
/// problem — real damaged-disc cases mark <50 MB NonTrimmed, and the
/// extra 5s/error is single-digit minutes per pass, not hours. The
/// cost of NOT pausing — a drive wedge that aborts the entire
/// multi-pass recovery — is much worse.
///
/// The wedge avoidance principle: error → drive ECC retry (5-10s
/// internal) → return → cooldown pause → next read. Same shape
/// everywhere reads can fail.
const FAIL_PAUSE_SECS: u64 = 5;
/// Long cooldown applied when a damage zone is first entered (the
/// FIRST read failure after a clean run, before the drive has had a
/// chance to cycle in retries that push it toward fast-fail).
///
/// Empirical: a 2026-05-11 wedge incident showed 7 medium
/// errors in 6.5 seconds (~1s per attempt + ~1s pause) push the
/// BU40N's firmware into IllegalRequest fast-fail mode permanently.
/// Once there, only physical eject + reload clears it. Giving the
/// drive 30s of breathing room after the FIRST error in a zone —
/// before we start adding more error counts in the firmware's
/// internal window — prevents the transition.
///
/// Cost on clean discs: zero (first-error path doesn't trigger).
/// Cost on damaged discs: ~30s × N damage zones; on a 5-zone disc
/// that's 2.5 min extra. Trade for never wedging the drive.
pub(crate) const ZONE_ENTRY_COOLDOWN_SECS: u64 = 30;
/// Cooldown when a long streak of failures suggests the drive is
/// stuck in a damage zone and needs MORE breathing room than the
/// standard inter-error pause. Same value as `FAIL_PAUSE_SECS`
/// because empirically 5s is enough; kept as a separate name so the
/// escalation policy is explicit at the call site.
const CONSECUTIVE_FAIL_LONG_PAUSE_SECS: u64 = 5;
const CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD: u64 = 10;
const POST_JUMP_EXTRA_PAUSE_SECS: u64 = 2;
const NOT_READY_PAUSE_SECS: u64 = 3;
const NOT_READY_MAX_RETRIES: u32 = 3;
const BRIDGE_DEGRADATION_PAUSE_SECS: u64 = 15;
const BRIDGE_DEGRADATION_MAX_RETRIES: u32 = 5;

/// Base of the damage-jump distance formula: `jump_sectors =
/// JUMP_BASE_SECTORS × batch × jump_multiplier`. Bumped 2026-05-10
/// from 256 → 1024 (4×) so the first damage-jump at batch=32 covers
/// 64 MB instead of 16 MB. Empirically the BU40N's damage clusters
/// are 100+ MB wide; 16 MB jumps landed inside the cluster and the
/// re-read added to the firmware wedge counter. 64 MB → 128 MB
/// (after one doubling) clears almost any single-cluster damage in
/// 2 jumps.
const JUMP_BASE_SECTORS: u64 = 1024;

// Firmware-wedge skip policy: a damaged drive's firmware can latch into
// returning HARDWARE_ERROR/ILLEGAL_REQUEST for every later read; instead of
// aborting immediately we JumpAhead + cooldown, aborting only after N wedges.

/// One-gigabyte jump (1024 MiB) on each wedge. Big enough to clear
/// almost any single-cluster damage zone we've seen.
const WEDGE_JUMP_SECTORS: u64 = 524_288;
/// Cooldown pause after each wedge. A wedged drive needs a
/// significant cool-down to leave fast-fail; 30 s strikes a balance
/// between giving the drive a chance to recover and not stalling the
/// rip if the drive is permanently stuck.
const WEDGE_PAUSE_SECS: u64 = 30;
/// Bail after this many consecutive wedges with no good read in
/// between. At 1 GB jumps this lets us scan ~16 GB worth of fully
/// wedged area before giving up — generous enough to clear most
/// physical-damage clusters, bounded enough to not loop forever on
/// a permanently bricked drive.
const WEDGE_ABORT_THRESHOLD: u64 = 16;

/// Pass-N wedge-skip distance. Pass N's batch=1 reads target
/// specific NonTrimmed sectors from Pass 1, so a big 1 GB skip
/// would blow past the current NonTrimmed range and abandon many
/// sectors that might still recover. Use a smaller skip just to
/// move past the bricked LBA + a small buffer — the outer patch
/// loop's next iteration picks up the next sector in the same or
/// next range.
const WEDGE_PASS_N_SKIP_SECTORS: u64 = 64;

/// Single source of truth for the Pass-N damage-window threshold.
/// [`ReadCtx::for_patch`] reads this constant for the Pass-N damage-skip
/// threshold.
///
/// 6% means: with a 16-entry sliding window, the damage-skip fires
/// once 1 out of 16 recent reads has failed. Pass 1 uses a 12%
/// threshold via `damage_threshold_pct` on `for_sweep`; Pass N is
/// twice as eager because patch's whole job is to converge on the
/// bad sub-zones inside a NonTrimmed range — a faster trigger
/// produces tighter convergence in fewer iterations.
pub const PATCH_DAMAGE_THRESHOLD_PCT: usize = 6;

/// THE single error-handling entry point. Updates `ctx`, returns the
/// action the caller must apply.
///
/// New error class = add a new arm here. New logging on errors = add
/// it once at the top. New retry policy = adjust the constants. No
/// other read site needs to change.
pub fn handle_read_error(err: &Error, ctx: &mut ReadCtx) -> ReadAction {
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
    if sense_key != scsi::SENSE_KEY_NOT_READY {
        ctx.not_ready_retries = 0;
    }

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

    // 7. Damage-jump: too many failures → skip ahead by an escalating gap,
    //    capped (a saturated multiplier once produced a 56 GB jump), sized to
    //    clear 100+ MB clusters in ~2 jumps via fast-entry (Pass 1) or window (Pass N).
    const MAX_JUMP_MULTIPLIER: u64 = 64;
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

    // 8. Default: zero-fill the failed batch as NonTrimmed and pause
    //    before the next read.
    ReadAction::SkipBlock { pause_secs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::error::Error;
    use libfreemkv::scsi::ScsiSense;

    fn medium_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(2),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x05,
            }),
        }
    }

    fn hardware_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(2),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_HARDWARE_ERROR,
                asc: 0x44,
                ascq: 0x00,
            }),
        }
    }

    fn illegal_request_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(2),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_ILLEGAL_REQUEST,
                asc: 0x24,
                ascq: 0x00,
            }),
        }
    }

    fn recovered_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(2),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_RECOVERED_ERROR,
                asc: 0x17,
                ascq: 0x01,
            }),
        }
    }

    #[test]
    fn recovered_error_skips_block_not_jump_pass_1() {
        // A recovered (marginal) read on Pass 1 must NOT trigger the damage-jump
        // (would nuke good regions); it SkipBlocks for a Pass N re-read instead,
        // counted as marginal_recovered.
        let mut ctx = ReadCtx::for_sweep(32);
        let action = handle_read_error(&recovered_err(), &mut ctx);
        assert!(
            matches!(action, ReadAction::SkipBlock { .. }),
            "recovered error must SkipBlock, not JumpAhead; got {action:?}"
        );
        assert_eq!(ctx.marginal_recovered, 1);
        // It must not have inflated the damage-jump multiplier (not damage signal).
        assert_eq!(ctx.jump_multiplier, 1);
        assert_eq!(ctx.jumps_taken, 0);
    }

    #[test]
    fn recovered_error_does_not_consume_zone_entry() {
        // A recovered (marginal) read must NOT latch in_damage_zone — otherwise a
        // genuine hard error that follows would not be seen as the zone entry and
        // would skip the 30s wedge cooldown.
        let mut ctx = ReadCtx::for_sweep(32);
        handle_read_error(&recovered_err(), &mut ctx);
        assert!(
            !ctx.in_damage_zone,
            "recovered read is not damage-zone signal"
        );
        assert_eq!(ctx.zones_entered, 0);
        // The following genuine hard error IS the real zone entry.
        handle_read_error(&hardware_err(), &mut ctx);
        assert!(ctx.in_damage_zone);
        assert_eq!(ctx.zones_entered, 1, "hard error registers the zone entry");
    }

    /// A NOT_READY retry must not consume the zone-entry transition either.
    ///
    /// The zone entry is what buys the drive the 30 s cooldown, and the whole
    /// point of that constant is the firmware fast-fail wedge. The BU40N's
    /// documented bad-sector signature IS a NOT_READY, so on the discs this
    /// matters most for, the first error spent the transition on a 3 s retry
    /// and the genuine hard error that followed got the ordinary 5 s pause.
    #[test]
    fn a_not_ready_retry_does_not_consume_the_zone_entry() {
        let mut ctx = ReadCtx::for_sweep(32);
        for _ in 0..NOT_READY_MAX_RETRIES {
            let a = handle_read_error(&not_ready_err(), &mut ctx);
            assert!(matches!(a, ReadAction::Retry { .. }), "got {a:?}");
        }
        assert!(
            !ctx.in_damage_zone,
            "a transient NOT_READY retry is not evidence of a damage zone"
        );
        assert_eq!(
            ctx.zones_entered, 0,
            "the zone counter must not tick for retries that never reach the cooldown"
        );

        // The error that DOES reach the pause selection is the real zone
        // entry, and it must get the long cooldown.
        let a = handle_read_error(&hardware_err(), &mut ctx);
        assert!(ctx.in_damage_zone);
        assert_eq!(ctx.zones_entered, 1);
        let pause = match a {
            ReadAction::JumpAhead { pause_secs, .. } => pause_secs,
            ReadAction::SkipBlock { pause_secs } => pause_secs,
            ReadAction::Retry { pause_secs } => pause_secs,
            other => panic!("expected a paused action, got {other:?}"),
        };
        // 30 s is the documented cooldown (2026-05-11 incident: BU40N wedged
        // after 7 errors in 6.5 s). Literal, not `>= the constant` — that would
        // pass even if the constant were 0, reintroducing the wedge.
        assert!(
            pause >= 30,
            "the real zone entry must get the 30 s wedge cooldown, got {pause}s \
             — the NOT_READY retries had eaten the transition"
        );
    }

    /// A transport failure aborts the pass, so it must not spend the
    /// transition on its way out either.
    #[test]
    fn a_transport_failure_does_not_consume_the_zone_entry() {
        let mut ctx = ReadCtx::for_sweep(32);
        assert!(matches!(
            handle_read_error(&transport_failure_err(), &mut ctx),
            ReadAction::AbortPass
        ));
        assert!(!ctx.in_damage_zone);
        assert_eq!(ctx.zones_entered, 0);
    }

    #[test]
    fn recovered_error_skips_block_pass_n_too() {
        // Pass N sees the same: a recovered read is distrusted → SkipBlock (the
        // outer patch loop re-reads the range with FUA).
        let mut ctx = ReadCtx::for_patch(1);
        let action = handle_read_error(&recovered_err(), &mut ctx);
        assert!(
            matches!(action, ReadAction::SkipBlock { .. }),
            "got {action:?}"
        );
        assert_eq!(ctx.marginal_recovered, 1);
    }

    #[test]
    fn many_recovered_errors_never_jump() {
        // Even a run of recovered errors must never escalate to a damage-jump —
        // they're marginal reads, not a hard-damage cluster.
        let mut ctx = ReadCtx::for_sweep(32);
        for _ in 0..40 {
            let a = handle_read_error(&recovered_err(), &mut ctx);
            assert!(matches!(a, ReadAction::SkipBlock { .. }), "got {a:?}");
        }
        assert_eq!(ctx.jumps_taken, 0, "recovered errors never jump");
        assert_eq!(ctx.marginal_recovered, 40);
    }

    #[test]
    fn pass_1_marginal_jumps_immediately() {
        // 2026-05-11 rewrite: Pass 1 jumps on the FIRST marginal error
        // (fast_jump_threshold=1) instead of SkipBlock — retrying the same
        // LBA quickly triggers the BU40N firmware fast-fail; Pass N revisits later.
        let mut ctx = ReadCtx::for_sweep(32);
        let action = handle_read_error(&medium_err(), &mut ctx);
        match action {
            ReadAction::JumpAhead { .. } => {}
            other => panic!("expected JumpAhead on first Pass 1 marginal error, got {other:?}"),
        }
    }

    #[test]
    fn medium_error_with_batch_1_skips() {
        let mut ctx = ReadCtx::for_patch(1);
        let action = handle_read_error(&medium_err(), &mut ctx);
        match action {
            ReadAction::SkipBlock { pause_secs } => assert!(pause_secs >= 1),
            other => panic!("expected SkipBlock, got {other:?}"),
        }
    }

    #[test]
    fn pass_1_jumps_immediately_on_first_outer_failure() {
        // 2026-05-11 rewrite: fast_jump_threshold=1, not 4 — even ONE error
        // triggers a jump since firmware fast-fail is sensitive to retry cadence
        // (the observed wedge hit at 7 errors/6.5s); jumping on #1 avoids the cascade.
        let mut ctx = ReadCtx::for_sweep(32);
        let a = handle_read_error(&medium_err(), &mut ctx);
        assert!(
            matches!(a, ReadAction::JumpAhead { .. }),
            "expected JumpAhead on first outer failure (fast_jump_threshold=1), got {a:?}"
        );
    }

    #[test]
    fn pass_n_does_not_fast_jump() {
        // Pass N's fast-entry trigger must stay OFF (it grinds on bad ranges),
        // however long the run gets. Suppress the window trigger (unsatisfiable
        // density) and run a long streak, since 4 failures never filled the window.
        let mut ctx = ReadCtx::for_patch(32);
        assert_eq!(
            ctx.fast_jump_threshold,
            u64::MAX,
            "Pass N's fast-entry trigger must be disabled outright, not merely \
             set high"
        );
        ctx.damage_threshold_pct = 101;
        for i in 1..=256 {
            let a = handle_read_error(&medium_err(), &mut ctx);
            assert!(
                !matches!(a, ReadAction::JumpAhead { .. }),
                "Pass N must not fast-jump, and did on failure {i}: {a:?}"
            );
        }
        assert_eq!(ctx.jumps_taken, 0, "no jump of any kind should have fired");
        assert_eq!(
            ctx.consecutive_outer_failures, 256,
            "the outer-failure counter must keep climbing — a reset would mean \
             a jump fired and the assertions above were vacuous"
        );
    }

    #[test]
    fn outer_success_resets_consecutive_outer_failures() {
        // `on_success` must clear the outer-failure counter, or scattered
        // failures across clean regions would fire a fast-entry jump. Use a Pass
        // N ctx (fast-jump off, window suppressed) so the counter stays non-zero.
        let mut ctx = ReadCtx::for_patch(1);
        ctx.damage_threshold_pct = 101; // no window jump; nothing else resets it
        for _ in 0..3 {
            handle_read_error(&medium_err(), &mut ctx);
        }
        assert_eq!(
            ctx.consecutive_outer_failures, 3,
            "fixture invalid: the failures must have accumulated, or the reset \
             below is asserted against a counter that was already 0"
        );
        ctx.on_success();
        assert_eq!(
            ctx.consecutive_outer_failures, 0,
            "a successful read ends the outer-failure streak"
        );
    }

    #[test]
    fn pass_1_hardware_error_jumps_ahead_not_aborts() {
        // Pass 1 should JumpAhead with a 1 GB skip + cooldown instead of
        // aborting immediately — the pre-fix behavior killed rips at 48%
        // on damaged discs.
        let mut ctx = ReadCtx::for_sweep(32);
        let action = handle_read_error(&hardware_err(), &mut ctx);
        match action {
            ReadAction::JumpAhead {
                sectors,
                pause_secs,
            } => {
                // Literals, not the constants themselves: comparing a value to
                // the constant that produced it holds for any value, so shrinking
                // the jump or zeroing the cooldown would still pass. 1 GiB = 524_288 sectors.
                assert_eq!(sectors, 1_073_741_824 / 2048);
                assert_eq!(pause_secs, 30);
            }
            other => panic!("expected JumpAhead, got {other:?}"),
        }
        assert_eq!(ctx.wedge_count, 1);
    }

    #[test]
    fn pass_1_hardware_error_aborts_after_threshold() {
        // After WEDGE_ABORT_THRESHOLD consecutive wedges with no good read
        // between them, autorip should see a real AbortPass to surface "drive
        // stuck, power-cycle required" rather than looping forever.
        let mut ctx = ReadCtx::for_sweep(32);
        for i in 0..WEDGE_ABORT_THRESHOLD - 1 {
            let action = handle_read_error(&hardware_err(), &mut ctx);
            assert!(
                matches!(action, ReadAction::JumpAhead { .. }),
                "iter {i}: expected JumpAhead, got {action:?}"
            );
        }
        // The Nth wedge crosses the threshold.
        let action = handle_read_error(&hardware_err(), &mut ctx);
        assert_eq!(action, ReadAction::AbortPass);
    }

    #[test]
    fn pass_1_good_read_resets_wedge_count() {
        // A single successful read between wedges must clear the skip
        // counter, or a disc with scattered bad zones would eventually run
        // out of skip budget even though the drive kept recovering.
        let mut ctx = ReadCtx::for_sweep(32);
        for _ in 0..(WEDGE_ABORT_THRESHOLD - 1) {
            handle_read_error(&hardware_err(), &mut ctx);
        }
        assert_eq!(ctx.wedge_count, WEDGE_ABORT_THRESHOLD - 1);
        ctx.on_success();
        assert_eq!(ctx.wedge_count, 0);
        // After the success, we should still get JumpAhead (not
        // AbortPass) on the next wedge.
        let action = handle_read_error(&hardware_err(), &mut ctx);
        assert!(matches!(action, ReadAction::JumpAhead { .. }));
    }

    #[test]
    fn pass_n_hardware_error_also_skips_not_aborts() {
        // 2026-05-11 reframe: skip+pause+continue applies to Pass N too
        // (previously it AbortPass'd on first wedge, same bug Pass 1 had).
        // Pass N's skip is smaller than Pass 1's 1 GB — over-skipping would abandon its target range.
        let mut ctx = ReadCtx::for_patch(1);
        let action = handle_read_error(&hardware_err(), &mut ctx);
        match action {
            ReadAction::JumpAhead {
                sectors,
                pause_secs,
            } => {
                // Literals, per `pass_1_hardware_error_jumps_ahead_not_aborts`.
                // 64 sectors is "past the bricked LBA + small buffer", deliberately
                // not the 1 GiB Pass-1 jump, which would blow past Pass N's target range.
                assert_eq!(sectors, 64);
                assert_eq!(pause_secs, 30);
            }
            other => panic!("expected JumpAhead, got {other:?}"),
        }
        assert_eq!(ctx.wedge_count, 1);
    }

    #[test]
    fn pass_n_hardware_error_aborts_after_threshold() {
        // Same threshold as Pass 1 — after WEDGE_ABORT_THRESHOLD
        // consecutive wedges with no good read in between, give up.
        let mut ctx = ReadCtx::for_patch(1);
        for _ in 0..WEDGE_ABORT_THRESHOLD - 1 {
            let action = handle_read_error(&hardware_err(), &mut ctx);
            assert!(matches!(action, ReadAction::JumpAhead { .. }));
        }
        let action = handle_read_error(&hardware_err(), &mut ctx);
        assert_eq!(action, ReadAction::AbortPass);
    }

    #[test]
    fn pass_1_illegal_request_also_routes_to_wedge_skip() {
        // ILLEGAL_REQUEST is the other half of the wedge family:
        // drive saying "I won't parse your CDB" after entering the
        // fast-fail state. Same treatment as HARDWARE_ERROR.
        let mut ctx = ReadCtx::for_sweep(32);
        let action = handle_read_error(&illegal_request_err(), &mut ctx);
        assert!(matches!(action, ReadAction::JumpAhead { .. }));
    }

    #[test]
    fn long_failure_streak_extends_pause_on_pass_n() {
        // Pass N keeps the cooldown behaviour: pauses extend after many
        // consecutive failures. Pass 1 pays MORE, not less (it alone takes the
        // 30s zone-entry cooldown); see the two tests named below for both sides.
        let mut ctx = ReadCtx::for_patch(1);
        for _ in 0..15 {
            handle_read_error(&medium_err(), &mut ctx);
        }
        let final_action = handle_read_error(&medium_err(), &mut ctx);
        // Literal 5 s: `>= CONSECUTIVE_FAIL_LONG_PAUSE_SECS` is the constant
        // compared against itself and held for any value, including 0.
        match final_action {
            ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
            ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 5 + 2),
            other => panic!("expected long-pause action, got {other:?}"),
        }
        assert!(
            ctx.long_pause_escalations > 0,
            "a 16-failure streak must have taken the escalation branch"
        );
    }

    #[test]
    fn pass_1_zone_entry_uses_long_cooldown() {
        // Pass 1's FIRST error (zone entry) gets 30s cooldown + 2s post-jump
        // extra, preventing the retry cadence that triggers firmware fast-fail.
        // Literal 32s, not the constants summed (that would pass even at cooldown=0).
        let mut ctx = ReadCtx::for_sweep(32);
        let action = handle_read_error(&medium_err(), &mut ctx);
        match action {
            ReadAction::JumpAhead { pause_secs, .. } => {
                assert_eq!(
                    pause_secs, 32,
                    "first-error pause is the 30 s zone-entry cooldown plus the \
                     2 s post-jump extra"
                );
            }
            other => panic!("expected JumpAhead on first Pass 1 error, got {other:?}"),
        }
        // And the cooldown is a real pause, not merely "some number": it must
        // dwarf the ordinary 5 s inter-error pause, which is the whole reason
        // the constant exists.
        assert_eq!(
            ZONE_ENTRY_COOLDOWN_SECS, 30,
            "the zone-entry cooldown is 30 s — the value the 2026-05-11 wedge \
             incident was tuned against"
        );
    }

    #[test]
    fn pass_1_subsequent_in_zone_errors_skip_long_cooldown() {
        // Regression: fast-jump resets consecutive_outer_failures after each
        // jump, so zone-entry must key off in_damage_zone, not that counter —
        // otherwise every error in a damaged region pays the 30s cooldown.
        let mut ctx = ReadCtx::for_sweep(32);
        // First error: genuine zone entry, gets the long cooldown.
        let first = handle_read_error(&medium_err(), &mut ctx);
        match first {
            // Literal 30 + 2, for the reason given in
            // `pass_1_zone_entry_uses_long_cooldown`.
            ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 32),
            other => panic!("expected JumpAhead on first error, got {other:?}"),
        }
        // We are now still in the damage zone; the jump reset the outer
        // counter. A second error must NOT re-arm the 30 s cooldown.
        assert!(ctx.in_damage_zone);
        let second = handle_read_error(&medium_err(), &mut ctx);
        let pause = match second {
            ReadAction::JumpAhead { pause_secs, .. } => pause_secs,
            ReadAction::SkipBlock { pause_secs } => pause_secs,
            other => panic!("expected pausing action, got {other:?}"),
        };
        assert_ne!(
            pause, 32,
            "subsequent in-zone error must not pay the 30 s zone-entry cooldown"
        );
        assert!(
            pause <= 7,
            "subsequent in-zone pause should be the standard 5 s fail pause \
             (+2 s post-jump), got {pause}"
        );
    }

    #[test]
    fn pass_n_pauses_uniformly_on_failed_read() {
        // Pass N is exempt from the zone-entry long pause — it retries single
        // sectors on already-known-bad LBAs, and a 30s pause per failure would
        // pointlessly slow recovery. Keeps the standard 5s FAIL_PAUSE_SECS.
        let mut ctx = ReadCtx::for_patch(1);
        let action = handle_read_error(&medium_err(), &mut ctx);
        // Literals, like the rest of this module's pause assertions: written
        // against `FAIL_PAUSE_SECS` these held for any value of it, INCLUDING
        // 0 — and 0 is precisely the un-paced hammering that wedged the BU40N.
        match action {
            ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
            ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 5 + 2),
            other => panic!("expected pausing action, got {other:?}"),
        }
    }

    /// The WINDOW trigger, isolated from the fast-entry trigger.
    ///
    /// This test used to build its ctx with `for_sweep`, which sets
    /// `fast_jump_threshold = 1` — so the very first error jumped via the
    /// fast path and the loop broke on iteration 0. `damage_window_max` and
    /// `damage_threshold_pct` were never consulted: replacing the whole
    /// `window_trigger` expression with `false` left this test (and all 36
    /// others in this module) green. The one test named for the damage window
    /// was pinning the path that bypasses it.
    ///
    /// Disable the fast path the way Pass N does (`u64::MAX`) so only the
    /// window can fire, then pin BOTH sides: no jump while the window is
    /// short, a jump on the read that fills it.
    #[test]
    fn damage_window_fills_then_jumps() {
        let mut ctx = ReadCtx::for_sweep(1);
        ctx.fast_jump_threshold = u64::MAX;
        ctx.damage_window_max = 4;
        ctx.damage_threshold_pct = 50;

        // Reads 1-3: the window is not full yet, so the window trigger must
        // NOT fire — a partly-filled window is not evidence of a damage zone.
        for i in 1..=3 {
            let a = handle_read_error(&medium_err(), &mut ctx);
            assert!(
                matches!(a, ReadAction::SkipBlock { .. }),
                "read {i} filled only {}/4 of the window and must skip in \
                 place, got {a:?}",
                ctx.damage_window.len()
            );
        }

        // Read 4 fills the window at 100% bad, which clears the 50% threshold.
        let a = handle_read_error(&medium_err(), &mut ctx);
        assert!(
            matches!(a, ReadAction::JumpAhead { .. }),
            "a full window at 100% bad against a 50% threshold must jump, \
             got {a:?}"
        );
    }

    /// The window threshold is a real comparison, not a formality: a threshold
    /// no density can reach must never jump, however long the failure run.
    /// Without this, `bad_pct >= ctx.damage_threshold_pct` could be inverted
    /// or dropped and only the test above would notice the direction.
    #[test]
    fn an_unreachable_damage_threshold_never_jumps() {
        let mut ctx = ReadCtx::for_sweep(1);
        ctx.fast_jump_threshold = u64::MAX;
        ctx.damage_window_max = 4;
        ctx.damage_threshold_pct = 101; // unsatisfiable: bad_pct maxes at 100
        for i in 1..=12 {
            let a = handle_read_error(&medium_err(), &mut ctx);
            assert!(
                matches!(a, ReadAction::SkipBlock { .. }),
                "read {i}: no density can reach 101%, so the window trigger \
                 must stay silent, got {a:?}"
            );
        }
    }

    #[test]
    fn jump_multiplier_resets_after_damage_zone_exit() {
        // A zone that doubles the multiplier must not carry the inflated
        // value into the next zone — otherwise the next zone's first
        // jump is up to 64x oversized and skips recoverable data.
        let mut ctx = ReadCtx::for_sweep(32);
        // First zone: a few errors push jumps and double the multiplier.
        for _ in 0..4 {
            handle_read_error(&medium_err(), &mut ctx);
        }
        assert!(
            ctx.jump_multiplier > 1,
            "expected the multiplier to inflate inside a damage zone"
        );
        // Exit the zone: damage_window_max consecutive good reads.
        for _ in 0..ctx.damage_window_max {
            ctx.on_success();
        }
        assert!(!ctx.in_damage_zone, "zone should have exited");
        assert_eq!(
            ctx.jump_multiplier, 1,
            "jump_multiplier must reset to 1 on zone exit"
        );
    }

    #[test]
    fn bridge_degradation_count_resets_on_success() {
        // After a good read the bridge recovered; the 15s-cooldown retry
        // budget must be available again instead of staying saturated
        // for the whole pass.
        let mut ctx = ReadCtx::for_patch(1);
        ctx.bridge_degradation_count = BRIDGE_DEGRADATION_MAX_RETRIES;
        ctx.on_success();
        assert_eq!(ctx.bridge_degradation_count, 0);
    }

    #[test]
    fn wedge_abort_threshold_is_reachable() {
        // A permanently wedged drive must reach the abort threshold
        // rather than burning a WEDGE_PAUSE cooldown per read forever.
        let mut ctx = ReadCtx::for_patch(32);
        let mut aborted = false;
        for _ in 0..WEDGE_ABORT_THRESHOLD {
            if matches!(
                handle_read_error(&hardware_err(), &mut ctx),
                ReadAction::AbortPass
            ) {
                aborted = true;
                break;
            }
        }
        assert!(
            aborted,
            "a permanently wedged drive must reach the abort threshold"
        );
    }

    #[test]
    fn on_success_resets_failure_counters_and_pushes_window() {
        let mut ctx = ReadCtx::for_sweep(32);
        for _ in 0..3 {
            handle_read_error(&medium_err(), &mut ctx);
        }
        assert!(ctx.consecutive_failures > 0);
        ctx.on_success();
        assert_eq!(ctx.consecutive_good, 1);
        assert_eq!(ctx.consecutive_failures, 0);
        assert!(*ctx.damage_window.last().unwrap());
    }

    // Additional hardening: retry-budget boundaries, transport-abort
    // precedence, and the bounded-jump invariant — guards against off-by-one
    // retry caps and an unbounded jump multiplier skipping the rest of the disc.

    /// NOT_READY check-condition (status 0x02 so it is NOT classified as
    /// bridge degradation, which keys off non-standard status bytes).
    /// sense_key=2 with a generic ASC routes to the NOT_READY retry path.
    fn not_ready_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_NOT_READY,
                asc: 0x04,
                ascq: 0x00,
            }),
        }
    }

    /// Transport failure: SCSI status 0xFF (bridge crash). CLAUDE.md
    /// "Bad-sector handling": this aborts the copy.
    fn transport_failure_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE),
            sense: None,
        }
    }

    /// Bridge degradation: a non-standard status byte (0x04 - neither
    /// GOOD/CHECK/TRANSPORT) with empty sense, per `Error::is_bridge_degradation`.
    fn bridge_degradation_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(0x04),
            sense: None,
        }
    }

    /// UNIT ATTENTION (sense key 6, ASC 28h "medium may have changed"),
    /// status 0x02 so it is a real CHECK CONDITION, not a transport failure.
    fn unit_attention_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_UNIT_ATTENTION,
                asc: 0x28,
                ascq: 0x00,
            }),
        }
    }

    #[test]
    fn not_ready_retries_capped_at_three_then_falls_through() {
        // CLAUDE.md "Bad-sector handling" mode 1: NOT READY -> pause 3s, retry
        // up to 3x (NOT_READY_MAX_RETRIES), then mark NonTrimmed. The 1st-3rd
        // must Retry, the 4th must fall through to skip.
        let mut ctx = ReadCtx::for_patch(1);
        for i in 0..NOT_READY_MAX_RETRIES {
            let a = handle_read_error(&not_ready_err(), &mut ctx);
            assert!(
                matches!(a, ReadAction::Retry { .. }),
                "NOT_READY attempt {i} should Retry, got {a:?}"
            );
        }
        // Budget exhausted: the next NOT_READY must not Retry.
        let a = handle_read_error(&not_ready_err(), &mut ctx);
        assert!(
            !matches!(a, ReadAction::Retry { .. }),
            "NOT_READY past the retry cap must fall through, got {a:?}"
        );
    }

    #[test]
    fn transport_failure_aborts_on_both_passes() {
        // CLAUDE.md "Bad-sector handling" mode 2: transport failure (bridge
        // crash, status 0xFF) aborts the pass to re-enumerate the bridge — must
        // hold on both passes, not get swallowed into a JumpAhead by wedge-skip.
        let mut ctx = ReadCtx::for_patch(32);
        assert_eq!(
            handle_read_error(&transport_failure_err(), &mut ctx),
            ReadAction::AbortPass
        );
        // And on a fresh Pass 1 context, still AbortPass.
        let mut ctx1 = ReadCtx::for_sweep(32);
        assert_eq!(
            handle_read_error(&transport_failure_err(), &mut ctx1),
            ReadAction::AbortPass
        );
    }

    #[test]
    fn unit_attention_aborts_the_pass_not_treated_as_a_bad_sector() {
        // A mid-read medium change must abort so the outer loop reacquires,
        // not fall through to the bad-sector path (SkipBlock/JumpAhead) and
        // stitch post-change sectors into the ISO. RED before the UA branch.
        let mut ctx = ReadCtx::for_sweep(32);
        assert_eq!(
            handle_read_error(&unit_attention_err(), &mut ctx),
            ReadAction::AbortPass
        );
        // Pass N too: the wedge/jump arms must not swallow it into a JumpAhead.
        let mut ctx_n = ReadCtx::for_patch(32);
        assert_eq!(
            handle_read_error(&unit_attention_err(), &mut ctx_n),
            ReadAction::AbortPass
        );
    }

    #[test]
    fn bridge_degradation_retries_to_budget_then_falls_through() {
        // Bridge-degradation cooldown retry is bounded by
        // BRIDGE_DEGRADATION_MAX_RETRIES (=5): first 5 errors Retry with the
        // long cooldown, the 6th falls through rather than retrying forever.
        let mut ctx = ReadCtx::for_patch(1);
        for i in 0..BRIDGE_DEGRADATION_MAX_RETRIES {
            let a = handle_read_error(&bridge_degradation_err(), &mut ctx);
            match a {
                ReadAction::Retry { pause_secs } => {
                    assert_eq!(
                        pause_secs, BRIDGE_DEGRADATION_PAUSE_SECS,
                        "bridge retry {i} should use the bridge cooldown"
                    );
                }
                other => panic!("bridge degradation attempt {i} should Retry, got {other:?}"),
            }
        }
        let a = handle_read_error(&bridge_degradation_err(), &mut ctx);
        assert!(
            !matches!(a, ReadAction::Retry { .. }),
            "bridge degradation past the retry budget must fall through, got {a:?}"
        );
    }

    /// The documented BU40N bad-sector signature: NOT_READY
    /// (sense_key=2, ASC=0x04, ASCQ=0x3E) delivered as a CHECK CONDITION
    /// (status 0x02). This is the case the old comment on the bridge
    /// branch wrongly claimed `is_bridge_degradation` matched.
    fn not_ready_04_3e_err() -> Error {
        Error::DiscRead {
            sector: 100,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(ScsiSense {
                sense_key: scsi::SENSE_KEY_NOT_READY,
                asc: 0x04,
                ascq: 0x3E,
            }),
        }
    }

    #[test]
    fn not_ready_04_3e_does_not_take_bridge_branch() {
        // Regression guard: the bridge branch keys on the status byte alone,
        // NOT the NOT_READY 04/3E sense — a real 04/3E error arrives as CHECK
        // CONDITION, so it must route to the NOT_READY retry, not bridge cooldown.
        let err = not_ready_04_3e_err();
        assert!(
            !err.is_bridge_degradation(),
            "04/3E arrives as CHECK CONDITION (0x02); it is not bridge degradation"
        );

        let mut ctx = ReadCtx::for_patch(1);
        match handle_read_error(&err, &mut ctx) {
            ReadAction::Retry { pause_secs } => {
                assert_eq!(
                    pause_secs, NOT_READY_PAUSE_SECS,
                    "04/3E must use the generic NOT_READY pause, not the bridge cooldown"
                );
                assert_ne!(
                    pause_secs, BRIDGE_DEGRADATION_PAUSE_SECS,
                    "04/3E must not take the bridge-degradation branch"
                );
                // Confirm it really went through the NOT_READY path.
                assert_eq!(ctx.not_ready_retries, 1);
                assert_eq!(ctx.bridge_degradation_count, 0);
            }
            other => panic!("04/3E should Retry via the NOT_READY path, got {other:?}"),
        }
    }

    #[test]
    fn jump_multiplier_caps_and_jump_distance_stays_bounded() {
        // CLAUDE.md damage-jump: multiplier doubles per jump but is capped at
        // MAX_JUMP_MULTIPLIER=64 (the "4 GiB cap"), so a single jump can never
        // grow unbounded and skip the rest of the disc; verify saturation holds.
        const MAX_JUMP_MULTIPLIER: u64 = 64;
        let batch: u16 = 32;
        let mut ctx = ReadCtx::for_sweep(batch);
        // Small window + 0% threshold so every failure can window-trigger
        // a jump and keep doubling the multiplier toward the cap.
        ctx.damage_window_max = 2;
        ctx.damage_threshold_pct = 0;
        let mut last_jump_sectors = 0u64;
        for _ in 0..40 {
            if let ReadAction::JumpAhead { sectors, .. } =
                handle_read_error(&medium_err(), &mut ctx)
            {
                last_jump_sectors = sectors;
            }
            assert!(
                ctx.jump_multiplier <= MAX_JUMP_MULTIPLIER,
                "jump_multiplier {} exceeded the cap {}",
                ctx.jump_multiplier,
                MAX_JUMP_MULTIPLIER
            );
        }
        // After saturation the jump distance is a LITERAL sector count, not
        // the production expression itself (which would agree with any base).
        // 1024 base × 32 batch × 64 = 2_097_152 sectors = 4 GiB, the documented cap.
        assert_eq!(
            last_jump_sectors, 2_097_152,
            "the saturated jump is 4 GiB (2_097_152 sectors at batch=32)"
        );
    }

    /// The FIRST damage-jump distance, which the base constant alone decides.
    ///
    /// Nothing pinned it: the only test that looked at a jump distance
    /// compared it against the same `JUMP_BASE_SECTORS` expression the
    /// handler evaluates, so doubling the base (64 MB → 128 MB first jump —
    /// exactly the 2026-05-10 retune, in reverse) passed the whole suite.
    /// The distance is the amount of disc a Pass 1 sweep writes off as
    /// NonTrimmed on the first error in a zone, so it is a data-loss-shaped
    /// number and belongs in a literal.
    #[test]
    fn the_first_damage_jump_clears_exactly_64_mib() {
        let mut ctx = ReadCtx::for_sweep(32);
        assert_eq!(ctx.jump_multiplier, 1, "the first jump is un-multiplied");
        match handle_read_error(&medium_err(), &mut ctx) {
            ReadAction::JumpAhead { sectors, .. } => {
                // 1024 (base) × 32 (batch) = 32_768 sectors × 2048 B = 64 MiB.
                assert_eq!(
                    sectors, 32_768,
                    "the first jump at batch=32 clears 64 MiB — the BU40N's \
                     damage clusters are 100+ MB wide and a shorter jump lands \
                     back inside the cluster"
                );
                assert_eq!(sectors * 2048, 67_108_864, "= 64 MiB");
            }
            other => panic!("expected JumpAhead on the first Pass 1 error, got {other:?}"),
        }
        // The second jump doubles it, and no further: the multiplier is the
        // only thing that grows.
        ctx.jump_multiplier = 2;
        match handle_read_error(&medium_err(), &mut ctx) {
            ReadAction::JumpAhead { sectors, .. } => assert_eq!(
                sectors, 65_536,
                "one doubling of the multiplier is 128 MiB, not more"
            ),
            other => panic!("expected JumpAhead, got {other:?}"),
        }
    }

    /// The long-streak pause escalation, which nothing could see.
    ///
    /// `CONSECUTIVE_FAIL_LONG_PAUSE_SECS` is deliberately the same 5 s as
    /// `FAIL_PAUSE_SECS`, so the branch returns a value indistinguishable
    /// from the ordinary path and deleting it outright left all 38 tests
    /// green. `ReadCtx::long_pause_escalations` now records that the branch
    /// was taken, which makes both the branch and its threshold pinnable —
    /// and gives the end-of-pass summary an honest count of how long the
    /// drive spent in a sustained failure streak.
    #[test]
    fn the_long_streak_escalation_fires_at_its_threshold() {
        let mut ctx = ReadCtx::for_patch(1);
        ctx.damage_threshold_pct = 101; // window jumps off; pause path only

        // Failures 1..=9 are ordinary: the standard 5 s pause, no escalation.
        for i in 1..=9 {
            match handle_read_error(&medium_err(), &mut ctx) {
                ReadAction::SkipBlock { pause_secs } => assert_eq!(
                    pause_secs, 5,
                    "failure {i} is below the streak threshold and gets the \
                     ordinary 5 s pause"
                ),
                other => panic!("failure {i}: expected SkipBlock, got {other:?}"),
            }
            assert_eq!(
                ctx.long_pause_escalations, 0,
                "the escalation must not fire before its 10-failure threshold \
                 (fired on failure {i})"
            );
        }

        // The 10th consecutive failure crosses CONSECUTIVE_FAIL_LONG_PAUSE_
        // THRESHOLD and every failure after it stays escalated.
        for i in 10..=13u64 {
            match handle_read_error(&medium_err(), &mut ctx) {
                ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
                other => panic!("failure {i}: expected SkipBlock, got {other:?}"),
            }
            assert_eq!(
                ctx.long_pause_escalations,
                i - 9,
                "failure {i} is inside the streak and must take the escalation"
            );
        }

        // A good read ends the streak, so the next failure is ordinary again.
        ctx.on_success();
        handle_read_error(&medium_err(), &mut ctx);
        assert_eq!(
            ctx.long_pause_escalations, 4,
            "the streak ended at the successful read; the next failure is not \
             an escalation"
        );
        assert_eq!(
            ctx.pass_summary().long_pause_escalations,
            4,
            "the pass summary reports the escalations, or the operator cannot \
             see them"
        );
    }

    /// A zone entry outranks a long streak: the 30 s cooldown is the pause
    /// that prevents the firmware wedge, and a streak must not downgrade it
    /// to 5 s.
    #[test]
    fn the_zone_entry_cooldown_outranks_the_streak_escalation() {
        let mut ctx = ReadCtx::for_sweep(1);
        ctx.fast_jump_threshold = u64::MAX;
        ctx.damage_threshold_pct = 101;
        // Pre-load a long failure streak WITHOUT entering the zone, so both
        // conditions hold on the next error.
        ctx.consecutive_failures = 50;
        match handle_read_error(&medium_err(), &mut ctx) {
            ReadAction::SkipBlock { pause_secs } => assert_eq!(
                pause_secs, 30,
                "the zone-entry cooldown wins over the streak escalation"
            ),
            other => panic!("expected SkipBlock, got {other:?}"),
        }
        assert_eq!(
            ctx.long_pause_escalations, 0,
            "the escalation branch must not also run"
        );
    }
}
