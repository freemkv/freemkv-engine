//! ISO/disc → MKV muxing, the multi-title rip loop, and the drive bring-up
//! ([`open_scan`]) the disc path starts from.
//!
//! Resolves which titles to rip, muxes each through `libfreemkv::mux_with_keys`,
//! and decides when a failure is fatal vs skippable. Three load-bearing
//! behaviours: fail-fast on a disc-level key failure (every title would fail
//! identically), cancel is a full stop (not a per-title cancel), and a
//! main-title default (via [`Selection`]) so an obfuscated disc doesn't rip
//! everything by accident.

use crate::job::{Job, Selection, StreamFilter};
use crate::sink::{Level, Sink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Human-readable byte count for a diagnostic line — GB/MB/KB, so a rip size
/// reads "~51.7 GB" instead of "~55460235264 bytes". A value that would round
/// to 1024 of a unit reads in the next one ("1 MB", not "1024 KB").
fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    let (mb, kb) = (f / (K * K), f / K);
    if f >= K * K * K || mb.round() >= K {
        format!("{:.1} GB", f / (K * K * K))
    } else if f >= K * K || kb.round() >= K {
        format!("{mb:.0} MB")
    } else if f >= K {
        format!("{kb:.0} KB")
    } else {
        format!("{b} B")
    }
}

/// `s` for a log line: control characters (newlines, terminal escapes) escaped, so
/// disc- or error-borne text cannot forge lines or drive a terminal.
pub(crate) fn log_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve a [`Selection`] to concrete 0-based title indices against a scanned
/// disc. Out-of-range explicit indices are dropped here (preflight surfaces
/// them as blocking reasons before we get here). `MainMovie` is title 0 (the
/// canonical main feature — first in every freemkv title list). `Longest` is
/// the max-duration title (may differ from the canonical feature on odd
/// authoring).
pub fn resolve_selection(disc: &libfreemkv::Disc, sel: &Selection) -> Vec<usize> {
    crate::SelectionModel::from_disc(disc)
        .select(sel, &StreamFilter::All)
        .indices
}

/// Resolve a job's selection, ranking equivalent main or episode presentations
/// by requested audio-language coverage. Ties retain the first presentation.
/// Explicit titles, All and Longest retain their selection semantics.
pub fn resolve_job_selection(disc: &libfreemkv::Disc, job: &Job) -> Vec<usize> {
    resolve_selection_with_audio(disc, &job.selection, &job.streams.audio)
}

/// Like [`resolve_selection`], but ranks equivalent main/episode presentations
/// by requested audio-language coverage. Missing identity never equates titles.
pub fn resolve_selection_with_audio(
    disc: &libfreemkv::Disc,
    selection: &Selection,
    audio: &StreamFilter,
) -> Vec<usize> {
    crate::SelectionModel::from_disc(disc)
        .select(selection, audio)
        .indices
}

/// The result of muxing one title, from the loop's point of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleResult {
    /// Muxed successfully.
    Ok,
    /// A DISC-LEVEL key failure — the whole disc can't be decrypted, so every
    /// remaining title would fail identically. The loop stops immediately
    /// (fail-fast) instead of iterating and re-printing the same error.
    DiscLevelNoKey,
    /// Failed, but skippably — an uncrackable/empty per-title stub (a menu
    /// loop, an FBI-warning nav title). Skipped on a non-feature title in a
    /// multi-title rip; fatal if it's the feature or an explicit selection.
    SkippableStub,
    /// A hard failure for this title (not a stub).
    Failed,
    /// The rip was cancelled (halt) during this title.
    Halted,
}

/// Classify the `io::Error` a single-title mux returned into a [`TitleResult`].
/// Uses libfreemkv's typed classifiers — never string-matches E-codes. Order
/// matters: halt first (a user stop wins), then disc-level no-key (fail-fast),
/// then the per-title skippable stub, else a hard failure.
pub fn classify_title_error(e: &std::io::Error) -> TitleResult {
    if libfreemkv::is_halt(e) {
        TitleResult::Halted
    } else if libfreemkv::is_disc_level_no_key(e) {
        TitleResult::DiscLevelNoKey
    } else if libfreemkv::is_skippable_title_stub(e) {
        TitleResult::SkippableStub
    } else {
        TitleResult::Failed
    }
}

