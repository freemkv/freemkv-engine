use super::*;
use std::sync::Mutex;

// A one-title image (its clip encrypted with K1) opened with its keys, and the sectors of
// its one clip.
fn opened_image(dir: &Path) -> (OpenedImage, PathBuf, (u32, u32)) {
    use crate::test_fixtures::{Answer, Calls, K1, bd_image, resolve};
    let fx = bd_image(&[Some(K1)], 1);
    let iso = fx.write(dir, "d.iso");
    let keydb = [(Answer::Keydb, &[K1][..])];
    let set = resolve(&fx, KeyScope::Titles(vec![0]), &keydb, &Calls::default()).unwrap();
    let source = ImageSource::Iso(iso.clone());
    let opened = crate::open_image_with(&source, OpenImageOptions::known(set)).unwrap();
    (opened, iso, fx.clip(0))
}

// A sink with hooks: `on_start` runs at the title's first cancel poll after its TitleStart
// (inside the mux, before the output opens), which then reports a Stop; `on_opened` runs
// when the mux opens its output.
type Hook<'a> = &'a (dyn Fn() + Sync);
struct Hooked<'a> {
    on_start: Option<Hook<'a>>,
    on_opened: Hook<'a>,
    started: AtomicBool,
    stopped: AtomicBool,
}

impl<'a> Hooked<'a> {
    fn new(on_start: Option<Hook<'a>>, on_opened: Hook<'a>) -> Self {
        Hooked {
            on_start,
            on_opened,
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    fn stop_at_start(on_start: Hook<'a>) -> Self {
        Hooked::new(Some(on_start), &|| {})
    }
}

impl Sink for Hooked<'_> {
    fn event(&self, e: &Event<'_>) {
        match e {
            Event::TitleStart { .. } => self.started.store(true, Ordering::SeqCst),
            Event::OutputOpened { .. } => (self.on_opened)(),
            _ => {}
        }
    }
    fn should_cancel(&self) -> bool {
        if self.stopped.load(Ordering::SeqCst) {
            return true;
        }
        match self.on_start {
            Some(hook) if self.started.load(Ordering::SeqCst) => {
                hook();
                self.stopped.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }
}

// A title stopped before its mux opened the output never deletes the file at its path, even
// one another run is writing meanwhile (its stamp changes) or one that appears there after
// the title started (a concurrent rip to the same path, a stale negative lookup).
#[test]
fn a_title_that_never_opened_its_output_never_deletes_the_file_there() {
    let dir = tempfile::tempdir().unwrap();
    let (opened, ..) = opened_image(dir.path());
    let out = dir.path().join("t0.mkv");
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    // Another writer grows the file at the path while this title runs.
    std::fs::write(&out, b"another run's").unwrap();
    let grow = || {
        let mut f = std::fs::OpenOptions::new().append(true).open(&out).unwrap();
        f.write_all(b" growing rip").unwrap();
    };
    let sink = Hooked::stop_at_start(&grow);
    let r = mux_image_titles(&opened, &plan, &dest, &sink);
    assert_eq!(r, RipOutcome::Halted);
    assert_eq!(std::fs::read(&out).unwrap(), b"another run's growing rip");
    // The path was absent at title start; another writer creates it before the Stop.
    std::fs::remove_file(&out).unwrap();
    let create = || std::fs::write(&out, b"appeared").unwrap();
    let sink = Hooked::stop_at_start(&create);
    let r = mux_image_titles(&opened, &plan, &dest, &sink);
    assert_eq!(r, RipOutcome::Halted);
    assert_eq!(std::fs::read(&out).unwrap(), b"appeared");
}

// Through the title loop: a title whose mux opened (so truncated) its output and then failed
// (a write error, no outcome) or stopped removes what it left there, known by its watch.
#[test]
fn a_title_that_opened_its_output_and_then_failed_or_stopped_removes_it() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t0.mkv");
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    let opens = |dest: &str, sink: &dyn Sink| {
        let path = Path::new(dest.strip_prefix("mkv://").unwrap());
        std::fs::write(path, b"half a title").unwrap();
        sink.event(&Event::OutputOpened {
            dest,
            title: &libfreemkv::DiscTitle::empty(),
            file: file_id(path),
        });
    };
    let (token, sink) = (Halt::new(), Events::default());
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let failed = |_, _, dest: &str, sink: &dyn Sink| {
        opens(dest, sink);
        Err(io::Error::other("E6000: 12"))
    };
    std::fs::write(&out, b"earlier rip").unwrap();
    let r = mux_titles(&plan, &dest, &sink, &halt, failed);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert!(!out.exists(), "the failed title's partial is removed");
    let stopped = |_, _, dest: &str, sink: &dyn Sink| {
        opens(dest, sink);
        Ok(libfreemkv::MuxOutcome {
            halted: true,
            ..outcome(false)
        })
    };
    std::fs::write(&out, b"earlier rip").unwrap();
    let r = mux_titles(&plan, &dest, &sink, &halt, stopped);
    assert_eq!(r, RipOutcome::Halted);
    assert!(!out.exists(), "the stopped title's partial is removed");
}

// A title whose mux opened its output removes only that file: one another writer put at the
// path after the open (a finished rip renamed into place) stays when the title fails or stops.
#[test]
fn a_file_replaced_after_the_open_is_never_removed() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t0.mkv");
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    let (token, sink) = (Halt::new(), Events::default());
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let replaced = |dest: &str, sink: &dyn Sink| {
        let path = dest.strip_prefix("mkv://").unwrap();
        std::fs::write(path, b"half a title").unwrap();
        let title = libfreemkv::DiscTitle::empty();
        sink.event(&Event::OutputOpened {
            dest,
            title: &title,
            file: file_id(Path::new(path)),
        });
        let other = dir.path().join("other.mkv");
        std::fs::write(&other, b"another rip").unwrap();
        std::fs::rename(&other, path).unwrap();
    };
    let failed = |_, _, dest: &str, sink: &dyn Sink| {
        replaced(dest, sink);
        Err(io::Error::other("E6000: 12"))
    };
    let r = mux_titles(&plan, &dest, &sink, &halt, failed);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"another rip");
    let stopped = |_, _, dest: &str, sink: &dyn Sink| {
        replaced(dest, sink);
        let halted = libfreemkv::MuxOutcome {
            halted: true,
            ..outcome(false)
        };
        Ok(halted)
    };
    let r = mux_titles(&plan, &dest, &sink, &halt, stopped);
    assert_eq!(r, RipOutcome::Halted);
    assert_eq!(std::fs::read(&out).unwrap(), b"another rip");
}

