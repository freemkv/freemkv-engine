//! Integration tests for progress reporting, halt behavior, drop safety,
//! and the file-backed sector reader round trip.

use freemkv_engine::CopyOptions;
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::Result;
use libfreemkv::pes::Stream as PesStream;
use libfreemkv::{
    ContentFormat, Disc, DiscFormat, DiscStream, DiscTitle, EventKind, Extent, FileSectorSource,
    SectorSource,
};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SECTOR_SIZE: usize = 2048;

// ── helpers ────────────────────────────────────────────────────────────────

/// Returns zeroed sectors. Always succeeds. Counts each call.
struct ZeroSectorReader {
    capacity: u32,
    calls: Arc<AtomicU64>,
}

impl ZeroSectorReader {
    fn new(capacity: u32) -> Self {
        Self {
            capacity,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl SectorSource for ZeroSectorReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let bytes = count as usize * SECTOR_SIZE;
        buf[..bytes].fill(0);
        Ok(bytes)
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

/// Zero-filling reader that raises the halt flag ITSELF on its Nth read and
/// counts every read the sweep issues, so a halt test can assert on a count
/// instead of a stopwatch.
///
/// The interesting number is `reads_total - halt_after_reads`: how many more
/// batches the sweep read after the flag went up. The sweep polls `halt` at the
/// top of every batch iteration, immediately before the read, so a correct
/// build issues exactly ZERO further reads — no wall-clock budget involved, and
/// no dependence on how fast the machine happens to be.
struct HaltingZeroSectorReader {
    capacity: u32,
    halt: Arc<AtomicBool>,
    reads: Arc<AtomicU64>,
    halt_after_reads: u64,
}

impl SectorSource for HaltingZeroSectorReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        let n = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
        if n == self.halt_after_reads {
            self.halt.store(true, Ordering::Relaxed);
        }
        let bytes = count as usize * SECTOR_SIZE;
        buf[..bytes].fill(0);
        Ok(bytes)
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

/// Build a Disc instance with a known capacity, no titles, no encryption.
/// Sufficient for `copy` (which only uses capacity_sectors + decrypt keys).
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

/// Build a DiscTitle with a single extent of `sector_count` sectors and no
/// streams (DiscStream still iterates sectors and would emit BytesRead).
fn synthetic_title(sector_count: u32) -> DiscTitle {
    DiscTitle {
        playlist: String::new(),
        playlist_id: 0,
        duration_secs: 0.0,
        size_bytes: sector_count as u64 * SECTOR_SIZE as u64,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![Extent {
            start_lba: 0,
            sector_count,
        }],
        content_format: ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

// ── 1. BytesRead events emitted during disc copy ──────────────────────────

#[test]
fn test_bytes_read_emitted_during_disc_copy() {
    // Build a tiny synthetic disc and stream it through DiscStream.
    let reader = ZeroSectorReader::new(64);
    let title = synthetic_title(64);
    let keys = libfreemkv::DecryptKeys::None;

    let mut stream = DiscStream::new(
        Box::new(reader),
        title,
        keys,
        60,
        ContentFormat::BdTs,
        false,
        None,
    )
    .unwrap();

    let count = Arc::new(AtomicU64::new(0));
    let count_cb = count.clone();
    stream.on_event(move |ev| {
        if let EventKind::BytesRead { .. } = ev.kind {
            count_cb.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Drive the stream to EOF. With no streams configured, read() returns
    // Ok(None) once all extents are exhausted.
    loop {
        match stream.read() {
            Ok(Some(_frame)) => {}
            Ok(None) => break,
            Err(e) => panic!("stream read failed: {e:?}"),
        }
    }

    let n = count.load(Ordering::Relaxed);
    assert!(
        n > 0,
        "expected at least one BytesRead event during disc copy, got {n}"
    );
}

// ── 2. copy() on_progress callback fires (regression guard) ───────────────

#[test]
fn test_disc_copy_progress_callback_fires() {
    let disc = synthetic_disc(64);
    let mut reader = ZeroSectorReader::new(64);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp); // we want the path, not the file handle

    let calls = Arc::new(AtomicU64::new(0));
    let last_bytes = Arc::new(AtomicU64::new(0));

    struct CountingReporter {
        calls: Arc<AtomicU64>,
        last_bytes: Arc<AtomicU64>,
    }
    impl libfreemkv::progress::Progress for CountingReporter {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.last_bytes.store(p.bytes_good_total, Ordering::Relaxed);
            true
        }
    }
    let reporter = CountingReporter {
        calls: calls.clone(),
        last_bytes: last_bytes.clone(),
    };

    let opts = CopyOptions {
        decrypt: false,
        progress: Some(&reporter),
        ..Default::default()
    };

    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy ok");

    // Cleanup any sidecar mapfile + ISO before assertions.
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    assert!(result.complete, "copy should be complete");
    let n = calls.load(Ordering::Relaxed);
    let last = last_bytes.load(Ordering::Relaxed);
    assert!(n > 0, "on_progress should fire at least once, got {n}");
    assert!(
        last > 0,
        "final progress bytes should be non-zero, got {last}"
    );
}

// ── 3. Halt aborts disc copy promptly ─────────────────────────────────────

#[test]
fn test_halt_aborts_disc_copy_promptly() {
    // "Promptly" measured in READS, not seconds: the sweep polls `halt` at the
    // top of every batch, before the read, so reads after the flag goes up
    // must be exactly zero — no wall-clock budget, no scheduler dependence.
    let capacity_sectors: u32 = 60_000; // ~1000 batches at 60 sectors/batch
    let halt_after_reads: u64 = 5;

    let halt = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let mut reader = HaltingZeroSectorReader {
        capacity: capacity_sectors,
        halt: halt.clone(),
        reads: reads.clone(),
        halt_after_reads,
    };
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        halt: Some(halt.clone()),
        ..Default::default()
    };
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts);

    // Cleanup
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    let copy_result = result.expect("copy returns Ok with halted=true on halt");
    assert!(
        copy_result.halted,
        "copy_result.halted should be true after halt"
    );
    assert!(
        !copy_result.complete,
        "copy_result.complete should be false when halted"
    );

    let reads_total = reads.load(Ordering::Relaxed);
    assert_eq!(
        reads_total,
        halt_after_reads,
        "the sweep issued {} read(s) after the halt flag was raised — halt must \
         be polled before EVERY batch read (this fixture is ~1000 batches, so an \
         unpolled halt reads all of them)",
        reads_total.saturating_sub(halt_after_reads)
    );

    // And it really did abandon the disc rather than finish it quietly.
    assert!(
        copy_result.bytes_good < copy_result.bytes_total,
        "a halted sweep cannot have read the whole disc: bytes_good={} bytes_total={}",
        copy_result.bytes_good,
        copy_result.bytes_total
    );
}

// ── 4. DiscStream Drop does not panic or block ────────────────────────────

#[test]
fn test_drop_impls_do_not_panic_or_block() {
    let reader = ZeroSectorReader::new(64);
    let title = synthetic_title(64);
    let keys = libfreemkv::DecryptKeys::None;
    let stream = DiscStream::new(
        Box::new(reader),
        title,
        keys,
        60,
        ContentFormat::BdTs,
        false,
        None,
    )
    .unwrap();

    // Two things are proved here, each with its own budget: (1) Drop must not
    // panic and must RETURN, backstopped by a generous 60 s recv timeout; (2)
    // Drop must not BLOCK, measured by timing the `drop` call itself in the worker.
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let t0 = Instant::now();
        drop(stream);
        let took = t0.elapsed();
        let _ = tx.send(took);
    });

    let took = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("DiscStream drop never returned — Drop is blocking or panicked");
    handle.join().expect("drop thread panicked");

    assert!(
        took < Duration::from_secs(1),
        "DiscStream::drop itself took {took:?} — Drop must not block on IO, a \
         lock, or a thread join"
    );
}

// ── 5. FileSectorSource round trip ────────────────────────────────────────

#[test]
fn test_file_sector_reader_round_trip() {
    // Build 8 sectors of pseudo-random bytes (sector-aligned).
    const N_SECTORS: usize = 8;
    let mut data = vec![0u8; N_SECTORS * SECTOR_SIZE];
    for (i, b) in data.iter_mut().enumerate() {
        // Cheap PRNG: just a multiplicative pattern, deterministic for asserts.
        *b = ((i as u64).wrapping_mul(2654435761) >> 16) as u8;
    }

    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    tmp.write_all(&data).expect("write data");
    tmp.flush().expect("flush");

    let path = tmp.path().to_path_buf();
    let mut fsr = FileSectorSource::open(&path).expect("open FileSectorSource");

    assert_eq!(
        fsr.capacity_sectors(),
        N_SECTORS as u32,
        "capacity mismatch"
    );

    // Read each sector individually and compare.
    let mut buf = vec![0u8; SECTOR_SIZE];
    for lba in 0..N_SECTORS as u32 {
        let n = fsr
            .read_sectors(lba, 1, &mut buf, false)
            .expect("read_sectors");
        assert_eq!(n, SECTOR_SIZE);
        let off = lba as usize * SECTOR_SIZE;
        assert_eq!(
            &buf[..],
            &data[off..off + SECTOR_SIZE],
            "sector {lba} mismatch"
        );
    }

    // Read all sectors at once and compare.
    let mut all = vec![0u8; N_SECTORS * SECTOR_SIZE];
    let n = fsr
        .read_sectors(0, N_SECTORS as u16, &mut all, false)
        .expect("read all sectors");
    assert_eq!(n, N_SECTORS * SECTOR_SIZE);
    assert_eq!(all, data, "bulk read mismatch");
}

// ── 6. Pass 1 sweeps the entire disc even when every read fails ───────────
// copy() must reach disc end regardless of read failures; only halt exits
// early. Expect: all NonTrimmed, bytes_good=0, bytes_unreadable=0, incomplete.

/// Reader that returns Err for every read. Optionally signals a halt
/// flag on the first read so tests can exercise the halt-during-skip-forward
/// path deterministically (no wallclock dependency).
struct FailingSectorReader {
    capacity: u32,
    /// If set, signals halt on the first `read_sectors` call. Cleared after
    /// the first signal so subsequent reads are plain Err.
    halt_on_first_read: Option<Arc<AtomicBool>>,
    /// Every `read_sectors` call. The halt test asserts on this instead of on
    /// a stopwatch. `&mut` borrow, so the test reads it back after `copy`.
    reads: u64,
}

impl FailingSectorReader {
    fn new(capacity: u32) -> Self {
        Self {
            capacity,
            halt_on_first_read: None,
            reads: 0,
        }
    }

    fn with_halt_on_first_read(capacity: u32, halt: Arc<AtomicBool>) -> Self {
        Self {
            capacity,
            halt_on_first_read: Some(halt),
            reads: 0,
        }
    }
}

impl SectorSource for FailingSectorReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        _count: u16,
        _buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        self.reads += 1;
        if let Some(h) = self.halt_on_first_read.take() {
            h.store(true, Ordering::Relaxed);
        }
        // Model a real damaged-disc read: CHECK CONDITION + MEDIUM ERROR
        // (UNRECOVERED READ ERROR / L-EC UNCORRECTABLE), not the crate's own
        // `Error::DiscRead` post-classification signal.
        Err(libfreemkv::error::Error::ScsiError {
            opcode: libfreemkv::scsi::SCSI_READ_10,
            status: libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION,
            sense: Some(libfreemkv::ScsiSense {
                sense_key: libfreemkv::scsi::SENSE_KEY_MEDIUM_ERROR,
                asc: 0x11,
                ascq: 0x05,
            }),
        })
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

#[test]
fn test_disc_copy_completes_full_disc_with_failing_reader() {
    // 1024 sectors = 2 MB. Reader fails every read. With skip_on_error +
    // skip_on_error, Pass 1 must mark every sector NonTrimmed and return
    // cleanly — no bail, no hang.
    let capacity_sectors: u32 = 1024;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    let mut reader = FailingSectorReader::new(capacity_sectors);
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,

        ..Default::default()
    };

    let t0 = Instant::now();
    let result =
        freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy returns Ok");
    let elapsed = t0.elapsed();

    // Cleanup
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    // Hard bound — Pass 1 must NOT infinite-loop on a fully-failing reader.
    // Accommodates the wedge-avoidance pause (5 s/failed batch); well-bounded
    // total (~20-30 s typical), not "completes in milliseconds."
    assert!(
        elapsed < Duration::from_secs(60),
        "Pass 1 took {elapsed:?} on a 2 MB synthetic disc — expected < 60 s (not infinite)"
    );

    // Per RIP_DESIGN.md §2.1: Pass 1 must reach end of disc regardless of
    // read outcomes.
    assert_eq!(
        result.bytes_total, total_bytes,
        "bytes_total must match disc capacity"
    );
    assert_eq!(
        result.bytes_good, 0,
        "no reads succeeded, bytes_good must be 0"
    );
    assert_eq!(
        result.bytes_unreadable, 0,
        "Pass 1 does not mark Unreadable; only Pass 2 (Disc::patch) does"
    );
    assert_eq!(
        result.bytes_pending, total_bytes,
        "every sector must be NonTrimmed → counted as pending. \
         Got bytes_pending={} of total {}",
        result.bytes_pending, total_bytes
    );
    assert!(
        !result.complete,
        "complete=false because NonTrimmed regions remain (work for Pass 2)"
    );
    assert!(!result.halted, "no halt was set; halted must be false");

    // ISO should be full disc size on disk (sparse zeros where reads failed).
    // tempfile was dropped above so the file may not still exist; we only
    // assert what CopyResult itself reports.
}

// ── 7. Halt during Pass 1 skip-forward path returns promptly (deterministic) ─
// Halt is the only legitimate early exit from Pass 1, even mid skip-forward.
// Fixture: reader signals halt on read #1, avoiding any wallclock race.

#[test]
fn test_disc_copy_halts_promptly_on_failing_reader() {
    let capacity_sectors: u32 = 1024 * 1024; // 2 GB synthetic disc

    let halt = Arc::new(AtomicBool::new(false));
    let mut reader = FailingSectorReader::with_halt_on_first_read(capacity_sectors, halt.clone());
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,

        halt: Some(halt),
        ..Default::default()
    };

