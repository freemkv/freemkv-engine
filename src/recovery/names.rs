//! The decrypted-name rename of a finished ISO. Rescue passes copy the disc's own names, so
//! sectors written by different passes never mix renamed and original metadata; the rename
//! runs once after the last pass, reading and writing only the image and its mapfile.
//!
//! `<iso>.names-undo` holds each touched sector's original bytes and mapfile status. It is
//! written before the image; the rebuilt mirror sectors are marked finished only once the
//! written image is verified, and the undo is then committed and dropped. A leftover pending
//! undo rolls back bytes and statuses ([`settle_names_undo`]); a committed one is just dropped.
//! A pass skips the sectors of an undo it could not roll back; the rename skips while any undo
//! is left.

use super::mapfile::{self, Mapfile, SectorStatus};
use libfreemkv::whole_disc::{DecryptedNames, ImageNames, NamesKept};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const SECTOR: u64 = 2048;
const MAGIC: &[u8; 8] = b"FMKVUND4";
// Header: magic, state, image length, record count, FNV-1a of all but the state byte; then
// per record lba, fill flag, mapfile status at plan time, planned bytes, original bytes.
const STATE_AT: usize = 8;
const HEADER: usize = 8 + 1 + 8 + 4 + 8;
const RECORD: usize = 4 + 1 + 1 + 2 * SECTOR as usize;
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
    Marked,
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
    // An undo that is still there could not be settled: leave the image as it is.
    let settled = settle_names_undo(path);
    let undo = names_undo_path(path);
    if settled != Settled::Done || undo.exists() {
        tracing::error!(target: "freemkv::udf", undo = %undo.display(), ?settled, "decrypted-name undo not settled; rename skipped");
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
    let statuses: Vec<SectorStatus> = plan
        .sectors
        .iter()
        .map(|&(lba, _)| status_at(&map, lba))
        .collect();
    drop(map);
    match apply(path, &plan, &statuses, &unreadable, aacs, hook) {
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

// Write the plan behind a pending undo, verify it, mark the rebuilt mirror sectors finished,
// then commit and drop the undo. Any failure before the commit rolls back.
fn apply(
    path: &Path,
    plan: &ImageNames,
    statuses: &[SectorStatus],
    unreadable: &[(u32, u32)],
    aacs: bool,
    hook: Hook<'_>,
) -> io::Result<()> {
    let records: Vec<Record> = {
        let mut file = std::fs::File::open(path)?;
        let mut out = Vec::with_capacity(plan.sectors.len());
        for (&(lba, _), &status) in plan.sectors.iter().zip(statuses) {
            let mut bytes = Box::new([0u8; SECTOR as usize]);
            file.seek(SeekFrom::Start(lba as u64 * SECTOR))?;
            file.read_exact(&mut bytes[..])?;
            let fill = plan.restored.contains(&lba);
            out.push(Record {
                lba,
                fill,
                status,
                planned: Box::new([0; SECTOR as usize]),
                bytes,
            });
        }
        out
    };
    let records: Vec<Record> = records
        .into_iter()
        .zip(&plan.sectors)
        .map(|(r, (_, planned))| Record {
            planned: planned.clone(),
            ..r
        })
        .collect();
    let image_len = std::fs::metadata(path)?.len();
    let undo = names_undo_path(path);
    write_undo(&undo, image_len, &records)?;
    let opened = hook(At::Open).and_then(|_| std::fs::OpenOptions::new().write(true).open(path));
    if let Err(e) = opened
        .and_then(|mut file| write_and_verify(&mut file, path, plan, unreadable, aacs, hook))
        .and_then(|_| mark_fills(path, &records))
        .and_then(|_| hook(At::Marked))
    {
        // The undo stays if the rollback itself fails; the next run settles it.
        rollback(path, &records)?;
        remove_undo(&undo)?;
        return Err(e);
    }
    // Verified and marked: either undo state now settles to a consistent image (pending rolls
    // back to the originals, committed is dropped), so a failed commit or delete is not a failure.
    let committed = commit_undo(&undo).and_then(|_| hook(At::Committed));
    if let Err(e) = committed.and_then(|_| remove_undo(&undo)) {
        tracing::warn!(target: "freemkv::udf", error = %e, "decrypted-name undo left for the next run");
    }
    Ok(())
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

/// How far [`settle_names_undo`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    /// No undo left to act on.
    Done,
    /// The original bytes are back; a mirror sector's status reset is still owed (undo kept).
    StatusOwed,
    /// The undo is kept with these sectors not rolled back.
    Unrestored(Vec<u32>),
    /// The undo is kept and cannot be read: its sectors are unknown.
    Unknown,
}

/// Settle a leftover `<iso>.names-undo`: roll a pending one back, drop a committed one or one
/// whose image is gone, resized or no longer holds what it wrote. Anything it cannot finish
/// keeps the undo for the next run.
pub(crate) fn settle_names_undo(path: &Path) -> Settled {
    let undo = names_undo_path(path);
    let bytes = match std::fs::read(&undo) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Settled::Done,
        Err(e) => {
            tracing::error!(target: "freemkv::udf", undo = %undo.display(), error = %e, "decrypted-name undo unreadable; kept");
            return Settled::Unknown;
        }
        Ok(bytes) => bytes,
    };
    let Some((state, image_len, records)) = parse_undo(&bytes) else {
        tracing::error!(target: "freemkv::udf", undo = %undo.display(), "decrypted-name undo is corrupt; kept");
        return Settled::Unknown;
    };
    let lbas = || Settled::Unrestored(records.iter().map(|r| r.lba).collect());
    let len = match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return lbas(),
        Ok(m) => Some(m.len()),
    };
    if len != Some(image_len) {
        tracing::warn!(target: "freemkv::udf", "dropping a decrypted-name undo whose image changed");
    } else if state == PENDING {
        match ours(path, &records) {
            Err(_) => return lbas(),
            Ok(None) => {
                tracing::warn!(target: "freemkv::udf", "dropping a decrypted-name undo the image no longer matches");
            }
            Ok(Some(undo)) => {
                let undo: Vec<&Record> = records
                    .iter()
                    .zip(undo)
                    .filter_map(|(r, u)| u.then_some(r))
                    .collect();
                if restore(path, &undo).is_err() {
                    return lbas();
                }
                // The bytes are the originals now: a pass re-reading a mirror sector reads
                // the disc's own copy, which matches the restored main.
                if reset_fills(path, &undo).is_err() {
                    return Settled::StatusOwed;
                }
            }
        }
    }
    match remove_undo(&undo) {
        Ok(()) => Settled::Done,
        Err(_) if state == PENDING => Settled::StatusOwed,
        Err(_) => Settled::Done,
    }
}

