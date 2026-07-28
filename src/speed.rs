//! The engine's ONE speed/ETA derivation.
//!
//! Speed and ETA are derived here and nowhere else, so every front-end — the
//! desktop UI (via the `Sink`), the CLI, and autorip — reads one agreed value
//! instead of each re-deriving it from raw byte deltas (the drift class the
//! engine split exists to prevent). A front-end formats the numbers; it never
//! computes them.
//!
//! ## Two rates, on purpose
//!
//! This estimator deliberately produces **two different** throughput figures,
//! because "how fast is it going *right now*" and "when will it *finish*" are
//! different questions and a single rate answers neither well:
//!
//! - [`SpeedEstimator::observe`] → the **displayed** speed: a sliding window
//!   ([`display_window_secs`], adaptive 10 s → 60 s, or a fixed 10 s in
//!   [`responsive`](SpeedEstimator::set_responsive) mode for bursty patch
//!   passes). Responsive enough that a stall visibly dips it, smooth enough
//!   that steady-state jitter doesn't.
//! - [`SpeedEstimator::eta_speed_mbs`] → the **ETA** rate: a long running
//!   average (bytes ripped this pass / elapsed this pass). Barely moves through
//!   a transient stall, so the ETA doesn't whipsaw.
//!
//! This split is a paid-for lesson (2026-05-08: a 12 s drive stall made a
//! shared-rate ETA jump 1:30:00 → 30:00:00 mid-rip). It was proven in autorip
//! and promoted here so every front-end inherits it.
//!
//! Front-ends that only want a single agreed answer call
//! [`SpeedEstimator::sample_at`] / [`SpeedEstimator::sample`], which run both
//! internally and return `(speed_bps, eta_secs)` for the `Sink` `Progress`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

/// Display-speed sliding-window size, adapted to elapsed pass time:
///
/// - `0..STATIC_PHASE_SECS` → fixed at `STATIC_WINDOW_SECS` (10 s). Early in a
///   pass we have little history; a small window stays responsive while the ETA
///   hasn't settled yet.
/// - `STATIC_PHASE_SECS..STATIC_PHASE_SECS+GROWTH_PHASE_SECS` → linear growth
///   from `STATIC_WINDOW_SECS` to `MAX_WINDOW_SECS` over `GROWTH_PHASE_SECS` of
///   elapsed time. Smooths progressively as we accumulate enough samples for a
///   longer window to be reliable.
/// - `STATIC_PHASE_SECS+GROWTH_PHASE_SECS..` → fixed at `MAX_WINDOW_SECS`
///   (60 s). Steady-state averaging window.
///
/// Resulting schedule (1.5 s callback ⇒ ~40 samples in a 60 s window):
/// ```text
///   t+ 30 s → 10 s window
///   t+ 60 s → 10 s window (start of growth phase)
///   t+210 s → 35 s window
///   t+360 s → 60 s window (cap reached)
///   t+1 h  → 60 s window
/// ```
const STATIC_PHASE_SECS: f64 = 60.0;
const STATIC_WINDOW_SECS: f64 = 10.0;
const GROWTH_PHASE_SECS: f64 = 300.0;
const MAX_WINDOW_SECS: f64 = 60.0;

/// Minimum elapsed time before the pass-start running average is trustworthy
/// enough to use for ETA. Below this the running average is noisy (small
/// denominator, first-sample artefacts) so we fall back to the displayed speed.
/// 10 s ≈ a few callbacks at the typical throttle, enough to settle.
const ETA_WARMUP_SECS: f64 = 10.0;

/// Sanity cap on any computed MB/s. Real optical drives top out around
/// 70–140 MB/s; ≥ 1 GB/s would be a measurement artefact (clock jitter, mapfile
/// replay, a resumed disc's already-copied bytes counted in one interval). Drop
/// rather than display — this is the guard that keeps "8000 MB/s" off screens.
const MAX_PLAUSIBLE_MBS: f64 = 1024.0;

