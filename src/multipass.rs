//! The multipass rip STRATEGY: sweep → N patch passes → abort-on-loss gate.
//!
//! [`crate::recovery::copy`] performs ONE dispatch step (sweep, one patch
//! pass, or a terminal result) chosen from mapfile state; this module's loop
//! calls it repeatedly until the disc is clean or progress stalls, then
//! applies the abort-on-loss gate mirroring autorip's `loss_aborts` (hard
//! rule #6): `abort_on_lost_secs == 0` requires a perfect rip, a positive
//! value tolerates that many seconds of loss, and NaN always fails safe.
//!
//! See docs/multipass.md — why this moved out of autorip's `rip_disc`.

use crate::job::Job;
use crate::recovery::mapfile::{MapStats, Mapfile, SectorStatus};
use crate::recovery::{CopyOptions, PatchOptions, SweepOptions};
use crate::run::ProgressBridge;
use crate::sink::{Level, Sink};

/// Milliseconds per second — the byte-loss→time conversion base.
const MILLIS_PER_SEC: f64 = 1000.0;

/// Bytes in one optical sector — the unit damage is scored in.
const SECTOR_BYTES: u64 = 2048;

/// Does the residual loss exceed the tolerance and therefore abort the rip?
///
/// Ported verbatim from autorip. `abort_on_lost_secs == 0` is byte-exact:
/// any lost byte (or an unquantifiable NaN loss) aborts; exactly zero proceeds.
/// A positive threshold switches to the seconds gate (bytes not consulted).
pub fn loss_aborts(lost_bytes: u64, lost_ms: f64, abort_on_lost_secs: u64) -> bool {
    if abort_on_lost_secs == 0 {
        lost_bytes > 0 || lost_ms.is_nan()
    } else {
        should_abort_for_loss(lost_ms, (abort_on_lost_secs as f64) * MILLIS_PER_SEC)
    }
}

/// The seconds-threshold half of the gate: strictly-greater-than aborts, and a
/// NaN (unquantifiable) loss fails safe to abort.
pub fn should_abort_for_loss(lost_ms: f64, abort_threshold_ms: f64) -> bool {
    lost_ms.is_nan() || lost_ms > abort_threshold_ms
}

/// An ISO-image output is a whole-disc backup and always requires 100% (the
/// `abort_on_lost_secs` tolerance is a muxed-output setting). Front-ends that
/// target an ISO pass their configured value through this to force 0.
pub fn effective_abort_secs(is_iso_output: bool, configured: u64) -> u64 {
    if is_iso_output { 0 } else { configured }
}

/// The unreadable byte count that the abort gate scopes to: whole-disc for an
/// ISO deliverable, in-title only for a muxed output (a scratched menu/trailer
/// outside the muxed title does not count for an MKV/M2TS mux). This is the RAW
/// source of truth the `abort_on_lost_secs == 0` ("perfect") gate keys on — no
/// bitrate, no float — so a zero-bitrate title can never hide unreadable loss.
///
/// Ported verbatim from autorip's `abort_lost_bytes`.
pub fn abort_lost_bytes(
    output_is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
) -> u64 {
    if output_is_iso {
        bad_ranges.iter().map(|(_, sz)| *sz).sum::<u64>()
    } else {
        libfreemkv::disc::bytes_bad_in_title(title, bad_ranges)
    }
}

/// True when loss EXISTS but cannot be scoped to the deliverable, so no honest
/// millisecond figure can be produced.
///
/// An mkv-scoped rip measures damage inside the main title's extents; a title
/// with no extents makes that measurement indistinguishable from "clean".
/// Whole-disc (ISO) scope needs no extents, so it is never unscopable.
///
/// Shared so the two loss paths ([`abort_lost_ms`] and the live gate in
/// `multipass_rip_inner`) cannot drift. See docs/multipass.md for why.
pub fn loss_is_unscopable(
    is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
) -> bool {
    !is_iso && title.extents.is_empty() && !bad_ranges.is_empty()
}

/// Milliseconds of playback lost, scoped by [`abort_lost_bytes`] and converted
/// via the title's own bytes/sec bitrate.
///
/// Fails safe to NaN when the loss exists but cannot be measured — see
/// [`loss_is_unscopable`]. NaN aborts under EVERY threshold, including
/// `u64::MAX`, which is a deliberate behaviour change from autorip's
/// `.accept-loss` escape hatch. See docs/multipass.md for the detail.
pub fn abort_lost_ms(
    output_is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
    title_bytes_per_sec: f64,
) -> f64 {
    // UNSCOPABLE title: no extents means `bytes_bad_in_title` returns 0,
    // indistinguishable from "no damage" (the same failed scan zeroes the
    // bitrate). Checked before the zero-bytes return, which would otherwise win.
    if loss_is_unscopable(output_is_iso, title, bad_ranges) {
        return f64::NAN;
    }
    let lost_bytes = abort_lost_bytes(output_is_iso, title, bad_ranges);
    // Genuinely no loss is genuinely zero — NaN here would abort clean rips.
    if lost_bytes == 0 {
        return 0.0;
    }
    // Loss exists but cannot be converted to time. Every other unquantifiable
    // path answers NaN (fail-safe abort); 0.0 would silently accept it. An
    // infinite bitrate must also be rejected (lost_bytes/inf == 0.0 too).
    if !(title_bytes_per_sec.is_finite() && title_bytes_per_sec > 0.0) {
        return f64::NAN;
    }
    lost_bytes as f64 / title_bytes_per_sec * MILLIS_PER_SEC
}

// MULTIPASS STRATEGY DECISIONS — relocated verbatim from autorip's `rip_disc`.
// Pure pass-ordering/convergence/exhaustion/promotion decisions, characterized
// byte-for-byte in autorip's `char_*` tests before the move (behavior-preserving).

/// The pass plan for a rip, derived purely from `max_retries`.
///
/// Pins the loop's pass ordering: multipass (`max_retries > 0`) runs exactly one
/// Pass-1 sweep (disc → ISO) followed by `max_retries` patch passes, and the UI
/// counts `max_retries + 2` total passes (sweep + N patch + mux). Single-pass
/// (`max_retries == 0`) is the direct disc → MKV stream: no sweep pass, no patch
/// passes, no ISO intermediate, and a `total_passes` of 0 (the mux-progress
/// helper falls through to mux-pct passthrough).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassPlan {
    /// True when the rip goes through the ISO intermediate + recovery loop.
    pub multipass: bool,
    /// Number of Pass-1 sweep passes (1 in multipass, 0 in single-pass).
    pub sweep_passes: u8,
    /// Number of patch retry passes (`max_retries` in multipass, 0 otherwise).
    pub patch_passes: u8,
    /// Total passes reported to the UI (sweep + N patch + mux, else 0).
    pub total_passes: u8,
}

pub fn plan_passes(max_retries: u8) -> PassPlan {
    if max_retries > 0 {
        PassPlan {
            multipass: true,
            sweep_passes: 1,
            patch_passes: max_retries,
            // saturating: max_retries is u8, caller clamps to u8::MAX, so `+ 2`
            // overflows above 253 — a dev-mode panic, or a silent wrap to 1 in
            // release, leaving the UI's pass denominator smaller than the pass count.
            total_passes: max_retries.saturating_add(2), // pass 1 + retries + mux
        }
    } else {
        PassPlan {
            multipass: false,
            sweep_passes: 0,
            patch_passes: 0,
            total_passes: 0,
        }
    }
}

/// The mapfile sector statuses that count as "still bad" (not yet recovered)
/// for the muxable-scope convergence check.
///
/// Defined in [`crate::recovery::mapfile`] alongside the enum it describes;
/// re-exported here because this is the public name front-ends already use.
pub use crate::recovery::mapfile::bad_sector_statuses;

/// Scope-aware bad-byte count for the convergence check.
///
/// For ISO output the deliverable is the whole-disc image, so EVERY bad byte
/// counts (menus / trailers / anything outside a title still has to be clean).
/// For MKV/M2TS only bytes inside the muxed title's extents count — bad ranges
/// in deleted scenes / menus / trailers are not going into the output and do not
/// earn retry passes. Same scoping the abort gate uses ([`abort_lost_bytes`]) —
/// this delegates to it rather than duplicating the sum.
pub fn scope_bad_bytes(
    is_iso: bool,
    bad_ranges: &[(u64, u64)],
    title: &libfreemkv::DiscTitle,
) -> u64 {
    abort_lost_bytes(is_iso, title, bad_ranges)
}

/// Loop-top convergence gate: the muxable scope is 100% recovered (nothing left
/// to retry) exactly when its scope-aware bad-byte count is zero.
pub fn scope_converged(mux_scope_bad: u64) -> bool {
    mux_scope_bad == 0
}

/// Loop-bottom exhaustion gate: a patch pass made progress iff it recovered a
/// non-zero number of bytes. `recovered == 0` means no future pass with the same
/// drive state will help, so the loop gives up and muxes on what it has.
pub fn patch_made_progress(recovered: u64) -> bool {
    recovered != 0
}

/// The unified per-pass convergence decision, composed from the two gates the
/// loop applies ([`scope_converged`] at the top of each iteration and
/// [`patch_made_progress`] at the bottom). This is the single canonical
/// multipass strategy fn every front-end shares.
///
/// See docs/multipass.md for how `mux_scope_bad`/`recovered` map to variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchDecision {
    /// Muxable scope fully recovered — stop retrying, proceed to mux.
    Converged,
    /// Last pass recovered nothing — stop retrying, mux on what we have.
    NoProgress,
    /// Keep retrying.
    Continue,
}

pub fn patch_pass_decision(mux_scope_bad: u64, recovered: Option<u64>) -> PatchDecision {
    if scope_converged(mux_scope_bad) {
        PatchDecision::Converged
    } else if matches!(recovered, Some(r) if !patch_made_progress(r)) {
        PatchDecision::NoProgress
    } else {
        PatchDecision::Continue
    }
}

/// [`patch_pass_decision`] when the muxable scope may not have been measurable.
///
/// `None` means the mapfile — the only place the scope can be read from —
/// could not be loaded. The distinction is load-bearing because ZERO and
/// UNKNOWN take opposite branches: zero is `Converged` ("nothing bad left, go
/// mux"), and an unknown scope substituted with any zero-valued fallback
/// therefore ENDS the recovery on the strength of a read that failed. Unknown
/// converges never; the no-progress rule still applies, since "the last pass
/// recovered nothing" is measured from the pass itself, not from the mapfile.
pub fn patch_pass_decision_measured(
    mux_scope_bad: Option<u64>,
    recovered: Option<u64>,
) -> PatchDecision {
    match mux_scope_bad {
        Some(bad) => patch_pass_decision(bad, recovered),
        None if matches!(recovered, Some(r) if !patch_made_progress(r)) => {
            PatchDecision::NoProgress
        }
        None => PatchDecision::Continue,
    }
}

/// The end-of-recovery promotion: after the final patch pass, bytes still in a
/// "maybe" state across every pass are promoted to `Unreadable` (confirmed
/// lost) BEFORE the abort/loss gate reads them. Returns the `(from, to)`
/// statuses the loop applies.
///
/// BOTH maybe-states are promoted (`NonTrimmed` and `NonScraped`), or a
/// surviving maybe-state stays invisible to the abort gate. See
/// docs/multipass.md for the full rationale.
pub fn end_of_recovery_promotion() -> (&'static [SectorStatus], SectorStatus) {
    (
        &[SectorStatus::NonTrimmed, SectorStatus::NonScraped],
        SectorStatus::Unreadable,
    )
}

