//! In-memory [`ImageFile`]s for the sweep and patch consumer tests and benches.

use std::io::{self, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::ImageFile;

/// A data-discarding image for benches: counts bytes and syncs, each sync taking `sync_cost`.
#[derive(Clone, Default)]
pub(crate) struct NullImage {
    pub counts: Arc<Mutex<(u64, usize)>>,
    pub sync_cost: Duration,
}

impl NullImage {
    /// `(bytes written, syncs)` so far.
    pub(crate) fn counts(&self) -> (u64, usize) {
        *self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Write for NullImage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for NullImage {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        Ok(match to {
            SeekFrom::Start(p) => p,
            _ => 0,
        })
    }
}

impl ImageFile for NullImage {
    fn sync_all(&mut self) -> io::Result<()> {
        std::thread::sleep(self.sync_cost);
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).1 += 1;
        Ok(())
    }
}