// Records to roll back, `None` for another image: each main byte is what the undo wrote or
// replaced (a torn write mixes them). A rebuilt mirror holding neither was read back from the
// disc by a pass, which matches the restored main: it is left, with its status.
fn ours(path: &Path, records: &[Record]) -> io::Result<Option<Vec<bool>>> {
    let mut file = std::fs::File::open(path)?;
    let mut got = [0u8; SECTOR as usize];
    let mut undo = Vec::with_capacity(records.len());
    for r in records {
        file.seek(SeekFrom::Start(r.lba as u64 * SECTOR))?;
        file.read_exact(&mut got)?;
        let mixed = (0..got.len()).all(|i| got[i] == r.bytes[i] || got[i] == r.planned[i]);
        if !mixed && !r.fill {
            return Ok(None);
        }
        undo.push(mixed);
    }
    Ok(Some(undo))
}

/// [`settle_names_undo`] before a pass reads the image, never fatal: the byte ranges the pass
/// must not read this run (an undo's sectors it could not roll back).
pub(crate) fn settle_before_pass(path: &Path) -> Vec<(u64, u64)> {
    match settle_names_undo(path) {
        Settled::Unrestored(lbas) => lbas
            .into_iter()
            .map(|lba| (lba as u64 * SECTOR, SECTOR))
            .collect(),
        _ => Vec::new(),
    }
}

/// Drop `<iso>.names-undo`: the image it describes is being replaced.
pub(crate) fn remove_names_undo(path: &Path) -> io::Result<()> {
    remove_undo(&names_undo_path(path))
}

