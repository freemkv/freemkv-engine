//! The multipass rip STRATEGY: sweep → N patch passes → abort-on-loss gate.
//!
//! [`crate::recovery::copy`] performs ONE dispatch step (sweep, or one patch
//! pass, or a terminal result — chosen from mapfile state). The multipass
//! *loop* — call it repeatedly until the disc is clean or recovery stops making
//! progress, then decide whether the residual loss is acceptable — is the
//! strategy that lived duplicated inside autorip's `rip_disc` and (partially)
//! the CLI. It moves here so every front-end shares one implementation.
//!
//! The abort-on-loss gate mirrors autorip's `loss_aborts` semantics exactly
//! (hard rule #6): `abort_on_lost_secs == 0` means "require a perfect rip"
//! (ANY lost byte aborts), a positive value tolerates that many seconds of
//! main-movie loss, and an unquantifiable (NaN) loss always fails safe to
//! abort. These are pure functions, ported verbatim with their tests.

use crate::job::Job;
use crate::recovery::mapfile::{Mapfile, SectorStatus};
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
/// An mkv-scoped rip measures damage inside the main title's extents. A title
/// with no extents — a scan that could not read the playlist metadata, or the
/// `DiscTitle::empty()` fallback `multipass_rip_inner` uses when the disc
/// reports no titles at all — makes that measurement return 0, which is
/// indistinguishable from "clean". Whole-disc (ISO) scope sums the bad ranges
/// directly and needs no extents, so it is never unscopable.
///
/// Shared so the two loss paths cannot drift: `abort_lost_ms` had this guard
/// and the live gate in `multipass_rip_inner` did not, which is precisely how a
/// damaged rip kept being delivered as clean after the guard was written.
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
/// Originally a verbatim port of autorip's `abort_lost_ms`; it no longer is.
/// It now fails safe to NaN when the loss exists but cannot be measured —
/// see [`loss_is_unscopable`].
///
/// NOTE for callers offering a loss override: NaN aborts under EVERY
/// threshold, including `u64::MAX`. autorip's `.accept-loss` escape hatch
/// therefore does NOT apply to a disc whose title has no extents. That is
/// deliberate — an override should waive a loss you can measure, not one you
/// cannot — but it is a behaviour change worth knowing about.
pub fn abort_lost_ms(
    output_is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
    title_bytes_per_sec: f64,
) -> f64 {
    // An UNSCOPABLE mkv-output title: `bytes_bad_in_title` returns 0 when the
    // title has no extents, which is indistinguishable from "no damage" — and
    // an extent-less title comes from the same failed scan that leaves the
    // bitrate at 0, so the two arrive together. Returning 0.0 here would report
    // a damaged disc as clean. Checked BEFORE the zero-bytes early return,
    // because that is the branch it would otherwise take.
    if loss_is_unscopable(output_is_iso, title, bad_ranges) {
        return f64::NAN;
    }
    let lost_bytes = abort_lost_bytes(output_is_iso, title, bad_ranges);
    // Genuinely no loss is genuinely zero — NaN here would abort clean rips.
    if lost_bytes == 0 {
        return 0.0;
    }
    // Loss exists but cannot be converted to time. Every other unquantifiable
    // path in this crate answers NaN, which `loss_aborts` /
    // `should_abort_for_loss` treat as fail-safe abort; answering 0.0 let a
    // configured seconds tolerance silently accept loss it could not measure.
    // `<= 0.0` alone let an INFINITE bitrate through: `lost_bytes / inf` is
    // 0.0 ms, so a tolerance accepted loss that was never measured. Any
    // non-finite or non-positive rate is unusable.
    if !(title_bytes_per_sec.is_finite() && title_bytes_per_sec > 0.0) {
        return f64::NAN;
    }
    lost_bytes as f64 / title_bytes_per_sec * MILLIS_PER_SEC
}

