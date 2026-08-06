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

/// A multipass copy over a damaged region completes rather than erroring.
///
/// (Historic name: this once wrote to `/dev/null`, which returned ENODEV from
/// `set_len`. It writes to a regular file now — `sweep_to_dev_null_real` below
/// is the one that still exercises the character-device path.)
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
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts)
        .expect("a multipass copy over bad sectors is a reported result, not an Err");
    // `is_ok()` alone constrained nothing. Three bad sectors must show up as
    // damage and the rip must not claim to be complete.
    assert!(
        !result.complete,
        "a disc with unreadable sectors is not complete"
    );
    assert!(
        result.bytes_unreadable + result.bytes_pending > 0,
        "three dead sectors must be accounted for somewhere"
    );
    assert!(
        result.bytes_good > 0,
        "the readable 997 sectors must be recovered"
    );
    assert_eq!(result.bytes_total, sectors as u64 * 2048);
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
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts)
        .expect("--raw copy of an encrypted disc must proceed (the encrypted image is the goal)");
    // The mirror of the blocked case above, which checks the ISO is EMPTY.
    // `is_ok()` alone let a gate that over-fires into a silent no-op — return
    // Ok having written nothing — pass as "proceeded". Proceeding means the
    // whole image is on disk.
    assert!(result.complete, "a clean raw copy completes");
    let produced = std::fs::metadata(&iso_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        produced,
        sectors as u64 * 2048,
        "the full ciphertext image must be written"
    );
    assert_eq!(result.bytes_good, sectors as u64 * 2048);
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
    let result = freemkv_engine::copy(&disc, &mut reader, std::path::Path::new("/dev/null"), &opts)
        .expect("sweep to /dev/null must not fail with ENODEV");
    // `is_ok()` alone constrained nothing — the identical fixture on a regular
    // file (`sweep_to_dev_null_no_enodev`) was upgraded past that and this one,
    // the test that actually exercises the character-device destination, was
    // left behind. A `copy` that returned Ok having read nothing passed it.
    // The accounting must be the SAME as on a regular file: the sink swallows
    // the bytes, it does not excuse the bookkeeping.
    assert!(
        !result.complete,
        "a disc with unreadable sectors is not complete, sink notwithstanding"
    );
    assert!(
        result.bytes_unreadable + result.bytes_pending > 0,
        "three dead sectors must be accounted for even when writing to /dev/null"
    );
    assert!(
        result.bytes_good > 0,
        "the readable 997 sectors must be counted as read"
    );
    assert_eq!(result.bytes_total, sectors as u64 * 2048);
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

    // The un-swept tail [150..200) MUST have been read by the resume sweep —
    // ALL of it. `.any()` here accepted one sector of fifty, which is the
    // shape of a resume that starts the tail and stops.
    let got = reads.lock().unwrap();
    let missed: Vec<u32> = (150u32..200).filter(|lba| !got.contains(lba)).collect();
    assert!(
        missed.is_empty(),
        "resume must sweep the WHOLE NonTried tail; these sectors were never read: {missed:?}"
    );
    // And it must not have re-read the Finished prefix.
    let refetched: Vec<u32> = (0u32..100).filter(|lba| got.contains(lba)).collect();
    assert!(
        refetched.is_empty(),
        "the Finished prefix must not be re-read on a resume: {refetched:?}"
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

/// A patch pass whose destination is the `/dev/null` sink resumes from the
/// mapfile and recovers what the sweep could not.
///
/// This test used to sweep to a tempdir ISO and then "patch" `/dev/null`, which
/// patched nothing: `Disc::mapfile_for` deliberately redirects a `/dev/null`
/// destination to `$TMPDIR/<title>.mapfile`, so the second call never saw the
/// mapfile the first one wrote — it was a fresh full sweep of a clean reader
/// wearing a patch pass's name. Deleting the entire sweep half left it green,
/// and its only assertion was `is_ok()`, which a `copy` that read nothing also
/// satisfies. It also had no `CleanupGuard`, so it leaked that fixed temp path
/// on every run AND could resume a previous run's leftovers.
///
/// Both calls now target `/dev/null`, so they share one mapfile and the second
/// really is a patch pass over the first's damage.
#[test]
fn patch_dev_null_direct() {
    let dev_null = std::path::Path::new("/dev/null");
    let sectors: u32 = 500;
    let bad: std::collections::HashSet<u32> = [100u32, 200, 300].into_iter().collect();
    let disc = make_test_disc(sectors, "T5");
    // Own the shared temp mapfile path for the whole test: removed up front so
    // a leftover from an earlier run cannot be resumed, and on the way out so
    // this run leaves nothing behind.
    let map_path = disc.mapfile_for(dev_null);
    let _ = std::fs::remove_file(&map_path);
    let _cleanup = CleanupGuard(map_path.clone());

    let opts = || CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    };

    // Pass 1: three dead sectors, so the sweep leaves real damage behind.
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad.clone(),
    };
    let sweep = freemkv_engine::copy(&disc, &mut reader, dev_null, &opts())
        .expect("a sweep to /dev/null must not fail with ENODEV");
    assert!(
        !sweep.complete,
        "three dead sectors must leave the sweep incomplete"
    );
    assert!(
        sweep.bytes_unreadable + sweep.bytes_pending > 0,
        "the damage must reach the mapfile, or the patch below has nothing to do"
    );
    assert!(
        map_path.exists(),
        "a /dev/null destination still gets a mapfile, at {}",
        map_path.display()
    );

    // Pass 2: the same disc on a drive that can now read everything. The patch
    // resumes from that mapfile and must clear the damage.
    let mut reader2 = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let patched = freemkv_engine::copy(&disc, &mut reader2, dev_null, &opts())
        .expect("a patch pass to /dev/null must not fail");
    assert!(
        patched.complete,
        "a healthy re-read must clear the damage: pending={} unreadable={}",
        patched.bytes_pending, patched.bytes_unreadable
    );
    assert_eq!(patched.bytes_unreadable, 0);
    assert_eq!(patched.bytes_total, sectors as u64 * 2048);
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

