//! Muxing titles out of an opened image, verifying the MKV that comes out, and
//! the verified in-place remux a library runs when the engine improves.
//!
//! Stop (design v5 §4.2, §4.4): `Sink::should_cancel` stays a first-class cancel input,
//! and the additive `_with` entries add an op token; either cancels every wait here (the
//! mux, the `<target>.lock` wait, the durable sync and verify). Sync and verify report
//! real progress as `Sink::progress` passes `"sync"` / `"verify"` (§4.5, T12b, T30).

use crate::artifact_lock::{ArtifactLock, DeleteOnDrop, LOCK_STALL};
use crate::engine_halt::{EngineHalt, HaltSink};
use crate::image::{ImageSource, OpenImageOptions, OpenedImage, open_image_with};
use crate::job::{Selection, StreamChoice};
use crate::keys::{KeyParams, key_source_factory};
use crate::mux::{RipOutcome, TitleResult, classify_title_error, mux_title, resolve_selection};
use crate::mux::{mux_iso_title, run_titles};
use crate::sink::Level;
use crate::sink::{Event, Sink};
use libfreemkv::Halt;
use libfreemkv::halt::{Stall, StallTimer, WAIT_SLICE};
use libfreemkv::keys::{KeyScope, ResolvedKeySet};
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The mux options every front-end rips with; `raw` passes ciphertext through.
pub fn mux_options(raw: bool) -> libfreemkv::MuxOptions {
    libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: 64,
        raw,
        // Per title from `MuxPlan::streams` (an `iso://` title) or `InputOptions` (`dir://`).
        selection: libfreemkv::StreamSelection::default(),
        // Stop design v5 T27: the per-frame send deadline is "retired in freemkv and the
        // engine"; "a halt-aware send only, and the user's Stop is the bound" (§2.10).
        send_deadline: None,
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
    keys: &ResolvedKeySet,
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
                skip_errors: mux.skip_errors,
                batch_sectors: mux.batch_sectors,
                raw: mux.raw,
                selection,
                send_deadline: mux.send_deadline,
            };
            let format = opened.disc.content_format;
            mux_iso_title(path, title.clone(), format, keys, dest, &opts, sink)
        }
        ImageSource::Dir(_) => {
            let input = libfreemkv::InputOptions {
                keys: Some(keys.clone()),
                ..opened.input_options(idx, selection)
            };
            let url = opened.source.url();
            mux_title(&url, dest, input, mux, title.size_bytes, sink)
        }
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
    if std::fs::metadata(path)?.len() == 0 {
        return Err(verify_failed(path, "file is empty"));
    }
    let file = io::BufReader::new(std::fs::File::open(path)?);
    check_probe(path, title, libfreemkv::probe_mkv_with_cues(file)?)
}

fn check_probe(
    path: &Path,
    title: &libfreemkv::DiscTitle,
    probe: libfreemkv::MkvProbe,
) -> io::Result<libfreemkv::MkvProbe> {
    if probe.tracks.is_empty() {
        return Err(verify_failed(path, "no tracks"));
    }
    let expected = title.duration_secs;
    if expected.is_finite() && expected > 0.0 {
        let Some(runtime) = muxed_runtime(&probe) else {
            return Err(verify_failed(path, "no runtime"));
        };
        let slack = RUNTIME_SLACK_SECS.max(expected * RUNTIME_SLACK_FRACTION);
        if !runtime.is_finite() || (runtime - expected).abs() > slack {
            return Err(verify_failed(
                path,
                &format!("runtime {runtime:.1}s, title is {expected:.1}s"),
            ));
        }
    }
    Ok(probe)
}

fn muxed_runtime(probe: &libfreemkv::MkvProbe) -> Option<f64> {
    probe.last_cue_secs.or(probe.duration_secs)
}

fn verify_failed(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("verify {}: {why}", path.display()),
    )
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

// `remux_iso_with` over any key sources: one open, one resolution round for the job's title
// (KU §3.2), then its mux through that set.
pub(crate) fn remux_iso_sources(
    job: &RemuxJob,
    sources: libfreemkv::KeySourceFactory,
    sink: &dyn Sink,
    halt: &Halt,
) -> io::Result<RemuxReport> {
    // §4.2: "cancellation is `EngineHalt::is_cancelled() = op.is_cancelled() || extra ||
    // sink.should_cancel()`".
    let halt = EngineHalt::new(halt, None).with_sink(sink);
    let sink = &HaltSink {
        inner: sink,
        halt: &halt,
    };
    refuse_existing(job)?;
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
    let selection = if job.streams.is_all() {
        libfreemkv::StreamSelection::default()
    } else {
        crate::streams::resolve_stream_selection_forced(
            title,
            &job.streams.audio,
            &job.streams.subtitles,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e:?}")))?
    };
    land_verified(job, idx, title, sink, &halt, &OsRemuxIo, |dest| {
        mux_opened_title(
            &opened,
            &keys,
            idx,
            selection,
            dest,
            &mux_options(false),
            sink,
        )
    })
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
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", job.target.display()),
        ));
    }
    Ok(())
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

// Removes the partial file unless disarmed — every early return and a panic included.
struct PartialFile<'a>(&'a Path, bool);

impl Drop for PartialFile<'_> {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_file(self.0);
        }
    }
}

