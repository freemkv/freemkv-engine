//! Muxing titles out of an opened image, verifying the MKV that comes out, and
//! the verified in-place remux a library runs when the engine improves.
//!
//! Stop (design v5 §4.2, §4.4): `Sink::should_cancel` stays a first-class cancel input,
//! and the additive `_with` entries add an op token; either cancels every wait here (the
//! mux, the `<target>.lock` wait, the durable sync and verify). Sync and verify report
//! real progress as `Sink::progress` passes `"sync"` / `"verify"` (§4.5, T12b, T30).

use crate::engine_halt::{EngineHalt, HaltSink};
use crate::image::{ImageSource, OpenImageOptions, OpenedImage, open_image_with};
use crate::job::{Selection, StreamChoice};
use crate::keys::{KeyParams, key_source_factory};
use crate::mux::{
    RipOutcome, TitleResult, classify_title_error, mux_iso_title, mux_title, resolve_selection,
    run_titles,
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
/// file sink has its partial file removed (a directory sink, ending in `/`,
/// is left alone).
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
        let result = mux_opened_title(opened, &keys, idx, selection, &dest, &plan.mux, sink);
        if result.is_err() && !dest.ends_with('/') {
            let _ = std::fs::remove_file(libfreemkv::parse_url(&dest).path_str());
        }
        sink.event(&Event::TitleDone {
            idx,
            dest: &dest,
            result: result.as_ref(),
        });
        result.map(|_| ())
    })
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
        let idx = pick_title(&opened.disc, job.title)?;
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

fn target_exists(target: &Path) -> io::Error {
    let path = target.display().to_string();
    libfreemkv::Error::RemuxTargetExists { path }.into()
}

