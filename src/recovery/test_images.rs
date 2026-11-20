//! In-memory [`ImageFile`]s for the sweep and patch consumer tests and benches.

use std::io::{self, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::ImageFile;

/// What a [`MemImage`] holds: `written` takes every write, and `durable` is what a crash
/// keeps (a copy taken at each successful `sync_all`).
#[derive(Default)]
pub(crate) struct MemState {
    pos: u64,
    pub written: Vec<u8>,
    pub durable: Vec<u8>,
    pub syncs: usize,
    /// Accept this many more bytes, then fail mid-write (a partial write).
    pub fail_after: Option<usize>,
    /// Every `sync_all` fails.
    pub fail_sync: bool,
}

/// A shared in-memory image: a test keeps one clone to inspect after the sink moves.
#[derive(Clone, Default)]
pub(crate) struct MemImage(Arc<Mutex<MemState>>);

impl MemImage {
    /// An image of `len` bytes of `fill`, already durable.
    pub(crate) fn filled(len: usize, fill: u8) -> Self {
        let img = Self::default();
        {
            let mut s = img.state();
            s.written = vec![fill; len];
            s.durable = s.written.clone();
        }
        img
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, MemState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Write for MemImage {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.state();
        let n = match s.fail_after {
            Some(0) => return Err(io::Error::other("injected write failure")),
            Some(left) => buf.len().min(left),
            None => buf.len(),
        };
        if let Some(left) = s.fail_after.as_mut() {
            *left -= n;
        }
        let at = s.pos as usize;
        if s.written.len() < at + n {
            s.written.resize(at + n, 0);
        }
        s.written[at..at + n].copy_from_slice(&buf[..n]);
        s.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemImage {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let mut s = self.state();
        s.pos = match to {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => s.pos.saturating_add_signed(d),
            SeekFrom::End(d) => (s.written.len() as u64).saturating_add_signed(d),
        };
        Ok(s.pos)
    }
}

impl ImageFile for MemImage {
    fn sync_all(&mut self) -> io::Result<()> {
        let mut s = self.state();
        if s.fail_sync {
            return Err(io::Error::other("injected sync failure"));
        }
        s.syncs += 1;
        s.durable = s.written.clone();
        Ok(())
    }
}

/// Simulates a crash now: every range the mapfile at `map_path` calls Finished must already
/// hold its `expected` bytes in the image's durable copy.
pub(crate) fn assert_crash_safe(map_path: &std::path::Path, img: &MemImage, expected: &[u8]) {
    let on_disk = super::mapfile::Mapfile::load(map_path).unwrap();
    let durable = img.state().durable.clone();
    for (pos, len) in on_disk.ranges_with(&[super::mapfile::SectorStatus::Finished]) {
        let r = pos as usize..(pos + len) as usize;
        assert!(
            durable.get(r.clone()) == expected.get(r),
            "the map on disk calls {pos:#x}+{len:#x} Finished before its data was durable"
        );
    }
}

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