/// What the loop should DO about one title's [`TitleResult`]. The single
/// source of the multi-title loop policy — [`run_titles`] uses it, and a
/// front-end with its own loop + error rendering (the CLI) calls it directly so
/// the policy is never duplicated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleAction {
    /// The title muxed (or should be treated as done); move on.
    Continue,
    /// A skippable stub on a non-feature title in a multi-title, non-explicit
    /// rip — skip it with a notice and keep going.
    Skip,
    /// Cancelled — stop the WHOLE rip (full stop, not per-title).
    StopHalt,
    /// Disc-level key failure — stop; every remaining title fails identically.
    StopNoKey,
    /// A hard failure on a title the user wanted — stop and surface it.
    StopFatal,
}

/// Decide what to do about one title's result. `is_feature` = title index 0;
/// `multi_title` = more than one title selected; `explicit_selection` = the
/// user named specific titles. This is THE loop policy, shared by [`run_titles`]
/// and any front-end that drives its own loop.
pub fn decide_title(
    result: &TitleResult,
    is_feature: bool,
    multi_title: bool,
    explicit_selection: bool,
) -> TitleAction {
    match result {
        TitleResult::Ok => TitleAction::Continue,
        TitleResult::Halted => TitleAction::StopHalt,
        TitleResult::DiscLevelNoKey => TitleAction::StopNoKey,
        TitleResult::SkippableStub if !is_feature && multi_title && !explicit_selection => {
            TitleAction::Skip
        }
        TitleResult::SkippableStub | TitleResult::Failed => TitleAction::StopFatal,
    }
}

/// The terminal outcome of a whole multi-title rip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RipOutcome {
    /// Every selected title that mattered succeeded (skippable stubs may have
    /// been skipped). Carries the count actually written.
    ///
    /// `titles_written: 0` is NOT a rip: no file was produced, either because
    /// the selection was empty or because every title in it was a skippable
    /// stub. It is still `Ok` — nothing failed — but a front-end must branch on
    /// the count before it reports success to the user. `run_titles` logs an
    /// Error line naming which of the two happened.
    Ok { titles_written: usize },
    /// A disc-level key failure surfaced — the whole disc can't be decrypted,
    /// so the loop stopped (fail-fast) rather than iterate every title.
    NoKey,
    /// A title the user wanted (the feature, or an explicit `-t`) failed hard.
    ///
    /// Carries both `code` and `kind` because libfreemkv reports a failure's cause two
    /// different ways depending on its origin.
    Failed {
        title_index: usize,
        /// libfreemkv's numeric code, when the error carried one.
        code: Option<u16>,
        /// The `io::ErrorKind`, which is where a passthrough OS error
        /// (`StorageFull`, `PermissionDenied`) keeps its meaning.
        kind: std::io::ErrorKind,
        /// The coded error's data (`E<code>: <data>`, e.g. a disc hash), language-neutral;
        /// empty for an uncoded error or one with no data.
        data: String,
    },
    /// The rip was cancelled — a full stop, not a per-title cancel.
    Halted,
}

/// The data of a libfreemkv error's `E<code>: <data>` form; empty when it has none.
pub(crate) fn error_data(e: &std::io::Error) -> String {
    let text = e.to_string();
    crate::parse_error_code(&text).map_or_else(String::new, |(_, d)| d.to_string())
}

/// Drive the multi-title rip loop. `mux_one(idx) -> io::Result<()>` muxes a
/// single title; injecting it keeps the loop's control flow unit-testable
/// without a real ISO. Production passes [`mux_title`] (or the consumer's
/// own single-title mux). Self-contained — no `Disc` needed.
///
/// Fails fast on a disc-level key error, treats cancel as a full stop, and skips skippable
/// stubs only on a non-feature title in a multi-title, non-explicit rip (fatal otherwise).
/// `explicit_selection` is `true` when the user named specific titles.
pub fn run_titles<F>(
    indices: &[usize],
    explicit_selection: bool,
    sink: &dyn Sink,
    mut mux_one: F,
) -> RipOutcome
where
    F: FnMut(usize) -> std::io::Result<()>,
{
    run_titles_with(indices, explicit_selection, sink, |idx| {
        mux_one(idx).map_err(TitleError::from)
    })
}

/// A title's failure as [`run_titles_with`] decides on it: the policy class and the error.
#[derive(Debug)]
pub struct TitleError {
    /// How the loop's policy ([`decide_title`]) classifies the failure.
    pub result: TitleResult,
    /// The error the title failed with, as a front end renders it.
    pub error: std::io::Error,
}