// The watch keeps the identity the mux read from its own handle, not the file at the path
// when the event arrives: one renamed in before a late event stays, and an opening that
// reports no identity removes nothing.
#[test]
fn the_watch_keeps_the_identity_the_mux_reports() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("t0.mkv");
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    let (token, sink) = (Halt::new(), Events::default());
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let title = libfreemkv::DiscTitle::empty();
    let late = |_, _, dest: &str, sink: &dyn Sink| {
        let path = Path::new(dest.strip_prefix("mkv://").unwrap());
        std::fs::write(path, b"half a title").unwrap();
        let file = file_id(path);
        let other = dir.path().join("other.mkv");
        std::fs::write(&other, b"another rip").unwrap();
        std::fs::rename(&other, path).unwrap();
        let title = &title;
        sink.event(&Event::OutputOpened { dest, title, file });
        Err(io::Error::other("E6000: 12"))
    };
    let r = mux_titles(&plan, &dest, &sink, &halt, late);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"another rip");
    let unknown = |_, _, dest: &str, sink: &dyn Sink| {
        std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half a title").unwrap();
        let (title, file) = (&title, None);
        sink.event(&Event::OutputOpened { dest, title, file });
        Err(io::Error::other("E6000: 12"))
    };
    let r = mux_titles(&plan, &dest, &sink, &halt, unknown);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert_eq!(std::fs::read(&out).unwrap(), b"half a title");
}

// A destination that is a symlink is never removed: the link stays (the run did not create
// it), and so does the file the mux wrote through it.
#[cfg(unix)]
#[test]
fn a_symlinked_destination_is_never_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (out, pool) = (dir.path().join("t0.mkv"), dir.path().join("pool.mkv"));
    std::os::unix::fs::symlink(&pool, &out).unwrap();
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    let (token, sink) = (Halt::new(), Events::default());
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let failed = |_, _, dest: &str, sink: &dyn Sink| {
        let mut f = std::fs::File::create(dest.strip_prefix("mkv://").unwrap()).unwrap();
        f.write_all(b"half a title").unwrap();
        let file = libfreemkv::io::artifact_lock::file_identity_of(&f).ok();
        let title = &libfreemkv::DiscTitle::empty();
        sink.event(&Event::OutputOpened { dest, title, file });
        Err(io::Error::other("E6000: 12"))
    };
    let r = mux_titles(&plan, &dest, &sink, &halt, failed);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert!(std::fs::symlink_metadata(&out).unwrap().is_symlink());
    assert_eq!(std::fs::read(&pool).unwrap(), b"half a title");
}

// `land_verified` as the legacy `remux_iso` runs it: no op token, the OS primitives.
pub(super) fn land(
    job: &RemuxJob,
    idx: usize,
    title: &libfreemkv::DiscTitle,
    sink: &dyn Sink,
    mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(sink);
    land_verified(job, idx, title, sink, &halt, &OsRemuxIo, None, mux)
}

#[test]
fn staged_remux_replaces_only_after_copy_and_verification() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("ssd");
    let nas = dir.path().join("nas");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(&nas).unwrap();
    let target = nas.join("Movie.mkv");
    let local_partial = stage.join("1.mkv.partial");
    std::fs::write(&target, b"old").unwrap();
    let new = mkv(600.0, Some(598), 2);
    let sink = Events::default();
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let report = land_verified(
        &job(target.clone(), true),
        0,
        &title(600.0),
        &sink,
        &halt,
        &OsRemuxIo,
        Some(&local_partial),
        writes(new.clone(), true),
    )
    .unwrap();
    assert!(report.replaced);
    assert_eq!(std::fs::read(&target).unwrap(), new);
    assert!(!local_partial.exists());
    assert!(!partial_path(&target).exists());
    assert!(sink.0.lock().unwrap().contains(&"phase:copy".to_string()));
}

#[test]
fn staged_remux_failure_preserves_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let local_partial = dir.path().join("1.mkv.partial");
    std::fs::write(&target, b"old").unwrap();
    let sink = Events::default();
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let result = land_verified(
        &job(target.clone(), true),
        0,
        &title(600.0),
        &sink,
        &halt,
        &OsRemuxIo,
        Some(&local_partial),
        writes(mkv(600.0, Some(598), 2), false),
    );
    let e = result.unwrap_err();
    assert!(
        !libfreemkv::is_halt(&e),
        "an incomplete mux is a failure: {e}"
    );
    assert_eq!(e.kind(), io::ErrorKind::Other);
    assert_eq!(e.to_string(), "E9078: 1");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!local_partial.exists());
    assert!(!partial_path(&target).exists());
}

// A staged partial that is the target or `<target>.partial` would overwrite the library
// file or delete its own source: refused before the lock, the mux or any file change.
#[test]
fn a_staging_path_on_the_target_or_its_partial_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let target_partial = partial_path(&target);
    for staged in [&target, &target_partial] {
        let sink = Events::default();
        let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
        let muxed = AtomicBool::new(false);
        let e = land_verified(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            &halt,
            &OsRemuxIo,
            Some(staged),
            |dest| {
                muxed.store(true, Ordering::SeqCst);
                writes(mkv(600.0, Some(598), 2), true)(dest)
            },
        )
        .unwrap_err();
        assert_eq!(
            e.kind(),
            io::ErrorKind::InvalidInput,
            "{}",
            staged.display()
        );
        let code = libfreemkv::error::E_REMUX_STAGING_INVALID;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert!(!muxed.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!target_partial.exists());
    }
}

// The public staged entry: an image that cannot open leaves no partial anywhere.
#[test]
fn remux_iso_staged_with_an_unopenable_image_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let staged = dir.path().join("7.mkv.partial");
    std::fs::write(&target, b"old").unwrap();
    let sink = Events::default();
    let keys = KeyParams::default();
    let e = remux_iso_staged(&job(target.clone(), true), &keys, &sink, &staged).unwrap_err();
    assert!(!libfreemkv::is_halt(&e), "{e}");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!staged.exists() && !partial_path(&target).exists());
    let refused = remux_iso_staged(&job(target.clone(), false), &keys, &sink, &staged);
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn staged_copy_refuses_to_overwrite_an_existing_partial() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.mkv");
    let destination = dir.path().join("target.mkv.partial");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&destination, b"other writer").unwrap();
    let token = Halt::new();
    let sink = Events::default();
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let timing = RemuxTiming::default();
    let err = copy_staged(&source, &destination, &halt, &sink, &OsRemuxIo, timing).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&destination).unwrap(), b"other writer");
}

// A destination that accepts writes but keeps no bytes: the copy counts `total`, the file holds 0.
struct DroppingIo;
impl RemuxIo for DroppingIo {
    fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        OsRemuxIo.sync(f, h, p)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        OsRemuxIo.open_read(path)
    }
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        drop(OsRemuxIo.create_new(path)?);
        Ok(Box::new(io::sink()))
    }
    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
}

#[test]
fn a_staged_copy_short_on_disk_is_a_size_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (source, destination) = (dir.path().join("s.mkv"), dir.path().join("d.partial"));
    std::fs::write(&source, b"new bytes").unwrap();
    let token = Halt::new();
    let sink = Events::default();
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let timing = RemuxTiming::default();
    let e = copy_staged(&source, &destination, &halt, &sink, &DroppingIo, timing).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    assert_eq!(e.to_string(), "E9080: 0/9");
}

// A worker that drops its sender without reporting is WorkerLost naming the op (E9081).
#[test]
fn a_lost_worker_is_its_code_naming_the_op() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    drop(tx);
    let token = Halt::new();
    let sink = Events::default();
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let timing = RemuxTiming::default();
    let beat = LockBeat::new(None, timing.lock_beat);
    let moved = AtomicU64::new(0);
    let report = |n: u64| activity("verify", n, 1);
    let e = watch_worker(
        &rx,
        &moved,
        "verify",
        timing.verify_stall,
        &halt,
        &sink,
        timing,
        beat,
        &report,
    )
    .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Other);
    assert_eq!(e.to_string(), "E9081: verify");
}

