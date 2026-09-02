//! The sweep producer's consumer-side `Sink<WorkItem>`.
//!
//! A producer/consumer split overlaps the SCSI read with decrypt +
//! file-write + mapfile fsync on the generic [`libfreemkv::io::Pipeline`] +
//! [`libfreemkv::io::Sink`] primitive. This module is the sweep-specific
//! `Sink` impl; the producer-side state machine stays with the producer —
//! the free `sweep` fn in `recovery/mod.rs`.
//!
//! See docs/sweep-sink.md for the throughput rationale and the
//! correctness invariants preserved by the split.

use std::io::{Seek, SeekFrom, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use libfreemkv::error::Error;
use libfreemkv::io::{Flow, Sink};

use super::mapfile::{MapStats, Mapfile, SectorStatus};

/// Reusable zero buffer for SkipFill / GapFill. 64 KB matches the
/// existing zero_gap chunk size used by the pre-split sweep loop.
const ZERO_CHUNK: usize = 64 * 1024;

/// Producer → Consumer messages. The consumer applies these in FIFO
/// order; ordering of file writes and mapfile records across items is
/// preserved.
pub(super) enum WorkItem {
    /// Successful batch read. Producer has already decrypted `buf` if
    /// `opts.decrypt` was set. Consumer writes `buf` at `pos` and
    /// records the range as `Finished`.
    Good { pos: u64, buf: Vec<u8> },

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

// `Sink<WorkItem>` for sweep. Owns the writeback file + mapfile +
// progress back-channel. `apply` does the file-write + mapfile.record
// per item; `close` drains, fsyncs, and flushes the mapfile.
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
    /// Reusable zero buffer for SkipFill / GapFill. Held in the sink
    /// so each apply call doesn't reallocate.
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
                // DAMAGE only — NOT NonTried (unread remainder ahead of the
                // sweep head): including it made the live drilldown treat the
                // whole unread disc as damage at sweep start, melting to 0.
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

    // `/dev/null`'s fsync genuinely fails at the OS level (macOS
    // ENODEV, Linux EINVAL); `is_regular` is supplied by the caller
    // so both arms of `close`'s policy can be driven over it.
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

    // The zero-fill loop must write EXACTLY the skipped range — under-fill
    // leaves stale bytes the mapfile now claims are NonTrimmed, over-fill
    // clobbers good data past the gap. See docs/sweep-sink.md.
    #[test]
    fn a_skip_fill_writes_exactly_the_gap_and_records_exactly_the_gap() {
        let dir = scratch("skipfill");
        let gap_start = 4096u64;
        // Two whole 64 KB chunks plus a short final one, so the chunking runs
        // three times with a partial tail. Sector-aligned since the PRODUCER
        // snaps every span via `snap_to_sectors` before reaching this sink.
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

    // `close` must surface a failed `sync_all` when the output is a regular
    // file — the last barrier before `copy` reports a finished rip.
    // See docs/sweep-sink.md for why the failure is genuine, not injected.
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

    // ...and must NOT surface it when the output is not a regular file
    // (/dev/null and pipes always fail `sync_all`). See docs/sweep-sink.md
    // for why this also checks the rest of `close`'s job ran.
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
