//! The engine's ONE speed/ETA derivation.
//!
//! Speed and ETA are derived here and nowhere else, so every front-end reads
//! one agreed value instead of re-deriving it from raw byte deltas. A
//! front-end formats the numbers; it never computes them.
//!
//! [`SpeedEstimator::observe`] returns the **displayed** speed (smoothed sliding window);
//! [`SpeedEstimator::eta_speed_mbs`] returns the stable **ETA** rate.
//! [`SpeedEstimator::sample_at`] / [`SpeedEstimator::sample`] run both and return `(speed_bps,
//! eta_secs)`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

// Display-window growth curve: flat 10s, then linear growth to 60s over GROWTH_PHASE_SECS, then
// flat 60s.
const STATIC_PHASE_SECS: f64 = 60.0;
const STATIC_WINDOW_SECS: f64 = 10.0;
const GROWTH_PHASE_SECS: f64 = 300.0;
const MAX_WINDOW_SECS: f64 = 60.0;

// Minimum elapsed time before the running average is trustworthy for ETA (below this it's
// noisy: small denominator, first-sample artefacts).
const ETA_WARMUP_SECS: f64 = 10.0;

// Sanity cap on computed MB/s: real optical drives top out ~70-140 MB/s, so >=1 GB/s is a
// measurement artefact (clock jitter, mapfile replay) to drop rather than display.
const MAX_PLAUSIBLE_MBS: f64 = 1024.0;

/// Compute the appropriate sliding-window size for the displayed speed given
/// how long the pass has been running. See [`STATIC_PHASE_SECS`] for the curve.
///
/// Both `<` here are equivalent to `<=`: the curve is continuous at each breakpoint, so no test
/// can pin them.
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
/// `run.rs`, `mux.rs`).
///
/// Construct one **per pass** (each pass anchors its own running-average clock
/// on the first `observe`); a `bytes_done` below that first observation's count
/// is also treated as a fresh pass and re-anchors cleanly.
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

    /// Feed a fresh sample; returns the windowed **display** speed in MB/s.
    /// Also anchors the pass-start clock + byte counter on the first
    /// observation so [`eta_speed_mbs`](Self::eta_speed_mbs) can compute a
    /// stable running average.
    ///
    /// Drops samples older than the current window size, pushes the new one,
    /// then computes `(newest_bytes - oldest_bytes) / (newest_t - oldest_t)`.
    /// Returns 0 when the window holds fewer than 2 samples. A `bytes_done`
    /// below the pass-start baseline re-anchors as a fresh pass.
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
        let (Some(&(oldest_t, oldest_b)), Some(&(newest_t, newest_b))) =
            (self.samples.front(), self.samples.back())
        else {
            return 0.0;
        };
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
        // 0.0001 MB/s (~0.1 KB/s) floor: any real forward motion yields an ETA, but a dead
        // stall doesn't divide toward a multi-year number. Strict: exactly the floor is a stall.
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
#[path = "speed_tests.rs"]
mod tests;
