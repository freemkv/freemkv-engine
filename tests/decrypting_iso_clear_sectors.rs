//! Whole-disc decryption (sweep / patch) over a real UDF tree: clear filesystem/nav
//! sectors pass through unchanged even when they look AACS-flagged (#55), every AACS
//! content file (`/BDMV/STREAM`, HD DVD `.EVO`) is decrypted on its OWN unit grid or
//! refused up front (never shipped as ciphertext), and decrypt refusals keep their
//! classification through the sweep pipeline. Each pass reads through the rip's up-front
//! key set (KU §2.1), resolved over the same drive before any output (KU-X1: the passes
//! take no disc-banked keys).

use freemkv_engine::{Mapfile, SectorStatus, SweepOptions};
use libfreemkv::aacs::types::UnitKey;
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::Error;
use libfreemkv::keys::{KeyScope, ResolvedKeySet};
use libfreemkv::keysource::ResolveCtx;
use libfreemkv::{ContentFormat, Disc, DiscFormat, DiscTitle, Extent};

const SECTOR: usize = 2048;
const UNIT_SECTORS: u32 = 3; // 6144-byte AACS aligned unit
const FILE_SECTORS: u32 = 30;

/// A clear sector in the UDF reserved area (in no content file) whose unit-start
/// byte carries the CPI bits — the #55 trigger. Its 60-sector batch starts at 120.
const POISONED_UNIT_LBA: u32 = 123;
const POISONED_BLOCK_LBA: u32 = 120;

const UNIT_KEY: [u8; 16] = [0x5A; 16];
const SECOND_KEY: [u8; 16] = [0x33; 16];
/// A key the disc's key pool does not hold: a file under it is unprovable.
const FOREIGN_KEY: [u8; 16] = [0x77; 16];

/// The plaintext of every encrypted content unit: zeroes but for a TS sync byte
/// at offset 4 of each 192-byte BD-TS packet, plus CPI 11₂ on packet 0, flagged
/// before `encrypt_unit`. Decryption may clear that CPI (KU §5.4), so a decrypted
/// image is compared CPI-masked ([`cpi_masked_eq`]).
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

fn encrypted_content_unit(key: &[u8; 16]) -> Vec<u8> {
    let mut unit = clear_content_unit();
    assert!(libfreemkv::aacs::content::encrypt_unit(&mut unit, key));
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
    /// Any read COVERING this LBA fails as a medium error (a bad sector).
    bad_lba: Option<u32>,
    /// One-unit (3-sector) reads starting in `[start, end)` fail: probes of a
    /// damaged area, while the pass's longer reads still get through.
    probe_fail: Option<(u32, u32)>,
}

impl MemDisc {
    fn new(image: &[u8]) -> Self {
        MemDisc {
            image: image.to_vec(),
            fail_at: None,
            bad_lba: None,
            probe_fail: None,
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
        let probe_hit = self
            .probe_fail
            .is_some_and(|(s, e)| count as u32 == UNIT_SECTORS && (s..e).contains(&lba));
        if probe_hit
            || self
                .bad_lba
                .is_some_and(|bad| (lba..lba + count as u32).contains(&bad))
        {
            return Err(Error::ScsiError {
                status: 0x02,
                sense: Some(libfreemkv::ScsiSense {
                    sense_key: 0x03,
                    asc: 0x11,
                    ascq: 0x00,
                }),
                opcode: 0x28,
            });
        }
        let at = lba as usize * SECTOR;
        let n = count as usize * SECTOR;
        buf[..n].copy_from_slice(&self.image[at..at + n]);
        Ok(n)
    }
}

/// A synthesized UDF disc: the drive's bytes, what a correct decrypting read
/// yields, and each tree file's `(start_lba, sectors)` in the order given.
struct Fixture {
    source: Vec<u8>,
    expected: Vec<u8>,
    files: Vec<(u32, u32)>,
    /// Start LBAs of the units a correct read yields as plaintext.
    decrypted_units: Vec<u32>,
}

impl Fixture {
    /// Encrypt every unit of `file` under `key`, on the FILE's own unit grid.
    /// `decrypts`: a correct read yields plaintext (else the ciphertext stays).
    fn encrypt(&mut self, file: usize, key: &[u8; 16], decrypts: bool) {
        let units = self.files[file].1 / UNIT_SECTORS;
        self.encrypt_units(file, key, decrypts, 0..units);
    }