/// `copy()`'s "already complete, don't re-read a finished ISO" shortcut must
/// verify the ISO is still THERE.
///
/// The shortcut trusts the mapfile alone: identity matches, every range is
/// Finished, so it returns success without reading a sector. But the mapfile
/// and the ISO are two files, and only one of them is being checked. Delete or
/// truncate the ISO — a staging cleanup, a remount, an operator freeing space —
/// and the mapfile still says "complete", so the call reports bytes_good = the
/// whole disc with no image on disk at all. The caller then muxes from
/// nothing.
///
/// `sweep()` already guards this exact case (see
/// `sweep_resume_downgrades_on_zero_iso_with_progress_mapfile`); the dispatch
/// shortcut in `copy()` simply never got the same check. The rip must
/// self-heal into a fresh sweep instead of claiming a success it cannot back.
#[test]
fn complete_mapfile_with_a_missing_iso_re_reads_instead_of_claiming_success() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("vanished.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "Vanished");
    let disc_size = sectors as u64 * 2048;

    // A mapfile from a rip that genuinely finished: every byte Finished.
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, disc_size, "test").unwrap();
        mf.record(0, disc_size, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
    }
    // ...but the ISO it describes is gone.
    assert!(!iso_path.exists(), "fixture: the ISO must be absent");

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

    // The claim and the artifact have to agree. Asserting on bytes_good alone
    // would pass on the broken behaviour, which reports a full disc of good
    // bytes precisely because it skipped the work.
    assert!(
        iso_path.exists(),
        "copy() reported success from a Finished mapfile but wrote no ISO"
    );
    assert_eq!(
        std::fs::metadata(&iso_path).unwrap().len(),
        disc_size,
        "the re-read ISO must be the full disc, not a stub"
    );
    assert_eq!(r.bytes_good, disc_size);
}

/// A resume against a SHORT ISO must re-read, not leave a hole.
///
/// The inconsistent-resume guard's own comment says "missing or truncated",
/// but it only ever tested for zero length. An ISO truncated to a non-zero
/// length — a partial copy, a full disk, an interrupted transfer — therefore
/// passed the guard and resumed. Since the producer builds work only from
/// NonTried ranges, every Finished range beyond the truncation point is never
/// re-read, so it stays a hole in an image the mapfile calls complete.
#[test]
fn resume_against_a_truncated_iso_re_reads_instead_of_leaving_a_hole() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("short.iso");
    let sectors: u32 = 200;
    let disc = make_test_disc(sectors, "Short");
    let disc_size = sectors as u64 * 2048;

    // The mapfile says the first half is already written and the rest is
    // un-swept.
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, disc_size, "test").unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
    }
    // ...but the ISO on disk is far shorter than the Finished prefix claims.
    // Non-zero, so the old `iso_len == 0` guard did not fire.
    std::fs::write(&iso_path, vec![0xAAu8; 10 * 2048]).unwrap();

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

    // Assert CONTENT, not length. Writing the NonTried tail extends the file
    // to full size either way, so a length check passes even when the middle
    // is a hole — the defect is that sectors 10..100 are marked Finished, were
    // never actually written, and are left as zero-fill.
    let img = std::fs::read(&iso_path).unwrap();
    assert_eq!(img.len() as u64, disc_size, "ISO must be full length");
    let first_hole = img.iter().position(|&b| b != 0xAA);
    assert_eq!(
        first_hole, None,
        "hole at byte {:?}: a Finished range past the truncation point was \
         never re-read, so it stayed zero in an image the mapfile calls good",
        first_hole
    );
    assert_eq!(r.bytes_good, disc_size);
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

/// A front-end that asks to cancel must stop the patch handler chain at the
/// next inter-read check, not after the whole chain's per-handler budgets have
/// run out. The tick already polls `should_cancel()`; it used to discard the
/// answer, so the only halt check the chain could see was `opts.halt` — which
/// every caller but `extract` leaves None.
#[test]
fn cancelling_reporter_stops_the_patch_chain_promptly() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingReader {
        total_sectors: u32,
        bad: std::collections::HashSet<u32>,
        reads: Arc<AtomicUsize>,
    }
    impl libfreemkv::sector::SectorSource for CountingReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::error::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            for i in 0..count {
                if self.bad.contains(&(lba + i as u32)) {
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
            let n = count as usize * 2048;
            buf[..n].fill(0xAA);
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.total_sectors
        }
    }

    // A reporter that cancels on the very first tick.
    struct CancelNow;
    impl libfreemkv::progress::Progress for CancelNow {
        fn report(&self, _p: &libfreemkv::progress::PassProgress) -> bool {
            false // false == halt
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("cancel.iso");
    let sectors: u32 = 500;
    let bad: std::collections::HashSet<u32> = (100u32..160).collect();
    let disc = make_test_disc(sectors, "Cancel");

    // Pass 1: lay down a mapfile with a real bad range.
    {
        let mut reader = MockReader {
            total_sectors: sectors,
            bad_sectors: bad.clone(),
        };
        let opts = CopyOptions {
            decrypt: false,
            multipass: true,
            ..Default::default()
        };
        freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).unwrap();
    }

    // Pass N: patch that bad range with a reporter that cancels immediately.
    let reads = Arc::new(AtomicUsize::new(0));
    let mut reader = CountingReader {
        total_sectors: sectors,
        bad: bad.clone(),
        reads: Arc::clone(&reads),
    };
    let reporter = CancelNow;
    let popts = freemkv_engine::PatchOptions::for_patch_pass(false, Some(&reporter), None, None);
    let out = freemkv_engine::patch(&disc, &mut reader, &iso_path, &popts).unwrap();

    let n = reads.load(Ordering::Relaxed);
    assert!(out.halted, "a cancelling reporter must halt the pass");
    assert!(
        n <= 8,
        "cancel must stop the handler chain at the next inter-read check; \
         took {n} reads (the whole chain grinds on when the tick's halt answer \
         is discarded)"
    );
}

