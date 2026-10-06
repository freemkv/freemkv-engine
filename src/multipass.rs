//! The multipass rip STRATEGY: sweep → N patch passes → abort-on-loss gate.
//!
//! [`crate::recovery::copy`] performs ONE dispatch step (sweep, one patch
//! pass, or a terminal result) chosen from mapfile state; this module's loop
//! calls it repeatedly until the disc is clean or progress stalls, then
//! applies the abort-on-loss gate ([`loss_aborts`], hard rule #6):
//! `abort_on_lost_secs == 0` requires a perfect rip, and a positive value
//! tolerates that many seconds of loss; an untimeable loss never exceeds it.

use crate::job::Job;
use crate::recovery::mapfile::{MapStats, Mapfile, SectorStatus};
use crate::recovery::{CopyOptions, PatchOptions, SweepOptions};
use crate::run::ProgressBridge;
use crate::sink::{Level, RecoveryEvent, Sink};

/// Milliseconds per second — the byte-loss→time conversion base.
const MILLIS_PER_SEC: f64 = 1000.0;

/// Bytes in one optical sector — the unit damage is scored in.
pub(crate) const SECTOR_BYTES: u64 = 2048;

// Typical average playback bitrates (bytes/sec) by video class, the estimate a title gets when
// it reports neither a usable size nor a usable duration. Averages, not peaks: a lower rate
// converts the same lost bytes into MORE lost time, so the estimate errs towards reporting loss.
const SD_BYTES_PER_SEC: f64 = 6_000_000.0 / 8.0;
const HD_BYTES_PER_SEC: f64 = 30_000_000.0 / 8.0;
const UHD_BYTES_PER_SEC: f64 = 60_000_000.0 / 8.0;

/// The title's playback rate in bytes/sec, the one conversion between lost bytes and lost time.
///
/// Its own size over its own duration when it has both. A missing size is taken from its
/// extents and a missing duration from its clips; when either is still missing, a typical
/// rate for the title's video (UHD, HD or SD, else its container: a program stream is DVD-class,
/// a transport stream Blu-ray-class). Always finite and positive, so missing metadata never
/// leaves a loss unmeasured.
pub fn title_bytes_per_sec(title: &libfreemkv::DiscTitle) -> f64 {
    let size = if title.size_bytes > 0 {
        title.size_bytes
    } else {
        title
            .extents
            .iter()
            .map(|e| u64::from(e.sector_count) * SECTOR_BYTES)
            .sum()
    };
    let usable = |d: f64| d.is_finite() && d > 0.0;
    let duration = if usable(title.duration_secs) {
        title.duration_secs
    } else {
        title
            .clips
            .iter()
            .map(|c| c.duration_secs)
            .filter(|d| usable(*d))
            .sum()
    };
    if size > 0 && usable(duration) {
        let bps = size as f64 / duration;
        if usable(bps) {
            return bps;
        }
    }
    format_bytes_per_sec(title)
}

// The typical rate for a title's video class, for a title whose size or duration is missing.
fn format_bytes_per_sec(title: &libfreemkv::DiscTitle) -> f64 {
    use libfreemkv::Resolution as R;
    let video = title.streams.iter().find_map(|s| match s {
        libfreemkv::Stream::Video(v) if !v.secondary => Some(v.resolution),
        _ => None,
    });
    match video {
        Some(R::R2160p | R::R4320p) => UHD_BYTES_PER_SEC,
        Some(R::R720p | R::R1080i | R::R1080p) => HD_BYTES_PER_SEC,
        Some(R::R480i | R::R480p | R::R576i | R::R576p) => SD_BYTES_PER_SEC,
        Some(R::Unknown) | None => match title.content_format {
            libfreemkv::ContentFormat::MpegPs | libfreemkv::ContentFormat::DvdPs => {
                SD_BYTES_PER_SEC
            }
            libfreemkv::ContentFormat::BdTs => HD_BYTES_PER_SEC,
        },
    }
}

/// Milliseconds of `title`'s playback that `bad_bytes` of it hold, at [`title_bytes_per_sec`].
pub fn lost_ms_in_title(title: &libfreemkv::DiscTitle, bad_bytes: u64) -> f64 {
    if bad_bytes == 0 {
        return 0.0;
    }
    bad_bytes as f64 / title_bytes_per_sec(title) * MILLIS_PER_SEC
}

/// The engine's one loss verdict over the ripped titles: what was lost and whether it aborts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LossVerdict {
    /// Unreadable bytes under the deliverable's scope: the whole disc for an ISO, inside the
    /// titles for a mux, and the whole disc's when a title has no extents to scope by.
    pub lost_bytes: u64,
    /// Playback milliseconds lost in the titles. NaN when a damaged title has no extents, so
    /// its share of the damage cannot be timed: the loss is then reported in bytes only.
    pub lost_ms: f64,
    /// The loss exceeds the tolerance: `abort_on_lost_secs == 0` aborts on any lost byte, a
    /// positive tolerance on more lost time than that. A loss reported in bytes only never
    /// exceeds a positive tolerance (a rip is never stopped for missing metadata).
    pub aborts: bool,
}

