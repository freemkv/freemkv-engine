//! ET11-ET11h (stop design v5 §5.4): the `<final>.lock` sidecar.
//! Per spec; do not change without a spec citation proving otherwise.

use super::*;
use crate::engine_halt::EngineHalt;
use libfreemkv::Halt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn no_stop() -> EngineHalt<'static> {
    EngineHalt::new(&Halt::new(), None)
}

// A second acquire of a held sidecar that must NOT succeed within `for_`.
fn blocked_for(artifact: &Path, watch: &[PathBuf], for_: Duration) -> io::Result<ArtifactLock> {
    ArtifactLock::acquire(artifact, watch, &no_stop(), for_)
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
    let held = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    let got = Arc::new(Mutex::new(None));
    let (iso2, mf2, got2) = (iso.clone(), mf.clone(), got.clone());
    // The waiter's window (300 ms) is far shorter than the holder's 10 flushes (~1 s): it
    // survives only because each flush's rename is seen as progress, by path.
    let waiter = std::thread::spawn(move || {
        let r = ArtifactLock::acquire(&iso2, &[mf2], &no_stop(), Duration::from_millis(300));
        *got2.lock().unwrap() = Some(Instant::now());
        r
    });
    for i in 0..10u64 {
        map.record(i * 2048, 2048, crate::SectorStatus::Finished)
            .unwrap();
        map.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(got.lock().unwrap().is_none(), "acquired while held");
    }
    let released = Instant::now();
    drop(held);
    let lock = waiter.join().unwrap().expect("acquires once released");
    assert!(got.lock().unwrap().unwrap() >= released);
    assert_eq!(lock.path(), sidecar_for(&iso));
}

// ET11b `discard_then_waiter_cannot_share_the_artifact` — §2.5: "acquire = lock + id
// re-check with retry (fixes the unlink race)"; "never two owners".
#[test]
fn discard_then_waiter_cannot_share_the_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let iso = Arc::new(dir.path().join("Movie.iso"));
    let a = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    let (owners, max) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let contender = |delay: Duration| {
        let (iso, owners, max) = (iso.clone(), owners.clone(), max.clone());
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let lock = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
            // It holds the file the path names (the id re-check).
            assert!(OsLockOps.same(&lock.file, lock.path()).unwrap());
            let now = owners.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            owners.fetch_sub(1, Ordering::SeqCst);
        })
    };
    let b = contender(Duration::ZERO);
    std::thread::sleep(Duration::from_millis(100)); // B is blocked on A's (old) file
    a.delete_while_held().unwrap(); // Discard: delete while holding, then release
    let c = contender(Duration::ZERO);
    b.join().unwrap();
    c.join().unwrap();
    assert_eq!(max.load(Ordering::SeqCst), 1, "two owners at once");
}

// ET11c `success_deletes_sidecar_crash_keeps_it` — §2.5: "Deleted on success … Kept after
// Stop, a failure or a crash, where it guards the resumable artifact."
#[test]
fn success_deletes_sidecar_crash_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let lock = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    lock.delete_while_held().unwrap();
    assert!(
        !sidecar_for(&iso).exists(),
        "a finalize removes <final>.lock"
    );
    let lock = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    drop(lock); // a Stop (or a crash: the process's fds close)
    assert!(sidecar_for(&iso).exists(), "a Stop keeps it");
    let resumed = blocked_for(&iso, &[], Duration::from_millis(200));
    assert!(resumed.is_ok(), "Resume re-acquires it");
}

// ET11d `same_stem_artifacts_do_not_share_a_lock` — §2.5: "Two artifacts that share a stem
// (`Movie.iso`, `Movie.mkv`) never share a lock."
#[test]
fn same_stem_artifacts_do_not_share_a_lock() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let mkv = dir.path().join("Movie.mkv");
    assert_ne!(sidecar_for(&iso), sidecar_for(&mkv));
    let _a = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    let b = blocked_for(&mkv, &[], Duration::from_millis(200));
    assert!(b.is_ok(), "Movie.mkv locks independently of Movie.iso");
}