/// Compute the appropriate sliding-window size for the displayed speed given
/// how long the pass has been running. See [`STATIC_PHASE_SECS`] for the curve.
fn display_window_secs(elapsed_pass_secs: f64) -> f64 {
    if elapsed_pass_secs < STATIC_PHASE_SECS {
        STATIC_WINDOW_SECS
    } else if elapsed_pass_secs < STATIC_PHASE_SECS + GROWTH_PHASE_SECS {
        let t = elapsed_pass_secs - STATIC_PHASE_SECS;
        STATIC_WINDOW_SECS + (MAX_WINDOW_SECS - STATIC_WINDOW_SECS) * (t / GROWTH_PHASE_SECS)
    } else {
        MAX_WINDOW_SECS
    }
}

/// Tracks byte-progress samples and produces a smoothed *display* throughput
/// plus a stable *ETA* rate. Not thread-safe by itself; callers that touch it
/// from a callback wrap it in the appropriate interior-mutability/lock (see
/// `run.rs`, `mux.rs`, and autorip's pass state).
///
/// Construct one **per pass** (each pass anchors its own running-average clock
/// on the first `observe`); a `bytes_done` that goes backwards mid-life is also
/// treated as a fresh pass and re-anchors cleanly.
#[derive(Debug)]
pub struct SpeedEstimator {
    /// Sliding window of `(observation_time, bytes_done)`, oldest at the front.
    /// Pruned to the current window size on each `observe`. Drives the displayed
    /// speed.
    samples: VecDeque<(Instant, u64)>,
    /// Wall-clock + byte count of this pass's first observation. Set on the
    /// first `observe` (not `new`) so a cold-start gap before the first byte
    /// doesn't stretch the running-average denominator. Drives the ETA rate.
    pass_start: Option<(Instant, u64)>,
    /// Hold the display window at a fixed 10 s (instead of growing to 60 s) for
    /// bursty patch passes, so a fast-capture burst shows up fast instead of
    /// being diluted over a minute. The steady sweep leaves this false.
    responsive: bool,
}

impl SpeedEstimator {
    pub fn new() -> Self {
        SpeedEstimator {
            samples: VecDeque::with_capacity(16),
            pass_start: None,
            responsive: false,
        }
    }

    /// Set responsive mode (fixed 10 s display window) — used for bursty patch
    /// passes where the steady growing window would dilute a recovery burst.
    pub fn set_responsive(&mut self, responsive: bool) {
        self.responsive = responsive;
    }