/// A mapfile carrying unaligned ranges — e.g. imported from ddrescue run with
/// `-b 512` — must not cause a shifted write. `read_span` computes
/// `lba = pos / SECTOR`, so an unaligned `pos` reads the sector CONTAINING
/// pos and then writes those 2048 real bytes at byte offset `pos`, recording
/// them Finished: genuine payload at the wrong offset, marked good.
#[test]
fn unaligned_mapfile_ranges_never_produce_unaligned_records() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("unaligned.iso");
    let sectors: u32 = 300;
    let disc = make_test_disc(sectors, "Unaligned");
    let mf_path = disc.mapfile_for(&iso_path);

    {
        let mut mf = Mapfile::create(&mf_path, sectors as u64 * 2048, "test").unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
        // 512-aligned, NOT 2048-aligned — what ddrescue -b 512 produces.
        mf.record(100 * 2048 + 512, 1024, SectorStatus::NonTrimmed)
            .unwrap();
        mf.flush().unwrap();
    }
    std::fs::write(&iso_path, vec![0u8; sectors as usize * 2048]).unwrap();

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let popts = freemkv_engine::PatchOptions::for_patch_pass(false, None, None, None);
    freemkv_engine::patch(&disc, &mut reader, &iso_path, &popts).unwrap();

    let after = Mapfile::load(&mf_path).unwrap();
    for (pos, size) in after.ranges_with(&[SectorStatus::Finished]) {
        assert_eq!(
            pos % 2048,
            0,
            "Finished record starts at unaligned offset {pos} — a shifted write"
        );
        assert_eq!(size % 2048, 0, "Finished record has sub-sector size {size}");
    }
    // ...and the unaligned range must actually have been PROCESSED. The
    // alignment loop above is vacuously satisfied by the pre-existing aligned
    // prefix, so without this the test passed even if patch touched nothing.
    let recovered = after.ranges_with(&[SectorStatus::Finished]);
    assert!(
        recovered
            .iter()
            .any(|&(pos, size)| pos <= 100 * 2048 + 512 && pos + size >= 100 * 2048 + 512 + 1024),
        "the unaligned NonTrimmed range was never recovered — the alignment \
         assertions above only saw the pre-existing prefix: {recovered:?}"
    );
    // And the payload landed at the SNAPPED offset, not the raw byte offset:
    // a shifted write keeps the record aligned while putting bytes at 205312.
    let img = std::fs::read(&iso_path).unwrap();
    assert!(
        img[100 * 2048..101 * 2048].iter().any(|&b| b != 0),
        "the recovered sector is still all zeros — the write went somewhere else"
    );
}

/// A disc that reports itself encrypted but resolved NO cipher state at all
/// (`aacs: None`, `css: None` — scan sets `encrypted` from the presence of
/// /AACS, and leaves `aacs: None` with `aacs_error: Some(..)` when the VID
/// probe fails) must be REFUSED, not written out as ciphertext.
///
/// `ensure_decryptable` only errors when it has an aacs/css state to judge, so
/// this disc slipped through, the decrypt wrapper became a pass-through, and
/// the copy finished at `complete: true`, exit 0 — with an unplayable
/// ciphertext ISO on disk. Preflight blocks this disc; the executor didn't.
#[test]
fn encrypted_disc_with_no_cipher_state_is_refused_not_written_as_ciphertext() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("cipher.iso");
    let sectors: u32 = 300;
    let mut disc = make_test_disc(sectors, "NoCipherState");
    disc.encrypted = true;
    disc.aacs = None;
    disc.css = None;

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = CopyOptions {
        decrypt: true,
        multipass: true,
        ..Default::default()
    };
    let r = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);

    assert!(
        r.is_err(),
        "a decrypting copy of an encrypted disc with no usable cipher state \
         must refuse, not emit ciphertext at complete:true"
    );
    let wrote = std::fs::metadata(&iso_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wrote, 0, "nothing may be written before the refusal");
}

/// A mapfile left by a DIFFERENT disc of the same capacity must not be
/// resumed. Two box-set reprints authored at the same size satisfy the old
/// `total_size == capacity_bytes` gate, so disc A's Finished ranges were
/// trusted for disc B — never re-read — and the ISO silently spliced sectors
/// from two physical discs while passing every completeness check.
#[test]
fn mapfile_from_a_different_disc_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("swap.iso");
    let sectors: u32 = 300;

    // Disc A resolved one set of unit keys; disc B, same capacity, another.
    let mut disc_a = make_test_disc(sectors, "DiscA");
    disc_a.encrypted = true;
    disc_a.aacs = Some(aacs_with(vec![(0u32, [0xAA; 16])]));
    let mut disc_b = make_test_disc(sectors, "DiscB");
    disc_b.encrypted = true;
    disc_b.aacs = Some(aacs_with(vec![(0u32, [0xBB; 16])]));

    // A's mapfile, carrying A's identity and a Finished prefix.
    let mf_path = disc_a.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, sectors as u64 * 2048, "test").unwrap();
        mf.set_unit_keys(&[(0u32, [0xAA; 16])]);
        mf.record(0, 200 * 2048, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
    }
    std::fs::write(&iso_path, vec![0u8; sectors as usize * 2048]).unwrap();

    // Same capacity, so the size gate alone would let this through.
    assert_eq!(disc_a.capacity_bytes, disc_b.capacity_bytes);

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let r = freemkv_engine::copy(&disc_b, &mut reader, &iso_path, &opts);
    assert!(
        r.is_err(),
        "resuming disc A's mapfile against disc B must be refused, not spliced"
    );

    // The same guard must hold for a DIRECT patch() call. `copy` refuses
    // above because mod.rs checks identity before dispatching, but `patch` is
    // half of the exposed sweep/patch pair a front-end drives itself on a
    // resume, and it used to load the mapfile and trust it. Without this, disc
    // B's recovered ranges are written into disc A's ISO and recorded
    // Finished — corruption presented as a successful recovery, and no test
    // covered it.
    let patch_opts = freemkv_engine::PatchOptions::for_patch_pass(false, None, None, None);
    let r = freemkv_engine::patch(&disc_b, &mut reader, &iso_path, &patch_opts);
    assert!(
        r.is_err(),
        "patch() must refuse disc A's mapfile against disc B, exactly as copy() does"
    );
}

