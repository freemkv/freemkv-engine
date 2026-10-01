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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A synthetic all-zero SectorSource (hard rule #2: no live drive). Reports a
    // fixed capacity and fills every requested read with zeros, always
    // succeeding — the clean-disc happy path for the sweep dispatch.
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

    /// The double must honour the contract it stands in for:
    /// `SectorSource::read_sectors` returns the number of BYTES written into
    /// `buf`. This one filled `count * 2048` bytes and returned `count`.
    #[test]
    fn the_zero_reader_returns_a_byte_count_like_the_trait_says() {
        use libfreemkv::SectorSource as _;
        let mut buf = vec![0u8; 4 * 2048];
        let mut zero = ZeroReader { capacity: 64 };
        assert_eq!(
            zero.read_sectors(0, 4, &mut buf, false).unwrap(),
            8192,
            "4 sectors is 8192 BYTES"
        );
    }

    // Counts progress ticks and log lines so a test can assert the bridge fired.
    #[derive(Default)]
    struct CountingSink {
        ticks: AtomicUsize,
        logs: AtomicUsize,
    }
    impl Sink for CountingSink {
        fn log(&self, _l: Level, _m: &str) {
            self.logs.fetch_add(1, Ordering::Relaxed);
        }
        fn progress(&self, _p: &Progress) {
            self.ticks.fetch_add(1, Ordering::Relaxed);
        }
    }

    // A minimal unencrypted disc fixture sized to `sectors`.
    fn clean_disc(sectors: u32) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "TESTDISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: sectors,
            capacity_bytes: sectors as u64 * 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    #[test]
    fn single_pass_recovers_a_clean_synthetic_disc_to_iso() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("out.iso");

        let sectors = 256u32;
        let disc = clean_disc(sectors);
        let mut reader = ZeroReader { capacity: sectors };
        let sink = CountingSink::default();
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());

        let result = recover_to_iso(&disc, &mut reader, &iso, &job, &sink)
            .expect("clean single-pass recovery should succeed");

        // The whole disc was readable → complete, all bytes good, nothing bad.
        assert_eq!(result.bytes_total, sectors as u64 * 2048);
        assert_eq!(result.bytes_good, sectors as u64 * 2048);
        assert_eq!(result.bytes_unreadable, 0);
        assert!(result.complete);
        assert!(!result.halted);
        // The engine logged the recovery start (the bridge/sink is wired).
        assert!(sink.logs.load(Ordering::Relaxed) >= 1);
    }

    // Exercises the WATCHER, not the check-before-starting: a sink that stays uncancelled for
    // the first few polls, so only a live watcher (not the entry check) can set the halt flag.
    #[test]
    fn a_cancel_raised_after_the_work_starts_is_still_observed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct LateCancelSink {
            asks: AtomicUsize,
        }
        impl Sink for LateCancelSink {
            fn should_cancel(&self) -> bool {
                // False on the first ask (the pre-start check), true after —
                // so passing this REQUIRES the polling watcher to run.
                self.asks.fetch_add(1, Ordering::Relaxed) > 0
            }
        }

        let sink = LateCancelSink {
            asks: AtomicUsize::new(0),
        };
        let observed = with_cancel_watcher(&sink, |halt| {
            // Give the watcher time to poll at least once (every 100 ms); the
            // loop returns the instant the flag goes up, costing nothing on the
            // happy path. This is a liveness backstop, not a timing measurement.
            for _ in 0..400 {
                if halt.load(std::sync::atomic::Ordering::Relaxed) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            false
        });
        assert!(
            observed,
            "the watcher never set the halt flag — a cancel raised after start \
             would be lost"
        );
    }

    #[test]
    fn cancel_via_sink_halts_recovery() {
        // A sink cancelled before the call: the pre-start check halts it. The watcher and
        // `report`'s return are pinned by their own tests.
        struct CancelSink;
        impl Sink for CancelSink {
            fn should_cancel(&self) -> bool {
                true
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("c.iso");
        let sectors = 4096u32;
        let disc = clean_disc(sectors);
        let mut reader = ZeroReader { capacity: sectors };
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &CancelSink)
            .expect("a cancelled rip halts, it does not error");
        // Assert the actual property: `is_ok()` alone passed even if
        // should_cancel was never consulted. NOT asserting byte counts —
        // wiring a halt token would legitimately change them.
        assert!(r.halted, "a cancelling sink must halt the rip");
        assert!(!r.complete, "a halted rip is not complete");
    }

    // `sectors_bad` must count DAMAGE, not un-swept territory: deriving it from
    // `bytes_pending_total` made a flawless disc report ~12M bad sectors on the first tick.
    #[test]
    fn a_clean_disc_reports_no_bad_sectors_while_the_sweep_is_still_running() {
        #[derive(Default)]
        struct Captured(std::sync::Mutex<Vec<u64>>);
        impl Sink for Captured {
            fn progress(&self, p: &Progress) {
                self.0.lock().unwrap().push(p.sectors_bad);
            }
        }

        let sink = Captured::default();
        let bridge = ProgressBridge::new(&sink);

        // One tick, early in a 25 GB sweep of an undamaged disc: nothing read
        // yet is NOT damage. `bytes_retryable_total` and
        // `bytes_unreadable_total` are both zero because nothing has failed.
        let disc = 25u64 * 1024 * 1024 * 1024;
        bridge.report_at(
            std::time::Instant::now(),
            &libfreemkv::progress::PassProgress {
                kind: libfreemkv::progress::PassKind::Sweep,
                work_done: 4096,
                work_total: disc,
                bytes_good_total: 4096,
                bytes_unreadable_total: 0,
                bytes_pending_total: disc - 4096, // the un-swept remainder
                bytes_retryable_total: 0,
                bytes_total_disc: disc,
                disc_duration_secs: None,
                bytes_bad_in_main_title: 0,
                main_title_duration_secs: None,
                main_title_size_bytes: None,
                located: libfreemkv::progress::LocatedProgress::default(),
            },
        );

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[0],
            "a disc with nothing wrong reported bad sectors purely because the \
             sweep had not finished yet"
        );
    }

    // ...and it must COUNT the damage once there is some: the companion test uses an all-zero
    // `PassProgress`, where `/`, `%` and `*` by 2048 all agree on `0`.
    #[test]
    fn sectors_bad_converts_bad_bytes_into_a_sector_count() {
        #[derive(Default)]
        struct Captured(std::sync::Mutex<Vec<u64>>);
        impl Sink for Captured {
            fn progress(&self, p: &Progress) {
                self.0.lock().unwrap().push(p.sectors_bad);
            }
        }

        let sink = Captured::default();
        let bridge = ProgressBridge::new(&sink);

        // 4096 unreadable + 2952 retryable = 7048 bytes, deliberately NOT a
        // multiple of 2048, so `/`, `%` and `*` all give different answers
        // (3 vs 904 vs an overflow-saturated absurdity).
        bridge.report_at(
            std::time::Instant::now(),
            &libfreemkv::progress::PassProgress {
                kind: libfreemkv::progress::PassKind::Sweep,
                work_done: 1_000_000,
                work_total: 25 * 1024 * 1024 * 1024,
                bytes_good_total: 1_000_000,
                bytes_unreadable_total: 4096,
                bytes_pending_total: 8192,
                bytes_retryable_total: 2952,
                bytes_total_disc: 25 * 1024 * 1024 * 1024,
                disc_duration_secs: None,
                bytes_bad_in_main_title: 0,
                main_title_duration_secs: None,
                main_title_size_bytes: None,
                located: libfreemkv::progress::LocatedProgress::default(),
            },
        );

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[3],
            "7048 bad bytes is 3 whole bad sectors — unreadable and retryable \
             are summed, then converted once, rounding down"
        );
    }

    // The watcher is woken when the work ends: a quick call must not wait out a poll interval.
    #[test]
    fn a_finished_call_returns_without_waiting_for_the_next_poll() {
        struct NeverCancel;
        impl Sink for NeverCancel {}
        let t0 = std::time::Instant::now();
        // Each call outlives the watcher's first poll, so the watcher is asleep when it ends.
        for _ in 0..5 {
            with_cancel_watcher(&NeverCancel, |_halt| {
                std::thread::sleep(std::time::Duration::from_millis(5))
            });
        }
        let took = t0.elapsed();
        assert!(
            took < std::time::Duration::from_millis(250),
            "5 short calls took {took:?}: each waited out the watcher's 100 ms sleep"
        );
    }

    fn tick(kind: libfreemkv::progress::PassKind, done: u64) -> libfreemkv::progress::PassProgress {
        libfreemkv::progress::PassProgress {
            kind,
            work_done: done,
            work_total: 1 << 40,
            bytes_good_total: done,
            bytes_unreadable_total: 0,
            bytes_pending_total: 0,
            bytes_retryable_total: 0,
            bytes_total_disc: 1 << 40,
            disc_duration_secs: None,
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: None,
            main_title_size_bytes: None,
            located: libfreemkv::progress::LocatedProgress::default(),
        }
    }

    // Patch passes read in bursts: their displayed speed uses the fixed 10 s window, while
    // the steady sweep's window has grown (to 20 s at 120 s), diluting the same burst.
    #[test]
    fn patch_passes_show_a_burst_over_the_fixed_window() {
        use libfreemkv::progress::PassKind;
        #[derive(Default)]
        struct Speeds(std::sync::Mutex<Vec<u64>>);
        impl Sink for Speeds {
            fn progress(&self, p: &Progress) {
                self.0.lock().unwrap().push(p.speed_bps);
            }
        }
        let mib = 1024 * 1024;
        let last_speed = |kind: PassKind| {
            let sink = Speeds::default();
            let bridge = ProgressBridge::new(&sink);
            let t0 = std::time::Instant::now();
            for (secs, done) in [(0, 0), (100, 0), (112, 0), (120, 100 * mib)] {
                bridge.report_at(t0 + std::time::Duration::from_secs(secs), &tick(kind, done));
            }
            let last = *sink.0.lock().unwrap().last().unwrap();
            last / mib
        };
        let scrape = PassKind::Scrape { reverse: false };
        assert_eq!(
            last_speed(scrape),
            12,
            "100 MiB over the last 8 s of a 10 s window"
        );
        assert_eq!(last_speed(PassKind::Sweep), 5, "100 MiB over a 20 s window");
    }

    // `report`'s return is the library's keep-going flag: false exactly when the sink cancels.
    // A Stop the Sink raises at a progress tick halts the recovery before another tick (the
    // run's halt polls `should_cancel()`), not a watcher poll later.
    #[test]
    fn a_cancel_raised_at_a_progress_tick_halts_before_the_next_tick() {
        #[derive(Default)]
        struct StopAtFirstTick {
            ticks: AtomicUsize,
        }
        impl Sink for StopAtFirstTick {
            fn progress(&self, _p: &Progress) {
                self.ticks.fetch_add(1, Ordering::SeqCst);
            }
            fn should_cancel(&self) -> bool {
                self.ticks.load(Ordering::SeqCst) > 0
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("t.iso");
        let sectors = 1 << 16;
        let disc = clean_disc(sectors);
        let mut reader = ZeroReader { capacity: sectors };
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.raw = true;
        let sink = StopAtFirstTick::default();
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &sink).unwrap();
        assert!(r.halted, "the tick's Stop halts the rip");
        assert_eq!(
            sink.ticks.load(Ordering::SeqCst),
            1,
            "no tick after the Stop"
        );
    }

    // A panic inside the watched call must PROPAGATE, not hang the join. The failure mode is a
    // deadlock, so this runs the call on its own thread and asserts via a receive timeout.
    #[test]
    fn a_panic_inside_the_watched_call_propagates_instead_of_hanging() {
        struct NeverCancel;
        impl Sink for NeverCancel {}

        let (tx, rx) = std::sync::mpsc::channel();
        // The panic message is expected in the log: the hook is process-global, so swapping
        // it here would race every other test thread.
        std::thread::spawn(move || {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_cancel_watcher(&NeverCancel, |_halt| panic!("deliberate test panic"));
            }));
            let _ = tx.send(caught.is_err());
        });

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(panicked) => assert!(panicked, "the panic must reach the caller"),
            Err(_) => panic!(
                "with_cancel_watcher hung: `f` panicked, the unwind skipped the \
                 `done` store, and thread::scope is joining a watcher that will \
                 never stop looping"
            ),
        }
    }

    // Stop must be honoured DURING a damage cooldown (3-30 s pauses that produce no progress
    // ticks), not only after it. Reader fails with the NOT READY signature; assertion is
    // wall-clock.
    #[test]
    fn cancel_is_honoured_during_a_damage_cooldown() {
        struct NotReadyReader {
            capacity: u32,
        }
        impl libfreemkv::SectorSource for NotReadyReader {
            fn read_sectors(
                &mut self,
                _lba: u32,
                _count: u16,
                _buf: &mut [u8],
                _recovery: bool,
            ) -> libfreemkv::Result<usize> {
                Err(libfreemkv::error::Error::ScsiError {
                    opcode: libfreemkv::scsi::SCSI_READ_10,
                    status: libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION,
                    sense: Some(libfreemkv::ScsiSense {
                        sense_key: libfreemkv::scsi::SENSE_KEY_NOT_READY,
                        asc: 0x04,
                        ascq: 0x3E,
                    }),
                })
            }
            fn capacity_sectors(&self) -> u32 {
                self.capacity
            }
        }

        // NOT always-cancelled (pre-start check halts before the first read,
        // cooldown unreachable) nor flip-on-2nd-ask (watcher's first poll
        // usually wins). Wall-clock gating gives a real window to the cooldown.
        struct DelayedCancelSink {
            start: std::time::Instant,
        }
        impl Sink for DelayedCancelSink {
            fn should_cancel(&self) -> bool {
                self.start.elapsed() > std::time::Duration::from_millis(50)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("cd.iso");
        let sectors = 4096u32;
        let disc = clean_disc(sectors);
        let mut reader = NotReadyReader { capacity: sectors };
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        // Multipass, so the sweep skips on error and reaches the cooldown
        // instead of aborting at the first failed read. Multipass implies raw.
        job.mode = RipMode::Multi;
        job.raw = true;

        // ONE origin for the sink's cancel-delay and the measurement below.
        // Separate instants used to let filesystem setup eat into the sink's
        // 50 ms window, so a slow runner failed the test on its own lower bound.
        let start = std::time::Instant::now();
        let sink = DelayedCancelSink { start };
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &sink)
            .expect("a cancelled rip halts, it does not error");
        let elapsed = start.elapsed();

        assert!(r.halted, "a cancelling sink must halt the rip");
        assert!(
            elapsed >= std::time::Duration::from_millis(50),
            "finished in {elapsed:?} — faster than the sink's own 50 ms \
             cancel-delay, so this never actually entered the NOT_READY \
             cooldown at all (the fixture made the interesting branch \
             unreachable again)"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Stop waited out the cooldown: took {elapsed:?}, but a wired halt \
             token is polled every 100 ms and must break the pause"
        );
    }

    // Decryption is orthogonal to the read policy (EO6): a decrypting multipass job runs.
    #[test]
    fn a_decrypting_multipass_job_runs() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("decrypted.iso");
        let disc = clean_disc(64);
        let mut reader = ZeroReader { capacity: 64 };
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        job.mode = RipMode::Multi;
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &CountingSink::default())
            .expect("a decrypting multipass recovery is allowed");
        assert!(r.complete, "{r:?}");
        assert_eq!(std::fs::metadata(&iso).unwrap().len(), 64 * 2048);
    }
}
