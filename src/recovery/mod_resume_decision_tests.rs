use super::*;
use std::io::ErrorKind;

fn err(kind: ErrorKind) -> std::io::Result<std::fs::Metadata> {
    Err(std::io::Error::from(kind))
}

// NotFound is the only error meaning "no file yet"; every other error is UNKNOWN and must
// not be answered "missing".
#[test]
fn only_not_found_means_the_image_is_missing() {
    assert_eq!(
        iso_len_from_metadata(err(ErrorKind::NotFound)).unwrap(),
        IsoLen::Missing
    );
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::Other,
        ErrorKind::TimedOut,
        ErrorKind::InvalidInput,
    ] {
        assert!(
            iso_len_from_metadata(err(kind)).is_err(),
            "{kind:?} is not evidence that the image is absent — it must \
                 abort, not fall through to a truncating create"
        );
    }
}

/// And a real length comes through as itself, including zero.
#[test]
fn a_readable_image_reports_its_own_length() {
    let dir = std::env::temp_dir().join(format!("fmkv-isolen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let empty = dir.join("empty.iso");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(
        iso_len_from_metadata(std::fs::metadata(&empty)).unwrap(),
        IsoLen::Len(0),
        "an existing zero-length image is NOT the same as an absent one"
    );

    let full = dir.join("full.iso");
    std::fs::write(&full, vec![0u8; 4096]).unwrap();
    assert_eq!(
        iso_len_from_metadata(std::fs::metadata(&full)).unwrap(),
        IsoLen::Len(4096)
    );
    assert_eq!(
        iso_len_from_metadata(std::fs::metadata(dir.join("nope.iso"))).unwrap(),
        IsoLen::Missing
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stale mapfile that will not delete must abort the fresh sweep.
#[test]
fn a_stale_mapfile_that_cannot_be_removed_aborts_the_fresh_sweep() {
    assert!(stale_mapfile_removed(Ok(())).is_ok());
    assert!(
        stale_mapfile_removed(Err(std::io::Error::from(ErrorKind::NotFound))).is_ok(),
        "already gone is the same as removed"
    );
    for kind in [ErrorKind::PermissionDenied, ErrorKind::Other] {
        assert!(
            stale_mapfile_removed(Err(std::io::Error::from(kind))).is_err(),
            "{kind:?}: proceeding would inherit the previous disc's Finished \
                 ranges and silently zero-fill the new ISO there"
        );
    }
}

/// The default batch size is a mode decision, not a constant.
#[test]
fn the_sweep_batch_defaults_by_mode_and_format() {
    use libfreemkv::DiscFormat::*;

    // skip-on-error (multipass Pass 1): one ECC block, so one skipped
    // batch loses exactly one ECC block.
    assert_eq!(sweep_batch_sectors(None, true, Uhd), 32);
    assert_eq!(sweep_batch_sectors(None, true, BluRay), 32);
    assert_eq!(sweep_batch_sectors(None, true, Dvd), 16);
    assert_eq!(sweep_batch_sectors(None, true, HdDvd), 16);

    // Clean sweep: the larger optical batch, regardless of format.
    assert_eq!(sweep_batch_sectors(None, false, Uhd), 60);
    assert_eq!(sweep_batch_sectors(None, false, Dvd), 60);

    // An explicit request wins in either mode.
    assert_eq!(sweep_batch_sectors(Some(7), true, Uhd), 7);
    assert_eq!(sweep_batch_sectors(Some(7), false, Uhd), 7);

    // Zero is clamped: a zero batch makes block_bytes zero, so `pos` never
    // advances and the producer spins forever.
    assert_eq!(sweep_batch_sectors(Some(0), true, Uhd), 1);
    assert_eq!(sweep_batch_sectors(Some(0), false, Dvd), 1);
}