#[test]
fn stop_during_staged_copy_keeps_old_target() {
    struct StopOnCopy(std::sync::atomic::AtomicBool);
    impl Sink for StopOnCopy {
        fn event(&self, event: &Event<'_>) {
            if matches!(event, Event::Phase { name: "copy" }) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        fn should_cancel(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let local_partial = dir.path().join("1.mkv.partial");
    std::fs::write(&target, b"old").unwrap();
    let sink = StopOnCopy(std::sync::atomic::AtomicBool::new(false));
    let token = Halt::new();
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let result = land_verified(
        &job(target.clone(), true),
        0,
        &title(600.0),
        &sink,
        &halt,
        &OsRemuxIo,
        Some(&local_partial),
        writes(mkv(600.0, Some(598), 2), true),
    );
    let e = result.unwrap_err();
    assert!(
        libfreemkv::is_halt(&e),
        "a Stop is Halted, not a failure: {e}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!local_partial.exists());
    assert!(!partial_path(&target).exists());
}

// ── A hand-built MKV: EBML header, Segment, Info, one track, Cues ──────

pub(super) fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

pub(super) fn mkv(duration_secs: f64, last_cue_secs: Option<u64>, tracks: usize) -> Vec<u8> {
    let mut info = el(&[0x2A, 0xD7, 0xB1], &1_000_000u64.to_be_bytes());
    info.extend(el(&[0x44, 0x89], &(duration_secs * 1000.0).to_be_bytes()));
    info.extend(el(&[0x4D, 0x80], b"freemkv 9.9.9 (gtest)"));
    info.extend(el(&[0x57, 0x41], b"freemkv 9.9.9 (gtest)"));
    // TrackNumber is unique per Matroska; libfreemkv rejects a duplicate.
    let entries: Vec<u8> = (0..tracks)
        .flat_map(|i| {
            let mut entry = el(&[0xD7], &[i as u8 + 1]);
            entry.extend(el(&[0x83], &[1]));
            entry.extend(el(&[0x86], b"V_MPEG4/ISO/AVC"));
            el(&[0xAE], &entry)
        })
        .collect();
    let mut body = el(&[0x15, 0x49, 0xA9, 0x66], &info);
    body.extend(el(&[0x16, 0x54, 0xAE, 0x6B], &entries));
    if let Some(t) = last_cue_secs {
        let point = el(&[0xB3], &(t * 1000).to_be_bytes());
        body.extend(el(&[0x1C, 0x53, 0xBB, 0x6B], &el(&[0xBB], &point)));
    }
    let mut out = el(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
    out.extend(el(&[0x18, 0x53, 0x80, 0x67], &body));
    out
}

pub(super) fn title(duration_secs: f64) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        duration_secs,
        ..libfreemkv::DiscTitle::empty()
    }
}

pub(super) fn outcome(completed: bool) -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        completed,
        halted: false,
        output_opened: true,
        bytes_written: 1,
        errors: 0,
        lost_bytes: 0,
        streams: 1,
        undelivered_streams: Vec::new(),
    }
}

#[derive(Default)]
pub(super) struct Events(pub(super) Mutex<Vec<String>>);
impl Sink for Events {
    fn title_opened(&self, _title: &libfreemkv::DiscTitle) {
        self.0.lock().unwrap().push("title".into());
    }

    fn event(&self, e: &Event<'_>) {
        let s = match e {
            Event::Phase { name } => format!("phase:{name}"),
            Event::TitleStart { idx, .. } => format!("start:{idx}"),
            Event::TitleDone { idx, result, .. } => format!("done:{idx}:{}", result.is_ok()),
            Event::Verify { ok, .. } => format!("verify:{ok}"),
            Event::Replaced { .. } => "replaced".into(),
            Event::OutputOpened { .. } => "opened".into(),
            Event::SourceOpened { .. } => "source".into(),
            Event::Keys { .. } => "keys".into(),
            Event::Pass(_) => "pass".into(),
            Event::Recovery(_) => "recovery".into(),
            Event::TitleSkipped { idx, .. } => format!("skipped:{idx}"),
            Event::TitleFailed { idx, .. } => format!("failed:{idx}"),
        };
        self.0.lock().unwrap().push(s);
    }
}

pub(super) fn job(target: PathBuf, replace: bool) -> RemuxJob {
    RemuxJob {
        iso: ImageSource::Iso("/nonexistent/freemkv/none.iso".into()),
        title: None,
        streams: StreamChoice::default(),
        target,
        replace,
    }
}

// A mux stand-in that writes `bytes` where the sink URL points.
pub(super) fn writes(
    bytes: Vec<u8>,
    completed: bool,
) -> impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome> {
    move |dest| {
        let path = dest.strip_prefix("mkv://").unwrap();
        assert!(
            path.ends_with(".partial"),
            "muxes to the partial file, got {path}"
        );
        std::fs::write(path, bytes)?;
        Ok(outcome(completed))
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_target_folder_is_refused_not_taken_as_empty() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // ENOTDIR (a file where the folder should be) holds as root too.
    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let under_file = file.join("Title.mkv");
    let err = refuse_existing(&job(under_file.clone(), false)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    assert!(target_present(&under_file).is_err());
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let target = locked.join("Title").join("Title.mkv");
    let probe = std::fs::symlink_metadata(&target);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if probe.is_ok() || probe.as_ref().unwrap_err().kind() == io::ErrorKind::NotFound {
        return; // root: permissions are not enforced (ENOTDIR above still ran)
    }
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refused = refuse_existing(&job(target.clone(), false));
    let present = target_present(&target);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = refused.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(present.is_err());
}

#[test]
fn verify_accepts_a_runtime_within_slack_and_rejects_a_short_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.mkv");
    std::fs::write(&p, mkv(7200.0, Some(7195), 1)).unwrap();
    let probe = verify_mkv(&p, &title(7200.0)).unwrap();
    assert_eq!(probe.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
    // Header claims the full runtime, the Cues show a third of it.
    std::fs::write(&p, mkv(7200.0, Some(2400), 1)).unwrap();
    assert!(verify_mkv(&p, &title(7200.0)).is_err());
    // No Cues: the header Duration is all there is.
    std::fs::write(&p, mkv(100.0, None, 1)).unwrap();
    assert!(verify_mkv(&p, &title(100.0)).is_ok());
    assert!(verify_mkv(&p, &title(200.0)).is_err());
    // A title with no declared duration skips the runtime check.
    assert!(verify_mkv(&p, &title(0.0)).is_ok());
}

// Slack is max(10 s, 2 %) either side; a title with a runtime needs a finite one.
#[test]
fn verify_runtime_slack_is_the_larger_of_ten_seconds_and_two_percent() {
    let probe = |last_cue: Option<f64>, duration: Option<f64>| libfreemkv::MkvProbe {
        tracks: vec![libfreemkv::MkvProbeTrack {
            number: 1,
            kind: libfreemkv::MkvTrackKind::Video,
            codec_id: "V_MPEG4/ISO/AVC".into(),
            language: "und".into(),
        }],
        last_cue_secs: last_cue,
        duration_secs: duration,
        ..Default::default()
    };
    let ok = |t: f64, runtime: f64| {
        check_probe(Path::new("a"), &title(t), probe(Some(runtime), None)).is_ok()
    };
    // 2 h title: 2 % = 144 s.
    assert!(ok(7200.0, 7200.0 - 143.0) && ok(7200.0, 7200.0 + 143.0));
    assert!(!ok(7200.0, 7200.0 - 145.0) && !ok(7200.0, 7200.0 + 145.0));
    // 100 s title: 2 % = 2 s, so the 10 s floor applies.
    assert!(ok(100.0, 91.0) && ok(100.0, 109.0));
    assert!(!ok(100.0, 89.0) && !ok(100.0, 111.0));
    assert!(!ok(100.0, f64::NAN) && !ok(100.0, f64::INFINITY));
    let t = title(100.0);
    let shown = |p| check_probe(Path::new("a"), &t, p).unwrap_err().to_string();
    assert_eq!(shown(probe(None, None)), "E9077: no-runtime a");
    assert_eq!(shown(probe(Some(f64::NAN), None)), "E9077: no-runtime a");
    assert_eq!(
        shown(probe(Some(80.0), None)),
        "E9077: runtime-mismatch 80.0/100.0 a"
    );
    assert!(
        check_probe(Path::new("a"), &t, probe(None, Some(100.0))).is_ok(),
        "header Duration"
    );
}

#[test]
fn verify_rejects_empty_trackless_and_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.mkv");
    let shown = |t: f64| verify_mkv(&p, &title(t)).unwrap_err().to_string();
    std::fs::write(&p, b"").unwrap();
    assert_eq!(shown(0.0), format!("E9077: empty {}", p.display()));
    std::fs::write(&p, mkv(10.0, Some(9), 0)).unwrap();
    assert_eq!(shown(10.0), format!("E9077: no-tracks {}", p.display()));
    std::fs::write(&p, vec![0x47u8; 4096]).unwrap();
    assert!(verify_mkv(&p, &title(10.0)).is_err());
    assert!(verify_mkv(&dir.path().join("missing.mkv"), &title(0.0)).is_err());
}

#[test]
fn a_verified_remux_replaces_the_target_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let new = mkv(600.0, Some(598), 2);
    let sink = Events::default();
    let r = land(
        &job(target.clone(), true),
        0,
        &title(600.0),
        &sink,
        writes(new.clone(), true),
    )
    .unwrap();
    assert!(r.replaced);
    assert_eq!(r.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
    assert_eq!(r.verified.tracks.len(), 2);
    assert_eq!(std::fs::read(&target).unwrap(), new);
    assert!(!partial_path(&target).exists());
    assert_eq!(
        *sink.0.lock().unwrap(),
        [
            "title",
            "phase:mux",
            "start:0",
            "done:0:true",
            "phase:sync",
            "phase:verify",
            "verify:true",
            "phase:replace",
            "replaced"
        ]
    );
}

#[test]
fn a_new_target_lands_without_replace() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("New.mkv");
    let r = land(
        &job(target.clone(), false),
        0,
        &title(60.0),
        &Events::default(),
        writes(mkv(60.0, Some(59), 1), true),
    )
    .unwrap();
    assert!(!r.replaced);
    assert!(target.exists());
    assert!(!partial_path(&target).exists());
}

#[test]
fn a_failed_verify_leaves_the_old_target_and_no_partial() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let sink = Events::default();
    let e = land(
        &job(target.clone(), true),
        0,
        &title(600.0),
        &sink,
        writes(mkv(600.0, Some(120), 1), true),
    )
    .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    let code = libfreemkv::error::E_REMUX_VERIFY_FAILED;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!partial_path(&target).exists());
    assert!(sink.0.lock().unwrap().contains(&"verify:false".to_string()));
}

