//! Whole-disc decryption (sweep / patch) over a real UDF tree: clear filesystem/nav
//! sectors pass through unchanged even when they look AACS-flagged (#55), every
//! `/BDMV/STREAM` unit is decrypted or refused up front (never shipped as ciphertext),
//! and decrypt refusals keep their classification through the sweep pipeline.

use freemkv_engine::{Mapfile, SectorStatus, SweepOptions};
use libfreemkv::disc::{AacsState, DiscRegion, KeyOrigin};
use libfreemkv::error::Error;
use libfreemkv::{ContentFormat, Disc, DiscFormat, DiscTitle, Extent};

const SECTOR: usize = 2048;
const UNIT_SECTORS: u32 = 3; // 6144-byte AACS aligned unit
const FILE_SECTORS: usize = 30;
const TITLE_UNITS: u32 = 8;

/// A clear sector in the UDF reserved area (in no stream file) whose unit-start
/// byte carries the CPI bits — the #55 trigger. Its 60-sector batch starts at 120.
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

/// A minimal AACS 1.0 `Unit_Key_RO.inf` declaring `cps_units` CPS units.
fn unit_key_ro(cps_units: u16) -> Vec<u8> {
    let uk_pos = 48usize;
    let mut v = vec![0u8; uk_pos + 48 + 48 * cps_units as usize];
    v[..4].copy_from_slice(&(uk_pos as u32).to_be_bytes());
    v[16] = 1; // BD-ROM
    v[17] = 1;
    v[uk_pos..uk_pos + 2].copy_from_slice(&cps_units.to_be_bytes());
    v
}

/// An in-memory drive serving `image`; optionally one read fails.
struct MemDisc {
    image: Vec<u8>,
    /// The read STARTING at this LBA fails with this error.
    fail_at: Option<(u32, fn() -> Error)>,
}

impl MemDisc {
    fn new(image: &[u8]) -> Self {
        MemDisc {
            image: image.to_vec(),
            fail_at: None,
        }
    }
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
        if let Some((at, make)) = self.fail_at
            && lba == at
        {
            return Err(make());
        }
        let at = lba as usize * SECTOR;
        let n = count as usize * SECTOR;
        buf[..n].copy_from_slice(&self.image[at..at + n]);
        Ok(n)
    }
}

/// A BD tree with two stream files: `00001.m2ts` (the one kept title plays) and
/// `00002.m2ts` (played by no kept title, e.g. a sub-30s logo/menu clip).
struct Fixture {
    /// The drive's bytes: title units (and optionally one orphan unit) encrypted.
    source: Vec<u8>,
    /// A correct decrypting read: `source` with every encrypted unit in the clear.
    expected: Vec<u8>,
    /// The title's 3-aligned extent inside `00001.m2ts`.
    title: (u32, u32),
    /// The aligned orphan unit's LBA inside `00002.m2ts`.
    orphan_unit: u32,
}

fn fixture(orphan_encrypted: bool) -> Fixture {
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
            .unwrap()
    };
    let first_aligned = |start: u32| start.div_ceil(UNIT_SECTORS) * UNIT_SECTORS;
    let title = (first_aligned(file_start(0xA1)), TITLE_UNITS * UNIT_SECTORS);
    let orphan_unit = first_aligned(file_start(0xB2)) + UNIT_SECTORS;

    // #55: clear, unused reserved-area sectors, the unit start CPI-flagged.
    let p = POISONED_UNIT_LBA as usize * SECTOR;
    assert!(
        source[p..p + 3 * SECTOR].iter().all(|&b| b == 0),
        "the poisoned unit must sit in unused space"
    );
    for (i, b) in source[p..p + 3 * SECTOR].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31) | 1;
    }
    source[p] = 0xC0;

    let unit_len = libfreemkv::aacs::content::ALIGNED_UNIT_LEN;
    let mut expected = source.clone();
    let mut units: Vec<u32> = (0..TITLE_UNITS)
        .map(|u| title.0 + u * UNIT_SECTORS)
        .collect();
    if orphan_encrypted {
        units.push(orphan_unit);
    }
    for lba in units {
        let at = lba as usize * SECTOR;
        source[at..at + unit_len].copy_from_slice(&encrypted_content_unit());
        expected[at..at + unit_len].copy_from_slice(&clear_content_unit());
    }
    Fixture {
        source,
        expected,
        title,
        orphan_unit,
    }
}