/// The loss verdict for `titles` over the confirmed-unreadable `bad_ranges`, against
/// `abort_on_lost_secs` (already [`effective_abort_secs`] for an ISO).
pub fn loss_verdict(
    is_iso_output: bool,
    titles: &[&libfreemkv::DiscTitle],
    bad_ranges: &[(u64, u64)],
    abort_on_lost_secs: u64,
) -> LossVerdict {
    let unscopable = titles
        .iter()
        .any(|t| loss_is_unscopable(is_iso_output, t, bad_ranges));
    let lost_bytes = if is_iso_output || unscopable {
        bad_ranges.iter().map(|(_, sz)| *sz).sum()
    } else {
        titles_abort_lost_bytes(is_iso_output, titles, bad_ranges)
    };
    let (lost_ms, _) = titles_lost_ms(true, titles, bad_ranges);
    LossVerdict {
        lost_bytes,
        lost_ms,
        aborts: loss_aborts(lost_bytes, lost_ms, abort_on_lost_secs),
    }
}

/// Does the residual loss exceed the tolerance and therefore abort the rip? The gate
/// [`loss_verdict`] applies.
///
/// `abort_on_lost_secs == 0` is byte-exact: any lost byte (or an unquantifiable NaN loss)
/// aborts; exactly zero proceeds. A positive threshold switches to the seconds gate (bytes
/// not consulted), where a NaN loss never aborts (a rip is never stopped for missing metadata).
pub fn loss_aborts(lost_bytes: u64, lost_ms: f64, abort_on_lost_secs: u64) -> bool {
    if abort_on_lost_secs == 0 {
        lost_bytes > 0 || lost_ms.is_nan()
    } else {
        should_abort_for_loss(lost_ms, (abort_on_lost_secs as f64) * MILLIS_PER_SEC)
    }
}

