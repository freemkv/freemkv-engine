//! Pass-N (`freemkv_engine::patch`) read-error handler — A/B golden fixture.
//!
//! Pins the end-to-end behaviour of `freemkv_engine::patch` over eight
//! canonical damage profiles against a synthetic `ScriptedSectorReader`:
//! final mapfile byte counts plus an upper bound on read count, so any
//! change to the Pass-N failure path preserves the goldens or fails loudly.
//! Pass N's recovery is the handler chain in `recovery/section_recover.rs`
//! driven by `recovery/patch.rs` — NOT `read_error::handle_read_error`.
//! See docs/passn-handler-ab.md for the full split and golden-value history.

use freemkv_engine::CopyOptions;
use freemkv_engine::{Mapfile, SectorStatus};
use libfreemkv::ContentFormat;
use libfreemkv::Disc;
use libfreemkv::DiscFormat;
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{Error, Result};
use libfreemkv::scsi;
use libfreemkv::{ScsiSense, SectorSource};
use std::sync::{Arc, Mutex};

const SECTOR_SIZE: usize = 2048;

// Per-attempt result the script can emit. `Ok` returns a deterministic
// per-sector byte pattern (LBA mod 256). `Err` returns the SCSI sense
// triple; the patch failure path classifies on `sense_key`.
#[derive(Debug, Clone, Copy)]
enum ScriptStep {
    Ok,
    Err { sense_key: u8, asc: u8, ascq: u8 },
}

// Scripted reader: per (lba, count) attempt, picks `attempt_idx[lba]`'s
// step and advances it (missing entries default `Ok`). Batch fails if
// ANY sector is bad, synthesizing an Err with the FIRST scripted failure.
struct ScriptedSectorReader {
    capacity: u32,
    /// Per-LBA script of (step, then next step on retry, …). When
    /// retries exhaust the script, the LAST step repeats forever.
    script: std::collections::HashMap<u32, Vec<ScriptStep>>,
    /// Per-LBA index into its script vec. Bumps on each read attempt
    /// at that LBA.
    attempt_idx: Mutex<std::collections::HashMap<u32, usize>>,
    /// Full read trace: every (lba, count, result_was_ok) tuple in
    /// call order. Lets the test assert that adaptive-batch dropped
    /// to count=1, bisection happened, etc.
    trace: Arc<Mutex<Vec<(u32, u16, bool)>>>,
}

/// A `ScriptedSectorReader` plus the handle recording its `(lba, count, ok)` trace.
type ScriptedHarness = (ScriptedSectorReader, Arc<Mutex<Vec<(u32, u16, bool)>>>);

impl ScriptedSectorReader {
    fn new(capacity: u32) -> ScriptedHarness {
        let trace = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                capacity,
                script: std::collections::HashMap::new(),
                attempt_idx: Mutex::new(std::collections::HashMap::new()),
                trace: trace.clone(),
            },
            trace,
        )
    }

    /// Set a single-step script for `lba`: every attempt yields `step`.
    fn always(&mut self, lba: u32, step: ScriptStep) {
        self.script.insert(lba, vec![step]);
    }

    /// Set a multi-step script for `lba`: first attempt yields
    /// `steps[0]`, second `steps[1]`, … on retry the last step repeats.
    fn sequence(&mut self, lba: u32, steps: Vec<ScriptStep>) {
        self.script.insert(lba, steps);
    }

    fn step_for(&self, lba: u32) -> ScriptStep {
        let v = match self.script.get(&lba) {
            Some(v) => v,
            None => return ScriptStep::Ok,
        };
        let mut idx = self.attempt_idx.lock().unwrap();
        let i = idx.entry(lba).or_insert(0);
        let step = v[(*i).min(v.len() - 1)];
        *i += 1;
        step
    }
}

