//! An image staged for an MKV rip reads only its scope (UDF, nav, the chosen titles) and
//! records it in the mapfile, so a disc with an unlocatable bus-encrypted stream file can
//! still be staged, while nothing takes the result for a whole-disc image.
//!
//! AACS BD Pre-recorded Book 0.953 §3.7: "the BEF shall be set to 0b for the sectors
//! that do not correspond to Clip AV stream files under \BDMV\STREAM directory", and its
//! Note: "PC Host shall decrypt bus-encrypted Clip AV stream file". Per spec; do not
//! change without a spec citation proving otherwise.

use std::sync::{Arc, Mutex};

use freemkv_engine::{CopyOptions, Mapfile, PatchOptions, SectorStatus, SweepOptions};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{E_BUS_STREAM_UNMAPPED, E_IMAGE_SCOPED, Error, Result};
use libfreemkv::sector::bus_removal::UnmappedStreamFile;
use libfreemkv::{ContentFormat, Disc, DiscFormat};

const SECTOR: usize = 2048;
const CAPACITY: u32 = 64;
const SCOPE: &[(u32, u32)] = &[(0, 8), (32, 8)];
const SPEC_BD_3_7_NOT_STREAM: &str = "AACS BD Pre-recorded Book 0.953 §3.7: \"the BEF shall \
    be set to 0b for the sectors that do not correspond to Clip AV stream files\"";
const SPEC_BD_3_7_NOTE: &str = "AACS BD Pre-recorded Book 0.953 §3.7 (Note): \"PC Host shall \
    decrypt bus-encrypted Clip AV stream file and hand it over to the application.\"";

// A drive stand-in: logs every LBA it serves (0x5A), reports `unmapped`.
struct Reader {
    unmapped: Vec<UnmappedStreamFile>,
    reads: Arc<Mutex<Vec<u32>>>,
}

impl libfreemkv::sector::SectorSource for Reader {
    fn capacity_sectors(&self) -> u32 {
        CAPACITY
    }
    fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], _r: bool) -> Result<usize> {
        self.reads.lock().unwrap().extend(lba..lba + count as u32);
        let n = count as usize * SECTOR;
        buf[..n].fill(0x5A);
        Ok(n)
    }
    fn unmapped_stream_files(&self) -> &[UnmappedStreamFile] {
        &self.unmapped
    }
}

fn reader(lost: bool) -> (Reader, Arc<Mutex<Vec<u32>>>) {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let cause = Error::UdfAdChainTooLong;
    let unmapped = if lost {
        vec![UnmappedStreamFile::new(
            "/BDMV/STREAM/00002.m2ts".into(),
            41,
            &cause,
        )]
    } else {
        Vec::new()
    };
    let r = Reader {
        unmapped,
        reads: reads.clone(),
    };
    (r, reads)
}

fn in_scope(lba: u32) -> bool {
    SCOPE.iter().any(|&(s, n)| lba >= s && lba < s + n)
}

fn disc() -> Disc {
    Disc {
        volume_id: "SCOPED".into(),
        meta_title: None,
        format: DiscFormat::Uhd,
        capacity_sectors: CAPACITY,
        capacity_bytes: CAPACITY as u64 * SECTOR as u64,
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

fn sweep_opts(resume: bool) -> SweepOptions<'static> {
    SweepOptions {
        decrypt: false,
        resume,
        batch_sectors: Some(4),
        skip_on_error: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
        keys: None,
    }
}

// Stage the scope with a lost clip; returns the image path, kept alive by the tempdir.
fn staged(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let iso = tmp.path().join("DISC.iso");
    let (mut r, _) = reader(true);
    freemkv_engine::sweep_scoped(&disc(), &mut r, &iso, &sweep_opts(false), SCOPE)
        .expect(SPEC_BD_3_7_NOT_STREAM);
    iso
}

#[test]
fn a_scoped_sweep_stages_despite_an_unmapped_stream_file_and_reads_only_its_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("DISC.iso");
    let (mut r, reads) = reader(true);
    let res = freemkv_engine::sweep_scoped(&disc(), &mut r, &iso, &sweep_opts(false), SCOPE)
        .expect(SPEC_BD_3_7_NOT_STREAM);
    let reads = reads.lock().unwrap().clone();
    assert!(
        reads.iter().all(|&l| in_scope(l)),
        "{SPEC_BD_3_7_NOTE}: {reads:?}"
    );
    assert_eq!(reads.len(), 16);
    assert!(res.complete, "the scope is the job: {res:?}");
    assert_eq!(res.bytes_pending, 0);
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
    assert_eq!(
        map.scope(),
        Some(&[(0, 8 * 2048), (32 * 2048, 8 * 2048)][..])
    );
    let img = std::fs::read(&iso).unwrap();
    assert!(
        img[8 * SECTOR..32 * SECTOR].iter().all(|&b| b == 0),
        "outside: never written"
    );
    assert!(img[..8 * SECTOR].iter().all(|&b| b == 0x5A));
}

