//! Muxing titles out of an opened image, verifying the MKV that comes out, and
//! the verified in-place remux a library runs when the engine improves.
//!
//! Stop (design v5 §4.2, §4.4): `Sink::should_cancel` stays a first-class cancel input,
//! and the additive `_with` entries add an op token; either cancels every wait here (the
//! mux, the `<target>.lock` wait, the durable sync and verify). Sync and verify report
//! real progress as `Sink::progress` passes `"sync"` / `"verify"` (§4.5, T12b, T30).

use crate::engine_halt::{EngineHalt, HaltSink};
use crate::image::{ImageSource, OpenImageOptions, OpenedImage, open_image_with};
use crate::job::{Selection, StreamChoice, StreamFilter};
use crate::keys::{KeyParams, key_source_factory};
use crate::mux::{
    RipOutcome, TitleResult, classify_title_error, mux_iso_title, mux_title, run_titles,
};
use crate::sink::{Event, Level, Sink};
use libfreemkv::halt::{Stall, StallTimer, WAIT_SLICE};
use libfreemkv::io::ArtifactLock;
use libfreemkv::keys::{KeyRing, KeyScope};
use libfreemkv::{Halt, RemuxVerifyKind};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The mux options every front-end rips with; `raw` passes ciphertext through.
pub fn mux_options(raw: bool) -> libfreemkv::MuxOptions {
    libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: libfreemkv::mux::resolve::ISO_MUX_BATCH_SECTORS,
        raw,
        // Per title from `MuxPlan::streams` (an `iso://` title) or `InputOptions` (`dir://`).
        selection: libfreemkv::StreamSelection::default(),
        title_index: 0,
    }
}

/// Which titles of an opened image to mux, and how.
pub struct MuxPlan {
    /// 0-based title indices, in mux order (see [`crate::resolve_selection`]).
    pub titles: Vec<usize>,
    /// The user named these titles: a stub among them is fatal, not skipped.
    pub explicit_selection: bool,
    /// Per-title stream selection; a title not listed keeps every stream.
    pub streams: Vec<(usize, libfreemkv::StreamSelection)>,
    pub mux: libfreemkv::MuxOptions,
}

impl MuxPlan {
    /// Every stream of `titles`, decrypted, with the default options.
    pub fn new(titles: Vec<usize>) -> Self {
        Self {
            titles,
            explicit_selection: false,
            streams: Vec::new(),
            mux: mux_options(false),
        }
    }

    fn selection_for(&self, idx: usize) -> libfreemkv::StreamSelection {
        self.streams
            .iter()
            .find(|(i, _)| *i == idx)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }
}

/// Mux `plan.titles` out of `opened`, title `idx` into the sink URL `dest(idx)`,
/// under the [`run_titles`] loop policy. Each title is bracketed by
/// [`Event::TitleStart`] / [`Event::TitleDone`]. A title that fails into a
/// file sink has its partial file removed, unless the title never touched the file (a
/// directory sink, ending in `/`, is left alone).
///
/// Keys (KU §3.2): `opened.keys`, or ONE resolve over the plan's titles seeded with it
/// before any output. Each title is `opened.disc.titles[idx]`, muxed from the image
/// with no rescan (J14).
pub fn mux_image_titles(
    opened: &OpenedImage,
    plan: &MuxPlan,
    dest: &dyn Fn(usize) -> String,
    sink: &dyn Sink,
) -> RipOutcome {
    mux_image_titles_with(opened, plan, dest, sink, &Halt::new())
}

/// [`mux_image_titles`] under the op token `halt` (stop design v5 §4.2): the token and
/// [`Sink::should_cancel`] each stop the rip.
pub fn mux_image_titles_with(
    opened: &OpenedImage,
    plan: &MuxPlan,
    dest: &dyn Fn(usize) -> String,
    sink: &dyn Sink,
    halt: &Halt,
) -> RipOutcome {
    let halt = EngineHalt::new(halt, None).with_sink(sink);
    let sink = &HaltSink {
        inner: sink,
        halt: &halt,
    };
    let keys = match halt.linked(|h| opened.keys_for(&plan.titles, Some(h))) {
        Ok(keys) => keys,
        Err(e) => return refused_up_front(e, plan, dest, sink),
    };
    run_titles(&plan.titles, plan.explicit_selection, sink, |idx| {
        let dest = dest(idx);
        sink.event(&Event::TitleStart { idx, dest: &dest });
        let selection = plan.selection_for(idx);
        let out_path = libfreemkv::parse_url(&dest).path_str().to_string();
        let before = output_stamp(&out_path);
        let result = mux_opened_title(opened, &keys, idx, selection, &dest, &plan.mux, sink);
        if result.is_err() && !dest.ends_with('/') {
            remove_failed_output(&out_path, before);
        }
        sink.event(&Event::TitleDone {
            idx,
            dest: &dest,
            result: result.as_ref(),
        });
        result.map(|_| ())
    })
}

