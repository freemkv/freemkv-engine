//! The sweep producer's consumer-side `Sink<WorkItem>`.
//!
//! A producer/consumer split overlaps the SCSI read with decrypt +
//! file-write + mapfile fsync on the generic [`libfreemkv::io::Pipeline`] +
//! [`libfreemkv::io::Sink`] primitive. This module is the sweep-specific
//! `Sink` impl; the producer-side state machine stays with the producer —
//! `sweep_internal` in `recovery/mod.rs`.

use std::io::{Seek, SeekFrom, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use libfreemkv::error::Error;
use libfreemkv::io::{Flow, Sink};

use super::mapfile::{MapStats, Mapfile, SectorStatus};

/// Reusable zero buffer for SkipFill / GapFill writes.
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

// No `close_stopped` override (the default `close`): this sink renames nothing, and a
// data-less mapfile flush would claim sectors not yet durable. T8: "halted: **Stopped**
// (not a failure)" after one 5 s grace; the disowned mapfile keeps the resumable record.
impl Sink<WorkItem> for SweepSink {
    type Output = ConsumerSummary;

    fn apply(&mut self, item: WorkItem) -> Result<Flow, Error> {
        // Abandoned by teardown: a resumed pass owns the image now.
        if self.map.is_disowned() {
            return Ok(Flow::Stop);
        }
        match item {
            WorkItem::Good { pos, buf } => {
                // Decrypt is on the producer; consumer assumes plaintext.
                let len = buf.len() as u64;
                let lost = |e| super::image_write_failed(&self.map, e);
                self.file.seek(SeekFrom::Start(pos)).map_err(lost)?;
                self.file.write_all(&buf).map_err(lost)?;
                if self.map.persist_due() && self.is_regular {
                    self.file.sync_all().map_err(lost)?;
                }
                self.map.record(pos, len, SectorStatus::Finished)?;
            }
            WorkItem::SkipFill { pos, len } | WorkItem::GapFill { pos, len } => {
                let map = &self.map;
                self.file
                    .seek(SeekFrom::Start(pos))
                    .map_err(|e| super::image_write_failed(map, e))?;
                // Subsequent writes are sequential; `WritebackFile`'s
                // seek-elision keeps them on the writeback pipeline path.
                let mut filled = 0u64;
                while filled < len {
                    // A fill can run GBs; stop mid-way if teardown abandons us.
                    if self.map.is_disowned() {
                        return Ok(Flow::Stop);
                    }
                    let chunk = (len - filled).min(self.zero.len() as u64) as usize;
                    self.file
                        .write_all(&self.zero[..chunk])
                        .map_err(|e| super::image_write_failed(&self.map, e))?;
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
            // The data is not durable: the dropped map must not flush it as Finished.
            self.map.disown_handle().disown();
            return Err(Error::from(e));
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

    // The zero-fill loop must write EXACTLY the skipped range — under-fill leaves stale bytes
    // the mapfile now claims are NonTrimmed, over-fill clobbers good data past the gap.
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

    // `close` must surface a failed `sync_all` when the output is a regular file — the last
    // barrier before `copy` reports a finished rip.
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

    // ...and must NOT surface it when the output is not a regular file (/dev/null and pipes
    // always fail `sync_all`).
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

    // A typed failure from the writeback flusher (SyncTimeout, Halted) must reach the
    // caller typed, not re-wrapped as IoError (read downstream as a dead USB bridge).
    #[test]
    fn a_typed_sync_failure_surfaces_typed_not_as_io_error() {
        let dir = scratch("synctyped");
        let (mut sink, _iso) = sink_over(&dir, 8192);
        let halt = libfreemkv::halt::Halt::new();
        halt.cancel();
        sink.file.set_halt(halt);
        let err = sink.close().err().expect("a halted fsync must fail");
        assert!(matches!(err, Error::Halted), "got {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // R3: a failed fsync means the bytes behind this close's Finished records are not
    // durable, so dropping the sink must not flush those records.
    #[test]
    fn a_failed_close_does_not_persist_finished_for_unsynced_data() {
        for _ in 0..ATTEMPTS {
            let dir = scratch("syncfail-drop");
            let (mut sink, _iso) = sink_over(&dir, 8192);
            let fresh = freshly_flushed(&mut sink.map);
            sink.apply(WorkItem::Good {
                pos: 0,
                buf: vec![0x5Au8; 2048],
            })
            .unwrap();
            if fresh.elapsed() >= FLUSH_WINDOW {
                continue; // the periodic persist already ran: try again
            }
            failed_close_case(sink, &dir);
            return;
        }
        panic!("no attempt stayed inside the mapfile's flush window");
    }

    fn failed_close_case(mut sink: SweepSink, dir: &std::path::Path) {
        let halt = libfreemkv::halt::Halt::new();
        halt.cancel();
        sink.file.set_halt(halt);
        assert!(sink.close().is_err(), "a halted fsync must fail");
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert!(
            reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
            "a range whose data never reached disk was persisted Finished"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // A timing-guarded case retries rather than passing silently on a slow runner.
    const ATTEMPTS: usize = 5;

    // R2: once teardown abandons this consumer (mapfile disowned), a resumed pass owns the
    // image; an in-flight zero-fill must not go on writing zeros over what it recovers.
    #[test]
    fn a_disowned_sink_writes_nothing_more_to_the_image() {
        let dir = scratch("disowned");
        let total = 4 * ZERO_CHUNK as u64;
        let (mut sink, iso) = sink_over(&dir, total);
        sink.map.disown_handle().disown();
        let _ = sink.apply(WorkItem::GapFill { pos: 0, len: total });
        let _ = sink.apply(WorkItem::Good {
            pos: 0,
            buf: vec![0x5Au8; 2048],
        });
        drop(sink);
        let got = std::fs::read(&iso).unwrap();
        assert!(
            got.iter().all(|&b| b == 0xAA),
            "an abandoned consumer overwrote the image a resumed pass now owns"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Under the mapfile's 1 s persist interval: a record made sooner stays in memory only.
    const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(900);

    // Persist `map` now, so the next record lands a full interval before the next persist.
    fn freshly_flushed(map: &mut Mapfile) -> std::time::Instant {
        map.record(0, 2048, SectorStatus::NonTried).unwrap();
        map.set_disc_hash(&"ab".repeat(20));
        map.flush().unwrap();
        std::time::Instant::now()
    }

    // An `apply` write error (often the latched writeback failure of an EARLIER item) means
    // the Finished records before it may not be on disk: the dropped sink must not flush them.
    #[test]
    fn a_failed_write_does_not_persist_earlier_finished_records() {
        for _ in 0..ATTEMPTS {
            let dir = scratch("writefail-drop");
            let (mut sink, iso) = sink_over(&dir, 8192);
            let read_only = std::fs::File::open(&iso).unwrap();
            sink.file = libfreemkv::io::WritebackFile::new(read_only).unwrap();
            let fresh = freshly_flushed(&mut sink.map);
            sink.map.record(0, 2048, SectorStatus::Finished).unwrap();
            let r = sink.apply(WorkItem::Good {
                pos: 2048,
                buf: vec![0x5Au8; 2048],
            });
            if fresh.elapsed() >= FLUSH_WINDOW {
                continue; // try again, as above
            }
            assert!(r.is_err(), "a write to a read-only handle must fail");
            drop(sink);
            let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
            assert!(
                reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
                "a failed write left earlier, possibly lost, ranges persisted Finished"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        panic!("no attempt stayed inside the mapfile's flush window");
    }

    // R2's in-loop check: a fill already under way stops when teardown disowns the map.
    #[test]
    fn a_fill_stops_when_disowned_mid_way() {
        const FILL: u64 = 256 << 20;
        let dir = scratch("disowned-mid");
        let (mut sink, iso) = sink_over(&dir, 8192);
        let disown = sink.map.disown_handle();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1));
            disown.disown();
        });
        let _ = sink.apply(WorkItem::GapFill { pos: 0, len: FILL });
        t.join().unwrap();
        drop(sink);
        let len = std::fs::metadata(&iso).unwrap().len();
        assert!(len < FILL, "the fill ran on past the disown ({len} bytes)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
