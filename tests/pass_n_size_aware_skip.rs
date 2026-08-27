//! Pass N (`freemkv_engine::patch`) size-aware-skip targeted tests.
//!
//! The user's failure mode (2026-05-07): "what if we have a 100 sector zone
//! and its really 2 25 sector zones and we keep jumping over the good in
//! the middle." Today's pre-fix patch escalates skip-distance based on
//! `consecutive_skips_without_recovery` with hardcoded 32 → 4096 sector
//! caps. A 100-sector bad range whose actual layout is 25 bad + 50 good +
//! 25 bad would have the patch skip 32-4096 sectors after a couple of
//! failures, leaping over the entire range AND the good middle.
//!
//! The fix AT THE TIME: cap each skip at `range_remaining/4`.
//!
//! HISTORY, so nobody goes looking for that cap: `range_remaining`,
//! `compute_damage_skip` and `MAX_SKIPS_PER_RANGE` do not exist in `src/` any
//! more. The per-section handler chain in `recovery/section_recover.rs`
//! (driven per bad range by `recovery/patch.rs`) replaced that loop wholesale,
//! and it is what recovers the good middle of a bad range today — by walking
//! the range from several angles under a time budget, not by bounding a skip
//! distance. The same note is on `tests/passn_handler_ab.rs`, the sibling
//! fixture from the same rework.
//!
//! What survived, and what these tests still pin, is the OBSERVABLE contract:
//! a 100-sector bad range whose middle 50 sectors are readable must come back
//! with that middle recovered, not leapt over. That assertion is independent of
//! which mechanism does it, which is why the file kept its value when the
//! mechanism it was named for was deleted.

use freemkv_engine::CopyOptions;
use freemkv_engine::PatchOptions;
use freemkv_engine::{Mapfile, SectorStatus};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::Result;
use libfreemkv::{ContentFormat, Disc, DiscFormat, SectorSource};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const SECTOR_SIZE: usize = 2048;

/// Reader where you specify exactly which LBAs return Err. Everything else
/// returns Ok with the LBA encoded in each byte for verification.
struct PatternedSectorReader {
    capacity: u32,
    bad_lbas: HashSet<u32>,
    /// Trace every read so tests can assert what was actually attempted.
    trace: Arc<Mutex<Vec<(u32, u16)>>>,
    /// Optional WORK-budget watchdog: raise this halt flag once the pass has
    /// issued `budget` reads. A deterministic stand-in for a wall-clock
    /// watchdog thread — a loop that fails to advance burns reads at whatever
    /// speed the machine runs at, so counting reads bounds it identically on a
    /// fast laptop and a contended CI runner, and a healthy run (which needs a
    /// handful) can never trip it by being slow.
    halt_after_reads: Option<(u64, Arc<std::sync::atomic::AtomicBool>)>,
}

type ReadTrace = Arc<Mutex<Vec<(u32, u16)>>>;

impl PatternedSectorReader {
    fn new(capacity: u32, bad_lbas: HashSet<u32>) -> (Self, ReadTrace) {
        let trace = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                capacity,
                bad_lbas,
                trace: trace.clone(),
                halt_after_reads: None,
            },
            trace,
        )
    }

    fn halt_after_reads(mut self, budget: u64, halt: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.halt_after_reads = Some((budget, halt));
        self
    }
}