impl From<std::io::Error> for TitleError {
    fn from(error: std::io::Error) -> Self {
        TitleError {
            result: classify_title_error(&error),
            error,
        }
    }
}

/// [`run_titles`] for a front end that classifies its own per-title failures (a setup error
/// that is always fatal, a Stop): `mux_one` returns the [`TitleError`] it decided.
pub fn run_titles_with<F>(
    indices: &[usize],
    explicit_selection: bool,
    sink: &dyn Sink,
    mux_one: F,
) -> RipOutcome
where
    F: FnMut(usize) -> Result<(), TitleError>,
{
    title_loop(indices, explicit_selection, false, sink, mux_one)
}

/// The loop for a disc's episodes beside its main title (a TV disc's fan-out): an episode
/// that fails is reported ([`crate::Event::TitleFailed`]) and dropped, and the rest still
/// run. A Stop and a disc with no key still end the loop.
pub fn run_episodes<F>(indices: &[usize], sink: &dyn Sink, mux_one: F) -> RipOutcome
where
    F: FnMut(usize) -> Result<(), TitleError>,
{
    title_loop(indices, false, true, sink, mux_one)
}

fn title_loop<F>(
    indices: &[usize],
    explicit_selection: bool,
    keep_going: bool,
    sink: &dyn Sink,
    mut mux_one: F,
) -> RipOutcome
where
    F: FnMut(usize) -> Result<(), TitleError>,
{
    let multi_title = indices.len() > 1;
    let mut titles_written = 0usize;
    // A hard failure `keep_going` passed over: the rip's outcome when no title was written.
    let mut last_failure = None;

    for &idx in indices {
        // (2) Poll for a full-stop between titles.
        if sink.should_cancel() {
            sink.log(Level::Info, "cancelled — stopping the whole rip");
            return RipOutcome::Halted;
        }

        let is_feature = idx == 0;
        // Keep the rendered cause alive past the classification: the error
        // itself is consumed by `classify_title_error` and was then dropped,
        // taking the only description of WHY the title failed with it.
        let mut fail_detail = String::new();
        // The code has to be taken here too: `classify_title_error` consumes
        // the error into a coarse verdict, and the typed cause is gone after.
        let mut fail_code = None;
        let mut fail_kind = std::io::ErrorKind::Other;
        let mut fail_data = String::new();
        let mut failure: Option<std::io::Error> = None;
        let result = match mux_one(idx) {
            Ok(()) => TitleResult::Ok,
            Err(TitleError { result, error: e }) => {
                fail_detail = e.to_string();
                fail_code = crate::error_code(&e)
                    .or_else(|| crate::parse_error_code(&fail_detail).map(|(c, _)| c));
                fail_data = error_data(&e);
                fail_kind = e.kind();
                failure = Some(e);
                result
            }
        };
        let failed = |sink: &dyn Sink| {
            if let Some(e) = &failure {
                sink.event(&crate::Event::TitleFailed { idx, error: e });
            }
        };

        match decide_title(&result, is_feature, multi_title, explicit_selection) {
            TitleAction::Continue => titles_written += 1,
            TitleAction::Skip => {
                sink.log(
                    Level::Info,
                    &format!("title {} skipped (empty/uncrackable stub)", idx + 1),
                );
                sink.event(&crate::Event::TitleSkipped {
                    idx,
                    empty: fail_code == Some(libfreemkv::error::E_MKV_INVALID),
                });
            }
            TitleAction::StopHalt => {
                failed(sink);
                sink.log(Level::Info, "cancelled — stopping the whole rip");
                return RipOutcome::Halted;
            }
            TitleAction::StopNoKey => {
                failed(sink);
                sink.log(
                    Level::Error,
                    "disc has no decryption key — every title would fail; stopping",
                );
                return RipOutcome::NoKey;
            }
            TitleAction::StopFatal if keep_going => {
                failed(sink);
                sink.log(
                    Level::Error,
                    &format!(
                        "title {} failed — continuing with the rest: {}",
                        idx + 1,
                        log_safe(&fail_detail)
                    ),
                );
                last_failure = Some(RipOutcome::Failed {
                    title_index: idx,
                    code: fail_code,
                    kind: fail_kind,
                    data: fail_data,
                });
            }
            TitleAction::StopFatal => {
                failed(sink);
                // Unlike every other arm here, this one used to return silently,
                // so a hard failure (disk full, permission denied) reached the
                // front-end as a bare title index with no diagnostic anywhere.
                sink.log(
                    Level::Error,
                    &format!(
                        "title {} failed — stopping the rip: {}",
                        idx + 1,
                        log_safe(&fail_detail)
                    ),
                );
                return RipOutcome::Failed {
                    title_index: idx,
                    code: fail_code,
                    kind: fail_kind,
                    data: fail_data,
                };
            }
        }
    }

    // A rip that wrote NOTHING must not return silently: an empty `indices` or
    // an all-skippable-stub selection used to surface as exit-0 success with no
    // explanation. `Ok` stays the variant (matched exhaustively elsewhere).
    if titles_written == 0 {
        if let Some(failure) = last_failure {
            return failure;
        }
        sink.log(
            Level::Error,
            if indices.is_empty() {
                "no titles were selected — nothing was written"
            } else {
                "every selected title was skipped (empty/uncrackable stub) — \
                 nothing was written"
            },
        );
    }
    RipOutcome::Ok { titles_written }
}