/// The seconds-threshold half of the gate: strictly-greater-than aborts; a NaN
/// (unquantifiable) loss does not.
pub fn should_abort_for_loss(lost_ms: f64, abort_threshold_ms: f64) -> bool {
    lost_ms.is_finite() && lost_ms > abort_threshold_ms
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
/// Shared by [`abort_lost_ms`], [`measured_scope_bad`] and the live gate's
/// [`end_of_recovery_lost_ms`] so they cannot drift.
pub(crate) fn loss_is_unscopable(
    is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
) -> bool {
    !is_iso && title.extents.is_empty() && !bad_ranges.is_empty()
}

/// Milliseconds of playback lost, scoped by [`abort_lost_bytes`] and converted
/// via the title's own bytes/sec bitrate.
///
/// NaN when the loss exists but cannot be measured — see [`loss_is_unscopable`]. A NaN
/// aborts only the perfect (`0`) gate, through its byte count ([`loss_aborts`]).
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

// MULTIPASS STRATEGY DECISIONS: pure pass-ordering, convergence, exhaustion and
// promotion rules the loop below composes.

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
    // Capped so `total_passes` (retries + 2) still fits a u8 and counts every pass.
    let max_retries = max_retries.min(u8::MAX - 2);
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

/// [`scope_bad_bytes`] for the patch loop: `None` (unmeasured, never converges)
/// when the loss is [`loss_is_unscopable`], whose scoped count reads a false 0.
/// With no extents, out-of-title (menu/trailer) damage then also earns passes:
/// fail-safe, and bounded by the loop's no-progress stop.
pub fn measured_scope_bad(
    is_iso: bool,
    bad_ranges: &[(u64, u64)],
    title: &libfreemkv::DiscTitle,
) -> Option<u64> {
    if loss_is_unscopable(is_iso, title, bad_ranges) {
        return None;
    }
    Some(scope_bad_bytes(is_iso, bad_ranges, title))
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
pub(crate) fn patch_pass_decision_measured(
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

/// Loop-top convergence gate for the patch retry loop, guarding the fail-open
/// the bare [`patch_pass_decision_measured`] can't see: `Converged` fires on
/// `scope_bad == 0`, but an EMPTY mapfile (Pass 1 read ZERO bytes) ALSO has
/// zero bad bytes. "Nothing bad recorded" is not "everything good", so require
/// the recovery actually read something (`bytes_good > 0`) first: a complete
/// Pass 1 still converges and skips redundant passes; an empty mapfile (or a
/// `None`/unreadable one, which measured never converges) falls through to run
/// the pass. The server's patch loop calls this same gate.
pub fn pre_pass_converged(mux_scope_bad: Option<u64>, bytes_good: u64) -> bool {
    bytes_good > 0 && patch_pass_decision_measured(mux_scope_bad, None) == PatchDecision::Converged
}

/// The end-of-recovery promotion: after the final patch pass, bytes still in a
/// "maybe" state across every pass are promoted to `Unreadable` (confirmed
/// lost) BEFORE the abort/loss gate reads them. Returns the `(from, to)`
/// statuses the loop applies.
///
/// BOTH maybe-states are promoted (`NonTrimmed` and `NonScraped`), or a surviving maybe-state
/// stays invisible to the abort gate.
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
    // An unquantifiable loss badges Serious: every NaN comparison is false, so
    // without this it fell through both tiers to Cosmetic.
    if lost_ms.is_nan() {
        return Serious;
    }
    if bad_sectors >= SERIOUS_SECTORS || lost_ms >= SERIOUS_LOST_MS {
        return Serious;
    }
    if bad_sectors >= MODERATE_SECTORS || lost_ms >= MODERATE_LOST_MS {
        return Moderate;
    }
    Cosmetic
}

// The damage tiers' thresholds (documented on `DamageSeverity`).
const SERIOUS_SECTORS: u64 = 500;
const SERIOUS_LOST_MS: f64 = 30_000.0;
const MODERATE_SECTORS: u64 = 51;
const MODERATE_LOST_MS: f64 = 1_000.0;

// Whether a recovery pass decrypts in place, given the job's `raw` flag. Named so the
// `!job.raw` policy shared by four call sites reads as a decision, not a stray `!`.
pub(crate) fn pass_should_decrypt(raw: bool) -> bool {
    !raw
}

// Bad bytes expressed in whole sectors, the unit `classify_damage` scores. Rounds down.
// `retryable_bytes` must be RETRYABLE, never `bytes_pending` (which also counts un-attempted
// `NonTried`).
fn bad_sector_count(unreadable_bytes: u64, retryable_bytes: u64) -> u64 {
    unreadable_bytes.saturating_add(retryable_bytes) / SECTOR_BYTES
}

// `bad_sector_count` for the FINAL verdict, taken from the mapfile's own split so the caller
// cannot pick the field that folds in un-attempted disc (`bytes_pending`).
fn end_of_recovery_bad_sectors(stats: &MapStats) -> u64 {
    bad_sector_count(stats.bytes_unreadable, stats.bytes_retryable)
}

// The retryable-bytes argument for an exit that did NOT finish the sweep.
// A `CopyResult` can't split retryable damage from never-attempted disc, so
// zero is the honest answer — the unreadable count alone is what is KNOWN.
const UNMEASURED_ON_AN_INTERRUPTED_PASS: u64 = 0;

/// How a finished patch pass ends the loop, if it does.
///
/// A pure function over the two flags a `PatchOutcome` carries: `halted` (the user pressed
/// Stop) and `wedged_exit` (a transport fault mid-pass). `halted` wins when both are set — the
/// more specific thing to tell the user.
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

// Severity for a run that stopped early: scored from unreadable bytes ALONE (never `NonTried`),
// with a non-zero pending count denying the Clean badge rather than inventing a tier for it.
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

// The loss figure for a run that stopped before the gate measured it: genuinely 0.0 only when
// nothing is unreadable or pending, else NaN (unmeasured), as single-pass reports it.
fn interrupted_lost_ms(unreadable_bytes: u64, pending_bytes: u64) -> f64 {
    if unreadable_bytes == 0 && pending_bytes == 0 {
        0.0
    } else {
        f64::NAN
    }
}

// A recovery is complete only when the abort-on-loss gate did NOT fire and the mapfile shows
// zero unreadable and zero pending bytes. `aborted_for_loss` is load-bearing on its own.
fn recovery_is_complete(aborted_for_loss: bool, unreadable_bytes: u64, pending_bytes: u64) -> bool {
    !aborted_for_loss && unreadable_bytes == 0 && pending_bytes == 0
}

// Milliseconds of main-title playback lost, at `title`'s [`title_bytes_per_sec`]. `title` must
// be the title `main_bad_bytes` was scoped to.
fn main_title_lost_ms(title: &libfreemkv::DiscTitle, main_bad_bytes: u64) -> f64 {
    lost_ms_in_title(title, main_bad_bytes)
}

/// The end-of-recovery loss figure, plus the reason it is unquantifiable when
/// it is. `None` means the number is trustworthy. Pure and separate from
/// [`recover`] deliberately, so it can be tested without a drive.
///
/// SCOPE — ALWAYS main-title-scoped, whatever the deliverable is: it derives its own byte count
/// from `title` + `bad_ranges` rather than accepting the ABORT GATE's count
/// ([`abort_lost_bytes`]), which is whole-disc for an ISO deliverable.
pub(crate) fn end_of_recovery_lost_ms(
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

// The titles this rip delivers (`Job::selection`), whose damage the loop measures. Never empty:
// with none, the extent-less `empty` title makes any damage unmeasurable, not clean.
fn measured_titles<'a>(
    disc: &'a libfreemkv::Disc,
    job: &Job,
    empty: &'a libfreemkv::DiscTitle,
) -> Vec<&'a libfreemkv::DiscTitle> {
    let picked: Vec<_> = crate::mux::resolve_selection(disc, &job.selection)
        .into_iter()
        .filter_map(|i| disc.titles.get(i))
        .collect();
    if picked.is_empty() {
        vec![empty]
    } else {
        picked
    }
}

// [`measured_scope_bad`] over every measured title: `None` if any is unscopable. ISO scope is
// whole-disc (the same count for each title), so it is taken once, not summed.
fn titles_scope_bad(
    is_iso: bool,
    bad_ranges: &[(u64, u64)],
    titles: &[&libfreemkv::DiscTitle],
) -> Option<u64> {
    let mut total = 0u64;
    for t in titles {
        let bad = measured_scope_bad(is_iso, bad_ranges, t)?;
        total = if is_iso {
            bad
        } else {
            total.saturating_add(bad)
        };
    }
    Some(total)
}

// [`abort_lost_bytes`] over every measured title, with the same ISO rule as `titles_scope_bad`.
fn titles_abort_lost_bytes(
    is_iso: bool,
    titles: &[&libfreemkv::DiscTitle],
    bad_ranges: &[(u64, u64)],
) -> u64 {
    titles.iter().fold(0u64, |total, t| {
        let bad = abort_lost_bytes(is_iso, t, bad_ranges);
        if is_iso {
            bad
        } else {
            total.saturating_add(bad)
        }
    })
}

// [`end_of_recovery_lost_ms`] summed over every measured title; the first unquantifiable one
// makes the whole figure NaN.
fn titles_lost_ms(
    promotion_intact: bool,
    titles: &[&libfreemkv::DiscTitle],
    bad_ranges: &[(u64, u64)],
) -> (f64, Option<&'static str>) {
    let mut total = 0.0;
    for t in titles {
        let (ms, why) = end_of_recovery_lost_ms(promotion_intact, t, bad_ranges);
        if why.is_some() {
            return (ms, why);
        }
        total += ms;
    }
    (total, None)
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
    /// Playback milliseconds lost in the ripped titles: those
    /// [`Job::selection`] resolves to, summed per title (title 0 for the default `MainMovie`).
    ///
    /// Always scoped to those titles' own extents, even on an ISO rip (an unreadable menu is
    /// not lost feature playback). A title missing its size or duration is timed at an
    /// estimated rate ([`title_bytes_per_sec`]). NaN when the loss cannot be timed: a damaged
    /// title with no extents (see [`Self::lost_bytes`]), an unreadable damage record, or a
    /// run that stopped before the verdict.
    pub main_lost_ms: f64,
    /// Unreadable bytes the loss verdict counted ([`LossVerdict::lost_bytes`]): the whole
    /// disc for an ISO, the ripped titles' otherwise. The loss when `main_lost_ms` is NaN.
    /// A run with no verdict (halted, wedged, single pass) counts the whole image's.
    pub lost_bytes: u64,
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
    /// Distinct from [`Self::halted`] (the user pressing Stop): a wedged pass leaves its
    /// unreached ranges RETRYABLE, so the end-of-recovery promotion must not run on them. The
    /// front-end's cue to power-cycle the drive and resume from the mapfile.
    pub wedged: bool,
    /// True when the image ended with zero unreadable and zero pending bytes (the whole disc,
    /// or a staged image's scope), and the run was neither halted nor aborted for loss.
    /// NOT narrowed to the ripped titles by [`MultipassOpts::is_iso_output`].
    pub complete: bool,
}

/// Options controlling a [`multipass_rip`] run.
#[derive(Clone, Copy, Debug)]
pub struct MultipassOpts {
    /// Patch-retry pass cap, fed into [`plan_passes`] clamped to 255 (values above
    /// run 255 patch passes). `0` selects single-pass mode: one
    /// `recovery::copy` dispatch (sweep-or-resume), no sweep/patch split, no
    /// convergence loop, no abort-on-loss gate.
    pub max_passes: u32,
    /// Seconds of playback loss in the ripped titles ([`Job::selection`]) tolerated once
    /// patch retries are exhausted, summed per title: a clip two selected titles share (a
    /// "play all" and its episodes) counts once for each, as each muxed file loses it.
    /// `0` requires a perfect rip (any residual loss aborts).
    /// Forced to `0` when `is_iso_output` regardless of the configured value
    /// (see [`effective_abort_secs`]) — an ISO deliverable is a whole-disc
    /// backup and always requires 100%.
    pub abort_on_lost_secs: u64,
    /// True when the deliverable is a whole-disc ISO image. Scopes both the
    /// per-pass convergence check ([`scope_bad_bytes`]) and the end-of-
    /// recovery abort gate's BYTE count ([`abort_lost_bytes`]) to the whole
    /// disc instead of just the ripped titles' extents, and forces
    /// `abort_on_lost_secs` to `0` via [`effective_abort_secs`].
    ///
    /// It does NOT widen the MILLISECOND figure — [`MultipassResult::main_lost_ms`] stays
    /// main-title-scoped regardless.
    pub is_iso_output: bool,
}

/// Drive the full multipass STRATEGY LOOP: sweep, then patch passes until the
/// muxable scope is clean, a pass makes no progress, or `opts.max_passes` is
/// reached, then apply the end-of-recovery promotion and the abort-on-loss
/// gate. Damage is measured over the titles `job.selection` picks.
///
/// `opts.max_passes == 0` takes the single-pass branch: one `recovery::copy`
/// dispatch, no retry loop. Otherwise Pass 1 is a `recovery::sweep` (resuming the
/// image's mapfile when one exists), followed by up to `opts.max_passes`
/// `recovery::patch` passes.
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
        let halt = crate::EngineHalt::legacy(Some(halt.clone())).with_sink(sink);
        recover(
            disc,
            &mut ReaderHost(reader),
            iso_path,
            job,
            opts,
            None,
            sink,
            &halt,
        )
    })
}