// Length and mtime of the file at `path`; `None` when absent.
fn output_stamp(path: &str) -> Option<(u64, Option<std::time::SystemTime>)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()))
}

// Remove the file a failed mux left at `path`, unless it is the one `before` saw: an error
// before the output opened must not delete an earlier rip.
fn remove_failed_output(path: &str, before: Option<(u64, Option<std::time::SystemTime>)>) {
    if output_stamp(path) != before {
        let _ = std::fs::remove_file(path);
    }
}

// A key refusal before any title started (E7022/E7026/E7034, or a halt), as the loop would
// report it: a refusal is the first planned title's TitleStart + TitleDone(Err), so a
// consumer never reads it as a Stop; a halt stays silent, as a Stop between titles is.
fn refused_up_front(
    e: libfreemkv::Error,
    plan: &MuxPlan,
    dest: &dyn Fn(usize) -> String,
    sink: &dyn Sink,
) -> RipOutcome {
    sink.log(
        Level::Error,
        &format!("keys refused before any output: {e}"),
    );
    let io: io::Error = e.into();
    let first = plan.titles.first().copied().unwrap_or(0);
    let (verdict, code, kind) = (classify_title_error(&io), crate::error_code(&io), io.kind());
    let data = crate::mux::error_data(&io);
    if verdict != TitleResult::Halted && !plan.titles.is_empty() {
        let dest = dest(first);
        sink.event(&Event::TitleStart {
            idx: first,
            dest: &dest,
        });
        let result: io::Result<libfreemkv::MuxOutcome> = Err(io);
        sink.event(&Event::TitleDone {
            idx: first,
            dest: &dest,
            result: result.as_ref(),
        });
    }
    match verdict {
        TitleResult::Halted => RipOutcome::Halted,
        TitleResult::DiscLevelNoKey => RipOutcome::NoKey,
        _ => RipOutcome::Failed {
            title_index: first,
            code,
            kind,
            data,
        },
    }
}

// Title `idx` of `opened` through `keys`: an ISO title straight from the image (no rescan),
// a `dir://` folder through `input()` with the same set.
fn mux_opened_title(
    opened: &OpenedImage,
    keys: &KeyRing,
    idx: usize,
    selection: libfreemkv::StreamSelection,
    dest: &str,
    mux: &libfreemkv::MuxOptions,
    sink: &dyn Sink,
) -> io::Result<libfreemkv::MuxOutcome> {
    let title = opened
        .disc
        .titles
        .get(idx)
        .ok_or(libfreemkv::Error::DiscTitleRange {
            index: idx,
            count: opened.disc.titles.len(),
        })?;
    match &opened.source {
        ImageSource::Iso(path) => {
            let opts = libfreemkv::MuxOptions {
                selection,
                ..mux.clone()
            };
            let scanned = libfreemkv::ScannedTitle::of(&opened.disc, idx).ok_or(
                libfreemkv::Error::DiscTitleRange {
                    index: idx,
                    count: opened.disc.titles.len(),
                },
            )?;
            mux_iso_title(path, scanned, keys, dest, &opts, sink)
        }
        ImageSource::Dir(_) => {
            utf8_source(&opened.source)?;
            let input = libfreemkv::InputOptions {
                keys: Some(keys.clone()),
                ..opened.input_options(idx, selection)
            };
            let url = opened.source.url();
            mux_title(&url, dest, input, mux, title.size_bytes, sink)
        }
    }
}

// A `dir://` title muxes through a String URL: a non-UTF-8 folder cannot be named in one.
fn utf8_source(source: &ImageSource) -> io::Result<()> {
    match source {
        ImageSource::Dir(p) if p.to_str().is_none() => {
            Err(libfreemkv::Error::StreamUrlInvalid { url: source.url() }.into())
        }
        _ => Ok(()),
    }
}

