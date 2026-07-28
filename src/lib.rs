//! # freemkv-engine
//!
//! The freemkv **rip engine**: the disc→MKV orchestration strategy that every
//! front-end shares. It sits *above* [`libfreemkv`]'s public API — composing
//! `DiscSession`, `scan_iso`, `resolve_keys_for`, `Disc::sweep`/`patch`, and
//! `mux_stream` — and *below* the front-ends (CLI, autorip, desktop UI), each
//! of which is a thin shell that supplies a [`Sink`] and a [`Job`].
//!
//! ## Layering
//!
//! ```text
//! libfreemkv     ← library: SCSI, parse, decrypt, mux highway, recovery PRIMITIVES
//! freemkv-engine ← THIS crate: rip STRATEGY (multipass, job model, preflight, Sink)
//!    ├── freemkv   ← CLI front-end
//!    ├── autorip   ← service front-end (polling/staging/resume/web)
//!    └── freemkv-gui ← desktop front-end (future)
//! ```
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
mod outcome;
mod sink;

pub use job::{Job, RipMode, Selection};
pub use outcome::{DamageSeverity, KeyStatus, Outcome, RipFile};
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