#[test]
fn a_failed_or_incomplete_mux_leaves_the_old_target_and_no_partial() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let j = job(target.clone(), true);
    let fail = |dest: &str| {
        std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half")?;
        Err(io::Error::other("E6000: 12"))
    };
    assert!(land(&j, 0, &title(60.0), &Events::default(), fail).is_err());
    let incomplete = writes(mkv(60.0, Some(59), 1), false);
    assert!(land(&j, 0, &title(60.0), &Events::default(), incomplete).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!partial_path(&target).exists());
}

#[test]
fn an_existing_target_without_replace_is_refused_before_opening() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let sink = Events::default();
    let e = remux_iso(&job(target.clone(), false), &KeyParams::default(), &sink).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert!(sink.0.lock().unwrap().is_empty(), "nothing was opened");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

// A sink URL is a String: a non-UTF-8 partial path would mux to a different, lossy name.
#[cfg(unix)]
#[test]
fn a_non_utf8_partial_path_is_refused_before_the_mux() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join(std::ffi::OsStr::from_bytes(b"Caf\xe9"));
    let _ = std::fs::create_dir(&folder); // APFS refuses the name; the check comes first
    let target = folder.join("Movie.mkv");
    let muxed = AtomicBool::new(false);
    let e = land(
        &job(target.clone(), true),
        0,
        &title(60.0),
        &Events::default(),
        |_| {
            muxed.store(true, Ordering::SeqCst);
            Ok(outcome(true))
        },
    )
    .unwrap_err();
    let code = libfreemkv::error::E_STREAM_URL_INVALID;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert!(!muxed.load(Ordering::SeqCst));
    assert!(!partial_path(&target).exists());
}

// A `dir://` source muxes through a String URL: a non-UTF-8 folder is refused up front.
#[cfg(unix)]
#[test]
fn a_non_utf8_disc_folder_is_refused_before_opening() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join(std::ffi::OsStr::from_bytes(b"Disc\xe9"));
    let mut j = job(dir.path().join("Movie.mkv"), true);
    j.iso = ImageSource::Dir(folder);
    let sink = Events::default();
    let e = remux_iso(&j, &KeyParams::default(), &sink).unwrap_err();
    let code = libfreemkv::error::E_STREAM_URL_INVALID;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert!(sink.0.lock().unwrap().is_empty(), "nothing was opened");
}

// Preflight catches an unknown tag first; a caller that skips it still gets E9083, not prose.
#[test]
fn an_unknown_stream_language_is_its_code() {
    let streams = StreamChoice {
        audio: crate::job::StreamFilter::Langs(vec!["Klingonish".into()]),
        subtitles: crate::job::StreamFilter::All.into(),
    };
    let e = title_selection(&title(60.0), &streams).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    let code = libfreemkv::error::E_STREAM_LANGUAGE_UNKNOWN;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert_eq!(e.to_string(), "E9083: Klingonish");
}

// A relative target named like a libfreemkv code reads as E9084, not that code.
#[test]
fn an_existing_target_named_like_an_error_code_is_not_that_code() {
    let e = target_exists(Path::new("E7022 Movie.mkv"));
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    let code = libfreemkv::error::E_REMUX_TARGET_EXISTS;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert_eq!(e.to_string(), "E9084: E7022 Movie.mkv");
}

