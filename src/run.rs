//! Driving the relocated recovery strategy from the engine.
//!
//! [`recover_to_iso`] is the disc→ISO half of a rip: it runs the multipass
//! sweep/patch dispatch ([`crate::recovery::copy`]) against a caller-provided
//! [`libfreemkv::SectorSource`] and reports progress through the engine [`Sink`].
//! The mux stage lives beside it ([`crate::mux_title`], [`crate::run_titles`],
//! [`crate::mux_image_titles`], [`crate::remux_iso`]); [`ProgressBridge`] adapts
//! libfreemkv `Event::Pass` progress to the [`Sink`].

use crate::job::{Job, RipMode};
use crate::recovery::{self, CopyOptions, CopyResult};
use crate::sink::{Level, Progress, Sink};

/// The run context the engine hands every libfreemkv stage: the op's `halt`, no events yet,
/// and the developer diagnostics read once from the environment.
pub(crate) fn ctx(halt: &libfreemkv::Halt) -> libfreemkv::Ctx {
    libfreemkv::Ctx::new(halt.clone()).with_diag(libfreemkv::Diag::from_env())
}

// Sets `done` on every exit path, including a panic unwind, since a plain `store(true)` placed
// after the call is skipped by an unwind and the join would hang forever. Release: a watcher
// that Acquire-loads `true` sees everything the work sent before it returned.
pub(crate) struct SignalDone<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for SignalDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

// Wakes a parked watcher on every exit path. Declare it BEFORE the `SignalDone` guard: locals
// drop in reverse, so `done` is already set when the watcher wakes.
pub(crate) struct WakeOnDrop(pub(crate) std::thread::Thread);

impl Drop for WakeOnDrop {
    fn drop(&mut self) {
        self.0.unpark();
    }
}

// Wires a halt token, not just the progress callback, so Stop is honoured even during a retry
// cooldown when no progress tick fires.
pub(crate) fn with_cancel_watcher<T>(
    sink: &dyn Sink,
    f: impl FnOnce(&std::sync::Arc<std::sync::atomic::AtomicBool>) -> T,
) -> T {
    use std::sync::atomic::Ordering;

    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Check once BEFORE starting: a watcher alone makes cancellation a race
    // the work can win on a small job. Only asking before work begins closes
    // that window — and it's obviously right not to start what's already stopped.
    if sink.should_cancel() {
        halt.store(true, Ordering::Relaxed);
    }

    std::thread::scope(|s| {
        let watcher_halt = halt.clone();
        let watcher_done = done.clone();
        let watcher = s.spawn(move || {
            while !watcher_done.load(Ordering::Acquire) {
                if sink.should_cancel() {
                    watcher_halt.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::park_timeout(std::time::Duration::from_millis(100));
            }
        });

        let _wake = WakeOnDrop(watcher.thread().clone());
        let _signal_done = SignalDone(&done);
        f(&halt)
    })
}

// Bridges libfreemkv's `Event::Pass` progress onto the engine `Sink`, translating each
// tick (a Stop comes through the run's halt, which polls `should_cancel()`). `pub(crate)`
// so `crate::multipass::multipass_rip` reuses the same speed/ETA derivation.
pub(crate) struct ProgressBridge<'a> {
    sink: &'a dyn Sink,
    // The engine's ONE speed/ETA derivation. `event` takes `&self`, and the
    // library may hold the `&dyn Events` across threads, so guard the
    // estimator with a Mutex.
    speed: std::sync::Mutex<crate::speed::SpeedEstimator>,
}

impl<'a> ProgressBridge<'a> {
    // A fresh estimator per primitive call (`sweep`/`patch`/`copy`), so a new
    // pass's speed reading doesn't inherit the previous pass's smoothing state.
    pub(crate) fn new(sink: &'a dyn Sink) -> Self {
        ProgressBridge {
            sink,
            speed: std::sync::Mutex::new(crate::speed::SpeedEstimator::new()),
        }
    }
}