/// A scanned single-CPS AACS 1.0 BD over `fx`: one kept title (`00001.m2ts`).
fn disc(fx: &Fixture) -> Disc {
    let cap = (fx.source.len() / SECTOR) as u32;
    Disc {
        volume_id: "ISSUE55".into(),
        meta_title: Some("ISSUE55".into()),
        format: DiscFormat::BluRay,
        capacity_sectors: cap,
        capacity_bytes: cap as u64 * SECTOR as u64,
        layers: 1,
        titles: vec![DiscTitle {
            playlist: "00000.mpls".into(),
            playlist_id: 0,
            duration_secs: 1.0,
            size_bytes: fx.title.1 as u64 * SECTOR as u64,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![Extent {
                start_lba: fx.title.0,
                sector_count: fx.title.1,
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
            unit_keys: vec![(1, UNIT_KEY)],
            volume_id: [0u8; 16],
            uk_ro: unit_key_ro(1),
            mkb: Vec::new(),
        }),
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

/// The same disc, but its `Unit_Key_RO.inf` declares two CPS units.
fn multi_cps_disc(fx: &Fixture) -> Disc {
    let mut d = disc(fx);
    let aacs = d.aacs.as_mut().unwrap();
    aacs.uk_ro = unit_key_ro(2);
    aacs.unit_keys.push((2, [0x33; 16]));
    d
}

fn sweep_opts<'a>() -> SweepOptions<'a> {
    SweepOptions {
        decrypt: true,
        resume: false,
        // 60 sectors: the shipping optical batch, and what the #55 log shows.
        batch_sectors: Some(60),
        skip_on_error: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    }
}

fn sweep_to(
    dir: &tempfile::TempDir,
    disc: &Disc,
    reader: &mut MemDisc,
) -> (
    std::path::PathBuf,
    libfreemkv::error::Result<freemkv_engine::CopyResult>,
) {
    let iso = dir.path().join("out.iso");
    let r = freemkv_engine::sweep(disc, reader, &iso, &sweep_opts());
    (iso, r)
}

fn assert_refused_before_output(
    iso: &std::path::Path,
    r: libfreemkv::error::Result<freemkv_engine::CopyResult>,
    code: u16,
) {
    let err = r.expect_err("the sweep must refuse");
    assert_eq!(err.code(), code, "wrong refusal: {err}");
    assert!(!iso.exists(), "refusal must precede creating the ISO");
    assert!(
        !freemkv_engine::mapfile_path_for(iso).exists(),
        "refusal must precede creating the mapfile"
    );
}

/// #55 + E6: a clean disc sweeps to EXACTLY the decrypted image — the clear
/// flagged-looking reserved unit, all UDF metadata, both stream files, every unit.
#[test]
fn a_decrypting_sweep_passes_clear_filesystem_sectors_through() {
    let fx = fixture(false);
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut MemDisc::new(&fx.source));
    let result = r.unwrap_or_else(|e| panic!("a clean disc must sweep, got {e}"));
    assert_eq!(result.bytes_good, fx.source.len() as u64);
    assert!(
        std::fs::read(&iso).unwrap() == fx.expected,
        "the ISO must equal the decrypted image exactly"
    );
}

/// Single-CPS disc: an encrypted unit of a stream file no kept title plays is
/// keyed by the disc's only unit key and lands in the ISO as plaintext.
#[test]
fn a_single_cps_sweep_decrypts_a_non_title_stream_unit() {
    let fx = fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut MemDisc::new(&fx.source));
    r.unwrap_or_else(|e| panic!("a single-CPS disc must sweep, got {e}"));
    let image = std::fs::read(&iso).unwrap();
    let at = fx.orphan_unit as usize * SECTOR;
    assert_eq!(
        &image[at..at + libfreemkv::aacs::content::ALIGNED_UNIT_LEN],
        &clear_content_unit()[..],
        "the non-title stream unit must be decrypted, not shipped as ciphertext"
    );
    assert!(
        image == fx.expected,
        "the ISO must equal the decrypted image"
    );
}

/// Multi-CPS: which key opens a non-title stream file is unknown, so refuse
/// before any output exists rather than failing (or shipping ciphertext) mid-rip.
#[test]
fn a_multi_cps_sweep_with_non_title_streams_refuses_up_front() {
    let fx = fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &multi_cps_disc(&fx), &mut MemDisc::new(&fx.source));
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());
}

/// The multipass shape (skip_on_error sweep, and `copy` with `multipass`) refuses
/// the same way up front — never a pass that aborts, or records damage, mid-disc.
#[test]
fn a_multi_cps_multipass_rip_with_non_title_streams_refuses_up_front() {
    let fx = fixture(true);
    let d = multi_cps_disc(&fx);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("skip.iso");
    let opts = SweepOptions {
        skip_on_error: true,
        ..sweep_opts()
    };
    let r = freemkv_engine::sweep(&d, &mut MemDisc::new(&fx.source), &iso, &opts);
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());

    let iso = tmp.path().join("copy.iso");
    let opts = freemkv_engine::CopyOptions {
        decrypt: true,
        multipass: true,
        ..Default::default()
    };
    let r = freemkv_engine::copy(&d, &mut MemDisc::new(&fx.source), &iso, &opts);
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());
}

