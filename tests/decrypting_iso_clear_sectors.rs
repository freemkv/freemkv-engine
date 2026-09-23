//! Regression suite for freemkv/freemkv#55 — "Can't rip to a decrypted ISO".
//!
//! A whole-disc decrypting sweep (`disc:// → iso://`) walks EVERY sector,
//! including the UDF filesystem / BDMV nav sectors that live outside every
//! title extent and are always clear. Those sectors must pass through the
//! decrypting reader untouched.
//!
//! Two things went wrong on the reporter's disc:
//!
//! 1. The sweep installed the AACS key map but NOT the encrypted-content
//!    extent map, so a clear filesystem unit whose first byte happens to have
//!    the AACS CPI bits set (`byte0 & 0xC0 != 0` — true for ~3 of every 4
//!    arbitrary bytes) was judged an un-keyable "orphan encrypted unit" and
//!    the whole read failed with `DecryptFailed`.
//! 2. The sweep's producer relabelled EVERY read error as
//!    `Error::DiscRead` (E6000, "the disc may be dirty or scratched"), so the
//!    decrypt failure was reported as a media fault at the batch's start LBA.
//!
//! Together they produced `E6000 Could not read the disc at sector 480` on a
//! disc that reads perfectly, and the identical error when decrypting an
//! already-ripped encrypted ISO (no drive involved at all).

use freemkv_engine::SweepOptions;
use libfreemkv::disc::{AacsState, DiscRegion, KeyOrigin};
use libfreemkv::{ContentFormat, Disc, DiscFormat, DiscTitle, Extent};

const SECTOR: usize = 2048;
const UNIT_SECTORS: u32 = 3; // 6144-byte AACS aligned unit

/// Sectors 0..CONTENT_START are the clear "filesystem" region — in NO title
/// extent, exactly like the UDF metadata around LBA 480 on the reporter's disc.
const CONTENT_START: u32 = 300;
const CONTENT_SECTORS: u32 = 300;
const CAPACITY: u32 = CONTENT_START + CONTENT_SECTORS;

/// A clear filesystem unit that *looks* AACS-flagged: its first byte has the
/// CPI bits set. Chosen so it lands in the third 60-sector batch, mirroring the
/// report (the failure surfaces at the batch's start LBA, not this one).
const POISONED_UNIT_LBA: u32 = 123;
const POISONED_BLOCK_LBA: u32 = 120;

const UNIT_KEY: [u8; 16] = [0x5A; 16];

/// The plaintext of every encrypted content unit: zeroes but for a TS sync byte
/// at offset 4 of each 192-byte BD-TS packet, plus the CPI bits on byte 0 (which
/// decryption never rewrites, so the unit still reads as "flagged encrypted").
fn clear_content_unit() -> Vec<u8> {
    let mut unit = vec![0u8; libfreemkv::aacs::content::ALIGNED_UNIT_LEN];
    let mut off = 4;
    while off < unit.len() {
        unit[off] = 0x47;
        off += 192;
    }
    unit[0] |= 0xC0;
    unit
}

fn encrypted_content_unit() -> Vec<u8> {
    let mut unit = clear_content_unit();
    assert!(
        libfreemkv::aacs::content::encrypt_unit(&mut unit, &UNIT_KEY),
        "a full-length unit must encrypt"
    );
    unit
}

/// One clear filesystem sector: deterministic filler, with the unit-start byte
/// carrying the CPI bits only for [`POISONED_UNIT_LBA`]. Every other unit start
/// gets a low byte (`0x01`), the common case for UDF descriptor tags.
fn filesystem_sector(lba: u32) -> Vec<u8> {
    let mut s = vec![0u8; SECTOR];
    for (i, b) in s.iter_mut().enumerate() {
        *b = (lba as u8).wrapping_mul(31).wrapping_add(i as u8);
    }
    if lba.is_multiple_of(UNIT_SECTORS) {
        s[0] = if lba == POISONED_UNIT_LBA { 0xC0 } else { 0x01 };
    }
    s
}