#[test]
fn an_unopenable_image_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    assert!(
        remux_iso(
            &job(target.clone(), true),
            &KeyParams::default(),
            &Events::default()
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!partial_path(&target).exists());
}

#[test]
fn the_main_title_is_the_default_and_a_missing_title_is_range_error() {
    let mut disc = libfreemkv::Disc {
        volume_id: String::new(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 1,
        capacity_bytes: 2048,
        layers: 1,
        titles: vec![title(10.0), title(7200.0)],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    };
    assert_eq!(pick_title(&disc, None, &StreamFilter::All).unwrap(), 0);
    assert_eq!(pick_title(&disc, Some(1), &StreamFilter::All).unwrap(), 1);
    let e = pick_title(&disc, Some(5), &StreamFilter::All).unwrap_err();
    assert_eq!(
        crate::error_code(&e),
        Some(libfreemkv::Error::DiscTitleRange { index: 5, count: 2 }.code())
    );
    disc.titles.clear();
    for title in [None, Some(0)] {
        let e = pick_title(&disc, title, &StreamFilter::All).unwrap_err();
        let code = libfreemkv::error::E_NO_STREAMS;
        assert_eq!(crate::error_code(&e), Some(code), "{title:?}: {e}");
    }
}

#[test]
fn remux_main_title_honors_an_equivalent_audio_language_presentation() {
    let mut english = title(3600.0);
    english.content_format = libfreemkv::ContentFormat::DvdPs;
    english.extents = vec![libfreemkv::disc::Extent {
        start_lba: 7,
        sector_count: 10,
    }];
    english.selection_evidence.segments =
        libfreemkv::disc::ContentSegment::from_cell_extents(&english.extents);
    english
        .streams
        .push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid: 0x1100,
            codec: libfreemkv::Codec::TrueHd,
            channels: libfreemkv::AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        }));
    let mut german = english.clone();
    if let Some(libfreemkv::Stream::Audio(a)) = german.streams.first_mut() {
        a.language = "deu".into();
    }
    let disc = libfreemkv::Disc {
        volume_id: String::new(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 1,
        capacity_bytes: 2048,
        layers: 1,
        titles: vec![english, german],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    };
    assert_eq!(
        pick_title(&disc, None, &StreamFilter::Langs(vec!["deu".into()])).unwrap(),
        1
    );
}

#[test]
fn plan_selection_defaults_to_every_stream() {
    let mut plan = MuxPlan::new(vec![0, 3]);
    let only = libfreemkv::StreamSelection {
        audio: libfreemkv::PidFilter::Only(vec![4352]),
        subtitle: libfreemkv::PidFilter::Only(vec![]),
    };
    plan.streams.push((3, only.clone()));
    assert!(plan.selection_for(0).is_all());
    assert_eq!(plan.selection_for(3), only);
}

// A remux's main title is title 0 unless the audio filter wants another cut's language;
// an explicit title always wins.
#[test]
fn a_remux_main_title_follows_the_audio_filter_among_cuts() {
    let disc = crate::test_fixtures::disc_of(crate::test_fixtures::branch_titles());
    assert_eq!(pick_title(&disc, None, &StreamFilter::All).unwrap(), 0);
    let de = StreamFilter::Langs(vec!["de".into()]);
    assert_eq!(pick_title(&disc, None, &de).unwrap(), 1);
    assert_eq!(pick_title(&disc, Some(0), &de).unwrap(), 0);
}

// A target that appears after the re-check is still the typed refusal (E9084) when
// publication finds it, and is never overwritten.
#[test]
fn a_target_appearing_before_publication_is_its_coded_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let landing = dir.path().join("Movie.mkv.partial");
    std::fs::write(&landing, b"new").unwrap();
    std::fs::write(&target, b"raced in").unwrap();
    let e = super::land(&landing, &job(target.clone(), false), &OsRemuxIo).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    let code = libfreemkv::error::E_REMUX_TARGET_EXISTS;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert_eq!(std::fs::read(&target).unwrap(), b"raced in");
}

// A Stop during the remux's own mux (incomplete, halted) is Halted, not a failure; an
// incomplete mux nobody stopped is E9078 naming the 1-based title. Neither replaces the target.
#[test]
fn a_stop_mid_mux_is_halted_and_an_incomplete_mux_is_e9078() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let j = job(target.clone(), true);
    let stopped = |dest: &str| {
        std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half")?;
        Ok(libfreemkv::MuxOutcome {
            halted: true,
            ..outcome(false)
        })
    };
    let e = land(&j, 2, &title(60.0), &Events::default(), stopped).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    let incomplete = writes(mkv(60.0, Some(59), 1), false);
    let e = land(&j, 2, &title(60.0), &Events::default(), incomplete).unwrap_err();
    let code = libfreemkv::error::E_MUX_INCOMPLETE;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert_eq!(e.to_string(), "E9078: 3");
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(!partial_path(&target).exists());
}

// The staged copy's speed/ETA come from the one SpeedEstimator: a single sample shows no
// speed yet, and no tick exceeds its plausibility cap, however fast a local copy runs.
#[test]
fn staged_copy_speed_comes_from_the_speed_estimator() {
    #[derive(Default)]
    struct Ticks(Mutex<Vec<crate::sink::Progress>>);
    impl Sink for Ticks {
        fn progress(&self, p: &crate::sink::Progress) {
            self.0.lock().unwrap().push(p.clone());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (source, destination) = (dir.path().join("s.mkv"), dir.path().join("d.partial"));
    std::fs::write(&source, vec![7u8; 64 << 20]).unwrap();
    let token = Halt::new();
    let sink = Ticks::default();
    let halt = EngineHalt::new(&token, None).with_sink(&sink);
    let timing = RemuxTiming {
        activity_every: Duration::from_secs(3600),
        ..RemuxTiming::default()
    };
    let _ = copy_staged(&source, &destination, &halt, &sink, &DroppingIo, timing);
    let ticks = sink.0.lock().unwrap();
    assert!(!ticks.is_empty(), "the copy reports its progress");
    assert_eq!(ticks[0].speed_bps, 0, "one sample is no speed yet");
    assert!(ticks.iter().all(|t| t.speed_bps <= 1024 << 20), "{ticks:?}");
}

// A remux's TitleDone carries the title's verdict as `mux_image_titles` reports it: an
// incomplete mux (E9078) or a stopped one (Halted) is TitleDone(Err), never Ok.
#[test]
fn an_incomplete_remux_title_is_reported_done_with_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let j = job(target.clone(), true);
    let sink = Events::default();
    let incomplete = writes(mkv(60.0, Some(59), 1), false);
    assert!(land(&j, 0, &title(60.0), &sink, incomplete).is_err());
    let stopped = |dest: &str| {
        std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half")?;
        Ok(libfreemkv::MuxOutcome {
            halted: true,
            ..outcome(false)
        })
    };
    assert!(land(&j, 1, &title(60.0), &sink, stopped).is_err());
    let events = sink.0.lock().unwrap().clone();
    let done: Vec<&String> = events.iter().filter(|e| e.starts_with("done:")).collect();
    assert_eq!(done, ["done:0:false", "done:1:false"], "{events:?}");
}

// Primitives whose no-replace publication is unsupported (a share that can neither
// hard-link nor rename without replacing); everything else is the OS's.
struct NoPublishIo;
impl RemuxIo for NoPublishIo {
    fn publish(&self, landing: &Path, target: &Path) -> io::Result<()> {
        NoPublishErrno(45).publish(landing, target)
    }
    fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        OsRemuxIo.sync(f, h, p)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        OsRemuxIo.open_read(path)
    }
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        OsRemuxIo.create_new(path)
    }
    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
}

