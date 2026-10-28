//! The decrypted-name rename of a finished ISO. Rescue passes copy the disc's own names, so
//! sectors written by different passes never mix renamed and original metadata; the rename
//! runs once after the last pass, reading and writing only the image and its mapfile.
//!
//! The rename never demotes the mapfile. `<iso>.names-undo` holds the original bytes of every
//! sector it writes: pending until the written image is verified, then committed. A later run
//! settles a leftover undo before any pass reads ([`settle_names_undo`]): a pending one is
//! rolled back, a committed one has its rebuilt mirror sectors marked finished.

use super::mapfile::{self, Mapfile, SectorStatus};
use libfreemkv::whole_disc::{DecryptedNames, ImageNames, NamesKept};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SECTOR: u64 = 2048;
const MAGIC: &[u8; 8] = b"FMKVUNDO";
// Header: magic, state, image length, record count; then per record lba, fill flag, bytes.
const STATE_AT: u64 = 8;
const HEADER: usize = 8 + 1 + 8 + 4;
const RECORD: usize = 4 + 1 + SECTOR as usize;
const PENDING: u8 = 0;
const COMMITTED: u8 = 1;

/// The undo sidecar of the ISO at `path`.
pub(crate) fn names_undo_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".names-undo");
    PathBuf::from(s)
}

// A point the rename passes; a test seam that can fail, drop a write or crash there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum At {
    Open,
    Write(u32),
    Committed,
}

// `Ok(false)` skips the write at `At::Write`.
type Hook<'a> = &'a mut dyn FnMut(At) -> io::Result<bool>;

/// Rename `AACS/Unit_Key_RO.inf` in the decrypted ISO at `path`. `None` when the image has
/// no such name, or is not one to touch: not a regular file, raw, scoped, or without a mapfile.
pub(crate) fn finish_image_names(path: &Path, aacs: bool) -> Option<DecryptedNames> {
    finish_with(path, aacs, &mut |_| Ok(true))
}

fn finish_with(path: &Path, aacs: bool, hook: Hook<'_>) -> Option<DecryptedNames> {
    let kept = |cause| aacs.then_some(DecryptedNames::Kept { cause });
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    if let Err(e) = settle_names_undo(path) {
        tracing::error!(target: "freemkv::udf", error = %e, "decrypted-name undo not settled");
        return kept(NamesKept::Unexpected);
    }
    let map = Mapfile::load(&mapfile::mapfile_path_for(path)).ok()?;
    match (map.raw(), map.scope()) {
        (Some(false), None) => {}
        // A map that does not say decrypted can't vouch for the image's sectors.
        (None, None) => {
            tracing::warn!(target: "freemkv::udf", "decrypted-name rename skipped; the mapfile does not say the image is decrypted");
            return kept(NamesKept::Unexpected);
        }
        _ => return None,
    }
    let unreadable = unfinished(&map);
    drop(map);
    let plan = match libfreemkv::whole_disc::plan_image_names(path, &unreadable) {
        Ok(Some(plan)) => plan,
        Ok(None) => return libfreemkv::whole_disc::check_image_names(path, &unreadable, aacs),
        // Nothing written: a later run that reads the damage renames it.
        Err(e) => {
            tracing::warn!(
                target: "freemkv::udf",
                code = e.code(),
                "decrypted-name rename skipped; image keeps its original names"
            );
            return kept(NamesKept::from_plan_error(&e, &unreadable));
        }
    };
    match apply(path, &plan, &unreadable, aacs, hook) {
        Ok(()) => Some(DecryptedNames::Renamed {
            mirror_sectors_restored: plan.restored.len() as u32,
        }),
        Err(e) => {
            tracing::error!(target: "freemkv::udf", error = %e, "decrypted-name rename not applied");
            Some(DecryptedNames::Kept {
                cause: NamesKept::Unexpected,
            })
        }
    }
}