/// Coarse damage tier from raw counters — the freemkv product judgment
/// (thresholds), relocated from libfreemkv. Returns the engine-owned
/// [`crate::DamageSeverity`] (defined in `outcome.rs`).
pub fn classify_damage(bad_sectors: u64, lost_ms: f64) -> crate::DamageSeverity {
    use crate::DamageSeverity::*;
    if bad_sectors == 0 {
        return Clean;
    }
    // An unquantifiable loss fails SAFE, matching `should_abort_for_loss`: every
    // NaN comparison is false, so without this it fell through both tiers to
    // Cosmetic — badging "Cosmetic" on the rip the abort gate is refusing.
    if lost_ms.is_nan() {
        return Serious;
    }
    if bad_sectors >= 500 || lost_ms >= 30_000.0 {
        return Serious;
    }
    if bad_sectors >= 51 || lost_ms >= 1_000.0 {
        return Moderate;
    }
    Cosmetic
}

// Whether a recovery pass decrypts in place, given the job's `raw` flag.
// Named so the `!job.raw` policy shared by four call sites reads as a
// decision, not a stray `!`. See docs/multipass.md for the rationale.
pub(crate) fn pass_should_decrypt(raw: bool) -> bool {
    !raw
}

// Bad bytes expressed in whole sectors, the unit `classify_damage` scores.
// Rounds down. `retryable_bytes` must be RETRYABLE, never `bytes_pending`
// (which also counts un-attempted `NonTried`) — see docs/multipass.md.
fn bad_sector_count(unreadable_bytes: u64, retryable_bytes: u64) -> u64 {
    unreadable_bytes.saturating_add(retryable_bytes) / SECTOR_BYTES
}

// `bad_sector_count` for the FINAL verdict, taken from the mapfile's own
// split so the caller cannot pick the field that folds in un-attempted
// disc (`bytes_pending`) — see docs/multipass.md.
fn end_of_recovery_bad_sectors(stats: &MapStats) -> u64 {
    bad_sector_count(stats.bytes_unreadable, stats.bytes_retryable)
}

// The retryable-bytes argument for an exit that did NOT finish the sweep.
// A `CopyResult` can't split retryable damage from never-attempted disc, so
// zero is the honest answer — the unreadable count alone is what is KNOWN.
const UNMEASURED_ON_AN_INTERRUPTED_PASS: u64 = 0;

/// How a finished patch pass ends the loop, if it does.
///
/// A pure function over the two flags a `PatchOutcome` carries: `halted` (the
/// user pressed Stop) and `wedged_exit` (a transport fault mid-pass).
/// `halted` wins when both are set — the more specific thing to tell the
/// user. See docs/multipass.md for why `wedged_exit` must not be dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassExit {
    /// Keep going — evaluate the exhaustion gate.
    Continue,
    /// The operator pressed Stop.
    Cancelled,
    /// The transport died mid-pass. The remaining damage is still RETRYABLE.
    Wedged,
}

/// See [`PassExit`].
pub fn pass_exit(halted: bool, wedged_exit: bool) -> PassExit {
    if halted {
        PassExit::Cancelled
    } else if wedged_exit {
        PassExit::Wedged
    } else {
        PassExit::Continue
    }
}

// Severity for a run that stopped early: scored from unreadable bytes ALONE
// (never `NonTried`), with a non-zero pending count denying the Clean badge
// rather than inventing a tier for it. See docs/multipass.md.
fn interrupted_severity(unreadable_bytes: u64, pending_bytes: u64) -> crate::DamageSeverity {
    let measured = classify_damage(
        bad_sector_count(unreadable_bytes, UNMEASURED_ON_AN_INTERRUPTED_PASS),
        0.0,
    );
    if measured == crate::DamageSeverity::Clean && pending_bytes > 0 {
        return crate::DamageSeverity::Cosmetic;
    }
    measured
}

// A recovery is complete only when the abort-on-loss gate did NOT fire and
// the mapfile shows zero unreadable and zero pending bytes. `aborted_for_loss`
// is load-bearing on its own — see docs/multipass.md.
fn recovery_is_complete(aborted_for_loss: bool, unreadable_bytes: u64, pending_bytes: u64) -> bool {
    !aborted_for_loss && unreadable_bytes == 0 && pending_bytes == 0
}

// Milliseconds of main-title playback lost, scaling `main_bad_bytes` by
// `title`'s own size/runtime — NaN when unquantifiable. `title` must be the
// title `main_bad_bytes` was scoped to; see docs/multipass.md for why.
fn main_title_lost_ms(title: &libfreemkv::DiscTitle, main_bad_bytes: u64) -> f64 {
    if main_bad_bytes == 0 {
        return 0.0;
    }
    if title.size_bytes > 0 && title.duration_secs > 0.0 && title.duration_secs.is_finite() {
        main_bad_bytes as f64 / title.size_bytes as f64 * title.duration_secs * MILLIS_PER_SEC
    } else {
        // Loss exists but we can't quantify it (no bitrate) → NaN, which the
        // gate treats as fail-safe abort.
        f64::NAN
    }
}

/// The end-of-recovery loss figure, plus the reason it is unquantifiable when
/// it is. `None` means the number is trustworthy. Pure and separate from
/// [`multipass_rip_inner`] deliberately, so it can be tested without a drive.
///
/// SCOPE — ALWAYS main-title-scoped, whatever the deliverable is: it derives
/// its own byte count from `title` + `bad_ranges` rather than accepting the
/// ABORT GATE's count ([`abort_lost_bytes`]), which is whole-disc for an ISO
/// deliverable. See docs/multipass.md for the incident this guards against.
pub fn end_of_recovery_lost_ms(
    promotion_intact: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
) -> (f64, Option<&'static str>) {
    if !promotion_intact {
        // The damage record itself is incomplete, so nothing derived from it
        // can be trusted.
        return (
            f64::NAN,
            Some(
                "multipass_rip: damage record is incomplete after a failed \
                 promotion — treating loss as unquantifiable",
            ),
        );
    }
    // `loss_is_unscopable`'s `is_iso` answers the ABORT GATE's question ("can the
    // byte count be produced without extents?"). The question HERE never depends
    // on the deliverable: a main-title ms figure always needs title extents.
    const MS_IS_ALWAYS_TITLE_SCOPED: bool = false;
    if loss_is_unscopable(MS_IS_ALWAYS_TITLE_SCOPED, title, bad_ranges) {
        return (
            f64::NAN,
            Some(
                "multipass_rip: the disc reports no title extents, so in-title \
                 loss cannot be measured — treating loss as unquantifiable",
            ),
        );
    }
    let main_bad_bytes = libfreemkv::disc::bytes_bad_in_title(title, bad_ranges);
    (main_title_lost_ms(title, main_bad_bytes), None)
}

/// The result of a multipass run.
#[derive(Clone, Debug)]
pub struct MultipassResult {
    /// Total bytes the drive could never read (0 = perfect).
    pub unreadable_bytes: u64,
    /// Bytes still pending (un-attempted or retryable) when the loop stopped.
    pub pending_bytes: u64,
    /// Good bytes recovered across all passes.
    pub good_bytes: u64,
    /// Main-title playback milliseconds lost (NaN if unquantifiable).
    ///
    /// ALWAYS scoped to the main title's own extents, even on an ISO rip whose
    /// abort gate counts bytes across the whole disc — an unreadable menu or
    /// trailer is not lost feature playback, and reporting it as such once
    /// stamped `Serious` on a movie the drive had read perfectly.
    pub main_lost_ms: f64,
    /// Damage classification from the residual loss.
    pub severity: crate::DamageSeverity,
    /// Number of passes executed (1 sweep + N patch in multipass mode; 1 in
    /// single-pass mode).
    pub passes: u32,
    /// Whether the abort-on-loss gate fired (loss exceeded tolerance after
    /// retries were exhausted).
    pub aborted_for_loss: bool,
    /// Whether the rip was cancelled (halt) mid-pass.
    pub halted: bool,
    /// Whether a pass ended early on a TRANSPORT FAULT — the USB-bridge crash
    /// that `patch` reports as `wedged_exit`.
    ///
    /// Distinct from [`Self::halted`] (the user pressing Stop): a wedged
    /// pass leaves its unreached ranges RETRYABLE, so the end-of-recovery
    /// promotion must not run on them. The front-end's cue to power-cycle
    /// the drive and resume from the mapfile. See docs/multipass.md.
    pub wedged: bool,
    /// True when the disc (or the scoped muxable portion of it, per
    /// [`MultipassOpts::is_iso_output`]) ended with zero unreadable and zero
    /// pending bytes, and the run was neither halted nor aborted for loss.
    pub complete: bool,
}

/// Options controlling a [`multipass_rip`] run.
#[derive(Clone, Copy, Debug)]
pub struct MultipassOpts {
    /// Patch-retry pass cap — autorip's `max_retries` analogue, fed straight
    /// into [`plan_passes`]. `0` selects single-pass mode: one
    /// `recovery::copy` dispatch (sweep-or-resume), no sweep/patch split, no
    /// convergence loop, no abort-on-loss gate.
    pub max_passes: u32,
    /// Seconds of main-title playback loss tolerated once patch retries are
    /// exhausted. `0` requires a perfect rip (any residual loss aborts).
    /// Forced to `0` when `is_iso_output` regardless of the configured value
    /// (see [`effective_abort_secs`]) — an ISO deliverable is a whole-disc
    /// backup and always requires 100%.
    pub abort_on_lost_secs: u64,
    /// True when the deliverable is a whole-disc ISO image. Scopes both the
    /// per-pass convergence check ([`scope_bad_bytes`]) and the end-of-
    /// recovery abort gate's BYTE count ([`abort_lost_bytes`]) to the whole
    /// disc instead of just the muxed title's extents, and forces
    /// `abort_on_lost_secs` to `0` via [`effective_abort_secs`].
    ///
    /// It does NOT widen the MILLISECOND figure — [`MultipassResult::main_lost_ms`]
    /// stays main-title-scoped regardless. See docs/multipass.md for why.
    pub is_iso_output: bool,
}

/// Drive the full multipass STRATEGY LOOP: sweep, then patch passes until the
/// muxable scope is clean, a pass makes no progress, or `opts.max_passes` is
/// reached, then apply the end-of-recovery promotion and the abort-on-loss
/// gate — the shared composition every front-end drives instead of its own
/// copy of the loop.
///
/// `opts.max_passes == 0` takes the single-pass branch: one `recovery::copy`
/// dispatch, no retry loop. Otherwise Pass 1 is a fresh `recovery::sweep`,
/// followed by up to `opts.max_passes` `recovery::patch` passes.
pub fn multipass_rip(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    sink: &dyn Sink,
) -> crate::Result<MultipassResult> {
    // Every recovery primitive sleeps through damage cooldowns, emitting no
    // progress ticks, so `should_cancel` can't be polled while waiting. Run the
    // whole multipass under one halt token so Stop works mid-cooldown too.
    crate::run::with_cancel_watcher(sink, |halt| {
        multipass_rip_inner(disc, reader, iso_path, job, opts, sink, halt)
    })
}