/// [`multipass_rip`] under the op token `op` (stop design v5 §4.2): every pass and the
/// pass boundaries observe it. A Stop is [`crate::EngineOutcome::Stopped`].
pub fn multipass_rip_with(
    op: &libfreemkv::Halt,
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    sink: &dyn Sink,
) -> crate::EngineOutcome<MultipassResult> {
    // §4.2: "ST-E1 stops depending on [the watchers], because the op token is observed
    // directly"; the Sink's `should_cancel` stays a cancel input.
    let halt = crate::EngineHalt::new(op, None).with_sink(sink);
    let r = recover(
        disc,
        &mut ReaderHost(reader),
        iso_path,
        job,
        opts,
        None,
        sink,
        &halt,
    );
    crate::EngineOutcome::from_result(r, &halt, |r| r.halted)
}

/// What a staged image for an MKV rip of `titles` must cover: `None` = the whole disc,
/// `Some(ranges)` = only [`libfreemkv::Disc::mkv_staging_ranges`] (nav, UDF and those
/// titles), which holds no bus-encrypted byte even when a stream file is unmapped.
///
/// JUDGEMENT (`keep_image`): a kept staging image must be a real whole-disc image, so
/// it is whole only when kept AND the drive's bus map located every stream file. A
/// scoped image is never a keepable deliverable: the caller discards it after the mux.
pub fn mkv_staging_scope(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    titles: &[usize],
    keep_image: bool,
) -> crate::Result<Option<Vec<(u32, u32)>>> {
    if keep_image && reader.unmapped_stream_files().is_empty() {
        return Ok(None);
    }
    disc.mkv_staging_ranges(reader, titles).map(Some)
}

