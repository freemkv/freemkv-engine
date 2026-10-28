//! The decrypted-name rename of a finished ISO. Rescue passes copy the disc's own names, so
//! sectors written by different passes never mix renamed and original metadata; the rename
//! runs once after the last pass, reading and writing only the image and its mapfile.

use super::mapfile::{self, Mapfile, SectorStatus};
use libfreemkv::whole_disc::{DecryptedNames, ImageNames, NamesKept};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

const SECTOR: u64 = 2048;

// Writes one whole sector of the image; a test seam for a failed or lost write.
type WriteSector<'a> = &'a mut dyn FnMut(&mut std::fs::File, u32, &[u8]) -> std::io::Result<()>;

/// Rename `AACS/Unit_Key_RO.inf` in the decrypted ISO at `path`. `None` when the image has
/// no such name, or is not one to touch: not a regular file, raw, scoped, or without a mapfile.
pub(crate) fn finish_image_names(path: &Path, aacs: bool) -> Option<DecryptedNames> {
    finish_with(path, aacs, &mut |file, lba, bytes| {
        file.seek(SeekFrom::Start(lba as u64 * SECTOR))?;
        file.write_all(bytes)
    })
}

fn finish_with(path: &Path, aacs: bool, write: WriteSector<'_>) -> Option<DecryptedNames> {
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut map = Mapfile::load(&mapfile::mapfile_path_for(path)).ok()?;
    if map.raw() != Some(false) || map.scope().is_some() {
        return None;
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
            let cause = NamesKept::from_plan_error(&e, &unreadable);
            return aacs.then_some(DecryptedNames::Kept { cause });
        }
    };
    if let Err(e) = write_names(&mut map, path, &plan, &unreadable, write) {
        tracing::error!(target: "freemkv::udf", error = %e, "decrypted-name rename write failed");
        return Some(DecryptedNames::Kept {
            cause: NamesKept::Unexpected,
        });
    }
    match libfreemkv::whole_disc::check_image_names(path, &unfinished(&map), aacs) {
        Some(DecryptedNames::Renamed { .. }) => Some(DecryptedNames::Renamed {
            mirror_sectors_restored: plan.restored.len() as u32,
        }),
        got => {
            tracing::error!(target: "freemkv::udf", ?got, "decrypted-name rename did not take");
            Some(DecryptedNames::Kept {
                cause: NamesKept::Unexpected,
            })
        }
    }
}

// The mapfile is the journal: the finished sectors about to change are marked unread first,
// so a crash before the last record leaves them for a resume to read again from the disc.
fn write_names(
    map: &mut Mapfile,
    path: &Path,
    plan: &ImageNames,
    unreadable: &[(u32, u32)],
    write: WriteSector<'_>,
) -> std::io::Result<()> {
    for &(lba, _) in &plan.sectors {
        if !unreadable.iter().any(|&(s, n)| lba >= s && lba - s < n) {
            map.record(lba as u64 * SECTOR, SECTOR, SectorStatus::NonTried)?;
        }
    }
    map.flush()?;
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    for (lba, bytes) in &plan.sectors {
        write(&mut file, *lba, &bytes[..])?;
    }
    file.sync_all()?;
    for &(lba, _) in &plan.sectors {
        map.record(lba as u64 * SECTOR, SECTOR, SectorStatus::Finished)?;
    }
    map.flush()
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