/// The byte image the drive presents: a clear filesystem region followed by a
/// genuinely AACS-encrypted title extent.
fn source_sector(lba: u32) -> Vec<u8> {
    if lba < CONTENT_START {
        return filesystem_sector(lba);
    }
    let off_in_extent = (lba - CONTENT_START) as usize;
    let unit = encrypted_content_unit();
    let within = (off_in_extent % UNIT_SECTORS as usize) * SECTOR;
    unit[within..within + SECTOR].to_vec()
}

struct SyntheticDrive {
    /// When set, the read starting at this LBA fails with this error. The LBA is
    /// in the CLEAR region, which key resolution never touches, so the failure
    /// lands inside the sweep's producer loop — the arm under test.
    fail_at: Option<(u32, fn() -> libfreemkv::error::Error)>,
}

impl libfreemkv::sector::SectorSource for SyntheticDrive {
    fn capacity_sectors(&self) -> u32 {
        CAPACITY
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        if let Some((at, make)) = self.fail_at
            && lba == at
        {
            return Err(make());
        }
        for i in 0..count as usize {
            let s = source_sector(lba + i as u32);
            buf[i * SECTOR..(i + 1) * SECTOR].copy_from_slice(&s);
        }
        Ok(count as usize * SECTOR)
    }
}

fn synthetic_aacs_disc() -> Disc {
    Disc {
        volume_id: "ISSUE55".into(),
        meta_title: Some("ISSUE55".into()),
        format: DiscFormat::BluRay,
        capacity_sectors: CAPACITY,
        capacity_bytes: CAPACITY as u64 * SECTOR as u64,
        layers: 1,
        titles: vec![DiscTitle {
            playlist: "00000.mpls".into(),
            playlist_id: 0,
            duration_secs: 1.0,
            size_bytes: CONTENT_SECTORS as u64 * SECTOR as u64,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![Extent {
                start_lba: CONTENT_START,
                sector_count: CONTENT_SECTORS,
            }],
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }],
        region: DiscRegion::Free,
        aacs: Some(AacsState {
            version: 1,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: String::new(),
            key_source: KeyOrigin::DeviceKey,
            vuk: None,
            unit_keys: vec![(CONTENT_START, UNIT_KEY)],
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        }),
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

fn sweep_opts<'a>() -> SweepOptions<'a> {
    SweepOptions {
        decrypt: true,
        resume: false,
        // 60 sectors: the shipping optical batch, and what the reporter's log
        // shows (`Drive::read enter lba=480 count=60`).
        batch_sectors: Some(60),
        skip_on_error: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    }
}

/// #55: the clear filesystem sectors outside every title extent must reach the
/// ISO byte-for-byte, and the sweep must not fail. Before the fix the sweep
/// installed the key map but not the content-extent gate, so the flagged-looking
/// clear unit at `POISONED_UNIT_LBA` was treated as an orphan encrypted unit and
/// the sweep died with E6000 at the batch start LBA.
#[test]
fn a_decrypting_sweep_passes_clear_filesystem_sectors_through() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("issue55.iso");
    let mut reader = SyntheticDrive { fail_at: None };
    let disc = synthetic_aacs_disc();

    let result = freemkv_engine::sweep(&disc, &mut reader, &iso, &sweep_opts())
        .unwrap_or_else(|e| panic!("a clean disc must sweep to a decrypted ISO, got {e}"));
    assert_eq!(
        result.bytes_good,
        CAPACITY as u64 * SECTOR as u64,
        "every sector must be recovered"
    );

    let image = std::fs::read(&iso).expect("the ISO must exist");
    assert_eq!(image.len(), CAPACITY as usize * SECTOR);

    // The clear region is verbatim — decrypting a clear UDF sector would corrupt
    // the filesystem even when it did not fail loud.
    for lba in [
        0u32,
        POISONED_UNIT_LBA,
        POISONED_UNIT_LBA + 1,
        CONTENT_START - 1,
    ] {
        let at = lba as usize * SECTOR;
        assert_eq!(
            &image[at..at + SECTOR],
            &filesystem_sector(lba)[..],
            "clear filesystem sector {lba} must pass through untouched"
        );
    }

    // The content region is decrypted.
    let at = CONTENT_START as usize * SECTOR;
    assert_eq!(
        &image[at..at + libfreemkv::aacs::content::ALIGNED_UNIT_LEN],
        &clear_content_unit()[..],
        "the encrypted content unit must come back as its known plaintext"
    );
}

/// #55, part two: a failure that is NOT a media read fault must keep its own
/// error code. The sweep used to rewrite every producer error into
/// `Error::DiscRead` (E6000, "the disc may be dirty or scratched"), which sent
/// the reporter off to clean a disc that reads fine and hid the real cause.
#[test]
fn a_non_read_failure_is_not_reported_as_a_disc_read_error() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("issue55-misclass.iso");
    // Stands in for the decrypting decorator above this reader failing a unit:
    // the producer sees a non-read error coming back from `read_sectors`.
    let mut reader = SyntheticDrive {
        fail_at: Some((POISONED_BLOCK_LBA, || {
            libfreemkv::error::Error::DecryptFailed
        })),
    };
    let disc = synthetic_aacs_disc();

    let err = freemkv_engine::sweep(&disc, &mut reader, &iso, &sweep_opts())
        .expect_err("a decrypt failure must abort the sweep");
    assert_eq!(
        err.code(),
        libfreemkv::error::Error::DecryptFailed.code(),
        "a decrypt failure must surface as itself, not as a disc-read fault: got {err}"
    );
    assert_ne!(
        err.code(),
        libfreemkv::error::E_DISC_READ,
        "E6000 blames the media; it must not swallow a decrypt failure"
    );
}