#[allow(clippy::too_many_arguments)]
fn multipass_rip_inner(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    sink: &dyn Sink,
    halt: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> crate::Result<MultipassResult> {
    let plan = plan_passes(opts.max_passes.min(u8::MAX as u32) as u8);

    // Multipass implies raw — gated on the RESOLVED PLAN, not the entry point:
    // `max_passes: 0` takes the single-pass (decrypting) branch and must stay
    // allowed. Enforced here too since `preflight` is advisory and skippable.
    if plan.multipass && !job.raw {
        return Err(crate::run::multipass_requires_raw());
    }
    let empty_title = libfreemkv::DiscTitle::empty();
    let main_title = disc.titles.first().unwrap_or(&empty_title);
    let vid = disc.aacs.as_ref().map(|a| a.volume_id);
    let unit_keys = disc
        .aacs
        .as_ref()
        .map(|a| a.unit_keys.clone())
        .unwrap_or_default();

    if !plan.multipass {
        // Single-pass: one `copy` dispatch (sweep-or-resume via mapfile
        // state), no retry loop, no ISO-multipass semantics, no abort gate —
        // mirrors `RipMode::Single`.
        let bridge = ProgressBridge::new(sink);
        let copy_opts = CopyOptions {
            decrypt: pass_should_decrypt(job.raw),
            multipass: false,
            progress: Some(&bridge),
            halt: Some(halt.clone()),
            vid,
            unit_keys,
            key_fetch: None,
        };
        let cr = crate::recovery::copy(disc, reader, iso_path, &copy_opts)?;
        // Clean is a claim about the DISC, not the plan. `bytes_pending` is safe
        // here (unlike the aggregate `bad_sector_count` forbids) because every
        // un-halted route here has `nontried == 0`, so pending is retryable damage.
        let bad_sectors = bad_sector_count(cr.bytes_unreadable, cr.bytes_pending);
        return Ok(MultipassResult {
            unreadable_bytes: cr.bytes_unreadable,
            pending_bytes: cr.bytes_pending,
            good_bytes: cr.bytes_good,
            // Single-pass never runs the end-of-recovery loss gate, so a flat
            // 0.0 would falsely claim "no playback lost" beside real damage.
            // NaN marks it unquantified — except zero bad sectors, genuinely 0.0.
            main_lost_ms: if bad_sectors == 0 { 0.0 } else { f64::NAN },
            // Severity comes from the SECTOR count, which single-pass knows.
            // NaN would wrongly escalate to Serious via `classify_damage`'s
            // fail-safe (right for the abort gate; single-pass has none).
            severity: if cr.halted {
                interrupted_severity(cr.bytes_unreadable, cr.bytes_pending)
            } else {
                classify_damage(bad_sectors, 0.0)
            },
            passes: 1,
            aborted_for_loss: false,
            halted: cr.halted,
            // Single-pass has no patch stage, so no transport-fault exit to
            // report: `recovery::copy` aborts the pass on a bridge crash
            // rather than continuing past it.
            wedged: false,
            complete: cr.complete,
        });
    }

    // ── Pass 1: the forward sweep. ──
    let mut passes = 0u32;
    let (mut last_good, mut last_unreadable, mut last_pending, mut halted);
    {
        let bridge = ProgressBridge::new(sink);
        let sweep_opts = SweepOptions {
            decrypt: pass_should_decrypt(job.raw),
            resume: false,
            batch_sectors: None,
            skip_on_error: true,
            progress: Some(&bridge),
            halt: Some(halt.clone()),
            vid,
            unit_keys: unit_keys.clone(),
            key_fetch: None,
        };
        let sr = crate::recovery::sweep(disc, reader, iso_path, &sweep_opts)?;
        passes += 1;
        last_good = sr.bytes_good;
        last_unreadable = sr.bytes_unreadable;
        last_pending = sr.bytes_pending;
        halted = sr.halted;
    }

    // ── Pass 2..N: patch passes over the mapfile's bad ranges. ──
    let mapfile_path = disc.mapfile_for(iso_path);
    if !halted {
        for _ in 1..=plan.patch_passes {
            if sink.should_cancel() {
                halted = true;
                break;
            }

            // Loop-top convergence gate: skip remaining passes if the mapfile
            // shows the muxable scope already clean. `None` (mapfile unreadable)
            // must never be conflated with 0 — that used to fake "Converged".
            let mux_scope_bad = match Mapfile::load(&mapfile_path) {
                Ok(map) => {
                    let bad = map.ranges_with(&bad_sector_statuses());
                    Some(scope_bad_bytes(opts.is_iso_output, &bad, main_title))
                }
                Err(e) => {
                    sink.log(
                        Level::Warn,
                        &format!(
                            "multipass_rip: could not read the mapfile to check convergence ({e}) — running the pass"
                        ),
                    );
                    None
                }
            };
            if patch_pass_decision_measured(mux_scope_bad, None) == PatchDecision::Converged {
                sink.log(
                    Level::Info,
                    "multipass_rip: muxable scope 100% recovered — skipping remaining patch passes",
                );
                break;
            }

            let bridge = ProgressBridge::new(sink);
            let patch_opts = PatchOptions::for_patch_pass(
                pass_should_decrypt(job.raw),
                Some(&bridge),
                Some(halt.clone()),
                None,
            );
            let pr = crate::recovery::patch(disc, reader, iso_path, &patch_opts)?;
            passes += 1;
            last_good = pr.bytes_good;
            last_unreadable = pr.bytes_unreadable;
            last_pending = pr.bytes_pending;
            let recovered = pr.bytes_recovered_this_pass;

            let exit = pass_exit(pr.halted, pr.wedged_exit);
            if exit == PassExit::Cancelled {
                halted = true;
                break;
            }
            // A transport fault is NOT an exhausted pass: unreached ranges are
            // still retryable, and falling through would promote them to
            // permanently Unreadable, so a re-run would skip them forever.
            if exit == PassExit::Wedged {
                sink.log(
                    Level::Warn,
                    "multipass_rip: patch pass ended on a transport fault — \
                     the remaining damage is still retryable; power-cycle the \
                     drive and resume from the mapfile",
                );
                return Ok(MultipassResult {
                    unreadable_bytes: last_unreadable,
                    pending_bytes: last_pending,
                    good_bytes: last_good,
                    main_lost_ms: 0.0,
                    severity: interrupted_severity(last_unreadable, last_pending),
                    passes,
                    aborted_for_loss: false,
                    halted: false,
                    wedged: true,
                    complete: false,
                });
            }
            sink.log(
                Level::Info,
                &format!(
                    "multipass_rip: pass {passes} recovered {recovered} bytes; {} bytes still pending",
                    last_pending
                ),
            );
            // Loop-bottom exhaustion gate, evaluated against the SAME pre-pass
            // `mux_scope_bad` the top-of-loop check used: a pass that recovered
            // nothing won't be helped by another pass with the same drive state.
            if patch_pass_decision_measured(mux_scope_bad, Some(recovered))
                == PatchDecision::NoProgress
            {
                sink.log(
                    Level::Info,
                    "multipass_rip: patch pass made no progress — exhausted, muxing on what we have",
                );
                break;
            }
        }
    }

    if halted {
        // Severity comes from damage actually recorded — hard-coding Clean here
        // made a cancelled rip with 300 MB unreadable show a "Clean" badge.
        // main_lost_ms stays 0.0 (uncomputable mid-recovery); `halted` marks it partial.
        return Ok(MultipassResult {
            unreadable_bytes: last_unreadable,
            pending_bytes: last_pending,
            good_bytes: last_good,
            main_lost_ms: 0.0,
            severity: interrupted_severity(last_unreadable, last_pending),
            passes,
            aborted_for_loss: false,
            halted: true,
            wedged: false,
            complete: false,
        });
    }

    // ── End-of-recovery promotion + abort-on-loss gate. ──
    // `bad_sectors` is carried out of the match, not derived after: the Ok
    // branch has the mapfile split; the Err branch has only unsplittable counters.
    let (main_lost_ms, main_lost_bytes, good_bytes, unreadable_bytes, pending_bytes, bad_sectors) =
        match Mapfile::load(&mapfile_path) {
            Ok(mut map) => {
                // Promotion MAKES the loss visible: the abort gate reads only
                // Unreadable ranges, so a range that fails to promote out of
                // NonTrimmed silently drops out — a write error ships as a good rip.
                let mut promotion_intact = true;
                let (promote_from, promote_to) = end_of_recovery_promotion();
                for (pos, size) in map.ranges_with(promote_from) {
                    if let Err(e) = map.record(pos, size, promote_to) {
                        promotion_intact = false;
                        sink.log(
                            Level::Warn,
                            &format!("multipass_rip: end-of-recovery promotion failed: {e}"),
                        );
                    }
                }
                if let Err(e) = map.flush() {
                    promotion_intact = false;
                    sink.log(
                        Level::Warn,
                        &format!("multipass_rip: failed to flush promoted mapfile: {e}"),
                    );
                }
                let stats = map.stats();
                let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
                let lost_bytes = abort_lost_bytes(opts.is_iso_output, main_title, &bad_ranges);
                // Fail-safe: an incomplete damage record makes loss NaN, so
                // `loss_aborts` fires regardless of threshold. Deliberately
                // asymmetric: `lost_bytes` is whole-disc; the ms below stays title-scoped.
                let (lost_ms, unquantifiable) =
                    end_of_recovery_lost_ms(promotion_intact, main_title, &bad_ranges);
                if let Some(why) = unquantifiable {
                    sink.log(Level::Error, why);
                }
                (
                    lost_ms,
                    lost_bytes,
                    stats.bytes_good,
                    stats.bytes_unreadable,
                    stats.bytes_pending,
                    end_of_recovery_bad_sectors(&stats),
                )
            }
            Err(_) => {
                // Fail-safe (mirrors autorip): the mapfile — the rip's only
                // damage record — couldn't be read at the abort-decision point.
                // NaN makes `loss_aborts` fire instead of shipping this as a perfect rip.
                sink.log(
                    Level::Error,
                    "multipass_rip: mapfile could not be loaded to verify loss — forcing abort",
                );
                // No `MapStats` to split, so the score keeps the whole in-flight
                // aggregate deliberately — this fail-safe path must over-report,
                // not under-report (NaN can't escalate a zero-sector Clean verdict).
                (
                    f64::NAN,
                    0,
                    last_good,
                    last_unreadable,
                    last_pending,
                    bad_sector_count(last_unreadable, last_pending),
                )
            }
        };

    let effective_abort = effective_abort_secs(opts.is_iso_output, opts.abort_on_lost_secs);
    let aborted_for_loss = loss_aborts(main_lost_bytes, main_lost_ms, effective_abort);
    let severity = classify_damage(bad_sectors, main_lost_ms);
    let complete = recovery_is_complete(aborted_for_loss, unreadable_bytes, pending_bytes);

    Ok(MultipassResult {
        unreadable_bytes,
        pending_bytes,
        good_bytes,
        main_lost_ms,
        severity,
        passes,
        aborted_for_loss,
        halted: false,
        wedged: false,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every `MultipassResult` field must be listed in `USING_THE_ENGINE.md`'s
    // §4 (the GUI contract), derived from the SOURCE so a new field can't
    // repeat the omission that once hid `wedged`/`complete`. See docs/multipass.md.
    #[test]
    fn every_multipass_result_field_is_documented() {
        let src = include_str!("multipass.rs");
        let guide = include_str!("../USING_THE_ENGINE.md");
        // The struct body: declaration to the closing brace in column 0.
        // `MultipassResult` has no `impl` block to terminate on (unlike
        // `Reason`), and no field/doc line inside contains a brace, so `\n}` is the end.
        let body = src
            .split_once("pub struct MultipassResult {")
            .expect("the file declares MultipassResult")
            .1
            .split_once("\n}")
            .expect("the struct body is closed")
            .0;

        let fields: Vec<&str> = body
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|rest| rest.split_once(':'))
            .map(|(name, _)| name.trim())
            .collect();

        // Fixture checks: a parser that silently extracts nothing (or loses
        // the two fields this test was written for) must fail LOUDLY rather
        // than pass vacuously.
        assert!(
            fields.len() >= 10,
            "fixture check: expected at least the ten known fields, found {fields:?}"
        );
        for expected in ["wedged", "complete", "halted"] {
            assert!(
                fields.contains(&expected),
                "fixture check: the field this test was written for is gone: {fields:?}"
            );
        }

        for field in fields {
            // `mp.<field>` — the notation the guide's own example establishes.
            // Matching the bare word would let "completed" (an unrelated sink
            // method in §1) pass for `complete`.
            assert!(
                guide.contains(&format!("mp.{field}")),
                "MultipassResult field {field:?} is public but not listed in \
                 USING_THE_ENGINE.md — a front-end reading that guide will \
                 never know it exists"
            );
        }
    }

    // ── Abort-gate: ported verbatim from autorip's loss_aborts_zero_threshold
    //    _is_byte_exact so the engine keeps identical semantics. ──
    #[test]
    fn loss_aborts_zero_threshold_is_byte_exact() {
        assert!(
            loss_aborts(1, 0.0, 0),
            "1 lost byte must abort at threshold 0"
        );
        assert!(
            !loss_aborts(0, 12_345.0, 0),
            "0 lost bytes proceeds at threshold 0 even if seconds estimate nonzero"
        );
        assert!(
            loss_aborts(0, f64::NAN, 0),
            "NaN loss fails safe to abort even at threshold 0"
        );
        assert!(
            !loss_aborts(9_999_999, 999.0, 1),
            "999ms under a 1s threshold proceeds (bytes ignored on the seconds path)"
        );
        assert!(
            loss_aborts(0, 1001.0, 1),
            "1001ms over a 1s threshold aborts"
        );
        assert!(
            !loss_aborts(0, 1000.0, 1),
            "exactly 1000ms at a 1s threshold proceeds (strictly greater-than aborts)"
        );
        assert!(
            loss_aborts(0, f64::NAN, 30),
            "NaN loss fails safe to abort on the seconds path too"
        );
    }

    #[test]
    fn effective_abort_secs_forces_iso_to_zero() {
        assert_eq!(
            effective_abort_secs(true, 30),
            0,
            "ISO output requires 100%"
        );
        assert_eq!(
            effective_abort_secs(false, 30),
            30,
            "muxed keeps configured"
        );
        assert_eq!(effective_abort_secs(false, 0), 0);
    }

    // `abort_lost_ms` must never answer "0 ms lost" when loss exists but
    // cannot be measured — 0.0 reads as "within tolerance" and ships a
    // damaged rip as good. Not reachable from today's callers; pins the API.
    #[test]
    fn abort_lost_ms_fails_safe_when_loss_cannot_be_quantified() {
        let mut t = test_title(0, 100);
        t.size_bytes = 0;
        t.duration_secs = 0.0;
        let damage = [(0u64, 4096u64)];

        // Zero bitrate + real in-title loss -> unquantifiable, not zero.
        let ms = abort_lost_ms(false, &t, &damage, 0.0);
        assert!(ms.is_nan(), "zero-bitrate loss must be NaN, got {ms}");
        assert!(
            loss_aborts(abort_lost_bytes(false, &t, &damage), ms, 30),
            "an unquantifiable loss must abort even under a 30s tolerance"
        );

        // The scope hole: a title with NO EXTENTS can't be scoped, so
        // `bytes_bad_in_title` answers 0 — indistinguishable from "clean". A
        // naive `lost_bytes == 0 -> 0.0` guard would wave this through.
        let mut no_extents = libfreemkv::DiscTitle::empty();
        no_extents.size_bytes = 1_000_000;
        no_extents.duration_secs = 100.0;
        assert!(no_extents.extents.is_empty());
        let ms = abort_lost_ms(false, &no_extents, &damage, 8_250_000.0);
        assert!(
            ms.is_nan(),
            "an unscopable title with damage must be NaN, got {ms}"
        );

        // ISO scope is whole-disc, so it never needs extents: still quantified.
        let ms_iso = abort_lost_ms(true, &no_extents, &damage, 8_250_000.0);
        assert!(ms_iso > 0.0 && ms_iso.is_finite(), "iso scope: {ms_iso}");
    }

    /// The other direction: genuinely no loss must stay 0.0, or every clean rip
    /// aborts. This is the guard that makes the NaN above safe to add.
    #[test]
    fn abort_lost_ms_reports_zero_for_a_genuinely_clean_rip() {
        let t = test_title(0, 100);
        assert_eq!(
            abort_lost_ms(false, &t, &[], 0.0),
            0.0,
            "no damage, no bitrate"
        );
        assert_eq!(abort_lost_ms(true, &t, &[], 0.0), 0.0, "iso, no damage");
        // Damage entirely OUTSIDE the title is not this title's loss (the
        // autorip test `mkv_resume_ignores_out_of_title_loss` pins this).
        let outside = [(500_000_000u64, 2048u64)];
        assert_eq!(abort_lost_ms(false, &t, &outside, 8_250_000.0), 0.0);
    }

    // The LIVE abort gate must not answer "0 ms lost" for damage it cannot
    // measure — `multipass_rip_inner`'s hand-rolled pair had a hole where an
    // extents-less title made it return 0.0. See docs/multipass.md.
    #[test]
    fn unmeasurable_in_title_loss_is_never_reported_as_zero() {
        let empty = libfreemkv::DiscTitle::empty();
        let damage = [(0u64, 8192u64)];

        // The exact shape the live gate builds.
        assert!(empty.extents.is_empty());
        assert!(loss_is_unscopable(false, &empty, &damage));

        // And the two functions the live gate actually calls still answer the
        // misleading zero — which is why the gate needs the predicate, not a
        // change to either of them.
        assert_eq!(
            abort_lost_bytes(false, &empty, &damage),
            0,
            "extents-less scoping still answers 0; the guard is what catches it"
        );

        // Now the GATE'S OWN decision function, not the predicate in isolation.
        // An earlier version asserted only `loss_is_unscopable(..)` and stayed
        // green with the guard deleted — the bug was the gate not consulting it.
        let (lost_ms, why) =
            end_of_recovery_lost_ms(/* promotion_intact */ true, &empty, &damage);
        assert!(lost_ms.is_nan(), "gate answered {lost_ms}, not NaN");
        assert!(why.is_some(), "an unquantifiable verdict must say why");
        assert!(
            loss_aborts(0, lost_ms, 30),
            "unmeasurable loss must abort even under a 30s tolerance"
        );
        // What the gate used to answer, pinned so the regression is legible:
        assert!(
            !loss_aborts(0, 0.0, 30),
            "0.0 passes a 30s tolerance — that was the bug"
        );
    }

    /// The gate must still produce a real number when the loss IS measurable —
    /// the guard must not swallow the normal path.
    #[test]
    fn the_gate_still_quantifies_a_measurable_loss() {
        let mut t = test_title(0, 100);
        t.size_bytes = 1_000_000;
        t.duration_secs = 100.0;
        // 100_000 bad bytes, all of them INSIDE the title's 0..100-sector
        // extent, so the gate's scoping and the millisecond scoping agree:
        // 100_000 / 1_000_000 * 100 s = 10 s.
        let (ms, why) = end_of_recovery_lost_ms(true, &t, &[(0, 100_000)]);
        assert!(
            why.is_none(),
            "measurable loss must not be flagged: {why:?}"
        );
        assert!((ms - 10_000.0).abs() < 1e-6, "expected 10s, got {ms}");
    }

    /// A failed promotion still wins over everything else.
    #[test]
    fn the_gate_reports_an_incomplete_damage_record_first() {
        let t = test_title(0, 100);
        let (ms, why) = end_of_recovery_lost_ms(false, &t, &[]);
        assert!(ms.is_nan());
        assert!(why.unwrap().contains("damage record is incomplete"));
    }

    /// ISO scope sums the bad ranges whole-disc and needs no extents, so it is
    /// never unscopable — the guard must not fire there.
    #[test]
    fn iso_scope_is_never_unscopable() {
        let empty = libfreemkv::DiscTitle::empty();
        let damage = [(0u64, 8192u64)];
        assert!(!loss_is_unscopable(true, &empty, &damage));
        assert_eq!(abort_lost_bytes(true, &empty, &damage), 8192);
    }

    /// And the direction that matters most: no damage means the guard cannot
    /// fire, so a clean rip is never turned into an abort.
    #[test]
    fn a_clean_rip_is_never_made_unscopable() {
        let empty = libfreemkv::DiscTitle::empty();
        assert!(!loss_is_unscopable(false, &empty, &[]));
        let t = test_title(0, 100);
        assert!(!loss_is_unscopable(false, &t, &[]));
        assert!(!loss_is_unscopable(false, &t, &[(0, 4096)]));
    }

    // `main_title_lost_ms`'s bitrate guard, pinned in every direction: the
    // mutation run flipped the `&&`/`>` operators here and the suite stayed
    // green. Getting it wrong divides by zero by accident. See docs/multipass.md.
    #[test]
    fn lost_ms_needs_both_a_size_and_a_duration() {
        let damage = 4096u64;

        // Both present -> a real number.
        let mut ok = libfreemkv::DiscTitle::empty();
        ok.size_bytes = 1_000_000;
        ok.duration_secs = 100.0;
        let ms = main_title_lost_ms(&ok, damage);
        assert!(
            ms.is_finite() && ms > 0.0,
            "expected a real figure, got {ms}"
        );

        // Size alone, duration alone, and neither: all unquantifiable. `||`
        // would let the first two through; `true` would let all three.
        let mut size_only = libfreemkv::DiscTitle::empty();
        size_only.size_bytes = 1_000_000;
        let mut dur_only = libfreemkv::DiscTitle::empty();
        dur_only.duration_secs = 100.0;
        for (name, t) in [
            ("size only", size_only),
            ("duration only", dur_only),
            ("neither", libfreemkv::DiscTitle::empty()),
        ] {
            let ms = main_title_lost_ms(&t, damage);
            assert!(ms.is_nan(), "{name}: expected NaN, got {ms}");
        }

        // Exactly zero is NOT usable — `>=` would admit it and divide by zero.
        let mut zero_size = libfreemkv::DiscTitle::empty();
        zero_size.size_bytes = 0;
        zero_size.duration_secs = 100.0;
        assert!(main_title_lost_ms(&zero_size, damage).is_nan());

        let mut zero_dur = libfreemkv::DiscTitle::empty();
        zero_dur.size_bytes = 1_000_000;
        zero_dur.duration_secs = 0.0;
        assert!(main_title_lost_ms(&zero_dur, damage).is_nan());

        // An extent-less / bitrate-less title (the shape a failed scan leaves).
        assert!(main_title_lost_ms(&libfreemkv::DiscTitle::empty(), damage).is_nan());
        // And no damage is genuinely zero regardless of the title.
        assert_eq!(main_title_lost_ms(&libfreemkv::DiscTitle::empty(), 0), 0.0);
    }

    // `abort_lost_ms`'s arithmetic, pinned so the operators cannot drift: the
    // mutation run swapped `/`/`*` in the conversion and nothing failed — a
    // wrong operator here is a wrong abort decision.
    #[test]
    fn abort_lost_ms_converts_bytes_to_milliseconds_exactly() {
        let mut t = test_title(0, 100);
        t.size_bytes = 1_000_000;
        t.duration_secs = 100.0;
        // Whole-disc scope so the figure is the bad-byte sum, not extent-scoped.
        // 2 MB at 1 MB/s = 2 s = 2000 ms.
        let ms = abort_lost_ms(true, &t, &[(0, 2_000_000)], 1_000_000.0);
        assert!((ms - 2_000.0).abs() < 1e-6, "expected 2000 ms, got {ms}");
        // Halving the rate doubles the time — pins the division, not just the
        // magnitude.
        let ms_slow = abort_lost_ms(true, &t, &[(0, 2_000_000)], 500_000.0);
        assert!(
            (ms_slow - 4_000.0).abs() < 1e-6,
            "expected 4000 ms, got {ms_slow}"
        );
        // Doubling the bytes doubles the time — pins the multiplication.
        let ms_more = abort_lost_ms(true, &t, &[(0, 4_000_000)], 1_000_000.0);
        assert!(
            (ms_more - 4_000.0).abs() < 1e-6,
            "expected 4000 ms, got {ms_more}"
        );
    }

    #[test]
    fn classify_damage_tiers() {
        use crate::DamageSeverity::*;
        assert_eq!(classify_damage(0, 0.0), Clean);
        assert_eq!(classify_damage(1, 5.0), Cosmetic);
        assert_eq!(classify_damage(50, 999.0), Cosmetic);
        assert_eq!(classify_damage(51, 0.0), Moderate);
        assert_eq!(classify_damage(10, 1_000.0), Moderate);
        // Both sides of the sector boundary, because the tier doc used to
        // claim 500 for Moderate ("51–500") AND for Serious ("500+").
        assert_eq!(classify_damage(499, 0.0), Moderate);
        assert_eq!(classify_damage(500, 0.0), Serious);
        assert_eq!(classify_damage(10, 30_000.0), Serious);
    }

    #[test]
    fn main_title_lost_ms_scales_by_own_size_and_runtime() {
        let mut t = libfreemkv::DiscTitle::empty();
        t.size_bytes = 1_000_000;
        t.duration_secs = 100.0;
        // 10% of the title bad → 10% of 100s = 10s = 10_000 ms.
        assert!((main_title_lost_ms(&t, 100_000) - 10_000.0).abs() < 1e-6);
        // No loss → 0.
        assert_eq!(main_title_lost_ms(&t, 0), 0.0);
    }

    // `end_of_recovery_lost_ms` must scope BOTH the bad-byte count AND its ms
    // divisor to the passed `title`, never to `disc.titles.first()` — pins
    // the round-2 fix. See docs/multipass.md.
    #[test]
    fn end_of_recovery_lost_ms_scopes_divisor_to_the_passed_title() {
        let mut title = test_title(0, 100);
        title.size_bytes = 1_000_000;
        title.duration_secs = 100.0;
        let (ms, why) = end_of_recovery_lost_ms(true, &title, &[(0, 100_000)]);
        assert!(
            why.is_none(),
            "measurable loss must not be flagged: {why:?}"
        );
        assert!((ms - 10_000.0).abs() < 1e-6, "expected 10s, got {ms}");
    }

    /// Minimal `DiscTitle` whose single extent spans `[start_lba, start_lba +
    /// sector_count)`. Mirrors autorip's `test_title` helper — only `extents`
    /// matters for `bytes_bad_in_title` / the scope-aware gates.
    fn test_title(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            playlist: "00800.mpls".to_string(),
            playlist_id: 800,
            duration_secs: 7200.0,
            size_bytes: (sector_count as u64) * 2048,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![libfreemkv::disc::Extent {
                start_lba,
                sector_count,
            }],
            content_format: libfreemkv::disc::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    // ── Multipass strategy decisions — relocated from autorip's char_* tests.
    //    autorip keeps its own characterization coverage via its call path;
    //    these are the engine's own coverage now that it owns the implementation. ──

    #[test]
    fn plan_passes_single_vs_multipass() {
        let single = plan_passes(0);
        assert!(!single.multipass);
        assert_eq!(single.sweep_passes, 0);
        assert_eq!(single.patch_passes, 0);
        assert_eq!(single.total_passes, 0);

        for n in 1u8..=10 {
            let plan = plan_passes(n);
            assert!(plan.multipass);
            assert_eq!(plan.sweep_passes, 1);
            assert_eq!(plan.patch_passes, n);
            assert_eq!(plan.total_passes, n + 2);
        }
    }

    #[test]
    fn scope_bad_bytes_mkv_scopes_to_title() {
        let title = test_title(0, 48_829);

        let out_of_title = [(500_000_000u64, 2048u64)];
        let bad = scope_bad_bytes(false, &out_of_title, &title);
        assert_eq!(bad, 0);
        assert!(scope_converged(bad));

        let in_title = [(1_000_000u64, 2048u64)];
        let bad_in = scope_bad_bytes(false, &in_title, &title);
        assert_eq!(bad_in, 2048);
        assert!(!scope_converged(bad_in));
    }

    #[test]
    fn scope_bad_bytes_iso_scopes_whole_disc() {
        let title = test_title(0, 48_829);

        let out_of_title = [(500_000_000u64, 2048u64)];
        let bad = scope_bad_bytes(true, &out_of_title, &title);
        assert_eq!(bad, 2048);
        assert!(!scope_converged(bad));

        assert!(scope_converged(scope_bad_bytes(true, &[], &title)));
    }

    #[test]
    fn patch_made_progress_zero_vs_nonzero() {
        assert!(!patch_made_progress(0));
        assert!(patch_made_progress(1));
        assert!(patch_made_progress(2048));
    }

    #[test]
    fn patch_pass_decision_matrix() {
        assert_eq!(patch_pass_decision(0, None), PatchDecision::Converged);
        assert_eq!(patch_pass_decision(0, Some(0)), PatchDecision::Converged);
        assert_eq!(patch_pass_decision(0, Some(999)), PatchDecision::Converged);
        assert_eq!(patch_pass_decision(2048, None), PatchDecision::Continue);
        assert_eq!(
            patch_pass_decision(2048, Some(0)),
            PatchDecision::NoProgress
        );
        assert_eq!(
            patch_pass_decision(2048, Some(4096)),
            PatchDecision::Continue
        );
    }

    // An unquantifiable loss must classify as SERIOUS, not Cosmetic: every
    // NaN comparison in Rust is false, so both tier tests used to fall
    // through while `loss_aborts` was simultaneously refusing to deliver it.
    #[test]
    fn an_unquantifiable_loss_is_serious_not_cosmetic() {
        // Few enough bad sectors that every sector-based tier is false, so the
        // verdict rests entirely on the NaN.
        assert_eq!(
            classify_damage(10, f64::NAN),
            crate::DamageSeverity::Serious,
            "NaN must fail safe here exactly as it does in should_abort_for_loss"
        );
        // And a quantified small loss still classifies normally.
        assert_eq!(classify_damage(10, 0.0), crate::DamageSeverity::Cosmetic);
    }

    // The pass count must not overflow at the top of the u8 range: 255 is
    // reachable via `.min(u8::MAX as u32)`, and `max_retries + 2` used to
    // panic in dev / wrap to 1 in release. See docs/multipass.md.
    #[test]
    fn the_pass_count_saturates_instead_of_wrapping() {
        let plan = plan_passes(u8::MAX);
        assert_eq!(plan.patch_passes, u8::MAX);
        assert_eq!(
            plan.total_passes,
            u8::MAX,
            "total_passes wrapped: in release this silently becomes 1"
        );
        // (No `total_passes >= patch_passes` check: both operands are pinned
        // to `u8::MAX`, so it could only read `255 >= 255` — an assertion
        // that can't fail is noise for a test about overflow.) Ordinary case unchanged.
        assert_eq!(plan_passes(5).total_passes, 7);
    }

    // ── A cancelled or wedged pass has not MEASURED the disc ──────────────
    // `bytes_pending` counts NonTried — the un-attempted remainder ahead of the
    // sweep head — folding it into the damage score made an interruption look catastrophic.

    /// A rip cancelled seconds into a 66 GB disc has found nothing bad. It
    /// must not be scored as though the whole disc were unreadable.
    #[test]
    fn an_early_cancel_is_not_scored_as_a_destroyed_disc() {
        const DISC: u64 = 66_000_000_000;
        // Nothing unreadable; essentially the whole disc still un-attempted.
        assert_eq!(
            interrupted_severity(0, DISC),
            crate::DamageSeverity::Cosmetic,
            "an immediate cancel must not claim damage nobody measured — but \
             it must not read Clean beside a pending count either"
        );
        // The number the old aggregate handed to classify_damage, for contrast.
        assert!(
            bad_sector_count(0, DISC) > 30_000_000,
            "this is what used to be scored: the entire un-read disc"
        );
        assert_eq!(
            classify_damage(bad_sector_count(0, DISC), 0.0),
            crate::DamageSeverity::Serious,
            "and it stamped Serious on a disc nobody had looked at"
        );
    }

    /// Damage that WAS found still scores. A cancel is not an amnesty:
    /// 300 MB of unreadable sectors on the record must reach a real tier, not
    /// merely the not-Clean floor.
    #[test]
    fn a_cancel_does_not_erase_damage_already_found() {
        assert_eq!(
            interrupted_severity(300 * 1024 * 1024, 0),
            crate::DamageSeverity::Serious,
            "unreadable bytes are KNOWN damage and must still score in full"
        );
    }

    /// Nothing outstanding and nothing bad really is Clean — the floor must
    /// not deny a badge that was earned.
    #[test]
    fn an_interrupted_run_with_nothing_outstanding_is_still_clean() {
        assert_eq!(interrupted_severity(0, 0), crate::DamageSeverity::Clean);
    }

    /// The wedge branch itself. It needs a live USB-bridge crash to reach in
    /// place, which is why the decision was extracted: `wedged_exit` was not
    /// read at all, and nothing in the suite could have said so.
    #[test]
    fn a_transport_fault_ends_the_pass_instead_of_exhausting_the_recovery() {
        assert_eq!(pass_exit(false, false), PassExit::Continue);
        assert_eq!(
            pass_exit(false, true),
            PassExit::Wedged,
            "a wedged pass must end the loop — falling through to the \
             exhaustion gate promotes its never-attempted ranges to \
             permanently Unreadable, and a re-run then skips them forever"
        );
        assert_eq!(
            pass_exit(true, false),
            PassExit::Cancelled,
            "a cancel is the operator's, and is reported as such"
        );
        assert_eq!(
            pass_exit(true, true),
            PassExit::Cancelled,
            "both at once is the user's Stop: the more specific thing to say"
        );
    }

    // A transport fault is not an exhausted pass, and must not be reported as
    // a cancel either — drives a real `multipass_rip` end to end, since the
    // old hand-built-result version tested nothing. See docs/multipass.md.
    #[test]
    fn a_wedged_result_is_distinguishable_from_a_cancelled_one() {
        // Marginal (RECOVERED) errors at `bad_lba` while the sweep walks
        // forward, then a TRANSPORT FAILURE (status 0xFF) once the sweep
        // reaches the end and the patch pass returns for the leftover range.
        struct WedgeOnPatchReader {
            capacity: u32,
            bad_lba: u32,
            sweep_done: bool,
        }
        impl libfreemkv::SectorSource for WedgeOnPatchReader {
            fn read_sectors(
                &mut self,
                lba: u32,
                count: u16,
                buf: &mut [u8],
                _recovery: bool,
            ) -> libfreemkv::Result<usize> {
                let end = lba + count as u32;
                if end >= self.capacity {
                    self.sweep_done = true;
                }
                if lba <= self.bad_lba && self.bad_lba < end {
                    return Err(libfreemkv::Error::DiscRead {
                        sector: self.bad_lba as u64,
                        status: Some(if self.sweep_done {
                            libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE
                        } else {
                            libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION
                        }),
                        sense: if self.sweep_done {
                            None
                        } else {
                            // RECOVERED (marginal): the sweep marks NonTrimmed
                            // without the 30s damage-zone cooldown a hard error
                            // would earn — the pass this test is about is the patch one.
                            Some(libfreemkv::scsi::ScsiSense {
                                sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                                asc: 0x17,
                                ascq: 0x01,
                            })
                        },
                    });
                }
                let n = ((count as usize) * 2048).min(buf.len());
                buf[..n].fill(0);
                // BYTES, per `SectorSource::read_sectors`' contract.
                Ok(n)
            }
            fn capacity_sectors(&self) -> u32 {
                self.capacity
            }
        }

        let (dir, iso) = scratch_iso("wedged-patch-pass");
        let sectors = 8192u32;
        let disc = test_disc(sectors, vec![test_title(0, sectors)]);
        let mut reader = WedgeOnPatchReader {
            capacity: sectors,
            bad_lba: 4000,
            sweep_done: false,
        };
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };
        // Logs only; nothing cancels, so `halted` can only come from the code
        // under test.
        let sink = HookSink::new("never-logged-trigger", false, Box::new(|| {}));
        let result = multipass_rip(&disc, &mut reader, &iso, &raw_job(&iso), &opts, &sink)
            .expect("a wedged pass is a partial result, not an Err");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            result.wedged,
            "a patch pass killed by a bridge crash must be reported as wedged"
        );
        assert!(
            !result.halted,
            "nobody pressed Stop — reporting a wedge as a cancel tells the \
             operator to do the wrong thing"
        );
        assert!(
            !result.complete,
            "a wedged pass left retryable damage behind, so the run is not done"
        );
        assert!(
            result.pending_bytes > 0,
            "the ranges the crashed pass never reached must still be pending, \
             not promoted to permanently Unreadable — a re-run has to retry them"
        );
        assert_eq!(
            result.unreadable_bytes, 0,
            "nothing was CONFIRMED lost: the end-of-recovery promotion must not \
             have run on the wedged exit"
        );
        assert!(
            sink.logged(Level::Warn, "transport fault"),
            "the operator has to be told the drive needs a power-cycle"
        );
    }

    #[test]
    fn end_of_recovery_promotion_covers_every_maybe_state() {
        let (from, to) = end_of_recovery_promotion();
        assert_eq!(to, SectorStatus::Unreadable);
        assert!(from.contains(&SectorStatus::NonTrimmed));
        assert!(
            from.contains(&SectorStatus::NonScraped),
            "NonScraped survives every patch pass as a failed read; if it is \
             not promoted it never reaches the abort gate, which reads only \
             Unreadable, and the loss is delivered as a clean rip"
        );
        assert!(!from.contains(&SectorStatus::Finished));
        assert!(!from.contains(&SectorStatus::NonTried));

        // The promotion source set must stay a subset of what the rest of the
        // module already calls damage, or the two rules drift apart.
        let damage = crate::recovery::mapfile::damage_sector_statuses();
        for st in from {
            assert!(
                damage.contains(st),
                "{st:?} promoted but not counted as damage"
            );
        }

        let bad_set = bad_sector_statuses();
        assert!(bad_set.contains(&SectorStatus::NonTrimmed));
        assert!(bad_set.contains(&SectorStatus::Unreadable));
        assert!(!bad_set.contains(&SectorStatus::Finished));
    }

    #[test]
    fn main_title_lost_ms_is_nan_when_unquantifiable() {
        // A title with loss but no measurable bitrate (size 0, dur 0) → NaN
        // (fail-safe abort).
        assert!(main_title_lost_ms(&libfreemkv::DiscTitle::empty(), 4096).is_nan());
    }

    // ── multipass_rip strategy LOOP, exercised headlessly (hard rule #2) ──
    // Every double must honour the contract: `read_sectors` returns BYTES
    // written, not sectors. See docs/multipass.md.
    #[test]
    fn the_doubles_return_a_byte_count_like_the_trait_says() {
        use libfreemkv::SectorSource as _;
        let mut buf = vec![0u8; 4 * 2048];
        let mut zero = ZeroReader { capacity: 64 };
        assert_eq!(
            zero.read_sectors(0, 4, &mut buf, false).unwrap(),
            8192,
            "4 sectors is 8192 BYTES"
        );
        let mut spots = MultiSpotReader {
            capacity: 64,
            spots: vec![],
        };
        assert_eq!(spots.read_sectors(0, 4, &mut buf, false).unwrap(), 8192);
    }

    /// A `SectorSource` whose entire capacity reads back as zeros — the
    /// clean-disc path. Mirrors `run.rs`'s private `ZeroReader`.
    struct ZeroReader {
        capacity: u32,
    }
    impl libfreemkv::SectorSource for ZeroReader {
        fn read_sectors(
            &mut self,
            _lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::Result<usize> {
            let n = ((count as usize) * 2048).min(buf.len());
            buf[..n].fill(0);
            // BYTES, per `SectorSource::read_sectors`' contract — not `count`.
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.capacity
        }
    }

    // One deliberately-bad single-sector LBA: fails every read overlapping it
    // until touched `heal_after` times, then reads clean forever.
    // `heal_after: u32::MAX` never heals — permanent loss. See docs/multipass.md.
    struct Spot {
        lba: u32,
        heal_after: u32,
        attempts: u32,
    }

    /// A `SectorSource` that is clean everywhere except a fixed set of
    /// [`Spot`]s.
    struct MultiSpotReader {
        capacity: u32,
        spots: Vec<Spot>,
    }
    impl libfreemkv::SectorSource for MultiSpotReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::Result<usize> {
            let end = lba + count as u32;
            for spot in &mut self.spots {
                if lba <= spot.lba && spot.lba < end {
                    spot.attempts += 1;
                    if spot.attempts <= spot.heal_after {
                        return Err(libfreemkv::Error::DiscRead {
                            sector: spot.lba as u64,
                            status: Some(2),
                            sense: Some(libfreemkv::scsi::ScsiSense {
                                sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                                asc: 0x17,
                                ascq: 0x01,
                            }),
                        });
                    }
                }
            }
            let n = ((count as usize) * 2048).min(buf.len());
            buf[..n].fill(0);
            // BYTES, per `SectorSource::read_sectors`' contract — not `count`.
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.capacity
        }
    }

    /// A minimal unencrypted `sectors`-sized disc with the given titles (may
    /// be empty — several tests don't need a title at all).
    fn test_disc(sectors: u32, titles: Vec<libfreemkv::DiscTitle>) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "TESTDISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: sectors,
            capacity_bytes: sectors as u64 * 2048,
            layers: 1,
            titles,
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    /// A fresh scratch dir + `out.iso` path for one test, so parallel test
    /// threads never collide on the same mapfile.
    fn scratch_iso(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "fmkv-engine-multipass-rip-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("out.iso");
        (dir, iso)
    }

    // The only single-pass test ran a CLEAN disc, where `main_lost_ms: 0.0`
    // is indistinguishable from a hard-coded constant. On a DAMAGED disc a
    // constant claims loss nothing measured. See docs/multipass.md.
    #[test]
    fn single_pass_reports_loss_as_unquantified_on_a_damaged_disc() {
        let (dir, iso) = scratch_iso("single-damaged");
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![test_title(0, sectors)]);
        let total = sectors as u64 * 2048;

        // The reachable damaged-single-pass state is the RESUME one: a plain
        // copy aborts at the first read error, so damage only returns as a
        // RESULT after a prior run attempted the whole disc — build that mapfile.
        let mapfile_path = disc.mapfile_for(&iso);
        let _ = std::fs::remove_file(&mapfile_path);
        let mut mf =
            crate::recovery::mapfile::Mapfile::create(&mapfile_path, total, "vTEST").unwrap();
        mf.record(0, total, crate::recovery::mapfile::SectorStatus::Finished)
            .unwrap();
        mf.record(
            1000 * 2048,
            2048 * 8,
            crate::recovery::mapfile::SectorStatus::Unreadable,
        )
        .unwrap();
        mf.flush().unwrap();
        std::fs::write(&iso, vec![0u8; total as usize]).unwrap();

        // This path must not read the disc at all — it is terminal.
        let mut reader = ZeroReader { capacity: sectors };
        let mut job = raw_job(&iso);
        job.mode = crate::RipMode::Single;
        let opts = MultipassOpts {
            max_passes: 0,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let r = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("a fully-attempted mapfile with bad bytes is terminal, not an error");

        assert_eq!(r.passes, 1, "single-pass is one dispatch");
        assert!(
            r.unreadable_bytes + r.pending_bytes > 0,
            "fixture check: this run must come back damaged, else every \
             assertion below is vacuous"
        );
        assert!(
            !r.main_lost_ms.is_finite(),
            "single-pass measured nothing, so it must not report a NUMBER of \
             milliseconds lost beside {} unreadable + {} pending bytes",
            r.unreadable_bytes,
            r.pending_bytes,
        );
        // Severity comes from the sector count, not escalated merely because
        // loss is unquantified (that's the abort gate's rule, unused here).
        // The badge is spelled out, not recomputed via the producer's own fns.
        assert_eq!(
            r.severity,
            crate::DamageSeverity::Cosmetic,
            "8 bad sectors and no quantified time loss is a Cosmetic rip: not \
             Clean (bytes ARE missing), and not escalated by the unquantifiable \
             NaN either"
        );
        assert!(!r.aborted_for_loss, "single-pass has no abort gate");

        let _ = std::fs::remove_file(&mapfile_path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multipass_rip_single_pass_mode_is_one_dispatch_no_retry_loop() {
        // max_passes == 0 -> plan_passes(0).multipass == false: one
        // `recovery::copy` dispatch, no sweep/patch split, no abort gate.
        let (dir, iso) = scratch_iso("single-pass");
        let sectors = 256u32;
        let disc = test_disc(sectors, vec![]);
        let mut reader = ZeroReader { capacity: sectors };
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
        let opts = MultipassOpts {
            max_passes: 0,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("single-pass dispatch should succeed on a clean synthetic disc");

        assert_eq!(result.passes, 1, "single-pass mode is exactly one pass");
        assert_eq!(result.unreadable_bytes, 0);
        assert_eq!(result.pending_bytes, 0);
        assert_eq!(result.good_bytes, sectors as u64 * 2048);
        assert!(result.complete);
        assert!(!result.halted);
        assert!(
            !result.aborted_for_loss,
            "single-pass never applies the abort gate"
        );
        // No mapfile-driven patch pass ever ran — no NonTrimmed/Unreadable
        // promotion logic touched, no bad_ranges built.
        assert_eq!(result.main_lost_ms, 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multipass_rip_clean_disc_converges_with_zero_patch_passes() {
        // A fully-readable disc: Pass 1 sweep finds nothing bad, so the
        // patch loop's very first top-of-loop `scope_bad_bytes` check is
        // already 0 -> Converged -> break before any `recovery::patch` call.
        let (dir, iso) = scratch_iso("clean");
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![]);
        let mut reader = ZeroReader { capacity: sectors };
        // Multipass implies raw (enforced in `multipass_rip`); a multipass
        // fixture must say so.
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("clean multipass recovery should succeed");

        assert_eq!(result.passes, 1, "sweep only — no patch pass needed");
        assert_eq!(result.unreadable_bytes, 0);
        assert_eq!(result.pending_bytes, 0);
        assert_eq!(result.good_bytes, sectors as u64 * 2048);
        assert!(result.complete);
        assert!(!result.halted);
        assert!(!result.aborted_for_loss);
        assert_eq!(result.main_lost_ms, 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multipass_rip_recoverable_bad_sector_converges_after_a_patch_pass() {
        // One sector fails Pass 1's touch, then reads clean from Pass 2 on:
        // the patch pass recovers it, muxable scope hits 0 bad bytes, and the
        // NEXT loop-top check (Converged) stops early, well under the 5-pass cap.
        let (dir, iso) = scratch_iso("recoverable");
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![Spot {
                lba: 1000,
                heal_after: 1,
                attempts: 0,
            }],
        };
        // Multipass implies raw (enforced in `multipass_rip`); a multipass
        // fixture must say so.
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("recoverable bad sector should converge");

        assert_eq!(result.passes, 2, "sweep + exactly 1 patch pass to converge");
        assert_eq!(result.unreadable_bytes, 0, "fully recovered — nothing lost");
        assert_eq!(result.pending_bytes, 0);
        assert!(result.complete);
        assert!(!result.halted);
        assert!(!result.aborted_for_loss);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multipass_rip_permanent_loss_past_tolerance_aborts() {
        // A sector that NEVER heals: Pass 1 marks it NonTrimmed, the patch
        // pass recovers nothing (NoProgress -> stop early), promotion turns it
        // Unreadable, and — whole-disc ISO scope + zero tolerance — the gate fires.
        let (dir, iso) = scratch_iso("permanent-loss");
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![Spot {
                lba: 1000,
                heal_after: u32::MAX,
                attempts: 0,
            }],
        };
        // Multipass implies raw (enforced in `multipass_rip`); a multipass
        // fixture must say so.
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("a permanently-bad sector is a reported result, not an Err");

        // NoProgress stops the retry loop before the 5-pass cap: the first
        // patch pass still recovers the bad sector's readable ECC-block
        // neighbours, so pin "stopped early", not an exact ECC-dependent count.
        assert!(
            result.passes > 1 && result.passes < 1 + opts.max_passes,
            "expected the retry loop to exhaust progress before the pass cap, got {} passes",
            result.passes
        );
        // The exact count matters: a loose range let `== NoProgress` be
        // mutated to `!=` and stay green (breaking after the first, progress-
        // making pass). Pass 1 heals ECC neighbours; pass 2 must stop the loop.
        assert_eq!(
            result.passes, 3,
            "1 sweep + 2 patch passes: the loop must stop on the pass that \
             recovered nothing, not on the pass that made progress"
        );
        assert!(
            result.unreadable_bytes > 0,
            "the sector was never recovered"
        );
        assert!(!result.halted);
        assert!(
            result.aborted_for_loss,
            "zero tolerance + confirmed loss must abort"
        );
        assert!(!result.complete);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multipass_rip_respects_max_passes_bound_even_with_ongoing_progress() {
        // Two bad sectors: one heals next touch (progress, so NoProgress never
        // fires); the other never heals (scope never hits 0, so Converged never
        // fires). With max_passes == 1 the loop must stop purely on budget.
        let (dir, iso) = scratch_iso("max-passes-bound");
        let sectors = 200_000u32;
        let disc = test_disc(sectors, vec![]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![
                Spot {
                    lba: 1_000,
                    heal_after: 1,
                    attempts: 0,
                },
                Spot {
                    lba: 100_000,
                    heal_after: u32::MAX,
                    attempts: 0,
                },
            ],
        };
        // Multipass implies raw (enforced in `multipass_rip`).
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        let opts = MultipassOpts {
            max_passes: 1,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("bounded run is a reported result, not an Err");

        assert_eq!(
            result.passes, 2,
            "sweep + exactly the 1 allowed patch pass, no more"
        );
        assert!(
            !result.complete,
            "the permanent spot is still bad — never converged"
        );
        assert!(result.unreadable_bytes > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multipass_rip_scope_bad_bytes_wiring_drives_convergence_by_output_kind() {
        // A permanently-bad sector OUTSIDE the muxed title's extents. MKV/M2TS
        // scope sees 0 bad bytes and converges immediately, without calling
        // `recovery::patch`; ISO's whole-disc scope grinds a pass, then aborts.
        let title = test_title(0, 2_000); // extents [0, 2000)
        let bad_lba = 50_000; // outside the title's extents
        let sectors = 200_000u32;
        // Multipass implies raw (enforced in `multipass_rip`).
        let mut job = Job::new("disc:///dev/null", "placeholder");
        job.raw = true;

        // MKV/M2TS scope: out-of-title damage doesn't earn a retry pass.
        {
            let (dir, iso) = scratch_iso("scope-mkv");
            let disc = test_disc(sectors, vec![title.clone()]);
            let mut reader = MultiSpotReader {
                capacity: sectors,
                spots: vec![Spot {
                    lba: bad_lba,
                    heal_after: u32::MAX,
                    attempts: 0,
                }],
            };
            let opts = MultipassOpts {
                max_passes: 5,
                abort_on_lost_secs: 0,
                is_iso_output: false,
            };
            let result = multipass_rip(
                &disc,
                &mut reader,
                &iso,
                &job,
                &opts,
                &crate::sink::NoopSink,
            )
            .expect("out-of-title loss must not fail the rip");
            assert_eq!(
                result.passes, 1,
                "muxable scope was already 0 bad bytes — no patch pass ran"
            );
            assert!(!result.aborted_for_loss, "loss is entirely out of scope");
            let _ = std::fs::remove_dir_all(&dir);
        }

        // ISO scope: the SAME out-of-title byte counts whole-disc and aborts.
        {
            let (dir, iso) = scratch_iso("scope-iso");
            let disc = test_disc(sectors, vec![title.clone()]);
            let mut reader = MultiSpotReader {
                capacity: sectors,
                spots: vec![Spot {
                    lba: bad_lba,
                    heal_after: u32::MAX,
                    attempts: 0,
                }],
            };
            let opts = MultipassOpts {
                max_passes: 5,
                abort_on_lost_secs: 0,
                is_iso_output: true,
            };
            let result = multipass_rip(
                &disc,
                &mut reader,
                &iso,
                &job,
                &opts,
                &crate::sink::NoopSink,
            )
            .expect("whole-disc-scoped loss is still a reported result");
            // Pinned exactly, as above: a loose `passes > 1 && < 1 + max_passes`
            // range let `== NoProgress` mutate to `!=` and stay green (breaking
            // after the first, progress-making pass). Pass 2 must stop the loop.
            assert_eq!(
                result.passes, 3,
                "whole-disc scope must see the bad byte and keep patching until \
                 a pass recovers nothing: 1 sweep + 2 patch passes"
            );
            assert!(
                result.aborted_for_loss,
                "ISO scope counts every byte — this loss must abort"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ── The three decisions lifted out of `multipass_rip_inner`. ──

    #[test]
    fn pass_should_decrypt_is_the_negation_of_raw() {
        assert!(
            !pass_should_decrypt(true),
            "a raw pass must never decrypt — raw exists to keep the ciphertext"
        );
        assert!(
            pass_should_decrypt(false),
            "a non-raw pass must decrypt, or the ISO is unplayable ciphertext"
        );
    }

    #[test]
    fn bad_sector_count_divides_bytes_into_whole_sectors() {
        assert_eq!(bad_sector_count(0, 0), 0);
        assert_eq!(bad_sector_count(2048, 0), 1);
        assert_eq!(bad_sector_count(0, 2048), 1);
        assert_eq!(
            bad_sector_count(4096, 2048),
            3,
            "both counters are summed, then converted once"
        );
        assert_eq!(
            bad_sector_count(2047, 0),
            0,
            "a partial sector rounds down, it does not become a whole bad sector"
        );
        assert_eq!(
            bad_sector_count(u64::MAX, u64::MAX),
            u64::MAX / 2048,
            "the sum saturates instead of wrapping to a tiny damage count"
        );
    }

    // The final verdict must score DAMAGE, never un-attempted disc: a 66 GB
    // disc with 64 GB never attempted must not read ~33M bad sectors
    // (`Serious`) via `bytes_pending`. See docs/multipass.md.
    #[test]
    fn the_final_score_ignores_un_attempted_sectors() {
        let nothing_failed_much_unread = MapStats {
            bytes_total: 66_000_000_000,
            bytes_good: 2_000_000_000,
            bytes_unreadable: 0,
            // 64 GB pending, ALL of it never attempted.
            bytes_pending: 64_000_000_000,
            bytes_nontried: 64_000_000_000,
            bytes_retryable: 0,
            num_bad_ranges: 0,
            main_lost_ms: 0.0,
        };
        assert_eq!(
            end_of_recovery_bad_sectors(&nothing_failed_much_unread),
            0,
            "un-attempted disc is not damage"
        );
        assert_eq!(
            classify_damage(
                end_of_recovery_bad_sectors(&nothing_failed_much_unread),
                0.0
            ),
            crate::DamageSeverity::Clean,
        );
    }

    /// The other side: real damage still scores, and both damage kinds count.
    /// 8 unreadable sectors + 4 retryable = 12 → `Cosmetic` (1..=50), a
    /// literal read off `classify_damage`'s documented boundaries.
    #[test]
    fn the_final_score_counts_unreadable_and_retryable_damage() {
        let damaged = MapStats {
            bytes_total: 66_000_000_000,
            bytes_good: 65_000_000_000,
            bytes_unreadable: 8 * 2048,
            bytes_pending: 4 * 2048 + 1_000_000_000,
            bytes_nontried: 1_000_000_000,
            bytes_retryable: 4 * 2048,
            num_bad_ranges: 2,
            main_lost_ms: 0.0,
        };
        assert_eq!(end_of_recovery_bad_sectors(&damaged), 12);
        assert_eq!(
            classify_damage(end_of_recovery_bad_sectors(&damaged), 0.0),
            crate::DamageSeverity::Cosmetic,
        );
    }

    // An UNMEASURED muxable scope must never read as a converged one: a
    // failed mapfile load used to fall back to zero, the ONE value meaning
    // "converged, stop retrying". See docs/multipass.md.
    #[test]
    fn an_unmeasured_scope_never_converges() {
        assert_eq!(
            patch_pass_decision_measured(None, None),
            PatchDecision::Continue,
            "unknown scope must run the pass, not declare victory"
        );
        // Zero is still convergence when it was actually MEASURED — the
        // distinction this function exists to draw.
        assert_eq!(
            patch_pass_decision_measured(Some(0), None),
            PatchDecision::Converged,
        );
        // A pass that recovered nothing is exhausted whether or not the scope
        // could be measured: that fact comes from the pass, not the mapfile.
        assert_eq!(
            patch_pass_decision_measured(None, Some(0)),
            PatchDecision::NoProgress,
        );
        assert_eq!(
            patch_pass_decision_measured(None, Some(1_000_000)),
            PatchDecision::Continue,
        );
        // And it still defers to the measured answer when there is one.
        assert_eq!(
            patch_pass_decision_measured(Some(4096), Some(0)),
            PatchDecision::NoProgress,
        );
        assert_eq!(
            patch_pass_decision_measured(Some(4096), Some(1_000_000)),
            PatchDecision::Continue,
        );
    }

    // ── Three `multipass_rip_inner` fail-safes no black-box fixture reached
    // (promotion, unreadable mapfile, mid-loop cancel) — `HookSink` below
    // uses log lines as the clock that lets a test change the world mid-loop. ──
    struct HookSink {
        trigger: &'static str,
        action: Box<dyn Fn() + Send + Sync>,
        cancel_after_trigger: bool,
        fired: std::sync::atomic::AtomicBool,
        logs: std::sync::Mutex<Vec<(Level, String)>>,
    }

    impl HookSink {
        fn new(
            trigger: &'static str,
            cancel_after_trigger: bool,
            action: Box<dyn Fn() + Send + Sync>,
        ) -> Self {
            Self {
                trigger,
                action,
                cancel_after_trigger,
                fired: std::sync::atomic::AtomicBool::new(false),
                logs: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn cancelling(trigger: &'static str) -> Self {
            Self::new(trigger, true, Box::new(|| {}))
        }

        fn did_fire(&self) -> bool {
            self.fired.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Did the loop log a line containing `needle` at `level`?
        fn logged(&self, level: Level, needle: &str) -> bool {
            self.logs
                .lock()
                .unwrap()
                .iter()
                .any(|(l, m)| *l == level && m.contains(needle))
        }
    }

    impl Sink for HookSink {
        fn log(&self, level: Level, msg: &str) {
            self.logs.lock().unwrap().push((level, msg.to_string()));
            if msg.contains(self.trigger)
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                (self.action)();
            }
        }
        fn should_cancel(&self) -> bool {
            self.cancel_after_trigger && self.did_fire()
        }
    }

    // A 4096-sector disc whose ONE title spans the whole image, so an
    // MKV-scoped gate sees damage at LBA 1000 and runs patch passes.
    // Returns (scratch dir, ISO path, mapfile path, disc).
    fn in_title_damage_fixture(
        tag: &str,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        libfreemkv::Disc,
    ) {
        let (dir, iso) = scratch_iso(tag);
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![test_title(0, sectors)]);
        let mapfile = disc.mapfile_for(&iso);
        (dir, iso, mapfile, disc)
    }

    /// The reader half of [`in_title_damage_fixture`]: LBA 1000 never heals.
    fn never_healing_reader() -> MultiSpotReader {
        MultiSpotReader {
            capacity: 4096,
            spots: vec![Spot {
                lba: 1000,
                heal_after: u32::MAX,
                attempts: 0,
            }],
        }
    }

    /// A raw (multipass-legal) job writing to `iso`.
    fn raw_job(iso: &std::path::Path) -> Job {
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        job
    }

    // A generous tolerance: the real residual loss on the fixture above is
    // ~35s of a 7200s title, so an HOUR accepts it — every abort asserted
    // below comes from the fail-safe under test and nothing else.
    const GENEROUS_TOLERANCE_SECS: u64 = 3600;

    // CONTROL for the two sabotage tests below: same disc/damage/tolerance,
    // mapfile untouched. Must NOT abort, or the sabotage tests could pass
    // for the wrong reason. See docs/multipass.md.
    #[test]
    fn multipass_rip_accepts_a_measurable_loss_under_a_generous_tolerance() {
        let (dir, iso, _mapfile, disc) = in_title_damage_fixture("gate-control");
        let mut reader = never_healing_reader();
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
            is_iso_output: false,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("a permanently-bad sector is a reported result, not an Err");

        assert!(!result.halted);
        assert!(
            result.unreadable_bytes > 0,
            "the fixture must actually end with confirmed loss"
        );
        assert!(
            result.main_lost_ms.is_finite() && result.main_lost_ms > 0.0,
            "the loss must be quantifiable when the mapfile is intact, got {}",
            result.main_lost_ms
        );
        assert!(
            !result.aborted_for_loss,
            "{} ms of loss is well inside a {GENEROUS_TOLERANCE_SECS}s tolerance",
            result.main_lost_ms
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // On an ISO rip, damage entirely OUTSIDE the main title must not be
    // reported as main-title playback loss (the whole-disc `abort_lost_bytes`
    // count once got scaled by the main title's size/duration). See docs/multipass.md.
    #[test]
    fn iso_damage_outside_the_main_title_is_not_reported_as_main_title_loss() {
        let (dir, iso) = scratch_iso("iso-off-title-loss");
        let sectors = 4096u32;
        // The title occupies sectors 0..100 ONLY. The never-healing spot is at
        // LBA 1000, comfortably outside it.
        let disc = test_disc(sectors, vec![test_title(0, 100)]);
        let mut reader = never_healing_reader();
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
            is_iso_output: true,
        };

        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("permanent off-title damage is a reported result, not an Err");

        assert!(
            result.unreadable_bytes > 0,
            "the fixture must actually end with confirmed loss"
        );
        assert_eq!(
            libfreemkv::disc::bytes_bad_in_title(&test_title(0, 100), &[(1000 * 2048, 2048)]),
            0,
            "sanity: the damaged LBA really is outside the title's extents"
        );
        assert_eq!(
            result.main_lost_ms, 0.0,
            "the main title was read perfectly; its playback loss is zero"
        );
        assert_eq!(
            result.severity,
            crate::DamageSeverity::Cosmetic,
            "a handful of off-title bad sectors is Cosmetic, not Serious"
        );
        assert!(
            result.aborted_for_loss,
            "an ISO deliverable still refuses ANY unreadable byte — the honest \
             millisecond figure must not weaken the whole-disc gate"
        );
        assert!(!result.complete, "a rip the gate refused is never complete");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The LIVE end-of-recovery gate must abort when the mapfile cannot be
    // read at the abort-decision point: sabotages it into a directory right
    // as the patch loop breaks and expects the NaN fail-safe. See docs/multipass.md.
    #[test]
    fn multipass_rip_aborts_when_the_mapfile_cannot_be_read_at_the_gate() {
        let (dir, iso, mapfile, disc) = in_title_damage_fixture("gate-unreadable-mapfile");
        let mut reader = never_healing_reader();
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
            is_iso_output: false,
        };

        // Sabotage: the mapfile becomes a DIRECTORY, so `read_to_string`
        // fails with EISDIR no matter which user runs the suite.
        let victim = mapfile.clone();
        let sink = HookSink::new(
            "exhausted",
            false,
            Box::new(move || {
                let _ = std::fs::remove_file(&victim);
                std::fs::create_dir_all(&victim).expect("sabotage: mapfile -> directory");
            }),
        );

        let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
            .expect("an unreadable mapfile is a fail-safe verdict, not an Err");

        assert!(
            sink.did_fire(),
            "the sabotage never ran — test proves nothing"
        );
        assert!(mapfile.is_dir(), "the mapfile must still be unreadable");
        assert!(
            sink.logged(
                Level::Error,
                "mapfile could not be loaded to verify loss — forcing abort"
            ),
            "the gate must say it is failing safe: {:?}",
            sink.logs.lock().unwrap()
        );
        assert!(
            result.main_lost_ms.is_nan(),
            "an unreadable damage record is unquantifiable loss, got {}",
            result.main_lost_ms
        );
        assert!(
            result.aborted_for_loss,
            "the abort must fire even under a {GENEROUS_TOLERANCE_SECS}s tolerance"
        );
        assert!(!result.complete, "a rip the gate refused is never complete");
        assert_eq!(
            result.severity,
            crate::DamageSeverity::Serious,
            "an unquantifiable loss is Serious, not a lower tier"
        );
        assert!(!result.halted, "this is the gate firing, not a cancel");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // A failed end-of-recovery PROMOTION must abort the rip: `Mapfile::load`
    // SUCCEEDS here (unlike the test above) but `<mapfile>.tmp` is sabotaged
    // into a directory so `flush()` fails. See docs/multipass.md.
    #[test]
    fn multipass_rip_aborts_when_the_end_of_recovery_promotion_cannot_be_persisted() {
        let (dir, iso, mapfile, disc) = in_title_damage_fixture("gate-promotion-failure");
        let mut reader = never_healing_reader();
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
            is_iso_output: false,
        };

        let tmp_path = {
            let mut s = mapfile.clone().into_os_string();
            s.push(".tmp");
            std::path::PathBuf::from(s)
        };
        let victim = tmp_path.clone();
        let sink = HookSink::new(
            "exhausted",
            false,
            Box::new(move || {
                let _ = std::fs::remove_file(&victim);
                std::fs::create_dir_all(&victim).expect("sabotage: mapfile.tmp -> directory");
            }),
        );

        let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
            .expect("a failed promotion is a fail-safe verdict, not an Err");

        assert!(
            sink.did_fire(),
            "the sabotage never ran — test proves nothing"
        );
        assert!(tmp_path.is_dir(), "the mapfile must still be unwritable");
        assert!(
            !sink.logged(Level::Error, "mapfile could not be loaded"),
            "the mapfile must LOAD fine — this is the promotion branch, not \
             the unreadable-mapfile branch: {:?}",
            sink.logs.lock().unwrap()
        );
        assert!(
            sink.logged(Level::Warn, "failed to flush promoted mapfile")
                || sink.logged(Level::Warn, "end-of-recovery promotion failed"),
            "a failed promotion must be reported: {:?}",
            sink.logs.lock().unwrap()
        );
        assert!(
            sink.logged(Level::Error, "damage record is incomplete"),
            "the gate must say WHY the loss is unquantifiable: {:?}",
            sink.logs.lock().unwrap()
        );
        assert!(
            result.main_lost_ms.is_nan(),
            "an incomplete damage record is unquantifiable loss, got {}",
            result.main_lost_ms
        );
        assert!(
            result.aborted_for_loss,
            "the abort must fire even under a {GENEROUS_TOLERANCE_SECS}s tolerance"
        );
        assert!(!result.complete);
        assert_eq!(result.severity, crate::DamageSeverity::Serious);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // A rip cancelled mid-loop, AFTER damage has been found, is halted and
    // never Clean — severity there was once hard-coded `Clean`. Cancel is
    // armed from the "pass N recovered" log line; see docs/multipass.md.
    #[test]
    fn multipass_rip_cancelled_mid_loop_is_halted_and_never_reported_clean() {
        let (dir, iso) = scratch_iso("mid-loop-cancel");
        let sectors = 200_000u32;
        let disc = test_disc(sectors, vec![]);
        // One spot that heals on the next touch (so patch pass 1 makes real
        // progress and the loop-bottom NoProgress gate does NOT break for us)
        // and one that never heals (so the scope never converges either).
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![
                Spot {
                    lba: 1_000,
                    heal_after: 1,
                    attempts: 0,
                },
                Spot {
                    lba: 100_000,
                    heal_after: u32::MAX,
                    attempts: 0,
                },
            ],
        };
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let sink = HookSink::cancelling("multipass_rip: pass ");
        let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
            .expect("a cancelled rip is a partial result, not an Err");

        assert!(
            sink.did_fire(),
            "the cancel was never armed — test proves nothing"
        );
        assert!(result.halted, "a cancelled rip must report halted");
        assert_eq!(
            result.passes, 2,
            "sweep + the one patch pass that ran before the cancel: the loop \
             must stop at its own top-of-loop cancel check"
        );
        assert!(
            result.unreadable_bytes + result.pending_bytes > 0,
            "the fixture must have found damage BEFORE the cancel, or the \
             severity assertion below is vacuous"
        );
        assert_ne!(
            result.severity,
            crate::DamageSeverity::Clean,
            "a cancelled rip holding {} unreadable + {} pending bytes is not \
             Clean — that badge contradicted the counters next to it",
            result.unreadable_bytes,
            result.pending_bytes
        );
        // Expects the literal tier, not a re-run of the old formula
        // (`classify_damage(bad_sector_count(unreadable, pending), 0.0)`),
        // which `interrupted_severity` deliberately avoids — see the wide-pending test.
        assert_eq!(
            result.unreadable_bytes, 0,
            "fixture: nothing was CONFIRMED lost, so the tier below is the \
             not-Clean floor an interrupted run gets for outstanding work"
        );
        assert_eq!(
            result.severity,
            crate::DamageSeverity::Cosmetic,
            "a cancel holding {} pending bytes and nothing unreadable is \
             Cosmetic: not Clean, and not a damage claim nobody measured",
            result.pending_bytes
        );
        assert!(!result.complete, "an interrupted rip is never complete");
        assert!(
            !result.aborted_for_loss,
            "the abort gate is not reached on the halted path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The halted exit must score damage it MEASURED, not work not got to —
    // a wide unrecovered region where folding pending in would wrongly
    // stamp Serious on confirmed-zero loss. See docs/multipass.md.
    #[test]
    fn a_cancel_with_a_wide_pending_region_is_not_scored_from_it() {
        /// Cancels on the FIRST progress tick, so the sweep stops with the bulk
        /// of the disc never attempted — the "cancelled ten seconds into a
        /// 66 GB disc" case, at fixture scale.
        struct CancelAtOnce;
        impl Sink for CancelAtOnce {
            fn should_cancel(&self) -> bool {
                true
            }
        }

        let (dir, iso) = scratch_iso("wide-pending-cancel");
        let sectors = 8192u32;
        let disc = test_disc(sectors, vec![test_title(0, sectors)]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: Vec::new(), // a PERFECT disc: nothing is unreadable
        };
        let job = raw_job(&iso);
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };

        let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &CancelAtOnce)
            .expect("a cancelled rip is a partial result, not an Err");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.halted);
        assert_eq!(
            result.unreadable_bytes, 0,
            "nothing is promoted to Unreadable on the halted path, so nothing \
             is CONFIRMED lost"
        );
        assert!(
            result.pending_bytes / 2048 >= 500,
            "fixture: the pending region must clear the Serious threshold or \
             the two formulas agree again — got {} sectors",
            result.pending_bytes / 2048
        );
        assert_eq!(
            result.severity,
            crate::DamageSeverity::Cosmetic,
            "an interrupted run scores only what it measured: {} unreadable \
             bytes beside {} pending",
            result.unreadable_bytes,
            result.pending_bytes
        );
    }

    #[test]
    fn recovery_is_complete_requires_all_three() {
        assert!(recovery_is_complete(false, 0, 0));
        assert!(
            !recovery_is_complete(true, 0, 0),
            "a rip the abort gate refused is never complete, however clean the counters look"
        );
        assert!(
            !recovery_is_complete(false, 1, 0),
            "unreadable bytes remain"
        );
        assert!(!recovery_is_complete(false, 0, 1), "pending bytes remain");
        assert!(!recovery_is_complete(true, 1, 1));
    }
}
