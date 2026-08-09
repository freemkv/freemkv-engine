//! Driving the relocated recovery strategy from the engine.
//!
//! [`recover_to_iso`] is the disc→ISO half of a rip: it runs the multipass
//! sweep/patch dispatch ([`crate::recovery::copy`]) against a caller-provided
//! [`SectorSource`] and reports progress through the engine [`Sink`]. It is the
//! first consumer of the relocated recovery module, and the piece a front-end
//! composes with `mux_stream` to get disc→MKV.
//!
//! The ISO→MKV mux stage is driven by the front-end (or a later engine `run()`
//! surface) via `libfreemkv::mux_stream`; keeping the recovery and mux stages
//! separately callable mirrors how the CLI and autorip already stage a rip
//! (recover to an ISO intermediate, then mux from it) and keeps each stage
//! unit-testable in isolation.

use crate::job::{Job, RipMode};
use crate::recovery::{self, CopyOptions, CopyResult};
use crate::sink::{Level, Progress, Sink};

/// The error both recovery entry points return for a decrypting multipass job.
///
/// One constructor so the two call sites cannot describe the same refusal two
/// different ways — the duplication-that-drifts shape this crate keeps finding.
pub(crate) fn multipass_requires_raw() -> libfreemkv::Error {
    libfreemkv::Error::IoError {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "multipass implies raw: a multipass rip recovers a whole-disc \
             image and cannot decrypt",
        ),
    }
}

/// Sets a watcher's `done` flag on EVERY exit path, including a panic unwind.
///
/// A plain `done.store(true)` after the watched call is skipped by an unwind,
/// and `std::thread::scope` joins its threads BEFORE propagating a panic — so
/// the watcher keeps looping on a flag nobody will ever set, the join never
/// finishes, and a panic becomes a permanent hang. For a service holding a
/// drive open that is strictly worse than the crash it replaces.
///
/// Every scoped watcher in this crate must hold one of these rather than
/// storing the flag by hand. Two hand-rolled copies of that pattern existed
/// and both had the bug; this type is the single place the rule lives.
pub(crate) struct SignalDone<'a>(pub(crate) &'a std::sync::atomic::AtomicBool);