// ─────────────────────────────────────────────────────────────────────────────
// MULTIPASS STRATEGY DECISIONS — relocated verbatim from autorip's `rip_disc`.
//
// These pure functions are the loop's decision surface: pass ordering, the
// scope-aware convergence check, the no-progress exhaustion gate, and the
// end-of-recovery status promotion. They were characterized in place inside
// autorip (its `char_*` tests pin them byte-for-byte) before this move, so a
// relocation with identical signatures/bodies is behavior-preserving by
// construction. autorip now calls these directly instead of carrying its own
// copies; the freemkv GUI (a future front-end) will use the same
// implementation without duplicating it.
// ─────────────────────────────────────────────────────────────────────────────

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
            // saturating: max_retries is a u8 and the caller clamps to
            // u8::MAX, so `+ 2` overflows for anything >= 254 — a panic in
            // dev/test and a silent wrap to 1 under release, where the UI's
            // pass denominator would then be smaller than the pass count.
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
/// `mux_scope_bad` is the scope-aware bad-byte count observed BEFORE a pass runs
/// (0 ⇒ the scope is already clean ⇒ `Converged`). `recovered` is what a pass
/// recovered, consulted only when the scope is still bad: `None` models the
/// loop-top evaluation (no pass has run yet ⇒ `Continue`); `Some(0)` is a pass
/// that recovered nothing ⇒ `NoProgress`; `Some(n>0)` ⇒ `Continue`.
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

/// The end-of-recovery promotion: after the final patch pass, bytes still in a
/// "maybe" state across every pass are promoted to `Unreadable` (confirmed
/// lost) BEFORE the abort/loss gate reads them. Returns the `(from, to)`
/// statuses the loop applies. libfreemkv's patch loop intentionally defers this
/// final-verdict step to the orchestrator.
///
/// BOTH maybe-states are promoted. `NonScraped` ('/') is as much a failed read
/// as `NonTrimmed`: `damage_sector_statuses` includes it, so every patch pass
/// retries it, and `bad_sector_statuses` counts it as damage. Promoting only
/// NonTrimmed left any surviving NonScraped range invisible to the abort gate,
/// which reads `Unreadable` alone — so genuinely lost bytes could not fire the
/// abort, and the rip was delivered as good. Such ranges arrive from an
/// imported or ddrescue-written mapfile, an interop this module advertises.
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
    // An unquantifiable loss fails SAFE, matching `should_abort_for_loss`.
    // Every NaN comparison is false, so without this a NaN fell through both
    // tiers to Cosmetic — the badge would read "Cosmetic" on the very rip the
    // abort gate is simultaneously refusing to deliver.
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

/// Whether a recovery pass decrypts in place, given the job's `raw` flag.
///
/// FOUR pass-option sites spelled this `!job.raw` inline — the single-pass
/// copy, the Pass 1 sweep and every patch pass here, plus `recover_to_iso`'s
/// own `CopyOptions` in `run.rs`. Four copies of one policy is the shape that
/// drifts, and a lone `!` is the easiest character in the file to lose. It
/// matters in both directions: `raw` exists to preserve the ciphertext for
/// out-of-band decryption later, so a raw pass that decrypts defeats the mode
/// — and a non-raw pass that does NOT decrypt writes an ISO of ciphertext and
/// returns success, which is the silent-garbage-success class the pre-flight
/// gate in `copy` exists to stop. Named so it reads as a decision.
pub(crate) fn pass_should_decrypt(raw: bool) -> bool {
    !raw
}

/// Bad bytes expressed in whole sectors, the unit [`classify_damage`] scores.
///
/// Shared by all three `MultipassResult` exit paths (single-pass, halted,
/// end-of-recovery) so the byte→sector conversion cannot drift between them.
/// Rounds down: a partial sector of damage is still less than one sector.
fn bad_sector_count(unreadable_bytes: u64, pending_bytes: u64) -> u64 {
    unreadable_bytes.saturating_add(pending_bytes) / SECTOR_BYTES
}

/// A recovery is complete only when the abort-on-loss gate did NOT fire and
/// the mapfile shows zero unreadable and zero pending bytes.
///
/// `aborted_for_loss` is load-bearing on its own: the unreadable-mapfile
/// fail-safe sets `main_lost_ms = NaN` (so the gate fires) while the carried
/// in-flight counters can both legitimately be zero — a disc that looked clean
/// all the way through the loop but whose damage record could not be re-read
/// at the final verification step. Without that term such a rip reports
/// `complete: true` on the exact run the abort gate just refused. Extracted
/// because reaching that branch from outside needs the mapfile corrupted
/// mid-function, which no black-box fixture can do.
fn recovery_is_complete(aborted_for_loss: bool, unreadable_bytes: u64, pending_bytes: u64) -> bool {
    !aborted_for_loss && unreadable_bytes == 0 && pending_bytes == 0
}