// A muxed runtime may differ from the title's by this much (whichever is larger).
const RUNTIME_SLACK_SECS: f64 = 10.0;
const RUNTIME_SLACK_FRACTION: f64 = 0.02;

/// Check an MKV written for `title`: non-empty, at least one track, and a
/// runtime within max(10 s, 2 %) of `title.duration_secs` (skipped when the
/// title declares none). The runtime is the last Cues entry — the writer keeps
/// a title's declared Duration in the header, so only the Cues reflect what was
/// muxed; a file without Cues falls back to the header Duration.
pub fn verify_mkv(path: &Path, title: &libfreemkv::DiscTitle) -> io::Result<libfreemkv::MkvProbe> {
    nonempty_len(path)?;
    let file = io::BufReader::new(std::fs::File::open(path)?);
    check_probe(path, title, libfreemkv::probe_mkv_with_cues(file)?)
}

// The file's length; an empty file fails verify.
fn nonempty_len(path: &Path) -> io::Result<u64> {
    match std::fs::metadata(path)?.len() {
        0 => Err(verify_failed(path, RemuxVerifyKind::Empty)),
        len => Ok(len),
    }
}

fn check_probe(
    path: &Path,
    title: &libfreemkv::DiscTitle,
    probe: libfreemkv::MkvProbe,
) -> io::Result<libfreemkv::MkvProbe> {
    if probe.tracks.is_empty() {
        return Err(verify_failed(path, RemuxVerifyKind::NoTracks));
    }
    let expected = title.duration_secs;
    if expected.is_finite() && expected > 0.0 {
        // A non-finite runtime is no usable runtime.
        let Some(runtime) = muxed_runtime(&probe).filter(|r| r.is_finite()) else {
            return Err(verify_failed(path, RemuxVerifyKind::NoRuntime));
        };
        let slack = RUNTIME_SLACK_SECS.max(expected * RUNTIME_SLACK_FRACTION);
        if (runtime - expected).abs() > slack {
            let kind = RemuxVerifyKind::RuntimeMismatch {
                have_secs: runtime,
                want_secs: expected,
            };
            return Err(verify_failed(path, kind));
        }
    }
    Ok(probe)
}

fn muxed_runtime(probe: &libfreemkv::MkvProbe) -> Option<f64> {
    probe.last_cue_secs.or(probe.duration_secs)
}

fn verify_failed(path: &Path, kind: RemuxVerifyKind) -> io::Error {
    let path = path.display().to_string();
    libfreemkv::Error::RemuxVerifyFailed { kind, path }.into()
}

/// A remux of one title from an image to an MKV.
#[derive(Clone, Debug)]
pub struct RemuxJob {
    pub iso: ImageSource,
    /// 0-based title; `None` picks the main title as a rip would.
    pub title: Option<usize>,
    pub streams: StreamChoice,
    pub target: PathBuf,
    /// Replace an existing `target`. Without it an existing target is refused
    /// before anything is read.
    pub replace: bool,
}

/// What [`remux_iso`] produced.
#[derive(Clone, Debug)]
pub struct RemuxReport {
    pub outcome: libfreemkv::MuxOutcome,
    /// The probe of the file now at the target.
    pub verified: libfreemkv::MkvProbe,
    /// An existing file at the target was replaced.
    pub replaced: bool,
    /// The new file's writing-app stamp (see [`libfreemkv::parse_freemkv_version`]).
    pub writing_app: Option<String>,
}

/// Remux one title of `job.iso` to `job.target`: take `<target>.lock`, mux into
/// `<target>.partial`, fsync, [`verify_mkv`], then rename over the target and fsync its
/// folder. On any failure or cancel the partial file and the lock are removed and an
/// existing target is left exactly as it was. The chosen title goes to
/// [`Sink::title_opened`]; phases and the title's mux are reported as [`Event`]s.
pub fn remux_iso(job: &RemuxJob, keys: &KeyParams, sink: &dyn Sink) -> io::Result<RemuxReport> {
    remux_iso_with(job, keys, sink, &Halt::new())
}

