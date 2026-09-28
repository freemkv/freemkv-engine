//! The `<final>.lock` artifact sidecar (stop design v5 §2.5, T10).
//!
//! An exclusive `flock` / `LockFileEx` on a sidecar named after the FINAL artifact
//! (`Movie.mkv.lock` for `Movie.mkv` and `Movie.mkv.partial`), so two writers of one
//! artifact never interleave. The acquire loop re-checks that the locked file is still
//! the one the path names, closing the unlink race: a deleter deletes only while holding.
//! Same-host scope (J-5.5-1). Waits are halt-aware and stall-based: `TimedOut
//! { op: "artifact_lock" }` (E9073) only after 30 s with no change to the watched files.

use crate::engine_halt::EngineHalt;
use libfreemkv::halt::{Progress, Stall, StallTimer, WAIT_SLICE};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// T10: §3.1 "30 s (v4 value, kept)" with no progress from the holder.
pub(crate) const LOCK_STALL: Duration = Duration::from_secs(30);

/// §2.5: "The sidecar is always named after the **final** artifact name, `<final>.lock`".
pub(crate) fn sidecar_for(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let base = name.strip_suffix(".partial").unwrap_or(&name);
    path.with_file_name(format!("{base}.lock"))
}

/// The OS primitives of the acquire loop; a seam so a test can inject `ESTALE`.
pub(crate) trait LockOps: Sync {
    /// Open (creating) the sidecar read-write.
    fn open(&self, path: &Path) -> io::Result<File>;
    /// Take the exclusive lock without blocking: `Ok(false)` while another holds it.
    fn try_lock(&self, file: &File) -> io::Result<bool>;
    /// Whether `file` is the file `path` names now; `Err` (e.g. `ESTALE`) means "retry".
    fn same(&self, file: &File, path: &Path) -> io::Result<bool>;
}

/// The production primitives.
pub(crate) struct OsLockOps;

impl LockOps for OsLockOps {
    fn open(&self, path: &Path) -> io::Result<File> {
        let mut o = std::fs::OpenOptions::new();
        // SS-8 flock(2) NOTES: on NFS "in order to place an exclusive lock, the file must be
        // opened for writing" — always read-write (ET11h).
        o.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // SS-10 CreateFileW FILE_SHARE_DELETE: "Enables subsequent open operations on a
            // file or device to request delete access" (read + write + delete sharing).
            o.share_mode(0x1 | 0x2 | 0x4);
        }
        o.open(path)
    }

    fn try_lock(&self, file: &File) -> io::Result<bool> {
        // SS-8 flock(2): "LOCK_EX Place an exclusive lock"; SS-10 LockFileEx
        // LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY (fs4's `try_lock`).
        match fs4::FileExt::try_lock(file) {
            Ok(()) => Ok(true),
            Err(fs4::TryLockError::WouldBlock) => Ok(false),
            Err(fs4::TryLockError::Error(e)) => Err(e),
        }
    }

    fn same(&self, file: &File, path: &Path) -> io::Result<bool> {
        same_file(file, path)
    }
}

// SS-9 XBD <sys/stat.h>: "The st_ino and st_dev fields taken together uniquely identify the
// file within the system."
#[cfg(unix)]
fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (held, named) = (file.metadata()?, std::fs::metadata(path)?);
    Ok(held.dev() == named.dev() && held.ino() == named.ino())
}

// SS-10 BY_HANDLE_FILE_INFORMATION: "You can compare the VolumeSerialNumber and FileIndex
// members … to determine if two paths map to the same target".
#[cfg(windows)]
fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    let held = same_file::Handle::from_file(file.try_clone()?)?;
    Ok(held == same_file::Handle::from_path(path)?)
}

/// A held sidecar lock. Dropping it releases the lock and keeps the sidecar (§2.5: kept
/// after Stop, a failure or a crash); [`delete_while_held`](Self::delete_while_held)
/// removes it first.
#[derive(Debug)]
pub(crate) struct ArtifactLock {
    file: File,
    path: PathBuf,
}

impl ArtifactLock {
    /// Take `<final>.lock` for `artifact` (its final or `.partial` name), waiting halt-aware
    /// while the holder's `watch` files change; `TimedOut { op: "artifact_lock" }` after
    /// `stall` with none.
    pub(crate) fn acquire(
        artifact: &Path,
        watch: &[PathBuf],
        halt: &EngineHalt<'_>,
        stall: Duration,
    ) -> io::Result<Self> {
        Self::acquire_with(&OsLockOps, artifact, watch, halt, stall)
    }

    pub(crate) fn acquire_with(
        ops: &dyn LockOps,
        artifact: &Path,
        watch: &[PathBuf],
        halt: &EngineHalt<'_>,
        stall: Duration,
    ) -> io::Result<Self> {
        let path = sidecar_for(artifact);
        let progress = Progress::new();
        let mut timer = StallTimer::new(stall, &progress);
        let mut seen = snapshot(watch);
        // One slice of waiting: `Err` on a cancel or once the holder has stalled.
        let mut idle = || -> io::Result<()> {
            if halt.wait(WAIT_SLICE) {
                return Err(libfreemkv::Error::Halted.into());
            }
            let now = snapshot(watch);
            if now != seen {
                seen = now;
                progress.bump();
            }
            match timer.poll(&progress) {
                Stall::Expired => Err(libfreemkv::Error::TimedOut {
                    op: "artifact_lock",
                }
                .into()),
                _ => Ok(()),
            }
        };
        loop {
            if halt.is_cancelled() {
                return Err(libfreemkv::Error::Halted.into());
            }
            let file = match ops.open(&path) {
                Ok(f) => f,
                Err(e) if retry_open(&e) => {
                    idle()?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            while !ops.try_lock(&file)? {
                idle()?;
            }
            // §2.5: "(a, Ok(b)) if a == b => break"; "(_, Err(ENOENT | ESTALE)) | _ =>
            // close(fd), retry".
            if matches!(ops.same(&file, &path), Ok(true)) {
                return Ok(Self { file, path });
            }
            drop(file);
        }
    }

    /// The sidecar's path.
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// §2.5: "Deleted on success … and on Discard, both while holding it."
    pub(crate) fn delete_while_held(self) -> io::Result<()> {
        let r = match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        drop(self.file);
        r
    }
}

// §2.5 (Windows): "a delete-pending open fails with `ERROR_ACCESS_DENIED` … The loop retries."
fn retry_open(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
        || (cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied)
}

// T10's progress signal, §2.5: "the size or mtime of the `.partial` and the mapfile, read
// **by path** on each poll".
fn snapshot(watch: &[PathBuf]) -> Vec<Option<(u64, Option<SystemTime>)>> {
    watch
        .iter()
        .map(|p| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        })
        .collect()
}

/// A sidecar lock deleted on every exit, a panic included (§4.2: remux keeps no
/// resumable state, "so the sidecar is **deleted while held on every exit**").
#[derive(Debug)]
pub(crate) struct DeleteOnDrop(Option<ArtifactLock>);

impl DeleteOnDrop {
    pub(crate) fn new(lock: ArtifactLock) -> Self {
        Self(Some(lock))
    }
}

impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        if let Some(lock) = self.0.take() {
            let _ = lock.delete_while_held();
        }
    }
}

#[cfg(test)]
mod tests;