    /// [`Self::encrypt`] for only `units` of the file (the rest stay clear).
    fn encrypt_units(
        &mut self,
        file: usize,
        key: &[u8; 16],
        decrypts: bool,
        units: std::ops::Range<u32>,
    ) {
        let start = self.files[file].0;
        let unit_len = libfreemkv::aacs::content::ALIGNED_UNIT_LEN;
        for u in units {
            let at = (start + u * UNIT_SECTORS) as usize * SECTOR;
            let enc = encrypted_content_unit(key);
            self.source[at..at + unit_len].copy_from_slice(&enc);
            let out = if decrypts { clear_content_unit() } else { enc };
            self.expected[at..at + unit_len].copy_from_slice(&out);
            if decrypts {
                self.decrypted_units.push(start + u * UNIT_SECTORS);
            }
        }
    }
}

/// `got == fx.expected` except the CPI bits of each decrypted unit's source packets.
/// Per spec; do not change without a spec citation proving otherwise.
fn cpi_masked_eq(fx: &Fixture, got: &[u8]) -> bool {
    // KS-6 AACS BD §3.10.2 Table 3-34: "TP_extra_header { Copy_permission_indicator 2
    // uimsbf Arrival_time_stamp 30 uimsbf }": mask only the 2 CPI bits (byte0 & 0x3F).
    let mask = |img: &[u8]| {
        let mut img = img.to_vec();
        for &lba in &fx.decrypted_units {
            let at = lba as usize * SECTOR;
            let end = (at + libfreemkv::aacs::content::ALIGNED_UNIT_LEN).min(img.len());
            for off in (at..end).step_by(192) {
                img[off] &= 0x3F;
            }
        }
        img
    };
    got.len() == fx.expected.len() && mask(got) == mask(&fx.expected)
}

/// Lay `paths` (each `FILE_SECTORS` long, 1 sector for `*.bdmv`) out as a UDF image.
fn tree(paths: &[&str]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    for (i, p) in paths.iter().enumerate() {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let sectors = if p.ends_with(".bdmv") {
            1
        } else {
            FILE_SECTORS
        };
        let mut bytes = vec![0u8; sectors as usize * SECTOR];
        bytes[100..116].fill(0xA0 + i as u8); // byte 0 stays 0 (not CPI-flagged)
        std::fs::write(path, bytes).unwrap();
    }
    let mut img = libfreemkv::DirImage::open(dir.path()).unwrap();
    let fs = libfreemkv::read_filesystem(&mut img).unwrap();
    let files = paths
        .iter()
        .map(|p| fs.file_extents(&mut img, &format!("/{p}")).unwrap()[0])
        .collect();
    let cap = libfreemkv::sector::SectorSource::capacity_sectors(&img);
    let mut source = vec![0u8; cap as usize * SECTOR];
    for lba in 0..cap {
        let at = lba as usize * SECTOR;
        libfreemkv::sector::SectorSource::read_sectors(
            &mut img,
            lba,
            1,
            &mut source[at..at + SECTOR],
            false,
        )
        .unwrap();
    }
    // #55: clear, unused reserved-area sectors, the unit start CPI-flagged.
    let p = POISONED_UNIT_LBA as usize * SECTOR;
    assert!(source[p..p + 3 * SECTOR].iter().all(|&b| b == 0));
    for (i, b) in source[p..p + 3 * SECTOR].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31) | 1;
    }
    source[p] = 0xC0;
    Fixture {
        expected: source.clone(),
        source,
        files,
        decrypted_units: Vec::new(),
    }
}

const TITLE: usize = 0;
const ORPHAN: usize = 1;

