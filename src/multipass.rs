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
/// statuses the loop applies. The patch loop — `recovery::patch` in this crate
/// since 1.6.0, not libfreemkv — intentionally defers this final-verdict step
/// to the orchestrator: a range that is still `NonTrimmed` after pass N may
/// still read on pass N+1, so only the loop that knows there is no pass N+1 can
/// call it lost.
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
/// Shared by every `MultipassResult` exit path so the byte→sector conversion
/// cannot drift between them. Rounds down: a partial sector of damage is still
/// less than one sector.
///
/// The second term is RETRYABLE bytes, not `bytes_pending`. They are not the
/// same thing: `bytes_pending` also counts `NonTried` — the un-attempted
/// remainder of the disc ahead of the sweep head — so on any interrupted run
/// it is dominated by sectors nobody has looked at yet. Feeding that in scored
/// a rip cancelled ten seconds into a 66 GB disc as ~32 million bad sectors
/// and stamped it Serious, when what was actually known was zero damage.
/// `run.rs`'s live progress tick already refuses that aggregate for the same
/// reason and says so; this is the same rule, applied to the same number, at
/// the other end of the run.
fn bad_sector_count(unreadable_bytes: u64, retryable_bytes: u64) -> u64 {
    unreadable_bytes.saturating_add(retryable_bytes) / SECTOR_BYTES
}

/// [`bad_sector_count`] for the FINAL verdict, taken from the mapfile's own
/// split so the caller cannot pick the wrong field.
///
/// `MapStats` publishes three overlapping totals and only one of them is
/// damage: `bytes_pending` = `bytes_nontried` + `bytes_retryable`, and
/// `bytes_nontried` is disc nobody has attempted, not disc that failed. The
/// end-of-recovery gate read `bytes_pending`, which is the aggregate
/// `bad_sector_count`'s own doc forbids — so any state that reaches the gate
/// with un-attempted sectors would be scored as though every one of them were
/// a bad sector. (Today the sweep leaves none there; that is a property of the
/// pass order, not of this arithmetic, and it is not the sort of thing a
/// severity badge should depend on.)
fn end_of_recovery_bad_sectors(stats: &MapStats) -> u64 {
    bad_sector_count(stats.bytes_unreadable, stats.bytes_retryable)
}

/// The retryable-bytes argument for an exit that did NOT finish the sweep.
///
/// A `CopyResult` carries one `bytes_pending` total and cannot say how much of
/// it is retryable damage versus never-attempted disc, so an interrupted path
/// has nothing honest to pass. Zero, and the name says why: the unreadable
/// count is what is actually KNOWN, and it still scores — a cancel that had
/// already found 300 MB unreadable does not come back Clean.
const UNMEASURED_ON_AN_INTERRUPTED_PASS: u64 = 0;

/// How a finished patch pass ends the loop, if it does.
///
/// A pure function over the two flags a `PatchOutcome` carries, because the
/// branch that consumes them needs a live USB-bridge crash to reach and so
/// could not be tested where it sits. `wedged_exit` was simply not read at
/// all: a pass killed by a transport fault fell through to the exhaustion
/// gate, ended the recovery, and its never-attempted ranges were promoted to
/// permanently `Unreadable` — after which a re-run took `recovery::copy`'s
/// terminal shortcut and never retried them. Recoverable sectors, written off
/// on the strength of a flag nobody looked at.
///
/// `halted` wins when both are set: the user asked to stop, and that is the
/// more specific thing to tell them.
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

