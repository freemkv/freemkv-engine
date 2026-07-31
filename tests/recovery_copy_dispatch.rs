//! Recovery regression tests relocated from `libfreemkv`'s `src/disc/mod.rs`
//! inline test module when the recovery strategy moved into this crate.
//!
//! They drive the public `copy` / `sweep` / `patch` verbs end-to-end against a
//! synthetic `SectorSource`, so they live in `tests/` rather than inline: they
//! guard the copy-dispatch tree, the resume/mapfile reconciliation matrix, the
//! pre-flight decrypt gate, and the Pass-1 damage-jump bookkeeping.

use freemkv_engine::{
    CopyOptions, DamageSeverity, Mapfile, SectorStatus, SweepOptions, classify_damage,
};
use libfreemkv::disc::{AacsState, DiscRegion, KeyOrigin};
use libfreemkv::{ContentFormat, Disc, DiscFormat};

// ─── classify_damage severity thresholds ────────────────────────────────────

#[test]
fn clean_when_no_damage() {
    assert_eq!(classify_damage(0, 0.0), DamageSeverity::Clean);
}
#[test]
fn cosmetic_for_a_handful() {
    assert_eq!(classify_damage(1, 5.0), DamageSeverity::Cosmetic);
    assert_eq!(classify_damage(50, 999.0), DamageSeverity::Cosmetic);
}
#[test]
fn moderate_threshold_by_sectors() {
    assert_eq!(classify_damage(51, 0.0), DamageSeverity::Moderate);
}
#[test]
fn moderate_threshold_by_time() {
    assert_eq!(classify_damage(10, 1_000.0), DamageSeverity::Moderate);
}
#[test]
fn serious_threshold_by_sectors() {
    assert_eq!(classify_damage(500, 0.0), DamageSeverity::Serious);
}
#[test]
fn serious_threshold_by_time() {
    assert_eq!(classify_damage(10, 30_000.0), DamageSeverity::Serious);
}

struct MockReader {
    total_sectors: u32,
    bad_sectors: std::collections::HashSet<u32>,
}

impl libfreemkv::sector::SectorSource for MockReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        let n = count as usize * 2048;
        for i in 0..count {
            if self.bad_sectors.contains(&(lba + i as u32)) {
                return Err(libfreemkv::error::Error::DiscRead {
                    sector: (lba + i as u32) as u64,
                    status: Some(0x02),
                    sense: Some(libfreemkv::ScsiSense {
                        sense_key: 0x02,
                        asc: 0x04,
                        ascq: 0x3E,
                    }),
                });
            }
        }
        buf[..n].fill(0xAA);
        Ok(n)
    }

    fn capacity_sectors(&self) -> u32 {
        self.total_sectors
    }
}

fn make_test_disc(sectors: u32, name: &str) -> Disc {
    Disc {
        volume_id: name.into(),
        meta_title: Some(name.into()),
        format: DiscFormat::Uhd,
        capacity_sectors: sectors,
        capacity_bytes: sectors as u64 * 2048,
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

fn aacs_with(unit_keys: Vec<(u32, [u8; 16])>) -> AacsState {
    AacsState {
        version: 2,
        bus_encryption: true,
        mkb_version: None,
        disc_hash: String::new(),
        key_source: KeyOrigin::DeviceKey,
        vuk: None,
        unit_keys,
        read_data_key: None,
        volume_id: [0u8; 16],
        uk_ro: Vec::new(),
        mkb: Vec::new(),
    }
}

#[test]
fn sweep_to_dev_null_no_enodev() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 1000;
    let bad: std::collections::HashSet<u32> = [500u32, 501, 502].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad,
    };
    let disc = make_test_disc(sectors, "T1");
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);
    assert!(
        result.is_ok(),
        "sweep to regular file should succeed: {:?}",
        result.err()
    );
}