    let t0 = Instant::now();
    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts)
        .expect("copy returns Ok on halt");
    let elapsed = t0.elapsed();
    let reads = reader.reads;

    // Cleanup
    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    // The primary, clock-free assertion: the halt raised during read #1 means
    // read #2 never happens. The sweep polls `halt` at the top of every batch
    // iteration, so this is exact — it does not get less true on a slow runner.
    assert_eq!(
        reads,
        1,
        "the sweep issued {} read(s) after the reader raised halt on read #1",
        reads.saturating_sub(1)
    );
    // The wall clock catches what the read count can't: the post-failure
    // cooldown (30 s zone-entry) must itself be halt-aware, so this bound is
    // generous vs the ~10 ms actual, yet well below the 30 s regression.
    assert!(
        elapsed < Duration::from_secs(10),
        "halt returned in {elapsed:?}; the post-failure cooldown is not being \
         cut short by the halt flag"
    );
    assert!(result.halted, "result.halted must be true");
    assert!(
        !result.complete,
        "halted run cannot be complete (bytes_pending > 0 expected)"
    );
    assert!(
        result.bytes_pending > 0,
        "halt fired before sweep completed; bytes_pending must be > 0"
    );
}

// ── 8. Hysteresis recovers data the drive can read individually ──────────
// Pass 1 reads in 32-sector batches; a reader whose multi-sector reads all
// fail must mark every ECC block NonTrimmed, with zero bytes_good.

