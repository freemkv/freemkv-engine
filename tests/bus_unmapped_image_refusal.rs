//! A whole-disc image (copy / sweep / patch) is refused, before any output, when the
//! drive's host-key bus map could not locate a bus-encrypted stream file.
//!
//! AACS BD Pre-recorded Book 0.953 §3.7 (Note): "PC Host shall decrypt bus-encrypted
//! Clip AV stream file and hand it over to the application." A file whose sectors
//! cannot be located cannot be de-bussed, so an image would carry them still
//! bus-encrypted as if plaintext. Per spec; do not change without a spec citation
//! proving otherwise.

use freemkv_engine::{CopyOptions, Mapfile, PatchOptions, SectorStatus, SweepOptions};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{E_BUS_STREAM_UNMAPPED, Error, Result};
use libfreemkv::sector::bus_removal::UnmappedStreamFile;
use libfreemkv::{ContentFormat, Disc, DiscFormat};

const SECTOR: usize = 2048;
const CAPACITY: u32 = 64;
const SPEC_BD_3_7_NOTE: &str = "AACS BD Pre-recorded Book 0.953 §3.7 (Note): \"PC Host shall \
    decrypt bus-encrypted Clip AV stream file and hand it over to the application.\"";

// A drive stand-in whose bus stage reports `unmapped`; every read succeeds.
struct Reader {
    unmapped: Vec<UnmappedStreamFile>,
}

impl libfreemkv::sector::SectorSource for Reader {
    fn capacity_sectors(&self) -> u32 {
        CAPACITY
    }
    fn read_sectors(&mut self, _lba: u32, count: u16, buf: &mut [u8], _r: bool) -> Result<usize> {
        let n = count as usize * SECTOR;
        buf[..n].fill(0x5A);
        Ok(n)
    }
    fn unmapped_stream_files(&self) -> &[UnmappedStreamFile] {
        &self.unmapped
    }
}

fn lost_clip() -> Reader {
    let cause = Error::DiscRead {
        sector: 41,
        status: None,
        sense: None,
    };
    Reader {
        unmapped: vec![UnmappedStreamFile::new(
            "/BDMV/STREAM/00002.m2ts".into(),
            41,
            &cause,
        )],
    }
}

fn disc() -> Disc {
    Disc {
        volume_id: "BUS".into(),
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

fn sweep_opts() -> SweepOptions<'static> {
    SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: Some(16),
        skip_on_error: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    }
}

fn assert_refused(err: Error, what: &str) {
    assert_eq!(
        err.code(),
        E_BUS_STREAM_UNMAPPED,
        "{what}: {SPEC_BD_3_7_NOTE}"
    );
    assert_eq!(
        err.to_string(),
        format!("E{E_BUS_STREAM_UNMAPPED}: /BDMV/STREAM/00002.m2ts"),
        "{what} must name the file"
    );
}

#[test]
fn copy_refuses_before_creating_the_image() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("copy.iso");
    for decrypt in [true, false] {
        let opts = CopyOptions {
            decrypt,
            ..Default::default()
        };
        let err = freemkv_engine::copy(&disc(), &mut lost_clip(), &iso, &opts)
            .expect_err(SPEC_BD_3_7_NOTE);
        assert_refused(err, if decrypt { "copy" } else { "raw copy" });
        assert!(!iso.exists(), "{SPEC_BD_3_7_NOTE}");
        assert!(!freemkv_engine::mapfile_path_for(&iso).exists());
    }
}

// Even an image a mapfile calls complete is refused: its sectors may still be bus-encrypted.
#[test]
fn copy_refuses_to_resume_or_bless_an_existing_image() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("done.iso");
    let total = CAPACITY as u64 * SECTOR as u64;
    std::fs::write(&iso, vec![0u8; total as usize]).unwrap();
    {
        let mut mf =
            Mapfile::create(&freemkv_engine::mapfile_path_for(&iso), total, "test").unwrap();
        mf.record(0, total, SectorStatus::Finished).unwrap();
    }
    let opts = CopyOptions {
        decrypt: false,
        ..Default::default()
    };
    let err =
        freemkv_engine::copy(&disc(), &mut lost_clip(), &iso, &opts).expect_err(SPEC_BD_3_7_NOTE);
    assert_refused(err, "copy over a finished mapfile");
}

#[test]
fn sweep_refuses_before_creating_the_image() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("sweep.iso");
    let err = freemkv_engine::sweep(&disc(), &mut lost_clip(), &iso, &sweep_opts())
        .expect_err(SPEC_BD_3_7_NOTE);
    assert_refused(err, "sweep");
    assert!(!iso.exists(), "{SPEC_BD_3_7_NOTE}");
    assert!(!freemkv_engine::mapfile_path_for(&iso).exists());
}

#[test]
fn patch_refuses_to_write_into_an_existing_image() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("patch.iso");
    let total = CAPACITY as u64 * SECTOR as u64;
    std::fs::write(&iso, vec![0u8; total as usize]).unwrap();
    let mf_path = freemkv_engine::mapfile_path_for(&iso);
    {
        let mut mf = Mapfile::create(&mf_path, total, "test").unwrap();
        mf.record(0, total, SectorStatus::NonTrimmed).unwrap();
    }
    let popts = PatchOptions::for_patch_pass(false, None, None, None);
    let Err(err) = freemkv_engine::patch(&disc(), &mut lost_clip(), &iso, &popts) else {
        panic!("{SPEC_BD_3_7_NOTE}");
    };
    assert_refused(err, "patch");
    assert_eq!(
        std::fs::read(&iso).unwrap(),
        vec![0u8; total as usize],
        "untouched"
    );
}

// Control: with every stream file located, the same sweep runs to completion.
#[test]
fn sweep_proceeds_when_every_stream_file_is_located() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("ok.iso");
    let mut reader = Reader {
        unmapped: Vec::new(),
    };
    freemkv_engine::sweep(&disc(), &mut reader, &iso, &sweep_opts()).expect("clean sweep");
    assert_eq!(
        std::fs::metadata(&iso).unwrap().len(),
        CAPACITY as u64 * SECTOR as u64
    );
}
