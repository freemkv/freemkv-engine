//! Shared rip orchestration and recovery strategy for freemkv front-ends.
//!
//! [`libfreemkv`] provides drive access, parsing, decryption, muxing and single-shot
//! reads. This crate owns sweep/patch retries, mapfiles, preflight and job execution.
//! Front-ends supply a [`Job`] and [`Sink`]; diagnostics flow through the sink.
//! [`preflight()`] reports whether a job can run as data, without printing.

// App-layer (unlike libfreemkv): may carry English diagnostic text; front-ends localize via the
// message + code carried on events.
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