/// FMTS (AACS 2.1 forensic) and an unparseable `Unit_Key_RO.inf` cannot prove a
/// single CPS unit either: refused up front the same way.
#[test]
fn an_unprovable_single_cps_disc_refuses_up_front() {
    let fx = fixture(false);
    let mut fmts = disc(&fx);
    fmts.format = DiscFormat::Fmts;
    let mut no_ukro = disc(&fx);
    no_ukro.aacs.as_mut().unwrap().uk_ro = Vec::new();
    for d in [fmts, no_ukro] {
        let tmp = tempfile::tempdir().unwrap();
        let (iso, r) = sweep_to(&tmp, &d, &mut MemDisc::new(&fx.source));
        assert_refused_before_output(&iso, r, Error::DecryptFailed.code());
    }
}

/// An unreadable stream map on a BD must fail loud, not narrow the gate to the
/// title extents (which would pass non-title stream units through as ciphertext).
#[test]
fn an_unreadable_stream_map_fails_loud() {
    let fx = fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = MemDisc::new(&fx.source);
    reader.fail_at = Some((256, || Error::DecryptFailed)); // the UDF anchor
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut reader);
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());

    // No UDF at all on a BD-format disc: the error surfaces as itself.
    let blank = vec![0u8; fx.source.len()];
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut MemDisc::new(&blank));
    assert_refused_before_output(&iso, r, Error::UdfNotFilesystem.code());
}

/// A BD whose UDF lists no `/BDMV/STREAM` file while titles exist is inconsistent:
/// refuse rather than trusting the title extents alone.
#[test]
fn an_empty_stream_map_with_titles_fails_loud() {
    let fx = fixture(false);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("BDMV")).unwrap();
    std::fs::write(dir.path().join("BDMV").join("index.bdmv"), [0u8; 64]).unwrap();
    let mut img = libfreemkv::DirImage::open(dir.path()).unwrap();
    let cap = libfreemkv::sector::SectorSource::capacity_sectors(&img) as usize;
    let mut image = vec![0u8; fx.source.len().max(cap * SECTOR)];
    for lba in 0..cap {
        let at = lba * SECTOR;
        libfreemkv::sector::SectorSource::read_sectors(
            &mut img,
            lba as u32,
            1,
            &mut image[at..at + SECTOR],
            false,
        )
        .unwrap();
    }
    let mut d = disc(&fx);
    d.capacity_sectors = (image.len() / SECTOR) as u32;
    d.capacity_bytes = image.len() as u64;
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &d, &mut MemDisc::new(&image));
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());
}

/// #55, part two: a failure that is NOT a media read fault keeps its own code
/// (the sweep used to relabel every producer error as E6000 "dirty disc").
#[test]
fn a_non_read_failure_is_not_reported_as_a_disc_read_error() {
    let fx = fixture(false);
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = MemDisc::new(&fx.source);
    reader.fail_at = Some((POISONED_BLOCK_LBA, || Error::DecryptFailed));
    let (_, r) = sweep_to(&tmp, &disc(&fx), &mut reader);
    let err = r.expect_err("a decrypt failure must abort the sweep");
    assert_eq!(err.code(), Error::DecryptFailed.code(), "got {err}");
    assert_ne!(err.code(), libfreemkv::error::E_DISC_READ);
    let shown = err.to_string();
    assert!(!shown.starts_with("E6000: "), "not a disc read: {shown}");
    assert!(
        !shown.contains(&format!("{POISONED_BLOCK_LBA} 0x00")),
        "a status-0x00 'read error' at a batch start LBA is the #55 signature: {shown}"
    );
}

