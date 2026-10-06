use crate::recovery::snap_to_sectors;

/// An already-aligned range is untouched.
#[test]
fn aligned_ranges_pass_through() {
    assert_eq!(snap_to_sectors(0, 2048), (0, 2048));
    assert_eq!(snap_to_sectors(4096, 8192), (4096, 8192));
}

/// A 512-block ddrescue range widens outward to whole sectors. Both edges
/// move, and the result always covers the original span.
#[test]
fn unaligned_ranges_widen_outward_and_cover_the_original() {
    for &(pos, len) in &[
        (512u64, 1024u64),
        (2048 + 512, 512),
        (100 * 2048 + 1, 3000),
        (1, 1),
    ] {
        let (p, l) = snap_to_sectors(pos, len);
        assert_eq!(p % 2048, 0, "start {p} not sector-aligned");
        assert_eq!(l % 2048, 0, "len {l} not a sector multiple");
        assert!(p <= pos, "widening must not lose the head");
        assert!(p + l >= pos + len, "widening must not lose the tail");
        assert!(l >= 2048, "a non-empty span must be at least one sector");
    }
}

/// A sub-sector span must never yield a zero-sector read: `count =
/// (span / SECTOR) as u16` would truncate to 0, and a zero-length read
/// reports Good, crediting a recovery that never happened.
#[test]
fn sub_sector_spans_never_truncate_to_zero_sectors() {
    let (_, l) = snap_to_sectors(1000, 48);
    assert!(l / 2048 >= 1, "span {l} truncates to a zero-sector read");
}