/// Mux a single title from a source URL to `dest`, driving
/// `libfreemkv::mux_url` and reporting through the engine [`Sink`].
///
/// Bridges the run's [`libfreemkv::Ctx`] onto the Sink:
/// - its events' write progress → `Sink::progress` (via a channel + a scoped watcher
///   thread, because the `Ctx` holds an `Arc<dyn Events>` that cannot borrow the Sink).
/// - `Sink::should_cancel()` → the `Ctx`'s halt (the watcher cancels it), so a UI
///   Cancel / Ctrl-C stops the pump exactly as today.
pub fn mux_title(
    source_url: &str,
    dest: &str,
    input_opts: libfreemkv::InputOptions,
    mux_opts: &libfreemkv::MuxOptions,
    total_bytes_hint: u64,
    sink: &dyn Sink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    let opts = libfreemkv::MuxOptions {
        title_index: input_opts.title_index.unwrap_or(mux_opts.title_index),
        raw: input_opts.raw || mux_opts.raw,
        selection: input_opts.selection,
        ..mux_opts.clone()
    };
    let keys = input_opts.keys;
    with_mux_watcher(sink, dest, |ctx| {
        log_mux_start(sink, source_url, dest, total_bytes_hint);
        libfreemkv::mux_url(source_url, keys.as_ref(), dest, &opts, ctx)
    })
}

/// Mux `title` (already scanned: the drive's, or the image's own) out of the ISO at `path`
/// through the rip's key set, WITHOUT rescanning the image (KU J14): an unread UDF/MPLS
/// area of a staged ISO does not matter. `mux_opts.selection` picks the streams.
pub(crate) fn mux_iso_title(
    path: &std::path::Path,
    title: libfreemkv::ScannedTitle,
    keys: &libfreemkv::keys::KeyRing,
    dest: &str,
    mux_opts: &libfreemkv::MuxOptions,
    sink: &dyn Sink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    let hint = title.title.size_bytes;
    with_mux_watcher(sink, dest, |ctx| {
        let line = iso_mux_line(path, &title.title.playlist);
        sink.log(
            Level::Info,
            &format!("{line} -> {} (~{})", log_safe(dest), human_bytes(hint)),
        );
        let source = libfreemkv::Source::from_image(path, title);
        libfreemkv::mux_with_keys(source, Some(keys), dest, mux_opts, ctx)
    })
}

// The source half of an ISO title's mux log line.
fn iso_mux_line(path: &std::path::Path, playlist: &str) -> String {
    log_safe(&format!("mux: iso://{} {playlist}", path.display()))
}

// The "mux: <source> -> <dest> (~size)" line every mux opens with.
pub(crate) fn log_mux_start(
    sink: &dyn Sink,
    source_label: &str,
    dest: &str,
    total_bytes_hint: u64,
) {
    sink.log(
        Level::Info,
        &log_safe(&format!(
            "mux: {source_label} -> {dest} (~{})",
            human_bytes(total_bytes_hint)
        )),
    );
}

// The Sink↔libfreemkv bridge every mux runs inside, lifted out of the mux paths so it's
// testable against a closure without real media.
fn with_mux_watcher<T>(sink: &dyn Sink, dest: &str, f: impl FnOnce(&libfreemkv::Ctx) -> T) -> T {
    with_mux_watcher_for(sink, dest, None, None, f)
}