impl Drop for SignalDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run `f` with a halt token that mirrors `sink.should_cancel()`.
///
/// Cancellation needs BOTH channels, not just the progress callback.
/// [`ProgressBridge::report`] returns `!should_cancel()`, which stops the
/// library — but only on a tick, and a damaged disc produces no ticks while it
/// is sitting in a cooldown (`ReadAction::Retry { pause_secs }`: 3 s on a NOT
/// READY retry, 30 s on zone entry). With no halt token the user's Stop went
/// unheard for the whole pause, so the button looked broken exactly when the
/// rip was going badly and someone most wanted to abort it.
///
/// `sleep_secs_or_halt` polls the token every 100 ms, so wiring one bounds Stop
/// latency at ~100 ms regardless of pause length. The watcher is the same
/// `should_cancel` → halt bridge `mux_with_input` and `extract_tree` already
/// use, and the scope joins it before returning.
pub(crate) fn with_cancel_watcher<T>(
    sink: &dyn Sink,
    f: impl FnOnce(&std::sync::Arc<std::sync::atomic::AtomicBool>) -> T,
) -> T {
    use std::sync::atomic::Ordering;

    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Check once BEFORE starting. A watcher alone makes cancellation a race
    // the work can win: on a small job the call can finish before the watcher
    // thread is first scheduled, and then an already-cancelled request runs to
    // completion. Polling cannot close that window — only asking before the
    // work begins can. It is also the obviously right behaviour: do not start
    // something you have already been told to stop.
    if sink.should_cancel() {
        halt.store(true, Ordering::Relaxed);
    }

    std::thread::scope(|s| {
        let watcher_halt = halt.clone();
        let watcher_done = done.clone();
        s.spawn(move || {
            while !watcher_done.load(Ordering::Relaxed) {
                if sink.should_cancel() {
                    watcher_halt.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        let _signal_done = SignalDone(&done);
        f(&halt)
    })
}

/// Bridges libfreemkv's low-level [`libfreemkv::progress::Progress`] callback
/// onto the engine [`Sink`]. Recovery calls `report(&PassProgress) -> bool`
/// frequently; this translates each tick to a [`Progress`] and forwards it,
/// returning `!sink.should_cancel()` so the library's cooperative-cancellation
/// bool (`false` = stop) is driven by the front-end's Cancel/Ctrl-C exactly as
/// it is today.
/// Bridges libfreemkv's low-level `Progress` callback onto the engine
/// [`Sink`]. `pub(crate)` so [`crate::multipass::multipass_rip`] can reuse the
/// exact same bridge — one speed/ETA derivation, shared by every caller that
/// drives a recovery primitive directly (see the struct-level doc above for
/// why there is only ever one of these).
pub(crate) struct ProgressBridge<'a> {
    sink: &'a dyn Sink,
    // The engine's ONE speed/ETA derivation. `report` takes `&self`, and the
    // library may hold the `&dyn Progress` across threads, so guard the
    // estimator with a Mutex.
    speed: std::sync::Mutex<crate::speed::SpeedEstimator>,
}

impl<'a> ProgressBridge<'a> {
    /// Build a fresh bridge (fresh speed/ETA estimator) for one recovery
    /// primitive call (`sweep`, `patch`, or `copy`). Each primitive call gets
    /// its own estimator — matching `recover_to_iso`'s one-bridge-per-call
    /// convention — so a new pass's speed reading doesn't inherit the
    /// previous pass's smoothing state.
    pub(crate) fn new(sink: &'a dyn Sink) -> Self {
        ProgressBridge {
            sink,
            speed: std::sync::Mutex::new(crate::speed::SpeedEstimator::new()),
        }
    }
}

impl libfreemkv::progress::Progress for ProgressBridge<'_> {
    fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
        // Borrowed, not allocated: this runs once per batch.
        let pass: std::borrow::Cow<'static, str> = std::borrow::Cow::Borrowed(match p.kind {
            libfreemkv::progress::PassKind::Sweep => "sweep",
            libfreemkv::progress::PassKind::Scrape { .. } => "patch-scrape",
            libfreemkv::progress::PassKind::Trim { .. } => "patch-trim",
            libfreemkv::progress::PassKind::Mux => "mux",
            libfreemkv::progress::PassKind::Verify => "verify",
        });
        // Derive speed/ETA ONCE, here — the front-end just formats it. Sweep's
        // work_done/work_total are the authoritative progress denominator.
        let (speed_bps, eta_secs) = self
            .speed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sample(p.work_done, p.work_total);
        let progress = Progress {
            pass,
            bytes_done: p.work_done,
            bytes_total: p.work_total,
            // Damage only: permanently unreadable + queued-for-retry.
            //
            // NOT `bytes_pending_total`. That aggregate folds in NonTried —
            // territory Pass 1 simply has not reached yet — so it counts the
            // whole un-swept remainder as damage. On the first tick of a
            // pristine 25 GB disc that is ~12 million "bad sectors" reported
            // for a disc with none, falling toward zero as the sweep advances.
            // The mapfile's own docs name `bytes_retryable` as the field for
            // a "will retry" bucket and say `bytes_pending` over-counts for
            // exactly this reason.
            sectors_bad: p
                .bytes_unreadable_total
                .saturating_add(p.bytes_retryable_total)
                / 2048,
            speed_bps,
            eta_secs,
        };
        self.sink.progress(&progress);
        // `false` from report() tells the library to halt; the engine halts
        // when the front-end asks to cancel.
        !self.sink.should_cancel()
    }
}

