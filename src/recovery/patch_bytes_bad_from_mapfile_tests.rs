use super::*;
use crate::recovery::mapfile::{Mapfile, SectorStatus};

const SECTOR: u64 = 2048;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fmkv-bbt-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A title occupying sectors [start_lba, start_lba + count).
fn title_at(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents = vec![libfreemkv::disc::Extent {
        start_lba,
        sector_count,
    }];
    t
}

/// No mapfile means no damage was ever tracked — a clean single-pass rip.
/// Zero is the right answer here, and only here.
#[test]
fn missing_mapfile_is_clean() {
    let d = tmpdir("missing");
    let bad = bytes_bad_in_title_from_mapfile(&d.join("nope.mapfile"), &title_at(0, 100));
    assert_eq!(bad, 0);
}

// A mapfile that EXISTS but cannot be parsed means the damage record is
// gone; returning 0 would read as "clean", so the fail-safe reports the
// whole title bad — the difference between "fine" and "cannot tell".
#[test]
fn corrupt_mapfile_reports_the_whole_title_bad() {
    let d = tmpdir("corrupt");
    let p = d.join("broken.mapfile");
    std::fs::write(&p, b"this is not a rescue logfile\n\x00\xff garbage").unwrap();
    let title = title_at(10, 50);
    let bad = bytes_bad_in_title_from_mapfile(&p, &title);
    assert_eq!(
        bad,
        50 * SECTOR,
        "an unreadable damage record must surface as maximal loss, not as a clean rip"
    );
}

/// The normal path: only the bad bytes that actually fall inside the
/// title's extents count. Damage elsewhere on the disc is not this title's.
#[test]
fn counts_only_damage_inside_the_title_extents() {
    let d = tmpdir("scoped");
    let p = d.join("scoped.mapfile");
    let total = 1000 * SECTOR;
    let mut mf = Mapfile::create(&p, total, "test").unwrap();
    // Everything good, then damage inside the title and damage outside it.
    mf.record(0, total, SectorStatus::Finished).unwrap();
    mf.record(100 * SECTOR, 10 * SECTOR, SectorStatus::Unreadable)
        .unwrap();
    mf.record(900 * SECTOR, 20 * SECTOR, SectorStatus::Unreadable)
        .unwrap();
    mf.flush().unwrap();

    // Title spans sectors 50..150, so it contains the first damage only.
    let bad = bytes_bad_in_title_from_mapfile(&p, &title_at(50, 100));
    assert_eq!(
        bad,
        10 * SECTOR,
        "damage outside the extents must not count"
    );
}

/// The CONVERGENCE set, not the damage set. An interrupted rip leaves
/// `NonTried` sectors: they are unread, not proven good, and a front-end
/// that ignores them reports a half-finished rip as clean.
#[test]
fn unread_sectors_count_as_bad() {
    let d = tmpdir("nontried");
    let p = d.join("nontried.mapfile");
    let total = 200 * SECTOR;
    let mut mf = Mapfile::create(&p, total, "test").unwrap();
    // First half read clean; second half never attempted.
    mf.record(0, 100 * SECTOR, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();

    let bad = bytes_bad_in_title_from_mapfile(&p, &title_at(0, 200));
    assert_eq!(
        bad,
        100 * SECTOR,
        "NonTried is not good — an interrupted rip must not report clean"
    );
}

/// A fully-good mapfile really is zero, so the fail-safes above are not
/// just "always returns nonzero".
#[test]
fn a_fully_read_title_is_zero() {
    let d = tmpdir("clean");
    let p = d.join("clean.mapfile");
    let total = 200 * SECTOR;
    let mut mf = Mapfile::create(&p, total, "test").unwrap();
    mf.record(0, total, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();

    assert_eq!(
        bytes_bad_in_title_from_mapfile(&p, &title_at(0, 200)),
        0,
        "a fully-Finished mapfile must report no loss"
    );
}