/// A BD whose kept title plays `00001.m2ts`; `00002.m2ts` is played by no kept
/// title (a sub-30 s logo/menu clip). The title file is encrypted under `UNIT_KEY`;
/// `orphan` encrypts the second file under that key (`Some(false)` = unprovable).
fn bd(orphan: Option<(&[u8; 16], bool)>) -> Fixture {
    let mut fx = tree(&["BDMV/STREAM/00001.m2ts", "BDMV/STREAM/00002.m2ts"]);
    fx.encrypt(TITLE, &UNIT_KEY, true);
    if let Some((key, decrypts)) = orphan {
        fx.encrypt(ORPHAN, key, decrypts);
    }
    fx
}

/// A scanned single-CPS AACS 1.0 disc over `fx`: one kept title playing file 0.
fn disc(fx: &Fixture) -> Disc {
    let cap = (fx.source.len() / SECTOR) as u32;
    let (start, sectors) = fx.files[TITLE];
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
            size_bytes: sectors as u64 * SECTOR as u64,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![Extent {
                start_lba: start,
                sector_count: sectors,
            }],
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }],
        region: DiscRegion::Free,
        aacs: Some(
            libfreemkv::test_util::aacs_state()
                .uk_ro(unit_key_ro(1))
                .build(),
        ),
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

/// The same disc, but with two CPS units (keys `UNIT_KEY`, `SECOND_KEY`).
fn multi_cps_disc(fx: &Fixture) -> Disc {
    let mut d = disc(fx);
    let aacs = d.aacs.as_mut().unwrap();
    aacs.uk_ro = unit_key_ro(2);
    d
}

/// A key source answering with the fixture disc's key pool (one key per declared CPS unit).
struct Pool(Vec<(u32, [u8; 16])>);

impl libfreemkv::KeySource for Pool {
    fn get_unit_keys(&self, _ctx: &dyn ResolveCtx) -> libfreemkv::Result<Vec<UnitKey>> {
        Ok(self.0.iter().map(|&(i, k)| UnitKey::new(i, k)).collect())
    }
    fn label(&self) -> &'static str {
        "keydb"
    }
}

/// The rip's up-front key set for `d` from its pool, resolved over `reader` (the same
/// drive the pass reads) with scope `WholeDisc`, as a decrypted-image rip does.
fn keyed(d: &Disc, reader: &mut MemDisc) -> libfreemkv::Result<ResolvedKeySet> {
    let two = d.aacs.as_ref().is_some_and(|a| a.uk_ro == unit_key_ro(2));
    let pool = if two {
        vec![(1, UNIT_KEY), (2, SECOND_KEY)]
    } else {
        vec![(1, UNIT_KEY)]
    };
    let f: libfreemkv::KeySourceFactory = std::sync::Arc::new(move || {
        vec![Box::new(Pool(pool.clone())) as Box<dyn libfreemkv::KeySource>]
    });
    ResolvedKeySet::resolve(d, reader, KeyScope::WholeDisc, &f, Default::default()).map(|r| r.keys)
}