impl SectorSource for PatternedSectorReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        let reads = {
            let mut t = self.trace.lock().unwrap();
            t.push((lba, count));
            t.len() as u64
        };
        if let Some((budget, halt)) = &self.halt_after_reads
            && reads >= *budget
        {
            halt.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Whole-batch fails if ANY sector in the batch is bad. (Models a
        // real drive: a multi-sector READ aborts on the first ECC failure.)
        for offset in 0..count as u32 {
            if self.bad_lbas.contains(&(lba + offset)) {
                return Err(libfreemkv::error::Error::ScsiError {
                    opcode: libfreemkv::scsi::SCSI_READ_10,
                    status: libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION,
                    sense: Some(libfreemkv::ScsiSense {
                        sense_key: libfreemkv::scsi::SENSE_KEY_MEDIUM_ERROR,
                        asc: 0x11,
                        ascq: 0x00,
                    }),
                });
            }
        }
        // Fill each sector with ITS OWN LBA byte (not the starting LBA's),
        // matching real multi-sector READ behavior; adaptive batching needs
        // the per-sector pattern to verify correct positioning.
        for (i, chunk) in buf.chunks_mut(SECTOR_SIZE).enumerate() {
            chunk.fill(((lba + i as u32) & 0xff) as u8);
        }
        Ok(buf.len())
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

fn synthetic_disc(capacity_sectors: u32) -> Disc {
    Disc {
        volume_id: String::new(),
        meta_title: None,
        format: DiscFormat::BluRay,
        capacity_sectors,
        capacity_bytes: capacity_sectors as u64 * SECTOR_SIZE as u64,
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

/// Pre-populate a mapfile with one large NonTrimmed range so patch's work-
/// list has something to do. Caller pre-allocates the ISO at `total_bytes`
/// so seeks don't fail.
fn prep_iso_and_mapfile(
    iso_path: &std::path::Path,
    total_bytes: u64,
    finished_ranges: &[(u64, u64)],
    nontrimmed_ranges: &[(u64, u64)],
) {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(iso_path)
        .unwrap();
    f.set_len(total_bytes).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&[]).unwrap();

    let map_path = freemkv_engine::mapfile_path_for(iso_path);
    let mut mf = Mapfile::create(&map_path, total_bytes, "test").unwrap();
    for &(pos, size) in finished_ranges {
        mf.record(pos, size, SectorStatus::Finished).unwrap();
    }
    for &(pos, size) in nontrimmed_ranges {
        mf.record(pos, size, SectorStatus::NonTrimmed).unwrap();
    }
}

/// THE critical test. A 100-sector "bad" range hides 50 good sectors in
/// the middle (LBAs 125-174). The pre-fix patch loop would skip-escalate at
/// 32+ sectors and leap over the whole range, good middle included. The
/// recovered middle below is the contract; the `range_remaining/4` cap that
/// first delivered it is long gone (see this file's header) and the handler
/// chain delivers it now.
#[test]
fn patch_recovers_good_middle_of_a_bad_range() {
    let capacity_sectors: u32 = 1024;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    // Bad range layout: LBAs 100-124 bad, 125-174 GOOD, 175-199 bad.
    let mut bad_lbas = HashSet::new();
    for lba in 100..125 {
        bad_lbas.insert(lba);
    }
    for lba in 175..200 {
        bad_lbas.insert(lba);
    }

    let (mut reader, _trace) = PatternedSectorReader::new(capacity_sectors, bad_lbas);
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    // Pre-populate: 0..100 already Finished from an imagined Pass 1,
    //               100..200 NonTrimmed (the range we want patch to retry),
    //               200..1024 already Finished.
    let finished = [
        (0, 100 * 2048),
        (200 * 2048, (capacity_sectors as u64 - 200) * 2048),
    ];
    let nontrimmed = [(100 * 2048, 100 * 2048)];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    // Run patch.
    // freemkv_engine::copy() with multipass=true auto-dispatches to patch when the
    // mapfile already covers the disc and has retryable ranges.
    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let pr = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy returns Ok");

    // Re-load mapfile and inspect.
    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let map = Mapfile::load(&map_path).unwrap();

    // The good middle (125..175) MUST end up Finished. If size-aware skip
    // is not enabled, patch would skip 32+ sectors after a few failures
    // and leap clean over LBA 125 → middle stays NonTrimmed.
    let finished_ranges = map.ranges_with(&[SectorStatus::Finished]);
    let total_finished_in_middle: u64 = finished_ranges
        .iter()
        .map(|&(pos, sz)| {
            let start = pos.max(125 * 2048);
            let end = (pos + sz).min(175 * 2048);
            end.saturating_sub(start)
        })
        .sum();

    // Allow 2 sectors (4 KB) of boundary slop — bisection may not converge
    // exactly on the good/bad boundary in one pass. Pre-fix behaviour would
    // have left the entire good middle NonTrimmed (~0 bytes recovered).
    let good_middle_bytes: u64 = 50 * 2048;
    let min_acceptable: u64 = good_middle_bytes - 2 * 2048;

    // Cleanup before assertions
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);

    assert!(
        total_finished_in_middle >= min_acceptable,
        "size-aware skip should have discovered most of the 50 good sectors in the middle. \
         Recovered {} of {} good middle bytes (min acceptable {}). bytes_good={} bytes_total={}",
        total_finished_in_middle,
        good_middle_bytes,
        min_acceptable,
        pr.bytes_good,
        pr.bytes_total,
    );
}

/// Regression: `PatchOptions::block_sectors == Some(0)` must not
/// busy-spin. `block_sectors` is a public `Option<u16>` field; a zero
/// value would compute a zero-length read every iteration, never
/// advance `block_end`, and burn a CPU core until the per-range
/// watchdog fired (up to 30 min on a large range). The entry-point
/// `.max(1)` clamp turns Some(0) into a single-sector batch so the
/// range recovers and the call returns promptly.
#[test]
fn patch_block_sectors_zero_does_not_busy_spin() {
    let capacity_sectors: u32 = 256;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    // Bound this in WORK, not wall-clock seconds: a watchdog thread made a
    // slow runner indistinguishable from the regression. A read budget catches
    // an unbounded loop just as surely — 512 reads vs. the 3 a healthy run needs.
    const READ_BUDGET: u64 = 512;

    // Small NonTrimmed range that is entirely readable (no bad LBAs), so
    // single-sector patch reads recover it immediately. Without the
    // clamp the loop would never progress regardless of readability.
    let halt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (reader, trace) = PatternedSectorReader::new(capacity_sectors, HashSet::new());
    let mut reader = reader.halt_after_reads(READ_BUDGET, halt.clone());
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let finished = [
        (0, 100 * 2048),
        (110 * 2048, (capacity_sectors as u64 - 110) * 2048),
    ];
    let nontrimmed = [(100 * 2048, 10 * 2048)];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    // Capture the pass label the operator would see: the clamp drives it
    // end-to-end (`.max(1)` → 1-sector batch → SCRAPE); an unclamped
    // `Some(0)` would render the same pass as a TRIM instead.
    #[derive(Default)]
    struct KindReporter {
        kinds: Mutex<Vec<libfreemkv::progress::PassKind>>,
    }
    impl libfreemkv::progress::Progress for KindReporter {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            self.kinds.lock().unwrap().push(p.kind);
            true
        }
    }
    let reporter = KindReporter::default();

    let opts = PatchOptions {
        decrypt: false,
        block_sectors: Some(0),
        full_recovery: false,
        reverse: false,
        wedged_threshold: 0,
        progress: Some(&reporter),
        halt: Some(halt.clone()),
        key_fetch: None,
    };

    let outcome = freemkv_engine::patch(&disc, &mut reader, &iso_path, &opts);

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);

    let outcome = outcome.expect("patch returns Ok");
    let reads = trace.lock().unwrap().clone();
    assert!(
        !outcome.halted,
        "patch with block_sectors=Some(0) must complete on its own \
         (clamped to a 1-sector batch), not burn through the {READ_BUDGET}-read \
         budget; it issued {} reads",
        reads.len()
    );
    // The direct signature of the regression: an unclamped `Some(0)` sizes
    // reads at zero sectors, so the cursor never advances. Caught here by
    // inspecting the trace, not by how long the test happened to take.
    assert!(
        reads.iter().all(|&(_, count)| count >= 1),
        "every read must cover at least one sector; trace = {reads:?}"
    );
    let kinds = reporter.kinds.lock().unwrap().clone();
    assert!(
        !kinds.is_empty()
            && kinds
                .iter()
                .all(|k| matches!(k, libfreemkv::progress::PassKind::Scrape { reverse: false })),
        "Some(0) must be clamped to a 1-sector batch, which the pass reports as \
         a forward SCRAPE; got {kinds:?}"
    );
    let bytes_good = outcome.bytes_good;
    // The 10-sector NonTrimmed range was fully readable; clamped to a
    // 1-sector batch it must recover. Initial good = 100 + (256-110) =
    // 246 sectors; after patch the 10-sector range is also Finished.
    let initial_good_sectors: u64 = 100 + (capacity_sectors as u64 - 110);
    assert!(
        bytes_good >= (initial_good_sectors + 10) * 2048,
        "block_sectors=Some(0) clamped to 1 should recover the readable range; \
         bytes_good={bytes_good}"
    );
}