// The requested title, or the main title by the same rule a rip's default uses.
fn pick_title(disc: &libfreemkv::Disc, title: Option<usize>) -> io::Result<usize> {
    let sel = title.map_or(Selection::MainMovie, |i| Selection::Titles(vec![i]));
    resolve_selection(disc, &sel)
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

    sink.event(&Event::Phase { name: "verify" });
    let verified = verify_watched(partial, title, halt, sink, rio, timing);
    let stopped = verified.as_ref().is_err_and(libfreemkv::is_halt);
    if !stopped {
        sink.event(&Event::Verify {
            path: partial,
            ok: verified.is_ok(),
            runtime_secs: verified.as_ref().ok().and_then(muxed_runtime),
            expected_secs: title.duration_secs,
        });
    }
    let verified = verified?;

    let mut remote_guard = None;
    if staged_partial.is_some() {
        sink.event(&Event::Phase { name: "copy" });
        remove_stale_partial(&target_partial)?;
        remote_guard = Some(PartialFile(Some(&target_partial)));
        copy_staged(partial, &target_partial, halt, sink, rio, timing)?;
        durable_sync(rio, &target_partial, halt, sink, timing)?;
        // Check the NAS copy itself before replacing an existing library file.
        verify_watched(&target_partial, title, halt, sink, rio, timing)?;
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
    std::fs::rename(landing, &job.target)?;
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
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `land_verified` as the legacy `remux_iso` runs it: no op token, the OS primitives.
    pub(super) fn land(
        job: &RemuxJob,
        idx: usize,
        title: &libfreemkv::DiscTitle,
        sink: &dyn Sink,
        mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
    ) -> io::Result<RemuxReport> {
        let halt = EngineHalt::new(&Halt::new(), None).with_sink(sink);
        land_verified(job, idx, title, sink, &halt, &OsRemuxIo, None, mux)
    }

    #[test]
    fn staged_remux_replaces_only_after_copy_and_verification() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("ssd");
        let nas = dir.path().join("nas");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&nas).unwrap();
        let target = nas.join("Movie.mkv");
        let local_partial = stage.join("1.mkv.partial");
        std::fs::write(&target, b"old").unwrap();
        let new = mkv(600.0, Some(598), 2);
        let sink = Events::default();
        let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
        let report = land_verified(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            &halt,
            &OsRemuxIo,
            Some(&local_partial),
            writes(new.clone(), true),
        )
        .unwrap();
        assert!(report.replaced);
        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert!(!local_partial.exists());
        assert!(!partial_path(&target).exists());
        assert!(sink.0.lock().unwrap().contains(&"phase:copy".to_string()));
    }

    #[test]
    fn staged_remux_failure_preserves_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        let local_partial = dir.path().join("1.mkv.partial");
        std::fs::write(&target, b"old").unwrap();
        let sink = Events::default();
        let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
        let result = land_verified(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            &halt,
            &OsRemuxIo,
            Some(&local_partial),
            writes(mkv(600.0, Some(598), 2), false),
        );
        let e = result.unwrap_err();
        assert!(
            !libfreemkv::is_halt(&e),
            "an incomplete mux is a failure: {e}"
        );
        assert_eq!(e.kind(), io::ErrorKind::Other);
        assert_eq!(e.to_string(), "E9078: 1");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!local_partial.exists());
        assert!(!partial_path(&target).exists());
    }

    // A staged partial that is the target or `<target>.partial` would overwrite the library
    // file or delete its own source: refused before the lock, the mux or any file change.
    #[test]
    fn a_staging_path_on_the_target_or_its_partial_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        let target_partial = partial_path(&target);
        for staged in [&target, &target_partial] {
            let sink = Events::default();
            let halt = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
            let muxed = AtomicBool::new(false);
            let e = land_verified(
                &job(target.clone(), true),
                0,
                &title(600.0),
                &sink,
                &halt,
                &OsRemuxIo,
                Some(staged),
                |dest| {
                    muxed.store(true, Ordering::SeqCst);
                    writes(mkv(600.0, Some(598), 2), true)(dest)
                },
            )
            .unwrap_err();
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "{}",
                staged.display()
            );
            let code = libfreemkv::error::E_REMUX_STAGING_INVALID;
            assert_eq!(crate::error_code(&e), Some(code), "{e}");
            assert!(!muxed.load(Ordering::SeqCst));
            assert_eq!(std::fs::read(&target).unwrap(), b"old");
            assert!(!target_partial.exists());
        }
    }

    // The public staged entry: an image that cannot open leaves no partial anywhere.
    #[test]
    fn remux_iso_staged_with_an_unopenable_image_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        let staged = dir.path().join("7.mkv.partial");
        std::fs::write(&target, b"old").unwrap();
        let sink = Events::default();
        let keys = KeyParams::default();
        let e = remux_iso_staged(&job(target.clone(), true), &keys, &sink, &staged).unwrap_err();
        assert!(!libfreemkv::is_halt(&e), "{e}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!staged.exists() && !partial_path(&target).exists());
        let refused = remux_iso_staged(&job(target.clone(), false), &keys, &sink, &staged);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn staged_copy_refuses_to_overwrite_an_existing_partial() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mkv");
        let destination = dir.path().join("target.mkv.partial");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"other writer").unwrap();
        let token = Halt::new();
        let sink = Events::default();
        let halt = EngineHalt::new(&token, None).with_sink(&sink);
        let timing = RemuxTiming::default();
        let err = copy_staged(&source, &destination, &halt, &sink, &OsRemuxIo, timing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&destination).unwrap(), b"other writer");
    }

    // A destination that accepts writes but keeps no bytes: the copy counts `total`, the file holds 0.
    struct DroppingIo;
    impl RemuxIo for DroppingIo {
        fn sync(&self, f: &std::fs::File, h: &Halt, p: &mut dyn FnMut(u64, u64)) -> io::Result<()> {
            OsRemuxIo.sync(f, h, p)
        }
        fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
            OsRemuxIo.open_read(path)
        }
        fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
            drop(OsRemuxIo.create_new(path)?);
            Ok(Box::new(io::sink()))
        }
        fn timing(&self) -> RemuxTiming {
            RemuxTiming::default()
        }
    }

    #[test]
    fn a_staged_copy_short_on_disk_is_a_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (source, destination) = (dir.path().join("s.mkv"), dir.path().join("d.partial"));
        std::fs::write(&source, b"new bytes").unwrap();
        let token = Halt::new();
        let sink = Events::default();
        let halt = EngineHalt::new(&token, None).with_sink(&sink);
        let timing = RemuxTiming::default();
        let e = copy_staged(&source, &destination, &halt, &sink, &DroppingIo, timing).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(e.to_string(), "E9080: 0/9");
    }

    // A worker that drops its sender without reporting is WorkerLost naming the op (E9081).
    #[test]
    fn a_lost_worker_is_its_code_naming_the_op() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        let token = Halt::new();
        let sink = Events::default();
        let halt = EngineHalt::new(&token, None).with_sink(&sink);
        let timing = RemuxTiming::default();
        let beat = LockBeat::new(None, timing.lock_beat);
        let moved = AtomicU64::new(0);
        let report = |n: u64| activity("verify", n, 1);
        let e = watch_worker(
            &rx,
            &moved,
            "verify",
            timing.verify_stall,
            &halt,
            &sink,
            timing,
            beat,
            &report,
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other);
        assert_eq!(e.to_string(), "E9081: verify");
    }

    #[test]
    fn stop_during_staged_copy_keeps_old_target() {
        struct StopOnCopy(std::sync::atomic::AtomicBool);
        impl Sink for StopOnCopy {
            fn event(&self, event: &Event<'_>) {
                if matches!(event, Event::Phase { name: "copy" }) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }

            fn should_cancel(&self) -> bool {
                self.0.load(Ordering::SeqCst)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        let local_partial = dir.path().join("1.mkv.partial");
        std::fs::write(&target, b"old").unwrap();
        let sink = StopOnCopy(std::sync::atomic::AtomicBool::new(false));
        let token = Halt::new();
        let halt = EngineHalt::new(&token, None).with_sink(&sink);
        let result = land_verified(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            &halt,
            &OsRemuxIo,
            Some(&local_partial),
            writes(mkv(600.0, Some(598), 2), true),
        );
        let e = result.unwrap_err();
        assert!(
            libfreemkv::is_halt(&e),
            "a Stop is Halted, not a failure: {e}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!local_partial.exists());
        assert!(!partial_path(&target).exists());
    }

    // ── A hand-built MKV: EBML header, Segment, Info, one track, Cues ──────

    pub(super) fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    pub(super) fn mkv(duration_secs: f64, last_cue_secs: Option<u64>, tracks: usize) -> Vec<u8> {
        let mut info = el(&[0x2A, 0xD7, 0xB1], &1_000_000u64.to_be_bytes());
        info.extend(el(&[0x44, 0x89], &(duration_secs * 1000.0).to_be_bytes()));
        info.extend(el(&[0x4D, 0x80], b"freemkv 9.9.9 (gtest)"));
        info.extend(el(&[0x57, 0x41], b"freemkv 9.9.9 (gtest)"));
        // TrackNumber is unique per Matroska; libfreemkv rejects a duplicate.
        let entries: Vec<u8> = (0..tracks)
            .flat_map(|i| {
                let mut entry = el(&[0xD7], &[i as u8 + 1]);
                entry.extend(el(&[0x83], &[1]));
                entry.extend(el(&[0x86], b"V_MPEG4/ISO/AVC"));
                el(&[0xAE], &entry)
            })
            .collect();
        let mut body = el(&[0x15, 0x49, 0xA9, 0x66], &info);
        body.extend(el(&[0x16, 0x54, 0xAE, 0x6B], &entries));
        if let Some(t) = last_cue_secs {
            let point = el(&[0xB3], &(t * 1000).to_be_bytes());
            body.extend(el(&[0x1C, 0x53, 0xBB, 0x6B], &el(&[0xBB], &point)));
        }
        let mut out = el(&[0x1A, 0x45, 0xDF, 0xA3], &[]);
        out.extend(el(&[0x18, 0x53, 0x80, 0x67], &body));
        out
    }

    pub(super) fn title(duration_secs: f64) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            duration_secs,
            ..libfreemkv::DiscTitle::empty()
        }
    }

    pub(super) fn outcome(completed: bool) -> libfreemkv::MuxOutcome {
        libfreemkv::MuxOutcome {
            completed,
            halted: false,
            output_opened: true,
            bytes_written: 1,
            errors: 0,
            lost_bytes: 0,
            streams: 1,
            undelivered_streams: Vec::new(),
        }
    }

    #[derive(Default)]
    pub(super) struct Events(pub(super) Mutex<Vec<String>>);
    impl Sink for Events {
        fn title_opened(&self, _title: &libfreemkv::DiscTitle) {
            self.0.lock().unwrap().push("title".into());
        }

        fn event(&self, e: &Event<'_>) {
            let s = match e {
                Event::Phase { name } => format!("phase:{name}"),
                Event::TitleStart { idx, .. } => format!("start:{idx}"),
                Event::TitleDone { idx, result, .. } => format!("done:{idx}:{}", result.is_ok()),
                Event::Verify { ok, .. } => format!("verify:{ok}"),
                Event::Replaced { .. } => "replaced".into(),
                Event::OutputOpened { .. } => "opened".into(),
                Event::SourceOpened { .. } => "source".into(),
                Event::Keys { .. } => "keys".into(),
                Event::Pass(_) => "pass".into(),
            };
            self.0.lock().unwrap().push(s);
        }
    }

    pub(super) fn job(target: PathBuf, replace: bool) -> RemuxJob {
        RemuxJob {
            iso: ImageSource::Iso("/nonexistent/freemkv/none.iso".into()),
            title: None,
            streams: StreamChoice::default(),
            target,
            replace,
        }
    }

    // A mux stand-in that writes `bytes` where the sink URL points.
    pub(super) fn writes(
        bytes: Vec<u8>,
        completed: bool,
    ) -> impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome> {
        move |dest| {
            let path = dest.strip_prefix("mkv://").unwrap();
            assert!(
                path.ends_with(".partial"),
                "muxes to the partial file, got {path}"
            );
            std::fs::write(path, bytes)?;
            Ok(outcome(completed))
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_target_folder_is_refused_not_taken_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // ENOTDIR (a file where the folder should be) holds as root too.
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let under_file = file.join("Title.mkv");
        let err = refuse_existing(&job(under_file.clone(), false)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
        assert!(target_present(&under_file).is_err());
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let target = locked.join("Title").join("Title.mkv");
        let probe = std::fs::symlink_metadata(&target);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_ok() || probe.as_ref().unwrap_err().kind() == io::ErrorKind::NotFound {
            return; // root: permissions are not enforced (ENOTDIR above still ran)
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = refuse_existing(&job(target.clone(), false));
        let present = target_present(&target);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = refused.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(present.is_err());
    }

    #[test]
    fn verify_accepts_a_runtime_within_slack_and_rejects_a_short_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mkv");
        std::fs::write(&p, mkv(7200.0, Some(7195), 1)).unwrap();
        let probe = verify_mkv(&p, &title(7200.0)).unwrap();
        assert_eq!(probe.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
        // Header claims the full runtime, the Cues show a third of it.
        std::fs::write(&p, mkv(7200.0, Some(2400), 1)).unwrap();
        assert!(verify_mkv(&p, &title(7200.0)).is_err());
        // No Cues: the header Duration is all there is.
        std::fs::write(&p, mkv(100.0, None, 1)).unwrap();
        assert!(verify_mkv(&p, &title(100.0)).is_ok());
        assert!(verify_mkv(&p, &title(200.0)).is_err());
        // A title with no declared duration skips the runtime check.
        assert!(verify_mkv(&p, &title(0.0)).is_ok());
    }

    // Slack is max(10 s, 2 %) either side; a title with a runtime needs a finite one.
    #[test]
    fn verify_runtime_slack_is_the_larger_of_ten_seconds_and_two_percent() {
        let probe = |last_cue: Option<f64>, duration: Option<f64>| libfreemkv::MkvProbe {
            tracks: vec![libfreemkv::MkvProbeTrack {
                number: 1,
                kind: libfreemkv::MkvTrackKind::Video,
                codec_id: "V_MPEG4/ISO/AVC".into(),
                language: "und".into(),
            }],
            last_cue_secs: last_cue,
            duration_secs: duration,
            ..Default::default()
        };
        let ok = |t: f64, runtime: f64| {
            check_probe(Path::new("a"), &title(t), probe(Some(runtime), None)).is_ok()
        };
        // 2 h title: 2 % = 144 s.
        assert!(ok(7200.0, 7200.0 - 143.0) && ok(7200.0, 7200.0 + 143.0));
        assert!(!ok(7200.0, 7200.0 - 145.0) && !ok(7200.0, 7200.0 + 145.0));
        // 100 s title: 2 % = 2 s, so the 10 s floor applies.
        assert!(ok(100.0, 91.0) && ok(100.0, 109.0));
        assert!(!ok(100.0, 89.0) && !ok(100.0, 111.0));
        assert!(!ok(100.0, f64::NAN) && !ok(100.0, f64::INFINITY));
        let t = title(100.0);
        let shown = |p| check_probe(Path::new("a"), &t, p).unwrap_err().to_string();
        assert_eq!(shown(probe(None, None)), "E9077: no-runtime a");
        assert_eq!(shown(probe(Some(f64::NAN), None)), "E9077: no-runtime a");
        assert_eq!(
            shown(probe(Some(80.0), None)),
            "E9077: runtime-mismatch 80.0/100.0 a"
        );
        assert!(
            check_probe(Path::new("a"), &t, probe(None, Some(100.0))).is_ok(),
            "header Duration"
        );
    }

    #[test]
    fn verify_rejects_empty_trackless_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mkv");
        let shown = |t: f64| verify_mkv(&p, &title(t)).unwrap_err().to_string();
        std::fs::write(&p, b"").unwrap();
        assert_eq!(shown(0.0), format!("E9077: empty {}", p.display()));
        std::fs::write(&p, mkv(10.0, Some(9), 0)).unwrap();
        assert_eq!(shown(10.0), format!("E9077: no-tracks {}", p.display()));
        std::fs::write(&p, vec![0x47u8; 4096]).unwrap();
        assert!(verify_mkv(&p, &title(10.0)).is_err());
        assert!(verify_mkv(&dir.path().join("missing.mkv"), &title(0.0)).is_err());
    }

    #[test]
    fn a_verified_remux_replaces_the_target_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        let new = mkv(600.0, Some(598), 2);
        let sink = Events::default();
        let r = land(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            writes(new.clone(), true),
        )
        .unwrap();
        assert!(r.replaced);
        assert_eq!(r.writing_app.as_deref(), Some("freemkv 9.9.9 (gtest)"));
        assert_eq!(r.verified.tracks.len(), 2);
        assert_eq!(std::fs::read(&target).unwrap(), new);
        assert!(!partial_path(&target).exists());
        assert_eq!(
            *sink.0.lock().unwrap(),
            [
                "title",
                "phase:mux",
                "start:0",
                "done:0:true",
                "phase:sync",
                "phase:verify",
                "verify:true",
                "phase:replace",
                "replaced"
            ]
        );
    }

    #[test]
    fn a_new_target_lands_without_replace() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("New.mkv");
        let r = land(
            &job(target.clone(), false),
            0,
            &title(60.0),
            &Events::default(),
            writes(mkv(60.0, Some(59), 1), true),
        )
        .unwrap();
        assert!(!r.replaced);
        assert!(target.exists());
        assert!(!partial_path(&target).exists());
    }

    #[test]
    fn a_failed_verify_leaves_the_old_target_and_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        let sink = Events::default();
        let e = land(
            &job(target.clone(), true),
            0,
            &title(600.0),
            &sink,
            writes(mkv(600.0, Some(120), 1), true),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let code = libfreemkv::error::E_REMUX_VERIFY_FAILED;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!partial_path(&target).exists());
        assert!(sink.0.lock().unwrap().contains(&"verify:false".to_string()));
    }

    #[test]
    fn a_failed_or_incomplete_mux_leaves_the_old_target_and_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        let j = job(target.clone(), true);
        let fail = |dest: &str| {
            std::fs::write(dest.strip_prefix("mkv://").unwrap(), b"half")?;
            Err(io::Error::other("E6000: 12"))
        };
        assert!(land(&j, 0, &title(60.0), &Events::default(), fail).is_err());
        let incomplete = writes(mkv(60.0, Some(59), 1), false);
        assert!(land(&j, 0, &title(60.0), &Events::default(), incomplete).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!partial_path(&target).exists());
    }

    #[test]
    fn an_existing_target_without_replace_is_refused_before_opening() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        let sink = Events::default();
        let e = remux_iso(&job(target.clone(), false), &KeyParams::default(), &sink).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(sink.0.lock().unwrap().is_empty(), "nothing was opened");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
    }

    // A sink URL is a String: a non-UTF-8 partial path would mux to a different, lossy name.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_partial_path_is_refused_before_the_mux() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(std::ffi::OsStr::from_bytes(b"Caf\xe9"));
        let _ = std::fs::create_dir(&folder); // APFS refuses the name; the check comes first
        let target = folder.join("Movie.mkv");
        let muxed = AtomicBool::new(false);
        let e = land(
            &job(target.clone(), true),
            0,
            &title(60.0),
            &Events::default(),
            |_| {
                muxed.store(true, Ordering::SeqCst);
                Ok(outcome(true))
            },
        )
        .unwrap_err();
        let code = libfreemkv::error::E_STREAM_URL_INVALID;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert!(!muxed.load(Ordering::SeqCst));
        assert!(!partial_path(&target).exists());
    }

    // A `dir://` source muxes through a String URL: a non-UTF-8 folder is refused up front.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_disc_folder_is_refused_before_opening() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join(std::ffi::OsStr::from_bytes(b"Disc\xe9"));
        let mut j = job(dir.path().join("Movie.mkv"), true);
        j.iso = ImageSource::Dir(folder);
        let sink = Events::default();
        let e = remux_iso(&j, &KeyParams::default(), &sink).unwrap_err();
        let code = libfreemkv::error::E_STREAM_URL_INVALID;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert!(sink.0.lock().unwrap().is_empty(), "nothing was opened");
    }

    // Preflight catches an unknown tag first; a caller that skips it still gets E9083, not prose.
    #[test]
    fn an_unknown_stream_language_is_its_code() {
        let streams = StreamChoice {
            audio: crate::job::StreamFilter::Langs(vec!["Klingonish".into()]),
            subtitles: crate::job::StreamFilter::All.into(),
        };
        let e = title_selection(&title(60.0), &streams).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        let code = libfreemkv::error::E_STREAM_LANGUAGE_UNKNOWN;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert_eq!(e.to_string(), "E9083: Klingonish");
    }

    // A relative target named like a libfreemkv code reads as E9084, not that code.
    #[test]
    fn an_existing_target_named_like_an_error_code_is_not_that_code() {
        let e = target_exists(Path::new("E7022 Movie.mkv"));
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        let code = libfreemkv::error::E_REMUX_TARGET_EXISTS;
        assert_eq!(crate::error_code(&e), Some(code), "{e}");
        assert_eq!(e.to_string(), "E9084: E7022 Movie.mkv");
    }

    #[test]
    fn an_unopenable_image_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Movie.mkv");
        std::fs::write(&target, b"old").unwrap();
        assert!(
            remux_iso(
                &job(target.clone(), true),
                &KeyParams::default(),
                &Events::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!partial_path(&target).exists());
    }

    #[test]
    fn the_main_title_is_the_default_and_a_missing_title_is_range_error() {
        let mut disc = libfreemkv::Disc {
            volume_id: String::new(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![title(10.0), title(7200.0)],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        assert_eq!(pick_title(&disc, None).unwrap(), 0);
        assert_eq!(pick_title(&disc, Some(1)).unwrap(), 1);
        let e = pick_title(&disc, Some(5)).unwrap_err();
        assert_eq!(
            crate::error_code(&e),
            Some(libfreemkv::Error::DiscTitleRange { index: 5, count: 2 }.code())
        );
        disc.titles.clear();
        assert!(pick_title(&disc, None).is_err());
    }

    #[test]
    fn plan_selection_defaults_to_every_stream() {
        let mut plan = MuxPlan::new(vec![0, 3]);
        let only = libfreemkv::StreamSelection {
            audio: libfreemkv::PidFilter::Only(vec![4352]),
            subtitle: libfreemkv::PidFilter::Only(vec![]),
        };
        plan.streams.push((3, only.clone()));
        assert!(plan.selection_for(0).is_all());
        assert_eq!(plan.selection_for(3), only);
    }
}