/// Severity for a run that stopped early, given what it actually measured.
///
/// Two rules meet here and both are load-bearing:
///
/// - The score must not fold in `NonTried`, or a cancel ten seconds into a
///   66 GB disc reports ~32 million bad sectors and stamps Serious on a disc
///   nobody has looked at.
/// - The badge must not read **Clean** beside a non-zero pending count. The
///   front-end draws them together, and Clean is the reading that gets
///   believed — that contradiction is what the halted branch was written to
///   stop, and it stays stopped.
///
/// So: damage is scored from the unreadable bytes alone, and outstanding work
/// merely denies the Clean badge rather than inventing a tier for it.
/// `Cosmetic` is the floor because it is the mildest thing that is not Clean;
/// an interrupted run makes no claim about how bad the rest of the disc is.
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
    /// Whether a pass ended early on a TRANSPORT FAULT — the USB-bridge crash
    /// that `patch` reports as `wedged_exit`.
    ///
    /// Distinct from [`Self::halted`], which is the user pressing Stop, and it
    /// has to be: the ranges a wedged pass never reached are still RETRYABLE,
    /// so the end-of-recovery promotion that writes surviving ranges off as
    /// permanently `Unreadable` must not run. Dropping this signal meant a
    /// bridge crash was recorded as permanent loss, and a later re-run then
    /// took `recovery::copy`'s terminal shortcut (nothing retryable, nothing
    /// un-attempted) and never touched those sectors again.
    ///
    /// The front-end's cue to spin-cycle the drive and resume from the
    /// mapfile, which is exactly what `patch` documents the fault for.
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
    /// recovery abort gate ([`abort_lost_bytes`] for the byte count, then
    /// `end_of_recovery_lost_ms` → `main_title_lost_ms` for the
    /// milliseconds — NOT [`abort_lost_ms`], which no longer sits on that
    /// path) to the
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
        //
        // WHY `bytes_pending` is the right second term HERE, given
        // `bad_sector_count`'s doc forbids the aggregate: on every route that
        // reaches this line un-halted, the pending bytes contain no `NonTried`.
        // `recovery::copy` routes ANY mapfile with `bytes_nontried > 0` to a
        // resume sweep before it can return, and its terminal plain-copy branch
        // is reached only with `nontried == 0` — so a completed single pass has
        // attempted the whole disc and its pending bytes are retryable damage
        // (sweep's skipped `NonTrimmed` blocks), which is exactly what the
        // score wants. The interrupted route, where `NonTried` genuinely
        // dominates, takes `interrupted_severity` below instead.
        let bad_sectors = bad_sector_count(cr.bytes_unreadable, cr.bytes_pending);
        return Ok(MultipassResult {
            unreadable_bytes: cr.bytes_unreadable,
            pending_bytes: cr.bytes_pending,
            good_bytes: cr.bytes_good,
            // Single-pass never runs the end-of-recovery gate that measures
            // main-title loss. A flat 0.0 therefore asserted a measurement
            // that never happened — indistinguishable, to any consumer, from
            // "this damaged disc lost no playback", against a non-zero
            // unreadable count. NaN is this crate's existing and only marker
            // for "could not be quantified" (see the Err-branch fail-safe
            // below, and this field's own doc).
            //
            // A disc with NO bad sectors is the one case that needs no
            // measurement: nothing was lost because nothing was unreadable.
            // Reporting NaN there would mark a perfect rip as unquantified.
            main_lost_ms: if bad_sectors == 0 { 0.0 } else { f64::NAN },
            // Severity still comes from the SECTOR count, which single-pass
            // does know. Passing the NaN here instead would escalate every
            // damaged single-pass rip to Serious, because `classify_damage`
            // treats an unquantifiable loss as fail-safe — right for the
            // abort gate, wrong here, since single-pass has no abort gate
            // (`aborted_for_loss: false`). The 0.0 below is "no time-based
            // escalation", not a claim that nothing was lost.
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

            // Loop-top convergence gate: the muxable scope is already clean
            // in the mapfile — skip remaining passes and proceed to mux.
            //
            // `None` = the mapfile could not be read, so the scope is
            // UNMEASURED. It used to fall back to the in-flight counters, which
            // silently turned a failed read of the rip's only damage record
            // into a measurement: on a run whose carried counters are both zero
            // the fallback is 0, `patch_pass_decision(0, None)` answers
            // `Converged`, and the loop announces "muxable scope 100%
            // recovered" and skips every remaining pass on the strength of a
            // load that failed. Unmeasured must never converge.
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
            // A transport fault is NOT an exhausted pass. The bridge died
            // mid-range, so everything it had not reached is still retryable —
            // and falling through to the loop's exhaustion gate would end the
            // recovery, promote those ranges to permanently Unreadable, and
            // make a later re-run skip them entirely. Return partial, like a
            // cancel, and say which of the two it was.
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
            // Loop-bottom exhaustion gate, evaluated against the SAME
            // pre-pass `mux_scope_bad` the top-of-loop check used (see
            // `patch_pass_decision`'s doc): a pass that recovered nothing
            // won't be helped by another pass with the same drive state.
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
    // `bad_sectors` is carried out of the match rather than derived after it:
    // the two branches score from different evidence. The Ok branch has the
    // mapfile's own damage/un-attempted split (`end_of_recovery_bad_sectors`);
    // the Err branch has only in-flight counters that cannot be split.
    let (main_lost_ms, main_lost_bytes, good_bytes, unreadable_bytes, pending_bytes, bad_sectors) =
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
                    end_of_recovery_bad_sectors(&stats),
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
                // No `MapStats` to split, so the score keeps the whole
                // in-flight aggregate — deliberately, because this is the
                // fail-safe path and it must over-report rather than
                // under-report. (It cannot lean on the NaN to escalate either:
                // `classify_damage` answers Clean for zero bad sectors BEFORE
                // it looks at `lost_ms`.)
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

    /// Every field a front-end can read off a [`MultipassResult`] must be
    /// documented where a front-end will look for it.
    ///
    /// `USING_THE_ENGINE.md` is the GUI contract, and its §4 field list is
    /// what an implementer builds the result page from. The list was
    /// presented as complete for as long as there have been two more fields
    /// than it names: `wedged` — whose own rustdoc says it exists to be the
    /// front-end's cue to power-cycle the drive and resume — and `complete`,
    /// the only field that says whether the rip actually finished. A GUI
    /// following the list verbatim renders a bridge crash as an ordinary
    /// partial rip and has no correct test for "done".
    ///
    /// Derived from the SOURCE, not from a hand-kept list here, so adding a
    /// field without documenting it fails this test rather than quietly
    /// repeating the same omission. Mirrors `preflight.rs`'s
    /// `every_emitted_reason_key_is_documented`.
    #[test]
    fn every_multipass_result_field_is_documented() {
        let src = include_str!("multipass.rs");
        let guide = include_str!("../USING_THE_ENGINE.md");
        // The struct body: from its declaration to the closing brace in
        // column 0. `MultipassResult` has no `impl` block to terminate on
        // (unlike `Reason`), and no field type or doc line inside it
        // contains a brace, so the first `\n}` is the end.
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
            // `mp.<field>` — the notation the guide's own example
            // establishes (`let mp: MultipassResult = multipass_rip(...)`).
            // Matching the bare word would let "completed" (an unrelated
            // sink method in §1) pass for `complete`.
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
        // (No `total_passes >= patch_passes` check here: both operands were
        // just pinned to the same literal `u8::MAX`, so it could only ever read
        // `255 >= 255`. An assertion that cannot fail is noise in a test whose
        // subject is an arithmetic overflow.)
        // The ordinary case is unchanged.
        assert_eq!(plan_passes(5).total_passes, 7);
    }

    // ── A cancelled or wedged pass has not MEASURED the disc ──────────────
    //
    // `bytes_pending` counts NonTried — the un-attempted remainder ahead of
    // the sweep head — so folding it into the damage score made an early
    // interruption look like catastrophic damage.

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

    /// A transport fault is not an exhausted pass, and must not be reported
    /// as a cancel either — the two need different responses from the caller.
    ///
    /// This test used to assert `wedged.wedged && !wedged.halted` over a
    /// `MultipassResult` written out by hand in the test body. It executed no
    /// production code at all: the `PassExit::Wedged` arm — the one that
    /// returns early so the never-reached ranges are NOT promoted to
    /// permanently `Unreadable` — was unreached by the whole suite, and
    /// deleting it left every test green. It now drives a real
    /// `multipass_rip` whose patch pass dies on a bridge crash.
    #[test]
    fn a_wedged_result_is_distinguishable_from_a_cancelled_one() {
        /// Marginal (RECOVERED) errors at `bad_lba` while the sweep is walking forward, then
        /// a TRANSPORT FAILURE (status 0xFF — the USB bridge crashing) once the
        /// sweep has reached the end of the disc and the patch pass comes back
        /// for the range it left behind.
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
                            // RECOVERED (marginal), which the sweep marks
                            // NonTrimmed for Pass N without the 30 s
                            // damage-zone cooldown a hard error would earn —
                            // the pass this test is about is the patch one.
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

    /// Every test double in this module must honour the contract it is
    /// standing in for: `SectorSource::read_sectors` returns the number of
    /// BYTES written into `buf`, not the number of sectors. All three doubles
    /// here filled `count * 2048` bytes and then returned `count` — a lie that
    /// costs nothing while every in-crate consumer matches `Ok(_)`, and costs a
    /// silently desynced stream the moment one is handed to a consumer that
    /// believes the number (libfreemkv's own `PrefetchedSectorSource` advances
    /// its cursor by `n / 2048`).
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

    /// The only single-pass test ran a CLEAN disc, where `main_lost_ms: 0.0`
    /// is correct and therefore indistinguishable from a hard-coded constant.
    /// On a DAMAGED disc the constant was a claim nothing had measured: a rip
    /// returning `complete: false` and a non-zero unreadable count also
    /// reported, in the same breath, that no playback was lost.
    ///
    /// Single-pass never runs the gate that measures loss, so it must say so.
    #[test]
    fn single_pass_reports_loss_as_unquantified_on_a_damaged_disc() {
        let (dir, iso) = scratch_iso("single-damaged");
        let sectors = 4096u32;
        let disc = test_disc(sectors, vec![test_title(0, sectors)]);
        let total = sectors as u64 * 2048;

        // The reachable damaged-single-pass state is the RESUME one: a plain
        // copy aborts at the first read error, so damage only comes back as a
        // RESULT when a prior run already attempted the whole disc. That is
        // exactly what a user hits re-running after a failed rip. Build that
        // mapfile: fully attempted, some sectors permanently Unreadable.
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
        // Severity still comes from the sector count, which single-pass DOES
        // know. It must not have been escalated to Serious merely because the
        // loss is unquantified — that rule belongs to the abort gate, which
        // single-pass never runs.
        //
        // The expected badge is spelled out, not recomputed. This used to
        // assert against
        // `classify_damage(bad_sector_count(r.unreadable_bytes, r.pending_bytes), 0.0)`
        // — the same two functions the producer calls, on the same inputs — so
        // both sides moved together and it pinned nothing but self-agreement:
        // dropping the byte→sector division inside `bad_sector_count` (which
        // turns this disc's badge from Cosmetic into Serious) left it green.
        // The fixture records 8 sectors' worth of damage (8 * 2048 = 16384
        // bytes) and
        // nothing pending, and the tiers are ">= 500 sectors → Serious,
        // >= 51 → Moderate", so the honest badge is Cosmetic — and Cosmetic is
        // also what a NaN could never produce (a NaN fails safe to Serious).
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
            // Pinned exactly, for the reason the permanent-loss test above
            // spells out: the loose `passes > 1 && passes < 1 + max_passes`
            // range this used to carry let the loop-bottom
            // `== PatchDecision::NoProgress` be mutated to `!=` — which breaks
            // out after the FIRST patch pass (2 passes, still inside the
            // range) — with the suite green. Pass 1 sweeps, patch pass 1 heals
            // the bad sector's readable ECC-block neighbours (real progress),
            // patch pass 2 recovers nothing and is the pass that must stop the
            // loop.
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

    /// The final verdict must score DAMAGE, never un-attempted disc.
    ///
    /// `MapStats.bytes_pending` is `bytes_nontried + bytes_retryable`, and the
    /// end-of-recovery gate used it. The stats below are a 66 GB disc on which
    /// nothing has failed and 64 GB has never been attempted: the pending
    /// reading is ~33 million bad sectors — `Serious`, the badge that tells an
    /// operator to bin the disc — where the truth is zero.
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

    /// An UNMEASURED muxable scope must never read as a converged one.
    ///
    /// The loop-top gate loads the mapfile to ask "is the muxable scope clean
    /// yet?". When that load fails the answer is unknown — and the fallback it
    /// used to substitute could be zero, which is the ONE value that means
    /// "converged, stop retrying". A failed read of the rip's only damage
    /// record then ended the recovery and logged "muxable scope 100%
    /// recovered".
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

    // ─────────────────────────────────────────────────────────────────────
    // The three fail-safes inside `multipass_rip_inner` that no black-box
    // fixture reached: the failed end-of-recovery promotion, the unreadable
    // mapfile at the abort-decision point, and the mid-loop cancel.
    //
    // All three need the world to change WHILE the loop is running, which is
    // what `HookSink` is for: the loop's own log lines are the only
    // deterministic clock a caller has inside a `multipass_rip` call.
    // ─────────────────────────────────────────────────────────────────────

    /// A `Sink` that records every line the loop logs and, the first time a
    /// line contains `trigger`, runs `action` (and optionally starts
    /// cancelling). The trigger points used below are chosen so the action
    /// lands in a gap where the loop is doing no I/O of its own:
    ///
    /// * `"exhausted"` / `"skipping remaining"` — logged immediately before
    ///   the `break` that leaves the patch loop, so a sabotage takes effect
    ///   for the end-of-recovery `Mapfile::load` and nothing else.
    /// * `"multipass_rip: pass "` — logged right after a patch pass returns
    ///   and before the next iteration's cancel check, which is the only way
    ///   to arm a cancel that the *loop-top* check (and not a recovery
    ///   primitive's own halt token) is guaranteed to observe first.
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

    /// A 4096-sector disc whose ONE title's extents span the whole image, so
    /// an MKV-scoped (`is_iso_output: false`) gate sees the damage at LBA
    /// 1000 and the loop actually runs patch passes. Returns the scratch dir,
    /// the ISO path, the mapfile path the loop will use, and the disc.
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

    /// A generous tolerance: the real residual loss on the fixture above is
    /// ~35 s of a 7200 s title, so an HOUR of tolerance accepts it. Every
    /// abort asserted below therefore comes from the fail-safe under test and
    /// from nothing else.
    const GENEROUS_TOLERANCE_SECS: u64 = 3600;

    /// CONTROL for the two sabotage tests below: the same disc, the same
    /// damage, the same tolerance, with the mapfile left alone. The gate
    /// quantifies the loss, accepts it, and does NOT abort. Without this the
    /// sabotage tests could pass for the wrong reason (any run of this fixture
    /// aborting).
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

    /// The LIVE end-of-recovery gate must abort when the mapfile — the rip's
    /// only damage record — cannot be read at the abort-decision point.
    ///
    /// `end_of_recovery_lost_ms` is tested as a pure predicate, but the
    /// shipped bug (see its doc) was that the GATE did not consult a
    /// predicate. This drives `multipass_rip` end to end and replaces the
    /// mapfile with a directory at the instant the patch loop breaks, so
    /// `Mapfile::load` at the gate returns `Err`. The fail-safe answers NaN,
    /// which `loss_aborts` treats as abort under EVERY threshold — including
    /// the hour-long one that the control test above proves accepts this
    /// disc's real loss.
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

    /// A failed end-of-recovery PROMOTION must abort the rip.
    ///
    /// Promotion is what makes the loss visible: the gate reads `Unreadable`
    /// ranges only, so a range that fails to promote out of `NonTrimmed`
    /// silently drops out of the decision it should be driving — and the rip
    /// ships as good. Here `Mapfile::load` SUCCEEDS (so this is not the
    /// unreadable-mapfile branch above — asserted) but the promoted map
    /// cannot be persisted, because `write_to_disk` writes `<mapfile>.tmp`
    /// first and that path has been replaced by a directory. `flush()` fails,
    /// `promotion_intact` goes false, and the loss becomes unquantifiable.
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

    /// A rip cancelled mid-loop, AFTER damage has been found, is halted and is
    /// never reported as Clean.
    ///
    /// Two untested things at once, because they are one user-visible event:
    /// the loop-top cancel check (which breaks out between patch passes) and
    /// the halted result branch. Severity there was once hard-coded `Clean`,
    /// so a cancelled rip that had already found unreadable sectors rendered a
    /// "Clean" badge next to a non-zero unreadable count.
    ///
    /// The cancel is armed from the "pass N recovered" log line, i.e. after
    /// the first patch pass has returned and before the second iteration's
    /// cancel check — the only window in which the LOOP's own check, rather
    /// than a recovery primitive's halt token, is guaranteed to be the thing
    /// that stops the run. Hence the exact `passes == 2`.
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
        // The expectation is the literal tier, not a re-run of a formula.
        // It used to be `classify_damage(bad_sector_count(unreadable,
        // pending), 0.0)` — which is NOT what the halted exit computes (that
        // is `interrupted_severity`, and it deliberately does not fold pending
        // in). The two agree only because this fixture ends with a single
        // pending sector; the test therefore pinned the formula the code was
        // changed to stop using, and reverting production to it would have
        // stayed green. `a_cancel_with_a_wide_pending_region_is_not_scored_
        // from_it` is the fixture where the two disagree.
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

    /// The halted exit must score the damage it MEASURED, not the work it had
    /// not got to — end to end, on a fixture where the two answers differ.
    ///
    /// The mid-loop cancel test above ends with one pending sector, where
    /// "unreadable only" and "unreadable + pending" both come out Cosmetic, so
    /// neither formula could be told from the other. Here the pass leaves a
    /// wide unrecovered region: folding pending in scores >= 500 bad sectors
    /// and stamps **Serious** on a rip that confirmed no loss at all, which is
    /// the badge the front-end draws next to the counters.
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