// As [`NoPublishIo`], the refusal wrapping the OS error `errno` (libfreemkv's publish reports
// Linux's EINVAL from `RENAME_NOREPLACE` as `Unsupported` around it).
struct NoPublishErrno(i32);
impl RemuxIo for NoPublishErrno {
    fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        OsRemuxIo.sync(f, h, p)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        OsRemuxIo.open_read(path)
    }
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        OsRemuxIo.create_new(path)
    }
    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
    fn publish(&self, _: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            io::Error::from_raw_os_error(self.0),
        ))
    }
    fn replace(&self, landing: &Path, target: &Path) -> io::Result<()> {
        self.publish(landing, target)
    }
}

// An unpublished remux to `target` through `rio`, its mux writing `bytes`.
fn unpublished(target: &Path, bytes: &[u8], rio: &dyn RemuxIo) -> io::Error {
    let sink = Events::default();
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let j = job(target.to_path_buf(), false);
    let mux = writes(bytes.to_vec(), true);
    land_verified(&j, 0, &title(600.0), &sink, &halt, rio, None, mux).unwrap_err()
}

// The refusal libfreemkv makes of Linux's EINVAL (no RENAME_NOREPLACE on NFS or a FUSE share)
// is `Unsupported` like any other: the verified output is kept, by kind alone.
#[test]
fn an_unsupported_publish_from_einval_keeps_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let new = mkv(600.0, Some(598), 2);
    let e = unpublished(&target, &new, &NoPublishErrno(22));
    assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
    let kept = &RemuxUnpublished::of(&e).expect("kept").kept;
    assert_eq!(kept, &verified_path(&target));
    assert_eq!(std::fs::read(kept).unwrap(), new);
}

// A kept output never replaces a file: an earlier kept copy (or a user's file) at
// `<target>.verified` stays, and the next is kept at the first free `<target>.verified.<n>`.
#[test]
fn an_unpublished_remux_never_overwrites_an_earlier_kept_output() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let (first, second) = (mkv(600.0, Some(598), 2), mkv(600.0, Some(599), 1));
    let e = unpublished(&target, &first, &NoPublishIo);
    let kept = RemuxUnpublished::of(&e).unwrap().kept.clone();
    assert_eq!(kept, verified_path(&target));
    let e = unpublished(&target, &second, &NoPublishIo);
    let again = RemuxUnpublished::of(&e).unwrap().kept.clone();
    assert_eq!(again, dir.path().join("Movie.mkv.verified.1"));
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        first,
        "the earlier copy stays"
    );
    assert_eq!(std::fs::read(&again).unwrap(), second);
}

// A target name too long for `<target>.verified` (255 bytes with `.partial`, 256 with
// `.verified`) keeps the output under a short name beside it, never at `<target>.partial`,
// which the next remux to the target sweeps first.
#[test]
fn a_kept_output_whose_name_does_not_fit_is_kept_outside_the_partial() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(format!("{}.mkv", "m".repeat(243)));
    assert_eq!(partial_path(&target).file_name().unwrap().len(), 255);
    let new = mkv(600.0, Some(598), 2);
    let e = unpublished(&target, &new, &NoPublishIo);
    let kept = RemuxUnpublished::of(&e).expect("kept").kept.clone();
    assert_ne!(kept, partial_path(&target));
    assert_eq!(kept.parent(), target.parent());
    assert_eq!(std::fs::read(&kept).unwrap(), new);
    // The next remux to the target sweeps its stale partial, never the kept output.
    let _ = unpublished(&target, b"not an mkv", &NoPublishIo);
    assert_eq!(std::fs::read(&kept).unwrap(), new);
}

// OS primitives whose `keep` first runs `during` once (another writer, or another remux
// keeping meanwhile), then moves as production does, or fails when `fails`.
struct KeepRace<'a> {
    during: &'a (dyn Fn(&Path) + Sync),
    ran: AtomicBool,
    fails: bool,
}
impl<'a> KeepRace<'a> {
    fn new(during: &'a (dyn Fn(&Path) + Sync), fails: bool) -> Self {
        let ran = AtomicBool::new(false);
        KeepRace { during, ran, fails }
    }
}
impl RemuxIo for KeepRace<'_> {
    fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        OsRemuxIo.sync(f, h, p)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        OsRemuxIo.open_read(path)
    }
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        OsRemuxIo.create_new(path)
    }
    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
    fn keep(&self, landing: &Path, name: &Path) -> io::Result<()> {
        if !self.ran.swap(true, Ordering::SeqCst) {
            (self.during)(name);
        }
        match self.fails {
            true => Err(io::Error::other("rename failed")),
            false => std::fs::rename(landing, name),
        }
    }
}

// Two remuxes to different long-named targets in one folder (each holding only its own
// target's lock) both fall back to the shared short names: they never keep to the same one.
#[test]
fn concurrent_keeps_to_different_targets_never_share_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let (ta, tb) = (
        dir.path().join("a".repeat(250)),
        dir.path().join("b".repeat(250)),
    );
    let (la, lb) = (dir.path().join("a.landing"), dir.path().join("b.landing"));
    std::fs::write(&la, b"title A").unwrap();
    std::fs::write(&lb, b"title B").unwrap();
    let b_kept = Mutex::new(None);
    let b_keeps = |_: &Path| {
        *b_kept.lock().unwrap() = keep_unpublished(&lb, &tb, &crate::NoopSink, &OsRemuxIo);
    };
    let rio = KeepRace::new(&b_keeps, false);
    let a_kept = keep_unpublished(&la, &ta, &crate::NoopSink, &rio).expect("A kept");
    let b_kept = b_kept.lock().unwrap().clone().expect("B kept");
    assert_ne!(a_kept, b_kept);
    assert_eq!(std::fs::read(&a_kept).unwrap(), b"title A");
    assert_eq!(std::fs::read(&b_kept).unwrap(), b"title B");
}

// A writer that creates a kept name (exclusively) while the keep runs never has its file
// replaced: the name is reserved before the output moves there.
#[test]
fn a_kept_name_created_during_the_keep_is_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let landing = partial_path(&target);
    std::fs::write(&landing, b"verified").unwrap();
    let created = AtomicBool::new(false);
    let foreign = |name: &Path| {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(name);
        if f.and_then(|mut f| f.write_all(b"foreign")).is_ok() {
            created.store(true, Ordering::SeqCst);
        }
    };
    let rio = KeepRace::new(&foreign, false);
    let kept = keep_unpublished(&landing, &target, &crate::NoopSink, &rio).expect("kept");
    assert_eq!(std::fs::read(&kept).unwrap(), b"verified");
    let name = verified_path(&target);
    let intact = std::fs::read(&name).is_ok_and(|b| b == b"foreign");
    assert!(
        !created.load(Ordering::SeqCst) || intact,
        "the foreign file stays"
    );
}

