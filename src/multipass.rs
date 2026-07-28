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

use crate::job::{Job, RipMode};
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

/// Coarse damage tier from raw counters — the freemkv product judgment
/// (thresholds), relocated from libfreemkv. Returns the crate's
/// [`crate::DamageSeverity`] (still re-exported from the library during the
/// duplication window; the definition moves here in the deletion step).
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
    /// Number of passes executed (1 sweep + N patch).
    pub passes: u32,
    /// Whether the abort-on-loss gate fired (loss exceeded tolerance after
    /// retries were exhausted).
    pub aborted_for_loss: bool,
    /// Whether the rip was cancelled (halt) mid-pass.
    pub halted: bool,
}

/// Drive the full multipass strategy: sweep, then patch passes until the disc
/// is clean or recovery stops making progress or `max_passes` is reached, then
/// apply the abort-on-loss gate.
///
/// Only meaningful for [`RipMode::Multi`]; a `Single` job does one pass with no
/// retries and no abort gate (the caller should use [`crate::recover_to_iso`]
/// directly for that). `max_passes` bounds the patch retries (autorip's
/// `max_retries` analogue). The disc is re-read from `reader` each pass exactly
/// as the drive is today.
pub fn run_multipass(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    max_passes: u32,
    is_iso_output: bool,
    sink: &dyn Sink,
) -> crate::Result<MultipassResult> {
    debug_assert!(matches!(job.mode, RipMode::Multi));

    let mut passes = 0u32;
    let mut last_pending = u64::MAX;
    let mut last: crate::recovery::CopyResult;

    loop {
        last = crate::recover_to_iso(disc, reader, iso_path, job, sink)?;
        passes += 1;

        if last.halted {
            return Ok(MultipassResult {
                unreadable_bytes: last.bytes_unreadable,
                pending_bytes: last.bytes_pending,
                good_bytes: last.bytes_good,
                main_lost_ms: 0.0,
                severity: crate::DamageSeverity::Clean,
                passes,
                aborted_for_loss: false,
                halted: true,
            });
        }

        // Auto-exit early on a perfect rip (hard rule #6): no bad bytes at all.
        if last.bytes_unreadable == 0 && last.bytes_pending == 0 {
            break;
        }
        // Stop when a pass made no forward progress on the pending set (patch
        // has converged — grinding further won't recover more) or we hit the
        // retry cap.
        if last.bytes_pending >= last_pending || passes >= max_passes {
            break;
        }
        last_pending = last.bytes_pending;
        sink.log(
            Level::Info,
            &format!(
                "multipass: pass {passes} done, {} bytes still pending — retrying",
                last.bytes_pending
            ),
        );
    }

    // After retries are exhausted, any still-pending bytes are confirmed lost.
    let confirmed_lost_bytes = last.bytes_unreadable.saturating_add(last.bytes_pending);
    let main_bad = disc
        .titles
        .first()
        .map(|t| {
            // Reconstruct the main-title bad bytes from the mapfile the passes
            // wrote (the same source autorip's abort gate reads).
            crate::recovery::bytes_bad_in_title_from_mapfile(&disc.mapfile_for(iso_path), t)
        })
        .unwrap_or(0);
    let main_lost_ms = main_title_lost_ms(disc, main_bad);

    let abort_secs = effective_abort_secs(is_iso_output, job.abort_on_lost_secs as u64);
    let aborted_for_loss = loss_aborts(confirmed_lost_bytes, main_lost_ms, abort_secs);

    let bad_sectors = confirmed_lost_bytes / 2048;
    let severity = classify_damage(bad_sectors, main_lost_ms);

    Ok(MultipassResult {
        unreadable_bytes: last.bytes_unreadable,
        pending_bytes: last.bytes_pending,
        good_bytes: last.bytes_good,
        main_lost_ms,
        severity,
        passes,
        aborted_for_loss,
        halted: false,
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
}
