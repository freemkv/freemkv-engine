//! Stop on the remux path (stop design v5 §4.2, §4.4, §4.5; §5.4 ET12-ET17, ET19-ET22).
//! Per spec; do not change without a spec citation proving otherwise.
//!
//! `FakeIo` scales time: a production 60 s stall window is a few hundred ms here. Its
//! `sync` keeps libfreemkv's `durable_sync_file` contract (LP19): it reports each piece,
//! fails `SyncTimeout` only after its window with no piece, and ends `Halted` on a cancel.

use super::tests::{Events, job, mkv, outcome, title, writes};
use super::*;
use libfreemkv::halt::Progress as Counter;
use libfreemkv::io::artifact_lock::lock_path as sidecar_for;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

const WINDOW: Duration = Duration::from_millis(250);
// §5.0 (B): "Tests assert **≤ 1 s** wall beyond the injected in-flight time."
const STOP_LATENCY: Duration = Duration::from_secs(1);

#[derive(Default)]
struct SyncPlan {
    pieces: u64,
    gap: Duration,
    // Pieces completed before the flush stops making progress (`None`: never stalls).
    stall_at: Option<u64>,
}

#[derive(Default)]
struct ReadPlan {
    // Bytes per read and the delay before each.
    chunk: usize,
    delay: Duration,
    // Bytes returned before every later read blocks until `release`.
    block_after: Option<u64>,
}

// What the folder sync after the rename does.
#[derive(Default)]
enum DirSync {
    #[default]
    Ok,
    // Cancels this op token, then waits for the cancel to reach the sync.
    Stop(Halt),
    Fail,
}

#[derive(Default)]
struct WritePlan {
    // Bytes accepted before every later write blocks until `release`.
    block_after: Option<u64>,
    // Reports each write whole but drops its last byte.
    short: bool,
    // Writes zeros in place of the data.
    zeros: bool,
}

struct FakeIo {
    timing: RemuxTiming,
    sync: SyncPlan,
    read: ReadPlan,
    write: WritePlan,
    dir_sync: DirSync,
    in_sync: AtomicBool,
    returned: Arc<AtomicU64>,
    release: Arc<AtomicBool>,
}

impl FakeIo {
    fn new(sync: SyncPlan, read: ReadPlan) -> Self {
        Self {
            timing: RemuxTiming {
                verify_stall: WINDOW,
                copy_stall: WINDOW,
                activity_every: Duration::from_millis(10),
                lock_beat: Duration::from_millis(10),
            },
            sync,
            read,
            write: WritePlan::default(),
            dir_sync: DirSync::default(),
            in_sync: AtomicBool::new(false),
            returned: Arc::default(),
            release: Arc::default(),
        }
    }
}