    /// Feed a fresh sample; returns the windowed **display** speed in MB/s. Also
    /// anchors the pass-start clock + byte counter on the first observation so
    /// [`eta_speed_mbs`](Self::eta_speed_mbs) can compute a stable running
    /// average.
    ///
    /// Drops samples older than the current window size (which grows with
    /// elapsed pass time per [`display_window_secs`]), pushes the new one, then
    /// computes `(newest_bytes - oldest_bytes) / (newest_t - oldest_t)`.
    /// Returns 0 when the window holds fewer than 2 samples (we need at least
    /// one prior point to compute a rate). A `bytes_done` below the pass-start
    /// baseline re-anchors as a fresh pass.
    pub fn observe(&mut self, now: Instant, bytes_done: u64) -> f64 {
        match self.pass_start {
            None => self.pass_start = Some((now, bytes_done)),
            Some((_, start_bytes)) if bytes_done < start_bytes => {
                // Progress counter reset (new pass / new title): restart cleanly
                // rather than compute a negative delta.
                self.samples.clear();
                self.pass_start = Some((now, bytes_done));
            }
            Some(_) => {}
        }
        let elapsed_pass = self
            .pass_start
            .map(|(t, _)| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        let window_secs = if self.responsive {
            STATIC_WINDOW_SECS
        } else {
            display_window_secs(elapsed_pass)
        };
        if let Some(cutoff) = now.checked_sub(Duration::from_secs_f64(window_secs)) {
            while let Some(&(t, _)) = self.samples.front() {
                if t < cutoff {
                    self.samples.pop_front();
                } else {
                    break;
                }
            }
        }
        self.samples.push_back((now, bytes_done));

        if self.samples.len() < 2 {
            return 0.0;
        }
        let &(oldest_t, oldest_b) = self.samples.front().unwrap();
        let &(newest_t, newest_b) = self.samples.back().unwrap();
        let dt = newest_t.duration_since(oldest_t).as_secs_f64();
        if dt <= 0.0 {
            return 0.0;
        }
        let bytes = newest_b.saturating_sub(oldest_b);
        let mbs = bytes as f64 / BYTES_PER_MIB / dt;
        mbs.min(MAX_PLAUSIBLE_MBS)
    }

    /// Long-average rate for **ETA** — bytes ripped this pass divided by
    /// elapsed-this-pass. Stable; transient stalls barely move it (a 12 s stall
    /// after 5 minutes of healthy ripping shifts it by < 5 %). Falls back to
    /// `display_speed` during the first [`ETA_WARMUP_SECS`] while the running
    /// average is still noisy. Callers turn this rate into an ETA string with
    /// their own remaining-bytes and formatting/caps.
    pub fn eta_speed_mbs(&self, now: Instant, display_speed: f64) -> f64 {
        let Some((start_t, start_b)) = self.pass_start else {
            return display_speed;
        };
        let elapsed = now.duration_since(start_t).as_secs_f64();
        if elapsed < ETA_WARMUP_SECS {
            return display_speed;
        }
        let Some(&(_, latest_bytes)) = self.samples.back() else {
            return display_speed;
        };
        let bytes = latest_bytes.saturating_sub(start_b);
        if bytes == 0 {
            return display_speed;
        }
        let mbs = bytes as f64 / BYTES_PER_MIB / elapsed;
        mbs.min(MAX_PLAUSIBLE_MBS)
    }

    /// Convenience for front-ends that just want one agreed answer: feed a
    /// sample and get `(speed_bps, eta_secs)` for the `Sink` `Progress`. Runs
    /// [`observe`](Self::observe) (display speed) and
    /// [`eta_speed_mbs`](Self::eta_speed_mbs) (ETA rate) internally. `now` is
    /// injected so this is deterministically testable. ETA is `None` until a
    /// meaningful rate exists and there is work left.
    pub fn sample_at(
        &mut self,
        now: Instant,
        bytes_done: u64,
        bytes_total: u64,
    ) -> (u64, Option<u64>) {
        let display_mbs = self.observe(now, bytes_done);
        let eta_mbs = self.eta_speed_mbs(now, display_mbs);
        let speed_bps = (display_mbs * BYTES_PER_MIB) as u64;
        // 0.0001 MB/s (~0.1 KB/s) floor: any real forward motion yields an ETA,
        // but a dead stall doesn't divide toward a multi-year number.
        let eta_secs = if eta_mbs > 0.0001 && bytes_total > bytes_done {
            let rem_mb = (bytes_total - bytes_done) as f64 / BYTES_PER_MIB;
            Some((rem_mb / eta_mbs).round() as u64)
        } else {
            None
        };
        (speed_bps, eta_secs)
    }

    /// Convenience for production callers: sample at the real current time.
    pub fn sample(&mut self, bytes_done: u64, bytes_total: u64) -> (u64, Option<u64>) {
        self.sample_at(Instant::now(), bytes_done, bytes_total)
    }
}

impl Default for SpeedEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Display speed (observe) ────────────────────────────────────────────

    #[test]
    fn first_sample_returns_zero() {
        // A single sample can't yield a rate — and must not synthesize one from
        // already-copied bytes (the "2197.8 MB/s on a resumed BD rip" bug).
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        let speed = s.observe(t0, 20 * 1024 * 1024 * 1024);
        assert_eq!(speed, 0.0, "first sample must not synthesize a speed");
        assert_eq!(s.samples.len(), 1, "first sample is recorded for the next");
    }