/// [`remux_iso`] with an op token: stop design v5 §4.4, "additive `remux_iso_with(job,
/// keys, sink, &Halt)`; either input cancels". A cancel is an `Err` whose
/// [`libfreemkv::error_code`] is `Halted`, with `.partial` removed and the target untouched.
pub fn remux_iso_with(
    job: &RemuxJob,
    keys: &KeyParams,
    sink: &dyn Sink,
    halt: &Halt,
) -> io::Result<RemuxReport> {
    remux_iso_sources(job, key_source_factory(keys), sink, halt)
}

/// Remux to a local partial file, then copy a verified result to a partial
/// beside the target before the atomic replacement. The caller owns the
/// staging location; the engine removes both partial files on every exit.
pub fn remux_iso_staged(
    job: &RemuxJob,
    keys: &KeyParams,
    sink: &dyn Sink,
    staged_partial: &Path,
) -> io::Result<RemuxReport> {
    remux_iso_sources_at(
        job,
        key_source_factory(keys),
        sink,
        &Halt::new(),
        Some(staged_partial),
    )
}

// `remux_iso_with` over any key sources: one open, one resolution round for the job's title
// (KU §3.2), then its mux through that set.
pub(crate) fn remux_iso_sources(
    job: &RemuxJob,
    sources: libfreemkv::KeySourceFactory,
    sink: &dyn Sink,
    halt: &Halt,
) -> io::Result<RemuxReport> {
    remux_iso_sources_at(job, sources, sink, halt, None)
}

fn remux_iso_sources_at(
    job: &RemuxJob,
    sources: libfreemkv::KeySourceFactory,
    sink: &dyn Sink,
    halt: &Halt,
    staged_partial: Option<&Path>,
) -> io::Result<RemuxReport> {
    // §4.2: "cancellation is `EngineHalt::is_cancelled() = op.is_cancelled() || extra ||
    // sink.should_cancel()`".
    let halt = EngineHalt::new(halt, None).with_sink(sink);
    let sink = &HaltSink {
        inner: sink,
        halt: &halt,
    };
    refuse_existing(job)?;
    utf8_source(&job.iso)?;
    sink.event(&Event::Phase { name: "open" });
    // The open, its title and its key top-up all run under one halt (KU §2.3 step 13).
    let open = |h: &Halt| -> io::Result<_> {
        let opts = OpenImageOptions {
            scope: job.title.map(|i| KeyScope::Titles(vec![i])),
            halt: Some(h.clone()),
            ..OpenImageOptions::resolve(sources)
        };
        let opened = open_image_with(&job.iso, opts)?;
        let idx = pick_title(&opened.disc, job.title, &job.streams.audio)?;
        let keys = opened.keys_for(&[idx], Some(h))?;
        Ok((opened, idx, keys))
    };
    let (opened, idx, keys) = halt.linked(open)?;
    let title = &opened.disc.titles[idx];
    let selection = title_selection(title, &job.streams)?;
    let options = mux_options(false);
    land_verified(
        job,
        idx,
        title,
        sink,
        &halt,
        &OsRemuxIo,
        staged_partial,
        |dest| mux_opened_title(&opened, &keys, idx, selection, dest, &options, sink),
    )
}

// The job's stream choice as PIDs for `title`; an unknown language tag is E9083.
fn title_selection(
    title: &libfreemkv::DiscTitle,
    streams: &StreamChoice,
) -> io::Result<libfreemkv::StreamSelection> {
    if streams.is_all() {
        return Ok(libfreemkv::StreamSelection::default());
    }
    crate::streams::resolve_stream_selection_forced(title, &streams.audio, &streams.subtitles)
        .map_err(|e| io::Error::from(libfreemkv::Error::from(e)))
}

// Only NotFound means absent: EIO/ESTALE/permission errors surface, so a flaky
// mount never reads as an empty folder.
fn target_present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn refuse_existing(job: &RemuxJob) -> io::Result<()> {
    if !job.replace && target_present(&job.target)? {
        return Err(target_exists(&job.target));
    }
    Ok(())
}

// Move `landing` onto the target. Without `replace`, a hard link lands it only if the target
// is still absent (a plain rename would overwrite one that appeared after the re-check); a
// filesystem with no hard links falls back to the rename.
fn land(landing: &Path, job: &RemuxJob) -> io::Result<()> {
    if job.replace {
        return std::fs::rename(landing, &job.target);
    }
    match std::fs::hard_link(landing, &job.target) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_file(landing) {
                tracing::warn!(target: "freemkv::engine", "could not remove {}: {e}", landing.display());
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(target_exists(&job.target)),
        Err(_) => std::fs::rename(landing, &job.target),
    }
}