impl SectorSource for ScriptedSectorReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        // Look at every sector in the batch — first failure determines
        // the outcome.
        let mut failure: Option<(u8, u8, u8)> = None;
        for offset in 0..count as u32 {
            match self.step_for(lba + offset) {
                ScriptStep::Ok => {}
                ScriptStep::Err {
                    sense_key,
                    asc,
                    ascq,
                } => {
                    failure = Some((sense_key, asc, ascq));
                    break;
                }
            }
        }
        let ok = failure.is_none();
        self.trace.lock().unwrap().push((lba, count, ok));
        if let Some((sense_key, asc, ascq)) = failure {
            return Err(Error::ScsiError {
                opcode: scsi::SCSI_READ_10,
                status: scsi::SCSI_STATUS_CHECK_CONDITION,
                sense: Some(ScsiSense {
                    sense_key,
                    asc,
                    ascq,
                }),
            });
        }
        // Per-sector LBA byte pattern.
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

/// Observable outcome of a patch run. Goldens for each profile pin
/// these exact values.
#[derive(Debug, PartialEq, Eq)]
struct Golden {
    /// `bytes_good` at end of patch.
    bytes_good: u64,
    /// `bytes_unreadable` at end.
    bytes_unreadable: u64,
    /// `bytes_pending` (NonTrimmed) at end.
    bytes_pending: u64,
    /// Sanity bound on trace length — patch makes a finite number of reads,
    /// bounded today by the per-handler wall-clock deadlines and the fixed
    /// handler chain rather than by any skip count. Asserted as an UPPER bound
    /// only (so any reduction in retries via future tuning doesn't fail the
    /// test spuriously).
    max_reads: usize,
}

/// Common helper: prep ISO + mapfile, run `freemkv_engine::copy` (multipass),
/// return (PatchOutcome ↔ CopyResult, final-map stats, trace length).
fn run_profile(
    profile_name: &str,
    capacity_sectors: u32,
    nontrimmed: &[(u64, u64)],
    finished: &[(u64, u64)],
    scripted: ScriptedSectorReader,
    trace: Arc<Mutex<Vec<(u32, u16, bool)>>>,
) -> (freemkv_engine::CopyResult, freemkv_engine::MapStats, usize) {
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    prep_iso_and_mapfile(&iso_path, total_bytes, finished, nontrimmed);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };

    let mut reader = scripted;
    let pr = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts)
        .unwrap_or_else(|e| panic!("[{profile_name}] disc.copy returned Err: {e:?}"));

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let map = Mapfile::load(&map_path).unwrap();
    let stats = map.stats();

    let trace_len = trace.lock().unwrap().len();

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);

    (pr, stats, trace_len)
}

// ─────────────────────────── Profile 1: CLEAN ────────────────────────────
// Zero scripted failures: patch marches through and marks Finished.
// Validates the happy-path side of the failure-handler dispatch.

#[test]
fn profile_01_clean_all_recoverable() {
    let capacity_sectors: u32 = 256;
    let (reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    // No scripted errors → all reads succeed.

    let nontrimmed = [(100 * 2048, 16 * 2048)]; // 16-sector NonTrimmed range
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "01_clean",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    let expected = Golden {
        bytes_good: capacity_sectors as u64 * 2048,
        bytes_unreadable: 0,
        bytes_pending: 0,
        max_reads: 8, // adaptive batch=32 reads finishes 16 sectors in 1 read; allow up to 8.
    };
    assert_eq!(stats.bytes_good, expected.bytes_good, "01_clean bytes_good");
    assert_eq!(
        stats.bytes_unreadable, expected.bytes_unreadable,
        "01_clean bytes_unreadable"
    );
    assert_eq!(
        stats.bytes_pending, expected.bytes_pending,
        "01_clean bytes_pending"
    );
    assert!(!pr.halted, "01_clean halted");
    assert!(
        trace_len <= expected.max_reads,
        "01_clean trace_len={trace_len} exceeds bound {}",
        expected.max_reads
    );
}

// ─────────────────────────── Profile 2: ALL MEDIUM ───────────────────────
// Every LBA in the range returns MEDIUM_ERROR every attempt. The handler
// chain fails to recover; bytes stay NonTrimmed, not Unreadable (2026-05-11).

#[test]
fn profile_02_all_medium_error() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    for lba in 100..116 {
        reader.always(
            lba,
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
        );
    }

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "02_all_medium",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: the 16-sector bad range stays NonTrimmed (bytes_pending).
    // Pre-2026-05-11 patch would mark Unreadable here; current code
    // preserves NonTrimmed so subsequent passes get another shot.
    assert_eq!(
        stats.bytes_good,
        (capacity_sectors as u64 - 16) * 2048,
        "02_all_medium bytes_good"
    );
    assert_eq!(
        stats.bytes_unreadable, 0,
        "02_all_medium bytes_unreadable (must NOT be marked terminal in one pass)"
    );
    assert_eq!(
        stats.bytes_pending,
        16 * 2048,
        "02_all_medium bytes_pending (NonTrimmed retained across passes)"
    );
    assert!(!pr.halted, "02_all_medium halted");
    // Upper bound: per-sector probes + batch-drop/skip-escalation attempts
    // + scatter-recovery retries per hard sector. Guards against an
    // UNBOUNDED retry loop, which would be in the hundreds.
    assert!(
        trace_len <= 200,
        "02_all_medium trace_len={trace_len} exceeds 200"
    );
}

