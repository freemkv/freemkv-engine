//! The one seam every front-end implements.
//!
//! `Sink` is how the engine talks to the outside world without printing. The
//! CLI's impl prints; a server's updates its job state; a desktop UI marshals
//! to its UI thread. It mirrors
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
/// A front-end may still format `eta_secs`/`speed_bps` however it likes
/// (`"1:23"` vs `"0:01:23"`, MB/s vs Mb/s) — that's real presentation choice
/// with no correctness content. What must NOT happen is a front-end
/// recomputing the ETA from raw byte deltas itself.
///
/// All byte counts are of the *current* operation unless noted.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Stable key of the current pass, for a front-end to localize: `"sweep"`,
    /// `"patch-scrape"`, `"patch-trim"`, `"mux"`, `"sync"`, `"verify"`, `"copy"`.
    /// `Cow` so the fixed keys cost no allocation: `ProgressBridge::report` runs
    /// once per batch (400k-1.6M times per rip).
    pub pass: std::borrow::Cow<'static, str>,
    /// Bytes completed in the current operation.
    pub bytes_done: u64,
    /// Total bytes the current operation expects (0 if not yet known).
    pub bytes_total: u64,
    /// Sectors that could not be read so far this job.
    pub sectors_bad: u64,
    /// Throughput in bytes/sec, engine-smoothed (not an instant raw delta —
    /// see the struct doc). 0 until measurable.
    pub speed_bps: u64,
    /// Estimated seconds remaining for the current operation, engine-computed.
    /// `None` until a rate is measurable (the first tick), on a stall, and when
    /// nothing is left. For the first 10 s of a pass it follows `speed_bps`,
    /// then the pass's running average.
    pub eta_secs: Option<u64>,
}

/// A typed milestone of an engine operation, for a front-end that drives state
/// (a daemon's job record, a webhook) without parsing log text. Borrowed: an
/// impl copies out what it keeps.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    /// A stage of the operation began: `"open"`, `"mux"`, `"verify"`, `"replace"`.
    Phase { name: &'static str },
    /// Title `idx` (0-based) starts muxing into `dest` (a sink URL).
    TitleStart { idx: usize, dest: &'a str },
    /// Title `idx` finished muxing: the outcome (`completed = false` on a stop)
    /// or the error it failed with.
    TitleDone {
        idx: usize,
        dest: &'a str,
        result: Result<&'a libfreemkv::MuxOutcome, &'a std::io::Error>,
    },
    /// A written MKV was checked against its title. `runtime_secs` is what the
    /// file showed, `expected_secs` the title's duration.
    Verify {
        path: &'a std::path::Path,
        ok: bool,
        runtime_secs: Option<f64>,
        expected_secs: f64,
    },
    /// A verified file replaced the existing one at `path`.
    Replaced { path: &'a std::path::Path },
    /// A mux's output `dest` opened for `title` as it will be written (libfreemkv's
    /// `MuxEvents::on_output_opened`): where a front end prints the pre-mux notes.
    OutputOpened {
        dest: &'a str,
        title: &'a libfreemkv::DiscTitle,
    },
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

    /// Reserved: the engine does not call this. Build the result from the value
    /// the entry point returns (e.g. `MultipassResult`), mapped into an `Outcome`
    /// if the front-end wants one.
    fn completed(&self, _outcome: &crate::Outcome) {}

    /// A typed milestone (see [`Event`]). Default ignores it.
    fn event(&self, _e: &Event<'_>) {}

    /// Cooperative cancellation. The engine polls this in every long loop; a
    /// front-end returns `true` to stop the job (Cancel button, Ctrl-C, service
    /// shutdown). Default never cancels.
    fn should_cancel(&self) -> bool {
        false
    }
}

/// A `Sink` that does nothing — for benchmarks, tests, and headless callers
/// that only want the entry point's returned result.
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
            pass: std::borrow::Cow::Borrowed("sweep"),
            bytes_done: 10,
            bytes_total: 100,
            ..Default::default()
        };
        s.progress(&p);
        s.event(&Event::Phase { name: "mux" });
        s.completed(&crate::Outcome {
            files: Vec::new(),
            unreadable_bytes: 0,
            lost_ms: 0.0,
            severity: crate::DamageSeverity::Clean,
            elapsed_secs: 0.0,
            avg_bps: 0,
        });
    }

    #[test]
    fn engine_sink_is_object_safe() {
        // The engine takes `&dyn Sink`; prove the trait is object-safe.
        let s = NoopSink;
        let dynref: &dyn Sink = &s;
        assert!(!dynref.should_cancel());
    }
}