/// The genuine media fault still reports E6000 at the failing block's LBA —
/// the classification fix must not weaken the real read-error path.
#[test]
fn a_genuine_read_fault_still_reports_a_disc_read_error() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("issue55-read.iso");
    let mut reader = SyntheticDrive {
        fail_at: Some(
            (POISONED_BLOCK_LBA, || libfreemkv::error::Error::ScsiError {
                status: 0x02,
                sense: Some(libfreemkv::ScsiSense {
                    sense_key: 0x03,
                    asc: 0x11,
                    ascq: 0x00,
                }),
                opcode: 0x28,
            }),
        ),
    };
    let disc = synthetic_aacs_disc();

    let err = freemkv_engine::sweep(&disc, &mut reader, &iso, &sweep_opts())
        .expect_err("an unreadable sector must abort a non-skipping sweep");
    assert_eq!(
        err.code(),
        libfreemkv::error::E_DISC_READ,
        "a real SCSI read fault is still E6000: got {err}"
    );
}

/// Documents the exact string the reporter saw, so the misclassification cannot
/// come back wearing the same clothes: a `DiscRead` carrying SCSI status 0x00
/// and no sense data renders as `E6000: <lba> 0x00` — a "read error" the drive
/// never reported. Only a fault with real SCSI context may take that shape.
#[test]
fn the_reported_disc_read_error_carries_real_scsi_context() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("issue55-context.iso");
    // Stands in for the decrypting decorator above this reader failing a unit:
    // the producer sees a non-read error coming back from `read_sectors`.
    let mut reader = SyntheticDrive {
        fail_at: Some((POISONED_BLOCK_LBA, || {
            libfreemkv::error::Error::DecryptFailed
        })),
    };
    let disc = synthetic_aacs_disc();

    let err = freemkv_engine::sweep(&disc, &mut reader, &iso, &sweep_opts())
        .expect_err("a decrypt failure must abort the sweep");
    let shown = err.to_string();
    assert!(
        !shown.starts_with("E6000: "),
        "the user-facing string must not claim a disc read fault: {shown}"
    );
    assert!(
        !shown.contains(&format!("{POISONED_BLOCK_LBA} 0x00")),
        "a status-0x00 'read error' at a batch start LBA is the #55 signature: {shown}"
    );
}