struct BlockSizeFailingReader {
    capacity: u32,
}

impl SectorSource for BlockSizeFailingReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        if count == 1 {
            for chunk in buf.chunks_mut(SECTOR_SIZE) {
                chunk.fill((lba & 0xff) as u8);
            }
            Ok(buf.len())
        } else {
            Err(libfreemkv::error::Error::ScsiError {
                opcode: libfreemkv::scsi::SCSI_READ_10,
                status: libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION,
                sense: Some(libfreemkv::ScsiSense {
                    sense_key: libfreemkv::scsi::SENSE_KEY_MEDIUM_ERROR,
                    asc: 0x11,
                    ascq: 0x00,
                }),
            })
        }
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

#[test]
fn test_disc_copy_marks_failed_ecc_blocks_as_nontrimmed() {
    let capacity_sectors: u32 = 256;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    let mut reader = BlockSizeFailingReader {
        capacity: capacity_sectors,
    };
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,

        ..Default::default()
    };

    let result =
        freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy returns Ok");

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    // Pass 1 is "fast, get the most data" — it never retries a failed batch
    // sector-by-sector (that's Pass N's job). BlockSizeFailingReader fails
    // multi-sector reads, so every batch → SkipBlock → whole block NonTrimmed.
    assert_eq!(
        result.bytes_good, 0,
        "Pass 1 doesn't retry a failed batch per-sector — failed batches become NonTrimmed for Pass N to revisit"
    );
    assert_eq!(
        result.bytes_pending, total_bytes,
        "every sector is NonTrimmed (pending) after Pass 1, awaiting Pass N"
    );
    assert!(
        !result.complete,
        "complete=false because NonTrimmed regions remain (Pass N's work)"
    );
}