// [`with_mux_watcher`] under a front end's own stop token `halt` (else a fresh one) and with
// its own listener `extra` hearing every library event beside the bridge.
pub(crate) fn with_mux_watcher_for<T>(
    sink: &dyn Sink,
    dest: &str,
    halt: Option<libfreemkv::Halt>,
    extra: Option<Arc<dyn libfreemkv::Events>>,
    f: impl FnOnce(&libfreemkv::Ctx) -> T,
) -> T {
    use std::sync::mpsc;

    let halt = halt.unwrap_or_default();
    // Ask ONCE before starting (same rule as `with_cancel_watcher` in run.rs):
    // a watcher alone makes cancellation a race the work can win on a short
    // title, and only asking before the work begins closes that window.
    if sink.should_cancel() {
        halt.cancel();
    }
    let (tx, rx) = mpsc::channel::<(u64, u64)>();
    let (opened_tx, opened_rx) = mpsc::channel::<libfreemkv::DiscTitle>();
    let (flush_tx, flush_rx) = mpsc::channel::<(u64, u64)>();

    // Forwards write progress, the output opening and flush progress over channels.
    // Owned + 'static (holds only Senders), so it fits the `Ctx`'s `Arc<dyn Events>`.
    struct ChannelEvents {
        tx: mpsc::Sender<(u64, u64)>,
        opened: mpsc::Sender<libfreemkv::DiscTitle>,
        flush: mpsc::Sender<(u64, u64)>,
        extra: Option<Arc<dyn libfreemkv::Events>>,
    }
    impl libfreemkv::Events for ChannelEvents {
        fn event(&self, e: &libfreemkv::Event<'_>) {
            if let Some(extra) = &self.extra {
                extra.event(e);
            }
            match *e {
                libfreemkv::Event::BytesWritten { bytes, total } => {
                    let _ = self.tx.send((bytes, total));
                }
                libfreemkv::Event::OutputOpened { title } => {
                    let _ = self.opened.send(title.clone());
                }
                // Stop design v5 §4.5: flush progress becomes `Sink::progress(pass: "sync")`,
                // one call per event.
                libfreemkv::Event::BytesDurable { bytes, total } => {
                    let _ = self.flush.send((bytes, total));
                }
                _ => {}
            }
        }
    }
    let opened = |title: &libfreemkv::DiscTitle| {
        sink.event(&crate::sink::Event::OutputOpened { dest, title });
    };

    let done = Arc::new(AtomicBool::new(false));

    std::thread::scope(|s| {
        // Watcher: drains progress → sink, mirrors should_cancel → halt. `move`
        // captures the `!Sync` Receiver; `sink` is a borrowed ref tied to the
        // scope; `done` is a shared Arc the main thread sets when mux returns.
        let watcher_halt = halt.clone();
        let watcher_done = done.clone();
        let watcher = s.spawn(move || {
            // The engine's ONE speed/ETA derivation for the mux stage. Owned by
            // this single watcher thread, so a plain `mut` — no lock needed.
            let mut speed = crate::speed::SpeedEstimator::new();
            loop {
                // Read `done` BEFORE draining (Acquire pairs with SignalDone's Release): once
                // it reads true, the drain below sees every send, and the loop ends after it.
                let finished = watcher_done.load(Ordering::Acquire);
                // The opening first: it precedes every write-progress tick.
                while let Ok(title) = opened_rx.try_recv() {
                    opened(&title);
                }
                // Coalesce queued progress ticks to the LATEST, sample ONCE:
                // sampling per-message would measure `dt` in microseconds
                // against a ~100ms byte-delta, yielding absurd multi-GB/s speeds.
                let mut latest = None;
                while let Ok(m) = rx.try_recv() {
                    latest = Some(m);
                }
                if let Some((done_b, total_b)) = latest {
                    let (speed_bps, eta_secs) = speed.sample(done_b, total_b);
                    let p = crate::sink::Progress {
                        pass: std::borrow::Cow::Borrowed("mux"),
                        bytes_done: done_b,
                        bytes_total: total_b,
                        sectors_bad: 0,
                        speed_bps,
                        eta_secs,
                    };
                    sink.progress(&p);
                }
                for (done_b, total_b) in flush_rx.try_iter() {
                    sink.progress(&crate::sink::Progress {
                        pass: std::borrow::Cow::Borrowed("sync"),
                        bytes_done: done_b,
                        bytes_total: total_b,
                        ..Default::default()
                    });
                }
                if finished {
                    break;
                }
                if sink.should_cancel() {
                    watcher_halt.cancel();
                }
                std::thread::park_timeout(std::time::Duration::from_millis(100));
            }
        });
        // Woken when the mux returns, so a title's end is not held up by a 100 ms nap.
        let _wake = crate::run::WakeOnDrop(watcher.thread().clone());

        let ctx = crate::run::ctx(&halt).with_events(Arc::new(ChannelEvents {
            tx,
            opened: opened_tx,
            flush: flush_tx,
            extra,
        }));
        // Same guard the recovery paths use: `mux_with_keys` runs on damaged media
        // and can panic; storing `done` after the call would let an unwind skip
        // it, leaving thread::scope joining a watcher that loops forever.
        let _signal_done = crate::run::SignalDone(&done);
        f(&ctx)
    })
}

