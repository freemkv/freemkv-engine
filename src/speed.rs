//! The engine's ONE speed/ETA derivation.
//!
//! Speed and ETA are derived here and nowhere else, so every front-end — the
//! desktop UI (via the `Sink`), and eventually the CLI and autorip once they
//! move their progress onto the engine — reads one agreed value instead of
//! each re-deriving it from raw byte deltas (the drift class the engine split
//! exists to prevent). A front-end formats the numbers; it never computes them.

use std::time::Instant;

/// Exponential-moving-average smoothing factor for instantaneous throughput.
/// Higher = snappier but noisier; 0.3 tracks real speed changes within a few
/// ticks while damping the per-sample jitter a raw delta shows.
const EMA_ALPHA: f64 = 0.3;

/// Tracks byte-progress samples and produces a smoothed throughput + an ETA.
/// Not thread-safe by itself; callers that touch it from a callback wrap it in
/// the appropriate interior-mutability/lock (see `run.rs`, `mux.rs`).
#[derive(Debug)]
pub(crate) struct SpeedEstimator {
    /// (timestamp, bytes_done) of the previous sample.
    last: Option<(Instant, u64)>,
    /// Smoothed throughput in bytes/sec (EMA); 0 until the first interval.
    smoothed_bps: f64,
}

impl SpeedEstimator {
    pub(crate) fn new() -> Self {
        SpeedEstimator {
            last: None,
            smoothed_bps: 0.0,
        }
    }

    /// Feed a progress sample; returns `(speed_bps, eta_secs)` for the `Sink`
    /// `Progress`. `now` is injected so this is deterministically testable.
    /// Speed is 0 and ETA is `None` until a positive interval has elapsed and a
    /// speed is measurable.
    pub(crate) fn sample_at(
        &mut self,
        now: Instant,
        bytes_done: u64,
        bytes_total: u64,
    ) -> (u64, Option<u64>) {
        if let Some((t0, b0)) = self.last {
            let dt = now.duration_since(t0).as_secs_f64();
            // Only update on forward progress over a real interval; a zero/neg
            // interval or a counter that went backwards (a fresh pass reset) is
            // ignored rather than producing a nonsense spike.
            if dt > 0.0 && bytes_done >= b0 {
                let inst = (bytes_done - b0) as f64 / dt;
                self.smoothed_bps = if self.smoothed_bps <= 0.0 {
                    inst
                } else {
                    EMA_ALPHA * inst + (1.0 - EMA_ALPHA) * self.smoothed_bps
                };
            } else if bytes_done < b0 {
                // Progress counter reset (new pass / new title): restart cleanly.
                self.smoothed_bps = 0.0;
            }
        }
        self.last = Some((now, bytes_done));

        let speed = self.smoothed_bps.max(0.0) as u64;
        // ETA only once we have a meaningful speed and there is work left. The
        // 1.0 B/s floor avoids a divide that yields absurd multi-year ETAs on a
        // stalled tick.
        let eta = if self.smoothed_bps > 1.0 && bytes_total > bytes_done {
            Some(((bytes_total - bytes_done) as f64 / self.smoothed_bps).round() as u64)
        } else {
            None
        };
        (speed, eta)
    }

    /// Convenience for production callers: sample at the real current time.
    pub(crate) fn sample(&mut self, bytes_done: u64, bytes_total: u64) -> (u64, Option<u64>) {
        self.sample_at(Instant::now(), bytes_done, bytes_total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn first_sample_has_no_speed_or_eta() {
        let mut e = SpeedEstimator::new();
        let t = Instant::now();
        assert_eq!(e.sample_at(t, 0, 1000), (0, None));
    }

    #[test]
    fn steady_progress_yields_speed_and_eta() {
        let mut e = SpeedEstimator::new();
        let t0 = Instant::now();
        e.sample_at(t0, 0, 1000);
        // 100 bytes over 1s → 100 B/s; 900 left → ~9s ETA.
        let (speed, eta) = e.sample_at(t0 + Duration::from_secs(1), 100, 1000);
        assert_eq!(speed, 100);
        assert_eq!(eta, Some(9));
    }

    #[test]
    fn ema_smooths_toward_a_new_rate() {
        let mut e = SpeedEstimator::new();
        let t0 = Instant::now();
        e.sample_at(t0, 0, 100_000);
        // First interval: 100 B/s.
        let (s1, _) = e.sample_at(t0 + Duration::from_secs(1), 100, 100_000);
        assert_eq!(s1, 100);
        // Then a faster interval: 300 B/s. EMA = 0.3*300 + 0.7*100 = 160.
        let (s2, _) = e.sample_at(t0 + Duration::from_secs(2), 400, 100_000);
        assert_eq!(s2, 160);
    }

    #[test]
    fn no_eta_once_complete() {
        let mut e = SpeedEstimator::new();
        let t0 = Instant::now();
        e.sample_at(t0, 500, 1000);
        let (_, eta) = e.sample_at(t0 + Duration::from_secs(1), 1000, 1000);
        assert_eq!(eta, None, "no work left → no ETA");
    }

    #[test]
    fn counter_reset_restarts_cleanly() {
        // A new pass resets bytes_done to a lower value; must not spike or panic.
        let mut e = SpeedEstimator::new();
        let t0 = Instant::now();
        e.sample_at(t0, 900, 1000);
        e.sample_at(t0 + Duration::from_secs(1), 1000, 1000);
        // New pass: counter drops to 0.
        let (speed, eta) = e.sample_at(t0 + Duration::from_secs(2), 0, 1000);
        assert_eq!(speed, 0);
        assert_eq!(eta, None);
    }

    #[test]
    fn zero_interval_does_not_divide_by_zero() {
        let mut e = SpeedEstimator::new();
        let t = Instant::now();
        e.sample_at(t, 100, 1000);
        // Same instant again — no update, no panic.
        let (speed, _) = e.sample_at(t, 200, 1000);
        assert_eq!(speed, 0);
    }
}