/// A second test: a bad range that's actually 4 small bad sub-zones
/// separated by good sectors. Demonstrates the bisection behaviour
/// converges when zones are non-uniform.
#[test]
fn patch_recovers_multiple_good_middles() {
    let capacity_sectors: u32 = 2048;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    // Bad pattern: 1000-1024 bad, 1025-1099 good, 1100-1124 bad,
    //              1125-1199 good, 1200-1224 bad, 1225-1299 good.
    let mut bad_lbas = HashSet::new();
    for lba in 1000..1025 {
        bad_lbas.insert(lba);
    }
    for lba in 1100..1125 {
        bad_lbas.insert(lba);
    }
    for lba in 1200..1225 {
        bad_lbas.insert(lba);
    }
    let (mut reader, _trace) = PatternedSectorReader::new(capacity_sectors, bad_lbas);
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let finished = [
        (0, 1000 * 2048),
        (1300 * 2048, (capacity_sectors as u64 - 1300) * 2048),
    ];
    let nontrimmed = [(1000 * 2048, 300 * 2048)];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let pr = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy returns Ok");

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let map = Mapfile::load(&map_path).unwrap();
    let finished_ranges = map.ranges_with(&[SectorStatus::Finished]);
    let recovered: u64 = finished_ranges
        .iter()
        .map(|&(pos, sz)| {
            let start = pos.max(1000 * 2048);
            let end = (pos + sz).min(1300 * 2048);
            end.saturating_sub(start)
        })
        .sum();

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);

    // Three good middles of 75 sectors each = 225 good sectors in the
    // bad range. Total bad = 75. So we want at least most of 225 sectors
    // (= 460800 bytes) to be Finished after patch.
    let target = 200 * 2048; // be generous — anything over 200 sectors is convincing
    assert!(
        recovered >= target,
        "size-aware skip should find most of the 3 good middles. \
         Recovered {} bytes; expected ≥ {}. bytes_good={} bytes_total={}",
        recovered,
        target,
        pr.bytes_good,
        pr.bytes_total,
    );
}

