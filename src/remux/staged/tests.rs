//! Keeping a verified staged remux after a storage failure, and finishing it later.

use super::*;
use crate::remux::tests::{Events, job, mkv, outcome, title, writes};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};

// The file primitives with one injected fault each; `release` ends a stalled write.
#[derive(Default)]
struct TestIo {
    syncs: AtomicUsize,
    fail_sync_at: Option<usize>,
    reads: AtomicUsize,
    fail_read_at: Option<usize>,
    stall_write: bool,
    fail_write: bool,
    // Made read-only once the library copy exists, so its removal fails.
    #[cfg_attr(not(unix), allow(dead_code))]
    freeze_dir: Option<PathBuf>,
    release: Release,
}

// Set when the test's io goes, so a copy worker left stalled ends.
#[derive(Default)]
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct Stalling(std::fs::File, Arc<AtomicBool>);

impl Write for Stalling {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        while !self.1.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

struct Failing;

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(5))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RemuxIo for TestIo {
    fn sync(&self, file: &std::fs::File, _: &Halt, _: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        if file.metadata()?.is_dir() {
            return Ok(());
        }
        let n = self.syncs.fetch_add(1, Ordering::SeqCst);
        match self.fail_sync_at == Some(n) {
            true => Err(io::Error::from_raw_os_error(5)),
            false => Ok(()),
        }
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_read_at == Some(n) {
            return Err(io::Error::from_raw_os_error(5));
        }
        OsRemuxIo.open_read(path)
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        #[cfg(unix)]
        if let Some(dir) = &self.freeze_dir {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))?;
        }
        if self.fail_write {
            return Ok(Box::new(Failing));
        }
        if self.stall_write {
            return Ok(Box::new(Stalling(file, self.release.0.clone())));
        }
        Ok(Box::new(file))
    }

    fn timing(&self) -> RemuxTiming {
        RemuxTiming {
            verify_stall: Duration::from_secs(2),
            copy_stall: Duration::from_millis(300),
            activity_every: Duration::from_millis(10),
            lock_beat: Duration::from_millis(10),
        }
    }
}

// Events as `Events` records them, the log lines, and a Stop raised on a named phase.
#[derive(Default)]
struct Rec {
    events: Events,
    logs: Mutex<Vec<String>>,
    stop_on: Option<&'static str>,
    stopped: AtomicBool,
}