/// disc→ISO correctness gate (the headline bug, at the copy entry point):
/// a DECRYPTING copy (`decrypt: true`, i.e. not --raw) of an AACS disc with
/// no resolved key must ERROR before reading any sector — never write
/// ciphertext to the ISO and return Ok. Asserts the error code is NoDiscKey
/// AND that no non-empty ISO was produced.
#[test]
fn copy_decrypting_aacs_no_key_errors_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("garbage.iso");
    let sectors: u32 = 999; // 3-aligned for AACS units
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let mut disc = make_test_disc(sectors, "UHD");
    disc.encrypted = true;
    disc.aacs = Some(aacs_with(Vec::new())); // encrypted, no unit key → None
    let opts = CopyOptions {
        decrypt: true, // NOT --raw → decryption is required
        multipass: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let err = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts)
        .expect_err("decrypting copy of AACS-no-key disc must error pre-flight");
    assert_eq!(
        err.code(),
        libfreemkv::error::Error::NoDiscKey {
            disc_hash: String::new()
        }
        .code(),
        "must surface NoDiscKey, not silently write ciphertext"
    );
    // No partial/garbage ISO: the gate fired before the sweep opened/sized
    // the file, so either the file doesn't exist or it's empty.
    let produced = std::fs::metadata(&iso_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(produced, 0, "no ciphertext ISO may be written");
}

/// The same disc under `--raw` (`decrypt: false`) must PROCEED: the gate is
/// a no-op for raw, the sweep runs as a pass-through and writes the
/// encrypted image the user asked for. Proves the gate doesn't over-fire.
#[test]
fn copy_raw_aacs_no_key_proceeds() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("raw.iso");
    let sectors: u32 = 999;
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let mut disc = make_test_disc(sectors, "UHD");
    disc.encrypted = true;
    disc.aacs = Some(aacs_with(Vec::new()));
    let opts = CopyOptions {
        decrypt: false, // --raw: no decryption, no key needed
        multipass: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    assert!(
        freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).is_ok(),
        "--raw copy of an encrypted disc must proceed (encrypted image is the goal)"
    );
}

#[test]
fn sweep_to_dev_null_real() {
    let sectors: u32 = 1000;
    let bad: std::collections::HashSet<u32> = [500u32, 501, 502].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad,
    };
    let disc = make_test_disc(sectors, "T2");
    let _cleanup = CleanupGuard(disc.mapfile_for(std::path::Path::new("/dev/null")));
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, std::path::Path::new("/dev/null"), &opts);
    assert!(
        result.is_ok(),
        "sweep to /dev/null should not fail with ENODEV: {:?}",
        result.err()
    );
}

