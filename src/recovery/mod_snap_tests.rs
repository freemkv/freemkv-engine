use super::snap_to_sectors;

// Both mapfile ingresses must widen a range to whole sectors — an
// unaligned `pos` reaching a sector-addressed reader truncates the LBA
// and shifts real payload, recorded afterwards as Finished.
#[test]
fn an_unaligned_range_widens_to_whole_sectors() {
    // Mid-sector start: anchor down, and cover the tail.
    assert_eq!(snap_to_sectors(512, 1024), (0, 2048));
    // Spanning a boundary: cover both sectors.
    assert_eq!(snap_to_sectors(2048 - 512, 1024), (0, 4096));
    // Already aligned: unchanged.
    assert_eq!(snap_to_sectors(4096, 2048), (4096, 2048));
    // Zero length keeps the anchored start and stays empty.
    assert_eq!(snap_to_sectors(700, 0), (0, 0));
}

// Rounding up must not wrap at the top of the address space: the old
// `(pos + len).div_ceil(SECTOR) * SECTOR` overflowed u64 there, wrapping
// to a fabricated ~2^64-byte span in release builds.
#[test]
fn rounding_up_saturates_at_the_end_of_the_address_space() {
    let (start, len) = snap_to_sectors(u64::MAX - 1023, 1024);
    assert_eq!(start % 2048, 0, "start stays sector-aligned");
    assert!(
        start.checked_add(len).is_some(),
        "snapped range wrapped past u64::MAX: start={start} len={len}"
    );
    // Length must ALWAYS be a whole number of sectors (every handler's `count
    // = len / SECTOR` depends on it). The true final sector runs past u64::MAX
    // and can't be represented, so 0 is the only whole-sector-respecting answer.
    assert_eq!(
        len % 2048,
        0,
        "returned length must be a whole number of sectors"
    );
    assert_eq!(len, 0, "the final sector here cannot be represented in u64");
}

/// The end cap is the LARGEST sector-aligned u64, and nothing below it may be clamped.
/// Mis-derive that constant (`/ SECTOR + SECTOR`, or a second division) and the ceiling
/// drops to ~9 PB, where every range above it comes back length 0 — a real region silently
/// "nothing to do" instead of read. The `u64::MAX`-adjacent test above answers 0 either way
/// and cannot see it.
#[test]
fn the_end_cap_clamps_nothing_a_u64_can_actually_hold() {
    // 1e16 is an exact multiple of 2048, ~9 PB past a mis-derived cap.
    let pos = 10_000_000_000_000_000u64;
    assert_eq!(
        snap_to_sectors(pos, 2048),
        (pos, 2048),
        "an aligned range far below the last sector boundary was clamped"
    );
    // And the cap itself is the last whole sector, not one short of it.
    let last_sector_start = (u64::MAX / 2048) * 2048 - 2048;
    assert_eq!(
        snap_to_sectors(last_sector_start, 2048),
        (last_sector_start, 2048)
    );
}

// The u128 overflow-proofing above must not change ordinary snapping —
// pinned independently so a mutation of the fix goes red here even if
// the edge-case test above stays green (proving decoupling).
#[test]
fn a_normal_range_is_unaffected_by_the_overflow_fix() {
    assert_eq!(snap_to_sectors(512, 1024), (0, 2048));
    assert_eq!(snap_to_sectors(1_000_000, 5_000), (999_424, 6144));
}