// ET11e `windows_share_delete_sidecar` — compile-only on dev; runs in qa Windows
// `release-tests`. SS-10 CreateFileW FILE_SHARE_DELETE: "Enables subsequent open operations
// on a file or device to request delete access".
#[cfg(windows)]
#[test]
fn windows_share_delete_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let iso = Arc::new(dir.path().join("Movie.iso"));
    let a = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    let iso2 = iso.clone();
    let waiter =
        std::thread::spawn(move || ArtifactLock::acquire(&iso2, &[], &no_stop(), LOCK_STALL));
    std::thread::sleep(Duration::from_millis(100));
    a.delete_while_held().unwrap();
    let lock = waiter
        .join()
        .unwrap()
        .expect("the waiter retries onto a new file");
    assert!(OsLockOps.same(&lock.file, lock.path()).unwrap());
}

// ET11f `estale_on_id_check_retries` — §2.5: "`ESTALE` from `fstat`/`stat`, meaning another
// client deleted the file, is treated as 'retry'".
#[test]
fn estale_on_id_check_retries() {
    struct Stale(AtomicUsize);
    impl LockOps for Stale {
        fn open(&self, path: &Path) -> io::Result<File> {
            OsLockOps.open(path)
        }
        fn try_lock(&self, file: &File) -> io::Result<bool> {
            OsLockOps.try_lock(file)
        }
        fn same(&self, file: &File, path: &Path) -> io::Result<bool> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                // Linux ESTALE (116): "Stale file handle".
                return Err(io::Error::from_raw_os_error(116));
            }
            OsLockOps.same(file, path)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let ops = Stale(AtomicUsize::new(0));
    let lock = ArtifactLock::acquire_with(&ops, &iso, &[], &no_stop(), LOCK_STALL).unwrap();
    assert_eq!(
        ops.0.load(Ordering::SeqCst),
        2,
        "closed, retried, then acquired"
    );
    assert!(OsLockOps.same(&lock.file, lock.path()).unwrap());
}

// ET11g `resume_by_final_name_contends_with_live_partial` — §2.5: "A Resume that knows only
// `Movie.iso` and a live op writing `Movie.iso.partial` therefore contend on the same file."
#[test]
fn resume_by_final_name_contends_with_live_partial() {
    let dir = tempfile::tempdir().unwrap();
    let partial = dir.path().join("Movie.iso.partial");
    let _live = ArtifactLock::acquire(&partial, &[], &no_stop(), LOCK_STALL).unwrap();
    let resume = blocked_for(
        &dir.path().join("Movie.iso"),
        &[],
        Duration::from_millis(200),
    );
    let e = resume.expect_err("blocks on the same file");
    assert_eq!(
        crate::error_code(&e),
        Some(libfreemkv::error::E_TIMED_OUT),
        "a frozen holder: TimedOut{{artifact_lock}} (T10)"
    );
}

// ET11h `sidecar_opened_read_write` — SS-8 flock(2) NFS: "in order to place an exclusive
// lock, the file must be opened for writing".
#[test]
fn sidecar_opened_read_write() {
    let dir = tempfile::tempdir().unwrap();
    let lock =
        ArtifactLock::acquire(&dir.path().join("Movie.iso"), &[], &no_stop(), LOCK_STALL).unwrap();
    lock.file
        .set_len(0)
        .expect("a read-only open rejects ftruncate: the sidecar must be open for writing");
}

// T10 (§3.1): the wait is halt-aware.
#[test]
fn a_cancel_during_the_wait_is_halted() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("Movie.iso");
    let _held = ArtifactLock::acquire(&iso, &[], &no_stop(), LOCK_STALL).unwrap();
    let op = Halt::new();
    let o = op.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        o.cancel();
    });
    let t0 = Instant::now();
    let e = ArtifactLock::acquire(&iso, &[], &EngineHalt::new(&op, None), LOCK_STALL).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert!(t0.elapsed() < Duration::from_secs(1));
}