/// 0.18 Pass N pipeline split: exercises the new producer/consumer
/// path end-to-end on a synthetic patterned reader. Bad range layout
/// is small (5 bad LBAs surrounded by good middle) so the producer
/// emits a mix of `Recovered` and `NonTrimmed` items and the consumer
/// thread must apply both kinds. Verifies:
///
/// - `bytes_good` advances (good sectors flow producer→consumer→file
///   →mapfile with the data preserved).
/// - The recovered LBAs end up Finished; the bad LBAs end up NonTrimmed
///   (NOT Unreadable — promotion to Unreadable is the orchestrator's job
///   after the final pass).
/// - Bytes written at the recovered offsets match what the producer
///   read from the patterned source (proves the channel hand-off
///   didn't drop or reorder buffers, and the consumer's seek+write
///   landed at the right offsets).
#[test]
fn patch_pipeline_split_recovers_and_records_correctly() {
    let capacity_sectors: u32 = 512;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    // Layout: LBAs 200-204 inclusive are bad (5 sectors), 205-249 good.
    // The pre-existing range is LBAs 200-249 NonTrimmed (100 KB).
    let mut bad_lbas = HashSet::new();
    for lba in 200..205 {
        bad_lbas.insert(lba);
    }

    let (mut reader, _trace) = PatternedSectorReader::new(capacity_sectors, bad_lbas.clone());
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let finished = [
        (0, 200 * 2048),
        (250 * 2048, (capacity_sectors as u64 - 250) * 2048),
    ];
    let nontrimmed = [(200 * 2048, 50 * 2048)];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    let pr = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy returns Ok");

    // bytes_good should advance: the good LBAs in the bad range (205-249,
    // 45 sectors) are reachable via per-sector retry. Initial good = 462
    // sectors; after patch it should be ≥ 462 + 45 = 507 sectors worth.
    let initial_good_sectors: u64 = 200 + (capacity_sectors as u64 - 250);
    let min_expected_good_bytes = (initial_good_sectors + 30) * 2048;
    assert!(
        pr.bytes_good >= min_expected_good_bytes,
        "patch should have recovered most good LBAs in the bad range via the pipeline. \
         bytes_good={} (expected ≥ {}); bytes_total={}",
        pr.bytes_good,
        min_expected_good_bytes,
        pr.bytes_total,
    );

    // Verify the mapfile records: every good LBA is Finished, every
    // bad LBA is NonTrimmed (not Finished).
    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let map = Mapfile::load(&map_path).unwrap();
    let finished_ranges = map.ranges_with(&[SectorStatus::Finished]);
    let in_finished = |lba: u32| -> bool {
        let pos = lba as u64 * 2048;
        finished_ranges
            .iter()
            .any(|&(p, sz)| pos >= p && pos < p + sz)
    };

    for lba in 205..250 {
        assert!(
            in_finished(lba),
            "good LBA {lba} should be Finished after pipeline patch run"
        );
    }
    for lba in 200..205 {
        assert!(
            !in_finished(lba),
            "bad LBA {lba} should NOT be Finished after pipeline patch run"
        );
    }

    // Verify the consumer wrote the producer's bytes at the right offsets:
    // PatternedSectorReader fills each sector with `(lba & 0xff) as u8`, so
    // LBA 220 (inside the recovered region) gives a clean signature to check.
    use std::io::{Read, Seek, SeekFrom};
    let mut iso = std::fs::File::open(&iso_path).unwrap();
    iso.seek(SeekFrom::Start(220 * 2048)).unwrap();
    let mut sector = [0u8; 2048];
    iso.read_exact(&mut sector).unwrap();
    let expected_byte = (220u32 & 0xff) as u8;

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);

    assert!(
        sector.iter().all(|&b| b == expected_byte),
        "consumer should have written PatternedSectorReader's pattern \
         (byte {expected_byte:#x} for LBA 220) to the recovered offset; \
         got first 8 bytes = {:?}",
        &sector[..8]
    );
}