// ───────────────────── Profile 3: ALTERNATING GOOD/BAD ───────────────────
// LBAs 100, 102, 104, ... bad; odd LBAs good. Validates that good sectors
// interleaved with bad recover individually after the adaptive split.

#[test]
fn profile_03_alternating_good_bad() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    for lba in (100..116).step_by(2) {
        reader.always(
            lba,
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
        );
    }

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "03_alternating",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: 8 good sectors interleaved should mostly be Finished; 8 bad
    // stay NonTrimmed. Allow 2 sectors of slop — the bisect cursor doesn't
    // always converge exactly at boundaries with the size-aware skip cap.
    let good_total = stats.bytes_good;
    let baseline_good = (capacity_sectors as u64 - 16) * 2048;
    let middle_recovered = good_total - baseline_good;
    assert!(
        middle_recovered >= 6 * 2048,
        "03_alternating recovered only {middle_recovered} bytes of 8 good sectors"
    );
    assert!(
        middle_recovered <= 9 * 2048,
        "03_alternating recovered MORE than scripted good sectors: {middle_recovered}"
    );
    assert_eq!(stats.bytes_unreadable, 0, "03_alternating bytes_unreadable");
    // Remaining must be NonTrimmed (pending), not lost.
    assert!(
        stats.bytes_pending > 0,
        "03_alternating expected NonTrimmed remainder, got bytes_pending=0"
    );
    assert!(!pr.halted, "03_alternating halted");
    // Efficiency guard (catches runaway, not the tier-2 roster). The 8 bad
    // sectors are additionally probed by the tier-2 marginal specialists
    // before being left NonTrimmed — bounded, finite extra reads.
    assert!(
        trace_len <= 220,
        "03_alternating trace_len={trace_len} exceeds 220"
    );
}

// ───────────────────── Profile 4: EDGE-BAD (size-aware-skip canon) ───────
// Bad at start, good middle, bad at end — the size-aware-skip canonical
// case. Middle good sectors MUST be recovered, not skip-escalated over.

#[test]
fn profile_04_edge_bad_good_middle() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    for lba in 100..104 {
        reader.always(
            lba,
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
        );
    }
    for lba in 112..116 {
        reader.always(
            lba,
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
        );
    }

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "04_edge_bad",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: the 8 good middle sectors should land Finished (allowing
    // 2 sectors of bisection slop at boundaries).
    let middle_recovered = stats.bytes_good - (capacity_sectors as u64 - 16) * 2048;
    assert!(
        middle_recovered >= 6 * 2048,
        "04_edge_bad recovered only {middle_recovered} bytes of 8 good middle sectors"
    );
    assert_eq!(stats.bytes_unreadable, 0, "04_edge_bad bytes_unreadable");
    assert!(
        stats.bytes_pending > 0,
        "04_edge_bad bytes_pending expected > 0"
    );
    assert!(!pr.halted, "04_edge_bad halted");
    assert!(
        trace_len <= 120,
        "04_edge_bad trace_len={trace_len} exceeds 120"
    );
}

// ───────────────────── Profile 5: SINGLE BAD SECTOR ──────────────────────
// 1 bad sector amid an otherwise good 16-sector range. Validates the
// common "stochastic miss in Pass 1, easily picked up in Pass N" case.

