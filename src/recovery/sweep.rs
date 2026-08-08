//! `Disc::sweep`'s consumer-side `Sink<WorkItem>`.
//!
//! Background: the original sweep loop runs strictly serialised —
//! SCSI read → decrypt → seek + write → mapfile.record → next iter.
//! On a healthy disc the SCSI read costs ~5-12 ms per 64 KB batch and
//! the post-read work (decrypt 1-3 ms + file write + mapfile fsync
//! 5-15 ms) adds another batch's worth of latency. The drive idles
//! during the post-read work; throughput tops out at the *sum* of
//! both costs.
//!
//! A producer/consumer split overlaps the two stages on the generic
//! [`libfreemkv::io::Pipeline`] + [`libfreemkv::io::Sink`] primitive. This module
//! is the sweep-specific `Sink` impl; the producer-side state machine
//! (read_error context, decrypt, set_speed, halt) stays in
//! `Disc::sweep` in `disc/mod.rs`.
//!
//! Correctness invariants preserved:
//! - Mapfile is single-writer (consumer-only). No locking.
//! - All `read_error::ReadCtx` state stays on the producer thread.
//! - `set_speed` calls happen on the producer thread (same thread that
//!   owns the `SectorSource`). No new SCSI concurrency.
//! - Per-iteration ordering of file-write → mapfile-record is kept
//!   intact in the consumer (write before record), so the on-disk
//!   invariant "mapfile only marks Finished what the file has
//!   received" survives a crash mid-pass.
//! - Only one SCSI command is in flight at a time; error-path timing
//!   is identical and no new retry logic is introduced.