fn target_exists(target: &Path) -> io::Error {
    let path = target.display().to_string();
    libfreemkv::Error::RemuxTargetExists { path }.into()
}

// The requested title, or the main title by the same rule a rip's default uses.
fn pick_title(
    disc: &libfreemkv::Disc,
    title: Option<usize>,
    audio: &StreamFilter,
) -> io::Result<usize> {
    let sel = title.map_or(Selection::MainMovie, |i| Selection::Titles(vec![i]));
    crate::mux::resolve_selection_with_audio(disc, &sel, audio)
        .first()
        .copied()
        .ok_or_else(|| {
            libfreemkv::Error::DiscTitleRange {
                index: title.unwrap_or(0),
                count: disc.titles.len(),
            }
            .into()
        })
}

fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    target.with_file_name(name)
}

fn remove_stale_partial(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// §4.2: remux keeps no resumable state, "so the sidecar is **deleted while held on every
// exit**" — a panic included.
struct DeleteOnDrop(Option<ArtifactLock>);

impl Drop for DeleteOnDrop {
    fn drop(&mut self) {
        if let Some(lock) = self.0.take() {
            let _ = lock.delete();
        }
    }
}

// Removes the partial file unless disarmed — every early return and a panic included.
struct PartialFile<'a>(Option<&'a Path>);

impl PartialFile<'_> {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PartialFile<'_> {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The remux path's waits, production values by default; a parameter so tests scale time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RemuxTiming {
    /// T30: §3.1 "60 s with no bytes read (HR1)".
    pub(crate) verify_stall: Duration,
    /// The staged copy's bound, as T30: 60 s with no bytes written (HR1).
    pub(crate) copy_stall: Duration,
    /// §4.4: "rate-limited to one call per 250 ms".
    pub(crate) activity_every: Duration,
    /// How often real sync/verify progress refreshes the file's mtime for a lock waiter (T10).
    pub(crate) lock_beat: Duration,
}

impl Default for RemuxTiming {
    fn default() -> Self {
        Self {
            verify_stall: Duration::from_secs(60),
            copy_stall: Duration::from_secs(60),
            activity_every: Duration::from_millis(250),
            lock_beat: libfreemkv::io::artifact_lock::ARTIFACT_LOCK_WINDOW / 10,
        }
    }
}

