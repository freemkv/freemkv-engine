use libfreemkv::error::Error;
use libfreemkv::io::pipeline::{Flow, Sink};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// A gate a test thread can hold shut and then open.
#[derive(Clone)]
pub(super) struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub(super) fn shut() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }
    pub(super) fn wait(&self) {
        let (lock, cv) = &*self.0;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    }
    pub(super) fn open(&self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

/// The hung-mount consumer: alive, healthy, and stuck in its first `apply` until the
/// gate opens. Stands in for `SweepSink`/`PatchSink` blocked inside
/// `WritebackFile::write_all` on a mount that never answers.
pub(super) struct StalledSink {
    pub(super) entered: Arc<AtomicUsize>,
    pub(super) gate: Gate,
    /// Counts `close()` calls.
    pub(super) closed: Arc<AtomicUsize>,
    /// Set once the consumer thread has let go of the sink, closed or not.
    pub(super) dropped: Arc<AtomicBool>,
}

impl StalledSink {
    pub(super) fn new(entered: &Arc<AtomicUsize>, gate: &Gate) -> Self {
        StalledSink {
            entered: Arc::clone(entered),
            gate: gate.clone(),
            closed: Arc::default(),
            dropped: Arc::default(),
        }
    }
}

impl Sink<u32> for StalledSink {
    type Output = u32;
    fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.gate.wait();
        Ok(Flow::Continue)
    }
    fn close(self) -> std::result::Result<u32, Error> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
}

impl Drop for StalledSink {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}