/// The remux path's waits, production values by default; a parameter so tests scale time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RemuxTiming {
    /// T10: the `<target>.lock` wait's no-progress window.
    pub(crate) lock_stall: Duration,
    /// T30: §3.1 "60 s with no bytes read (HR1)".
    pub(crate) verify_stall: Duration,
    /// §4.4: "rate-limited to one call per 250 ms".
    pub(crate) activity_every: Duration,
}

impl Default for RemuxTiming {
    fn default() -> Self {
        Self {
            lock_stall: LOCK_STALL,
            verify_stall: Duration::from_secs(60),
            activity_every: Duration::from_millis(250),
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

    fn timing(&self) -> RemuxTiming {
        RemuxTiming::default()
    }
}

// Mux (via `mux`, given the partial file's sink URL), verify, and move into place.
fn land_verified(
    job: &RemuxJob,
    idx: usize,
    title: &libfreemkv::DiscTitle,
    sink: &dyn Sink,
    halt: &EngineHalt<'_>,
    rio: &dyn RemuxIo,
    mux: impl FnOnce(&str) -> io::Result<libfreemkv::MuxOutcome>,
) -> io::Result<RemuxReport> {
    let timing = rio.timing();
    sink.title_opened(title);
    let partial = partial_path(&job.target);
    // §4.2: "`land_verified` … takes `<target>.lock` (the §2.5 acquire loop) **before**
    // creating `.partial`"; "deleted while held on every exit" (dropped last).
    let watch = [partial.clone()];
    let lock = ArtifactLock::acquire(&job.target, &watch, halt, timing.lock_stall)?;
    let _lock = DeleteOnDrop::new(lock);
    let _ = std::fs::remove_file(&partial);
    let mut guard = PartialFile(&partial, false);

    sink.event(&Event::Phase { name: "mux" });
    let dest = format!("mkv://{}", partial.display());
    sink.event(&Event::TitleStart { idx, dest: &dest });
    let result = mux(&dest);
    sink.event(&Event::TitleDone {
        idx,
        dest: &dest,
        result: result.as_ref(),
    });
    let outcome = result?;
    if !outcome.completed {
        return Err(if halt.is_cancelled() {
            libfreemkv::Error::Halted.into()
        } else {
            io::Error::other(format!("mux of title {} did not complete", idx + 1))
        });
    }
    durable_sync(rio, &partial, halt, sink, timing)?;

    sink.event(&Event::Phase { name: "verify" });
    let verified = verify_watched(&partial, title, halt, sink, rio, timing);
    let stopped = verified.as_ref().is_err_and(libfreemkv::is_halt);
    if !stopped {
        sink.event(&Event::Verify {
            path: &partial,
            ok: verified.is_ok(),
            runtime_secs: verified.as_ref().ok().and_then(muxed_runtime),
            expected_secs: title.duration_secs,
        });
    }
    let verified = verified?;

    sink.event(&Event::Phase { name: "replace" });
    // Re-checked: the target may have appeared while the title muxed.
    refuse_existing(job)?;
    let replaced = target_present(&job.target)?;
    std::fs::rename(&partial, &job.target)?;
    guard.1 = true;
    sync_parent(rio, &job.target, halt)?;
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
    halt.linked(|h| {
        rio.sync(&file, h, &mut |done, total| {
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
    let total = std::fs::metadata(path)?.len();
    if total == 0 {
        return Err(verify_failed(path, "file is empty"));
    }
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
    let progress = libfreemkv::halt::Progress::new();
    let mut timer = StallTimer::new(timing.verify_stall, &progress);
    let (mut every, mut seen) = (Activity::new(timing.activity_every), 0);
    loop {
        let done = rx.recv_timeout(WAIT_SLICE);
        let now = read.load(Ordering::Relaxed);
        if now > seen {
            seen = now;
            progress.bump();
        }
        if every.due(now, done.is_ok()) {
            sink.progress(&activity("verify", now, total));
        }
        match done {
            Ok(probe) => return check_probe(path, title, probe?),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("verify worker lost"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if halt.is_cancelled() {
            return Err(libfreemkv::Error::Halted.into());
        }
        if timer.poll(&progress) == Stall::Expired {
            return Err(libfreemkv::Error::TimedOut { op: "verify" }.into());
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
        land_verified(job, idx, title, sink, &halt, &OsRemuxIo, mux)
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
        let mut entry = el(&[0xD7], &[1]);
        entry.extend(el(&[0x83], &[1]));
        entry.extend(el(&[0x86], b"V_MPEG4/ISO/AVC"));
        let entries: Vec<u8> = (0..tracks).flat_map(|_| el(&[0xAE], &entry)).collect();
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
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let target = locked.join("Title").join("Title.mkv");
        let probe = std::fs::symlink_metadata(&target);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_ok() || probe.as_ref().unwrap_err().kind() == io::ErrorKind::NotFound {
            return; // running as root: permissions are not enforced
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

    #[test]
    fn verify_rejects_empty_trackless_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.mkv");
        std::fs::write(&p, b"").unwrap();
        assert!(verify_mkv(&p, &title(0.0)).is_err());
        std::fs::write(&p, mkv(10.0, Some(9), 0)).unwrap();
        assert!(verify_mkv(&p, &title(10.0)).is_err());
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