#[test]
fn profile_05_single_bad_sector() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    reader.always(
        108,
        ScriptStep::Err {
            sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
            asc: 0x11,
            ascq: 0x00,
        },
    );

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "05_single_bad",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: 15 of 16 sectors recovered. 1 sector stays NonTrimmed
    // (NOT Unreadable — same multi-pass tolerance principle).
    assert_eq!(
        stats.bytes_good,
        (capacity_sectors as u64 - 1) * 2048,
        "05_single_bad bytes_good"
    );
    assert_eq!(stats.bytes_unreadable, 0, "05_single_bad bytes_unreadable");
    assert_eq!(stats.bytes_pending, 2048, "05_single_bad bytes_pending");
    assert!(!pr.halted, "05_single_bad halted");
    assert!(
        trace_len <= 80,
        "05_single_bad trace_len={trace_len} exceeds 80"
    );
}

// ───────────────────── Profile 6: DEEP PIT ───────────────────────────────
// An 8-sector bad pit inside a wider 24-sector range. Tests the handler
// chain converging on the pit boundaries instead of abandoning the range.

#[test]
fn profile_06_deep_pit() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    for lba in 108..116 {
        reader.always(
            lba,
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
        );
    }

    // 24 sectors NonTrimmed: 100..108 good, 108..116 BAD, 116..124 good.
    let nontrimmed = [(100 * 2048, 24 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (124 * 2048, (capacity_sectors as u64 - 124) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "06_deep_pit",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: 16 good (8 on each side of the pit) recovered, 8 bad
    // stay NonTrimmed.
    let recovered_in_range = stats.bytes_good - (capacity_sectors as u64 - 24) * 2048;
    assert!(
        recovered_in_range >= 14 * 2048,
        "06_deep_pit recovered only {recovered_in_range} bytes of 16 good sectors"
    );
    assert_eq!(stats.bytes_unreadable, 0, "06_deep_pit bytes_unreadable");
    assert!(
        stats.bytes_pending > 0,
        "06_deep_pit bytes_pending expected > 0"
    );
    assert!(!pr.halted, "06_deep_pit halted");
    // Bounded as in profile 02: the pit's hard single sectors each get
    // scatter-recovery on top of the baseline probe/skip walk. A runaway
    // loop would be in the hundreds.
    assert!(
        trace_len <= 180,
        "06_deep_pit trace_len={trace_len} exceeds 180"
    );
}

// ───────────────────── Profile 7: MEDIUM-THEN-GOOD ───────────────────────
// Each bad LBA fails with MEDIUM_ERROR N times, then succeeds. Tests
// whether the handler chain revisits failed sectors within one pass.

#[test]
fn profile_07_medium_then_good() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    // Sectors 105..110: fail twice, then succeed.
    for lba in 105..110 {
        reader.sequence(
            lba,
            vec![
                ScriptStep::Err {
                    sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                    asc: 0x11,
                    ascq: 0x00,
                },
                ScriptStep::Err {
                    sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                    asc: 0x11,
                    ascq: 0x00,
                },
                ScriptStep::Ok,
            ],
        );
    }

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "07_medium_then_good",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: with script "fail, fail, ok" per bad sector, the handler
    // chain re-reads each sector enough times to consume both failures
    // and reach Ok within one pass — all 256 sectors recover; none defer.
    assert_eq!(
        stats.bytes_good,
        256 * 2048,
        "07_medium_then_good bytes_good (handler chain re-reads each sector \
         across handlers, consuming the fail,fail,ok script for the whole cluster)"
    );
    assert_eq!(
        stats.bytes_unreadable, 0,
        "07_medium_then_good bytes_unreadable (NonTrimmed, never terminal in one pass)"
    );
    assert_eq!(
        stats.bytes_pending, 0,
        "07_medium_then_good bytes_pending (whole cluster recovered in one pass)"
    );
    assert!(!pr.halted, "07_medium_then_good halted");
    assert!(
        trace_len <= 100,
        "07_medium_then_good trace_len={trace_len} exceeds 100"
    );
}

// ───────────────────── Profile 8: BATCHED-FAIL ONLY ──────────────────────
// LBA 108 fails on batch reads but succeeds individually. Script fails
// once then succeeds on retry, simulating drop-to-count=1 recovery.

#[test]
fn profile_08_batch_fail_singles_ok() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    // Sector 108: fail on first call (which is the batch read), succeed
    // on second call (the drop-to-count=1 retry at the same position).
    reader.sequence(
        108,
        vec![
            ScriptStep::Err {
                sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x00,
            },
            ScriptStep::Ok,
        ],
    );

    let nontrimmed = [(100 * 2048, 16 * 2048)];
    let finished = [
        (0, 100 * 2048),
        (116 * 2048, (capacity_sectors as u64 - 116) * 2048),
    ];

    let (pr, stats, trace_len) = run_profile(
        "08_batch_fail",
        capacity_sectors,
        &nontrimmed,
        &finished,
        reader,
        trace,
    );

    // GOLDEN: the second attempt succeeds → all 16 sectors recovered.
    assert_eq!(
        stats.bytes_good,
        capacity_sectors as u64 * 2048,
        "08_batch_fail bytes_good — second attempt should recover"
    );
    assert_eq!(stats.bytes_unreadable, 0, "08_batch_fail bytes_unreadable");
    assert_eq!(stats.bytes_pending, 0, "08_batch_fail bytes_pending");
    assert!(!pr.halted, "08_batch_fail halted");
    assert!(
        trace_len <= 80,
        "08_batch_fail trace_len={trace_len} exceeds 80"
    );
}