use std::io::{Seek, SeekFrom, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use libfreemkv::error::Error;
use libfreemkv::io::{Flow, Sink};

use super::mapfile::{MapStats, Mapfile, SectorStatus};

/// Reusable zero buffer for SkipFill / GapFill / BisectBad. 64 KB
/// matches the existing zero_gap chunk size used by the pre-split
/// sweep loop.
const ZERO_CHUNK: usize = 64 * 1024;

/// Producer → Consumer messages. The consumer applies these in FIFO
/// order; ordering of file writes and mapfile records across items is
/// preserved.
pub(super) enum WorkItem {
    /// Successful batch read. Producer has already decrypted `buf` if
    /// `opts.decrypt` was set. Consumer writes `buf` at `pos` and
    /// records the range as `Finished`.
    Good { pos: u64, buf: Vec<u8> },

    /// Bisect inner-loop good single sector (already decrypted by the
    /// producer). 2048 bytes.
    BisectGood { pos: u64, buf: Box<[u8; 2048]> },

    /// Bisect inner-loop bad single sector. Consumer writes 2048
    /// zeros at `pos` and records the sector as `NonTrimmed`.
    BisectBad { pos: u64 },

    /// Whole-batch zero-fill (failed batch on `SkipBlock`, or the
    /// failed batch portion of `JumpAhead`). Consumer streams zeros
    /// across `[pos, pos+len)` and records the range as `NonTrimmed`.
    SkipFill { pos: u64, len: u64 },

    /// Gap fill following a `JumpAhead`. Same effect as `SkipFill`;
    /// distinguished only so future logging / instrumentation can
    /// tell them apart without parsing a flag.
    GapFill { pos: u64, len: u64 },

    /// Producer wants the latest mapfile stats for the progress
    /// callback. Consumer responds on `prog_tx` with a fresh
    /// [`ProgressSnapshot`]. Best-effort: if the producer hasn't
    /// drained the previous snapshot, the new one is silently
    /// dropped — the producer's local cache stays current enough.
    StatsRequest,
}

/// Snapshot the consumer sends back to the producer for the progress
/// callback.
pub(super) struct ProgressSnapshot {
    pub stats: MapStats,
    pub bad_ranges: Vec<(u64, u64)>,
}

/// Final summary returned by the consumer thread on shutdown — what
/// `SweepSink::close` produces, surfaced to the producer via
/// `Pipeline::finish`.
pub(super) struct ConsumerSummary {
    pub stats: MapStats,
}

/// Drain any pending progress snapshots from the consumer. Returns
/// the most recent one, if any. The producer caches it and uses it
/// for subsequent progress callbacks until a fresh one arrives.
pub(super) fn try_recv_progress(rx: &Receiver<ProgressSnapshot>) -> Option<ProgressSnapshot> {
    let mut latest = None;
    while let Ok(snap) = rx.try_recv() {
        latest = Some(snap);
    }
    latest
}

/// `Sink<WorkItem>` for sweep. Owns the writeback file + mapfile +
/// progress back-channel. `apply` carries the file-write +
/// mapfile.record per item; `close` drains the writeback pipeline,
/// fsyncs the ISO, and flushes the mapfile.
pub(super) struct SweepSink {
    file: libfreemkv::io::WritebackFile,
    map: Mapfile,
    /// `sync_all`-on-failure-is-an-error iff the output is a regular
    /// file. `/dev/null` and pipes always fail `sync_all`; that's not
    /// a real error.
    is_regular: bool,
    /// Back-channel for `StatsRequest` responses. The producer caches
    /// the latest snapshot and uses it for the progress callback;
    /// dropped sends on a full channel are by design.
    prog_tx: SyncSender<ProgressSnapshot>,
    /// Reusable zero buffer for SkipFill / GapFill / BisectBad. Held
    /// in the sink so each apply call doesn't reallocate.
    zero: Box<[u8; ZERO_CHUNK]>,
}

impl SweepSink {
    /// Construct a new `SweepSink` plus the matching progress
    /// receiver. Channel depth on the back-channel is `1` — the
    /// producer's cache is the source of truth between snapshots.
    pub(super) fn new(
        file: libfreemkv::io::WritebackFile,
        map: Mapfile,
        is_regular: bool,
    ) -> (Self, Receiver<ProgressSnapshot>) {
        let (prog_tx, prog_rx) = sync_channel::<ProgressSnapshot>(1);
        let sink = SweepSink {
            file,
            map,
            is_regular,
            prog_tx,
            zero: Box::new([0u8; ZERO_CHUNK]),
        };
        (sink, prog_rx)
    }
}

impl Sink<WorkItem> for SweepSink {
    type Output = ConsumerSummary;

    fn apply(&mut self, item: WorkItem) -> Result<Flow, Error> {
        match item {
            WorkItem::Good { pos, buf } => {
                // Decrypt is on the producer; consumer assumes plaintext.
                let len = buf.len() as u64;
                self.file.seek(SeekFrom::Start(pos))?;
                self.file.write_all(&buf)?;
                self.map.record(pos, len, SectorStatus::Finished)?;
            }
            WorkItem::BisectGood { pos, buf } => {
                self.file.seek(SeekFrom::Start(pos))?;
                self.file.write_all(&buf[..])?;
                self.map.record(pos, 2048, SectorStatus::Finished)?;
            }
            WorkItem::BisectBad { pos } => {
                self.file.seek(SeekFrom::Start(pos))?;
                self.file.write_all(&self.zero[..2048])?;
                self.map.record(pos, 2048, SectorStatus::NonTrimmed)?;
            }
            WorkItem::SkipFill { pos, len } | WorkItem::GapFill { pos, len } => {
                self.file.seek(SeekFrom::Start(pos))?;
                // Subsequent writes are sequential; `WritebackFile`'s
                // seek-elision keeps them on the writeback pipeline path.
                let mut filled = 0u64;
                while filled < len {
                    let chunk = (len - filled).min(self.zero.len() as u64) as usize;
                    self.file.write_all(&self.zero[..chunk])?;
                    filled += chunk as u64;
                }
                self.map.record(pos, len, SectorStatus::NonTrimmed)?;
            }
            WorkItem::StatsRequest => {
                let stats = self.map.stats();
                // DAMAGE only — NOT NonTried. NonTried is the unread remainder
                // ahead of the sweep head, not damage; including it made the live
                // located drilldown (at-risk movie time + range count) treat the
                // whole unread disc as confirmed damage, so at sweep start it
                // showed ~full-movie at-risk and melted to 0 as the sweep
                // progressed. Matches the one-shot progress path, which already
                // excludes NonTried.
                let bad_ranges = self
                    .map
                    .ranges_with(&crate::recovery::mapfile::damage_sector_statuses());
                // Best-effort: drop on backpressure; producer's cache
                // stays current enough.
                let _ = self
                    .prog_tx
                    .try_send(ProgressSnapshot { stats, bad_ranges });
            }
        }
        Ok(Flow::Continue)
    }

    fn close(mut self) -> Result<Self::Output, Error> {
        // Drain the writeback pipeline + fsync the ISO, then persist
        // any pending mapfile state. Same finalisation order as the
        // pre-Pipeline consumer loop.
        if let Err(e) = self.file.sync_all()
            && self.is_regular
        {
            return Err(Error::IoError { source: e });
        }
        // Non-regular outputs (/dev/null, pipes) always fail
        // sync_all; that's not a real error.
        self.map.flush()?;

        Ok(ConsumerSummary {
            stats: self.map.stats(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Build a `SweepSink` over a scratch ISO pre-filled with `0xAA`, plus a
    /// fresh mapfile. No drive, no producer thread — the consumer is driven
    /// by hand.
    fn sink_over(dir: &std::path::Path, total: u64) -> (SweepSink, std::path::PathBuf) {
        let iso = dir.join("out.iso");
        std::fs::write(&iso, vec![0xAAu8; total as usize]).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&iso)
            .unwrap();
        let wf = libfreemkv::io::WritebackFile::new(f).unwrap();
        let map = Mapfile::create(&dir.join("out.map"), total, "test").unwrap();
        let (sink, _rx) = SweepSink::new(wf, map, true);
        (sink, iso)
    }

    /// A `SweepSink` whose output is `/dev/null` — a character device whose
    /// `fsync` genuinely fails at the OS level (macOS `F_FULLFSYNC` → ENODEV,
    /// Linux `fsync` → EINVAL). `is_regular` is supplied by the caller so both
    /// arms of `close`'s policy can be driven over the SAME real failure.
    #[cfg(unix)]
    fn sink_over_dev_null(dir: &std::path::Path, is_regular: bool) -> SweepSink {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let wf = libfreemkv::io::WritebackFile::new(f).unwrap();
        let map = Mapfile::create(&dir.join("out.map"), 8192, "test").unwrap();
        let (sink, _rx) = SweepSink::new(wf, map, is_regular);
        sink
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fmkv-sweepsink-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The zero-fill loop must write EXACTLY the skipped range.
    ///
    /// `SkipFill`/`GapFill` is the consumer half of damage handling: the
    /// producer could not read `[pos, pos+len)`, so the consumer punches zeros
    /// there and the mapfile records the range `NonTrimmed`. Those two halves
    /// have to agree, and nothing tested them — this module had no tests at
    /// all. If the loop writes nothing, the mapfile still claims the gap was
    /// filled while the image keeps whatever was there before (on a resume,
    /// bytes from a different disc); if it writes past `len`, it clobbers good
    /// data the sweep already wrote just beyond the gap.
    ///
    /// `len` here spans more than two 64 KB chunks with a ragged remainder, so
    /// the chunking arithmetic is genuinely exercised rather than short-cut by
    /// a single pass.
    #[test]
    fn a_skip_fill_writes_exactly_the_gap_and_records_exactly_the_gap() {
        let dir = scratch("skipfill");
        let gap_start = 4096u64;
        // Two whole 64 KB chunks plus a short final one, so the chunking
        // arithmetic runs three times with a partial tail. Sector-aligned,
        // because `Mapfile::record` widens a ragged range to whole sectors and
        // that would blur the "exactly len" assertion.
        let len = ZERO_CHUNK as u64 * 2 + 3 * 2048;
        // Trailing slack wider than one chunk, so an overshooting fill has
        // somewhere visible to overshoot INTO.
        let total = gap_start + len + 2 * ZERO_CHUNK as u64;
        let (mut sink, iso) = sink_over(&dir, total);

        sink.apply(WorkItem::SkipFill {
            pos: gap_start,
            len,
        })
        .unwrap();
        let summary = sink.close().unwrap();

        let mut got = Vec::new();
        std::fs::File::open(&iso)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(
            got.len() as u64,
            total,
            "the file must not have been resized"
        );
        assert!(
            got[..gap_start as usize].iter().all(|&b| b == 0xAA),
            "bytes before the gap were rewritten"
        );
        assert!(
            got[gap_start as usize..(gap_start + len) as usize]
                .iter()
                .all(|&b| b == 0),
            "the gap the mapfile is about to call NonTrimmed was not actually zeroed"
        );
        assert!(
            got[(gap_start + len) as usize..].iter().all(|&b| b == 0xAA),
            "the fill ran past the end of the gap and clobbered good data"
        );

        assert_eq!(summary.stats.bytes_good, 0);
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert_eq!(
            reloaded.ranges_with(&[SectorStatus::NonTrimmed]),
            vec![(gap_start, len)],
            "exactly the gap is recorded NonTrimmed — no more, no less"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gap smaller than one chunk is still written in full.
    #[test]
    fn a_sub_chunk_gap_fill_writes_its_whole_length() {
        let dir = scratch("gapfill");
        let len = 3072u64;
        let total = 8192u64;
        let (mut sink, iso) = sink_over(&dir, total);

        sink.apply(WorkItem::GapFill { pos: 0, len }).unwrap();
        let summary = sink.close().unwrap();

        let got = std::fs::read(&iso).unwrap();
        assert!(got[..len as usize].iter().all(|&b| b == 0));
        assert!(
            got[len as usize..].iter().all(|&b| b == 0xAA),
            "a fill shorter than one chunk still stopped at len"
        );
        assert_eq!(summary.stats.bytes_good, 0);
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert_eq!(
            reloaded.ranges_with(&[SectorStatus::NonTrimmed]),
            vec![(0, len)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A good batch lands at its own offset and is recorded Finished there.
    #[test]
    fn a_good_batch_is_written_at_its_position_and_recorded_finished() {
        let dir = scratch("good");
        let total = 8192u64;
        let (mut sink, iso) = sink_over(&dir, total);

        sink.apply(WorkItem::Good {
            pos: 2048,
            buf: vec![0x5Au8; 2048],
        })
        .unwrap();
        let summary = sink.close().unwrap();

        let got = std::fs::read(&iso).unwrap();
        assert!(got[..2048].iter().all(|&b| b == 0xAA));
        assert!(got[2048..4096].iter().all(|&b| b == 0x5A));
        assert!(got[4096..].iter().all(|&b| b == 0xAA));
        assert_eq!(summary.stats.bytes_good, 2048);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bisect-bad sector is zeroed AND recorded `NonTrimmed` — never
    /// `Finished`.
    ///
    /// This is the one item in the enum whose payload is invented by the
    /// consumer rather than read off the disc: the producer could not read the
    /// sector, so `apply` writes 2048 zeros of its own. The status it records
    /// is therefore the ONLY thing standing between those zeros and the user.
    /// If this arm ever recorded `Finished`, the mapfile would call fabricated
    /// zeros recovered data, `bytes_good` would include them, a later patch
    /// pass would never retry the sector, and the rip would be handed over as
    /// complete with a hole in it. Nothing else in the tree checks it: the
    /// producer-side bisect only decides which item to send.
    #[test]
    fn a_bisect_bad_sector_is_zeroed_and_recorded_nontrimmed_not_finished() {
        let dir = scratch("bisectbad");
        let total = 8192u64;
        let (mut sink, iso) = sink_over(&dir, total);

        sink.apply(WorkItem::BisectBad { pos: 4096 }).unwrap();
        let summary = sink.close().unwrap();

        let got = std::fs::read(&iso).unwrap();
        assert_eq!(got.len() as u64, total);
        assert!(
            got[..4096].iter().all(|&b| b == 0xAA),
            "the zero-fill ran backwards past the failed sector"
        );
        assert!(
            got[4096..6144].iter().all(|&b| b == 0),
            "the failed sector must be zero-filled, not left as whatever was there"
        );
        assert!(
            got[6144..].iter().all(|&b| b == 0xAA),
            "the zero-fill wrote past the single 2048-byte sector"
        );

        assert_eq!(
            summary.stats.bytes_good, 0,
            "a sector the drive could not read must not count as recovered data"
        );
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert_eq!(
            reloaded.ranges_with(&[SectorStatus::NonTrimmed]),
            vec![(4096, 2048)],
            "exactly the failed sector is NonTrimmed, so a patch pass retries it"
        );
        assert!(
            reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
            "fabricated zeros must never be recorded Finished"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bisect-good sector lands at its own offset and is recorded `Finished`.
    ///
    /// The mirror of the arm above, and the pair is the point: the two carry
    /// the same 2048-byte length and differ only in payload and status. This
    /// one asserts the sector really is written where it says (`pos`, not the
    /// file's current position) and really is marked `Finished` — a bisect that
    /// recovered a sector but recorded it `NonTrimmed` would make every later
    /// pass re-read a sector already in hand, and one that wrote to the wrong
    /// offset would corrupt a neighbour while claiming success.
    #[test]
    fn a_bisect_good_sector_is_written_at_its_position_and_recorded_finished() {
        let dir = scratch("bisectgood");
        let total = 8192u64;
        let (mut sink, iso) = sink_over(&dir, total);

        sink.apply(WorkItem::BisectGood {
            pos: 4096,
            buf: Box::new([0x5Au8; 2048]),
        })
        .unwrap();
        let summary = sink.close().unwrap();

        let got = std::fs::read(&iso).unwrap();
        assert!(got[..4096].iter().all(|&b| b == 0xAA));
        assert!(
            got[4096..6144].iter().all(|&b| b == 0x5A),
            "the recovered sector must land at pos, not at the file's cursor"
        );
        assert!(got[6144..].iter().all(|&b| b == 0xAA));

        assert_eq!(summary.stats.bytes_good, 2048);
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert_eq!(
            reloaded.ranges_with(&[SectorStatus::Finished]),
            vec![(4096, 2048)],
            "a sector recovered by bisect must be Finished, or later passes re-read it forever"
        );
        assert!(reloaded.ranges_with(&[SectorStatus::NonTrimmed]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `close` must surface a failed `sync_all` when the output is a regular
    /// file.
    ///
    /// `output_is_regular` is unit-tested where it is computed; what was
    /// untested is its CONSUMPTION here, which is the half that decides whether
    /// a real durability failure on the just-written image reaches the user or
    /// is thrown away. `sync_all` is the last barrier before `copy` reports a
    /// finished rip — if the swallow ever widened to cover regular files, an
    /// ENOSPC/EIO fsync on the ISO would be reported as a successful rip.
    ///
    /// The failure is genuine, not injected: `/dev/null`'s fsync fails at the
    /// OS level (macOS `F_FULLFSYNC` → ENODEV, Linux `fsync` → EINVAL). Only
    /// the `is_regular` flag — the input to the policy under test — is varied
    /// between this test and its pair below, so the two differ in exactly the
    /// bit being asserted.
    #[cfg(unix)]
    #[test]
    fn a_failed_sync_all_is_an_error_when_the_output_is_regular() {
        let dir = scratch("syncfail-regular");
        let mut sink = sink_over_dev_null(&dir, true);
        sink.apply(WorkItem::Good {
            pos: 0,
            buf: vec![0x5Au8; 2048],
        })
        .unwrap();

        let err = sink
            .close()
            .err()
            .expect("a failed fsync on a regular output must be reported, not swallowed");
        assert!(
            matches!(err, Error::IoError { .. }),
            "the underlying io error must be surfaced as-is, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ...and must NOT surface it when the output is not a regular file.
    ///
    /// `/dev/null` and pipes always fail `sync_all`; treating that as an error
    /// would make every benchmark/sink rip fail at the finish line. The
    /// exemption also has to keep doing the rest of `close`'s job, so this
    /// asserts the mapfile was still flushed and the summary is real — not just
    /// that no error came back.
    #[cfg(unix)]
    #[test]
    fn a_failed_sync_all_is_exempt_when_the_output_is_not_regular() {
        let dir = scratch("syncfail-devnull");
        let mut sink = sink_over_dev_null(&dir, false);
        sink.apply(WorkItem::Good {
            pos: 0,
            buf: vec![0x5Au8; 2048],
        })
        .unwrap();

        let summary = sink
            .close()
            .expect("a /dev/null output always fails fsync; that is not a rip failure");
        assert_eq!(summary.stats.bytes_good, 2048);
        let reloaded = Mapfile::load(&dir.join("out.map"))
            .expect("the exempt path must still flush the mapfile");
        assert_eq!(
            reloaded.ranges_with(&[SectorStatus::Finished]),
            vec![(0, 2048)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