/// End-to-end Pass-1 sweep against a synthetic `MockReader` with an injected
/// bad-sector region, asserting the RESULTING MAPFILE — the thing the sweep
/// loop and damage-jump exist to produce. Drives the real `sweep` (no
/// live drive, per the project's "synthetic fixtures only" rule) and checks:
///   * the leading good region is marked Finished,
///   * the bad region (and the skip-ahead gap the damage-jump zero-fills) is
///     marked NonTrimmed,
///   * the damage-jump actually engaged — the NonTrimmed span is far larger
///     than the single failed ECC batch, which only happens if Pass-1 jumped
///     ahead (JUMP_BASE_SECTORS×batch) and zero-filled the gap as NonTrimmed,
///   * the mapfile covers the whole disc with no overlap, and good+retryable
///     accounting matches.
///
/// Note: this exercises the real cooldown/pause pacing, so it spends a few
/// seconds of wall time on the single zone-entry pause (same cost the
/// existing `sweep_to_dev_null_real` already pays) — but unlike that test it
/// asserts the actual recovery bookkeeping, not just `is_ok()`.
#[test]
fn sweep_marks_bad_region_nontrimmed_and_engages_damage_jump() {
    let sectors: u32 = 1000;
    // One bad sector at LBA 320 fails the entire ECC batch [320,352).
    // batch=32 for UHD, so [0,320) = 10 clean batches before the failure.
    let bad: std::collections::HashSet<u32> = [320u32].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad,
    };
    let disc = make_test_disc(sectors, "DJ");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("dj.iso");
    let opts = SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: None, // → ecc batch (32) for UHD
        skip_on_error: true, // multipass → damage-jump engaged
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("sweep");

    let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).expect("load mapfile");
    let good = mf.ranges_with(&[SectorStatus::Finished]);
    let bad_ranges = mf.ranges_with(&[SectorStatus::NonTrimmed]);
    const SEC: u64 = libfreemkv::consts::SECTOR_BYTES_U64;
    let disc_bytes = sectors as u64 * SEC;

    // The first failing batch starts at LBA 320; everything before it read
    // cleanly and must be Finished.
    let good_bytes: u64 = good.iter().map(|(_, sz)| sz).sum();
    assert!(
        good_bytes > 0,
        "leading clean region must be marked Finished"
    );
    assert!(
        good.iter().all(|(pos, sz)| pos + sz <= 320 * SEC),
        "all Finished bytes must lie before the bad batch at LBA 320; got {good:?}"
    );
    // The clean lead is the 10 batches [0,320) = 320 sectors.
    assert_eq!(
        good_bytes,
        320 * SEC,
        "exactly the 320 clean sectors before the failure are Finished"
    );

    // The bad region must be NonTrimmed and must START at the failed batch.
    assert!(
        !bad_ranges.is_empty(),
        "the failed batch must produce a NonTrimmed range"
    );
    let bad_bytes: u64 = bad_ranges.iter().map(|(_, sz)| sz).sum();
    let (first_bad_pos, _) = bad_ranges[0];
    assert_eq!(
        first_bad_pos,
        320 * SEC,
        "NonTrimmed must begin at the failed ECC batch (LBA 320)"
    );

    // Damage-jump proof: a single ECC batch is 32 sectors. If only the failed
    // batch were marked, NonTrimmed would be ~32 sectors. The fast-jump
    // (JUMP_BASE_SECTORS=1024 × batch=32) overshoots this 1000-sector disc, so
    // the entire tail from the failure to EOF is zero-filled NonTrimmed — far
    // more than one batch. That can ONLY happen if the jump engaged.
    assert!(
        bad_bytes > 32 * SEC,
        "NonTrimmed span ({} sectors) must exceed a single ECC batch — proves \
         the damage-jump skipped ahead and zero-filled the gap",
        bad_bytes / SEC
    );
    // Specifically: the jump overshoots EOF, so the whole tail [320,1000) is
    // NonTrimmed.
    assert_eq!(
        bad_bytes,
        (sectors as u64 - 320) * SEC,
        "the damage-jump overshoots EOF → the entire tail is NonTrimmed"
    );

    // Whole-disc coverage with no gaps/overlap: Finished + NonTrimmed = disc.
    assert_eq!(
        good_bytes + bad_bytes,
        disc_bytes,
        "Finished + NonTrimmed must cover the whole disc exactly"
    );
    // Stats agree with the range view.
    let stats = mf.stats();
    assert_eq!(stats.bytes_good, good_bytes, "stats.bytes_good vs ranges");
    assert_eq!(
        stats.bytes_retryable, bad_bytes,
        "NonTrimmed counts as retryable in stats"
    );
    assert!(
        stats.bytes_unreadable == 0,
        "Pass-1 never promotes to Unreadable (that's a later pass's job)"
    );
}