// Run a single-always-bad-sector (LBA 130, inside a NonTrimmed [128,192)
// range) patch pass; returns final map stats (256-sector synthetic disc).
// Coverage + the sense-family invariant history: docs/passn-handler-ab.md.
fn single_dead_sector_patch_stats(step: ScriptStep) -> freemkv_engine::MapStats {
    let capacity_sectors: u32 = 256;
    let (mut reader, _trace) = ScriptedSectorReader::new(capacity_sectors);
    reader.always(130, step);

    let total_bytes = capacity_sectors as u64 * SECTOR_SIZE as u64;
    let disc = synthetic_disc(capacity_sectors);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);
    let nontrimmed = [(128 * 2048, 64 * 2048)];
    let finished = [
        (0, 128 * 2048),
        (192 * 2048, (capacity_sectors as u64 - 192) * 2048),
    ];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    let opts = freemkv_engine::PatchOptions::for_patch_pass(false, None, None, None);
    freemkv_engine::patch(&disc, &mut reader, &iso_path, &opts)
        .expect("patch must not error on a per-sector failure sense");

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let stats = Mapfile::load(&map_path).unwrap().stats();
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);
    stats
}

/// A persistent sense that never clears must obey the pass contract: nothing
/// Unreadable, nothing lost (good + pending == total), and at least the dead
/// sector left pending.
fn assert_persistent_sense_contract(step: ScriptStep, label: &str) {
    let stats = single_dead_sector_patch_stats(step);
    let total = 256u64 * 2048;
    assert_eq!(
        stats.bytes_unreadable, 0,
        "{label}: a patch pass must NEVER mark Unreadable"
    );
    assert_eq!(
        stats.bytes_good + stats.bytes_pending,
        total,
        "{label}: conservation — no byte may be silently dropped"
    );
    assert!(
        stats.bytes_pending >= 2048,
        "{label}: the always-dead sector must remain pending (NonTrimmed)"
    );
}

#[test]
fn patch_persistent_hardware_error_conserves_and_never_unreadable() {
    // HARDWARE_ERROR (sense_key=0x04) — wedge family.
    assert_persistent_sense_contract(
        ScriptStep::Err {
            sense_key: 0x04,
            asc: 0x11,
            ascq: 0x00,
        },
        "HARDWARE_ERROR",
    );
}

#[test]
fn patch_persistent_illegal_request_conserves_and_never_unreadable() {
    // ILLEGAL_REQUEST (sense_key=0x05) — wedge family.
    assert_persistent_sense_contract(
        ScriptStep::Err {
            sense_key: 0x05,
            asc: 0x21,
            ascq: 0x00,
        },
        "ILLEGAL_REQUEST",
    );
}

#[test]
fn patch_persistent_aborted_command_conserves_and_never_unreadable() {
    // ABORTED_COMMAND (sense_key=0x0B).
    assert_persistent_sense_contract(
        ScriptStep::Err {
            sense_key: 0x0B,
            asc: 0x00,
            ascq: 0x00,
        },
        "ABORTED_COMMAND",
    );
}