fn sweep_opts<'a>(keys: ResolvedKeySet) -> SweepOptions<'a> {
    SweepOptions {
        decrypt: true,
        resume: false,
        // 60 sectors: the shipping optical batch, and what the #55 log shows.
        batch_sectors: Some(60),
        skip_on_error: false,
        progress: None,
        halt: None,
        keys: Some(keys),
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
    let r =
        keyed(disc, reader).and_then(|k| freemkv_engine::sweep(disc, reader, &iso, &sweep_opts(k)));
    (iso, r)
}

/// Sweep `d` over `fx` and require EXACTLY the decrypted image.
fn assert_sweeps_to_expected(fx: &Fixture, d: &Disc) {
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, d, &mut MemDisc::new(&fx.source));
    let result = r.unwrap_or_else(|e| panic!("the disc must sweep, got {e}"));
    assert_eq!(result.bytes_good, fx.source.len() as u64);
    assert!(
        cpi_masked_eq(fx, &std::fs::read(&iso).unwrap()),
        "the ISO must equal the decrypted image exactly (CPI-masked)"
    );
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

/// The mask hides only the CPI bits of decrypted units' packets: a cleared CPI still
/// matches, but an ATS bit, a payload byte, or the #55 clear unit's CPI does not.
/// Per spec; do not change without a spec citation proving otherwise.
#[test]
fn the_cpi_mask_ignores_only_the_cpi_bits_of_decrypted_units() {
    let fx = bd(None);
    let unit = fx.decrypted_units[3] as usize * SECTOR;
    assert_eq!(
        fx.decrypted_units.len(),
        (FILE_SECTORS / UNIT_SECTORS) as usize
    );
    // KS-5 AACS BD §3.10.2: "… or shall be set to 00₂ if the data is not encrypted".
    let mut cleared = fx.expected.clone();
    cleared[unit] &= 0x3F;
    cleared[unit + 31 * 192] |= 0xC0;
    assert!(
        cpi_masked_eq(&fx, &cleared),
        "CPI-only differences are masked"
    );
    let poisoned = POISONED_UNIT_LBA as usize * SECTOR;
    let (orphan, pkt) = (fx.files[ORPHAN].0 as usize * SECTOR, unit + 192);
    for (at, bit) in [
        (pkt, 0x20),
        (pkt + 1, 0x01),
        (pkt + 4, 0x01),
        (poisoned, 0xC0),
    ] {
        let mut other = fx.expected.clone();
        other[at] ^= bit;
        assert!(
            !cpi_masked_eq(&fx, &other),
            "byte {at} bit {bit:#x} must count"
        );
    }
    let mut clear_file = fx.expected.clone();
    clear_file[orphan] |= 0xC0;
    assert!(
        !cpi_masked_eq(&fx, &clear_file),
        "a unit left clear stays exact"
    );
    assert!(
        !cpi_masked_eq(&fx, &fx.expected[..SECTOR]),
        "length must count"
    );
}

/// #55 + E6: a clean disc sweeps to EXACTLY the decrypted image — the clear
/// flagged-looking reserved unit, all UDF metadata, both stream files, every unit.
/// The title file starts at an LBA that is NOT a multiple of 3 (DirImage lays
/// data from LBA 4096): its units must be decrypted on the file's own grid.
#[test]
fn a_decrypting_sweep_passes_clear_filesystem_sectors_through() {
    let fx = bd(None);
    assert_ne!(
        fx.files[TITLE].0 % UNIT_SECTORS,
        0,
        "fixture: misaligned file"
    );
    assert_sweeps_to_expected(&fx, &disc(&fx));
}

/// Single-CPS disc: a stream file no kept title plays is keyed and decrypted.
#[test]
fn a_single_cps_sweep_decrypts_a_non_title_stream_file() {
    let fx = bd(Some((&UNIT_KEY, true)));
    assert_sweeps_to_expected(&fx, &disc(&fx));
}

/// Multi-CPS: the non-title file's key is resolved for that file alone and
/// proven against its own ciphertext, so it decrypts rather than refusing.
#[test]
fn a_multi_cps_sweep_decrypts_a_provable_non_title_stream_file() {
    for key in [&UNIT_KEY, &SECOND_KEY] {
        let fx = bd(Some((key, true)));
        assert_sweeps_to_expected(&fx, &multi_cps_disc(&fx));
    }
}

/// A non-title file no held key opens is unprovable: refused before any output with
/// E7032 (MKV rip or raw copy), on the plain sweep, the multipass shapes
/// (skip_on_error sweep, `copy`), and a disc whose `Unit_Key_RO.inf` is missing.
#[test]
fn an_unprovable_non_title_stream_file_refuses_up_front() {
    let fx = bd(Some((&FOREIGN_KEY, false)));
    let mut no_ukro = disc(&fx);
    no_ukro.aacs.as_mut().unwrap().uk_ro = Vec::new();
    let mut fmts = disc(&fx);
    fmts.format = DiscFormat::Fmts;
    for d in [multi_cps_disc(&fx), disc(&fx), no_ukro, fmts] {
        let tmp = tempfile::tempdir().unwrap();
        let (iso, r) = sweep_to(&tmp, &d, &mut MemDisc::new(&fx.source));
        assert_refused_before_output(&iso, r, Error::WholeDiscKeyMissing.code());

        let iso = tmp.path().join("skip.iso");
        let mut reader = MemDisc::new(&fx.source);
        let r = keyed(&d, &mut reader).and_then(|k| {
            let opts = SweepOptions {
                skip_on_error: true,
                ..sweep_opts(k)
            };
            freemkv_engine::sweep(&d, &mut reader, &iso, &opts)
        });
        assert_refused_before_output(&iso, r, Error::WholeDiscKeyMissing.code());

        let iso = tmp.path().join("copy.iso");
        let mut reader = MemDisc::new(&fx.source);
        let r = keyed(&d, &mut reader).and_then(|k| {
            let opts = freemkv_engine::CopyOptions {
                decrypt: true,
                multipass: true,
                keys: Some(k),
                ..Default::default()
            };
            freemkv_engine::copy(&d, &mut reader, &iso, &opts)
        });
        assert_refused_before_output(&iso, r, Error::WholeDiscKeyMissing.code());
    }
}

/// Multi-CPS, the unplayed file encrypted only at its ends (the resolver's evenly
/// spaced samples all land on clear units): the probes prove the other held key.
#[test]
fn a_multi_cps_sweep_proves_a_file_from_its_first_and_last_units() {
    let mut fx = bd(None);
    fx.encrypt_units(ORPHAN, &SECOND_KEY, true, 0..1);
    fx.encrypt_units(ORPHAN, &SECOND_KEY, true, 9..10);
    assert_sweeps_to_expected(&fx, &multi_cps_disc(&fx));
}

/// An FMTS disc still tries its other held BASE keys, so the same file keys.
#[test]
fn an_fmts_sweep_proves_an_unplayed_file_with_another_base_key() {
    let mut fx = bd(None);
    fx.encrypt_units(ORPHAN, &SECOND_KEY, true, 0..1);
    fx.encrypt_units(ORPHAN, &SECOND_KEY, true, 9..10);
    let mut d = multi_cps_disc(&fx);
    d.format = DiscFormat::Fmts;
    assert_sweeps_to_expected(&fx, &d);
}

/// One probe is not proof for a key the resolver did not pick: a single chance
/// TS sync pass must not key a whole file. The file stays unkeyed, so the pass
/// stops at its encrypted unit with E7032 rather than trusting one sample.
#[test]
#[ignore = "KU-X1: legacy-reader contract; the key set proves this piece on arrival (J21) and decrypts it"]
fn an_alternate_key_opening_one_probe_is_not_trusted() {
    let mut fx = bd(None);
    fx.encrypt_units(ORPHAN, &SECOND_KEY, true, 0..1);
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &multi_cps_disc(&fx), &mut MemDisc::new(&fx.source));
    let err = r.expect_err("one opened probe must not key the file");
    assert_eq!(err.code(), Error::WholeDiscKeyMissing.code(), "got {err}");
    assert!(
        iso.exists(),
        "not refused up front: one probe is no evidence either way"
    );
}