#[test]
fn a_whole_disc_sweep_still_refuses_an_unmapped_stream_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut r, reads) = reader(true);
    let err = freemkv_engine::sweep(
        &disc(),
        &mut r,
        &tmp.path().join("x.iso"),
        &sweep_opts(false),
    )
    .expect_err(SPEC_BD_3_7_NOTE);
    assert_eq!(err.code(), E_BUS_STREAM_UNMAPPED);
    assert!(reads.lock().unwrap().is_empty());
}

#[test]
fn a_patch_over_a_scoped_map_reruns_only_in_scope_damage_without_the_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    {
        let mut map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
        map.record(2 * 2048, 2048, SectorStatus::NonTrimmed)
            .unwrap();
        map.record(20 * 2048, 2048, SectorStatus::NonTrimmed)
            .unwrap(); // outside the scope
        map.flush().unwrap();
    }
    let (mut r, reads) = reader(true);
    let popts = PatchOptions::for_patch_pass(false, None, None, None);
    let Ok(_) = freemkv_engine::patch(&disc(), &mut r, &iso, &popts) else {
        panic!("{SPEC_BD_3_7_NOT_STREAM}: a scoped patch needs no bus-map gate");
    };
    let reads = reads.lock().unwrap().clone();
    assert!(reads.contains(&2), "in-scope damage is retried");
    assert!(
        reads.iter().all(|&l| in_scope(l)),
        "{SPEC_BD_3_7_NOTE}: {reads:?}"
    );
}

#[test]
fn an_iso_resume_refuses_a_scoped_map_while_a_stream_file_is_unmapped() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    let before = std::fs::read(&iso).unwrap();
    let (mut r, reads) = reader(true);
    let err = freemkv_engine::copy(&disc(), &mut r, &iso, &CopyOptions::default())
        .expect_err(SPEC_BD_3_7_NOTE);
    assert_eq!(err.code(), E_BUS_STREAM_UNMAPPED);
    assert!(reads.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&iso).unwrap(), before);
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
    assert!(map.scope().is_some(), "still scoped");
}

#[test]
fn an_iso_resume_fills_the_rest_once_every_stream_file_is_located() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    let (mut r, reads) = reader(false);
    let opts = CopyOptions {
        decrypt: false,
        ..Default::default()
    };
    let res = freemkv_engine::copy(&disc(), &mut r, &iso, &opts).expect("fill");
    assert!(res.complete, "{res:?}");
    let reads = reads.lock().unwrap().clone();
    assert!(
        (8..32).chain(40..64).all(|l| reads.contains(&l)),
        "the rest is read"
    );
    assert!(
        reads.iter().all(|&l| !in_scope(l)),
        "the staged part is not re-read"
    );
    let map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
    assert_eq!(map.scope(), None);
    freemkv_engine::ensure_whole_image(&iso).expect("now a whole image");
}

#[test]
fn a_whole_disc_sweep_resume_over_a_scoped_map_fills_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    let (mut r, _) = reader(false);
    let res = freemkv_engine::sweep(&disc(), &mut r, &iso, &sweep_opts(true)).expect("fill");
    assert!(res.complete, "{res:?}");
    assert_eq!(
        std::fs::read(&iso).unwrap(),
        vec![0x5A; CAPACITY as usize * SECTOR]
    );
}

#[test]
fn ensure_whole_image_refuses_a_scoped_image_only() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    let err = freemkv_engine::ensure_whole_image(&iso).unwrap_err();
    assert_eq!(err.code(), E_IMAGE_SCOPED);
    assert_eq!(
        err.to_string(),
        format!("E{E_IMAGE_SCOPED}: {}", iso.display())
    );
    let plain = tmp.path().join("plain.iso");
    freemkv_engine::ensure_whole_image(&plain).expect("no mapfile: nothing says partial");
    let bad = tmp.path().join("bad.iso");
    std::fs::write(
        freemkv_engine::mapfile_path_for(&bad),
        "# freemkv-scope: zz\n0x0 ? 1\n0x0 0x800 ?\n",
    )
    .unwrap();
    assert!(
        freemkv_engine::ensure_whole_image(&bad).is_err(),
        "unreadable: refuse"
    );
}