    #[test]
    fn second_sample_matches_physical_rate() {
        // 70 MiB delta in 1 s → ~70 MB/s.
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        let _ = s.observe(t0, 1_000_000_000);
        let speed = s.observe(t0 + Duration::from_secs(1), 1_000_000_000 + 70 * 1_048_576);
        assert!((speed - 70.0).abs() < 1.0, "expected ~70 MB/s, got {speed}");
    }

    #[test]
    fn caps_absurd_instantaneous() {
        // An 80 GB jump in 1 s (mapfile replay on a resumed disc) must cap at
        // 1 GB/s, not blast a nonsense number to the display.
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        let _ = s.observe(t0, 0);
        let speed = s.observe(t0 + Duration::from_secs(1), 80 * 1024 * 1024 * 1024);
        assert!(speed <= MAX_PLAUSIBLE_MBS, "speed {speed} MB/s not capped");
    }

    #[test]
    fn steady_state_converges() {
        let mut s = SpeedEstimator::new();
        let mut t = Instant::now();
        let mut bytes: u64 = 1_000_000_000;
        let _ = s.observe(t, bytes);
        let mut last = 0.0;
        for _ in 0..20 {
            t += Duration::from_secs(1);
            bytes += 70 * 1_048_576;
            last = s.observe(t, bytes);
        }
        assert!((last - 70.0).abs() < 2.0, "expected ~70 MB/s, got {last}");
    }

    #[test]
    fn stall_drops_out_of_display_within_window() {
        // 2026-05-08 scenario: a 12 s stall must age out of the display window
        // within STATIC_WINDOW_SECS of recovery, not stick like the old EWMA.
        let mut s = SpeedEstimator::new();
        let mut t = Instant::now();
        let mut bytes: u64 = 0;
        let _ = s.observe(t, bytes);
        for _ in 0..10 {
            t += Duration::from_secs(1);
            bytes += 70 * 1_048_576;
            let _ = s.observe(t, bytes);
        }
        let healthy = s.observe(t, bytes);
        assert!((healthy - 70.0).abs() < 2.0, "pre-stall ~70, got {healthy}");

        for _ in 0..12 {
            t += Duration::from_secs(1);
            bytes += 1_048_576;
            let _ = s.observe(t, bytes);
        }
        let during = s.observe(t, bytes);
        assert!(during < 20.0, "stall must dip display, got {during} MB/s");

        for _ in 0..(STATIC_WINDOW_SECS as i32 + 2) {
            t += Duration::from_secs(1);
            bytes += 70 * 1_048_576;
            let _ = s.observe(t, bytes);
        }
        let recovered = s.observe(t, bytes);
        assert!(
            (recovered - 70.0).abs() < 2.0,
            "display must return to ~70 once stall ages out, got {recovered}"
        );
    }

    #[test]
    fn responsive_mode_holds_fixed_window() {
        // In responsive (patch) mode the window stays at 10 s even well past the
        // point the steady curve would have grown it.
        let mut s = SpeedEstimator::new();
        s.set_responsive(true);
        let mut t = Instant::now();
        let mut bytes: u64 = 0;
        let _ = s.observe(t, bytes);
        // 3 minutes in: steady mode would use a ~34 s window; responsive holds 10.
        for _ in 0..180 {
            t += Duration::from_secs(1);
            bytes += 70 * 1_048_576;
            let _ = s.observe(t, bytes);
        }
        // At most STATIC_WINDOW_SECS of samples retained (+1 for the boundary).
        assert!(
            s.samples.len() <= STATIC_WINDOW_SECS as usize + 2,
            "responsive window kept {} samples, expected ~10",
            s.samples.len()
        );
    }

    #[test]
    fn counter_reset_restarts_cleanly() {
        // A new pass drops bytes_done to a lower value; must not spike or panic.
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        let _ = s.observe(t0, 900 * 1_048_576);
        let _ = s.observe(t0 + Duration::from_secs(1), 1000 * 1_048_576);
        let speed = s.observe(t0 + Duration::from_secs(2), 0);
        assert_eq!(speed, 0.0, "reset re-anchors → no rate yet");
        assert_eq!(s.samples.len(), 1, "reset cleared the window");
    }