impl libfreemkv::Events for ProgressBridge<'_> {
    fn event(&self, e: &libfreemkv::Event<'_>) {
        if let libfreemkv::Event::Pass(p) = e {
            self.sink.event(&crate::sink::Event::Pass(p));
            self.report_at(std::time::Instant::now(), p);
        }
    }
}

impl ProgressBridge<'_> {
    // One pass tick at an injected `now`, so the speed derivation is testable.
    fn report_at(&self, now: std::time::Instant, p: &libfreemkv::progress::PassProgress) {
        // Borrowed, not allocated: this runs once per batch.
        let pass: std::borrow::Cow<'static, str> = std::borrow::Cow::Borrowed(match p.kind {
            libfreemkv::progress::PassKind::Sweep => "sweep",
            libfreemkv::progress::PassKind::Scrape { .. } => "patch-scrape",
            libfreemkv::progress::PassKind::Trim { .. } => "patch-trim",
            libfreemkv::progress::PassKind::Mux => "mux",
            libfreemkv::progress::PassKind::Verify => "verify",
            libfreemkv::progress::PassKind::Extract => "extract",
        });
        // Derive speed/ETA ONCE, here — the front-end just formats it. Sweep's
        // work_done/work_total are the authoritative progress denominator.
        let (speed_bps, eta_secs) = {
            let mut speed = self.speed.lock().unwrap_or_else(|e| e.into_inner());
            // Patch passes read in bursts: a fixed window shows a burst instead of diluting it.
            speed.set_responsive(matches!(
                p.kind,
                libfreemkv::progress::PassKind::Scrape { .. }
                    | libfreemkv::progress::PassKind::Trim { .. }
            ));
            speed.sample_at(now, p.work_done, p.work_total)
        };
        let progress = Progress {
            pass,
            bytes_done: p.work_done,
            bytes_total: p.work_total,
            // Damage only: permanently unreadable + queued-for-retry. NOT
            // `bytes_pending_total`, which folds in NonTried (unswept territory)
            // and would report ~12M "bad sectors" on a pristine disc's first tick.
            sectors_bad: p
                .bytes_unreadable_total
                .saturating_add(p.bytes_retryable_total)
                / crate::multipass::SECTOR_BYTES,
            speed_bps,
            eta_secs,
        };
        self.sink.progress(&progress);
    }
}

/// Recover a disc to an ISO image at `iso_path`, driving the relocated
/// multipass sweep/patch strategy and reporting through `sink`.
///
/// `reader` is the sector source (a live drive session's reader, or an ISO for
/// re-recovery). `disc` is the already-scanned disc. The [`RipMode`] on `job`
/// selects single-pass (`recovery::copy`, aborts on first read error) vs
/// multipass (sweep + patch dispatch). Decryption is on unless `job.raw`.
///
/// Returns the library's [`CopyResult`] (byte accounting + completion flags);
/// the caller maps it into an [`crate::Outcome`] once the mux stage has run.
pub fn recover_to_iso(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    sink: &dyn Sink,
) -> crate::Result<CopyResult> {
    let bridge = ProgressBridge::new(sink);

    sink.log(
        Level::Info,
        &format!(
            "recover_to_iso: mode={:?} raw={} dest={}",
            job.mode,
            job.raw,
            iso_path.display()
        ),
    );

    with_cancel_watcher(sink, |halt| {
        let opts = CopyOptions {
            decrypt: crate::multipass::pass_should_decrypt(job.raw),
            multipass: matches!(job.mode, RipMode::Multi),
            progress: Some(&bridge),
            halt: Some(halt.clone()),
            keys: job.keys.clone(),
        };
        // The Sink probe makes every progress report a `should_cancel()` check.
        let halt = crate::EngineHalt::legacy(opts.halt.clone()).with_sink(sink);
        recovery::copy_in(disc, reader, iso_path, &opts, &halt)
    })
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