// Write the plan behind a pending undo, verify it, then commit and mark the rebuilt mirror
// sectors finished. Any failure before the commit restores the originals.
fn apply(
    path: &Path,
    plan: &ImageNames,
    unreadable: &[(u32, u32)],
    aacs: bool,
    hook: Hook<'_>,
) -> io::Result<()> {
    let records: Vec<Record> = {
        let mut file = std::fs::File::open(path)?;
        let mut out = Vec::with_capacity(plan.sectors.len());
        for &(lba, _) in &plan.sectors {
            let mut bytes = Box::new([0u8; SECTOR as usize]);
            file.seek(SeekFrom::Start(lba as u64 * SECTOR))?;
            file.read_exact(&mut bytes[..])?;
            let fill = plan.restored.contains(&lba);
            out.push(Record { lba, fill, bytes });
        }
        out
    };
    let image_len = std::fs::metadata(path)?.len();
    let undo = names_undo_path(path);
    write_undo(&undo, image_len, &records)?;
    let opened = hook(At::Open).and_then(|_| std::fs::OpenOptions::new().write(true).open(path));
    let mut file = match opened {
        Ok(file) => file,
        Err(e) => {
            remove_undo(&undo)?;
            return Err(e);
        }
    };
    if let Err(e) = write_and_verify(&mut file, path, plan, unreadable, aacs, hook) {
        restore(&mut file, &records, |_| true)?;
        remove_undo(&undo)?;
        return Err(e);
    }
    let committed = commit_undo(&undo).and_then(|_| hook(At::Committed));
    if let Err(e) = committed {
        restore(&mut file, &records, |_| true)?;
        remove_undo(&undo)?;
        return Err(e);
    }
    // Committed: a failure from here leaves the undo for the next run to settle.
    mark_fills(path, &records)?;
    remove_undo(&undo)
}

fn write_and_verify(
    file: &mut std::fs::File,
    path: &Path,
    plan: &ImageNames,
    unreadable: &[(u32, u32)],
    aacs: bool,
    hook: Hook<'_>,
) -> io::Result<()> {
    for (lba, bytes) in &plan.sectors {
        if hook(At::Write(*lba))? {
            file.seek(SeekFrom::Start(*lba as u64 * SECTOR))?;
            file.write_all(&bytes[..])?;
        }
    }
    file.sync_all()?;
    let mut back = std::fs::File::open(path)?;
    let mut got = [0u8; SECTOR as usize];
    for (lba, bytes) in &plan.sectors {
        back.seek(SeekFrom::Start(*lba as u64 * SECTOR))?;
        back.read_exact(&mut got)?;
        if got != **bytes {
            return Err(io::Error::other(format!("sector {lba} did not take")));
        }
    }
    let mask = without(unreadable, &plan.restored);
    match libfreemkv::whole_disc::check_image_names(path, &mask, aacs) {
        Some(DecryptedNames::Renamed { .. }) => Ok(()),
        got => Err(io::Error::other(format!("the image lists {got:?}"))),
    }
}

/// Settle a leftover `<iso>.names-undo` before anything reads the image: roll a pending one
/// back (except mirror sectors a pass has since read), finish a committed one. An undo whose
/// image or decrypted mapfile is gone is dropped.
pub(crate) fn settle_names_undo(path: &Path) -> io::Result<()> {
    let undo = names_undo_path(path);
    let bytes = match std::fs::read(&undo) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        r => r?,
    };
    let map = Mapfile::load(&mapfile::mapfile_path_for(path))
        .ok()
        .filter(|m| m.raw() == Some(false));
    let len = std::fs::metadata(path).ok().map(|m| m.len());
    match (parse_undo(&bytes), map) {
        (Some((state, image_len, records)), Some(map)) if len == Some(image_len) => {
            if state == COMMITTED {
                drop(map);
                mark_fills(path, &records)?;
            } else {
                let finished = map.ranges_with(&[SectorStatus::Finished]);
                let read_since = |r: &Record| r.fill && covered(&finished, r.lba);
                let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
                restore(&mut file, &records, |r| !read_since(r))?;
            }
        }
        _ => {
            tracing::warn!(target: "freemkv::udf", "dropping a decrypted-name undo whose image changed")
        }
    }
    remove_undo(&undo)
}

/// Drop `<iso>.names-undo`: the image it describes is being replaced.
pub(crate) fn remove_names_undo(path: &Path) -> io::Result<()> {
    remove_undo(&names_undo_path(path))
}

struct Record {
    lba: u32,
    fill: bool,
    bytes: Box<[u8; SECTOR as usize]>,
}

