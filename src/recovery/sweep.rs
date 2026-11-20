//! The sweep producer's consumer-side `Sink<WorkItem>`.
//!
//! A producer/consumer split overlaps the SCSI read with decrypt +
//! file-write + mapfile fsync on the generic [`libfreemkv::io::Pipeline`] +
//! [`libfreemkv::io::Sink`] primitive. This module is the sweep-specific
//! `Sink` impl; the producer-side state machine stays with the producer —
//! `sweep_internal` in `recovery/mod.rs`.

use std::io::SeekFrom;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use libfreemkv::error::Error;
use libfreemkv::io::{Flow, Sink};

use super::ImageFile;
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
    /// failed batch portion of `JumpAhead`). Consumer makes `[pos, pos+len)` zero
    /// (writing zeros unless a fresh image already reads zero there) and records
    /// the range as `NonTrimmed`.
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
    /// The damage ranges, or `None` when unchanged since the last snapshot delivered:
    /// the producer then keeps what it derived from the earlier ones.
    pub bad_ranges: Option<Vec<(u64, u64)>>,
}

/// Final summary returned by the consumer thread on shutdown — what
/// `SweepSink::close` produces, surfaced to the producer via
/// `Pipeline::finish`.
pub(super) struct ConsumerSummary {
    pub stats: MapStats,
}

/// Drain any pending progress snapshots from the consumer. Returns
/// the most recent one, if any, carrying the newest damage ranges among
/// those drained when it has none of its own. The producer caches it
/// for subsequent progress callbacks until a fresh one arrives.
pub(super) fn try_recv_progress(rx: &Receiver<ProgressSnapshot>) -> Option<ProgressSnapshot> {
    let mut latest: Option<ProgressSnapshot> = None;
    while let Ok(mut snap) = rx.try_recv() {
        if snap.bad_ranges.is_none() {
            snap.bad_ranges = latest.and_then(|s| s.bad_ranges);
        }
        latest = Some(snap);
    }
    latest
}

// `Sink<WorkItem>` for sweep. Owns the writeback file + mapfile +
// progress back-channel. `apply` does the file-write + mapfile.record
// per item; `close` drains, fsyncs, and flushes the mapfile.
pub(super) struct SweepSink<F: ImageFile = libfreemkv::io::WritebackFile> {
    file: F,
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
    /// Sync-and-persist cadence ([`super::IMAGE_PERSIST_INTERVAL`]; tests shorten it).
    persist_every: std::time::Duration,
    /// Where a written `Good` buffer goes back to the producer for its next read.
    recycle: super::BufReturn,
    /// This pass created the image empty (truncated, then sized), so a byte no write of
    /// this sink reached reads as zero, and a fill over a NonTried range needs no write.
    fresh_image: bool,
    /// The map's damage epoch when damage ranges last reached the producer.
    sent_epoch: Option<u64>,
}

impl<F: ImageFile> SweepSink<F> {
    /// Return each written `Good` buffer through `recycle` instead of dropping it.
    pub(super) fn recycle_into(&mut self, recycle: super::BufReturn) {
        self.recycle = recycle;
    }

    /// Declare the image newly created empty by this pass (truncated and sized, never
    /// written since): fills over still-NonTried ranges then only record.
    pub(super) fn image_is_fresh(&mut self) {
        self.fresh_image = true;
    }

    // Every record goes through here, a fill's included: whichever record persists the map
    // must sync the image first, or earlier Finished records would outrun their data.
    fn record(&mut self, run: (u64, u64, SectorStatus)) -> Result<(), Error> {
        let every = self.persist_every;
        super::record_durably(&mut self.file, &mut self.map, self.is_regular, every, run)
    }

    /// Construct a new `SweepSink` plus the matching progress
    /// receiver. Channel depth on the back-channel is `1` — the
    /// producer's cache is the source of truth between snapshots.
    pub(super) fn new(
        file: F,
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
            persist_every: super::IMAGE_PERSIST_INTERVAL,
            recycle: super::BufReturn::default(),
            fresh_image: false,
            sent_epoch: None,
        };
        (sink, prog_rx)
    }
}

// No `close_stopped` override (the default `close`): this sink renames nothing, and a
// data-less mapfile flush would claim sectors not yet durable. T8: "halted: **Stopped**
// (not a failure)" after one 5 s grace; the disowned mapfile keeps the resumable record.
impl<F: ImageFile> Sink<WorkItem> for SweepSink<F> {
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
                self.recycle.give(buf);
                self.record((pos, len, SectorStatus::Finished))?;
            }
            WorkItem::SkipFill { pos, len } | WorkItem::GapFill { pos, len }
                if self.fresh_image && self.map.is_all(pos, len, SectorStatus::NonTried) =>
            {
                // Already zero: this pass created the image empty and nothing wrote here
                // (every write this sink makes is recorded non-NonTried right after).
                self.record((pos, len, SectorStatus::NonTrimmed))?;
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
                self.record((pos, len, SectorStatus::NonTrimmed))?;
            }
            WorkItem::StatsRequest => {
                let stats = self.map.stats();
                // DAMAGE only — NOT NonTried (unread remainder ahead of the
                // sweep head): including it made the live drilldown treat the
                // whole unread disc as damage at sweep start, melting to 0.
                let epoch = self.map.damage_epoch();
                let bad_ranges = (self.sent_epoch != Some(epoch)).then(|| {
                    self.map
                        .ranges_with(&crate::recovery::mapfile::damage_sector_statuses())
                });
                let carries = bad_ranges.is_some();
                // Best-effort: drop on backpressure; producer's cache
                // stays current enough.
                let sent = self
                    .prog_tx
                    .try_send(ProgressSnapshot { stats, bad_ranges });
                if sent.is_ok() && carries {
                    self.sent_epoch = Some(epoch);
                }
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
#[path = "sweep_tests.rs"]
mod tests;