struct Record {
    lba: u32,
    fill: bool,
    status: SectorStatus,
    planned: Box<[u8; SECTOR as usize]>,
    bytes: Box<[u8; SECTOR as usize]>,
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

// FNV-1a 64 over the undo, its state byte excluded (the commit rewrites it in place).
fn checksum(bytes: &[u8]) -> u64 {
    let mut b = bytes.to_vec();
    b[STATE_AT] = 0;
    fnv(&b)
}

fn write_undo(undo: &Path, image_len: u64, records: &[Record]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(HEADER + records.len() * RECORD);
    buf.extend_from_slice(MAGIC);
    buf.push(PENDING);
    buf.extend_from_slice(&image_len.to_le_bytes());
    buf.extend_from_slice(&(records.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0; 8]);
    for r in records {
        buf.extend_from_slice(&r.lba.to_le_bytes());
        buf.push(u8::from(r.fill));
        buf.push(r.status.to_char() as u8);
        buf.extend_from_slice(&r.planned[..]);
        buf.extend_from_slice(&r.bytes[..]);
    }
    let sum = checksum(&buf);
    buf[HEADER - 8..HEADER].copy_from_slice(&sum.to_le_bytes());
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
    match mapfile::parent_dir(undo) {
        Some(parent) => libfreemkv::io::fsync::dir_checked(parent),
        None => Ok(()),
    }
}

fn parse_undo(bytes: &[u8]) -> Option<(u8, u64, Vec<Record>)> {
    if bytes.len() < HEADER || &bytes[..8] != MAGIC {
        return None;
    }
    let state = bytes[STATE_AT];
    let image_len = u64::from_le_bytes(bytes[9..17].try_into().ok()?);
    let count = u32::from_le_bytes(bytes[17..21].try_into().ok()?) as usize;
    let sum = u64::from_le_bytes(bytes[21..29].try_into().ok()?);
    let mut zeroed = bytes.to_vec();
    zeroed[HEADER - 8..HEADER].fill(0);
    if state > COMMITTED
        || bytes.len() != HEADER + count.checked_mul(RECORD)?
        || checksum(&zeroed) != sum
    {
        return None;
    }
    (0..count)
        .map(|i| {
            let r = &bytes[HEADER + i * RECORD..][..RECORD];
            Some(Record {
                lba: u32::from_le_bytes(r[..4].try_into().ok()?),
                fill: r[4] != 0,
                status: SectorStatus::from_char(r[5] as char)?,
                planned: Box::new(r[6..6 + SECTOR as usize].try_into().ok()?),
                bytes: Box::new(r[6 + SECTOR as usize..].try_into().ok()?),
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(|records| (state, image_len, records))
}

fn commit_undo(undo: &Path) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(undo)?;
    f.seek(SeekFrom::Start(STATE_AT as u64))?;
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

// Every touched sector back to its original bytes, and each rebuilt mirror sector back to
// its plan-time status: a pass may have read it since, and it is unfinished again here.
fn rollback(path: &Path, records: &[Record]) -> io::Result<()> {
    let all: Vec<&Record> = records.iter().collect();
    restore(path, &all)?;
    reset_fills(path, &all)
}

#[cfg(test)]
thread_local! {
    // Fails `restore` on this thread.
    static FAIL_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn restore(path: &Path, records: &[&Record]) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_RESTORE.with(|f| f.get()) {
        return Err(io::Error::other("restore"));
    }
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    for r in records {
        file.seek(SeekFrom::Start(r.lba as u64 * SECTOR))?;
        file.write_all(&r.bytes[..])?;
    }
    file.sync_all()
}

fn reset_fills(path: &Path, records: &[&Record]) -> io::Result<()> {
    let reset: Vec<&Record> = records
        .iter()
        .copied()
        .filter(|r| r.fill && r.status != SectorStatus::Finished)
        .collect();
    if reset.is_empty() {
        return Ok(());
    }
    let mut map = match Mapfile::load(&mapfile::mapfile_path_for(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        r => r?,
    };
    for r in reset {
        map.record(r.lba as u64 * SECTOR, SECTOR, r.status)?;
    }
    map.flush()
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

fn status_at(map: &Mapfile, lba: u32) -> SectorStatus {
    let at = lba as u64 * SECTOR;
    map.entries()
        .iter()
        .find(|e| e.pos <= at && at < e.pos + e.size)
        .map_or(SectorStatus::NonTried, |e| e.status)
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