// Every keep fails, the first after `during` touched its placeholder: the file then at the
// first kept name is returned; no other placeholder is left and the output stays.
fn failed_keep(during: &(dyn Fn(&Path) + Sync)) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let landing = partial_path(&target);
    std::fs::write(&landing, b"verified").unwrap();
    let rio = KeepRace::new(during, true);
    assert_eq!(
        keep_unpublished(&landing, &target, &crate::NoopSink, &rio),
        None
    );
    let left: Vec<_> = kept_names(&target).skip(1).filter(|n| n.exists()).collect();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(std::fs::read(&landing).unwrap(), b"verified");
    std::fs::read(verified_path(&target)).expect("the first kept name stays")
}

// A move that fails never removes an empty file another writer renamed over the placeholder:
// it is not the file the keep created.
#[test]
fn a_failed_keep_leaves_a_foreign_empty_file_at_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let swap = |name: &Path| {
        let other = dir.path().join("other");
        std::fs::write(&other, b"").unwrap();
        std::fs::rename(&other, name).unwrap();
    };
    assert_eq!(failed_keep(&swap), b"");
}

// A move that fails never removes its own placeholder once another writer wrote into it:
// it is no longer the empty file the keep created.
#[test]
fn a_failed_keep_leaves_its_placeholder_once_written_into() {
    let write_in = |name: &Path| {
        let mut f = std::fs::OpenOptions::new().write(true).open(name).unwrap();
        f.write_all(b"written in place").unwrap();
    };
    assert_eq!(failed_keep(&write_in), b"written in place");
}

// A symlink left at the kept name, pointing at our moved-aside empty placeholder, is never
// removed: the check does not follow it.
#[cfg(unix)]
#[test]
fn a_failed_keep_leaves_a_symlink_to_its_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let link = |name: &Path| {
        let aside = dir.path().join("aside");
        std::fs::rename(name, &aside).unwrap();
        std::os::unix::fs::symlink(&aside, name).unwrap();
    };
    assert_eq!(failed_keep(&link), b"");
}

// OS primitives as `KeepRace`, but the identity of a file can never be read.
struct BlindKeep<'a>(KeepRace<'a>);
impl RemuxIo for BlindKeep<'_> {
    fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
        self.0.sync(f, h, p)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        self.0.open_read(path)
    }
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.0.create_new(path)
    }
    fn timing(&self) -> RemuxTiming {
        self.0.timing()
    }
    fn keep(&self, landing: &Path, name: &Path) -> io::Result<()> {
        self.0.keep(landing, name)
    }
    fn file_id(&self, _: &Path) -> Option<(u64, u64)> {
        None
    }
}

// A placeholder whose identity could not be read is never removed: nothing proves it is ours.
#[test]
fn a_failed_keep_leaves_a_placeholder_with_no_readable_identity() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let landing = partial_path(&target);
    std::fs::write(&landing, b"verified").unwrap();
    let nothing = |_: &Path| {};
    let rio = BlindKeep(KeepRace::new(&nothing, true));
    assert_eq!(
        keep_unpublished(&landing, &target, &crate::NoopSink, &rio),
        None
    );
    assert_eq!(std::fs::read(verified_path(&target)).unwrap(), b"");
}

// Every kept name taken: the remux fails E5000 (kind Unsupported) with no `RemuxUnpublished`,
// its partial is removed, and every file at a kept name is left as it was.
#[test]
fn an_unpublished_remux_with_every_kept_name_taken_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    let names: Vec<PathBuf> = kept_names(&target).collect();
    assert_eq!(names.len(), 2 * KEPT_NAMES);
    for name in &names {
        std::fs::write(name, b"taken").unwrap();
    }
    let e = unpublished(&target, &mkv(600.0, Some(598), 2), &NoPublishIo);
    assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
    let code = libfreemkv::error::E_IO_ERROR;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert!(RemuxUnpublished::of(&e).is_none());
    assert!(!partial_path(&target).exists() && !target.exists());
    for name in &names {
        assert_eq!(std::fs::read(name).unwrap(), b"taken", "{}", name.display());
    }
}

// A kept output's error shows the coded I/O text once: it is not also a source in the chain.
#[test]
fn an_unpublished_remux_error_shows_its_text_once_in_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let e = unpublished(
        &dir.path().join("Movie.mkv"),
        &mkv(600.0, Some(598), 2),
        &NoPublishIo,
    );
    let mut chain = vec![e.to_string()];
    let mut next = std::error::Error::source(&e);
    while let Some(cause) = next {
        chain.push(cause.to_string());
        next = cause.source();
    }
    assert!(chain[0].starts_with("E5000"), "{chain:?}");
    let coded = chain
        .iter()
        .filter(|text| text.starts_with("E5000"))
        .count();
    assert_eq!(coded, 1, "{chain:?}");
}

// A target filesystem that cannot publish without replacing fails the remux with a coded
// error naming the verified output it keeps as `<target>.verified` (never a `.partial`, which
// a retry sweeps): a retry that fails again leaves it. The target is not created.
#[test]
fn an_unsupported_no_replace_publish_keeps_the_verified_output() {
    let dir = tempfile::tempdir().unwrap();
    let new = mkv(600.0, Some(598), 2);
    for staged in [false, true] {
        let target = dir.path().join(format!("Movie-{staged}.mkv"));
        let local = dir.path().join(format!("{staged}.mkv.partial"));
        let sink = Events::default();
        let token = Halt::new();
        let halt = EngineHalt::new(&token, None).with_sink(&sink);
        let remux = |mux: &dyn Fn(&str) -> io::Result<libfreemkv::MuxOutcome>| {
            let j = job(target.clone(), false);
            let stage = staged.then_some(local.as_path());
            land_verified(&j, 0, &title(600.0), &sink, &halt, &NoPublishIo, stage, mux)
        };
        let e = remux(&|dest| writes(new.clone(), true)(dest)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
        let code = libfreemkv::error::E_IO_ERROR;
        assert_eq!(crate::error_code(&e), Some(code), "staged {staged}: {e}");
        let kept = verified_path(&target);
        assert_eq!(RemuxUnpublished::of(&e).map(|u| &u.kept), Some(&kept));
        assert!(!target.exists(), "staged {staged}: nothing published");
        assert_eq!(std::fs::read(&kept).unwrap(), new, "staged {staged}");
        assert!(
            !partial_path(&target).exists() && !local.exists(),
            "staged {staged}"
        );
        let lock = dir.path().join(format!("Movie-{staged}.mkv.lock"));
        assert!(!lock.exists(), "staged {staged}: the lock is released");
        // A retry whose mux fails sweeps its own partial, never the kept copy.
        let fail = |dest: &str| {
            std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half")?;
            Err(io::Error::other("E6000: 12"))
        };
        assert!(remux(&fail).is_err());
        assert_eq!(std::fs::read(&kept).unwrap(), new, "staged {staged}: retry");
    }
}

// With `replace` the publication is a plain rename: its failure is coded (E5000, its own
// kind) and keeps nothing.
#[test]
fn a_failed_replacing_rename_is_coded_and_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("missing-folder").join("Movie.mkv");
    let landing = dir.path().join("Movie.mkv.partial");
    std::fs::write(&landing, b"new").unwrap();
    let e = super::land(&landing, &job(target.clone(), true), &NoPublishIo).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    let code = libfreemkv::error::E_IO_ERROR;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert!(RemuxUnpublished::of(&e).is_none());
}