/// Recover a disc to an ISO image at `iso_path`, driving the relocated
/// multipass sweep/patch strategy and reporting through `sink`.
///
/// `reader` is the sector source (a live drive session's reader, or an ISO for
/// re-recovery). `disc` is the already-scanned disc. The [`RipMode`] on `job`
/// selects single-pass (`recovery::copy` with `multipass = false`, aborts on
/// first read error) vs multipass (sweep + patch dispatch). Decryption is on
/// unless `job.raw`.
///
/// Returns the library's [`CopyResult`] (byte accounting + completion flags);
/// the caller maps it into an [`crate::Outcome`] once the mux stage has also
/// run, or inspects it directly for the ISO-only case.
pub fn recover_to_iso(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    iso_path: &std::path::Path,
    job: &Job,
    sink: &dyn Sink,
) -> crate::Result<CopyResult> {
    // Multipass implies raw — refuse before touching the drive. `preflight`
    // reports this as data for a UI, but it is explicitly callable without
    // executing, so a front-end may skip it; this is the enforcement that
    // cannot be bypassed. Refused rather than silently forced to raw: a caller
    // that asked to decrypt and got a raw image would be handed an
    // undecrypted ISO it believes is playable.
    if matches!(job.mode, RipMode::Multi) && !job.raw {
        return Err(multipass_requires_raw());
    }

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
            vid: disc.aacs.as_ref().map(|a| a.volume_id),
            unit_keys: disc
                .aacs
                .as_ref()
                .map(|a| a.unit_keys.clone())
                .unwrap_or_default(),
            key_fetch: None,
        };

        recovery::copy(disc, reader, iso_path, &opts)
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
            Ok(count as usize)
        }
        fn capacity_sectors(&self) -> u32 {
            self.capacity
        }
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
        let dir = std::env::temp_dir().join(format!("fmkv-engine-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("out.iso");

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

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The WATCHER, not the check-before-starting.
    ///
    /// `with_cancel_watcher` does two things: it asks once before the work
    /// begins, and it polls on a thread while the work runs. The existing
    /// `cancel_via_sink_halts_recovery` uses a sink that is cancelled from the
    /// very first call, so the entry check alone satisfies it — and the
    /// mutation run duly deleted the `!` from the watcher's loop condition
    /// (`while !done` -> `while done`, so the loop body never runs) with the
    /// suite still green. A cancel arriving AFTER start would then be missed
    /// entirely.
    ///
    /// This sink stays uncancelled for the first few polls, so only a live
    /// watcher can set the halt flag.
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
            // Give the watcher time to poll at least once (it sleeps 100 ms).
            // The loop returns the instant the flag goes up, so the deadline
            // costs nothing on the happy path — it is a liveness backstop, not
            // a measurement, and is deliberately far past the one poll this
            // needs so a runner that doesn't schedule the watcher promptly
            // cannot turn a working watcher into a red build.
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
        // A sink whose should_cancel is always true must make the library
        // halt. The sweep's progress reporter is called on EVERY batch
        // iteration (only the tracing heartbeat is throttled), so a tick — and
        // therefore the halt — is guaranteed on the first iteration of any
        // disc with at least one sector.
        struct CancelSink;
        impl Sink for CancelSink {
            fn should_cancel(&self) -> bool {
                true
            }
        }
        let dir = std::env::temp_dir().join(format!("fmkv-engine-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("c.iso");
        let sectors = 4096u32;
        let disc = clean_disc(sectors);
        let mut reader = ZeroReader { capacity: sectors };
        let job = Job::new("disc:///dev/null", iso.to_string_lossy());
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &CancelSink)
            .expect("a cancelled rip halts, it does not error");
        // Assert the actual property. `is_ok()` alone passed even if
        // should_cancel was never consulted, since a clean synthetic disc
        // completes either way. Deliberately NOT asserting byte counts:
        // wiring a halt token would legitimately change them.
        assert!(r.halted, "a cancelling sink must halt the rip");
        assert!(!r.complete, "a halted rip is not complete");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `sectors_bad` must count DAMAGE, not un-swept territory.
    ///
    /// The bridge used to derive it from `bytes_pending_total`, which folds in
    /// NonTried — the part of the disc Pass 1 has not reached yet. On the
    /// first progress tick of a flawless disc that reports essentially the
    /// whole disc as bad sectors, and the number then falls as the sweep
    /// advances. An operator watching a damage counter start at twelve million
    /// and count down has no way to tell a pristine disc from a failing one.
    #[test]
    fn a_clean_disc_reports_no_bad_sectors_while_the_sweep_is_still_running() {
        use libfreemkv::progress::Progress as _;

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
        bridge.report(&libfreemkv::progress::PassProgress {
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
        });

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[0],
            "a disc with nothing wrong reported bad sectors purely because the \
             sweep had not finished yet"
        );
    }

    /// ...and it must COUNT the damage once there is some.
    ///
    /// The companion test above uses an all-zero `PassProgress`, where
    /// `/ 2048`, `% 2048` and `* 2048` all agree on `0` — so the conversion
    /// itself was exercised with the one input that can never distinguish it.
    /// The number this produces is the bad-sector count an operator reads off
    /// the progress line while deciding whether to stop a failing rip.
    #[test]
    fn sectors_bad_converts_bad_bytes_into_a_sector_count() {
        use libfreemkv::progress::Progress as _;

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
        bridge.report(&libfreemkv::progress::PassProgress {
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
        });

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[3],
            "7048 bad bytes is 3 whole bad sectors — unreadable and retryable \
             are summed, then converted once, rounding down"
        );
    }

    /// A panic inside the watched call must PROPAGATE, not hang.
    ///
    /// `with_cancel_watcher` sets `done` after `f` returns. On a panic the
    /// unwind skips that store, and `thread::scope` joins its threads before
    /// propagating — so the watcher, still looping on `done`, is joined
    /// forever and the panic becomes a permanent hang. A ripping service that
    /// would have crashed and restarted instead sits there holding the drive.
    ///
    /// The failure mode is a deadlock, so this cannot simply call and assert:
    /// a regression would hang the whole test binary rather than fail it. The
    /// call runs on its own thread and the assertion is on a receive timeout.
    #[test]
    fn a_panic_inside_the_watched_call_propagates_instead_of_hanging() {
        struct NeverCancel;
        impl Sink for NeverCancel {}

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // The panic is deliberate; keep it off the test log.
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_cancel_watcher(&NeverCancel, |_halt| panic!("boom"));
            }));
            std::panic::set_hook(prev);
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

    /// Stop must be honoured DURING a damage cooldown, not after it.
    ///
    /// The recovery cooldowns (`ReadAction::Retry { pause_secs }`) are 3 s for a
    /// NOT READY retry and 30 s on zone entry. Cancellation reaches the library
    /// two ways: the progress callback's return value, which is only consulted
    /// when a tick happens, and the halt token, which `sleep_secs_or_halt`
    /// polls every 100 ms. A damaged disc produces no ticks while it is
    /// sleeping, so with no halt token wired the user's Stop sat unheard for the
    /// full pause — up to 30 s of a button that looks broken.
    ///
    /// This reader fails every read with the BU40N bad-sector signature
    /// (NOT READY / 0x04 / 0x3E), so the first batch enters a cooldown
    /// immediately. The assertion is wall-clock: with the halt token wired the
    /// call returns in well under one pause.
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

        // NOT a sink that is already cancelled: `with_cancel_watcher`'s
        // "check once before starting" fires on the very FIRST `should_cancel`
        // call — before the sweep loop ever calls `read_sectors` once — so an
        // always-true sink proves nothing about the cooldown at all: the read
        // loop's own halt check (top of every iteration, before the read)
        // breaks it before `NotReadyReader` is ever invoked. That was this
        // test's original shape, and it stayed green when instrumented to
        // confirm `read_sectors` is called zero times under it — the fixture
        // made the cooldown branch this test is named for unreachable.
        //
        // A sink that flips to cancelled on the SECOND ask (as opposed to
        // this one, elapsed-time-gated) is not enough either: the watcher
        // thread's own first poll happens essentially immediately after
        // `with_cancel_watcher`'s pre-start check consumes ask #1, so ask #2
        // — the watcher's first loop iteration, no sleep yet — usually wins
        // the race against the main thread even reaching the sweep loop,
        // reproducing the exact same "halted before the first read" shape
        // this test exists to rule out. Gating on wall-clock time instead
        // gives the main thread a real window to issue the read and enter
        // the cooldown before any observer is allowed to see a cancellation.
        struct DelayedCancelSink {
            start: std::time::Instant,
        }
        impl Sink for DelayedCancelSink {
            fn should_cancel(&self) -> bool {
                self.start.elapsed() > std::time::Duration::from_millis(50)
            }
        }
        let dir = std::env::temp_dir().join(format!("fmkv-engine-cooldown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("cd.iso");
        let sectors = 4096u32;
        let disc = clean_disc(sectors);
        let mut reader = NotReadyReader { capacity: sectors };
        let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
        // Multipass, so the sweep skips on error and reaches the cooldown
        // instead of aborting at the first failed read. Multipass implies raw.
        job.mode = RipMode::Multi;
        job.raw = true;

        // ONE origin for the sink's cancel-delay and the measurement below.
        // These used to be separate instants with `create_dir_all`, `Job::new`
        // and path formatting in between, so filesystem latency came out of the
        // sink's 50 ms window: on a slow runner the sink already reported
        // cancelled before the first read, the run halted immediately, and this
        // test failed on its OWN lower bound. It gates every release (CI runs
        // this suite in both profiles), so the flake was a release blocker.
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