/// Regression (finding 6): sweep() resume against a mapfile whose
/// total_size != the real disc size must DOWNGRADE to a fresh full sweep
/// covering [0, capacity), not reuse the stale mapfile (which would
/// abandon the disc tail or read past capacity). Mirrors copy()'s
/// covers_disc reconciliation for the direct-sweep entry point.
#[test]
fn sweep_resume_downgrades_on_size_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("mismatch.iso");

    // First sweep: a small disc → mapfile sized to small_sectors.
    let small_sectors: u32 = 500;
    let mut small_reader = MockReader {
        total_sectors: small_sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let small_disc = make_test_disc(small_sectors, "SMALL");
    let opts0 = SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: None,
        skip_on_error: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    freemkv_engine::sweep(&small_disc, &mut small_reader, &iso_path, &opts0)
        .expect("initial small sweep");
    let mf = small_disc.mapfile_for(&iso_path);
    assert_eq!(
        Mapfile::load(&mf).unwrap().total_size(),
        small_sectors as u64 * 2048,
        "precondition: mapfile reflects the small disc"
    );

    // Now a LARGER disc resumes against that stale (under-cover) mapfile.
    // The reconciliation must force a fresh full sweep of the big disc.
    let big_sectors: u32 = 2000;
    let mut big_reader = MockReader {
        total_sectors: big_sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let big_disc = make_test_disc(big_sectors, "BIG");
    let opts_resume = SweepOptions {
        resume: true,
        ..opts0
    };
    let result = freemkv_engine::sweep(&big_disc, &mut big_reader, &iso_path, &opts_resume)
        .expect("resume sweep on mismatched mapfile");

    assert_eq!(
        result.bytes_total,
        big_sectors as u64 * 2048,
        "fresh sweep must be sized to the real (big) disc"
    );
    assert_eq!(
        result.bytes_good,
        big_sectors as u64 * 2048,
        "the whole big disc (incl. the tail beyond the stale mapfile) must be swept"
    );
    assert_eq!(
        Mapfile::load(&mf).unwrap().total_size(),
        big_sectors as u64 * 2048,
        "mapfile must be re-created at the real disc size, not the stale one"
    );
}

/// Regression (resume/mapfile consistency, MED): a resume sweep against a
/// mapfile that claims prior progress (Finished ranges) while the ISO is
/// missing/zero-length must DOWNGRADE to a fresh full sweep — NOT reuse the
/// stale mapfile. The producer only builds work from NonTried ranges, so a
/// reused mapfile would leave every Finished range unread and ZERO in the
/// new ISO (a silent hole). Reachable via autorip ResumeMode::Require when
/// the ISO was deleted/truncated but the mapfile survived. The fresh-sweep
/// downgrade self-heals: all ranges are re-read and the ISO is fully
/// populated.
#[test]
fn sweep_resume_downgrades_on_zero_iso_with_progress_mapfile() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("zeroed.iso");

    let sectors: u32 = 500;
    let total_bytes = sectors as u64 * 2048;
    let disc = make_test_disc(sectors, "ZEROED");

    // First sweep: clean disc → ISO fully written, mapfile all-Finished.
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts0 = SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: None,
        skip_on_error: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts0).expect("initial clean sweep");
    let mf = disc.mapfile_for(&iso_path);
    let loaded = Mapfile::load(&mf).unwrap();
    assert_eq!(
        loaded.stats().bytes_pending,
        0,
        "precondition: a clean sweep leaves no pending (all Finished) ranges"
    );

    // Truncate the ISO to zero length while the progress-claiming mapfile
    // survives — exactly the inconsistent-resume case.
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&iso_path)
        .expect("truncate ISO to zero");
    assert_eq!(
        std::fs::metadata(&iso_path).unwrap().len(),
        0,
        "precondition: ISO is zero-length"
    );

    // Resume sweep: must downgrade to a fresh FULL sweep, re-reading every
    // range (including the formerly-Finished ones).
    let mut reader2 = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts_resume = SweepOptions {
        resume: true,
        ..opts0
    };
    let result = freemkv_engine::sweep(&disc, &mut reader2, &iso_path, &opts_resume)
        .expect("resume sweep on zero-length ISO");

    // A holed resume would re-read nothing (no NonTried ranges) → bytes_good
    // == 0 and a zero ISO. The downgrade re-reads the whole disc.
    assert_eq!(
        result.bytes_good, total_bytes,
        "downgrade must re-read the whole disc, not skip Finished ranges"
    );
    assert_eq!(
        std::fs::metadata(&iso_path).unwrap().len(),
        total_bytes,
        "ISO must be re-sized + fully written, not left zero/holed"
    );

    // The ISO must actually contain the swept data (0xAA) at LBA 0 — proof
    // the formerly-Finished head range was re-read, not left as a hole.
    let iso = std::fs::read(&iso_path).unwrap();
    assert_eq!(
        &iso[..2048],
        &[0xAAu8; 2048][..],
        "head sector must hold re-read data, not a zero hole"
    );
}