fn write_undo(undo: &Path, image_len: u64, records: &[Record]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(HEADER + records.len() * RECORD);
    buf.extend_from_slice(MAGIC);
    buf.push(PENDING);
    buf.extend_from_slice(&image_len.to_le_bytes());
    buf.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for r in records {
        buf.extend_from_slice(&r.lba.to_le_bytes());
        buf.push(u8::from(r.fill));
        buf.extend_from_slice(&r.bytes[..]);
    }
    let mut tmp = undo.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = || -> io::Result<()> {
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        std::fs::rename(&tmp, undo)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    sync_parent(undo)
}

fn parse_undo(bytes: &[u8]) -> Option<(u8, u64, Vec<Record>)> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return None;
    }
    let state = bytes[8];
    let image_len = u64::from_le_bytes(bytes[9..17].try_into().ok()?);
    let count = u32::from_le_bytes(bytes[17..21].try_into().ok()?) as usize;
    if state > COMMITTED || bytes.len() != HEADER + count * RECORD {
        return None;
    }
    let records = (0..count)
        .map(|i| {
            let r = &bytes[HEADER + i * RECORD..][..RECORD];
            Record {
                lba: u32::from_le_bytes(r[..4].try_into().unwrap()),
                fill: r[4] != 0,
                bytes: Box::new(r[5..].try_into().unwrap()),
            }
        })
        .collect();
    Some((state, image_len, records))
}

fn commit_undo(undo: &Path) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(undo)?;
    f.seek(SeekFrom::Start(STATE_AT))?;
    f.write_all(&[COMMITTED])?;
    f.sync_all()
}

fn remove_undo(undo: &Path) -> io::Result<()> {
    match std::fs::remove_file(undo) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => sync_parent(undo),
    }
}

fn sync_parent(p: &Path) -> io::Result<()> {
    if let Some(parent) = mapfile::parent_dir(p) {
        libfreemkv::io::fsync::dir(parent);
    }
    Ok(())
}

fn restore(
    file: &mut std::fs::File,
    records: &[Record],
    keep: impl Fn(&Record) -> bool,
) -> io::Result<()> {
    for r in records.iter().filter(|r| keep(r)) {
        file.seek(SeekFrom::Start(r.lba as u64 * SECTOR))?;
        file.write_all(&r.bytes[..])?;
    }
    file.sync_all()
}

// The rebuilt mirror sectors are now confirmed: no pass reads them again.
fn mark_fills(path: &Path, records: &[Record]) -> io::Result<()> {
    if !records.iter().any(|r| r.fill) {
        return Ok(());
    }
    let mut map = Mapfile::load(&mapfile::mapfile_path_for(path))?;
    for r in records.iter().filter(|r| r.fill) {
        map.record(r.lba as u64 * SECTOR, SECTOR, SectorStatus::Finished)?;
    }
    map.flush()
}

// `ranges` less the single sectors `lbas` (the rebuilt mirror sectors, now written).
fn without(ranges: &[(u32, u32)], lbas: &[u32]) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(ranges.len());
    for &(start, n) in ranges {
        let end = start as u64 + n as u64;
        let mut at = start as u64;
        let mut cuts: Vec<u64> = lbas
            .iter()
            .map(|&l| l as u64)
            .filter(|&l| l >= at && l < end)
            .collect();
        cuts.sort_unstable();
        for cut in cuts {
            if cut > at {
                out.push((at as u32, (cut - at) as u32));
            }
            at = cut + 1;
        }
        if end > at {
            out.push((at as u32, (end - at) as u32));
        }
    }
    out
}

fn covered(ranges: &[(u64, u64)], lba: u32) -> bool {
    let at = lba as u64 * SECTOR;
    ranges.iter().any(|&(p, n)| p <= at && at < p + n)
}

// Every sector the mapfile does not mark finished, as `(start, count)`.
fn unfinished(map: &Mapfile) -> Vec<(u32, u32)> {
    map.ranges_with(&mapfile::bad_sector_statuses())
        .into_iter()
        .map(|(pos, size)| {
            let start = pos / SECTOR;
            let end = (pos + size).div_ceil(SECTOR);
            let start = start.min(u32::MAX as u64) as u32;
            (
                start,
                (end.min(u32::MAX as u64) as u32).saturating_sub(start),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "names_tests.rs"]
mod tests;
