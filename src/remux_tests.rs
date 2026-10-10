#[test]
fn a_failed_title_removes_only_an_output_it_touched() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.mkv");
    let path = p.to_str().unwrap();
    // Absent before, written by the failed title: removed.
    let before = output_stamp(path);
    std::fs::write(&p, b"partial").unwrap();
    remove_failed_output(path, before);
    assert!(!p.exists());
    // Present before and untouched: kept.
    std::fs::write(&p, b"earlier rip").unwrap();
    let before = output_stamp(path);
    remove_failed_output(path, before);
    assert_eq!(std::fs::read(&p).unwrap(), b"earlier rip");
    // Present before and rewritten by the failed title: removed.
    let before = output_stamp(path);
    std::fs::write(&p, b"partial rewrite").unwrap();
    remove_failed_output(path, before);
    assert!(!p.exists());
}

use super::*;
use std::sync::Mutex;

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
    assert!(pick_title(&disc, None, &StreamFilter::All).is_err());
}

#[test]
fn remux_main_title_honors_an_equivalent_audio_language_presentation() {
    let mut english = title(3600.0);
    english.content_format = libfreemkv::ContentFormat::DvdPs;
    english.extents = vec![libfreemkv::disc::Extent {
        start_lba: 7,
        sector_count: 10,
    }];
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
