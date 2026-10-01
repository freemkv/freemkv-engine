//! How a failed read ends (or does not end) a pass: only a genuine read fault
//! is disc damage; anything else aborts with its own error. Also the
//! zero-capacity guard on the public `copy` / `sweep` verbs.

use freemkv_engine::{CopyOptions, Mapfile, PatchOptions, SectorStatus, SweepOptions};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{E_DISC_READ, Error, Result};
use libfreemkv::{ContentFormat, Disc, DiscFormat, ScsiSense};

const SECTOR: usize = 2048;
const CAPACITY: u32 = 64;
const BATCH: u16 = 16;
const FAIL_LBA: u32 = 16;

enum Fault {
    Err(fn() -> Error),
    /// Report success but transfer one sector fewer than asked.
    Short,
}

struct FaultyReader {
    fault: Fault,
}

impl libfreemkv::sector::SectorSource for FaultyReader {
    fn capacity_sectors(&self) -> u32 {
        CAPACITY
    }
    fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], _r: bool) -> Result<usize> {
        let n = count as usize * SECTOR;
        let hit = lba <= FAIL_LBA && FAIL_LBA < lba + count as u32;
        buf[..n].fill(0xAA);
        match (&self.fault, hit) {
            (_, false) => Ok(n),
            (Fault::Err(make), true) => Err(make()),
            (Fault::Short, true) => Ok(n - SECTOR),
        }
    }
}

fn disc(capacity_sectors: u32) -> Disc {
    Disc {
        volume_id: "ABORT".into(),
        meta_title: None,
        format: DiscFormat::BluRay,
        capacity_sectors,
        capacity_bytes: capacity_sectors as u64 * SECTOR as u64,
        layers: 1,
        titles: Vec::new(),
        region: DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

fn opts(skip_on_error: bool) -> SweepOptions<'static> {
    SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: Some(BATCH),
        skip_on_error,
        progress: None,
        halt: None,
        keys: None,
    }
}

fn sweep_err(fault: Fault, skip_on_error: bool) -> (Error, std::path::PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("abort.iso");
    let mut reader = FaultyReader { fault };
    let err = freemkv_engine::sweep(&disc(CAPACITY), &mut reader, &iso, &opts(skip_on_error))
        .expect_err("the failing read must abort the sweep");
    (err, iso, tmp)
}

fn damage_bytes(iso: &std::path::Path) -> u64 {
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(iso)).unwrap();
    map.ranges_with(&[
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
    ])
    .iter()
    .map(|&(_, len)| len)
    .sum()
}

fn medium_error() -> Error {
    Error::DiscRead {
        sector: FAIL_LBA as u64 + 3,
        status: Some(0x02),
        sense: Some(ScsiSense {
            sense_key: 0x03,
            asc: 0x11,
            ascq: 0x05,
        }),
    }
}

// A refused short transfer carries NO SCSI status; relabelling it `status 0x00`
// is the freemkv#55 signature (a drive-reported "read error" that never was).
#[test]
fn a_short_transfer_keeps_its_absent_scsi_status() {
    let (err, _, _t) = sweep_err(Fault::Short, false);
    match err {
        Error::DiscRead {
            sector,
            status,
            sense,
        } => {
            assert_eq!(sector, FAIL_LBA as u64);
            assert_eq!(status, None, "no SCSI status was ever reported");
            assert_eq!(sense, None);
        }
        other => panic!("expected DiscRead, got {other:?}"),
    }
}

// A drive-reported DiscRead already names its sector, status and sense; the
// pass must not rewrite any of them.
#[test]
fn a_drive_disc_read_error_is_reported_unchanged() {
    let (err, _, _t) = sweep_err(Fault::Err(medium_error), false);
    match (err, medium_error()) {
        (
            Error::DiscRead {
                sector,
                status,
                sense,
            },
            Error::DiscRead {
                sector: s0,
                status: st0,
                sense: se0,
            },
        ) => assert_eq!((sector, status, sense), (s0, st0, se0)),
        (other, _) => panic!("expected DiscRead, got {other:?}"),
    }
}

// Skip-on-error must not zero-fill a decrypt refusal as "disc damage": the
// real error has to reach the user, and no range may be marked bad for it.
#[test]
fn a_skipping_sweep_aborts_on_a_non_read_error() {
    let (err, iso, _t) = sweep_err(Fault::Err(|| Error::DecryptFailed), true);
    assert_eq!(err.code(), Error::DecryptFailed.code(), "got {err}");
    assert_eq!(
        damage_bytes(&iso),
        0,
        "a decrypt refusal is not disc damage"
    );
}