impl Sink for Rec {
    fn log(&self, _: Level, msg: &str) {
        self.logs.lock().unwrap().push(msg.to_string());
    }
    fn title_opened(&self, t: &libfreemkv::DiscTitle) {
        self.events.title_opened(t);
    }
    fn event(&self, e: &Event<'_>) {
        if let Event::Phase { name } = e
            && Some(*name) == self.stop_on
        {
            self.stopped.store(true, Ordering::SeqCst);
        }
        self.events.event(e);
    }
    fn should_cancel(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

impl Rec {
    fn events(&self) -> Vec<String> {
        self.events.0.lock().unwrap().clone()
    }
}

struct Layout {
    _dir: tempfile::TempDir,
    stage: PathBuf,
    target: PathBuf,
    partial: PathBuf,
}

fn layout() -> Layout {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("ssd");
    let nas = dir.path().join("nas");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    let target = nas.join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let partial = stage.join("7.mkv.partial");
    Layout {
        _dir: dir,
        stage,
        target,
        partial,
    }
}

fn good() -> Vec<u8> {
    mkv(600.0, Some(598), 2)
}

fn remux(
    l: &Layout,
    sink: &dyn Sink,
    rio: &dyn RemuxIo,
    bytes: Vec<u8>,
) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(sink);
    let j = job(l.target.clone(), true);
    let mux = writes(bytes, true);
    land_verified(
        &j,
        0,
        &title(600.0),
        sink,
        &halt,
        rio,
        Some(&l.partial),
        mux,
    )
}

fn finish(path: &Path, sink: &dyn Sink, rio: &dyn RemuxIo) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(sink);
    finish_at(path, sink, &halt, rio)
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// The old target, no library copy and no lock left beside it.
fn target_untouched(l: &Layout) {
    assert_eq!(std::fs::read(&l.target).unwrap(), b"old");
    assert!(
        !partial_path(&l.target).exists(),
        "library copy left behind"
    );
    let lock = libfreemkv::io::artifact_lock::lock_path(&l.target);
    assert!(!lock.exists(), "lock left behind");
}

// The failure was kept: the MKV holds the muxed bytes, the sidecar says why, nothing else.
fn assert_kept(l: &Layout, e: &io::Error, phase: &str) -> StagedInfo {
    let kept = staged_kept(e).unwrap_or_else(|| panic!("not kept: {e}"));
    assert_eq!(kept.phase, phase);
    assert_eq!(
        e.to_string(),
        kept.cause.to_string(),
        "Display is the cause's"
    );
    assert_eq!(e.kind(), kept.cause.kind());
    assert_eq!(crate::error_code(e), crate::error_code(&kept.cause));
    assert_eq!(std::fs::read(&kept.path).unwrap(), good());
    assert!(!l.partial.exists());
    let info = read_staged(&kept.sidecar).unwrap();
    assert_eq!(info.staged, kept.path);
    assert_eq!(info.sidecar, kept.sidecar);
    assert_eq!(info.target, l.target);
    assert!(info.replace);
    assert_eq!(info.title, 0);
    assert!(info.streams.is_all());
    assert_eq!(info.size, good().len() as u64);
    assert_eq!(info.runtime_secs, Some(598.0));
    assert_eq!(info.expected_secs, Some(600.0));
    assert_eq!(info.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
    assert_eq!(info.engine_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(info.attempts, 1);
    assert_eq!(info.failed_phase, phase);
    assert_eq!(info.error, e.to_string());
    assert_eq!(info.error_code, crate::error_code(e));
    assert_eq!(info.source.url(), "iso:///nonexistent/freemkv/none.iso");
    let mut names = files_in(&l.stage);
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.iter().all(|n| n.starts_with("Movie.")), "{names:?}");
    assert!(is_kept_staged(&info.staged) && is_kept_staged(&info.sidecar));
    info
}

fn nothing_kept(l: &Layout) {
    assert_eq!(files_in(&l.stage), Vec::<String>::new());
}

#[test]
fn a_stalled_copy_keeps_the_verified_file() {
    let l = layout();
    let rio = TestIo {
        stall_write: true,
        ..TestIo::default()
    };
    let sink = Rec::default();
    let e = remux(&l, &sink, &rio, good()).unwrap_err();
    let stalled = libfreemkv::Error::TimedOut { op: "copy" };
    assert_eq!(e.to_string(), stalled.to_string());
    assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    let info = assert_kept(&l, &e, "copy");
    assert_eq!(info.error_code, Some(stalled.code()));
    target_untouched(&l);
    let logs = sink.logs.lock().unwrap().clone();
    assert!(
        logs.iter().any(|m| m.contains("kept the verified file")),
        "{logs:?}"
    );
}

#[test]
fn a_failed_library_sync_keeps_the_verified_file() {
    let l = layout();
    let rio = TestIo {
        fail_sync_at: Some(1),
        ..TestIo::default()
    };
    let e = remux(&l, &Rec::default(), &rio, good()).unwrap_err();
    assert_eq!(e.raw_os_error(), None, "wrapped");
    let kept = staged_kept(&e).unwrap();
    assert_eq!(kept.cause.raw_os_error(), Some(5));
    assert_kept(&l, &e, "sync");
    target_untouched(&l);
}

#[test]
fn a_library_copy_that_cannot_be_read_back_keeps_the_verified_file() {
    let l = layout();
    let rio = TestIo {
        fail_read_at: Some(1),
        ..TestIo::default()
    };
    let e = remux(&l, &Rec::default(), &rio, good()).unwrap_err();
    assert_kept(&l, &e, "verify");
    target_untouched(&l);
}

#[test]
fn a_failed_rename_keeps_the_verified_file() {
    let l = layout();
    // A non-empty folder at the target: the rename over it fails.
    std::fs::remove_file(&l.target).unwrap();
    std::fs::create_dir(&l.target).unwrap();
    std::fs::write(l.target.join("inside"), b"x").unwrap();
    let e = remux(&l, &Rec::default(), &TestIo::default(), good()).unwrap_err();
    assert_kept(&l, &e, "replace");
    assert!(l.target.join("inside").exists());
    assert!(!partial_path(&l.target).exists());
}

// A dead mount may refuse to remove the library copy: logged, and the local file is still kept.
#[cfg(unix)]
#[test]
fn a_library_copy_that_cannot_be_removed_is_tolerated() {
    use std::os::unix::fs::PermissionsExt;
    let l = layout();
    let nas = l.target.parent().unwrap().to_path_buf();
    let rio = TestIo {
        fail_write: true,
        freeze_dir: Some(nas.clone()),
        ..TestIo::default()
    };
    let sink = Rec::default();
    let result = remux(&l, &sink, &rio, good());
    std::fs::set_permissions(&nas, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = result.unwrap_err();
    assert_kept(&l, &e, "copy");
    let logs = sink.logs.lock().unwrap().clone();
    let refused = partial_path(&l.target).exists();
    if refused {
        assert!(
            logs.iter().any(|m| m.contains("could not remove")),
            "{logs:?}"
        );
    }
    assert_eq!(std::fs::read(&l.target).unwrap(), b"old");
}

#[test]
fn a_local_verify_failure_keeps_nothing() {
    let l = layout();
    let rio = TestIo {
        fail_sync_at: Some(1),
        ..TestIo::default()
    };
    let e = remux(&l, &Rec::default(), &rio, mkv(600.0, Some(100), 1)).unwrap_err();
    assert!(staged_kept(&e).is_none());
    assert_eq!(
        crate::error_code(&e),
        Some(libfreemkv::error::E_REMUX_VERIFY_FAILED)
    );
    nothing_kept(&l);
    target_untouched(&l);
}

#[test]
fn an_incomplete_mux_keeps_nothing() {
    let l = layout();
    let sink = Rec::default();
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let j = job(l.target.clone(), true);
    let e = land_verified(
        &j,
        0,
        &title(600.0),
        &sink,
        &halt,
        &TestIo::default(),
        Some(&l.partial),
        writes(good(), false),
    )
    .unwrap_err();
    assert!(staged_kept(&e).is_none());
    assert_eq!(e.to_string(), "E9078: 1");
    nothing_kept(&l);
    target_untouched(&l);
}

#[test]
fn a_stop_keeps_nothing() {
    for phase in ["copy", "sync", "replace"] {
        let l = layout();
        let rio = TestIo {
            stall_write: phase == "copy",
            ..TestIo::default()
        };
        let sink = Rec {
            stop_on: Some(phase),
            ..Rec::default()
        };
        // "sync" is first raised for the local file: the Stop lands there, before any copy.
        let e = remux(&l, &sink, &rio, good()).unwrap_err();
        assert!(libfreemkv::is_halt(&e), "{phase}: {e}");
        assert!(staged_kept(&e).is_none());
        nothing_kept(&l);
        target_untouched(&l);
    }
}

// The success path is unchanged: same bytes, same phases in the same order, nothing kept.
#[test]
fn a_staged_remux_that_succeeds_keeps_nothing() {
    let l = layout();
    let sink = Rec::default();
    let report = remux(&l, &sink, &TestIo::default(), good()).unwrap();
    assert!(report.replaced);
    assert_eq!(std::fs::read(&l.target).unwrap(), good());
    nothing_kept(&l);
    assert_eq!(
        sink.events(),
        [
            "title",
            "phase:mux",
            "start:0",
            "done:0:true",
            "phase:sync",
            "phase:verify",
            "verify:true",
            "phase:copy",
            "phase:sync",
            "phase:verify",
            "verify:true",
            "phase:replace",
            "replaced"
        ]
    );
}

fn kept_after_sync_failure(l: &Layout) -> StagedInfo {
    let rio = TestIo {
        fail_sync_at: Some(1),
        ..TestIo::default()
    };
    let e = remux(l, &Rec::default(), &rio, good()).unwrap_err();
    assert_kept(l, &e, "sync")
}

#[test]
fn finish_staged_lands_the_kept_file_and_removes_it() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    let sink = Rec::default();
    let report = finish(&info.sidecar, &sink, &TestIo::default()).unwrap();
    assert!(report.replaced);
    assert_eq!(report.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
    assert!(report.outcome.completed);
    assert_eq!(report.outcome.bytes_written, outcome(true).bytes_written);
    assert_eq!(std::fs::read(&l.target).unwrap(), good());
    nothing_kept(&l);
    assert!(!partial_path(&l.target).exists());
    assert_eq!(
        sink.events(),
        [
            "phase:verify",
            "verify:true",
            "phase:copy",
            "phase:sync",
            "phase:verify",
            "verify:true",
            "phase:replace",
            "replaced"
        ]
    );
}

#[test]
fn finish_staged_that_fails_again_keeps_it_again() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    let rio = TestIo {
        stall_write: true,
        ..TestIo::default()
    };
    let e = finish(&info.staged, &Rec::default(), &rio).unwrap_err();
    let kept = staged_kept(&e).expect("kept again");
    assert_eq!((kept.phase, &kept.path), ("copy", &info.staged));
    let again = read_staged(&info.sidecar).unwrap();
    assert_eq!(again.attempts, 2);
    assert_eq!(again.failed_phase, "copy");
    assert_eq!(
        again.error,
        libfreemkv::Error::TimedOut { op: "copy" }.to_string()
    );
    assert_eq!(again.created_at, info.created_at);
    assert_eq!(std::fs::read(&info.staged).unwrap(), good(), "not emptied");
    assert_eq!(files_in(&l.stage).len(), 2);
    target_untouched(&l);
    // A Stop leaves the pair as it is.
    let sink = Rec {
        stop_on: Some("copy"),
        ..Rec::default()
    };
    let rio = TestIo {
        stall_write: true,
        ..TestIo::default()
    };
    let e = finish(&info.staged, &sink, &rio).unwrap_err();
    assert!(libfreemkv::is_halt(&e) && staged_kept(&e).is_none(), "{e}");
    assert_eq!(read_staged(&info.sidecar).unwrap().attempts, 2);
    assert_eq!(std::fs::read(&info.staged).unwrap(), good());
    // And the next try lands it.
    finish(&info.staged, &Rec::default(), &TestIo::default()).unwrap();
    assert_eq!(std::fs::read(&l.target).unwrap(), good());
    nothing_kept(&l);
}

#[test]
fn finish_staged_refuses_a_target_that_changed_and_keeps_the_pair() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    std::fs::write(&l.target, b"another rip").unwrap();
    let e = finish(&info.staged, &Rec::default(), &TestIo::default()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    let code = libfreemkv::error::E_REMUX_TARGET_EXISTS;
    assert_eq!(crate::error_code(&e), Some(code));
    assert!(staged_kept(&e).is_none());
    assert_eq!(std::fs::read(&l.target).unwrap(), b"another rip");
    assert!(is_kept_staged(&info.staged));
    // A target that is gone is simply landed.
    std::fs::remove_file(&l.target).unwrap();
    let report = finish(&info.staged, &Rec::default(), &TestIo::default()).unwrap();
    assert!(!report.replaced);
    assert_eq!(std::fs::read(&l.target).unwrap(), good());
}

#[test]
fn finish_staged_discards_a_damaged_sidecar() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    std::fs::write(&info.sidecar, b"{ not json").unwrap();
    assert!(!is_kept_staged(&info.staged));
    assert_eq!(staged_orphans(&l.stage).len(), 2);
    let e = finish(&info.staged, &Rec::default(), &TestIo::default()).unwrap_err();
    let code = libfreemkv::error::E_REMUX_STAGING_INVALID;
    assert_eq!(crate::error_code(&e), Some(code));
    nothing_kept(&l);
    target_untouched(&l);
}

#[test]
fn finish_staged_discards_a_truncated_or_damaged_file() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&info.staged)
        .unwrap();
    f.set_len(info.size / 2).unwrap();
    drop(f);
    let e = finish(&info.staged, &Rec::default(), &TestIo::default()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    let code = libfreemkv::error::E_STAGED_COPY_SIZE_MISMATCH;
    assert_eq!(crate::error_code(&e), Some(code));
    nothing_kept(&l);

    // Same size, not an MKV any more.
    let info = kept_after_sync_failure(&l);
    std::fs::write(&info.staged, vec![0u8; info.size as usize]).unwrap();
    let sink = Rec::default();
    let e = finish(&info.staged, &sink, &TestIo::default()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert_eq!(sink.events(), ["phase:verify", "verify:false"]);
    nothing_kept(&l);
    target_untouched(&l);
}

#[test]
fn finish_staged_without_its_mkv_drops_the_sidecar() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    std::fs::remove_file(&info.staged).unwrap();
    let e = finish(&info.sidecar, &Rec::default(), &TestIo::default()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    nothing_kept(&l);
}

#[test]
fn a_path_that_is_not_a_kept_remux_is_refused() {
    let l = layout();
    let code = Some(libfreemkv::error::E_REMUX_STAGING_INVALID);
    let e = finish(&l.target, &Rec::default(), &TestIo::default()).unwrap_err();
    assert_eq!(crate::error_code(&e), code);
    let e = discard_staged(&l.target).unwrap_err();
    assert_eq!(crate::error_code(&e), code);
    assert_eq!(std::fs::read(&l.target).unwrap(), b"old");
    assert!(!is_kept_staged(&l.target));
    assert!(!is_kept_staged(&l.stage.join(".staged.mkv")));
}

#[test]
fn staged_pending_lists_kept_pairs_and_ignores_garbage() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    let s = &l.stage;
    std::fs::write(s.join("junk.staged.json"), b"[1,2").unwrap();
    std::fs::write(s.join("junk.staged.mkv"), b"x").unwrap();
    std::fs::write(s.join("lonely.staged.mkv"), b"x").unwrap();
    std::fs::copy(&info.sidecar, s.join("nomkv.staged.json")).unwrap();
    std::fs::write(s.join("half.staged.json.tmp"), b"{").unwrap();
    std::fs::write(s.join("future.staged.json"), br#"{"format": 99}"#).unwrap();
    std::fs::write(s.join("future.staged.mkv"), b"x").unwrap();
    std::fs::write(s.join("42.mkv.partial"), b"x").unwrap();
    std::fs::write(s.join("notes.txt"), b"x").unwrap();
    std::fs::create_dir(s.join("dir.staged.json")).unwrap();
    let pending = staged_pending(s);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].staged, info.staged);
    let orphans: Vec<String> = staged_orphans(s)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        orphans,
        [
            "half.staged.json.tmp",
            "junk.staged.json",
            "junk.staged.mkv",
            "lonely.staged.mkv",
            "nomkv.staged.json"
        ]
    );
    // A newer format's pair is someone else's: never pending, never debris.
    assert!(is_kept_staged(&s.join("future.staged.mkv")));
    assert!(staged_pending(&s.join("missing")).is_empty());
    discard_staged(&info.sidecar).unwrap();
    assert!(staged_pending(s).is_empty());
    assert!(!info.staged.exists() && !info.sidecar.exists());
}