// ── 9. PassProgress carries separate unreadable vs pending byte counts ─────
// 2026-05-11 design: failed reads stay NonTrimmed for retry, not Unreadable,
// until the orchestrator promotes them after the final pass.

#[test]
fn test_pass2_leaves_failed_reads_as_pending_not_unreadable() {
    let capacity_sectors: u32 = 128;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    let mut reader = FailingSectorReader::new(capacity_sectors);
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };

    let pass1 = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("pass1 ok");

    assert_eq!(pass1.bytes_good, 0, "pass1: no good sectors");
    assert_eq!(pass1.bytes_unreadable, 0, "pass1: no confirmed unreadable");
    assert_eq!(
        pass1.bytes_pending, total_bytes,
        "pass1: all sectors NonTrimmed"
    );

    let last_unreadable = Arc::new(AtomicU64::new(0));
    let last_pending = Arc::new(AtomicU64::new(0));
    let last_good = Arc::new(AtomicU64::new(0));
    let last_dur = Arc::new(AtomicU64::new(0));

    struct SnapshotReporter {
        unreadable: Arc<AtomicU64>,
        pending: Arc<AtomicU64>,
        good: Arc<AtomicU64>,
        dur: Arc<AtomicU64>,
    }
    impl libfreemkv::progress::Progress for SnapshotReporter {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            self.unreadable
                .store(p.bytes_unreadable_total, Ordering::Relaxed);
            self.pending.store(p.bytes_pending_total, Ordering::Relaxed);
            self.good.store(p.bytes_good_total, Ordering::Relaxed);
            if let Some(d) = p.disc_duration_secs {
                self.dur.store((d * 1000.0) as u64, Ordering::Relaxed);
            }
            true
        }
    }
    let reporter = SnapshotReporter {
        unreadable: last_unreadable.clone(),
        pending: last_pending.clone(),
        good: last_good.clone(),
        dur: last_dur.clone(),
    };

    let pass2_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: Some(&reporter),
        ..Default::default()
    };

    let pass2 = freemkv_engine::copy(&disc, &mut reader, &iso_path, &pass2_opts).expect("pass2 ok");

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    assert_eq!(
        pass2.bytes_good, 0,
        "pass2: still no good sectors (reader always fails)"
    );
    // 2026-05-11 design: pass-level retries do NOT promote failed bytes to
    // Unreadable; they stay NonTrimmed (pending) for a later pass. Promotion
    // is an orchestrator (autorip) concern, not the patch loop's.
    assert_eq!(
        pass2.bytes_unreadable, 0,
        "pass2: Disc::patch never marks Unreadable mid-multipass — orchestrator promotes after final pass"
    );
    // bytes_pending stays at total_bytes because everything still
    // failed and nothing got recovered or promoted out of pending.
    assert_eq!(
        pass2.bytes_pending, total_bytes,
        "pass2: failed bytes remain NonTrimmed for the next pass to retry"
    );

    let observed_unreadable = last_unreadable.load(Ordering::Relaxed);
    let observed_pending = last_pending.load(Ordering::Relaxed);
    assert_eq!(
        observed_unreadable, 0,
        "progress should report zero confirmed-unreadable mid-pass under the new design"
    );
    assert!(
        observed_pending > 0,
        "progress should report pending bytes as the reader keeps failing"
    );

    // Video damage time: unreadable / total * duration
    // With no titles on synthetic disc, disc_duration_secs = None
    assert_eq!(
        last_dur.load(Ordering::Relaxed),
        0,
        "synthetic disc has no titles, duration should be None/0"
    );
}