// A replacing remux whose rename fails Unsupported is a plain coded failure: no kept output,
// no `RemuxUnpublished`; its partial is removed and the existing target is untouched.
#[test]
fn an_unsupported_replacing_rename_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("Movie.mkv");
    std::fs::write(&target, b"old").unwrap();
    let sink = Events::default();
    let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let mux = writes(mkv(600.0, Some(598), 2), true);
    let rio = NoPublishErrno(45);
    let j = job(target.clone(), true);
    let e = land_verified(&j, 0, &title(600.0), &sink, &halt, &rio, None, mux).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
    let code = libfreemkv::error::E_IO_ERROR;
    assert_eq!(crate::error_code(&e), Some(code), "{e}");
    assert!(RemuxUnpublished::of(&e).is_none());
    assert!(!verified_path(&target).exists() && !partial_path(&target).exists());
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
}

// The image `iso` as two aliased cuts, title 0 English and title 1 German (still muxing its
// own clip, K2): both carry the segments the scan found for title 0.
fn branch_disc(iso: &Path) -> libfreemkv::Disc {
    let (mut disc, _) = libfreemkv::scan_iso(iso, Default::default()).unwrap();
    let shared = disc.titles[0].selection_evidence.segments.clone();
    assert!(!shared.is_empty(), "the scan describes title 0 by segments");
    for (title, language) in disc.titles.iter_mut().zip(["eng", "deu"]) {
        title.selection_evidence.segments = shared.clone();
        for stream in &mut title.streams {
            if let libfreemkv::Stream::Audio(a) = stream {
                a.language = language.into();
            }
        }
    }
    disc
}

fn branch_job(iso: PathBuf, target: PathBuf) -> RemuxJob {
    RemuxJob {
        iso: ImageSource::Iso(iso),
        title: None,
        streams: StreamChoice {
            audio: StreamFilter::Langs(vec!["de".into()]),
            ..StreamChoice::default()
        },
        target,
        replace: false,
    }
}

// A remux with no title picks the main title by the job's audio, and keys exactly that
// title: a Known set covering only the German cut remuxes it (no E7022 for title 0), and
// key sources are asked once, for the picked title alone (no second resolve).
#[test]
fn a_remux_keys_the_audio_picked_cut_alone() {
    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory, resolve};
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "d.iso");
    let disc = branch_disc(&iso);
    assert_eq!(pick_title(&disc, None, &StreamFilter::All).unwrap(), 0);
    let calls = Calls::default();
    let scope = KeyScope::Titles(vec![1]);
    let only_de = resolve(&fx, scope, &[(Answer::Keydb, &[K2])], &calls).unwrap();
    let opts = OpenImageOptions {
        disc: Some(branch_disc(&iso)),
        ..OpenImageOptions::known(only_de)
    };
    let target = dir.path().join("known.mkv");
    let sink = Events::default();
    let job = branch_job(iso.clone(), target.clone());
    let report = remux_opening(&job, opts, &sink, &Halt::new(), None).expect("remuxes");
    assert!(report.outcome.completed);
    assert!(sink.0.lock().unwrap().contains(&"start:1".to_string()));
    // Resolving from sources: one request, and for title 1 alone (only its key K2 is held,
    // so a resolve over title 0 would be refused).
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K2])], &calls);
    let opts = OpenImageOptions {
        disc: Some(disc),
        ..OpenImageOptions::resolve(f)
    };
    let job = branch_job(iso, dir.path().join("resolved.mkv"));
    remux_opening(&job, opts, &Events::default(), &Halt::new(), None).expect("remuxes");
    assert_eq!(
        calls.len(),
        1,
        "one request, for title 1: {:?}",
        calls.all()
    );
}

// Only a file sink's path is ever removed after a failed title: a network or stdio sink
// names no file (`network://host:port` must never become `remove_file("host:port")`), and a
// folder sink is left alone.
#[test]
fn only_a_file_sink_names_an_output_file() {
    for dest in [
        "mkv:///o/a.mkv",
        "m2ts:///o/a.m2ts",
        "mp4:///o/a.mp4",
        "mpg:///o/a.mpg",
        "fvi:///o/a.fvi",
        "chapters:///o/a.xml",
        "json:///o/a.json",
    ] {
        let path = dest.split_once("://").unwrap().1;
        assert_eq!(output_file(dest), Some(PathBuf::from(path)), "{dest}");
    }
    for dest in [
        "network://127.0.0.1:9000",
        "stdio://",
        "null://",
        "mkv:///o/dir/",
    ] {
        assert_eq!(output_file(dest), None, "{dest}");
    }
}

// A real mux of an image title reports its output opening, with the identity of the file it
// opened, to the title's `OpenedWatch`: the evidence a failed title uses to remove its file.
#[test]
fn a_real_mux_reports_its_output_opening_to_the_watch() {
    use crate::test_fixtures::{Answer, Calls, K1, bd_image, resolve};
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let iso = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let set = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    );
    let opened = crate::open_image_with(&iso, OpenImageOptions::known(set.unwrap())).unwrap();
    let out = dir.path().join("t.mkv");
    let dest = format!("mkv://{}", out.display());
    let watch = OpenedWatch::new(&crate::NoopSink, &dest);
    assert!(watch.opened.get().is_none());
    let selection = libfreemkv::StreamSelection::default();
    let mux = mux_options(false);
    let r = mux_opened_title(&opened, &opened.keys, 0, selection, &dest, &mux, &watch);
    assert!(r.unwrap().completed);
    let file = file_id(&out);
    assert!(file.is_some());
    assert_eq!(
        watch.opened.get(),
        Some(&file),
        "OutputOpened reached the watch"
    );
}

// The production title closure hands the real mux the title's watch, not the outer sink: a
// real mux that opened its output and then failed (an Err, no outcome) leaves nothing there.
#[test]
fn a_real_mux_failing_after_its_open_removes_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let (opened, ..) = opened_image(dir.path());
    let out = dir.path().join("t0.mkv");
    let dest = |_: usize| format!("mkv://{}", out.display());
    let plan = MuxPlan::new(vec![0]);
    let fail_after_open = |muxed: io::Result<libfreemkv::MuxOutcome>| {
        assert!(muxed?.output_opened, "the real mux opened its output");
        Err(io::Error::other("E6000: 12"))
    };
    let sink = crate::NoopSink;
    let r = mux_image_titles_then(&opened, &plan, &dest, &sink, &Halt::new(), fail_after_open);
    assert!(matches!(r, RipOutcome::Failed { .. }), "{r:?}");
    assert!(!out.exists(), "the failed title's output is removed");
}