// Lifted out of `open_scan`'s struct literal so the one field that
// matters (`credentials`, the sole input to the SCSI AACS handshake) is
// unit-testable — dropped, every caller silently authenticates as no-one.
fn build_keyspec(credentials: Option<libfreemkv::DriveCredentials>) -> libfreemkv::KeySpec {
    libfreemkv::KeySpec {
        credentials,
        ..Default::default()
    }
}

// `raw_copy` scans past an unreadable AACS key file (E7031 recorded, keys refused);
// only a copy that never decrypts may set it.
fn scan_options(raw_copy: bool) -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions {
        raw_copy,
        ..Default::default()
    }
}

/// Open a live optical drive, scan the disc, then lock its tray, with NO key call (KU §3.2):
/// the scan's in-memory VID and titles, for [`crate::keys::resolve_for_rip`] (one resolve
/// per rip), a raw copy (no key at all), or an image mux that needs the disc's VID (E7034).
/// `raw_copy`: pass `true` only for a raw (never-decrypting) disc→ISO copy, matching the
/// CLI's `--raw`.
pub fn open_scan(
    target: libfreemkv::DeviceTarget,
    credentials: Option<libfreemkv::DriveCredentials>,
    raw_copy: bool,
) -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
    let (halt, progress) = (libfreemkv::Halt::new(), libfreemkv::halt::Liveness::new());
    open_scan_with(target, credentials, raw_copy, &halt, &progress)
}

/// [`open_scan`] under an open's token (stop design v5 §4.3, "The open token"): `halt`
/// ends the bring-up and the scan `Halted`, and the drive bumps `progress` per CDB and
/// holds it `busy()` while one is in flight (T29). A Stop leaves the tray unlocked.
pub fn open_scan_with(
    target: libfreemkv::DeviceTarget,
    credentials: Option<libfreemkv::DriveCredentials>,
    raw_copy: bool,
    halt: &libfreemkv::Halt,
    progress: &libfreemkv::halt::Liveness,
) -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
    let mut session = libfreemkv::DiscSession::open_with(target, build_keyspec(credentials), halt)?;
    session.attach_progress(progress);
    let opts = scan_options_with(raw_copy, halt);
    scan_then_lock(
        session,
        |session| session.scan_with(opts).map(drop),
        // Lock the tray so the disc can't eject mid-rip; Drive::drop unlocks it.
        libfreemkv::DiscSession::lock_tray,
    )
}

// `scan_options` under the open's token (the `ScanOptions.halt` alias of `open_with`'s).
fn scan_options_with(raw_copy: bool, halt: &libfreemkv::Halt) -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions {
        halt: Some(halt.clone()),
        ..scan_options(raw_copy)
    }
}

// Stop design v5 §4.2: `open_scan` "locks the tray after the scan (ET9)", so a scan that
// fails or is stopped never leaves the tray locked.
fn scan_then_lock<S>(
    mut session: S,
    scan: impl FnOnce(&mut S) -> Result<(), libfreemkv::Error>,
    lock: impl FnOnce(&mut S),
) -> Result<S, libfreemkv::Error> {
    scan(&mut session)?;
    lock(&mut session);
    Ok(session)
}

#[cfg(test)]
#[path = "mux_tests.rs"]
mod tests;