/// Milliseconds of main-title playback lost, given the bad bytes in the main
/// title and the title's own size + duration. Scales the main title's bad
/// bytes by its OWN size and runtime (the dimensionally-correct figure the CLI
/// switched to — a whole-disc ratio × first-title duration was wrong once bonus
/// content made the disc larger than the main title). Returns NaN when the
/// title has no measurable bitrate but does have loss (unquantifiable → fails
/// safe in the gate).
fn main_title_lost_ms(disc: &libfreemkv::Disc, main_bad_bytes: u64) -> f64 {
    if main_bad_bytes == 0 {
        return 0.0;
    }
    match disc.titles.first() {
        Some(t) if t.size_bytes > 0 && t.duration_secs > 0.0 && t.duration_secs.is_finite() => {
            main_bad_bytes as f64 / t.size_bytes as f64 * t.duration_secs * MILLIS_PER_SEC
        }
        // Loss exists but we can't quantify it (no bitrate) → NaN, which the
        // gate treats as fail-safe abort.
        _ => f64::NAN,
    }
}

/// The end-of-recovery loss figure, plus the reason it is unquantifiable when
/// it is. `None` means the number is trustworthy.
///
/// Pure and separate from [`multipass_rip_inner`] deliberately. This decision
/// lived inline inside a function that needs a drive, a mapfile and a live
/// sink, so nothing could reach it — which is exactly how it shipped answering
/// `0.0` for damage it could not measure. A test of the PREDICATE alone does
/// not guard the gate: the bug was that the gate did not consult the predicate.
pub fn end_of_recovery_lost_ms(
    promotion_intact: bool,
    is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
    disc: &libfreemkv::Disc,
    lost_bytes: u64,
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
    if loss_is_unscopable(is_iso, title, bad_ranges) {
        return (
            f64::NAN,
            Some(
                "multipass_rip: the disc reports no title extents, so in-title \
                 loss cannot be measured — treating loss as unquantifiable",
            ),
        );
    }
    (main_title_lost_ms(disc, lost_bytes), None)
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
    /// recovery abort gate ([`abort_lost_bytes`]/[`abort_lost_ms`]) to the
    /// whole disc instead of just the muxed title's extents, and forces
    /// `abort_on_lost_secs` to `0` via [`effective_abort_secs`].
    pub is_iso_output: bool,
}

