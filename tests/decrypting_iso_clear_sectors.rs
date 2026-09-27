//! Whole-disc decryption must leave filesystem/navigation sectors outside content extents
//! unchanged, even when their leading bits resemble encrypted AACS units.
//! Decrypt refusals must retain their classification through the sweep pipeline.

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

    // Every sector: the clear region verbatim (decrypting a clear UDF sector
    // corrupts the filesystem even when it does not fail loud), every content
    // unit as its known plaintext.
    assert!(
        image == issue55_expected_image(),
        "the ISO must equal the decrypted image exactly"
    );
}

/// What a correct decrypting read of [`synthetic_aacs_disc`] yields.
fn issue55_expected_image() -> Vec<u8> {
    let mut img = Vec::new();
    for lba in 0..CONTENT_START {
        img.extend_from_slice(&filesystem_sector(lba));
    }
    for _ in 0..CONTENT_SECTORS / UNIT_SECTORS {
        img.extend_from_slice(&clear_content_unit());
    }
    img
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
    // The drive's own status and sense must survive, anchored to the batch LBA.
    match err {
        libfreemkv::error::Error::DiscRead {
            sector,
            status,
            sense,
        } => {
            assert_eq!(sector, POISONED_BLOCK_LBA as u64);
            assert_eq!(status, Some(0x02));
            assert_eq!(
                sense.map(|s| (s.sense_key, s.asc, s.ascq)),
                Some((0x03, 0x11, 0x00))
            );
        }
        other => panic!("expected DiscRead, got {other:?}"),
    }
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

// ── Whole-disc content gate over a real UDF tree (every /BDMV/STREAM file) ──

/// An in-memory disc image: `read_sectors` serves `image` verbatim.
struct MemDisc {
    image: Vec<u8>,
}

impl libfreemkv::sector::SectorSource for MemDisc {
    fn capacity_sectors(&self) -> u32 {
        (self.image.len() / SECTOR) as u32
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        let at = lba as usize * SECTOR;
        let n = count as usize * SECTOR;
        buf[..n].copy_from_slice(&self.image[at..at + n]);
        Ok(n)
    }
}

/// A BD tree with two stream files: `00001.m2ts` (the one kept title plays)
/// and `00002.m2ts` (played by no kept title, e.g. a sub-30s menu clip).
struct UdfFixture {
    /// The drive's bytes: title units and (optionally) one orphan unit encrypted.
    source: Vec<u8>,
    /// What a correct decrypting sweep writes: `source` with title units in the clear.
    expected: Vec<u8>,
    /// The title's 3-aligned extent inside `00001.m2ts`.
    title: (u32, u32),
    /// The aligned orphan unit's LBA inside `00002.m2ts`.
    orphan_unit: u32,
}

const FILE_SECTORS: usize = 30;

fn udf_fixture(orphan_encrypted: bool) -> UdfFixture {
    let dir = tempfile::tempdir().unwrap();
    let stream = dir.path().join("BDMV").join("STREAM");
    std::fs::create_dir_all(&stream).unwrap();
    for (name, tag) in [("00001.m2ts", 0xA1u8), ("00002.m2ts", 0xB2u8)] {
        let mut bytes = vec![0u8; FILE_SECTORS * SECTOR];
        bytes[100..116].fill(tag); // locator; byte 0 stays 0 (not CPI-flagged)
        std::fs::write(stream.join(name), bytes).unwrap();
    }
    let mut dirimage = libfreemkv::DirImage::open(dir.path()).unwrap();
    let cap = libfreemkv::sector::SectorSource::capacity_sectors(&dirimage);
    let mut source = vec![0u8; cap as usize * SECTOR];
    for lba in 0..cap {
        let at = lba as usize * SECTOR;
        libfreemkv::sector::SectorSource::read_sectors(
            &mut dirimage,
            lba,
            1,
            &mut source[at..at + SECTOR],
            false,
        )
        .unwrap();
    }
    let file_start = |tag: u8| -> u32 {
        (0..cap)
            .find(|&l| {
                let at = l as usize * SECTOR;
                source[at + 100..at + 116].iter().all(|&b| b == tag)
            })
            .unwrap() as u32
    };
    let first_aligned = |start: u32| start.div_ceil(UNIT_SECTORS) * UNIT_SECTORS;
    let title_start = first_aligned(file_start(0xA1));
    let title = (title_start, 8 * UNIT_SECTORS);
    let orphan_unit = first_aligned(file_start(0xB2)) + UNIT_SECTORS;

    let unit_len = libfreemkv::aacs::content::ALIGNED_UNIT_LEN;
    let mut expected = source.clone();
    for u in 0..title.1 / UNIT_SECTORS {
        let at = (title.0 + u * UNIT_SECTORS) as usize * SECTOR;
        source[at..at + unit_len].copy_from_slice(&encrypted_content_unit());
        expected[at..at + unit_len].copy_from_slice(&clear_content_unit());
    }
    if orphan_encrypted {
        let at = orphan_unit as usize * SECTOR;
        source[at..at + unit_len].copy_from_slice(&encrypted_content_unit());
        expected[at..at + unit_len].copy_from_slice(&encrypted_content_unit());
    }
    UdfFixture {
        source,
        expected,
        title,
        orphan_unit,
    }
}

fn udf_disc(fx: &UdfFixture) -> Disc {
    let cap = (fx.source.len() / SECTOR) as u32;
    let mut disc = synthetic_aacs_disc();
    disc.capacity_sectors = cap;
    disc.capacity_bytes = cap as u64 * SECTOR as u64;
    disc.titles[0].extents = vec![Extent {
        start_lba: fx.title.0,
        sector_count: fx.title.1,
    }];
    disc.titles[0].size_bytes = fx.title.1 as u64 * SECTOR as u64;
    disc
}

/// A clean BD: every sector — UDF metadata, both stream files, and every title
/// unit — must match the known-correct decrypted image byte for byte.
#[test]
fn a_whole_disc_sweep_writes_the_exact_decrypted_image() {
    let fx = udf_fixture(false);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("clean.iso");
    let mut reader = MemDisc {
        image: fx.source.clone(),
    };
    freemkv_engine::sweep(&udf_disc(&fx), &mut reader, &iso, &sweep_opts())
        .unwrap_or_else(|e| panic!("a clean disc must sweep, got {e}"));
    assert!(
        std::fs::read(&iso).unwrap() == fx.expected,
        "the ISO must equal the decrypted image exactly"
    );
}

/// An encrypted unit of a stream file no kept title plays has no key-map entry.
/// It must never land in a "decrypted" ISO as ciphertext at exit 0: the sweep
/// either decrypts it or fails loud (`DecryptFailed`, the orphan refusal).
#[test]
fn a_whole_disc_sweep_never_ships_a_non_title_stream_unit_as_ciphertext() {
    let fx = udf_fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("orphan.iso");
    let mut reader = MemDisc {
        image: fx.source.clone(),
    };
    let result = freemkv_engine::sweep(&udf_disc(&fx), &mut reader, &iso, &sweep_opts());
    let err = result.expect_err(
        "a non-title encrypted stream unit was written as ciphertext into a decrypted ISO",
    );
    assert_eq!(
        err.code(),
        libfreemkv::error::Error::DecryptFailed.code(),
        "the orphan unit must fail loud as a decrypt refusal: got {err}"
    );
}

/// Pre-seed `iso` with `image` and a mapfile marking everything Finished except
/// `bad` (NonTrimmed, zero-filled in the ISO) — the state Pass N retries.
fn prep_patch(iso: &std::path::Path, image: &[u8], bad: &[(u32, u32)]) {
    let total = image.len() as u64;
    let mut seeded = image.to_vec();
    let mut mf =
        freemkv_engine::Mapfile::create(&freemkv_engine::mapfile_path_for(iso), total, "test")
            .unwrap();
    mf.record(0, total, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    for &(lba, count) in bad {
        let (at, len) = (lba as usize * SECTOR, count as usize * SECTOR);
        seeded[at..at + len].fill(0);
        mf.record(
            at as u64,
            len as u64,
            freemkv_engine::SectorStatus::NonTrimmed,
        )
        .unwrap();
    }
    std::fs::write(iso, &seeded).unwrap();
}

fn patch_opts<'a>() -> freemkv_engine::PatchOptions<'a> {
    freemkv_engine::PatchOptions::for_patch_pass(true, None, None, None)
}

/// #55 for Pass N: a decrypting patch over a bad range holding the clear,
/// CPI-flagged-looking filesystem unit must restore it verbatim (content gate
/// installed alongside the key map), and decrypt the content range beside it.
#[test]
fn a_decrypting_patch_passes_clear_filesystem_sectors_through() {
    let expected = issue55_expected_image();
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch55.iso");
    prep_patch(
        &iso,
        &expected,
        &[(POISONED_BLOCK_LBA, 6), (CONTENT_START, 6)],
    );
    let disc = synthetic_aacs_disc();
    let mut reader = SyntheticDrive { fail_at: None };
    freemkv_engine::patch(&disc, &mut reader, &iso, &patch_opts())
        .unwrap_or_else(|e| panic!("a clean disc must patch, got {e}"));
    assert!(
        std::fs::read(&iso).unwrap() == expected,
        "the patched ISO must equal the decrypted image exactly"
    );
}

/// E1 for Pass N: re-reading a bad range over a non-title stream unit must not
/// record its ciphertext as a Finished, decrypted sector.
#[test]
fn a_decrypting_patch_never_ships_a_non_title_stream_unit_as_ciphertext() {
    let fx = udf_fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch-orphan.iso");
    prep_patch(&iso, &fx.expected, &[(fx.orphan_unit, UNIT_SECTORS)]);
    let mut reader = MemDisc {
        image: fx.source.clone(),
    };
    let outcome = freemkv_engine::patch(&udf_disc(&fx), &mut reader, &iso, &patch_opts());
    let map = freemkv_engine::Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
    let at = fx.orphan_unit as u64 * SECTOR as u64;
    let finished = map
        .ranges_with(&[freemkv_engine::SectorStatus::Finished])
        .iter()
        .any(|&(p, s)| p <= at && at < p + s);
    assert!(
        outcome.is_err() || !finished,
        "a non-title encrypted stream unit was patched in as ciphertext and marked Finished"
    );
}