// ── Survivors from the full-crate mutation run, killed here ────────────────

/// A PLAIN copy (no `--multipass`) must ABORT on the first unreadable sector,
/// not zero-fill and carry on.
///
/// `Err(err) if !opts.skip_on_error => { producer_err = ...; break 'outer }` is
/// the whole behaviour of `disc:// -> iso://` without `--multipass`, because
/// `sweep_internal` sets `skip_on_error: opts.multipass`. The mutation run
/// forced that guard to `false` and the suite stayed green: the error then
/// falls into the recovery arm, so the sweep zero-fills the bad region, marks
/// it NonTrimmed, damage-jumps and returns Ok — a plain copy of a damaged disc
/// exits 0 with a holed ISO.
///
/// It survived because every `CopyOptions` in this file sets `multipass: true`
/// and every direct `SweepOptions` sets `skip_on_error: true`, so nothing ever
/// ran a sweep with `skip_on_error: false` over a bad sector.
#[test]
fn a_plain_copy_aborts_on_the_first_bad_sector_instead_of_holing_the_iso() {
    let sectors: u32 = 1000;
    let bad: std::collections::HashSet<u32> = [320u32].into_iter().collect();
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: bad,
    };
    let disc = make_test_disc(sectors, "PLAIN");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("plain.iso");
    let opts = SweepOptions {
        decrypt: false,
        resume: false,
        batch_sectors: None,
        skip_on_error: false, // plain copy: the first error is fatal
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    };

    let err = freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts)
        .expect_err("a plain copy must fail on an unreadable sector");
    assert!(
        matches!(err, libfreemkv::Error::DiscRead { .. }),
        "expected a DiscRead error, got {err:?}"
    );

    // And it must not have recorded damage-jump state: the producer aborted
    // before any NonTrimmed range was written.
    let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).expect("load mapfile");
    assert!(
        mf.ranges_with(&[SectorStatus::NonTrimmed]).is_empty(),
        "a plain copy must not damage-jump; it aborts"
    );
}

/// A RESUME must not truncate the image it is resuming into.
///
/// `if resume && existing_len.is_some_and(|len| len > 0)` chooses open-existing
/// over `File::create` + `set_len` — i.e. over truncation. The mutation run
/// changed `>` to `<` and to `==`; both send every resume down the
/// create-and-truncate branch, zeroing bytes the mapfile still records as
/// Finished. The producer only builds work from NonTried ranges, so those
/// bytes are never re-read: silent, total loss of the recovered image.
///
/// The existing resume tests could not catch it because they pre-fill the ISO
/// with ZEROS and assert only which LBAs were read — truncating zeros to zeros
/// is invisible. This one fills the recovered prefix with a recognisable
/// pattern instead.
#[test]
fn a_resume_does_not_truncate_the_already_recovered_prefix() {
    const SEC: u64 = libfreemkv::consts::SECTOR_BYTES_U64;
    let sectors: u32 = 400;
    let recovered_sectors: u32 = 100;

    let disc = make_test_disc(sectors, "RESUME");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("resume.iso");

    // A full-length ISO whose recovered prefix is 0xCC, not zeros.
    let total = sectors as u64 * SEC;
    let mut img = vec![0u8; total as usize];
    for b in img
        .iter_mut()
        .take((recovered_sectors as u64 * SEC) as usize)
    {
        *b = 0xCC;
    }
    std::fs::write(&iso_path, &img).unwrap();

    // A mapfile saying the prefix is Finished and the rest is NonTried.
    let mf_path = disc.mapfile_for(&iso_path);
    {
        let mut mf = Mapfile::create(&mf_path, total, "test").expect("create mapfile");
        mf.record(0, recovered_sectors as u64 * SEC, SectorStatus::Finished)
            .expect("record");
        mf.flush().expect("flush");
    }

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
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("resume sweep");

    let after = std::fs::read(&iso_path).expect("read iso");
    assert_eq!(after.len() as u64, total, "the ISO was resized");
    assert!(
        after[..(recovered_sectors as u64 * SEC) as usize]
            .iter()
            .all(|&b| b == 0xCC),
        "the already-recovered prefix was overwritten — the resume truncated it"
    );
}

// ─── Instrumented readers for the sweep's drive-facing decisions ────────────

/// One thing the sweep did to the DRIVE, in order.
///
/// Neither the read/retry lever nor the speed lever shows up in the ISO or the
/// mapfile, and the ORDER matters: "full speed was restored" is not the same
/// claim as "full speed was restored only after sixteen clean batches".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriveEvent {
    /// A read, and the `recovery` (in-drive retry) flag it carried.
    Read { ok: bool, recovery: bool },
    /// A `SET CD SPEED`.
    Speed(u16),
}

/// A `MockReader` that records the drive-facing side of the sweep.
struct InstrumentedReader {
    total_sectors: u32,
    bad_sectors: std::collections::HashSet<u32>,
    events: std::sync::Arc<std::sync::Mutex<Vec<DriveEvent>>>,
}

impl libfreemkv::sector::SectorSource for InstrumentedReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        for i in 0..count {
            if self.bad_sectors.contains(&(lba + i as u32)) {
                self.events.lock().unwrap().push(DriveEvent::Read {
                    ok: false,
                    recovery,
                });
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
        self.events
            .lock()
            .unwrap()
            .push(DriveEvent::Read { ok: true, recovery });
        let n = count as usize * 2048;
        buf[..n].fill(0xAA);
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.total_sectors
    }
    fn set_speed(&mut self, kbs: u16) {
        self.events.lock().unwrap().push(DriveEvent::Speed(kbs));
    }
}

/// A clean reader that raises a halt flag once it has served `after` reads, so
/// a test can stop a sweep partway through deterministically.
struct HaltingReader {
    total_sectors: u32,
    reads: u32,
    after: u32,
    halt: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl libfreemkv::sector::SectorSource for HaltingReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        let n = count as usize * 2048;
        buf[..n].fill(0xAA);
        self.reads += 1;
        if self.reads >= self.after {
            self.halt.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.total_sectors
    }
}