/// Regression (resume reconciliation, MED follow-on): a resume sweep against
/// a CORRUPT / unparseable mapfile must DOWNGRADE to a fresh full sweep —
/// not proceed with resume=true (which would hand a garbage/empty mapfile to
/// open_or_create and silently skip ranges). The `load()` Err arm sets
/// resume=false; the `!resume` path then drops the corrupt mapfile and the
/// rip restarts clean. Consistent with the total_size-mismatch downgrade.
#[test]
fn sweep_resume_downgrades_on_corrupt_mapfile() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("corrupt.iso");

    let sectors: u32 = 500;
    let total_bytes = sectors as u64 * 2048;
    let disc = make_test_disc(sectors, "CORRUPT");
    let mf = disc.mapfile_for(&iso_path);

    // Write a non-empty ISO so the zero-length-ISO guard is NOT what triggers
    // the downgrade — we want the corrupt-mapfile path specifically.
    std::fs::write(&iso_path, vec![0u8; total_bytes as usize]).unwrap();
    // Plant a corrupt mapfile: garbage bytes that Mapfile::load can't parse.
    std::fs::write(&mf, b"this is not a valid ddrescue mapfile\nxxxx\n").unwrap();
    assert!(
        Mapfile::load(&mf).is_err(),
        "precondition: the planted mapfile must be unparseable"
    );

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = SweepOptions {
        decrypt: false,
        resume: true,
        batch_sectors: None,
        skip_on_error: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts)
        .expect("resume sweep on corrupt mapfile");

    // The downgrade must re-sweep the whole disc from a fresh mapfile.
    assert_eq!(
        result.bytes_good, total_bytes,
        "corrupt-mapfile resume must downgrade to a fresh full sweep"
    );
    let reloaded =
        Mapfile::load(&mf).expect("a valid mapfile must have been written by the fresh sweep");
    assert_eq!(
        reloaded.total_size(),
        total_bytes,
        "mapfile must be re-created at the real disc size"
    );
    assert_eq!(
        reloaded.stats().bytes_pending,
        0,
        "the fresh sweep must leave all ranges Finished"
    );
}

/// Regression: a fresh (non-resume) sweep MUST abort if the stale mapfile
/// cannot be removed, rather than swallowing the error and letting
/// `open_or_create` load the stale file (which would make the new disc
/// inherit old Finished ranges → silently zero-filled ISO). We force the
/// remove to fail with a non-ENOENT error by placing a NON-EMPTY DIRECTORY
/// at the mapfile path (`remove_file` on a dir fails, and a non-empty dir
/// can't be ENOENT).
#[test]
fn sweep_fresh_aborts_when_stale_mapfile_unremovable() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("blocked.iso");

    let sectors: u32 = 500;
    let disc = make_test_disc(sectors, "BLOCKED");
    let mf = disc.mapfile_for(&iso_path);
    // Put a non-empty directory where the mapfile would live.
    std::fs::create_dir_all(&mf).unwrap();
    std::fs::write(mf.join("occupant"), b"x").unwrap();

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: None,
        skip_on_error: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts);
    assert!(
        result.is_err(),
        "fresh sweep must abort when the stale mapfile cannot be removed"
    );
}