    #[test]
    fn display_window_grows_with_elapsed_time() {
        assert_eq!(display_window_secs(0.0), 10.0);
        assert_eq!(display_window_secs(30.0), 10.0);
        assert_eq!(display_window_secs(59.9), 10.0);
        assert_eq!(display_window_secs(60.0), 10.0);
        assert!((display_window_secs(210.0) - 35.0).abs() < 0.1);
        assert!((display_window_secs(360.0) - 60.0).abs() < 0.1);
        assert_eq!(display_window_secs(3600.0), 60.0);
    }

    // ─── ETA rate (eta_speed_mbs) ───────────────────────────────────────────

    #[test]
    fn eta_speed_stays_stable_through_a_stall() {
        // Display can dip during a stall; the ETA rate must not.
        let mut s = SpeedEstimator::new();
        let mut t = Instant::now();
        let mut bytes: u64 = 0;
        let _ = s.observe(t, bytes);
        for _ in 0..300 {
            t += Duration::from_secs(1);
            bytes += 70 * 1_048_576;
            let _ = s.observe(t, bytes);
        }
        let disp = s.observe(t, bytes);
        let eta_before = s.eta_speed_mbs(t, disp);
        assert!((eta_before - 70.0).abs() < 2.0, "ETA ~70, got {eta_before}");

        for _ in 0..12 {
            t += Duration::from_secs(1);
            bytes += 1_048_576;
            let _ = s.observe(t, bytes);
        }
        let disp2 = s.observe(t, bytes);
        let eta_during = s.eta_speed_mbs(t, disp2);
        assert!(
            (eta_during - 70.0).abs() < 5.0,
            "ETA rate must stay near true average through a stall, got {eta_during}"
        );
    }

    #[test]
    fn eta_falls_back_to_display_during_warmup() {
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        let _ = s.observe(t0, 0);
        let display = s.observe(t0 + Duration::from_secs(2), 100 * 1_048_576);
        let eta = s.eta_speed_mbs(t0 + Duration::from_secs(2), display);
        assert_eq!(eta, display, "before warmup ETA rate mirrors display");
    }

    // ─── Unified convenience (sample_at) ────────────────────────────────────

    #[test]
    fn sample_at_first_call_has_no_speed_or_eta() {
        let mut s = SpeedEstimator::new();
        let t = Instant::now();
        assert_eq!(s.sample_at(t, 0, 1000), (0, None));
    }

    #[test]
    fn sample_at_reports_speed_and_eta() {
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        s.sample_at(t0, 0, 200 * 1_048_576);
        // 100 MiB over 1 s → ~100 MB/s; but ETA rate is in warmup (<10 s) so it
        // falls back to display; 100 MiB left → ~1 s ETA.
        let (bps, eta) = s.sample_at(
            t0 + Duration::from_secs(1),
            100 * 1_048_576,
            200 * 1_048_576,
        );
        let mbs = bps as f64 / BYTES_PER_MIB;
        assert!((mbs - 100.0).abs() < 1.0, "expected ~100 MB/s, got {mbs}");
        assert_eq!(eta, Some(1));
    }

    #[test]
    fn sample_at_no_eta_once_complete() {
        let mut s = SpeedEstimator::new();
        let t0 = Instant::now();
        s.sample_at(t0, 500, 1000);
        let (_, eta) = s.sample_at(t0 + Duration::from_secs(1), 1000, 1000);
        assert_eq!(eta, None, "no work left → no ETA");
    }

    #[test]
    fn sample_at_zero_interval_does_not_panic() {
        let mut s = SpeedEstimator::new();
        let t = Instant::now();
        s.sample_at(t, 100, 1000);
        let (speed, _) = s.sample_at(t, 200, 1000);
        assert_eq!(speed, 0, "same instant → no rate");
    }
}
