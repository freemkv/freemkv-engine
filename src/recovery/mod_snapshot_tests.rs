use super::*;

// The pass-boundary / done-card snapshot must actually read the mapfile —
// `None` ("no card to paint") is a legitimate answer for an absent
// mapfile too, so this pins the real-content case, not just non-panic.
#[test]
fn a_snapshot_reports_the_damage_the_mapfile_records() {
    let dir = std::env::temp_dir().join(format!("fmkv-engine-snap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mf = dir.join("snap.map");

    // 200 sectors: 150 good, 50 confirmed unreadable.
    let total = 200u64 * 2048;
    {
        let mut map = mapfile::Mapfile::create(&mf, total, "test").unwrap();
        map.record(0, 150 * 2048, mapfile::SectorStatus::Finished)
            .unwrap();
        map.record(150 * 2048, 50 * 2048, mapfile::SectorStatus::Unreadable)
            .unwrap();
        map.flush().unwrap();
    }

    let mut title = libfreemkv::DiscTitle::empty();
    title.duration_secs = 7200.0;
    title.size_bytes = total;
    title.extents = vec![libfreemkv::disc::Extent {
        start_lba: 0,
        sector_count: 200,
    }];

    let snap = progress_snapshot_from_mapfile(
        &mf,
        Some(&title),
        libfreemkv::progress::PassKind::Sweep,
        total,
    )
    .expect("a readable mapfile has a card to paint");

    assert_eq!(snap.bytes_good_total, 150 * 2048);
    assert_eq!(snap.bytes_unreadable_total, 50 * 2048);
    assert_eq!(
        snap.bytes_bad_in_main_title,
        50 * 2048,
        "the damage falls inside the title's only extent"
    );
    assert_eq!(snap.bytes_total_disc, total);
    assert_eq!(snap.main_title_size_bytes, Some(total));
    // A snapshot is a point in time, not a pass tick.
    assert_eq!((snap.work_done, snap.work_total), (0, 0));

    // And the absent case genuinely is None, so the assertion above is
    // about content and not merely about the file existing.
    assert!(
        progress_snapshot_from_mapfile(
            &dir.join("nope.map"),
            Some(&title),
            libfreemkv::progress::PassKind::Sweep,
            total,
        )
        .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
