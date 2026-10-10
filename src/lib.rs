//! Shared rip orchestration and recovery strategy for freemkv front-ends.
//!
//! [`libfreemkv`] provides drive access, parsing, decryption, muxing and single-shot
//! reads. This crate owns sweep/patch retries, mapfiles, preflight and job execution.
//! Front-ends supply a [`Job`] and [`Sink`]; diagnostics flow through the sink.
//! [`preflight()`] reports whether a job can run as data, without printing.

// App-layer (unlike libfreemkv): may carry English diagnostic text; front-ends localize via the
// message + code carried on events.
#![forbid(unsafe_code)]

pub mod drive;
pub mod drive_info;
mod engine_halt;
mod episodes;
mod extract;
mod image;
mod job;
pub mod keys;
mod multipass;
mod mux;
mod outcome;
mod plan;
mod preflight;
mod presentation;
// The recovery strategy (sweep/patch/mapfile/read_error/section_recover).
mod recovery;

// Recovery primitives (relocated from libfreemkv). `multipass_rip` drives
// sweep/patch for the common case; a consumer that must interleave its own
// work between passes (staging, resume, a watchdog) drives them directly.
pub use recovery::mapfile::{MapStats, Mapfile, SectorStatus, mapfile_path_for, vid_fingerprint};
pub use recovery::{
    CopyOptions, CopyResult, PatchOptions, PatchOutcome, SweepOptions,
    bytes_bad_in_title_from_mapfile, copy, copy_with, ensure_titles_staged, ensure_whole_image,
    patch, patch_with, progress_snapshot_from_mapfile, sweep, sweep_scoped, sweep_with,
};
#[cfg(test)]
mod ku_image_tests;
#[cfg(test)]
mod parity_tests;
mod remux;
mod resolve;
mod run;
mod sink;
mod speed;
mod streams;
#[cfg(test)]
mod test_fixtures;

pub use drive_info::{CapturedFeature, DriveCapture, capture_drive_data, mask_bytes, mask_string};
pub use engine_halt::{EngineHalt, EngineOutcome};
pub use episodes::{TitleRole, episode_titles, title_roles};
pub use extract::{extract_tree, extract_tree_with};
pub use image::{
    ImageSource, KeyInput, OpenImageOptions, OpenedImage, error_code, open_image, open_image_with,
    open_image_with_traced, parse_error_code, scan_image, stream_info,
};
pub use job::{Job, RipMode, Selection, StreamChoice, StreamFilter};
pub use keys::{KeyParams, key_source_factory, key_sources, resolve_loose_clip, won_source};
pub use multipass::{
    LossVerdict, MultipassOpts, MultipassResult, PassExit, PassHost, PassPlan, PatchDecision,
    ReaderHost, abort_lost_bytes, abort_lost_ms, bad_sector_statuses, classify_damage,
    effective_abort_secs, end_of_recovery_promotion, loss_aborts, loss_verdict, lost_ms_in_title,
    measured_scope_bad, mkv_staging_scope, multipass_rip, multipass_rip_staged, multipass_rip_with,
    pass_exit, patch_made_progress, patch_pass_decision, plan_passes, pre_pass_converged,
    scope_bad_bytes, scope_converged, should_abort_for_loss, title_bytes_per_sec,
};
pub use mux::{
    RipOutcome, TitleAction, TitleError, TitleResult, classify_title_error, decide_title,
    mux_title, open_scan, open_scan_with, resolve_job_selection, resolve_selection,
    resolve_selection_with_audio, run_episodes, run_titles, run_titles_with,
};
pub use outcome::{DamageSeverity, KeyStatus, Outcome, RipFile};
pub use plan::{Held, KeyParamsData, Output, Plan, Report, RunWith, TitleOptions, run, run_with};
pub use preflight::{Preflight, Reason, preflight};
pub use remux::{
    MuxPlan, RemuxJob, RemuxReport, mux_image_titles, mux_image_titles_with, mux_options,
    remux_iso, remux_iso_staged, remux_iso_with, verify_mkv,
};
pub use run::recover_to_iso;
pub use sink::{Event, Level, NoopSink, Progress, RecoveryEvent, Sink};
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