// ── 9b. The patch pass's live drilldown is located against the main title ──
// `located`/`bytes_bad_in_main_title` intersect damage with the main title's
// extents; zero would mean the title never reached the consumer.

#[test]
fn test_patch_progress_locates_damage_against_the_main_title() {
    let capacity_sectors: u32 = 128;
    let total_bytes: u64 = capacity_sectors as u64 * SECTOR_SIZE as u64;

    let mut reader = FailingSectorReader::new(capacity_sectors);
    let mut disc = synthetic_disc(capacity_sectors);
    let mut title = synthetic_title(capacity_sectors);
    title.duration_secs = 60.0;
    disc.titles = vec![title];

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        ..Default::default()
    };
    // Pass 1 marks the whole disc NonTrimmed — the damage set the patch pass
    // then walks.
    freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("pass1 ok");

    struct LocatedReporter {
        bad_in_title: Arc<AtomicU64>,
        num_ranges: Arc<AtomicU64>,
        at_risk_ms: Arc<AtomicU64>,
    }
    impl libfreemkv::progress::Progress for LocatedReporter {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            self.bad_in_title
                .store(p.bytes_bad_in_main_title, Ordering::Relaxed);
            self.num_ranges
                .store(p.located.num_ranges as u64, Ordering::Relaxed);
            self.at_risk_ms
                .store(p.located.main_at_risk_ms as u64, Ordering::Relaxed);
            true
        }
    }
    let bad_in_title = Arc::new(AtomicU64::new(0));
    let num_ranges = Arc::new(AtomicU64::new(0));
    let at_risk_ms = Arc::new(AtomicU64::new(0));
    let reporter = LocatedReporter {
        bad_in_title: bad_in_title.clone(),
        num_ranges: num_ranges.clone(),
        at_risk_ms: at_risk_ms.clone(),
    };
    let pass2_opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: Some(&reporter),
        ..Default::default()
    };
    freemkv_engine::copy(&disc, &mut reader, &iso_path, &pass2_opts).expect("pass2 ok");

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    // Every sector of the title is damaged and the title covers the whole
    // disc, so the drilldown must account for all of it.
    assert_eq!(
        bad_in_title.load(Ordering::Relaxed),
        total_bytes,
        "the whole title is damaged; the live figure must say so"
    );
    assert!(
        num_ranges.load(Ordering::Relaxed) >= 1,
        "a damaged title must locate at least one range"
    );
    assert_eq!(
        at_risk_ms.load(Ordering::Relaxed),
        60_000,
        "all 60 s of a fully damaged title are at risk"
    );
}

