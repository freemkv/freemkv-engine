//! # freemkv-engine
//!
//! The freemkv **rip engine**: freemkv's recovery STRATEGY (sweep, patch, the
//! retry-decision state machine, mapfile bookkeeping, damage-severity
//! judgment) plus the rip ORCHESTRATION shared by every front-end. It sits
//! *above* [`libfreemkv`]'s public API — composing `DiscSession`, `scan_iso`,
//! `resolve_keys_for`, and `mux_stream` — and *below* the front-ends (CLI,
//! autorip, desktop UI), each of which is a thin shell that supplies a
//! [`Sink`] and a [`Job`].
//!
//! ## Layering
//!
//! ```text
//! libfreemkv     ← library: SCSI, parse, decrypt, mux highway, the raw read
//!                  primitive (single-shot, no retries) + SCSI-fact
//!                  translation (SenseFamily)
//! freemkv-engine ← THIS crate: recovery STRATEGY (sweep/patch/mapfile/
//!                  retry-decisions/damage-severity) + rip ORCHESTRATION
//!                  (multipass, job model, preflight, Sink)
//!    ├── freemkv   ← CLI front-end
//!    ├── autorip   ← service front-end (polling/staging/resume/web)
//!    └── freemkv-gui ← desktop front-end (future)
//! ```
//!
//! See `freemkv-private/audit/engine-split/DESIGN.md` for the full boundary
//! rationale (why sweep/patch/mapfile/damage-classification are strategy, not
//! primitives, and what's a small deliberate `pub` promotion in the lib vs a
//! physical relocation here).
//!
//! ## Two hard rules the API enforces
//!
//! 1. **Nothing prints.** Every diagnostic flows through the [`Sink`]; the
//!    engine never writes to stdout/stderr. A GUI has no other way to surface
//!    it.
//! 2. **Preflight is callable without executing.** [`preflight`] answers "can
//!    this job run, and if not why" as *data*, so a UI can keep a Start button
//!    honest on every selection change — without side effects.

// The engine is app-layer, so (unlike libfreemkv) it may carry English text in
// diagnostics. Front-ends localize via the message + code carried on events.

mod job;
mod multipass;
mod outcome;
mod preflight;
// The relocated recovery strategy (sweep/patch/mapfile/read_error/
// section_recover). `run()` drives the multipass dispatch (`recovery::copy`);
// some producer/consumer plumbing and Pass-N-only helpers still have no
// in-crate caller until the full run() surface lands, so allow dead_code at
// the module boundary for now.
#[allow(dead_code)]
mod recovery;
mod resolve;
mod run;
mod sink;

pub use job::{Job, RipMode, Selection};
pub use multipass::{
    MultipassResult, classify_damage, effective_abort_secs, loss_aborts, run_multipass,
    should_abort_for_loss,
};
pub use outcome::{DamageSeverity, KeyStatus, Outcome, RipFile};
pub use preflight::{Preflight, Reason, preflight};
pub use resolve::resolve_keys;
pub use run::recover_to_iso;
pub use sink::{Level, NoopSink, Progress, Sink};

// ─── Re-exports so a front-end can depend on the engine alone ────────────────
//
// A UI (or the CLI) building on the engine needs the disc-model and cancellation
// types to render titles/streams and wire a Cancel button. Re-export them here
// so front-ends never have to add a *direct* libfreemkv dependency just for
// these — the engine is their substrate.
pub use libfreemkv::{
    AudioStream, Codec, Disc, DiscFormat, DiscTitle, Halt, Resolution, Stream, SubtitleStream,
    VideoStream,
};

/// The engine's result type. Errors are [`libfreemkv::Error`] — a typed enum
/// with a numeric `code()` and no English text — so front-ends map codes to
/// localized messages exactly as they do for the library.
pub type Result<T> = std::result::Result<T, libfreemkv::Error>;