/// Drive the full multipass STRATEGY LOOP: sweep, then patch passes until the
/// muxable scope is clean or a pass makes no progress or `opts.max_passes` is
/// reached, then apply the end-of-recovery promotion and the abort-on-loss
/// gate. This is the shared composition every front-end (CLI, autorip, the
/// future GUI) drives instead of carrying its own copy of the loop — mirrors
/// autorip's `rip_disc` pass sequence exactly (sweep via `recovery::sweep`,
/// then `for` patch passes via `recovery::patch`, gated by
/// [`patch_pass_decision`] at loop-top and loop-bottom, promoted via
/// [`end_of_recovery_promotion`], gated by [`loss_aborts`]) with the
/// hardware/UI touch-points (transport-crash retry, `spin_cycle`, watchdogs,
/// per-pass STATE painting) omitted — those are autorip-shell concerns, not
/// strategy.
///
/// `opts.max_passes == 0` (via [`plan_passes`]) takes the single-pass branch:
/// one `recovery::copy` dispatch, decrypting unless `job.raw`, no retry loop.
/// Otherwise Pass 1 is a fresh (non-resume) `recovery::sweep` with
/// `skip_on_error: true`, followed by up to `opts.max_passes` `recovery::patch`
/// passes. The disc is re-read from `reader` each pass exactly as the drive is
/// today. Progress flows through `sink` via the same [`ProgressBridge`]
/// `recover_to_iso` uses — one bridge (fresh speed/ETA estimator) per
/// primitive call.
pub fn multipass_rip(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    sink: &dyn Sink,
) -> crate::Result<MultipassResult> {
    // Every recovery primitive below sleeps through damage cooldowns, and a
    // sleeping pass emits no progress ticks — so the progress callback's
    // `should_cancel` return value cannot be consulted while it waits. Run the
    // whole multipass under one halt token so Stop is honoured mid-cooldown in
    // the sweep AND in every patch pass. See `with_cancel_watcher`.
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

    // Multipass implies raw — but the test is the RESOLVED PLAN, not the
    // entry point. `max_passes: 0` selects the single-pass branch below, which
    // is an ordinary decrypting `copy` dispatch and must stay allowed;
    // refusing on `!job.raw` here would have broken it. Only a real
    // sweep-plus-patch plan is a whole-disc image recovery that cannot
    // decrypt.
    //
    // Enforced here as well as in `preflight` because preflight is advisory by
    // contract (callable without executing) and a front-end may skip it.
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
        // Same reasoning as the halted branch: a single-pass run that came
        // back with unreadable bytes is not Clean just because there was only
        // one pass. Clean is a claim about the DISC, not about the plan.
        let bad_sectors = bad_sector_count(cr.bytes_unreadable, cr.bytes_pending);
        return Ok(MultipassResult {
            unreadable_bytes: cr.bytes_unreadable,
            pending_bytes: cr.bytes_pending,
            good_bytes: cr.bytes_good,
            main_lost_ms: 0.0,
            severity: classify_damage(bad_sectors, 0.0),
            passes: 1,
            aborted_for_loss: false,
            halted: cr.halted,
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

            // Loop-top convergence gate: the muxable scope is already clean
            // in the mapfile — skip remaining passes and proceed to mux.
            // Conservative fallback (whole in-flight bad count) if the
            // mapfile can't be read, so a transient load error never skips a
            // needed pass.
            let mux_scope_bad = match Mapfile::load(&mapfile_path) {
                Ok(map) => {
                    let bad = map.ranges_with(&bad_sector_statuses());
                    scope_bad_bytes(opts.is_iso_output, &bad, main_title)
                }
                Err(_) => last_pending.saturating_add(last_unreadable),
            };
            if patch_pass_decision(mux_scope_bad, None) == PatchDecision::Converged {
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

            if pr.halted {
                halted = true;
                break;
            }
            sink.log(
                Level::Info,
                &format!(
                    "multipass_rip: pass {passes} recovered {recovered} bytes; {} bytes still pending",
                    last_pending
                ),
            );
            // Loop-bottom exhaustion gate, evaluated against the SAME
            // pre-pass `mux_scope_bad` the top-of-loop check used (see
            // `patch_pass_decision`'s doc): a pass that recovered nothing
            // won't be helped by another pass with the same drive state.
            if patch_pass_decision(mux_scope_bad, Some(recovered)) == PatchDecision::NoProgress {
                sink.log(
                    Level::Info,
                    "multipass_rip: patch pass made no progress — exhausted, muxing on what we have",
                );
                break;
            }
        }
    }

    if halted {
        // Severity comes from the damage actually recorded so far. Hard-coding
        // Clean here made a cancelled rip that had already found 300 MB of
        // unreadable sectors render a "Clean" badge next to a non-zero
        // unreadable count — the result contradicted itself, and Clean is the
        // reading that gets believed.
        //
        // main_lost_ms stays 0.0 and is NOT a damage claim: main-title loss is
        // only computable from the mapfile at end-of-recovery, which an
        // interrupted run never reaches. `halted: true` is the flag that says
        // this result is partial.
        let bad_sectors = bad_sector_count(last_unreadable, last_pending);
        return Ok(MultipassResult {
            unreadable_bytes: last_unreadable,
            pending_bytes: last_pending,
            good_bytes: last_good,
            main_lost_ms: 0.0,
            severity: classify_damage(bad_sectors, 0.0),
            passes,
            aborted_for_loss: false,
            halted: true,
            complete: false,
        });
    }

    // ── End-of-recovery promotion + abort-on-loss gate. ──
    let (main_lost_ms, main_lost_bytes, good_bytes, unreadable_bytes, pending_bytes) =
        match Mapfile::load(&mapfile_path) {
            Ok(mut map) => {
                // Promotion is what MAKES the loss visible: the abort gate a
                // few lines down reads Unreadable ranges only, so a range that
                // fails to promote out of NonTrimmed is not counted as lost —
                // it silently drops out of the very decision it should be
                // driving. Logging and carrying on therefore turns a write
                // error into a rip delivered as good.
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
                // Same fail-safe the unreadable-mapfile branch below uses: if
                // the damage record is incomplete, the loss is unquantifiable,
                // and NaN makes `loss_aborts` fire regardless of threshold
                // rather than delivering a possibly-lossy rip as perfect.
                let (lost_ms, unquantifiable) = end_of_recovery_lost_ms(
                    promotion_intact,
                    opts.is_iso_output,
                    main_title,
                    &bad_ranges,
                    disc,
                    lost_bytes,
                );
                if let Some(why) = unquantifiable {
                    sink.log(Level::Error, why);
                }
                (
                    lost_ms,
                    lost_bytes,
                    stats.bytes_good,
                    stats.bytes_unreadable,
                    stats.bytes_pending,
                )
            }
            Err(_) => {
                // Fail-safe (mirrors autorip): the mapfile — the rip's only
                // damage record — could not be read at the abort-decision
                // point. Report the loss as unquantifiable (NaN) so
                // `loss_aborts` fires regardless of threshold, rather than
                // silently delivering a possibly-lossy rip as perfect.
                sink.log(
                    Level::Error,
                    "multipass_rip: mapfile could not be loaded to verify loss — forcing abort",
                );
                (f64::NAN, 0, last_good, last_unreadable, last_pending)
            }
        };

    let effective_abort = effective_abort_secs(opts.is_iso_output, opts.abort_on_lost_secs);
    let aborted_for_loss = loss_aborts(main_lost_bytes, main_lost_ms, effective_abort);
    let bad_sectors = bad_sector_count(unreadable_bytes, pending_bytes);
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
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `abort_lost_ms` must never answer "0 ms lost" when loss exists but
    /// cannot be measured — 0.0 reads to `should_abort_for_loss` as "within
    /// tolerance", and a configured seconds tolerance then delivers a damaged
    /// rip as good. Every sibling here (`main_title_lost_ms`,
    /// `classify_damage`, `bytes_bad_in_title_from_mapfile`) fails safe; this
    /// one did not.
    ///
    /// Not reachable from today's callers (autorip guards one site and feeds a
    /// fallback bitrate at the other), so this pins an exported API against a
    /// future front-end — and against the mutation run, which replaced this
    /// whole body with a constant and kept the suite green.
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

        // The scope hole: a title with NO EXTENTS cannot be scoped, so
        // `bytes_bad_in_title` answers 0 — indistinguishable from "clean".
        // With a perfectly good bitrate, this is the case a naive
        // `lost_bytes == 0 -> 0.0` guard would wave through.
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

    /// The LIVE abort gate must not answer "0 ms lost" for damage it cannot
    /// measure.
    ///
    /// The previous round put this guard in `abort_lost_ms` — which has no
    /// production callers. `multipass_rip_inner` hand-rolls
    /// `abort_lost_bytes` + `main_title_lost_ms` instead, and that pair had the
    /// hole: an extents-less title makes `bytes_bad_in_title` return 0, so
    /// `main_title_lost_ms` returns 0.0 on its FIRST line and never reaches its
    /// own NaN branch. Under a non-zero tolerance that ships a damaged rip as
    /// clean. Reachable because `preflight` is advisory (see the note at the
    /// top of `multipass_rip_inner`) and the gate falls back to
    /// `DiscTitle::empty()` when the disc reports no titles.
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

        // Now the GATE'S OWN decision function — not the predicate in
        // isolation. An earlier version of this test asserted only
        // `loss_is_unscopable(..)`, and it passed with the guard deleted from
        // the gate, because the bug was never in the predicate: it was that
        // the gate did not consult one. Call what the gate calls.
        let disc = disc_with(vec![empty.clone()]);
        let (lost_ms, why) = end_of_recovery_lost_ms(
            /* promotion_intact */ true, /* is_iso */ false, &empty, &damage, &disc,
            /* lost_bytes, as abort_lost_bytes computed it */ 0,
        );
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

    /// A disc carrying exactly these titles.
    fn disc_with(titles: Vec<libfreemkv::DiscTitle>) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
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

    /// The gate must still produce a real number when the loss IS measurable —
    /// the guard must not swallow the normal path.
    #[test]
    fn the_gate_still_quantifies_a_measurable_loss() {
        let mut t = test_title(0, 100);
        t.size_bytes = 1_000_000;
        t.duration_secs = 100.0;
        let disc = disc_with(vec![t.clone()]);
        let (ms, why) = end_of_recovery_lost_ms(true, false, &t, &[(0, 4096)], &disc, 100_000);
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
        let disc = disc_with(vec![t.clone()]);
        let (ms, why) = end_of_recovery_lost_ms(false, false, &t, &[], &disc, 0);
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

    /// `main_title_lost_ms`'s bitrate guard, pinned in every direction.
    ///
    /// The mutation run flipped `t.size_bytes > 0 && t.duration_secs > 0.0` to
    /// `true`, to `||`, and each `>` to `>=`, and the suite stayed green — so
    /// nothing constrained the difference between "quantify it" and "admit we
    /// cannot". Getting that wrong in the permissive direction divides by zero
    /// and yields inf or NaN by accident rather than by decision.
    #[test]
    fn lost_ms_needs_both_a_size_and_a_duration() {
        let damage = 4096u64;

        // Both present -> a real number.
        let mut ok = libfreemkv::DiscTitle::empty();
        ok.size_bytes = 1_000_000;
        ok.duration_secs = 100.0;
        let ms = main_title_lost_ms(&disc_with(vec![ok]), damage);
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
            let ms = main_title_lost_ms(&disc_with(vec![t]), damage);
            assert!(ms.is_nan(), "{name}: expected NaN, got {ms}");
        }

        // Exactly zero is NOT usable — `>=` would admit it and divide by zero.
        let mut zero_size = libfreemkv::DiscTitle::empty();
        zero_size.size_bytes = 0;
        zero_size.duration_secs = 100.0;
        assert!(main_title_lost_ms(&disc_with(vec![zero_size]), damage).is_nan());

        let mut zero_dur = libfreemkv::DiscTitle::empty();
        zero_dur.size_bytes = 1_000_000;
        zero_dur.duration_secs = 0.0;
        assert!(main_title_lost_ms(&disc_with(vec![zero_dur]), damage).is_nan());

        // No titles at all.
        assert!(main_title_lost_ms(&disc_with(vec![]), damage).is_nan());
        // And no damage is genuinely zero regardless of the title.
        assert_eq!(main_title_lost_ms(&disc_with(vec![]), 0), 0.0);
    }

    /// `abort_lost_ms`'s arithmetic, pinned so the operators cannot drift.
    /// The run mutated `/` to `*`/`%` and `*` to `+`/`/` in the conversion and
    /// nothing failed — the ms figure feeds the abort gate, so a wrong operator
    /// is a wrong abort decision.
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
        let disc = libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![t],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        // 10% of the title bad → 10% of 100s = 10s = 10_000 ms.
        assert!((main_title_lost_ms(&disc, 100_000) - 10_000.0).abs() < 1e-6);
        // No loss → 0.
        assert_eq!(main_title_lost_ms(&disc, 0), 0.0);
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
    //    autorip keeps its own characterization coverage exercising these same
    //    fns through its call path; these are the engine's own unit coverage
    //    now that it owns the implementation. ──

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

    /// An unquantifiable loss must classify as SERIOUS, not Cosmetic.
    ///
    /// Every NaN comparison in Rust is false, so before this both tier tests
    /// (`>= 30_000.0`, `>= 1_000.0`) fell through and a rip whose damage
    /// record could not be read was badged Cosmetic — while `loss_aborts`,
    /// which handles NaN explicitly, was simultaneously refusing to deliver
    /// it. The two halves of the same decision disagreed.
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

    /// The pass count must not overflow at the top of the u8 range.
    ///
    /// `multipass_rip_inner` clamps `max_passes` with `.min(u8::MAX as u32)`,
    /// so 255 is reachable by construction. `max_retries + 2` then panicked in
    /// dev and, with debug-assertions off in release, wrapped to 1 — leaving
    /// the UI a total-pass denominator smaller than the number of passes about
    /// to run. Saturating is the only answer that is correct in both profiles.
    #[test]
    fn the_pass_count_saturates_instead_of_wrapping() {
        let plan = plan_passes(u8::MAX);
        assert_eq!(plan.patch_passes, u8::MAX);
        assert_eq!(
            plan.total_passes,
            u8::MAX,
            "total_passes wrapped: in release this silently becomes 1"
        );
        assert!(plan.total_passes >= plan.patch_passes);
        // The ordinary case is unchanged.
        assert_eq!(plan_passes(5).total_passes, 7);
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
        // A title with loss but no measurable bitrate → NaN (fail-safe abort).
        let disc = libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![libfreemkv::DiscTitle::empty()], // size 0, dur 0
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        assert!(main_title_lost_ms(&disc, 4096).is_nan());
    }

    // ─────────────────────────────────────────────────────────────────────
    // multipass_rip — the composed strategy LOOP, exercised headlessly
    // against synthetic `SectorSource`s (hard rule #2: no live drive).
    // ─────────────────────────────────────────────────────────────────────

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
            Ok(count as usize)
        }
        fn capacity_sectors(&self) -> u32 {
            self.capacity
        }
    }

    /// One deliberately-bad single-sector LBA: fails every read that
    /// overlaps it until it has been touched `heal_after` times, then reads
    /// clean forever after. `heal_after: 1` is "recoverable on the very
    /// first re-read" (Pass 1's own touch is attempt #1 and fails; Pass 2's
    /// re-read is attempt #2 and succeeds). `heal_after: u32::MAX` never
    /// heals within a test — permanent loss. Reports `SENSE_KEY_
    /// RECOVERED_ERROR` (a real "distrust and re-read" sense, not a
    /// transport/wedge fault) so Pass 1 marks it NonTrimmed via a plain
    /// `SkipBlock` — no 30s zone-entry cooldown, no wedge escalation —
    /// keeping the test fast.
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
            Ok(count as usize)
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
        // the patch pass recovers it, the muxable scope goes to 0 bad bytes,
        // and the NEXT loop-top check (Converged) stops the loop early —
        // well under the 5-patch-pass cap.
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
        // A sector that NEVER heals: Pass 1 marks it NonTrimmed, the one
        // patch pass recovers nothing (NoProgress -> stop retrying early),
        // end-of-recovery promotion turns it Unreadable, and — with a
        // whole-disc (ISO) scope and a zero-tolerance threshold — the
        // abort-on-loss gate fires.
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

        // NoProgress stops the retry loop before the 5-pass cap is reached:
        // the first patch pass still recovers the bad sector's readable ECC-
        // block neighbours (real progress), so NoProgress fires one pass
        // later than the target sector itself heals — pin "stopped early",
        // not the exact pass count that depends on the ECC block size.
        assert!(
            result.passes > 1 && result.passes < 1 + opts.max_passes,
            "expected the retry loop to exhaust progress before the pass cap, got {} passes",
            result.passes
        );
        // And the exact count, because the loose range above is what let the
        // loop-bottom `== PatchDecision::NoProgress` be mutated to `!=` with
        // the suite still green: `!=` breaks out after the FIRST patch pass
        // (which does make ECC-neighbour progress, so the decision is
        // `Continue`), giving 2 passes — still inside the range. Pass 1 heals
        // the neighbours, pass 2 recovers nothing and is the one that must
        // stop the loop.
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
        // Two bad sectors: one heals on the very next touch (so the patch
        // pass DOES recover bytes — NoProgress never fires), the other never
        // heals (so the muxable scope never reaches 0 — Converged never
        // fires either). With max_passes == 1, only ONE patch pass is
        // allowed to run; the `for` loop must stop there purely because the
        // pass budget is exhausted, not because of either strategy gate.
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
        // A permanently-bad sector OUTSIDE the muxed title's extents. For an
        // MKV/M2TS deliverable (is_iso_output=false) that byte is out of
        // scope: the loop-top `scope_bad_bytes` check sees 0 bad bytes in
        // scope and converges immediately, without ever calling
        // `recovery::patch`. For an ISO deliverable (is_iso_output=true) the
        // SAME byte counts (whole-disc scope): the loop grinds a patch pass,
        // makes no progress, and the promoted loss aborts.
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
            // See the permanent-loss test above for why this isn't pinned to
            // an exact pass count: the ECC block's readable neighbours give
            // the first patch pass real progress before NoProgress fires.
            assert!(
                result.passes > 1 && result.passes < 1 + opts.max_passes,
                "whole-disc scope must see the bad byte and run at least one patch pass \
                 before NoProgress stops the retry loop, got {} passes",
                result.passes
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