/// The genuine media fault still reports E6000 — the classification fix must
/// not weaken the real read-error path.
#[test]
fn a_genuine_read_fault_still_reports_a_disc_read_error() {
    let fx = fixture(false);
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = MemDisc::new(&fx.source);
    reader.fail_at = Some((POISONED_BLOCK_LBA, || Error::ScsiError {
        status: 0x02,
        sense: Some(libfreemkv::ScsiSense {
            sense_key: 0x03,
            asc: 0x11,
            ascq: 0x00,
        }),
        opcode: 0x28,
    }));
    let (_, r) = sweep_to(&tmp, &disc(&fx), &mut reader);
    let err = r.expect_err("an unreadable sector must abort a non-skipping sweep");
    assert_eq!(err.code(), libfreemkv::error::E_DISC_READ, "got {err}");
    // The drive's own status and sense must survive, anchored to the batch LBA.
    match err {
        Error::DiscRead {
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

// ── Pass N (patch) ──

/// Pre-seed `iso` with `image` and a mapfile marking everything Finished except
/// `bad` (NonTrimmed, zero-filled in the ISO) — the state Pass N retries.
fn prep_patch(iso: &std::path::Path, image: &[u8], bad: &[(u32, u32)]) {
    let total = image.len() as u64;
    let mut seeded = image.to_vec();
    let mut mf = Mapfile::create(&freemkv_engine::mapfile_path_for(iso), total, "test").unwrap();
    mf.record(0, total, SectorStatus::Finished).unwrap();
    for &(lba, count) in bad {
        let (at, len) = (lba as usize * SECTOR, count as usize * SECTOR);
        seeded[at..at + len].fill(0);
        mf.record(at as u64, len as u64, SectorStatus::NonTrimmed)
            .unwrap();
    }
    std::fs::write(iso, &seeded).unwrap();
}

fn patch_opts<'a>() -> freemkv_engine::PatchOptions<'a> {
    freemkv_engine::PatchOptions::for_patch_pass(true, None, None, None)
}

/// E5 (#55 for Pass N): patching a bad range over the clear flagged-looking unit
/// and one over title content yields EXACTLY the decrypted image.
#[test]
fn a_decrypting_patch_passes_clear_filesystem_sectors_through() {
    let fx = fixture(false);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch55.iso");
    prep_patch(
        &iso,
        &fx.expected,
        &[(POISONED_BLOCK_LBA, 6), (fx.title.0, 6)],
    );
    freemkv_engine::patch(
        &disc(&fx),
        &mut MemDisc::new(&fx.source),
        &iso,
        &patch_opts(),
    )
    .unwrap_or_else(|e| panic!("a clean disc must patch, got {e}"));
    assert!(
        std::fs::read(&iso).unwrap() == fx.expected,
        "the patched ISO must equal the decrypted image exactly"
    );
}

/// Single-CPS Pass N over a non-title stream unit patches in its plaintext.
#[test]
fn a_single_cps_patch_decrypts_a_non_title_stream_unit() {
    let fx = fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch-orphan.iso");
    prep_patch(&iso, &fx.expected, &[(fx.orphan_unit, UNIT_SECTORS)]);
    let out = freemkv_engine::patch(
        &disc(&fx),
        &mut MemDisc::new(&fx.source),
        &iso,
        &patch_opts(),
    )
    .unwrap_or_else(|e| panic!("a single-CPS disc must patch, got {e}"));
    assert_eq!(out.bytes_pending, 0, "the orphan unit must be recovered");
    assert!(
        std::fs::read(&iso).unwrap() == fx.expected,
        "the patched ISO must hold the orphan unit's plaintext"
    );
}

/// Multi-CPS Pass N: refused with `DecryptFailed` before touching the ISO or the
/// mapfile — the bad range stays NonTrimmed, nothing is recorded Finished.
#[test]
fn a_multi_cps_patch_with_non_title_streams_refuses_up_front() {
    let fx = fixture(true);
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch-multi.iso");
    prep_patch(&iso, &fx.expected, &[(fx.orphan_unit, UNIT_SECTORS)]);
    let iso_before = std::fs::read(&iso).unwrap();
    let map_path = freemkv_engine::mapfile_path_for(&iso);
    let map_before = std::fs::read(&map_path).unwrap();
    let Err(err) = freemkv_engine::patch(
        &multi_cps_disc(&fx),
        &mut MemDisc::new(&fx.source),
        &iso,
        &patch_opts(),
    ) else {
        panic!("a multi-CPS disc with non-title streams must refuse");
    };
    assert_eq!(err.code(), Error::DecryptFailed.code(), "got {err}");
    assert!(std::fs::read(&iso).unwrap() == iso_before, "ISO untouched");
    assert_eq!(
        std::fs::read(&map_path).unwrap(),
        map_before,
        "mapfile untouched"
    );
    let at = fx.orphan_unit as u64 * SECTOR as u64;
    assert_eq!(
        Mapfile::load(&map_path)
            .unwrap()
            .next_with(0, SectorStatus::NonTrimmed),
        Some((at, UNIT_SECTORS as u64 * SECTOR as u64)),
        "the orphan unit stays NonTrimmed"
    );
}