/// The same file under a key no held key matches is refused BEFORE the copy
/// starts, not hours into the pass when the walk reaches it.
#[test]
#[ignore = "KU-X1: legacy-reader contract; the key set refuses this at arrival (E7032), after the ISO opens"]
fn a_multi_cps_sweep_refuses_a_first_unit_no_key_opens_before_output() {
    let mut fx = bd(None);
    fx.encrypt_units(ORPHAN, &FOREIGN_KEY, false, 0..1);
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &multi_cps_disc(&fx), &mut MemDisc::new(&fx.source));
    assert_refused_before_output(&iso, r, Error::WholeDiscKeyMissing.code());
}

/// Last resort: every probe of the unplayed file is unreadable (damage), so its key
/// cannot be proven up front. The pass stops at its first encrypted unit with the
/// same E7032, never writing ciphertext and never as a generic decrypt failure.
#[test]
fn an_unreadable_probe_stops_the_pass_with_the_mkv_or_raw_error() {
    let fx = bd(Some((&FOREIGN_KEY, false)));
    let (o, n) = fx.files[ORPHAN];
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = MemDisc::new(&fx.source);
    reader.probe_fail = Some((o, o + n));
    let (iso, r) = sweep_to(&tmp, &multi_cps_disc(&fx), &mut reader);
    let err = r.expect_err("an encrypted unit with no proven key must stop the pass");
    assert_eq!(err.code(), Error::WholeDiscKeyMissing.code(), "got {err}");
    assert!(
        iso.exists(),
        "the pass started: this is the mid-pass last resort"
    );
}

