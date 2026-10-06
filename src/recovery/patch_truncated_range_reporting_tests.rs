use super::*;
use crate::recovery::mapfile::Mapfile;

const SECTOR: u64 = 2048;
/// Comfortably more damaged runs than the old 8192 snapshot cap kept, so
/// the truncation is unambiguously exercised.
const DAMAGED_RUNS: u64 = 9000;

// Builds a mapfile holding `DAMAGED_RUNS` single-sector Unreadable runs,
// separated by Finished sectors so nothing coalesces. Written as text and
// loaded, not recorded run by run, since `record()` is O(entries).
fn fragmented_mapfile(dir: &std::path::Path) -> Mapfile {
    let mut s = String::from(
        "# Rescue Logfile. Created by test\n\
             # Current pos / status / pass / pass_time\n\
             0x000000000  ?  1  0\n\
             #      pos        size  status\n",
    );
    use std::fmt::Write as _;
    for i in 0..DAMAGED_RUNS {
        let bad = i * 2 * SECTOR;
        let good = bad + SECTOR;
        let _ = writeln!(s, "0x{bad:09x}  0x{SECTOR:09x}    -");
        let _ = writeln!(s, "0x{good:09x}  0x{SECTOR:09x}    +");
    }
    let p = dir.join("fragmented.mapfile");
    std::fs::write(&p, s).unwrap();
    Mapfile::load(&p).unwrap()
}

/// A title spanning the whole fragmented region, so every damaged run falls
/// inside its extents and the expected totals are plain literals.
fn spanning_title() -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents = vec![libfreemkv::disc::Extent {
        start_lba: 0,
        sector_count: (DAMAGED_RUNS * 2) as u32,
    }];
    t.size_bytes = DAMAGED_RUNS * 2 * SECTOR;
    t.duration_secs = 3600.0;
    t
}

#[test]
fn snapshot_totals_cover_every_range_of_a_fragmented_disc() {
    let d = std::env::temp_dir().join(format!("fmkv-trunc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let map = fragmented_mapfile(&d);
    let title = spanning_title();
    let shared = SharedPatchState::from_map(&map, Some(&title));

    // Expected values, from literals — 9000 damaged single-sector runs,
    // every one of them inside the title.
    assert_eq!(shared.bad_bytes_in_title, 9000 * SECTOR);
    assert_eq!(shared.located.num_ranges, 9000);
    // libfreemkv's own display cap keeps the 50 biggest; the rest are
    // reported as the "+N more" tail, not silently dropped.
    assert_eq!(shared.located.ranges.len(), 50);
    assert_eq!(shared.located.truncated, 8950);
    // At-risk movie time: 9000 damaged sectors of an 18000-sector,
    // 3600 s title = half the runtime.
    assert!(
        (shared.located.main_at_risk_ms - 1_800_000.0).abs() < 1.0,
        "at-risk time {} ms should be half the runtime",
        shared.located.main_at_risk_ms
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A disc with no titles has nothing to locate damage against; the snapshot
/// reports zero rather than inventing a drilldown.
#[test]
fn no_title_yields_an_empty_drilldown() {
    let d = std::env::temp_dir().join(format!("fmkv-trunc-nt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let map = fragmented_mapfile(&d);
    let shared = SharedPatchState::from_map(&map, None);
    assert_eq!(shared.bad_bytes_in_title, 0);
    assert_eq!(shared.located.num_ranges, 0);
    assert!(shared.located.ranges.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