// Dead-bus faults take the skip arm's AbortPass route and surface as E6000
// with the transport-failure status, anchored to the failing block.
#[test]
fn a_skipping_sweep_aborts_on_a_dead_bus() {
    let faults: [fn() -> Error; 2] = [
        || Error::IoError {
            source: std::io::Error::other("ENODEV"),
        },
        || Error::DeviceNotFound {
            path: "/dev/sr0".into(),
        },
    ];
    for fault in faults {
        let (err, iso, _t) = sweep_err(Fault::Err(fault), true);
        match err {
            Error::DiscRead {
                sector,
                status,
                sense,
            } => {
                assert_eq!(sector, FAIL_LBA as u64);
                assert_eq!(
                    status,
                    Some(libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE)
                );
                assert_eq!(sense, None);
            }
            other => panic!("expected DiscRead, got {other:?}"),
        }
        assert_eq!(damage_bytes(&iso), 0, "a dead bus is not a bad sector");
    }
}

// Run a patch pass over a 4-sector NonTrimmed range at FAIL_LBA with `fault` on the read path.
fn patch_with(
    fault: fn() -> Error,
) -> (
    Result<freemkv_engine::PatchOutcome>,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch.iso");
    let total = CAPACITY as u64 * SECTOR as u64;
    std::fs::File::create(&iso).unwrap().set_len(total).unwrap();
    let bad = (FAIL_LBA as u64 * SECTOR as u64, 4 * SECTOR as u64);
    {
        let mut mf =
            Mapfile::create(&freemkv_engine::mapfile_path_for(&iso), total, "test").unwrap();
        mf.record(0, total, SectorStatus::Finished).unwrap();
        mf.record(bad.0, bad.1, SectorStatus::NonTrimmed).unwrap();
        mf.flush().unwrap();
    }
    let mut reader = FaultyReader {
        fault: Fault::Err(fault),
    };
    let popts = PatchOptions::for_patch_pass(false, None, None);
    let r = freemkv_engine::patch(&disc(CAPACITY), &mut reader, &iso, &popts);
    (r, iso, tmp)
}

// The failing sector must stay NonTrimmed: never recorded Unreadable (lost)
// or Finished (never written), and nothing may be dropped.
fn assert_fail_sector_still_pending(iso: &std::path::Path) {
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(iso)).unwrap();
    let st = map.stats();
    assert_eq!(st.bytes_unreadable, 0, "a fault is not an unreadable range");
    assert_eq!(
        st.bytes_good + st.bytes_pending,
        CAPACITY as u64 * SECTOR as u64
    );
    let pos = FAIL_LBA as u64 * SECTOR as u64;
    assert!(
        map.ranges_with(&[SectorStatus::NonTrimmed])
            .iter()
            .any(|&(p, len)| p <= pos && pos < p + len),
        "the failing sector must remain NonTrimmed"
    );
}

// Pass N: a non-read error during recovery must abort the patch with its own
// code instead of leaving the range NonTrimmed as if it were unreadable.
#[test]
fn a_patch_pass_aborts_on_a_non_read_error() {
    let (r, iso, _t) = patch_with(|| Error::DecryptFailed);
    let Err(err) = r else {
        panic!("a decrypt refusal must abort the patch pass");
    };
    assert_eq!(err.code(), Error::DecryptFailed.code(), "got {err}");
    assert_ne!(err.code(), E_DISC_READ);
    assert_fail_sector_still_pending(&iso);
}

// A dead bus during patch must not mark the range bad either.
#[test]
fn a_patch_pass_never_marks_a_dead_bus_unreadable() {
    let faults: [fn() -> Error; 2] = [
        || Error::IoError {
            source: std::io::Error::other("ENODEV"),
        },
        || Error::DeviceNotFound {
            path: "/dev/sr0".into(),
        },
    ];
    for fault in faults {
        let (_r, iso, _t) = patch_with(fault);
        assert_fail_sector_still_pending(&iso);
    }
}

// A zero-capacity disc (READ CAPACITY swallowed to 0) must be refused before
// any output exists, rather than "completing" a 0-byte image.
#[test]
fn copy_and_sweep_refuse_a_zero_capacity_disc() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = disc(0);
    let mut reader = FaultyReader {
        fault: Fault::Short,
    };

    // A leftover zero-size mapfile must not change the verdict: the capacity
    // gate runs before the mapfile is read (a zero-size mapfile is itself
    // refused as invalid on load), so the answer stays EmptyImage.
    let iso = tmp.path().join("copy.iso");
    let map_path = freemkv_engine::mapfile_path_for(&iso);
    Mapfile::create(&map_path, 0, "test").unwrap();
    let copy_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let err = freemkv_engine::copy(&empty, &mut reader, &iso, &copy_opts)
        .expect_err("copy of a zero-capacity disc");
    assert!(matches!(err, Error::EmptyImage), "copy: got {err:?}");
    assert!(!iso.exists(), "copy must not create the image");

    let iso = tmp.path().join("sweep.iso");
    let err = freemkv_engine::sweep(&empty, &mut reader, &iso, &opts(true))
        .expect_err("sweep of a zero-capacity disc");
    assert!(matches!(err, Error::EmptyImage), "sweep: got {err:?}");
    assert!(!iso.exists(), "sweep must not create the image");
    assert!(!freemkv_engine::mapfile_path_for(&iso).exists());
}