/// A reader verify can hand to its worker thread.
pub(crate) trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// The file primitives of the remux path; a seam so a test can slow or stall them.
pub(crate) trait RemuxIo: Sync {
    /// Make `file` durable: stall-based, `halt`-aware, reporting `(bytes_done, total)`.
    fn sync(
        &self,
        file: &std::fs::File,
        halt: &Halt,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()>;
    /// Open `path` for the verify reads.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>>;
    /// Create `path` for the staged copy; an existing file is refused, never truncated.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>>;
    fn timing(&self) -> RemuxTiming;
}

/// The production primitives: libfreemkv's durable sync and plain file reads.
pub(crate) struct OsRemuxIo;

impl RemuxIo for OsRemuxIo {
    // T12b (§3.1): "routes both through `io::durable_sync_file` (§4.5): halt- and
    // `should_cancel`-aware, stall-based, and reporting Sink `"sync"` progress".
    fn sync(
        &self,
        file: &std::fs::File,
        halt: &Halt,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()> {
        libfreemkv::io::durable_sync_file(file, Some(halt), on_progress)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(file))
    }

    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
}

// Mux (via `mux`, given the partial file's sink URL), verify, and move into place.
#[allow(clippy::too_many_arguments)]
fn land_verified(
    job: &RemuxJob,
    idx: usize,
    title: &libfreemkv::DiscTitle,
    sink: &dyn Sink,
    halt: &EngineHalt<'_>,
    rio: &dyn RemuxIo,
    staged_partial: Option<&Path>,
    mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
) -> io::Result<RemuxReport> {
    let timing = rio.timing();
    sink.title_opened(title);
    let target_partial = partial_path(&job.target);
    let partial = staged_partial.unwrap_or(&target_partial);
    if partial == job.target || (staged_partial.is_some() && partial == target_partial) {
        return Err(libfreemkv::Error::RemuxStagingInvalid.into());
    }
    // The mux takes a String URL: a non-UTF-8 path would be written under a lossy name.
    let Some(partial_str) = partial.to_str() else {
        let url = format!("mkv://{}", partial.display());
        return Err(libfreemkv::Error::StreamUrlInvalid { url }.into());
    };
    // §4.2: "`land_verified` … takes `<target>.lock` (the §2.5 acquire loop) **before**
    // creating `.partial`"; "deleted while held on every exit" (dropped last).
    // libfreemkv's lock watches `<target>.partial` as the holder's T10 progress.
    let watch: Vec<&Path> = staged_partial.into_iter().collect();
    let lock = halt.linked(|h| ArtifactLock::acquire(&job.target, &watch, h))?;
    let _lock = DeleteOnDrop(Some(lock));
    remove_stale_partial(partial)?;
    let mut guard = PartialFile(Some(partial));

    sink.event(&Event::Phase { name: "mux" });
    let dest = format!("mkv://{partial_str}");
    sink.event(&Event::TitleStart { idx, dest: &dest });
    let result = mux(&dest);
    sink.event(&Event::TitleDone {
        idx,
        dest: &dest,
        result: result.as_ref(),
    });
    let outcome = result?;
    if !outcome.completed {
        return Err(if outcome.halted || halt.is_cancelled() {
            libfreemkv::Error::Halted.into()
        } else {
            libfreemkv::Error::MuxIncomplete { title: idx + 1 }.into()
        });
    }
    durable_sync(rio, partial, halt, sink, timing)?;

    // Each verify (the local file, then a staged NAS copy) is its own phase and verdict.
    let verify = |path: &Path| {
        sink.event(&Event::Phase { name: "verify" });
        let verified = verify_watched(path, title, halt, sink, rio, timing);
        if !verified.as_ref().is_err_and(libfreemkv::is_halt) {
            sink.event(&Event::Verify {
                path,
                ok: verified.is_ok(),
                runtime_secs: verified.as_ref().ok().and_then(muxed_runtime),
                expected_secs: title.duration_secs,
            });
        }
        verified
    };
    let verified = verify(partial)?;

    let mut remote_guard = None;
    if staged_partial.is_some() {
        sink.event(&Event::Phase { name: "copy" });
        remove_stale_partial(&target_partial)?;
        remote_guard = Some(PartialFile(Some(&target_partial)));
        copy_staged(partial, &target_partial, halt, sink, rio, timing)?;
        durable_sync(rio, &target_partial, halt, sink, timing)?;
        // Check the NAS copy itself before replacing an existing library file.
        verify(&target_partial)?;
    }

    sink.event(&Event::Phase { name: "replace" });
    // Re-checked: the target may have appeared while the title muxed.
    refuse_existing(job)?;
    let replaced = target_present(&job.target)?;
    let landing = if staged_partial.is_some() {
        &target_partial
    } else {
        partial
    };
    // §2.6: "**Done after Stop** only if the commit … happened before `t_cancel`"; the rename
    // is the commit, so a Stop that arrived before it leaves the target untouched.
    if halt.is_cancelled() {
        return Err(libfreemkv::Error::Halted.into());
    }
    land(landing, job)?;
    remote_guard.as_mut().unwrap_or(&mut guard).disarm();
    // The rename committed: a Stop or a failure during the folder sync cuts only the sync
    // short (§2.6, §4.4), and the caller is still told the target was replaced.
    match sync_parent(rio, &job.target, halt) {
        Err(e) if libfreemkv::is_halt(&e) => sink.log(
            Level::Warn,
            "stopped during the folder sync after the target was replaced; the rename may not be durable yet",
        ),
        Err(e) => sink.log(
            Level::Warn,
            &format!("the target was replaced but its folder sync failed: {e}"),
        ),
        Ok(()) => {}
    }
    if replaced {
        sink.event(&Event::Replaced { path: &job.target });
    }
    Ok(RemuxReport {
        outcome,
        writing_app: verified.writing_app.clone(),
        verified,
        replaced,
    })
}

// The staged copy on a worker, waited on as verify is: Stop within a slice, and
// `TimedOut { op: "copy" }` after `copy_stall` with no bytes written (§3.1 HR1).
fn copy_staged(
    source: &Path,
    destination: &Path,
    halt: &EngineHalt<'_>,
    sink: &dyn Sink,
    rio: &dyn RemuxIo,
    timing: RemuxTiming,
) -> io::Result<()> {
    let mut src = std::fs::File::open(source)?;
    let total = src.metadata()?.len();
    let mut dst = rio.create_new(destination)?;
    let copied = Arc::new(AtomicU64::new(0));
    let quit = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let (count, stop) = (copied.clone(), quit.clone());
    std::thread::Builder::new()
        .name("freemkv-remux-copy".into())
        .spawn(move || {
            let result = copy_all(&mut src, &mut *dst, &count, &stop);
            drop((src, dst));
            let _ = tx.send(result);
        })?;
    let started = Instant::now();
    let report = |done: u64| {
        let speed = (done as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64;
        crate::sink::Progress {
            speed_bps: speed,
            eta_secs: (speed > 0).then(|| total.saturating_sub(done) / speed),
            ..activity("copy", done, total)
        }
    };
    let beat = LockBeat::new(None, timing.lock_beat);
    let watched = watch_worker(
        &rx,
        &copied,
        "copy",
        timing.copy_stall,
        halt,
        sink,
        timing,
        beat,
        &report,
    );
    // A leaked worker holds both files until its write returns: empty the local stage so its
    // blocks free now (never the NAS side, which may be the hung mount).
    if watched.is_err() {
        quit.store(true, Ordering::Relaxed);
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(source) {
            let _ = f.set_len(0);
        }
    }
    let done = watched??;
    let have = match done {
        n if n != total => n,
        _ => std::fs::metadata(destination)?.len(),
    };
    if have != total {
        let want = total;
        return Err(libfreemkv::Error::StagedCopySizeMismatch { have, want }.into());
    }
    Ok(())
}

// The copy worker's loop; `copied` is its progress, `quit` set once the caller gave up.
fn copy_all(
    src: &mut std::fs::File,
    dst: &mut dyn Write,
    copied: &AtomicU64,
    quit: &AtomicBool,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        if quit.load(Ordering::Relaxed) {
            return Err(libfreemkv::Error::Halted.into());
        }
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        done += n as u64;
        copied.store(done, Ordering::Relaxed);
    }
    dst.flush()?;
    Ok(done)
}

// A `Sink::progress` of `pass` (§4.4 "`Progress.pass` values are stable keys").
fn activity(pass: &'static str, bytes_done: u64, bytes_total: u64) -> crate::sink::Progress {
    crate::sink::Progress {
        pass: std::borrow::Cow::Borrowed(pass),
        bytes_done,
        bytes_total,
        ..Default::default()
    }
}

// §4.4: "on **every** increase of `bytes_done`, rate-limited to one call per 250 ms";
// "No synthetic heartbeats: every call means real forward progress".
struct Activity {
    every: Duration,
    last_at: Option<Instant>,
    last: u64,
}

impl Activity {
    fn new(every: Duration) -> Self {
        Self {
            every,
            last_at: None,
            last: 0,
        }
    }