/// A skipping sweep's batches tile each file's unit grid: a bad sector in the unit
/// straddling a 32-sector batch edge fails ONE batch, not both neighbours.
#[test]
fn a_skipping_sweep_fails_only_the_batch_holding_the_bad_unit() {
    let fx = bd(Some((&UNIT_KEY, true)));
    let (o, _) = fx.files[ORPHAN];
    assert_eq!(
        o % 32,
        30,
        "fixture: the orphan file starts 2 sectors before a batch edge"
    );
    let bad = o + 1; // its first unit [o, o+3) straddles the edge at o+2
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("skip.iso");
    let mut reader = MemDisc::new(&fx.source);
    reader.bad_lba = Some(bad);
    let opts = SweepOptions {
        skip_on_error: true,
        batch_sectors: Some(32),
        ..sweep_opts(keyed(&disc(&fx), &mut reader).unwrap())
    };
    let r = freemkv_engine::sweep(&disc(&fx), &mut reader, &iso, &opts)
        .unwrap_or_else(|e| panic!("a skipping sweep must finish, got {e}"));
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
    let bad_ranges = map.ranges_with(&[
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
    ]);
    let first_bad = bad_ranges.first().map(|&(p, _)| p / SECTOR as u64);
    assert_eq!(
        first_bad,
        Some(o as u64),
        "the batch BEFORE the bad unit must read clean: {bad_ranges:?}"
    );
    assert!(r.bytes_pending + r.bytes_unreadable > 0);
}

/// HD DVD: an `.EVO` no kept title plays is AACS content like a BD stream file —
/// keyed, decrypted on its own grid, never passed through as ciphertext.
#[test]
fn an_hd_dvd_sweep_decrypts_a_non_title_evo() {
    let mut fx = tree(&[
        "BDMV/index.bdmv",
        "HVDVD_TS/FEATURE_1.EVO",
        "HVDVD_TS/FEATURE_2.EVO",
    ]);
    fx.files.remove(0);
    fx.encrypt(TITLE, &UNIT_KEY, true);
    fx.encrypt(ORPHAN, &UNIT_KEY, true);
    let mut d = disc(&fx);
    d.format = DiscFormat::HdDvd;
    assert_sweeps_to_expected(&fx, &d);
}

/// An unreadable content-file map on an AACS disc fails loud, not narrowing the
/// gate to the title extents (which would pass non-title units as ciphertext).
#[test]
fn an_unreadable_stream_map_fails_loud() {
    let fx = bd(Some((&UNIT_KEY, true)));
    let tmp = tempfile::tempdir().unwrap();
    let mut reader = MemDisc::new(&fx.source);
    reader.fail_at = Some((256, || Error::DecryptFailed)); // the UDF anchor
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut reader);
    assert_refused_before_output(&iso, r, Error::DecryptFailed.code());

    // No UDF at all: the error surfaces as itself.
    let blank = vec![0u8; fx.source.len()];
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &disc(&fx), &mut MemDisc::new(&blank));
    assert_refused_before_output(&iso, r, Error::UdfNotFilesystem.code());
}

/// A UDF tree with no AACS content file while titles exist is inconsistent:
/// refuse (E6003, naming the missing folder) rather than trust the title extents.
#[test]
#[ignore = "KU-X1: legacy-reader contract; the key set's whole-disc reader has no E6003 check (reported)"]
fn an_empty_stream_map_with_titles_fails_loud() {
    let fx = bd(None);
    let empty = tree(&["BDMV/index.bdmv"]);
    let mut image = empty.source.clone();
    image.resize(fx.source.len().max(image.len()), 0);
    let mut d = disc(&fx);
    d.capacity_sectors = (image.len() / SECTOR) as u32;
    d.capacity_bytes = image.len() as u64;
    let tmp = tempfile::tempdir().unwrap();
    let (iso, r) = sweep_to(&tmp, &d, &mut MemDisc::new(&image));
    assert_refused_before_output(&iso, r, libfreemkv::error::E_UDF_NOT_FOUND);
}