#[test]
fn patch_not_ready_then_recovers_fully() {
    // NOT_READY (sense_key=0x02, asc=0x04) that clears after two attempts must
    // recover the sector in-pass — no residual loss, no Unreadable, no hang.
    let capacity_sectors: u32 = 256;
    let (mut reader, _trace) = ScriptedSectorReader::new(capacity_sectors);
    reader.sequence(
        130,
        vec![
            ScriptStep::Err {
                sense_key: 0x02,
                asc: 0x04,
                ascq: 0x00,
            },
            ScriptStep::Err {
                sense_key: 0x02,
                asc: 0x04,
                ascq: 0x00,
            },
            ScriptStep::Ok,
        ],
    );

    let total_bytes = capacity_sectors as u64 * SECTOR_SIZE as u64;
    let disc = synthetic_disc(capacity_sectors);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);
    let nontrimmed = [(128 * 2048, 64 * 2048)];
    let finished = [
        (0, 128 * 2048),
        (192 * 2048, (capacity_sectors as u64 - 192) * 2048),
    ];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    let opts = freemkv_engine::PatchOptions::for_patch_pass(false, None, None, None);
    freemkv_engine::patch(&disc, &mut reader, &iso_path, &opts)
        .expect("patch must not error on a transient NOT_READY");

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let stats = Mapfile::load(&map_path).unwrap().stats();
    assert_eq!(
        stats.bytes_unreadable, 0,
        "NOT_READY recovery must not mark Unreadable"
    );
    assert_eq!(
        stats.bytes_pending, 0,
        "a NOT_READY that clears must leave nothing pending"
    );
    assert_eq!(
        stats.bytes_good,
        capacity_sectors as u64 * 2048,
        "every sector recovers once NOT_READY clears"
    );
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);
}

// ──────── Handler chain recovers re-readable sectors inside a bad block ────────
// A bad range holds one dead sector amid readable ones. The chain narrows to
// per-sector reads, recovering the readable ones, leaving only the dead one pending.

#[test]
fn handler_chain_recovers_readable_sectors_leaving_only_dead_pending() {
    let capacity_sectors: u32 = 256;
    let (mut reader, trace) = ScriptedSectorReader::new(capacity_sectors);
    // One bad sector at LBA 130 — inside the LOW 32-sector block of the range.
    reader.always(
        130,
        ScriptStep::Err {
            sense_key: 3,
            asc: 0x11,
            ascq: 0x05,
        },
    );

    let total_bytes = capacity_sectors as u64 * SECTOR_SIZE as u64;
    let disc = synthetic_disc(capacity_sectors);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);
    // 64-sector NonTrimmed range [128,192); everything else already Finished.
    let nontrimmed = [(128 * 2048, 64 * 2048)];
    let finished = [
        (0, 128 * 2048),
        (192 * 2048, (capacity_sectors as u64 - 192) * 2048),
    ];
    prep_iso_and_mapfile(&iso_path, total_bytes, &finished, &nontrimmed);

    let opts = freemkv_engine::PatchOptions::for_patch_pass(false, None, None, None);
    freemkv_engine::patch(&disc, &mut reader, &iso_path, &opts)
        .expect("handler-chain patch must not error");

    let map_path = freemkv_engine::mapfile_path_for(&iso_path);
    let stats = Mapfile::load(&map_path).unwrap().stats();

    // The clean sectors of [128,192) all recover; only the always-dead
    // sector (LBA 130) stays NonTrimmed, not Unreadable. Conservation:
    // 255 good + 1 still-pending = the full 256, nothing lost.
    assert_eq!(
        stats.bytes_unreadable, 0,
        "recovery must never mark Unreadable in a pass"
    );
    assert_eq!(
        stats.bytes_pending, 2048,
        "only the single always-dead sector (LBA 130) stays NonTrimmed"
    );
    assert_eq!(
        stats.bytes_good,
        255 * 2048,
        "every sector except the one dead LBA is recovered"
    );

    let _ = trace; // read trace retained by the fixture; no ordering assertion here

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(&map_path);
}