struct CleanupGuard(std::path::PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn sweep_dev_null_full_good() {
    let sectors: u32 = 2000;
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let disc = make_test_disc(sectors, "T3");
    let _cleanup = CleanupGuard(disc.mapfile_for(std::path::Path::new("/dev/null")));
    let opts = CopyOptions {
        decrypt: false,
        multipass: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, std::path::Path::new("/dev/null"), &opts);
    assert!(
        result.is_ok(),
        "full-good sweep to /dev/null should succeed: {:?}",
        result.err()
    );
    let r = result.unwrap();
    assert!(r.complete, "should be complete");
    assert_eq!(r.bytes_good, sectors as u64 * 2048);
}

/// Finding #6 regression: on resume, copy() must NOT abandon the un-swept
/// NonTried tail when retryable (NonTrimmed) bytes also remain. The mapfile
/// covers the disc and has BOTH a NonTrimmed (retryable) range and a
/// NonTried tail; dispatch must route to a resume sweep first so the tail is
/// actually read. Before the fix, `bytes_retryable > 0` short-circuited to
/// patch and the NonTried tail was silently left unread.
#[test]
fn resume_sweeps_nontried_tail_even_with_retryable_present() {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    // Reader that records every LBA it is asked to read.
    struct TrackingReader {
        total_sectors: u32,
        reads: Arc<Mutex<HashSet<u32>>>,
    }
    impl libfreemkv::sector::SectorSource for TrackingReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::error::Result<usize> {
            {
                let mut r = self.reads.lock().unwrap();
                for i in 0..count as u32 {
                    r.insert(lba + i);
                }
            }
            let n = count as usize * 2048;
            buf[..n].fill(0xAA);
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.total_sectors
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "T6Tail");

    // Pre-build a mapfile covering the whole disc:
    //   [0..100)   Finished
    //   [100..150) NonTrimmed (retryable)
    //   [150..200) NonTried   (un-swept tail)
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, sectors as u64 * 2048, "test").unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        mf.record(100 * 2048, 50 * 2048, SectorStatus::NonTrimmed)
            .unwrap();
        // [150..200) stays NonTried from create()'s initial region.
        mf.flush().unwrap();

        // Sanity on the constructed state.
        let st = mf.stats();
        assert!(st.bytes_nontried > 0, "must have a NonTried tail");
        assert!(st.bytes_retryable > 0, "must have retryable bytes too");
        assert_eq!(mf.total_size(), sectors as u64 * 2048);
    }
    // The ISO file must exist for the sweep to write into.
    std::fs::write(&iso_path, vec![0u8; sectors as usize * 2048]).unwrap();

    let reads = Arc::new(Mutex::new(HashSet::new()));
    let mut reader = TrackingReader {
        total_sectors: sectors,
        reads: reads.clone(),
    };
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);
    assert!(result.is_ok(), "resume copy failed: {:?}", result.err());

    // The un-swept tail [150..200) MUST have been read by the resume sweep.
    let got = reads.lock().unwrap();
    let tail_read = (150u32..200).any(|lba| got.contains(&lba));
    assert!(
        tail_read,
        "resume must sweep the NonTried tail; tail sectors were never read"
    );
}