/// A reader that must never be asked for a sector.
struct NoReadsReader {
    total_sectors: u32,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl libfreemkv::sector::SectorSource for NoReadsReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::error::Result<usize> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let n = count as usize * 2048;
        buf[..n].fill(0xAA);
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.total_sectors
    }
}

const SEC: u64 = libfreemkv::consts::SECTOR_BYTES_U64;

fn plain_sweep_opts(resume: bool, skip_on_error: bool) -> SweepOptions<'static> {
    SweepOptions {
        decrypt: false,
        resume,
        batch_sectors: None,
        skip_on_error,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    }
}

fn multipass_copy_opts() -> CopyOptions<'static> {
    CopyOptions {
        decrypt: false,
        multipass: true,
        progress: None,
        halt: None,
        vid: None,
        unit_keys: Vec::new(),
        key_fetch: None,
    }
}

/// A FRESH sweep must truncate an image left over from a previous run.
///
/// The counterpart to the resume test above, and the other half of the same
/// condition: `resume && existing_len > 0`. Mutated to `||`, a fresh sweep
/// over a pre-existing image OPENS it instead of creating it — so wherever the
/// new sweep does not reach, the previous disc's bytes survive underneath and
/// are handed to the muxer as this disc's data.
///
/// The sweep is halted partway so there IS a region the new sweep never
/// reaches; a fresh create + `set_len` leaves that region zeroed.
#[test]
fn a_fresh_sweep_truncates_the_image_left_by_a_previous_run() {
    let sectors: u32 = 400;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "FRESH");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("fresh.iso");

    // The previous disc's image, every byte recognisable.
    std::fs::write(&iso_path, vec![0xFFu8; total as usize]).unwrap();

    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut reader = HaltingReader {
        total_sectors: sectors,
        reads: 0,
        after: 2,
        halt: halt.clone(),
    };
    let opts = SweepOptions {
        resume: false,
        halt: Some(halt.clone()),
        ..plain_sweep_opts(false, true)
    };
    let r = freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("fresh sweep");
    assert!(
        r.halted,
        "the premise of this test is a region the sweep never reaches — if it \
         ran to completion every byte was rewritten and the assertion below is \
         vacuous"
    );

    let after = std::fs::read(&iso_path).expect("read iso");
    assert_eq!(
        after.len() as u64,
        total,
        "a fresh sweep pre-sizes the image to the disc"
    );
    assert!(
        !after.contains(&0xFF),
        "the previous run's bytes survived a FRESH sweep — they would be muxed \
         as this disc's data"
    );
}

/// A resume into a zero-length image must still pre-size it.
///
/// A mapfile that is entirely NonTried claims no progress, so the
/// inconsistent-resume guard leaves `resume = true` even with a zero-length
/// ISO on disk. `existing_len > 0` is then what routes it to the create +
/// `set_len` branch; widened to `>=`, `Some(0)` takes the open-existing branch
/// and the pre-size never happens. A halt then leaves the image shorter than
/// the disc — and on the NEXT run the inconsistent-resume guard sees a short
/// ISO against a mapfile that now does claim progress, throws the whole
/// mapfile away, and re-rips from LBA 0.
#[test]
fn a_resume_into_an_empty_image_pre_sizes_it_to_the_disc() {
    let sectors: u32 = 400;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "PRESIZE");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("presize.iso");

    // Zero-length image + an all-NonTried mapfile (claims no progress).
    std::fs::write(&iso_path, b"").unwrap();
    {
        let mf = Mapfile::create(&disc.mapfile_for(&iso_path), total, "test").unwrap();
        drop(mf);
    }

    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut reader = HaltingReader {
        total_sectors: sectors,
        reads: 0,
        after: 2,
        halt: halt.clone(),
    };
    let opts = SweepOptions {
        halt: Some(halt.clone()),
        ..plain_sweep_opts(true, true)
    };
    let r = freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("resume sweep");
    assert!(r.halted, "the reader was supposed to halt this sweep");

    assert_eq!(
        std::fs::metadata(&iso_path).unwrap().len(),
        total,
        "a halted resume must still leave a full-length image — otherwise the \
         next run discards the mapfile and re-rips the whole disc"
    );
}

/// A resume whose image was DELETED must self-heal into a fresh sweep.
///
/// The inconsistent-resume guard reads the image length, and `NotFound` is the
/// one error that means "no file yet" — anything else aborts. Classify a
/// missing file as an unknown error and a resume whose ISO was cleaned up
/// returns an error instead of simply starting over.
///
/// The existing downgrade test writes a zero-LENGTH file, which exists; this
/// one removes it.
#[test]
fn a_resume_whose_image_was_deleted_starts_over_instead_of_erroring() {
    let sectors: u32 = 400;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "GONE");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("gone.iso");

    // A mapfile claiming real progress, and NO image at all.
    {
        let mut mf = Mapfile::create(&disc.mapfile_for(&iso_path), total, "test").unwrap();
        mf.record(0, 100 * SEC, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
    }
    assert!(!iso_path.exists());

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &plain_sweep_opts(true, true))
        .expect("a resume with no image self-heals into a fresh sweep");

    let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).unwrap();
    assert_eq!(
        mf.stats().bytes_good,
        total,
        "the whole disc was re-read, not just the NonTried tail"
    );
    assert_eq!(std::fs::metadata(&iso_path).unwrap().len(), total);
}