fn mp_opts() -> freemkv_engine::MultipassOpts {
    freemkv_engine::MultipassOpts {
        max_passes: 2,
        abort_on_lost_secs: 0,
        is_iso_output: false,
    }
}

fn raw_job(iso: &std::path::Path) -> freemkv_engine::Job {
    let mut job = freemkv_engine::Job::new("disc://x", iso.display().to_string());
    job.raw = true;
    job
}

// The staged MKV flow: every pass stays inside the scope, so a lost clip does not stop it.
#[test]
fn multipass_rip_staged_reads_only_the_scope_on_a_disc_with_an_unmapped_stream_file() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("DISC.iso");
    let (mut r, reads) = reader(true);
    let sink = freemkv_engine::NoopSink;
    let res = freemkv_engine::multipass_rip_staged(
        &disc(),
        &mut r,
        &iso,
        &raw_job(&iso),
        &mp_opts(),
        Some(SCOPE),
        &sink,
    )
    .expect(SPEC_BD_3_7_NOT_STREAM);
    assert!(res.complete, "{res:?}");
    assert!(
        reads.lock().unwrap().iter().all(|&l| in_scope(l)),
        "{SPEC_BD_3_7_NOTE}"
    );
    let one = freemkv_engine::MultipassOpts {
        max_passes: 0,
        ..mp_opts()
    };
    let iso1 = tmp.path().join("ONE.iso");
    let (mut r, reads) = reader(true);
    let res = freemkv_engine::multipass_rip_staged(
        &disc(),
        &mut r,
        &iso1,
        &raw_job(&iso1),
        &one,
        Some(SCOPE),
        &sink,
    )
    .expect("single pass, scoped");
    assert!(res.complete, "{res:?}");
    assert!(reads.lock().unwrap().iter().all(|&l| in_scope(l)));
}

#[test]
fn an_unscoped_multipass_rip_still_refuses_an_unmapped_stream_file() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("DISC.iso");
    let (mut r, _) = reader(true);
    let sink = freemkv_engine::NoopSink;
    let err =
        freemkv_engine::multipass_rip(&disc(), &mut r, &iso, &raw_job(&iso), &mp_opts(), &sink)
            .expect_err(SPEC_BD_3_7_NOTE);
    assert_eq!(err.code(), E_BUS_STREAM_UNMAPPED);
}

// JUDGEMENT: a kept staging image must be whole, so it is only when every stream file
// was located; otherwise (and whenever it is not kept) the staging is scoped.
#[test]
fn mkv_staging_scope_is_whole_only_for_a_kept_image_of_a_fully_mapped_disc() {
    let scoped = |lost: bool, keep: bool| {
        let (mut r, _) = reader(lost);
        !matches!(
            freemkv_engine::mkv_staging_scope(&disc(), &mut r, &[], keep),
            Ok(None)
        )
    };
    assert!(!scoped(false, true), "kept + fully mapped: whole disc");
    assert!(
        scoped(true, true),
        "{SPEC_BD_3_7_NOTE}: kept but unmapped: scoped"
    );
    assert!(scoped(false, false), "not kept: scoped");
}

fn title(extents: &[(u32, u32)]) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents = extents
        .iter()
        .map(|&(start_lba, sector_count)| libfreemkv::Extent {
            start_lba,
            sector_count,
        })
        .collect();
    t
}

// A leftover staged image muxes only titles whose every sector was staged; checked by
// extents (a re-mapped staging mux passes), so no title is ever muxed from zero-fill.
#[test]
fn a_staged_image_muxes_only_titles_its_scope_holds() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = staged(&tmp);
    let mut d = disc();
    d.titles = vec![
        title(&[(32, 8), (0, 4)]), // wholly in scope
        title(&[(8, 4)]),          // outside it
        title(&[(6, 4)]),          // straddles its edge
    ];
    freemkv_engine::ensure_titles_staged(&iso, &d, &[0]).expect("an in-scope title passes");
    for sel in [&[1][..], &[2], &[0, 1]] {
        let err = freemkv_engine::ensure_titles_staged(&iso, &d, sel).unwrap_err();
        assert_eq!(err.code(), E_IMAGE_SCOPED, "{sel:?}");
    }
    let plain = tmp.path().join("plain.iso");
    freemkv_engine::ensure_titles_staged(&plain, &d, &[1]).expect("not a staged image");
}
