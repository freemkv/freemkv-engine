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

/// Milliseconds of playback lost, scoped by [`abort_lost_bytes`] and converted
/// via the title's own bytes/sec bitrate. Ported verbatim from autorip's
/// `abort_lost_ms`.
pub fn abort_lost_ms(
    output_is_iso: bool,
    title: &libfreemkv::DiscTitle,
    bad_ranges: &[(u64, u64)],
    title_bytes_per_sec: f64,
) -> f64 {
    if title_bytes_per_sec <= 0.0 {
        return 0.0;
    }
    abort_lost_bytes(output_is_iso, title, bad_ranges) as f64 / title_bytes_per_sec * MILLIS_PER_SEC
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
            total_passes: max_retries + 2, // pass 1 + retries + mux
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
/// for the muxable-scope convergence check. Pins the exact status set the loop
/// treats as unfinished: not-tried, not-trimmed, not-scraped, and unreadable.
pub fn bad_sector_statuses() -> [SectorStatus; 4] {
    [
        SectorStatus::NonTried,
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
    ]
}

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

/// The end-of-recovery promotion: after the final patch pass, bytes still left
/// `NonTrimmed` in the mapfile (i.e. "maybe" across every pass) are promoted to
/// `Unreadable` (confirmed lost) BEFORE the abort/loss gate reads them. Returns
/// the `(from, to)` status pair the loop applies. libfreemkv's patch loop
/// intentionally defers this final-verdict step to the orchestrator.
pub fn end_of_recovery_promotion() -> (SectorStatus, SectorStatus) {
    (SectorStatus::NonTrimmed, SectorStatus::Unreadable)
}

/// Coarse damage tier from raw counters — the freemkv product judgment
/// (thresholds), relocated from libfreemkv. Returns the engine-owned
/// [`crate::DamageSeverity`] (defined in `outcome.rs`).
pub fn classify_damage(bad_sectors: u64, lost_ms: f64) -> crate::DamageSeverity {
    use crate::DamageSeverity::*;
    if bad_sectors == 0 {
        return Clean;
    }
    if bad_sectors >= 500 || lost_ms >= 30_000.0 {
        return Serious;
    }
    if bad_sectors >= 51 || lost_ms >= 1_000.0 {
        return Moderate;
    }
    Cosmetic
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
        Some(t) if t.size_bytes > 0 && t.duration_secs > 0.0 => {
            main_bad_bytes as f64 / t.size_bytes as f64 * t.duration_secs * MILLIS_PER_SEC
        }
        // Loss exists but we can't quantify it (no bitrate) → NaN, which the
        // gate treats as fail-safe abort.
        _ => f64::NAN,
    }
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
    let plan = plan_passes(opts.max_passes.min(u8::MAX as u32) as u8);
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
            decrypt: !job.raw,
            multipass: false,
            progress: Some(&bridge),
            halt: None,
            vid,
            unit_keys,
            key_fetch: None,
        };
        let cr = crate::recovery::copy(disc, reader, iso_path, &copy_opts)?;
        return Ok(MultipassResult {
            unreadable_bytes: cr.bytes_unreadable,
            pending_bytes: cr.bytes_pending,
            good_bytes: cr.bytes_good,
            main_lost_ms: 0.0,
            severity: crate::DamageSeverity::Clean,
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
            decrypt: !job.raw,
            resume: false,
            batch_sectors: None,
            skip_on_error: true,
            progress: Some(&bridge),
            halt: None,
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
            let patch_opts = PatchOptions {
                decrypt: !job.raw,
                block_sectors: Some(32),
                full_recovery: true,
                reverse: true,
                wedged_threshold: 50,
                progress: Some(&bridge),
                halt: None,
                key_fetch: None,
            };
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
        return Ok(MultipassResult {
            unreadable_bytes: last_unreadable,
            pending_bytes: last_pending,
            good_bytes: last_good,
            main_lost_ms: 0.0,
            severity: crate::DamageSeverity::Clean,
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
                let (promote_from, promote_to) = end_of_recovery_promotion();
                for (pos, size) in map.ranges_with(&[promote_from]) {
                    if let Err(e) = map.record(pos, size, promote_to) {
                        sink.log(
                            Level::Warn,
                            &format!("multipass_rip: end-of-recovery promotion failed: {e}"),
                        );
                    }
                }
                if let Err(e) = map.flush() {
                    sink.log(
                        Level::Warn,
                        &format!("multipass_rip: failed to flush promoted mapfile: {e}"),
                    );
                }
                let stats = map.stats();
                let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
                let lost_bytes = abort_lost_bytes(opts.is_iso_output, main_title, &bad_ranges);
                let lost_ms = main_title_lost_ms(disc, lost_bytes);
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
    let bad_sectors = unreadable_bytes.saturating_add(pending_bytes) / 2048;
    let severity = classify_damage(bad_sectors, main_lost_ms);
    let complete = !aborted_for_loss && unreadable_bytes == 0 && pending_bytes == 0;

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

    #[test]
    fn classify_damage_tiers() {
        use crate::DamageSeverity::*;
        assert_eq!(classify_damage(0, 0.0), Clean);
        assert_eq!(classify_damage(1, 5.0), Cosmetic);
        assert_eq!(classify_damage(50, 999.0), Cosmetic);
        assert_eq!(classify_damage(51, 0.0), Moderate);
        assert_eq!(classify_damage(10, 1_000.0), Moderate);
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

    #[test]
    fn end_of_recovery_promotion_is_nontrimmed_to_unreadable() {
        assert_eq!(
            end_of_recovery_promotion(),
            (SectorStatus::NonTrimmed, SectorStatus::Unreadable)
        );
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
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
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
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
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
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
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
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
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
        let job = Job::new("disc:///dev/null", "placeholder");

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
}
