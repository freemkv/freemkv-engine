//! The one seam every front-end implements.
//!
//! `Sink` is how the engine talks to the outside world without printing. The
//! CLI's impl prints; autorip's updates session state and feeds the web UI; a
//! desktop UI marshals to its UI thread. It mirrors
//! `libfreemkv::progress::Progress` — a `should_cancel()` bool is the same
//! cooperative-cancellation mechanism Ctrl-C already uses in the CLI.

/// Severity of a diagnostic line. Front-ends map this to colour / log level;
/// the engine never decides presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// A progress tick during a rip. DERIVED data — `speed_bps` and `eta_secs`
/// are computed ONCE by the engine (one smoothing algorithm, one
/// remaining-bytes/speed formula) and never re-derived per front-end.
///
/// This exists because three independent ETA implementations were found in
/// the pre-split tree, all disagreeing: `autorip/src/mover.rs` computed
/// `remaining_gb*1024/speed_mbs` and baked it straight into an `m:ss` string
/// with no hour-carry; `autorip/src/ripper/mux.rs` received an already-
/// formatted `eta: String` from yet another call site; the CLI's `fmt_eta`
/// took raw seconds and did its own `h:mm:ss` formatting. Three algorithms,
/// not just three renderers — exactly the drift class that let the Bourne
/// `key_fetch` bug happen (§2 of the design doc): the same computation
/// answered twice, and the two answers disagreed.
///
/// A front-end may still format `eta_secs`/`speed_bps` however it likes
/// (`"1:23"` vs `"0:01:23"`, MB/s vs Mb/s) — that's real presentation choice
/// with no correctness content. What must NOT happen is a front-end
/// recomputing the ETA from raw byte deltas itself.
///
/// All byte counts are of the *current* operation unless noted.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Human-facing name of the current pass, e.g. `"sweep"`, `"patch #2"`,
    /// `"mux"`. Front-ends may localize; the engine supplies a stable key.
    pub pass: String,
    /// Bytes completed in the current operation.
    pub bytes_done: u64,
    /// Total bytes the current operation expects (0 if not yet known).
    pub bytes_total: u64,
    /// Sectors that could not be read so far this job.
    pub sectors_bad: u64,
    /// Throughput in bytes/sec, engine-smoothed (not an instant raw delta —
    /// see the struct doc). 0 until measurable.
    pub speed_bps: u64,
    /// Estimated seconds remaining for the current operation, engine-
    /// computed from `speed_bps`, once the estimate has converged (`None`
    /// during the warm-up window — mirrors the MakeMKV "Elapsed only, then
    /// ETA" behaviour the UI study observed).
    pub eta_secs: Option<u64>,
}

/// The engine→front-end seam. One trait, implemented once per front-end.
///
/// Every method has a default no-op so a front-end can implement only what it
/// renders. The engine calls these from the rip thread; a UI-thread front-end
/// is responsible for marshalling.
pub trait Sink: Send + Sync {
    /// A diagnostic line. Replaces every `eprintln!`/log call the orchestration
    /// used to make directly.
    fn log(&self, _level: Level, _msg: &str) {}

    /// A title's structure became known (during scan). Lets a UI populate its
    /// tree incrementally rather than waiting for the whole scan.
    fn title_opened(&self, _title: &libfreemkv::DiscTitle) {}

    /// A progress tick. Called frequently; keep the impl cheap.
    fn progress(&self, _p: &Progress) {}

    /// The job finished (success, partial, or failure — see the outcome).
    fn completed(&self, _outcome: &crate::Outcome) {}

    /// Cooperative cancellation. The engine polls this in every long loop; a
    /// front-end returns `true` to stop the job (Cancel button, Ctrl-C, service
    /// shutdown). Default never cancels.
    fn should_cancel(&self) -> bool {
        false
    }
}

/// A `Sink` that does nothing — for benchmarks, tests, and headless callers
/// that only want the returned [`crate::Outcome`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSink;

impl Sink for NoopSink {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_sink_is_send_sync_and_never_cancels() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoopSink>();
        assert!(!NoopSink.should_cancel());
    }

    #[test]
    fn noop_sink_defaults_swallow_every_call() {
        // Exercises the default method bodies — no panic, no output.
        let s = NoopSink;
        s.log(Level::Info, "hello");
        let p = Progress {
            pass: "sweep".into(),
            bytes_done: 10,
            bytes_total: 100,
            ..Default::default()
        };
        s.progress(&p);
        assert_eq!(p.eta_secs, None);
    }

    #[test]
    fn engine_sink_is_object_safe() {
        // The engine takes `&dyn Sink`; prove the trait is object-safe.
        let s = NoopSink;
        let dynref: &dyn Sink = &s;
        assert!(!dynref.should_cancel());
    }
}