/// KEYS XOR VID: the mapfile header carries one or the other, never both.
///
/// A keyed disc writes its unit keys — the final answer, so deferred mux
/// decrypts directly with no key-service round trip. An unresolved disc writes
/// only the VID, the retry marker. Deleting the `!` swaps them: a keyed disc
/// records the VID and calls `set_unit_keys(&[])`, and deferred mux loses the
/// keys it was promised. `mapfile.rs` tests the setters; nothing tested the
/// wiring in `sweep`.
#[test]
fn the_mapfile_header_carries_the_unit_keys_when_there_are_keys() {
    let sectors: u32 = 64;
    let disc = make_test_disc(sectors, "KEYED");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("keyed.iso");
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = SweepOptions {
        vid: Some([0x11; 16]),
        unit_keys: vec![(0, [0xAB; 16])],
        ..plain_sweep_opts(false, true)
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("sweep");

    let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).unwrap();
    assert_eq!(
        mf.unit_keys(),
        &[(0u32, [0xABu8; 16])][..],
        "a keyed disc must carry its unit keys to deferred mux"
    );
    assert_eq!(
        mf.vid(),
        None,
        "keys are the final answer; the VID retry marker must not be written too"
    );
}

/// ...and the VID when there are none.
#[test]
fn the_mapfile_header_carries_the_vid_when_there_are_no_keys() {
    let sectors: u32 = 64;
    let disc = make_test_disc(sectors, "UNKEYED");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("unkeyed.iso");
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let opts = SweepOptions {
        vid: Some([0x11; 16]),
        unit_keys: Vec::new(),
        ..plain_sweep_opts(false, true)
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts).expect("sweep");

    let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).unwrap();
    assert_eq!(mf.vid(), Some([0x11u8; 16]));
    assert!(mf.unit_keys().is_empty());
}

/// The drive's in-drive retry lever is the INVERSE of skip-on-error.
///
/// Pass 1 of a multipass rip is "fast and accurate — get the most data in the
/// shortest time", so it asks the drive for fast-fail reads and handles damage
/// itself; a plain copy, which aborts on the first error, asks the drive to
/// retry hard before giving up. Invert `let recovery = !opts.skip_on_error`
/// and both modes get the wrong lever — a multipass Pass 1 crawls through
/// in-drive retries on every batch of a damaged disc. Invisible in the ISO and
/// the mapfile: it is a flag on a SCSI read.
#[test]
fn the_drive_retry_lever_is_the_inverse_of_skip_on_error() {
    let sectors: u32 = 128;
    let disc = make_test_disc(sectors, "LEVER");
    let tmp = tempfile::tempdir().unwrap();

    for (skip_on_error, want) in [(true, false), (false, true)] {
        let events: std::sync::Arc<std::sync::Mutex<Vec<DriveEvent>>> = Default::default();
        let mut reader = InstrumentedReader {
            total_sectors: sectors,
            bad_sectors: std::collections::HashSet::new(),
            events: events.clone(),
        };
        let iso_path = tmp.path().join(format!("lever-{skip_on_error}.iso"));
        freemkv_engine::sweep(
            &disc,
            &mut reader,
            &iso_path,
            &plain_sweep_opts(false, skip_on_error),
        )
        .expect("sweep");

        let seen: Vec<bool> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                DriveEvent::Read { recovery, .. } => Some(*recovery),
                DriveEvent::Speed(_) => None,
            })
            .collect();
        assert!(!seen.is_empty(), "the sweep read nothing");
        assert!(
            seen.iter().all(|&f| f == want),
            "skip_on_error={skip_on_error} must ask the drive for recovery={want}, \
             saw {seen:?}"
        );
    }
}

/// Damage slows the drive down, and a clean run brings it back up.
///
/// Entering a damage zone drops the drive to its minimum read speed, and
/// sixteen consecutive good batches restore maximum. Delete the `!` on the
/// entry check and the zone is never entered at all (the flag starts false),
/// so the drive is never slowed on damaged media and the whole recovery
/// behaviour quietly disappears — no test noticed, because a mock reader
/// ignores `set_speed`.
#[test]
fn damage_drops_the_drive_speed_and_a_clean_run_restores_it() {
    // Damage early, then a long clean tail so the exit threshold (16
    // consecutive good batches) is reached before EOF. The jump-ahead lands
    // well inside the disc rather than overshooting EOF.
    let sectors: u32 = 200_000;
    let bad: std::collections::HashSet<u32> = [320u32].into_iter().collect();
    let events: std::sync::Arc<std::sync::Mutex<Vec<DriveEvent>>> = Default::default();
    let mut reader = InstrumentedReader {
        total_sectors: sectors,
        bad_sectors: bad,
        events: events.clone(),
    };
    let disc = make_test_disc(sectors, "SLOW");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("slow.iso");
    freemkv_engine::sweep(
        &disc,
        &mut reader,
        &iso_path,
        &plain_sweep_opts(false, true),
    )
    .expect("sweep");

    let seen = events.lock().unwrap().clone();
    // The sweep asks for max speed once up front (riplock removal).
    assert_eq!(
        seen.first(),
        Some(&DriveEvent::Speed(0xFFFF)),
        "the sweep starts at full speed"
    );
    let slow_at = seen
        .iter()
        .position(|e| *e == DriveEvent::Speed(0x0000))
        .expect("damage must drop the drive to its minimum read speed");
    let restored_at = slow_at
        + 1
        + seen[slow_at + 1..]
            .iter()
            .position(|e| *e == DriveEvent::Speed(0xFFFF))
            .expect("full speed must be restored once the disc reads cleanly again");

    // The hysteresis is the point: the drive climbs back to full speed only
    // after DAMAGE_ZONE_EXIT_THRESHOLD (16) consecutive GOOD batches. Restore
    // it on the first clean read and the drive oscillates between minimum and
    // maximum across a damaged region, which is the behaviour the zone exists
    // to avoid.
    let good_between = seen[slow_at + 1..restored_at]
        .iter()
        .filter(|e| matches!(e, DriveEvent::Read { ok: true, .. }))
        .count();
    assert_eq!(
        good_between, 16,
        "the exit threshold is exactly 16 consecutive clean batches; a lower \
         count means the hysteresis was skipped and the drive will oscillate \
         across a damaged region, a higher one means it stayed slow too long"
    );
}