/// Regression (rc.6 user fix): a PLAIN (non-`--multipass`) `disc:// → iso://`
/// copy interrupted by Ctrl-C must RESUME from where it stopped when the
/// SAME command is re-issued — not restart from sector 0. The CLI help and
/// `rip_iso` examples promise "auto-resumes if interrupted". Before the fix
/// the whole mapfile-resume dispatch in `copy` was gated behind
/// `if opts.multipass`, so a plain copy always called
/// `sweep_internal(resume=false)`, which wiped the mapfile + ISO and swept
/// the disc again from LBA 0.
///
/// Simulate an interrupted plain sweep: a mapfile that covers the disc with
/// a Finished prefix [0..100) and a NonTried tail [100..200). A plain re-run
/// must read ONLY the tail (resume) and leave the prefix untouched.
#[test]
fn plain_copy_resumes_nontried_tail_after_interrupt() {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    // Reader that records every LBA it is asked to read.
    struct TrackingReader {
        total_sectors: u32,
        reads: Arc<Mutex<HashSet<u32>>>,
    }
    impl libfreemkv::sector::SectorSource for TrackingReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::error::Result<usize> {
            {
                let mut r = self.reads.lock().unwrap();
                for i in 0..count as u32 {
                    r.insert(lba + i);
                }
            }
            let n = count as usize * 2048;
            buf[..n].fill(0xAA);
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.total_sectors
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "PlainResume");

    // Pre-build a mapfile mimicking an interrupted plain sweep:
    //   [0..100)   Finished  (already written before Ctrl-C)
    //   [100..200) NonTried  (un-swept tail)
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, sectors as u64 * 2048, "test").unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        // [100..200) stays NonTried from create()'s initial region.
        mf.flush().unwrap();

        let st = mf.stats();
        assert!(st.bytes_nontried > 0, "must have a NonTried tail");
        assert_eq!(st.bytes_retryable, 0, "plain interrupt leaves no retryable");
        assert_eq!(mf.total_size(), sectors as u64 * 2048);
    }
    // The ISO file must already exist (it was being written before the
    // interrupt) so the resume opens it rather than recreating it.
    std::fs::write(&iso_path, vec![0u8; sectors as usize * 2048]).unwrap();

    let reads = Arc::new(Mutex::new(HashSet::new()));
    let mut reader = TrackingReader {
        total_sectors: sectors,
        reads: reads.clone(),
    };
    // PLAIN copy — multipass: false. This is the path the bug broke.
    let opts = CopyOptions {
        decrypt: false,
        multipass: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);
    assert!(
        result.is_ok(),
        "plain resume copy failed: {:?}",
        result.err()
    );

    let got = reads.lock().unwrap();
    // The NonTried tail [100..200) MUST have been read by the resume sweep.
    let tail_read = (100u32..200).any(|lba| got.contains(&lba));
    assert!(
        tail_read,
        "plain copy must resume-sweep the NonTried tail; tail sectors were never read"
    );
    // The Finished prefix [0..100) must NOT be re-read — that would mean a
    // restart-from-zero (the bug), not a resume.
    let prefix_reread = (0u32..100).any(|lba| got.contains(&lba));
    assert!(
        !prefix_reread,
        "plain copy must NOT re-read the already-Finished prefix (it restarted from sector 0)"
    );

    // The mapfile must now be fully Finished (disc fully swept on resume).
    let reloaded = Mapfile::load(&mf_path).unwrap();
    assert_eq!(
        reloaded.stats().bytes_nontried,
        0,
        "resume sweep must clear the NonTried tail"
    );
}

#[test]
fn patch_dev_null_after_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 500;
    let bad: std::collections::HashSet<u32> = [100u32, 200, 300].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad.clone(),
    };
    let disc = make_test_disc(sectors, "T4");

    let sweep_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let sweep_result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &sweep_opts);
    assert!(
        sweep_result.is_ok(),
        "sweep should succeed: {:?}",
        sweep_result.err()
    );

    let mut reader2 = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let patch_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let patch_result = freemkv_engine::copy(&disc, &mut reader2, &iso_path, &patch_opts);
    assert!(
        patch_result.is_ok(),
        "patch should succeed: {:?}",
        patch_result.err()
    );
    let pr = patch_result.unwrap();
    assert!(
        pr.complete,
        "patch should complete: bytes_pending={}",
        pr.bytes_pending
    );
}

#[test]
fn patch_dev_null_direct() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 500;
    let bad: std::collections::HashSet<u32> = [100u32, 200, 300].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad.clone(),
    };
    let disc = make_test_disc(sectors, "T5");

    let sweep_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let _sweep_result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &sweep_opts).unwrap();

    let mut reader2 = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let patch_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let patch_result = freemkv_engine::copy(
        &disc,
        &mut reader2,
        std::path::Path::new("/dev/null"),
        &patch_opts,
    );
    assert!(
        patch_result.is_ok(),
        "patch to /dev/null should succeed: {:?}",
        patch_result.err()
    );
}

/// Synthetic regression test for the 0.18 SweepSink + Pipeline
/// migration. ~100 batches of clean reads (6000 sectors at the
/// default 60-sector single-pass batch size); verifies all bytes
/// land in the ISO and the consumer's final stats match the input.
/// The throughput regression check (vs 0.17.13) is a separate
/// manual / live-drive concern; here we only assert correctness.
#[test]
fn sweep_pipeline_full_good_100_batches() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    // 6000 sectors / 60-sector default batch = exactly 100
    // produce/consume cycles through the pipeline.
    let sectors: u32 = 6000;
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let disc = make_test_disc(sectors, "TPipeline100");
    let opts = CopyOptions {
        decrypt: false,
        multipass: false,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);
    let r = result.expect("100-batch clean sweep should succeed");
    assert!(r.complete, "complete=true expected");
    assert!(!r.halted, "halted=false expected");
    assert_eq!(
        r.bytes_good,
        sectors as u64 * 2048,
        "all sectors must be marked good after a 100% clean sweep"
    );
    assert_eq!(
        r.bytes_pending, 0,
        "no pending bytes expected after a clean sweep"
    );
    // The ISO file must end up the right size — the consumer
    // wrote everything before fsync.
    let meta = std::fs::metadata(&iso_path).unwrap();
    assert_eq!(meta.len(), sectors as u64 * 2048);
}

