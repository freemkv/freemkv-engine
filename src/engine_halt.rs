//! The engine's one cancellation view (stop design v5 §4.2) and the result shape of its
//! `_with` entries (§2.6).
//!
//! [`EngineHalt`] is the op token OR'd with an optional narrower flag (the option structs'
//! `halt` fields, unchanged) and, on the remux path, the Sink's `should_cancel` probe.
//! Every cancellation read in engine production code goes through
//! [`EngineHalt::is_cancelled`]; the patch pass latch is exempt (§4.2).

use crate::sink::Sink;
use libfreemkv::halt::{Halt, WAIT_SLICE};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The op token, an optional narrower flag, and (remux) the Sink probe.
///
/// §4.2: "`EngineHalt { op, extra }`, with `is_cancelled() = op || extra`". The op token
/// is always observed, so Stop never depends on `extra` being linked.
pub struct EngineHalt<'a> {
    op: Halt,
    // The caller supplied no op token (a legacy entry): `op` is private and never cancelled.
    op_private: bool,
    extra: Option<Arc<AtomicBool>>,
    // §4.2: "The sink probe is an internal `pub(crate)` field of `EngineHalt`".
    pub(crate) sink: Option<&'a dyn Sink>,
}

impl std::fmt::Debug for EngineHalt<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHalt")
            .field("op", &self.op)
            .field("op_private", &self.op_private)
            .field("extra", &self.extra)
            .field("sink", &self.sink.is_some())
            .finish()
    }
}

impl EngineHalt<'static> {
    /// The view a `_with` entry builds from its `&Halt` and the options' `halt` field.
    pub fn new(op: &Halt, extra: Option<Arc<AtomicBool>>) -> Self {
        Self {
            op: op.clone(),
            op_private: false,
            extra,
            sink: None,
        }
    }

    /// [`new`](Self::new) for a caller that holds the Drive: in debug builds, asserts the
    /// op token is the Drive's (§4.2: "`debug_assert!(ptr_eq(op, drive.token()))`").
    pub fn for_drive(op: &Halt, extra: Option<Arc<AtomicBool>>, drive: Option<&Halt>) -> Self {
        debug_assert!(
            drive.is_some_and(|t| Arc::ptr_eq(t.as_arc(), op.as_arc())),
            "the op token must be the Drive's token (stop design §4.2)"
        );
        Self::new(op, extra)
    }

    // A legacy entry (no op token): only `extra` can cancel, exactly as before ST-E1.
    pub(crate) fn legacy(extra: Option<Arc<AtomicBool>>) -> Self {
        Self {
            op: Halt::new(),
            op_private: true,
            extra,
            sink: None,
        }
    }
}

impl<'a> EngineHalt<'a> {
    // Adds the Sink probe (§4.2: "`op.is_cancelled() || extra || sink.should_cancel()`").
    pub(crate) fn with_sink<'b>(self, sink: &'b dyn Sink) -> EngineHalt<'b>
    where
        'a: 'b,
    {
        EngineHalt {
            op: self.op,
            op_private: self.op_private,
            extra: self.extra,
            sink: Some(sink),
        }
    }

    /// `op || extra || sink.should_cancel()`.
    pub fn is_cancelled(&self) -> bool {
        self.op.is_cancelled()
            || self
                .extra
                .as_ref()
                .is_some_and(|e| e.load(Ordering::Acquire))
            || self.sink.is_some_and(|s| s.should_cancel())
    }

    /// The caller's op token (a private, never-cancelled one for a legacy entry).
    pub fn op(&self) -> &Halt {
        &self.op
    }

    // Anything but a legacy entry's private token can cancel.
    pub(crate) fn is_wired(&self) -> bool {
        !self.op_private || self.extra.is_some() || self.sink.is_some()
    }

    // Sleeps `d` in `WAIT_SLICE` slices; `true` when a cancel cut it short.
    pub(crate) fn wait(&self, d: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.is_cancelled() {
                return true;
            }
            let left = d.saturating_sub(start.elapsed());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(WAIT_SLICE));
        }
    }

    // One libfreemkv `Halt` that is cancelled once `self` is: the token itself when it is the
    // only input, else a child a bridge thread cancels within one `WAIT_SLICE`.
    pub(crate) fn linked<R>(&self, f: impl FnOnce(&Halt) -> R) -> R {
        match (&self.extra, self.sink, self.op_private) {
            (None, None, _) => f(&self.op),
            (Some(extra), None, true) => f(&Halt::from_arc(extra.clone())),
            _ => self.bridged(f),
        }
    }

    fn bridged<R>(&self, f: impl FnOnce(&Halt) -> R) -> R {
        let child = Halt::new();
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    if self.is_cancelled() {
                        child.cancel();
                        return;
                    }
                    std::thread::sleep(WAIT_SLICE);
                }
            });
            let _done = crate::run::SignalDone(&done);
            f(&child)
        })
    }
}

/// How a `_with` entry ended (§2.6): Stop is **Stopped**, never a failure.
#[derive(Debug)]
#[must_use]
pub enum EngineOutcome<T> {
    /// The op ran to its end.
    Done(T),
    /// A cancel ended the op; the partial result, when the op returns one.
    Stopped(Option<T>),
    /// Any error, including every `TimedOut` (§2.6: "`TimedOut` → **Failed** always").
    Failed(libfreemkv::Error),
}

impl<T> EngineOutcome<T> {
    /// Map a `_with` entry's result. `halted` reads an artifact result's own Stop flag.
    ///
    /// §2.6: "`EngineOutcome` maps `Halted` → `Stopped` only when the op token is
    /// cancelled. Otherwise it `debug_assert!`s and maps to `Failed`."
    pub(crate) fn from_result(
        r: crate::Result<T>,
        halt: &EngineHalt<'_>,
        halted: impl FnOnce(&T) -> bool,
    ) -> Self {
        match r {
            Ok(t) if halted(&t) => EngineOutcome::Stopped(Some(t)),
            Ok(t) => EngineOutcome::Done(t),
            Err(libfreemkv::Error::Halted) if halt.is_cancelled() => EngineOutcome::Stopped(None),
            Err(libfreemkv::Error::Halted) => {
                debug_assert!(false, "Halted with no cancel (stop design §2.6)");
                EngineOutcome::Failed(libfreemkv::Error::Halted)
            }
            Err(e) => EngineOutcome::Failed(e),
        }
    }

    /// The result, whichever way the op ended.
    pub fn value(&self) -> Option<&T> {
        match self {
            EngineOutcome::Done(t) | EngineOutcome::Stopped(Some(t)) => Some(t),
            _ => None,
        }
    }

    /// `true` for [`EngineOutcome::Stopped`].
    pub fn is_stopped(&self) -> bool {
        matches!(self, EngineOutcome::Stopped(_))
    }
}

#[cfg(test)]
mod tests;