impl Drop for FakeIo {
    fn drop(&mut self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

impl FakeIo {
    fn sync_dir(&self, halt: &Halt) -> io::Result<()> {
        match &self.dir_sync {
            DirSync::Ok => Ok(()),
            DirSync::Fail => Err(io::Error::from_raw_os_error(5)),
            DirSync::Stop(op) => {
                op.cancel();
                let t0 = Instant::now();
                while t0.elapsed() < STOP_LATENCY {
                    halt.check()?;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            }
        }
    }
}

impl RemuxIo for FakeIo {
    fn sync(
        &self,
        file: &std::fs::File,
        halt: &Halt,
        on: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()> {
        let m = file.metadata()?;
        if m.is_dir() {
            return self.sync_dir(halt);
        }
        if !m.is_file() || self.sync.pieces == 0 {
            return Ok(());
        }
        self.in_sync.store(true, Ordering::SeqCst);
        let (len, p) = (m.len(), Counter::new());
        let mut timer = StallTimer::new(WINDOW, &p);
        let piece = len.div_ceil(self.sync.pieces).max(1);
        let mut done = 0;
        for i in 0..self.sync.pieces {
            let start = Instant::now();
            let stalled = self.sync.stall_at.is_some_and(|k| i >= k);
            while stalled || start.elapsed() < self.sync.gap {
                halt.check()?;
                if timer.poll(&p) == Stall::Expired {
                    return Err(libfreemkv::Error::SyncTimeout.into());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            done = (done + piece).min(len);
            p.bump();
            on(done, len);
        }
        Ok(())
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Throttled {
            inner: std::fs::File::open(path)?,
            chunk: self.read.chunk.max(1),
            delay: self.read.delay,
            block_after: self.read.block_after,
            returned: self.returned.clone(),
            release: self.release.clone(),
        }))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(Faulty {
            inner: file,
            block_after: self.write.block_after,
            short: self.write.short,
            zeros: self.write.zeros,
            written: 0,
            release: self.release.clone(),
        }))
    }

    fn timing(&self) -> RemuxTiming {
        self.timing
    }
}

// A copy destination per `WritePlan`.
struct Faulty {
    inner: std::fs::File,
    block_after: Option<u64>,
    short: bool,
    zeros: bool,
    written: u64,
    release: Arc<AtomicBool>,
}

impl Write for Faulty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.block_after.is_some_and(|b| self.written >= b) {
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let n = match self.block_after {
            Some(b) => buf.len().min(
                usize::try_from(b.saturating_sub(self.written))
                    .unwrap()
                    .max(1),
            ),
            None => buf.len(),
        };
        let data = if self.zeros {
            vec![0; n]
        } else {
            buf[..n].to_vec()
        };
        let kept = if self.short { n - 1 } else { n };
        self.inner.write_all(&data[..kept])?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// Reads at most `chunk` bytes after `delay`; past `block_after` bytes, blocks until released.
struct Throttled {
    inner: std::fs::File,
    chunk: usize,
    delay: Duration,
    block_after: Option<u64>,
    returned: Arc<AtomicU64>,
    release: Arc<AtomicBool>,
}

impl Read for Throttled {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let so_far = self.returned.load(Ordering::SeqCst);
        if self.block_after.is_some_and(|b| so_far >= b) {
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        std::thread::sleep(self.delay);
        let n = buf.len().min(self.chunk);
        let n = self.inner.read(&mut buf[..n])?;
        self.returned.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

impl Seek for Throttled {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

// Records `(pass, bytes_done, bytes_total, at)` for every progress call, plus phase order.
#[derive(Default)]
struct Watch {
    progress: Mutex<Vec<(String, u64, u64, Instant)>>,
    phases: Mutex<Vec<String>>,
    cancel: AtomicBool,
}

impl Sink for Watch {
    fn progress(&self, p: &crate::sink::Progress) {
        let at = Instant::now();
        let row = (p.pass.to_string(), p.bytes_done, p.bytes_total, at);
        self.progress.lock().unwrap().push(row);
    }
    fn event(&self, e: &Event<'_>) {
        if let Event::Phase { name } = e {
            self.phases.lock().unwrap().push(name.to_string());
        }
    }
    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

impl Watch {
    fn of(&self, pass: &str) -> Vec<(u64, u64, Instant)> {
        let rows = self.progress.lock().unwrap();
        rows.iter()
            .filter(|r| r.0 == pass)
            .map(|r| (r.1, r.2, r.3))
            .collect()
    }
}

fn run(
    target: &Path,
    sink: &dyn Sink,
    op: &Halt,
    rio: &dyn RemuxIo,
    mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(op, None).with_sink(sink);
    let j = job(target.to_path_buf(), true);
    land_verified(&j, 0, &title(600.0), sink, &halt, rio, None, mux)
}

fn good() -> Vec<u8> {
    mkv(600.0, Some(598), 1)
}

// "`.partial` removed; target bytes and mtime unchanged; no `.lock` left" (ET14, ET16).
fn untouched(target: &Path, mtime: std::time::SystemTime) {
    assert_eq!(
        std::fs::read(target).unwrap(),
        b"old",
        "target bytes changed"
    );
    assert_eq!(
        std::fs::metadata(target).unwrap().modified().unwrap(),
        mtime
    );
    assert!(!partial_path(target).exists(), ".partial left behind");
    assert!(!sidecar_for(target).exists(), ".lock left behind");
}

fn old_target(dir: &Path) -> (PathBuf, std::time::SystemTime) {
    let target = dir.join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
    (target, mtime)
}

fn cancel_later(flag: impl FnOnce() + Send + 'static, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        flag();
    });
}

// ET12 `remux_takes_target_lock` — §4.2: "`land_verified` … takes `<target>.lock` … **before**
// creating `.partial`, so two remuxes of one library file cannot interleave."
#[test]
fn remux_takes_target_lock() {
    let dir = tempfile::tempdir().unwrap();
    let target = Arc::new(dir.path().join("Movie.mkv"));
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let second_muxed = Arc::new(AtomicBool::new(false));
    let t1 = target.clone();
    let first = std::thread::spawn(move || {
        run(&t1, &Events::default(), &Halt::new(), &OsRemuxIo, |dest| {
            let path = dest.strip_prefix("mkv://").unwrap().to_string();
            std::fs::write(&path, good())?;
            wait.recv().unwrap(); // holds the lock with `.partial` written
            Ok(outcome(true))
        })
    });
    let t0 = Instant::now();
    while !sidecar_for(&target).exists() && t0.elapsed() < STOP_LATENCY {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        sidecar_for(&target).exists(),
        "the first remux holds <target>.lock"
    );
    let (t2, seen) = (target.clone(), second_muxed.clone());
    let second = std::thread::spawn(move || {
        run(&t2, &Events::default(), &Halt::new(), &OsRemuxIo, |dest| {
            seen.store(true, Ordering::SeqCst);
            writes(mkv(600.0, Some(597), 2), true)(dest)
        })
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !second_muxed.load(Ordering::SeqCst),
        "two writers on <target>.partial"
    );
    go.send(()).unwrap();
    first.join().unwrap().expect("the first lands");
    let r = second
        .join()
        .unwrap()
        .expect("the second proceeds after the first");
    assert_eq!(
        r.verified.tracks.len(),
        2,
        "the target is the second's output"
    );
    assert!(!sidecar_for(&target).exists(), "<target>.lock is gone");
}

// ET12, the other arm — T10 (§3.1): "`TimedOut{artifact_lock}` (E9073)" after the window with
// no progress from a frozen holder.
#[test]
fn remux_lock_wait_times_out_on_a_frozen_holder() {
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let _held = DeleteOnDrop(Some(
        ArtifactLock::acquire(&target, &[], &Halt::new()).unwrap(),
    ));
    let rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    let muxed = AtomicBool::new(false);
    let e = run(&target, &Events::default(), &Halt::new(), &rio, |_| {
        muxed.store(true, Ordering::SeqCst);
        Ok(outcome(true))
    })
    .unwrap_err();
    assert_eq!(crate::error_code(&e), Some(libfreemkv::error::E_TIMED_OUT));
    assert!(!muxed.load(Ordering::SeqCst));
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(
        std::fs::metadata(&target).unwrap().modified().unwrap(),
        mtime
    );
    assert!(!partial_path(&target).exists());
}

// ET13 `remux_durable_sync_is_stall_based_and_stoppable` — T12b: "E9056 only on a true 60 s
// stall; Stop → `Halted`"; §5.0: "(a) progresses at 0.5 × window for ≥ 4 windows".
#[test]
fn remux_durable_sync_is_stall_based_and_stoppable() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let slow = SyncPlan {
        pieces: 10,
        gap: WINDOW / 2,
        stall_at: None,
    };
    let rio = FakeIo::new(slow, ReadPlan::default());
    let t0 = Instant::now();
    run(
        &target,
        &Events::default(),
        &Halt::new(),
        &rio,
        writes(good(), true),
    )
    .expect("a slow but progressing sync lands");
    assert!(t0.elapsed() > WINDOW * 2, "the sync ran past two windows");

    let (target, mtime) = old_target(dir.path());
    let stall = SyncPlan {
        pieces: 4,
        gap: Duration::ZERO,
        stall_at: Some(1),
    };
    let rio = FakeIo::new(stall, ReadPlan::default());
    let t0 = Instant::now();
    let e = run(
        &target,
        &Events::default(),
        &Halt::new(),
        &rio,
        writes(good(), true),
    );
    let e = e.unwrap_err();
    assert_eq!(
        crate::error_code(&e),
        Some(libfreemkv::error::E_SYNC_TIMEOUT)
    );
    assert!(t0.elapsed() < WINDOW + STOP_LATENCY);
    untouched(&target, mtime);

    let op = Halt::new();
    let stall = SyncPlan {
        pieces: 4,
        gap: Duration::from_secs(30),
        stall_at: None,
    };
    let rio = FakeIo::new(stall, ReadPlan::default());
    let o = op.clone();
    cancel_later(move || o.cancel(), Duration::from_millis(100));
    let t0 = Instant::now();
    let e = run(&target, &Events::default(), &op, &rio, writes(good(), true)).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert!(
        rio.in_sync.load(Ordering::SeqCst),
        "stopped inside the sync"
    );
    assert!(t0.elapsed() < STOP_LATENCY);
    untouched(&target, mtime);
}

// ET14 `remux_cancel_via_op_token_keeps_guarantees` — §4.2: "Cancel guarantees kept exactly,
// whether the cancel comes from `should_cancel` or the op token".
#[test]
fn remux_cancel_via_op_token_keeps_guarantees() {
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let op = Halt::new();
    let e = run(&target, &Events::default(), &op, &OsRemuxIo, |dest| {
        writes(good(), true)(dest)?;
        op.cancel(); // mid-mux: the mux stops incomplete
        Ok(outcome(false))
    })
    .unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    untouched(&target, mtime);
}

// ET14 on the real path: the op token reaches the mux itself (no Sink cancel at all).
#[test]
fn remux_iso_with_op_token_stops_the_real_mux() {
    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    struct CancelAtMux(Halt);
    impl Sink for CancelAtMux {
        fn event(&self, e: &Event<'_>) {
            if matches!(e, Event::Phase { name: "mux" }) {
                self.0.cancel();
            }
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let j = RemuxJob {
        iso: ImageSource::Iso(fx.write(dir.path(), "d.iso")),
        title: Some(1),
        streams: StreamChoice::default(),
        target: target.clone(),
        replace: true,
    };
    let op = Halt::new();
    let f = factory(&[(Answer::Online, &[K1, K2])], &Calls::default());
    let e = remux_iso_sources(&j, f, &CancelAtMux(op.clone()), &op).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    untouched(&target, mtime);
}

// ET16 `remux_should_cancel_still_cancels_everywhere` — §4.4: `Sink::should_cancel` is
// "observed during the mux **and** during the durable sync, the lock wait and verify".
// The legacy `remux_iso` (no `Halt`) runs exactly this: a never-cancelled op token.
#[test]
fn remux_should_cancel_still_cancels_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let flip = |w: &Arc<Watch>| {
        let w = w.clone();
        cancel_later(
            move || w.cancel.store(true, Ordering::SeqCst),
            Duration::from_millis(100),
        );
    };
    // (a) mid-mux.
    let (target, mtime) = old_target(dir.path());
    let w = Arc::new(Watch::default());
    let e = run(&target, &*w, &Halt::new(), &OsRemuxIo, |dest| {
        writes(good(), true)(dest)?;
        w.cancel.store(true, Ordering::SeqCst);
        Ok(outcome(false))
    });
    assert!(libfreemkv::is_halt(&e.unwrap_err()), "(a)");
    untouched(&target, mtime);
    // (b) during the durable sync, blocked for 3 × window.
    let w = Arc::new(Watch::default());
    let blocked = SyncPlan {
        pieces: 2,
        gap: WINDOW * 3,
        stall_at: None,
    };
    let rio = FakeIo::new(blocked, ReadPlan::default());
    flip(&w);
    let t0 = Instant::now();
    let e = run(&target, &*w, &Halt::new(), &rio, writes(good(), true)).unwrap_err();
    assert!(
        libfreemkv::is_halt(&e) && rio.in_sync.load(Ordering::SeqCst),
        "(b) {e}"
    );
    assert!(t0.elapsed() < STOP_LATENCY);
    untouched(&target, mtime);
    // (c) while waiting for the sidecar lock: no `.partial` is ever created.
    let held = DeleteOnDrop(Some(
        ArtifactLock::acquire(&target, &[], &Halt::new()).unwrap(),
    ));
    let w = Arc::new(Watch::default());
    let rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    flip(&w);
    let muxed = AtomicBool::new(false);
    let e = run(&target, &*w, &Halt::new(), &rio, |_| {
        muxed.store(true, Ordering::SeqCst);
        Ok(outcome(true))
    })
    .unwrap_err();
    assert!(libfreemkv::is_halt(&e), "(c) {e}");
    assert!(!muxed.load(Ordering::SeqCst), "(c) nothing muxed");
    drop(held);
    untouched(&target, mtime);
    // (d) during verify, the reader blocked.
    let w = Arc::new(Watch::default());
    let blocking = ReadPlan {
        chunk: 16,
        delay: Duration::ZERO,
        block_after: Some(16),
    };
    let mut rio = FakeIo::new(SyncPlan::default(), blocking);
    rio.timing.verify_stall = Duration::from_secs(30);
    flip(&w);
    let t0 = Instant::now();
    let e = run(&target, &*w, &Halt::new(), &rio, writes(good(), true)).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "(d) {e}");
    assert!(t0.elapsed() < STOP_LATENCY);
    untouched(&target, mtime);
}

// ET17 `remux_preempt_then_requeue_same_target` — §4.2: "The lock is released before
// `remux_iso` returns, so a canceller that immediately re-queues the same target … can
// acquire it at once."
#[test]
fn remux_preempt_then_requeue_same_target() {
    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    struct CancelAtMux(AtomicBool);
    impl Sink for CancelAtMux {
        fn event(&self, e: &Event<'_>) {
            if matches!(e, Event::Phase { name: "mux" }) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        fn should_cancel(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let j = RemuxJob {
        iso: ImageSource::Iso(fx.write(dir.path(), "d.iso")),
        title: Some(1),
        streams: StreamChoice::default(),
        target: target.clone(),
        replace: true,
    };
    let f = || factory(&[(Answer::Online, &[K1, K2])], &Calls::default());
    let never = Halt::new();
    let e = remux_iso_sources(&j, f(), &CancelAtMux(AtomicBool::new(false)), &never);
    assert!(libfreemkv::is_halt(&e.unwrap_err()));
    untouched(&target, mtime);
    let t0 = Instant::now();
    let r = remux_iso_sources(&j, f(), &crate::NoopSink, &never).expect("the re-queue lands");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "the lock was free at once"
    );
    assert!(r.replaced);
    assert_eq!(
        verify_mkv(&target, &fx.disc.titles[1])
            .unwrap()
            .tracks
            .len(),
        r.verified.tracks.len()
    );
    assert!(!sidecar_for(&target).exists());
}

// ET19 `remux_sync_emits_activity_while_healthy` — §4.4: `"sync"` progress "on **every**
// increase of `bytes_done`, rate-limited"; "`Event::Phase { name: "sync" }` (**new name**)".
#[test]
fn remux_sync_emits_activity_while_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let healthy = SyncPlan {
        pieces: 40,
        gap: Duration::from_millis(20),
        stall_at: None,
    };
    let rio = FakeIo::new(healthy, ReadPlan::default());
    let w = Watch::default();
    let started = Instant::now();
    run(&target, &w, &Halt::new(), &rio, writes(good(), true)).unwrap();
    let phases = w.phases.lock().unwrap().clone();
    assert_eq!(phases, ["mux", "sync", "verify", "replace"]);
    let sync = w.of("sync");
    assert!(sync.len() >= 10, "{} sync calls", sync.len());
    // Scaled "at least every 3 s" for ~2 s pieces: 5 × margin over the 20 ms pieces.
    let mut last = sync[0].2.max(started);
    for &(_, _, at) in &sync[1..] {
        assert!(at - last < Duration::from_millis(150), "a silent gap");
        last = at;
    }
    assert!(
        sync.windows(2).all(|p| p[0].0 < p[1].0),
        "bytes_done is monotonic"
    );
    let (done, total, _) = *sync.last().unwrap();
    assert_eq!(done, total, "the phase ends at the file length");
}

// ET20 `remux_sync_stall_goes_silent_then_fails` — §4.4: "no call while no bytes become
// durable. A truly stalled flush is silent … T12b fails with E9056 after 60 s".
#[test]
fn remux_sync_stall_goes_silent_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let stall = SyncPlan {
        pieces: 20,
        gap: Duration::from_millis(10),
        stall_at: Some(5),
    };
    let rio = FakeIo::new(stall, ReadPlan::default());
    let w = Watch::default();
    let e = run(&target, &w, &Halt::new(), &rio, writes(good(), true)).unwrap_err();
    assert_eq!(
        crate::error_code(&e),
        Some(libfreemkv::error::E_SYNC_TIMEOUT)
    );
    let sync = w.of("sync");
    let (done, total, _) = *sync.last().unwrap();
    assert!(
        done < total && sync.len() <= 5,
        "sync progress after the stall point"
    );
    untouched(&target, mtime);
}

// ET21 `remux_verify_emits_activity_while_healthy` — §4.5: "a counting reader that adds
// each successful read's byte count … Seeks do not count."
#[test]
fn remux_verify_emits_activity_while_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let slow = ReadPlan {
        chunk: 8,
        delay: Duration::from_millis(15),
        block_after: None,
    };
    let rio = FakeIo::new(SyncPlan::default(), slow);
    let w = Watch::default();
    run(&target, &w, &Halt::new(), &rio, writes(good(), true)).unwrap();
    let verify = w.of("verify");
    assert!(verify.len() >= 5, "{} verify calls", verify.len());
    assert!(
        verify
            .windows(2)
            .all(|p| p[0].0 < p[1].0 && p[1].2 - p[0].2 < Duration::from_millis(150))
    );
    let (done, total, _) = *verify.last().unwrap();
    assert_eq!(total, std::fs::metadata(&target).unwrap().len());
    assert!(
        done <= rio.returned.load(Ordering::SeqCst),
        "seeks add nothing"
    );
}

// ET22 `remux_verify_stall_goes_silent_then_fails` — T30: "60 s with no bytes read →
// `TimedOut { op: "verify" }` (E9073); `.partial` removed; target untouched; cancel → `Halted`".
#[test]
fn remux_verify_stall_goes_silent_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let blocks = ReadPlan {
        chunk: 16,
        delay: Duration::ZERO,
        block_after: Some(32),
    };
    let rio = FakeIo::new(SyncPlan::default(), blocks);
    let w = Watch::default();
    let t0 = Instant::now();
    let e = run(&target, &w, &Halt::new(), &rio, writes(good(), true)).unwrap_err();
    assert_eq!(crate::error_code(&e), Some(libfreemkv::error::E_TIMED_OUT));
    assert!(t0.elapsed() < WINDOW + STOP_LATENCY);
    assert!(
        w.of("verify").iter().all(|v| v.0 <= 32),
        "verify progress after the block"
    );
    untouched(&target, mtime);
}

// §4.2: "ST-E1 adds … `mux_image_titles_with(.., &Halt)`; the old functions call them with a
// never-cancelled private `Halt`".
#[test]
fn mux_image_titles_with_honours_the_op_token() {
    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let f = factory(&[(Answer::Online, &[K1, K2])], &Calls::default());
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    let dest = |i: usize| format!("mkv://{}", dir.path().join(format!("t{i}.mkv")).display());
    let op = Halt::new();
    op.cancel();
    let plan = MuxPlan::new(vec![1]);
    let out = mux_image_titles_with(&opened, &plan, &dest, &crate::NoopSink, &op);
    assert_eq!(out, RipOutcome::Halted);
    assert!(!dir.path().join("t1.mkv").exists());
    let out = mux_image_titles(&opened, &plan, &dest, &crate::NoopSink);
    assert_eq!(out, RipOutcome::Ok { titles_written: 1 });
}

// §4.5 on the real path: remux's explicit sync "calls `durable_sync_file` directly with a
// closure onto the same Sink" — the production primitive reports the `.partial` durable.
#[test]
fn remux_real_sync_reports_through_the_sink() {
    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let j = RemuxJob {
        iso: ImageSource::Iso(fx.write(dir.path(), "d.iso")),
        title: Some(1),
        streams: StreamChoice::default(),
        target: dir.path().join("Movie.mkv"),
        replace: false,
    };
    let w = Watch::default();
    let f = factory(&[(Answer::Online, &[K1, K2])], &Calls::default());
    remux_iso_sources(&j, f, &w, &Halt::new()).unwrap();
    assert_eq!(
        *w.phases.lock().unwrap(),
        ["open", "mux", "sync", "verify", "replace"]
    );
    let (done, total, _) = *w.of("sync").last().expect("sync activity");
    assert_eq!(
        (done, total),
        (total, std::fs::metadata(&j.target).unwrap().len())
    );
    let (done, total, _) = *w.of("verify").last().expect("verify activity");
    assert!(done > 0 && done <= total);
}

// ET11 `artifact_lock_survives_real_mapfile_flush` — §2.5: "A lock on the mapfile would pin
// the old inode after the first flush and exclude nothing"; SS-11 rename(): "a link named
// new shall remain visible to other threads throughout the renaming operation".
#[test]
fn artifact_lock_survives_real_mapfile_flush() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let mf = crate::mapfile_path_for(&iso);
    let mut map = crate::Mapfile::create(&mf, 1 << 20, "t").unwrap();
    let held = ArtifactLock::acquire(&iso, &[&mf], &Halt::new()).unwrap();
    let got = Arc::new(Mutex::new(None));
    let (iso2, mf2, got2) = (iso.clone(), mf.clone(), got.clone());
    let waiter = std::thread::spawn(move || {
        let r = ArtifactLock::acquire(&iso2, &[&mf2], &Halt::new());
        *got2.lock().unwrap() = Some(Instant::now());
        r
    });
    for i in 0..10u64 {
        map.record(i * 2048, 2048, crate::SectorStatus::Finished)
            .unwrap();
        map.flush().unwrap(); // the real tmp + rename
        std::thread::sleep(Duration::from_millis(50));
        assert!(got.lock().unwrap().is_none(), "acquired while held");
    }
    let released = Instant::now();
    drop(held);
    let lock = waiter.join().unwrap().expect("acquires once released");
    assert!(got.lock().unwrap().unwrap() >= released);
    assert_eq!(lock.path(), sidecar_for(&iso));
}

// §2.6: "**Done after Stop** only if the commit … happened before `t_cancel`" — the rename is
// the commit, so a Stop that lands before it is Halted with the target untouched.
#[test]
fn a_stop_before_the_rename_leaves_the_target() {
    struct CancelAtReplace(AtomicBool);
    impl Sink for CancelAtReplace {
        fn event(&self, e: &Event<'_>) {
            if matches!(e, Event::Phase { name: "replace" }) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        fn should_cancel(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let sink = CancelAtReplace(AtomicBool::new(false));
    let e = run(
        &target,
        &sink,
        &Halt::new(),
        &OsRemuxIo,
        writes(good(), true),
    )
    .unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    untouched(&target, mtime);
}

// §2.6 and §4.4: a Stop during the folder sync lands after the rename committed, so it cuts
// only the sync short and the remux is Done.
#[cfg(unix)]
#[test]
fn a_stop_after_the_rename_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let (target, _) = old_target(dir.path());
    let op = Halt::new();
    let mut rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    rio.dir_sync = DirSync::Stop(op.clone());
    let sink = Events::default();
    let r = run(&target, &sink, &op, &rio, writes(good(), true))
        .expect("the target was replaced before the Stop: Done");
    assert!(op.is_cancelled(), "the Stop reached the folder sync");
    assert!(r.replaced);
    assert!(sink.0.lock().unwrap().contains(&"replaced".to_string()));
    assert_eq!(std::fs::read(&target).unwrap(), good());
    assert!(!partial_path(&target).exists() && !sidecar_for(&target).exists());
}

// Once the rename committed, a failed folder sync cannot undo it: the caller is told the
// target was replaced (Done, with `Event::Replaced`), not that the remux failed.
#[cfg(unix)]
#[test]
fn a_failed_folder_sync_after_the_rename_still_reports_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let (target, _) = old_target(dir.path());
    let mut rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    rio.dir_sync = DirSync::Fail;
    let sink = Events::default();
    let r = run(&target, &sink, &Halt::new(), &rio, writes(good(), true))
        .expect("the rename committed: Done");
    assert!(r.replaced);
    assert!(sink.0.lock().unwrap().contains(&"replaced".to_string()));
    assert_eq!(std::fs::read(&target).unwrap(), good());
    assert!(!partial_path(&target).exists() && !sidecar_for(&target).exists());
}

// T10 (§2.5, §3.1): a waiter on `<target>.lock` reads the holder's progress as the size or
// mtime of `<target>.partial`, so a slow but healthy sync and verify must keep changing it.
#[test]
fn remux_holder_stays_observable_during_sync_and_verify() {
    struct Stamps {
        partial: PathBuf,
        at: Mutex<Vec<(String, std::time::SystemTime)>>,
    }
    impl Sink for Stamps {
        fn event(&self, e: &Event<'_>) {
            if let Event::Phase { name } = e {
                let m = std::fs::metadata(&self.partial).and_then(|m| m.modified());
                if let Ok(m) = m {
                    self.at.lock().unwrap().push((name.to_string(), m));
                }
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let slow_sync = SyncPlan {
        pieces: 10,
        gap: Duration::from_millis(20),
        stall_at: None,
    };
    let slow_read = ReadPlan {
        chunk: 8,
        delay: Duration::from_millis(5),
        block_after: None,
    };
    let rio = FakeIo::new(slow_sync, slow_read);
    let sink = Stamps {
        partial: partial_path(&target),
        at: Mutex::default(),
    };
    run(&target, &sink, &Halt::new(), &rio, writes(good(), true)).unwrap();
    let at = sink.at.lock().unwrap().clone();
    let names: Vec<&str> = at.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["sync", "verify", "replace"]);
    assert_ne!(
        at[0].1, at[1].1,
        "no holder progress visible during the sync"
    );
    assert_ne!(at[1].1, at[2].1, "no holder progress visible during verify");
}

// `run` for the staged path: mux into `stage`, copy beside the target, then replace.
fn run_staged(
    target: &Path,
    stage: &Path,
    sink: &dyn Sink,
    op: &Halt,
    rio: &dyn RemuxIo,
    mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(op, None).with_sink(sink);
    let j = job(target.to_path_buf(), true);
    land_verified(&j, 0, &title(600.0), sink, &halt, rio, Some(stage), mux)
}

// The staged copy to the library share is a wait like every other (§3.1 HR1, §4.2): a Stop
// ends a copy blocked in a write within the Stop latency, a copy with no bytes written for
// the window fails E9073, and the target is untouched either way.
#[test]
fn remux_staged_copy_is_stall_based_and_stoppable() {
    let dir = tempfile::tempdir().unwrap();
    let (target, mtime) = old_target(dir.path());
    let stage = dir.path().join("7.mkv.partial");
    let mut rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    rio.write.block_after = Some(16);
    // Unblocks the write long after the Stop, so a copy that ignores Stop still ends.
    let release = rio.release.clone();
    cancel_later(
        move || release.store(true, Ordering::SeqCst),
        STOP_LATENCY * 3,
    );
    let op = Halt::new();
    let o = op.clone();
    cancel_later(move || o.cancel(), Duration::from_millis(100));
    let t0 = Instant::now();
    let e = run_staged(
        &target,
        &stage,
        &Events::default(),
        &op,
        &rio,
        writes(good(), true),
    );
    let e = e.unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert!(
        t0.elapsed() < STOP_LATENCY,
        "the Stop waited out the blocked write"
    );
    untouched(&target, mtime);
    assert!(!stage.exists());

    let mut rio = FakeIo::new(SyncPlan::default(), ReadPlan::default());
    rio.write.block_after = Some(16);
    let never = Halt::new();
    let t0 = Instant::now();
    let e = run_staged(
        &target,
        &stage,
        &Events::default(),
        &never,
        &rio,
        writes(good(), true),
    );
    let e = e.unwrap_err();
    let stalled = libfreemkv::Error::TimedOut { op: "copy" };
    assert_eq!(e.to_string(), stalled.to_string());
    assert!(t0.elapsed() < WINDOW + STOP_LATENCY);
    untouched(&target, mtime);
    assert!(!stage.exists());
}