/// [`multipass_rip`] for an MKV deliverable staged through an image: with `scope`
/// (from [`mkv_staging_scope`]) the sweep and patch passes read only those sectors.
pub fn multipass_rip_staged(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    scope: Option<&[(u32, u32)]>,
    sink: &dyn Sink,
) -> crate::Result<MultipassResult> {
    crate::run::with_cancel_watcher(sink, |halt| {
        let halt = crate::EngineHalt::legacy(Some(halt.clone())).with_sink(sink);
        recover(
            disc,
            &mut ReaderHost(reader),
            iso_path,
            job,
            opts,
            scope,
            sink,
            &halt,
        )
    })
}

/// What a front end does around the passes of a recovery: it owns the reader the passes
/// read, and may bring a lost transport back, un-wedge the drive before a patch pass, or
/// end the passes on a failed one and keep what was read (the server's autorip 1.7.7
/// policy). Every method but [`reader`](Self::reader) defaults to what an engine-only run
/// does; [`ReaderHost`] is that host over a plain reader.
pub trait PassHost {
    /// The sector source every pass reads. A host that recovers a lost transport may
    /// return a different (re-opened) drive afterwards.
    fn reader(&mut self) -> &mut dyn libfreemkv::SectorSource;

    /// Whether to resume the image's mapfile in the sweep: `Some` decides (the server's
    /// Resume), `None` (default) resumes a mapfile written in the run's raw/decrypt mode.
    fn resume_sweep(&self) -> Option<bool> {
        None
    }

    /// Whether sweep attempt `attempt` (1-based) may start. Default: the first only.
    fn sweep_attempt(&mut self, attempt: u32) -> bool {
        attempt == 1
    }

    /// Sweep attempt `attempt` failed on a transport fault `e` (a bridge crash): bring the
    /// drive back and return `true` to sweep again, resuming the mapfile. Default: `false`,
    /// the run fails with `e`.
    fn recover_transport(&mut self, _attempt: u32, _e: &libfreemkv::Error) -> bool {
        false
    }

    /// Patch pass `pass` (2 = the first patch pass) is about to read: the moment to
    /// un-wedge the drive. Default: nothing.
    fn before_patch(&mut self, _pass: u32) {}

    /// Patch pass `pass` failed with `e`: `true` ends the passes and finishes the recovery
    /// with what was read (promotion, loss gate); `false` (default) fails the run with `e`.
    fn patch_failed(&mut self, _pass: u32, _e: &libfreemkv::Error) -> bool {
        false
    }

    /// A patch pass ended on a transport fault: `true` treats it as any other pass (the
    /// no-progress rule decides); `false` (default) ends the recovery there, its damage
    /// left retryable and unpromoted.
    fn continue_after_wedge(&mut self) -> bool {
        false
    }
}

/// A [`PassHost`] over a plain reader: one sweep attempt, nothing between passes.
pub struct ReaderHost<'a>(pub &'a mut dyn libfreemkv::SectorSource);

impl PassHost for ReaderHost<'_> {
    fn reader(&mut self) -> &mut dyn libfreemkv::SectorSource {
        &mut *self.0
    }
}

// The recovery milestone `e`, to the sink.
fn milestone(sink: &dyn Sink, e: RecoveryEvent<'_>) {
    sink.event(&crate::sink::Event::Recovery(&e));
}

