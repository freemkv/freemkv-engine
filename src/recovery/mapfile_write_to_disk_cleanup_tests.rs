use super::*;

fn tmp_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

// R1/M2: an abandoned (disowned) writer paused mid-write, then the resumed owner of the
// same path writes. The stale snapshot must never reach the path, whichever runs first.
#[test]
fn a_disowned_writer_cannot_clobber_the_resumed_owners_mapfile() {
    use std::sync::mpsc;
    use std::time::Duration;
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("race.mapfile");
    let mut stale = Mapfile::create(&path, 4096, "abandoned-pass").unwrap();
    stale.record(0, 2048, SectorStatus::Unreadable).unwrap();
    let disown = stale.disown_handle();

    let (at_tx, at_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    WRITE_HOOKS.lock().unwrap().push((
        path.clone(),
        HookAt::TmpBuffered,
        Box::new(move || {
            at_tx.send(()).unwrap();
            let _ = go_rx.recv();
        }),
    ));
    let abandoned = std::thread::spawn(move || stale.flush());
    at_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the abandoned writer never reached the hook");
    let tmp = tmp_of(&path);
    assert_eq!(
        std::fs::metadata(&tmp).unwrap().len(),
        0,
        "fixture: still buffered"
    );

    // The stop disowns it; the resume builds a fresh mapfile on the same path.
    disown.disown();
    let owner_path = path.clone();
    let owner = std::thread::spawn(move || {
        Mapfile::create(&owner_path, 4096, "resumed-pass").map(|m| m.total_size())
    });
    let waited = std::time::Instant::now();
    while !owner.is_finished() && waited.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(5));
    }
    go_tx.send(()).unwrap();
    abandoned
        .join()
        .unwrap()
        .expect("a disowned flush reports success");
    assert_eq!(owner.join().unwrap().expect("the owner's write"), 4096);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("Created by resumed-pass") && !text.contains("abandoned-pass"),
        "the abandoned writer's stale snapshot reached the resumed owner's mapfile:\n{text}"
    );
    assert_eq!(Mapfile::load(&path).unwrap().stats().bytes_unreadable, 0);
    assert!(!tmp.exists(), "no tmp left behind");
}

// M2: the abandoned writer passed its disowned check and stalls before its rename while
// the resumed owner writes. The owner must wait for that rename, not race it.
#[test]
fn a_resumed_owner_waits_for_a_stalled_disowned_rename() {
    use std::sync::mpsc;
    use std::time::Duration;
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("rename-race.mapfile");
    let mut stale = Mapfile::create(&path, 4096, "abandoned-pass").unwrap();
    stale.record(0, 2048, SectorStatus::Unreadable).unwrap();
    let disown = stale.disown_handle();

    let pause = |at: HookAt| {
        let (at_tx, at_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let hook: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = at_tx.send(());
            let _ = go_rx.recv();
        });
        WRITE_HOOKS.lock().unwrap().push((path.clone(), at, hook));
        (at_rx, go_tx)
    };
    let (a_at, a_go) = pause(HookAt::PreRename);
    let abandoned = std::thread::spawn(move || stale.flush());
    a_at.recv_timeout(Duration::from_secs(30))
        .expect("the abandoned writer never reached its rename");
    disown.disown();

    let (b_at, b_go) = pause(HookAt::PreRename);
    let owner_path = path.clone();
    let owner = std::thread::spawn(move || {
        Mapfile::create(&owner_path, 4096, "resumed-pass").map(|m| m.total_size())
    });
    // Without the lock the owner reaches its own rename; with it, it waits on the lock.
    let _ = b_at.recv_timeout(Duration::from_millis(500));
    a_go.send(()).unwrap();
    abandoned.join().unwrap().expect("the abandoned rename");
    b_go.send(()).unwrap();
    assert_eq!(owner.join().unwrap().expect("the owner's write"), 4096);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("Created by resumed-pass"), "{text}");
    assert!(!tmp_of(&path).exists(), "no tmp left behind");
}

// M27: a bare relative mapfile name syncs the current directory, not `""` (ENOENT).
#[test]
fn a_relative_mapfile_syncs_the_current_directory() {
    for p in ["rel.mapfile", "sub/rel.mapfile", "/abs/rel.mapfile"] {
        let dir = parent_dir(Path::new(p)).unwrap();
        assert!(!dir.as_os_str().is_empty(), "{p}");
    }
    let dir = parent_dir(Path::new("rel.mapfile")).unwrap();
    libfreemkv::io::fsync::dir_checked(dir).expect("fsync the current directory");
}

// N2: a symlink planted at the predictable tmp name is replaced, never written through.
#[cfg(unix)]
#[test]
fn a_symlink_planted_at_the_tmp_name_is_not_followed() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("m.mapfile");
    let victim = td.path().join("victim");
    std::fs::write(&victim, b"precious").unwrap();
    std::os::unix::fs::symlink(&victim, tmp_of(&path)).unwrap();
    Mapfile::create(&path, 4096, "test").unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(
        meta.file_type().is_file(),
        "the mapfile must be a regular file"
    );
    assert_eq!(Mapfile::load(&path).unwrap().total_size(), 4096);
}

// A failed write must not leave `<path>.tmp` behind: every `?` between
// `File::create(&tmp)` and the final `rename` used to orphan the tmp file.
// Reachable via a directory sitting on the destination name (rename fails).
#[test]
fn a_failed_write_does_not_orphan_the_tmp_file() {
    let td = tempfile::tempdir().unwrap();
    let dir = td.path();

    // Occupy the mapfile's own name with a non-empty DIRECTORY, so the
    // rename at the end of `write_to_disk` cannot succeed.
    let path = dir.join("m.mapfile");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("occupied"), b"x").unwrap();

    // `create` writes eagerly, so this exercises the failing path.
    let res = Mapfile::create(&path, 4096, "test");
    assert!(res.is_err(), "expected the rename onto a directory to fail");

    let tmp = {
        let mut s = path.clone().into_os_string();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    assert!(
        !tmp.exists(),
        "a partially-written tmp was left behind at {tmp:?}"
    );
}