// Section 10 ("Damage time calculation") used to be an empty heading — the
// formula lives in libfreemkv, not here, and 9b above already pins the value
// end to end. A heading promising coverage that doesn't exist was removed.

// ── 11. A live progress tick never counts zero-filled damage as "good" ─────
// The sweep cursor also advances on damage paths (SkipBlock, JumpAhead, gaps),
// so feeding it straight to bytes_good_total would render data loss as success.

/// Fails every read with RECOVERED ERROR (a marginal read the sweep distrusts):
/// each batch becomes a plain `SkipBlock` — zero-filled, marked NonTrimmed —
/// with the ordinary 5 s pause and no 30 s damage-zone cooldown, so the whole
/// disc is damage and the test stays short.
struct MarginalSectorReader {
    capacity: u32,
}

impl SectorSource for MarginalSectorReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        _count: u16,
        _buf: &mut [u8],
        _recovery: bool,
    ) -> Result<usize> {
        Err(libfreemkv::error::Error::DiscRead {
            sector: lba as u64,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(libfreemkv::ScsiSense {
                sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                asc: 0x17,
                ascq: 0x01,
            }),
        })
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

#[test]
fn live_progress_never_reports_zero_filled_damage_as_good_bytes() {
    let capacity_sectors: u32 = 60; // one batch: the sweep's optical default
    let mut reader = MarginalSectorReader {
        capacity: capacity_sectors,
    };
    let disc = synthetic_disc(capacity_sectors);

    let tmp = tempfile::NamedTempFile::new().expect("tempfile create");
    let iso_path = tmp.path().to_path_buf();
    drop(tmp);

    /// Remembers the HIGHEST `bytes_good_total` any tick ever claimed, and
    /// the four totals from the tick that accounted for the FEWEST bytes —
    /// the worst case for the partition assertion below.
    struct MaxGoodReporter {
        max_good: Arc<AtomicU64>,
        worst_sum: Arc<AtomicU64>,
        worst: Arc<std::sync::Mutex<(u64, u64, u64, u64)>>,
    }
    impl libfreemkv::progress::Progress for MaxGoodReporter {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            self.max_good
                .fetch_max(p.bytes_good_total, Ordering::Relaxed);
            let sum = p.bytes_good_total
                + p.bytes_unreadable_total
                + p.bytes_pending_total
                + p.bytes_retryable_total;
            let prev = self.worst_sum.load(Ordering::Relaxed);
            if prev == 0 || sum < prev {
                self.worst_sum.store(sum, Ordering::Relaxed);
                *self.worst.lock().unwrap() = (
                    p.bytes_good_total,
                    p.bytes_unreadable_total,
                    p.bytes_pending_total,
                    p.bytes_retryable_total,
                );
            }
            true
        }
    }
    let max_good = Arc::new(AtomicU64::new(0));
    let worst_sum = Arc::new(AtomicU64::new(0));
    let worst_totals = Arc::new(std::sync::Mutex::new((0u64, 0u64, 0u64, 0u64)));
    let reporter = MaxGoodReporter {
        max_good: max_good.clone(),
        worst_sum: worst_sum.clone(),
        worst: worst_totals.clone(),
    };

    let opts = CopyOptions {
        decrypt: false,
        multipass: true,
        progress: Some(&reporter),
        ..Default::default()
    };

    let result = freemkv_engine::copy(&disc, &mut reader, &iso_path, &opts).expect("copy ok");

    let _ = std::fs::remove_file(&iso_path);
    let _ = std::fs::remove_file(freemkv_engine::mapfile_path_for(&iso_path));

    // Fixture check: every sector must really have failed, or the assertion
    // below is vacuous.
    assert_eq!(
        result.bytes_good, 0,
        "fixture invalid: this reader never returns data, so the pass must \
         recover nothing"
    );

    assert_eq!(
        max_good.load(Ordering::Relaxed),
        0,
        "a live tick claimed recovered bytes on a disc where every single read \
         failed and every byte written was a zero fill"
    );

    // ...and the damage must land in a BUCKET, not merely be absent from
    // `good`: the four totals partition the disc, so on a tick with nothing
    // recovered, every byte must be unreadable, retryable, or pending.
    let total = capacity_sectors as u64 * SECTOR_SIZE as u64;
    let (g, u, p, r) = *worst_totals.lock().unwrap();
    assert_eq!(
        g + u + p + r,
        total,
        "the live totals must account for the whole disc: good={g} \
         unreadable={u} pending={p} retryable={r}, disc={total} — \
         {} bytes are in no bucket at all",
        total.saturating_sub(g + u + p + r)
    );
}
