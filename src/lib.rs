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
//! The boundary rationale — why sweep/patch/mapfile/damage-classification are
//! strategy, not primitives, and what's a small deliberate `pub` promotion in
//! the lib vs a physical relocation here — is captured in the project's
//! internal design notes.
//!
//! ## Two hard rules the API enforces
//!
//! 1. **Nothing prints.** Every diagnostic flows through the [`Sink`]; the
//!    engine never writes to stdout/stderr. A GUI has no other way to surface
//!    it.
//! 2. **Preflight is callable without executing.** [`preflight()`] answers "can
//!    this job run, and if not why" as *data*, so a UI can keep a Start button
//!    honest on every selection change — without side effects.

// The engine is app-layer, so (unlike libfreemkv) it may carry English text in
// diagnostics. Front-ends localize via the message + code carried on events.

// No reason for this crate to use `unsafe`; a prior test helper's SAFETY note
// justified the wrong condition for `std::env::set_var` (env access must be
// single-threaded process-wide). `forbid`, not `deny`, so it can't be re-allowed.
#![forbid(unsafe_code)]

pub mod drive_info;
mod extract;
mod job;
mod keys;
mod multipass;
mod mux;
mod outcome;
mod preflight;
// Relocated recovery strategy (sweep/patch/mapfile/read_error/section_recover).
// Some faithfully-relocated internals have no in-crate caller yet — allow
// dead_code here rather than diverge from the byte-faithful move.
#[allow(dead_code)]
mod recovery;

// Recovery primitives (relocated from libfreemkv). `multipass_rip` drives
// sweep/patch for the common case; a consumer that must interleave its own
// work between passes (autorip's staging/resume/watchdog) drives them directly.
pub use recovery::mapfile::{MapStats, Mapfile, SectorStatus, mapfile_path_for};
pub use recovery::{
    CopyOptions, CopyResult, PatchOptions, PatchOutcome, SweepOptions,
    bytes_bad_in_title_from_mapfile, copy, patch, progress_snapshot_from_mapfile, sweep,
};
mod resolve;
mod run;
mod sink;
mod speed;
mod streams;

pub use drive_info::{CapturedFeature, DriveCapture, capture_drive_data, mask_bytes, mask_string};
pub use extract::extract_tree;
pub use job::{Job, RipMode, Selection, StreamChoice, StreamFilter};
pub use keys::{KeyParams, key_source_factory, key_sources, resolve_disc_keys, won_source};
pub use multipass::{
    MultipassOpts, MultipassResult, PassExit, PassPlan, PatchDecision, abort_lost_bytes,
    abort_lost_ms, bad_sector_statuses, classify_damage, effective_abort_secs,
    end_of_recovery_promotion, loss_aborts, multipass_rip, pass_exit, patch_made_progress,
    patch_pass_decision, plan_passes, scope_bad_bytes, scope_converged, should_abort_for_loss,
};
pub use mux::{
    RipOutcome, TitleAction, TitleResult, classify_title_error, decide_title, mux_title,
    mux_title_session, open_scan_resolve, resolve_selection, run_titles,
};
pub use outcome::{DamageSeverity, KeyStatus, Outcome, RipFile};
pub use preflight::{Preflight, Reason, preflight};
pub use resolve::resolve_keys;
pub use run::recover_to_iso;
pub use sink::{Level, NoopSink, Progress, Sink};
pub use speed::SpeedEstimator;
pub use streams::{
    StreamSelError, SubtitleFilter, UnmatchedClass, resolve_stream_selection,
    resolve_stream_selection_forced,
};

// Re-exports so a front-end can depend on the engine alone: a UI needs the
// disc-model and cancellation types to render titles/streams and wire a
// Cancel button, without a *direct* libfreemkv dependency just for these.
pub use libfreemkv::{
    AudioStream, Codec, Disc, DiscFormat, DiscTitle, Halt, PidFilter, Resolution, Stream,
    StreamSelection, SubtitleStream, VideoStream,
};

/// The engine's result type. Errors are [`libfreemkv::Error`] — a typed enum
/// with a numeric `code()` and no English text — so front-ends map codes to
/// localized messages exactly as they do for the library.
pub type Result<T> = std::result::Result<T, libfreemkv::Error>;
