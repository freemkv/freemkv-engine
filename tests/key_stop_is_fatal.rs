//! KU-L2 review: the decrypting readers' on-arrival loud stop is FATAL in every pass.
//!
//! KU §2.4: "No held key opens U → loud stop: E7022 (title) or E7032 (image or folder)",
//! and §6: it "is the only failure after output starts". Such a unit was READ; it is not
//! damage. So sweep, patch and a multipass copy must end with that code: no retry, no
//! skip, no hole in the mapfile and no zero-fill.

use freemkv_engine::{CopyOptions, Mapfile, PatchOptions, SectorStatus, SweepOptions};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{Error, Result};
use libfreemkv::{ContentFormat, Disc, DiscFormat};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SECTOR: usize = 2048;
const CAPACITY: u32 = 64;
const BATCH: u16 = 16;
const STOP_LBA: u32 = 20;

fn e7022() -> Error {
    Error::NoDiscKey {
        disc_hash: "ab".repeat(20),
    }
}

fn e7032() -> Error {
    Error::WholeDiscKeyMissing
}

/// Serves `0xAA` everywhere except a read covering `STOP_LBA`, which fails with `stop()`;
/// counts those reads (a retry or a bisection would read it again).
struct KeyStopReader {
    stop: fn() -> Error,
    stop_reads: Arc<AtomicUsize>,
}

impl libfreemkv::sector::SectorSource for KeyStopReader {
    fn capacity_sectors(&self) -> u32 {
        CAPACITY
    }
    fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], _r: bool) -> Result<usize> {
        let n = count as usize * SECTOR;
        if lba <= STOP_LBA && STOP_LBA < lba + count as u32 {
            self.stop_reads.fetch_add(1, Ordering::SeqCst);
            return Err((self.stop)());
        }
        buf[..n].fill(0xAA);
        Ok(n)
    }
}

fn reader(stop: fn() -> Error) -> (KeyStopReader, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let r = KeyStopReader {
        stop,
        stop_reads: reads.clone(),
    };
    (r, reads)
}

fn disc() -> Disc {
    Disc {
        volume_id: "KEYSTOP".into(),
        meta_title: None,
        format: DiscFormat::BluRay,
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

// Bytes the mapfile holds as damage (a hole the next pass would re-read or zero-fill).
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

#[test]
fn a_sweep_ends_on_a_key_stop_with_its_code_and_no_hole() {
    for stop in [e7022 as fn() -> Error, e7032] {
        for skip_on_error in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let iso = tmp.path().join("sweep.iso");
            let (mut r, reads) = reader(stop);
            let opts = SweepOptions {
                batch_sectors: Some(BATCH),
                skip_on_error,
                ..Default::default()
            };
            let err = freemkv_engine::sweep(&disc(), &mut r, &iso, &opts)
                .expect_err("a key stop must end the sweep");
            assert_eq!(err.code(), stop().code(), "skip={skip_on_error}: got {err}");
            assert_eq!(reads.load(Ordering::SeqCst), 1, "never retried or bisected");
            assert_eq!(damage_bytes(&iso), 0, "no NonTrimmed hole, no zero-fill");
        }
    }
}

#[test]
fn a_patch_pass_ends_on_a_key_stop_and_leaves_the_range_pending() {
    for stop in [e7022 as fn() -> Error, e7032] {
        let tmp = tempfile::tempdir().unwrap();
        let iso = tmp.path().join("patch.iso");
        let total = CAPACITY as u64 * SECTOR as u64;
        std::fs::File::create(&iso).unwrap().set_len(total).unwrap();
        let bad = (16 * SECTOR as u64, 8 * SECTOR as u64);
        {
            let path = freemkv_engine::mapfile_path_for(&iso);
            let mut mf = Mapfile::create(&path, total, "test").unwrap();
            mf.record(0, total, SectorStatus::Finished).unwrap();
            mf.record(bad.0, bad.1, SectorStatus::NonTrimmed).unwrap();
        }
        let (mut r, reads) = reader(stop);
        let popts = PatchOptions::for_patch_pass(false, None, None, None);
        let Err(err) = freemkv_engine::patch(&disc(), &mut r, &iso, &popts) else {
            panic!("a key stop must end the patch pass");
        };
        assert_eq!(err.code(), stop().code(), "got {err}");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "never retried or bisected");
        let map = Mapfile::load(&freemkv_engine::mapfile_path_for(&iso)).unwrap();
        assert!(
            map.ranges_with(&[SectorStatus::Unreadable]).is_empty(),
            "a read unit is never marked unreadable"
        );
        assert_eq!(
            damage_bytes(&iso),
            bad.1,
            "the range stays pending, unchanged"
        );
    }
}

#[test]
fn a_multipass_copy_ends_on_a_key_stop() {
    for stop in [e7022 as fn() -> Error, e7032] {
        let tmp = tempfile::tempdir().unwrap();
        let iso = tmp.path().join("copy.iso");
        let (mut r, reads) = reader(stop);
        let opts = CopyOptions {
            multipass: true,
            ..Default::default()
        };
        let err = freemkv_engine::copy(&disc(), &mut r, &iso, &opts)
            .expect_err("a key stop must end the copy");
        assert_eq!(err.code(), stop().code(), "got {err}");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "no later pass re-reads it");
        assert_eq!(damage_bytes(&iso), 0, "no NonTrimmed hole, no zero-fill");
    }
}