    // Whether an increase to `done` is reported now; `last` always forces it.
    fn due(&mut self, done: u64, last: bool) -> bool {
        let grew = done > self.last;
        let spaced = self.last_at.is_none_or(|t| t.elapsed() >= self.every);
        if grew && (spaced || last) {
            self.last = done;
            self.last_at = Some(Instant::now());
            return true;
        }
        false
    }
}

// T10 (§2.5): a waiter on `<target>.lock` sees the holder live only while the `.partial`
// changes size or mtime. Sync and verify change neither, so their real progress bumps the
// mtime of the file they work on, at most once per `every` (best effort).
struct LockBeat {
    file: Option<std::fs::File>,
    every: Duration,
    last: Option<Instant>,
}

impl LockBeat {
    fn new(file: Option<std::fs::File>, every: Duration) -> Self {
        Self {
            file,
            every,
            last: None,
        }
    }

    fn progressed(&mut self) {
        if self.last.is_some_and(|t| t.elapsed() < self.every) {
            return;
        }
        if let Some(f) = &self.file {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        self.last = Some(Instant::now());
    }
}

// T12b: the `.partial`'s durable sync, preceded by `Event::Phase { name: "sync" }` (§4.4).
fn durable_sync(
    rio: &dyn RemuxIo,
    partial: &Path,
    halt: &EngineHalt<'_>,
    sink: &dyn Sink,
    timing: RemuxTiming,
) -> io::Result<()> {
    sink.event(&Event::Phase { name: "sync" });
    // Read-write: Windows' FlushFileBuffers needs a handle with GENERIC_WRITE.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(partial)?;
    let mut every = Activity::new(timing.activity_every);
    let mut beat = LockBeat::new(file.try_clone().ok(), timing.lock_beat);
    halt.linked(|h| {
        rio.sync(&file, h, &mut |done, total| {
            beat.progressed();
            if every.due(done, done >= total) {
                sink.progress(&activity("sync", done, total));
            }
        })
    })
}

// Makes the rename durable through the same stall-based, halt-aware sync. Windows cannot
// open a directory for this; NTFS journals it (§4.5 "`sync_parent`, Unix only").
fn sync_parent(rio: &dyn RemuxIo, path: &Path, halt: &EngineHalt<'_>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let dir = std::fs::File::open(dir)?;
        halt.linked(|h| rio.sync(&dir, h, &mut |_, _| {}))?;
    }
    #[cfg(not(unix))]
    let _ = (rio, path, halt);
    Ok(())
}

// Counts the bytes each read returns; seeks count nothing (§4.5, the counting reader).
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

// [`verify_mkv`] with its reads on a worker the caller waits on halt-aware: T30 (§4.5),
// "60 s with no bytes read → `TimedOut { op: "verify" }` (E9073)"; a cancel leaks the worker.
fn verify_watched(
    path: &Path,
    title: &libfreemkv::DiscTitle,
    halt: &EngineHalt<'_>,
    sink: &dyn Sink,
    rio: &dyn RemuxIo,
    timing: RemuxTiming,
) -> io::Result<libfreemkv::MkvProbe> {
    let total = nonempty_len(path)?;
    let read = Arc::new(AtomicU64::new(0));
    let reader = Counting {
        inner: io::BufReader::new(rio.open_read(path)?),
        read: read.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("freemkv-remux-verify".into())
        .spawn(move || {
            let _ = tx.send(libfreemkv::probe_mkv_with_cues(reader));
        })?;
    let beat_file = std::fs::OpenOptions::new().write(true).open(path).ok();
    let beat = LockBeat::new(beat_file, timing.lock_beat);
    let report = |done| activity("verify", done, total);
    let probe = watch_worker(
        &rx,
        &read,
        "verify",
        timing.verify_stall,
        halt,
        sink,
        timing,
        beat,
        &report,
    )?;
    check_probe(path, title, probe?)
}

// Waits halt-aware on a worker whose forward progress is the byte count `moved`, reported
// through `report` and `beat`: `Halted` on a cancel, `TimedOut { op }` after `stall` with no
// progress. Either return leaks the worker.
#[allow(clippy::too_many_arguments)]
fn watch_worker<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    moved: &AtomicU64,
    op: &'static str,
    stall: Duration,
    halt: &EngineHalt<'_>,
    sink: &dyn Sink,
    timing: RemuxTiming,
    mut beat: LockBeat,
    report: &dyn Fn(u64) -> crate::sink::Progress,
) -> io::Result<T> {
    let progress = libfreemkv::halt::Liveness::new();
    let mut timer = StallTimer::new(stall, &progress);
    let (mut every, mut seen) = (Activity::new(timing.activity_every), 0);
    loop {
        let done = rx.recv_timeout(WAIT_SLICE);
        let now = moved.load(Ordering::Relaxed);
        if now > seen {
            seen = now;
            progress.bump();
            beat.progressed();
        }
        if every.due(now, done.is_ok()) {
            sink.progress(&report(now));
        }
        match done {
            Ok(result) => return Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(libfreemkv::Error::WorkerLost { op }.into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if halt.is_cancelled() {
            return Err(libfreemkv::Error::Halted.into());
        }
        if timer.poll(&progress) == Stall::Expired {
            return Err(libfreemkv::Error::TimedOut { op }.into());
        }
    }
}

#[cfg(test)]
mod stop_tests;

#[cfg(test)]
#[path = "remux_tests.rs"]
mod tests;