#[test]
fn one_kept_file_per_target() {
    let l = layout();
    let first = kept_after_sync_failure(&l);
    let second = kept_after_sync_failure(&l);
    assert_eq!(first.staged, second.staged);
    assert_eq!(staged_pending(&l.stage).len(), 1);
    let (a, _) = kept_paths(&l.stage, Path::new("/a/Movie.mkv"));
    let (b, _) = kept_paths(&l.stage, Path::new("/b/Movie.mkv"));
    assert_ne!(a, b);
    let (odd, _) = kept_paths(&l.stage, Path::new("/x/a:b*c.mkv"));
    let name = odd.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("a_b_c.") && name.ends_with(".staged.mkv"),
        "{name}"
    );
}

#[test]
fn expiry_and_budget_pick_the_oldest() {
    let l = layout();
    let info = kept_after_sync_failure(&l);
    assert!(!staged_expired(&info, Duration::from_secs(3600)));
    let mut old = info.clone();
    old.created_at = SystemTime::now() - Duration::from_secs(7200);
    assert!(staged_expired(&old, Duration::from_secs(3600)));
    let mut future = info.clone();
    future.created_at = SystemTime::now() + Duration::from_secs(7200);
    assert!(!staged_expired(&future, Duration::ZERO));
    let mut a = info.clone();
    a.size = 10;
    a.created_at = UNIX_EPOCH + Duration::from_secs(1);
    let mut b = info.clone();
    b.size = 20;
    b.created_at = UNIX_EPOCH + Duration::from_secs(2);
    let mut c = info;
    c.size = 30;
    c.created_at = UNIX_EPOCH + Duration::from_secs(3);
    let all = [c, a, b];
    let sizes = |v: Vec<&StagedInfo>| v.iter().map(|i| i.size).collect::<Vec<_>>();
    assert_eq!(sizes(staged_over_budget(&all, 60)), Vec::<u64>::new());
    assert_eq!(sizes(staged_over_budget(&all, 50)), [10]);
    assert_eq!(sizes(staged_over_budget(&all, 29)), [10, 20, 30]);
    assert_eq!(sizes(staged_over_budget(&all, 30)), [10, 20]);
}