/// #55, part two: a failure that is NOT a media read fault keeps its own code
/// (the sweep used to relabel every producer error as E6000 "dirty disc").
#[test]
fn a_non_read_failure_is_not_reported_as_a_disc_read_error() {
    let fx = bd(None);
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
    let fx = bd(None);
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

fn patch_opts<'a>(keys: ResolvedKeySet) -> freemkv_engine::PatchOptions<'a> {
    freemkv_engine::PatchOptions {
        keys: Some(keys),
        ..freemkv_engine::PatchOptions::for_patch_pass(true, None, None)
    }
}

/// Patch `bad` ranges of `d` over `fx` and require EXACTLY the decrypted image.
fn assert_patches_to_expected(fx: &Fixture, d: &Disc, bad: &[(u32, u32)]) {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch.iso");
    prep_patch(&iso, &fx.expected, bad);
    let mut reader = MemDisc::new(&fx.source);
    let out = keyed(d, &mut reader)
        .and_then(|k| freemkv_engine::patch(d, &mut reader, &iso, &patch_opts(k)))
        .unwrap_or_else(|e| panic!("the disc must patch, got {e}"));
    assert_eq!(out.bytes_pending, 0, "every bad range must be recovered");
    assert!(
        cpi_masked_eq(fx, &std::fs::read(&iso).unwrap()),
        "the patched ISO must equal the decrypted image exactly (CPI-masked)"
    );
}

/// E5 (#55 for Pass N): bad ranges over the clear flagged-looking unit and over
/// the misaligned title file (single sectors mid-unit) yield the decrypted image.
#[test]
fn a_decrypting_patch_passes_clear_filesystem_sectors_through() {
    let fx = bd(None);
    let t = fx.files[TITLE].0;
    assert_patches_to_expected(
        &fx,
        &disc(&fx),
        &[(POISONED_BLOCK_LBA, 6), (t + 1, 1), (t + 5, 4)],
    );
}

/// Pass N over non-title stream files: single-CPS and provable multi-CPS files
/// are patched in as plaintext.
#[test]
fn a_patch_decrypts_a_non_title_stream_file() {
    let fx = bd(Some((&SECOND_KEY, true)));
    let o = fx.files[ORPHAN].0;
    assert_patches_to_expected(&fx, &multi_cps_disc(&fx), &[(o + 2, 5)]);
    let fx = bd(Some((&UNIT_KEY, true)));
    assert_patches_to_expected(&fx, &disc(&fx), &[(o + 2, 5)]);
}

/// Unprovable non-title file on Pass N: refused with E7032 before
/// touching the ISO or the mapfile — the bad range stays NonTrimmed.
#[test]
fn an_unprovable_patch_refuses_up_front() {
    let fx = bd(Some((&FOREIGN_KEY, false)));
    let o = fx.files[ORPHAN].0;
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch-multi.iso");
    prep_patch(&iso, &fx.expected, &[(o, UNIT_SECTORS)]);
    let iso_before = std::fs::read(&iso).unwrap();
    let map_path = freemkv_engine::mapfile_path_for(&iso);
    let map_before = std::fs::read(&map_path).unwrap();
    let d = multi_cps_disc(&fx);
    let mut reader = MemDisc::new(&fx.source);
    let r = keyed(&d, &mut reader)
        .and_then(|k| freemkv_engine::patch(&d, &mut reader, &iso, &patch_opts(k)));
    let Err(err) = r else {
        panic!("an unprovable non-title file must refuse");
    };
    assert_eq!(err.code(), Error::WholeDiscKeyMissing.code(), "got {err}");
    assert!(std::fs::read(&iso).unwrap() == iso_before, "ISO untouched");
    assert_eq!(std::fs::read(&map_path).unwrap(), map_before);
    let at = o as u64 * SECTOR as u64;
    assert_eq!(
        Mapfile::load(&map_path)
            .unwrap()
            .next_with(0, SectorStatus::NonTrimmed),
        Some((at, UNIT_SECTORS as u64 * SECTOR as u64)),
        "the orphan unit stays NonTrimmed"
    );
}