/// Regression: copy() dispatch with covers_disc=true, retryable=0, nontried>0 must
/// route to sweep_internal(resume=true) so the unread NonTried ranges are actually
/// read rather than silently abandoned.
///
/// Before the fix the fallthrough returned a terminal CopyResult immediately,
/// leaving the NonTried sectors unread.
#[test]
fn copy_dispatch_routes_to_sweep_when_nontried_gt_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("test.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "DispatchNonTried");
    let disc_size = sectors as u64 * 2048;

    // Synthesise a mapfile that covers the disc (total_size == disc_size) with:
    //   - [0, half_bytes): Finished
    //   - [half_bytes, disc_size): NonTried
    // This gives covers_disc=true, bytes_retryable=0, bytes_nontried>0.
    let mf_path = disc.mapfile_for(&iso_path);
    let half_bytes = disc_size / 2;
    {
        let mut map = Mapfile::create(&mf_path, disc_size, "test").expect("create mapfile");
        map.record(0, half_bytes, SectorStatus::Finished)
            .expect("record Finished");
        map.flush().expect("flush");
    }

    // Create an ISO file pre-sized to the full disc size so the resume
    // sweep can open it and write the NonTried regions at their offsets.
    // (len > 0 selects the resume-open branch; full pre-size avoids
    // short-seek writes past EOF.)
    {
        let f = std::fs::File::create(&iso_path).expect("create iso");
        f.set_len(disc_size).expect("pre-size iso");
    }

    // All sectors are readable in this reader.
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),

        key_fetch: None,
    };

    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);
    assert!(
        result.is_ok(),
        "copy with nontried>0 should succeed: {:?}",
        result.err()
    );
    let r = result.unwrap();
    // The sweep must have read the NonTried half — bytes_good should be
    // the whole disc, not just the already-Finished half.
    assert_eq!(
        r.bytes_good, disc_size,
        "all sectors must be good after resume sweep reads the NonTried half \
         (before fix: terminal returned with bytes_good={}, skipping {} NonTried bytes)",
        half_bytes, half_bytes
    );
}

/// A resumed sweep that clears the NonTried tail but leaves pre-existing
/// Unreadable bytes must NOT report `complete: true`. `complete` means
/// "nothing pending AND nothing permanently lost" — reporting a lossy rip as
/// finished is the silent-loss shape this crate exists to prevent.
#[test]
fn resume_with_pre_existing_unreadable_is_not_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("lossy.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "Lossy");

    // [0..100)   Finished
    // [100..150) Unreadable  (permanently lost on a previous run)
    // [150..200) NonTried    (tail this resume will clear)
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, sectors as u64 * 2048, "test").unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        mf.record(100 * 2048, 50 * 2048, SectorStatus::Unreadable)
            .unwrap();
        mf.flush().unwrap();
    }
    // The ISO must already exist at full length: sweep's inconsistent-resume
    // guard forces a FRESH sweep (dropping the mapfile) when the mapfile
    // claims progress but the ISO is missing or zero-length.
    std::fs::write(&iso_path, vec![0u8; sectors as usize * 2048]).unwrap();

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let r = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).unwrap();

    assert!(
        r.bytes_unreadable > 0,
        "fixture must retain the Unreadable range (got {})",
        r.bytes_unreadable
    );
    assert_eq!(r.bytes_pending, 0, "the NonTried tail should be swept");
    assert!(
        !r.complete,
        "complete must be false while {} bytes are permanently unreadable",
        r.bytes_unreadable
    );
}