/// A finished rip with an intact image is a no-op — no reads, no re-write.
///
/// The dispatch shortcut is `covers_disc && bad_bytes == 0 && nontried == 0 &&
/// !iso_is_intact` for the repair path, and the same conjunction with
/// `iso_is_intact` for "already done". Loosen either and a COMPLETE rip with a
/// perfectly good ISO takes `sweep_internal(resume = false)` — which removes
/// the mapfile, `File::create`s the image and re-rips the whole disc, undoing
/// a finished job.
#[test]
fn re_issuing_a_finished_copy_reads_nothing_and_rewrites_nothing() {
    let sectors: u32 = 200;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "DONE");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("done.iso");

    // A complete mapfile and a full-length image with recognisable content.
    std::fs::write(&iso_path, vec![0x5Au8; total as usize]).unwrap();
    {
        let mut mf = Mapfile::create(&disc.mapfile_for(&iso_path), total, "test").unwrap();
        mf.record(0, total, SectorStatus::Finished).unwrap();
        mf.flush().unwrap();
    }

    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut reader = NoReadsReader {
        total_sectors: sectors,
        reads: reads.clone(),
    };
    let r = freemkv_engine::copy(&disc, &mut reader, &iso_path, &multipass_copy_opts())
        .expect("a finished copy is a no-op, not an error");

    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a finished rip must not touch the disc again"
    );
    assert_eq!(r.bytes_good, total);
    assert!(r.complete);
    assert!(
        std::fs::read(&iso_path).unwrap().iter().all(|&b| b == 0x5A),
        "the finished image was re-created and re-swept"
    );
}

/// An all-Unreadable disc is terminal — it must not be patched again.
///
/// Once every bad sector has been promoted to Unreadable there is nothing
/// retryable left, and the documented fallthrough returns the terminal result
/// immediately. `bytes_retryable > 0` mutated to `>= 0` is always true for a
/// u64, so that fallthrough never runs and every finished-but-lossy disc gets
/// one more patch pass — and `patch` selects `damage_sector_statuses()`, which
/// INCLUDES Unreadable, so it re-reads ranges the design considers permanently
/// lost.
#[test]
fn a_disc_whose_damage_is_all_permanent_is_not_patched_again() {
    let sectors: u32 = 200;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "TERMINAL");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("terminal.iso");

    std::fs::write(&iso_path, vec![0u8; total as usize]).unwrap();
    {
        let mut mf = Mapfile::create(&disc.mapfile_for(&iso_path), total, "test").unwrap();
        mf.record(0, 150 * SEC, SectorStatus::Finished).unwrap();
        mf.record(150 * SEC, 50 * SEC, SectorStatus::Unreadable)
            .unwrap();
        mf.flush().unwrap();
    }

    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut reader = NoReadsReader {
        total_sectors: sectors,
        reads: reads.clone(),
    };
    let r = freemkv_engine::copy(&disc, &mut reader, &iso_path, &multipass_copy_opts())
        .expect("a terminal result, not an error");

    assert_eq!(
        reads.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "permanently lost sectors must not be re-read"
    );
    assert_eq!(r.bytes_unreadable, 50 * SEC);
    assert_eq!(r.bytes_good, 150 * SEC);
    assert!(!r.complete, "a lossy rip is never complete");
}

/// Retryable bytes are not "no bad bytes", even when they exactly cancel.
///
/// `bad_bytes = bytes_pending + bytes_unreadable`, and the two are DISJOINT
/// counters — Unreadable feeds only `bytes_unreadable`, while NonTried /
/// NonTrimmed / NonScraped feed `bytes_pending`. Turn the `+` into a `-` and
/// the two cancel whenever they happen to be equal, so `bad_bytes == 0` and
/// the dispatch takes the "already complete" shortcut instead of routing to
/// `patch`: retryable bytes silently abandoned and the rip reported terminal.
#[test]
fn equal_pending_and_unreadable_counts_still_route_to_a_patch_pass() {
    let sectors: u32 = 200;
    let total = sectors as u64 * SEC;
    let disc = make_test_disc(sectors, "CANCEL");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("cancel.iso");

    std::fs::write(&iso_path, vec![0u8; total as usize]).unwrap();
    {
        // 50 sectors Unreadable and 50 NonTrimmed: pending == unreadable, so a
        // subtraction gives zero.
        let mut mf = Mapfile::create(&disc.mapfile_for(&iso_path), total, "test").unwrap();
        mf.record(0, 100 * SEC, SectorStatus::Finished).unwrap();
        mf.record(100 * SEC, 50 * SEC, SectorStatus::Unreadable)
            .unwrap();
        mf.record(150 * SEC, 50 * SEC, SectorStatus::NonTrimmed)
            .unwrap();
        mf.flush().unwrap();
    }
    {
        let mf = Mapfile::load(&disc.mapfile_for(&iso_path)).unwrap();
        let st = mf.stats();
        assert_eq!(
            st.bytes_pending, st.bytes_unreadable,
            "fixture precondition: the two counters must be equal, or the \
             subtraction would not cancel"
        );
        assert!(st.bytes_nontried == 0);
    }

    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    let r =
        freemkv_engine::copy(&disc, &mut reader, &iso_path, &multipass_copy_opts()).expect("copy");

    assert!(
        r.recovered_this_pass > 0,
        "the NonTrimmed range must be handed to a patch pass, not written off"
    );
    assert!(
        r.bytes_good >= 150 * SEC,
        "the recovered range must be counted good, got {}",
        r.bytes_good
    );
}

