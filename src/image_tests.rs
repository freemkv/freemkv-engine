use super::*;
use crate::test_fixtures::{K1, bd_image};

#[test]
fn parse_error_code_splits_code_and_data() {
    assert_eq!(parse_error_code("E6009"), Some((6009, "")));
    assert_eq!(parse_error_code("E7022: abcdef"), Some((7022, "abcdef")));
    assert_eq!(parse_error_code("E5000: 13"), Some((5000, "13")));
    assert_eq!(
        parse_error_code("E99999"),
        None,
        "no libfreemkv code is wider than u16"
    );
    assert_eq!(parse_error_code("No drive found"), None);
    assert_eq!(parse_error_code("E"), None);
    assert_eq!(parse_error_code("Eabc"), None);
}

// Agrees with the library's own reading of every error it renders, and of any text.
#[test]
fn error_code_matches_the_library() {
    for e in [
        libfreemkv::Error::MkvInvalid,
        libfreemkv::Error::Halted,
        libfreemkv::Error::MkvSourceInvalid,
    ] {
        let code = e.code();
        let io: std::io::Error = e.into();
        assert_eq!(error_code(&io), Some(code));
        assert_eq!(error_code(&io), libfreemkv::error_code(&io));
    }
    assert_eq!(error_code(&std::io::Error::other("disk full")), None);
    for s in ["E99999", "E65536: x", "E65535", "E7022: a", "E", "E-1", "x"] {
        let io = std::io::Error::other(s);
        assert_eq!(error_code(&io), libfreemkv::error_code(&io), "{s}");
        assert_eq!(parse_error_code(s).map(|(c, _)| c), error_code(&io), "{s}");
    }
}

#[test]
fn image_source_urls_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let d = ImageSource::from_path(dir.path());
    assert!(matches!(d, ImageSource::Dir(_)));
    assert_eq!(ImageSource::from_url(&d.url()), Some(d.clone()));
    let iso = ImageSource::from_path(dir.path().join("x.iso"));
    assert!(matches!(iso, ImageSource::Iso(_)));
    assert_eq!(ImageSource::from_url(&iso.url()), Some(iso));
    assert_eq!(ImageSource::from_url("mkv:///tmp/a.mkv"), None);
}

#[test]
fn open_image_reports_a_missing_image_as_a_scan_error() {
    let src = ImageSource::Iso("/nonexistent/freemkv/none.iso".into());
    let err = open_image(&src, &KeyParams::default())
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(&err, Error::IoError { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
        "{err:?}"
    );
}

// Counts reads of the UDF anchor (LBA 256); reads touching `fail` fail with EIO.
struct Probe<S> {
    inner: S,
    anchor_reads: usize,
    fail: Option<(u32, u32)>,
}

impl<S: libfreemkv::SectorSource> libfreemkv::SectorSource for Probe<S> {
    fn capacity_sectors(&self) -> u32 {
        self.inner.capacity_sectors()
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> libfreemkv::Result<usize> {
        let end = lba + count as u32;
        if (lba..end).contains(&256) {
            self.anchor_reads += 1;
        }
        if self.fail.is_some_and(|(s, e)| s < end && lba < e) {
            return Err(Error::from(std::io::Error::from_raw_os_error(5)));
        }
        self.inner.read_sectors(lba, count, buf, recovery)
    }
}

// An OS error reading the image's key file is that I/O error, not "a different disc".
#[test]
fn a_key_file_read_error_is_an_io_error_not_a_disc_mismatch() {
    let fx = bd_image(&[Some(K1)], 1);
    let inf = *fx.metadata.last().unwrap();
    let mut r = Probe {
        inner: fx.source(),
        anchor_reads: 0,
        fail: Some((inf.0, inf.0 + inf.1)),
    };
    let err = check_prescanned(&fx.disc, &mut r, None, None, &[]).unwrap_err();
    assert!(
        matches!(&err, Error::IoError { source, .. } if source.raw_os_error() == Some(5)),
        "{err:?}"
    );
}

// With a sidecar, the pre-scanned check parses the image's UDF once.
#[test]
fn a_prescanned_check_parses_the_udf_once() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let total = fx.img.image.len() as u64;
    let mut map = Mapfile::create(&dir.path().join("d.mapfile"), total, "t").unwrap();
    map.record(0, total, crate::SectorStatus::Finished).unwrap();
    let mut r = Probe {
        inner: fx.source(),
        anchor_reads: 0,
        fail: None,
    };
    check_prescanned(&fx.disc, &mut r, Some(&map), None, &[]).unwrap();
    assert_eq!(r.anchor_reads, 1);
}

// Every refusal a top-up keeps comes back with its own code and text; Missing and a
// Stop are never kept.
#[test]
fn a_kept_refusal_rebuilds_to_itself() {
    let kept = [
        Error::KeyServiceUnavailable,
        Error::KeyServiceUnauthorized,
        Error::KeyServiceRateLimited,
        Error::AacsVidNeedsDisc,
        Error::FmtsKeyMissing,
        Error::DecryptFailed,
        Error::KeydbConnect {
            host: "h.test".into(),
        },
        Error::KeydbHttp { status: 403 },
        Error::KeydbInvalid,
        Error::KeydbWrite { path: "/k".into() },
        Error::KeydbParse,
        Error::KeydbLoad { path: "/k".into() },
        Error::KeydbUnsupportedScheme {
            scheme: "ftp".into(),
        },
        Error::KeydbTooManyRedirects,
        Error::from(std::io::Error::from_raw_os_error(28)),
        Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
    ];
    for e in kept {
        let back = Remembered::of(&e).expect("kept").rebuild();
        assert_eq!((back.code(), back.to_string()), (e.code(), e.to_string()));
    }
    let missing = Error::NoDiscKey {
        disc_hash: String::new(),
    };
    assert!(Remembered::of(&missing).is_none());
    assert!(Remembered::of(&Error::WholeDiscKeyMissing).is_none());
    assert!(Remembered::of(&Error::Halted).is_none());
}

// A kept OS error keeps its kind and errno: `RipOutcome::Failed.kind` reads them.
#[test]
fn a_kept_io_refusal_keeps_its_os_error() {
    let errors = [
        std::io::Error::from_raw_os_error(28),
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    ];
    for io in errors {
        let want = (io.kind(), io.raw_os_error());
        let back: std::io::Error = Remembered::of(&Error::from(io)).unwrap().rebuild().into();
        assert_eq!((back.kind(), back.raw_os_error()), want);
    }
}
