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

        let outcome = f(&halt);
        done.store(true, Ordering::Relaxed);
        outcome
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
        let pass = match p.kind {
            libfreemkv::progress::PassKind::Sweep => "sweep".to_string(),
            libfreemkv::progress::PassKind::Scrape { .. } => "patch-scrape".to_string(),
            libfreemkv::progress::PassKind::Trim { .. } => "patch-trim".to_string(),
            libfreemkv::progress::PassKind::Mux => "mux".to_string(),
            libfreemkv::progress::PassKind::Verify => "verify".to_string(),
        };
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
            sectors_bad: p
                .bytes_unreadable_total
                .saturating_add(p.bytes_pending_total)
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
            decrypt: !job.raw,
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

        struct CancelSink;
        impl Sink for CancelSink {
            fn should_cancel(&self) -> bool {
                true
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
        // instead of aborting at the first failed read.
        job.mode = RipMode::Multi;

        let start = std::time::Instant::now();
        let r = recover_to_iso(&disc, &mut reader, &iso, &job, &CancelSink)
            .expect("a cancelled rip halts, it does not error");
        let elapsed = start.elapsed();

        assert!(r.halted, "a cancelling sink must halt the rip");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "Stop waited out the cooldown: took {elapsed:?}, but a wired halt \
             token is polled every 100 ms and must break the pause"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