/// A fresh sweep over another disc's mapfile drops it and sweeps.
///
/// `if resume && mapfile_path.exists()` guards the identity check. Widened to
/// `||`, a FRESH sweep runs the identity check against the leftover mapfile
/// and errors out — a new disc in the drive after a previous rip refuses to
/// start, instead of doing the obvious thing and starting over. The existing
/// identity test only covers the resume path, which is the direction that must
/// error.
#[test]
fn a_fresh_sweep_over_a_different_discs_mapfile_starts_over() {
    let sectors: u32 = 128;
    let total = sectors as u64 * SEC;
    let disc_a = make_test_disc(sectors, "DISC-A");
    let disc_b = make_test_disc(sectors, "DISC-B");
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("swap.iso");

    // Disc A finished here; the mapfile path is the same for both discs.
    assert_eq!(disc_a.mapfile_for(&iso_path), disc_b.mapfile_for(&iso_path));
    {
        let mut reader = MockReader {
            total_sectors: sectors,
            bad_sectors: std::collections::HashSet::new(),
        };
        // A's volume id goes into the mapfile header — that is what makes the
        // leftover mapfile identifiably A's rather than anonymous.
        let a_opts = SweepOptions {
            vid: Some([0xAA; 16]),
            ..plain_sweep_opts(false, true)
        };
        freemkv_engine::sweep(&disc_a, &mut reader, &iso_path, &a_opts).expect("disc A sweep");
        let a_map = Mapfile::load(&disc_a.mapfile_for(&iso_path)).unwrap();
        assert_eq!(
            a_map.vid(),
            Some([0xAAu8; 16]),
            "fixture precondition: the leftover mapfile must carry A's identity"
        );
    }

    // Now disc B, fresh. It must drop A's mapfile rather than compare against
    // it.
    let mut reader = MockReader {
        total_sectors: sectors,
        bad_sectors: std::collections::HashSet::new(),
    };
    freemkv_engine::sweep(
        &disc_b,
        &mut reader,
        &iso_path,
        &plain_sweep_opts(false, true),
    )
    .expect("a FRESH sweep must drop the previous disc's mapfile, not refuse");

    let mf = Mapfile::load(&disc_b.mapfile_for(&iso_path)).unwrap();
    assert_eq!(
        mf.vid(),
        None,
        "A's mapfile must have been dropped, not inherited"
    );
    assert_eq!(mf.stats().bytes_good, total);
}

/// A DECRYPTING sweep must actually decrypt — and the crate never once ran one.
///
/// Every `CopyOptions`/`SweepOptions` in this suite sets `decrypt: false`, and
/// the two AACS fixtures are refused pre-flight, so the whole decrypt-wiring
/// triangle at the top of `sweep` — resolve a whole-disc AACS key map, or fall
/// back to the CSS self-descramble path — was unexercised. Widening
/// `opts.decrypt && decrypt_is_aacs` to `||` installs an AACS key map on a CSS
/// disc, and a `Some(key_map)` takes the mapped early-return in
/// `DecryptingSectorSource` and never reaches the CSS descramble at all: a
/// `--decrypt` CSS rip writes scrambled bytes to the ISO and exits 0. That is
/// exactly the silent-garbage-success this file's pre-flight gate exists to
/// stop, arriving one layer below the gate.
///
/// A CSS sector carries its own scramble flag in bits 4-5 of byte 0x14, so the
/// fixture can mark some sectors scrambled and leave others clear and the
/// assertion is simply: the scrambled ones changed, the clear ones did not.
#[test]
fn a_decrypting_css_sweep_descrambles_the_scrambled_sectors() {
    /// Deterministic per-sector content, before any descrambling.
    fn plain_sector(lba: u32, scrambled: bool) -> [u8; 2048] {
        let mut s = [0u8; 2048];
        for (i, b) in s.iter_mut().enumerate() {
            *b = (lba as u8)
                .wrapping_mul(7)
                .wrapping_add((i as u8).wrapping_mul(31));
        }
        // Scrambled DVD sectors are MPEG-2 PS packs, and the descramble policy
        // requires the pack start code as well as the flag bits — byte 0x14
        // alone means nothing in an IFO or UDF sector, so it is not sufficient
        // on its own to authorise descrambling.
        s[0x00..0x04].copy_from_slice(&[0x00, 0x00, 0x01, 0xBA]);
        // Bits 4-5 of the sub-header byte are the CSS scramble flag.
        s[0x14] &= !0x30;
        if scrambled {
            s[0x14] |= 0x30;
        }
        s
    }

    struct CssReader {
        total_sectors: u32,
    }
    fn is_scrambled_lba(lba: u32) -> bool {
        (32..64).contains(&lba)
    }
    impl libfreemkv::sector::SectorSource for CssReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::error::Result<usize> {
            for i in 0..count as u32 {
                let s = plain_sector(lba + i, is_scrambled_lba(lba + i));
                let off = i as usize * 2048;
                buf[off..off + 2048].copy_from_slice(&s);
            }
            Ok(count as usize * 2048)
        }
        fn capacity_sectors(&self) -> u32 {
            self.total_sectors
        }
    }

    let sectors: u32 = 96;
    let mut disc = make_test_disc(sectors, "CSSDISC");
    disc.format = DiscFormat::Dvd;
    disc.content_format = ContentFormat::MpegPs;
    disc.encrypted = true;
    disc.css = Some(libfreemkv::css::CssState {
        title_key: [0x11, 0x22, 0x33, 0x44, 0x55],
        crack_span: None,
    });

    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("css.iso");
    let mut reader = CssReader {
        total_sectors: sectors,
    };
    let opts = SweepOptions {
        decrypt: true, // NOT --raw: this rip must produce plaintext
        ..plain_sweep_opts(false, true)
    };
    freemkv_engine::sweep(&disc, &mut reader, &iso_path, &opts)
        .expect("a CSS disc with a title key is decryptable");

    let img = std::fs::read(&iso_path).expect("read iso");
    assert_eq!(img.len() as u64, sectors as u64 * SEC);

    let mut changed = 0usize;
    for lba in 0..sectors {
        let got = &img[(lba as u64 * SEC) as usize..((lba as u64 + 1) * SEC) as usize];
        let raw = plain_sector(lba, is_scrambled_lba(lba));
        if is_scrambled_lba(lba) {
            if got != raw.as_slice() {
                changed += 1;
            }
        } else {
            assert_eq!(
                got,
                raw.as_slice(),
                "clear sector {lba} was altered — a decrypting sweep must pass \
                 unscrambled filesystem/nav sectors through untouched"
            );
        }
    }
    assert_eq!(
        changed, 32,
        "every scrambled sector must have been descrambled on the way to the ISO"
    );
}