/// The one recovery implementation behind every `iso://` deliverable (CLI, app, server):
/// a single pass (`max_passes == 0`), or a sweep then up to `max_passes` patch passes, the
/// end-of-recovery promotion and the abort-on-loss gate, with `host`'s hooks between the
/// passes. Damage is measured over the titles `job.selection` picks; `scope` limits the
/// passes to a staged image's sectors.
#[allow(clippy::too_many_arguments)]
pub(crate) fn recover(
    disc: &libfreemkv::Disc,
    host: &mut dyn PassHost,
    iso_path: &std::path::Path,
    job: &Job,
    opts: &MultipassOpts,
    scope: Option<&[(u32, u32)]>,
    sink: &dyn Sink,
    halt: &crate::EngineHalt<'_>,
) -> crate::Result<MultipassResult> {
    let plan = plan_passes(opts.max_passes.min(u8::MAX as u32) as u8);

    // The recovery sweeps an image of the other raw/decrypt mode fresh; say so where users see it.
    // A missing or corrupt map starts fresh; an unreadable one (EIO, EACCES) fails the run.
    let prior = match Mapfile::load(&disc.mapfile_for(iso_path)) {
        Ok(m) => Some(m),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            None
        }
        Err(e) => return Err(libfreemkv::Error::from(e)),
    };
    let map_mode = prior.as_ref().and_then(|m| m.raw());
    if map_mode.is_some_and(|r| r != job.raw) {
        sink.log(
            Level::Warn,
            "multipass_rip: the existing image is in the other raw/decrypt mode; overwriting it",
        );
    }
    let empty_title = libfreemkv::DiscTitle::empty();
    let titles = measured_titles(disc, job, &empty_title);
    if !plan.multipass {
        return single_pass(disc, host.reader(), iso_path, job, scope, sink, halt);
    }

    // ── Pass 1: the sweep over a mapfile this version wrote (or as the host decides): it
    // refuses another disc's map, sweeps the other raw/decrypt mode fresh, else re-reads
    // NonTried. A transport fault the host recovers from sweeps again, resuming. ──
    let resume = host.resume_sweep().unwrap_or(map_mode.is_some());
    let resumed = match (&prior, resume && map_mode == Some(job.raw)) {
        (Some(m), true) => m.stats(),
        _ => MapStats::default(),
    };
    let mut passes = 0u32;
    let (mut last_good, mut last_unreadable, mut last_pending, mut halted);
    {
        milestone(
            sink,
            RecoveryEvent::PassStart {
                pass: 1,
                good: resumed.bytes_good,
                pending: resumed.bytes_pending,
                unreadable: resumed.bytes_unreadable,
            },
        );
        let scope = scope.map(crate::recovery::sector_scope_to_bytes);
        let mut attempt = 0u32;
        let mut last_err = None;
        let sr = loop {
            attempt += 1;
            if !host.sweep_attempt(attempt) {
                return Err(last_err.unwrap_or(libfreemkv::Error::SourceTerminated));
            }
            let bridge = ProgressBridge::new(sink);
            let sweep_opts = SweepOptions {
                decrypt: pass_should_decrypt(job.raw),
                resume: resume || attempt > 1,
                batch_sectors: None,
                skip_on_error: true,
                progress: Some(&bridge),
                halt: None,
                keys: job.keys.clone(),
            };
            match crate::recovery::sweep_in(
                disc,
                host.reader(),
                iso_path,
                &sweep_opts,
                scope.clone(),
                halt,
            ) {
                Ok(sr) => break sr,
                Err(e) if e.is_scsi_transport_failure() && !halt.is_cancelled() => {
                    if !host.recover_transport(attempt, &e) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        };
        passes += 1;
        last_good = sr.bytes_good;
        last_unreadable = sr.bytes_unreadable;
        last_pending = sr.bytes_pending;
        halted = sr.halted;
        milestone(
            sink,
            RecoveryEvent::PassDone {
                pass: 1,
                good: last_good,
                unreadable: last_unreadable,
                pending: last_pending,
                recovered: 0,
                wedged: false,
                halted,
            },
        );
    }

    // ── Pass 2..N: patch passes over the mapfile's bad ranges. ──
    let mapfile_path = disc.mapfile_for(iso_path);
    milestone(
        sink,
        RecoveryEvent::PatchesStart {
            max: plan.patch_passes as u32,
            pending: last_pending,
        },
    );
    if halted {
        milestone(sink, RecoveryEvent::Stopped { pass: 2 });
    } else {
        for n in 1..=plan.patch_passes as u32 {
            let pass = n + 1;
            if sink.should_cancel() || halt.is_cancelled() {
                milestone(sink, RecoveryEvent::Stopped { pass });
                halted = true;
                break;
            }

            // Loop-top convergence gate: skip remaining passes if the mapfile
            // shows the muxable scope clean. `None` (unreadable/unscopable) never
            // converges; an EMPTY mapfile's `Some(0)` is guarded by `bytes_good`.
            let mux_scope_bad = match Mapfile::load(&mapfile_path) {
                Ok(map) => {
                    let bad = map.ranges_with(&bad_sector_statuses());
                    titles_scope_bad(opts.is_iso_output, &bad, &titles)
                }
                Err(e) => {
                    milestone(sink, RecoveryEvent::MapUnreadable { pass, error: &e });
                    sink.log(
                        Level::Warn,
                        &format!(
                            "multipass_rip: could not read the mapfile to check convergence ({e}) — running the pass"
                        ),
                    );
                    None
                }
            };
            if pre_pass_converged(mux_scope_bad, last_good) {
                milestone(sink, RecoveryEvent::Converged { pass });
                sink.log(
                    Level::Info,
                    "multipass_rip: muxable scope 100% recovered — skipping remaining patch passes",
                );
                break;
            }

            milestone(
                sink,
                RecoveryEvent::PassStart {
                    pass,
                    good: last_good,
                    pending: last_pending,
                    unreadable: last_unreadable,
                },
            );
            host.before_patch(pass);
            let bridge = ProgressBridge::new(sink);
            let patch_opts = PatchOptions {
                keys: job.keys.clone(),
                ..PatchOptions::for_patch_pass(pass_should_decrypt(job.raw), Some(&bridge), None)
            };
            let pr =
                match crate::recovery::patch_in(disc, host.reader(), iso_path, &patch_opts, halt) {
                    Ok(pr) => pr,
                    Err(e) if host.patch_failed(pass, &e) => break,
                    Err(e) => return Err(e),
                };
            passes += 1;
            last_good = pr.bytes_good;
            last_unreadable = pr.bytes_unreadable;
            last_pending = pr.bytes_pending;
            let recovered = pr.bytes_recovered_this_pass;
            milestone(
                sink,
                RecoveryEvent::PassDone {
                    pass,
                    good: last_good,
                    unreadable: last_unreadable,
                    pending: last_pending,
                    recovered,
                    wedged: pr.wedged_exit,
                    halted: pr.halted,
                },
            );

            let exit = pass_exit(pr.halted, pr.wedged_exit);
            if exit == PassExit::Cancelled || sink.should_cancel() || halt.is_cancelled() {
                halted = true;
                break;
            }
            // A transport fault is NOT an exhausted pass: unreached ranges are
            // still retryable, and falling through would promote them to
            // permanently Unreadable, so a re-run would skip them forever.
            if exit == PassExit::Wedged && !host.continue_after_wedge() {
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
                    main_lost_ms: interrupted_lost_ms(last_unreadable, last_pending),
                    lost_bytes: last_unreadable,
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
                milestone(sink, RecoveryEvent::NoProgress { pass, recovered });
                sink.log(
                    Level::Info,
                    "multipass_rip: patch pass made no progress — exhausted, muxing on what we have",
                );
                break;
            }
        }
    }
    // A Stop that landed as the last pass ended still keeps the image resumable.
    halted |= sink.should_cancel() || halt.is_cancelled();

    if halted {
        // Severity comes from damage actually recorded — hard-coding Clean here
        // made a cancelled rip with 300 MB unreadable show a "Clean" badge.
        return Ok(MultipassResult {
            unreadable_bytes: last_unreadable,
            pending_bytes: last_pending,
            good_bytes: last_good,
            main_lost_ms: interrupted_lost_ms(last_unreadable, last_pending),
            lost_bytes: last_unreadable,
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
    let effective_abort = effective_abort_secs(opts.is_iso_output, opts.abort_on_lost_secs);
    // `verdict` is the engine's one loss verdict; `bad_sectors` is carried out of the match,
    // not derived after: the Ok branch has the mapfile split; the Err branch has only
    // unsplittable counters.
    let (verdict, good_bytes, unreadable_bytes, pending_bytes, bad_sectors) = match Mapfile::load(
        &mapfile_path,
    ) {
        Ok(mut map) => {
            // Promotion MAKES the loss visible: the abort gate reads only
            // Unreadable ranges, so a range that fails to promote out of
            // NonTrimmed silently drops out — a write error ships as a good rip.
            let mut promotion_intact = true;
            let (promote_from, promote_to) = end_of_recovery_promotion();
            if let Err(e) = map.promote(promote_from, promote_to) {
                promotion_intact = false;
                sink.log(
                    Level::Warn,
                    &format!("multipass_rip: end-of-recovery promotion failed: {e}"),
                );
            }
            if let Err(e) = map.flush() {
                promotion_intact = false;
                sink.log(
                    Level::Warn,
                    &format!("multipass_rip: failed to flush promoted mapfile: {e}"),
                );
            }
            milestone(
                sink,
                RecoveryEvent::Promoted {
                    map: &map,
                    intact: promotion_intact,
                },
            );
            let stats = map.stats();
            let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
            let mut verdict =
                loss_verdict(opts.is_iso_output, &titles, &bad_ranges, effective_abort);
            if !promotion_intact {
                // Fail-safe: an incomplete damage record cannot vouch for any figure
                // derived from it, so the rip is not shipped as within tolerance.
                let (ms, why) = titles_lost_ms(false, &titles, &bad_ranges);
                if let Some(why) = why {
                    sink.log(Level::Error, why);
                }
                verdict.lost_ms = ms;
                verdict.aborts = true;
            } else if verdict.lost_ms.is_nan() {
                sink.log(
                    Level::Warn,
                    &format!(
                        "multipass_rip: a ripped title reports no extents, so its loss \
                             cannot be timed — {} bytes unreadable, reported in bytes",
                        verdict.lost_bytes
                    ),
                );
            }
            (
                verdict,
                stats.bytes_good,
                stats.bytes_unreadable,
                stats.bytes_pending,
                end_of_recovery_bad_sectors(&stats),
            )
        }
        Err(e) => {
            // Fail-safe: the mapfile — the rip's only damage record — couldn't be read at
            // the abort-decision point, so the rip is not shipped as perfect.
            milestone(sink, RecoveryEvent::LossUnmeasured { error: &e });
            sink.log(
                    Level::Error,
                    &format!(
                        "multipass_rip: mapfile could not be loaded to verify loss — forcing abort ({e})"
                    ),
                );
            // No `MapStats` to split, so the score keeps the whole in-flight
            // aggregate deliberately — this fail-safe path must over-report,
            // not under-report (NaN can't escalate a zero-sector Clean verdict).
            (
                LossVerdict {
                    lost_bytes: last_unreadable,
                    lost_ms: f64::NAN,
                    aborts: true,
                },
                last_good,
                last_unreadable,
                last_pending,
                bad_sector_count(last_unreadable, last_pending),
            )
        }
    };

    let aborted_for_loss = verdict.aborts;
    // A loss reported in bytes only is scored by its sectors; an untrustworthy record (an
    // aborting NaN) stays Serious.
    let severity = if verdict.lost_ms.is_nan() && !verdict.aborts {
        classify_damage(bad_sectors, 0.0)
    } else {
        classify_damage(bad_sectors, verdict.lost_ms)
    };
    let main_lost_ms = verdict.lost_ms;
    let complete = recovery_is_complete(aborted_for_loss, unreadable_bytes, pending_bytes);

    Ok(MultipassResult {
        unreadable_bytes,
        pending_bytes,
        good_bytes,
        main_lost_ms,
        lost_bytes: verdict.lost_bytes,
        severity,
        passes,
        aborted_for_loss,
        halted: false,
        wedged: false,
        complete,
    })
}

/// One pass of the recovery, for a front end that runs one per invocation (the CLI): a plain
/// copy, or with `multipass` the next step of a resumable recovery — the sweep (skipping past
/// bad sectors), or once it is done a patch pass over what it left — chosen from the image's
/// mapfile. No promotion, no loss gate: the next run carries on.
pub(crate) fn one_pass(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    multipass: bool,
    sink: &dyn Sink,
    (halt, op): (&crate::EngineHalt<'_>, &libfreemkv::Halt),
) -> crate::Result<crate::CopyResult> {
    let bridge = ProgressBridge::new(sink);
    let opts = CopyOptions {
        decrypt: pass_should_decrypt(job.raw),
        multipass,
        progress: Some(&bridge),
        halt: Some(op.as_arc().clone()),
        keys: job.keys.clone(),
    };
    crate::recovery::copy_in(disc, reader, iso_path, &opts, halt)
}

// The single-pass recovery (`max_passes == 0`): one `copy` dispatch (sweep-or-resume via
// mapfile state), no retry loop, no ISO-multipass semantics, no abort gate.
fn single_pass(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    scope: Option<&[(u32, u32)]>,
    sink: &dyn Sink,
    halt: &crate::EngineHalt<'_>,
) -> crate::Result<MultipassResult> {
    let bridge = ProgressBridge::new(sink);
    let copy_opts = CopyOptions {
        decrypt: pass_should_decrypt(job.raw),
        multipass: false,
        progress: Some(&bridge),
        halt: None,
        keys: job.keys.clone(),
    };
    let cr = match scope {
        // A scoped single pass resumes its own staging; `copy` is the iso:// path.
        Some(scope) => {
            let sweep_opts = SweepOptions {
                decrypt: copy_opts.decrypt,
                resume: disc.mapfile_for(iso_path).exists(),
                batch_sectors: None,
                skip_on_error: false,
                progress: copy_opts.progress,
                halt: copy_opts.halt.clone(),
                keys: copy_opts.keys.clone(),
            };
            let scope = crate::recovery::sector_scope_to_bytes(scope);
            crate::recovery::sweep_in(disc, reader, iso_path, &sweep_opts, Some(scope), halt)?
        }
        None => crate::recovery::copy_in(disc, reader, iso_path, &copy_opts, halt)?,
    };
    // Clean is a claim about the DISC, not the plan. `bytes_pending` is safe
    // here (unlike the aggregate `bad_sector_count` forbids) because every
    // un-halted route here has `nontried == 0`, so pending is retryable damage.
    let bad_sectors = bad_sector_count(cr.bytes_unreadable, cr.bytes_pending);
    Ok(MultipassResult {
        unreadable_bytes: cr.bytes_unreadable,
        pending_bytes: cr.bytes_pending,
        good_bytes: cr.bytes_good,
        // Single-pass never runs the end-of-recovery loss gate, so a flat
        // 0.0 would falsely claim "no playback lost" beside real damage.
        // NaN marks it unquantified — except zero bad sectors, genuinely 0.0.
        main_lost_ms: if bad_sectors == 0 { 0.0 } else { f64::NAN },
        lost_bytes: cr.bytes_unreadable,
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
    })
}

#[cfg(test)]
#[path = "multipass_tests.rs"]
mod tests;
