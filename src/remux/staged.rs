//! A staged remux whose local file verified but whose delivery to the library failed for a
//! storage reason is kept in the staging folder, beside a JSON sidecar, so it can be finished
//! later without muxing again.
//!
//! The pair is `<stage>/<name>.staged.mkv` and `<stage>/<name>.staged.json`, where `<name>`
//! is the target's file stem and a hash of the full target path: one kept file per target.
//! [`finish_staged`] runs the delivery again; [`staged_pending`], [`staged_orphans`],
//! [`discard_staged`], [`staged_expired`] and [`staged_over_budget`] keep the folder bounded.

use super::*;
use crate::job::StreamFilter;
use crate::streams::SubtitleFilter;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const SIDECAR_FORMAT: u32 = 1;
const KEPT_MKV: &str = ".staged.mkv";
const KEPT_JSON: &str = ".staged.json";
const KEPT_TMP: &str = ".staged.json.tmp";
// A sidecar is a few hundred bytes; anything far larger is not one.
const SIDECAR_MAX: u64 = 1 << 20;
const STEM_MAX: usize = 80;

/// A remux failed after its local file verified, and that file was kept for
/// [`finish_staged`]. Carried inside the returned [`io::Error`] (see [`staged_kept`]),
/// which keeps the failure's [`io::ErrorKind`], Display and error code.
#[derive(Debug)]
pub struct StagedKept {
    /// The kept MKV, `<stage>/<name>.staged.mkv`.
    pub path: PathBuf,
    /// Its sidecar, `<stage>/<name>.staged.json`.
    pub sidecar: PathBuf,
    /// The step that failed: `"copy"`, `"sync"`, `"verify"` (the library copy), `"replace"`,
    /// or `"target"` (a resume could not examine the target).
    pub phase: &'static str,
    /// The failure itself, with its OS error (`raw_os_error`) intact.
    pub cause: io::Error,
}

impl std::fmt::Display for StagedKept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)
    }
}

impl std::error::Error for StagedKept {
    // Transparent: the cause's own source, so an error chain does not print the cause twice.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.source()
    }
}

/// The [`StagedKept`] inside an error from [`remux_iso_staged`] or [`finish_staged`]:
/// `Some` when the verified local file was kept and can be finished later.
pub fn staged_kept(e: &io::Error) -> Option<&StagedKept> {
    e.get_ref()?.downcast_ref::<StagedKept>()
}

/// One kept remux, read from its sidecar ([`staged_pending`], [`read_staged`]).
#[derive(Clone, Debug)]
pub struct StagedInfo {
    /// The kept MKV and its sidecar.
    pub staged: PathBuf,
    pub sidecar: PathBuf,
    /// Where it lands, and whether an existing file there was to be replaced.
    pub target: PathBuf,
    pub replace: bool,
    /// The image it was muxed from, its length and modification time then, and the title.
    pub source: ImageSource,
    pub source_len: Option<u64>,
    pub source_modified: Option<SystemTime>,
    pub title: usize,
    pub streams: StreamChoice,
    /// Bytes the kept file holds on the staging disk.
    pub size: u64,
    /// Verified runtime and the title's expected runtime (`None`: the title declares none).
    pub runtime_secs: Option<f64>,
    pub expected_secs: Option<f64>,
    pub writing_app: Option<String>,
    /// The engine version that kept it.
    pub engine_version: String,
    pub created_at: SystemTime,
    /// Failed deliveries so far (1 when first kept), and when the last one failed.
    pub attempts: u32,
    pub last_attempt_at: SystemTime,
    /// The last failure: its step (see [`StagedKept::phase`]), Display and error code.
    pub failed_phase: String,
    pub error: String,
    pub error_code: Option<u16>,
    record: Sidecar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum TargetStamp {
    Absent,
    Present {
        len: u64,
        mtime_secs: Option<u64>,
        mtime_nanos: Option<u32>,
    },
    Unknown,
}

impl TargetStamp {
    pub(crate) fn of(path: &Path) -> Self {
        match std::fs::symlink_metadata(path) {
            Ok(m) => Self::present(&m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }

    fn present(m: &std::fs::Metadata) -> Self {
        let t = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        Self::Present {
            len: m.len(),
            mtime_secs: t.map(|d| d.as_secs()),
            mtime_nanos: t.map(|d| d.subsec_nanos()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FilterRecord {
    All,
    None,
    Langs(Vec<String>),
}

impl FilterRecord {
    fn of(f: &StreamFilter) -> Self {
        match f {
            StreamFilter::All => Self::All,
            StreamFilter::None => Self::None,
            StreamFilter::Langs(l) => Self::Langs(l.clone()),
        }
    }

    fn filter(&self) -> StreamFilter {
        match self {
            Self::All => StreamFilter::All,
            Self::None => StreamFilter::None,
            Self::Langs(l) => StreamFilter::Langs(l.clone()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StreamsRecord {
    audio: FilterRecord,
    subtitles: FilterRecord,
    forced_subtitles: FilterRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MuxRecord {
    bytes_written: u64,
    errors: u64,
    lost_bytes: u64,
    streams: usize,
    undelivered_streams: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sidecar {
    format: u32,
    target: PathBuf,
    replace: bool,
    target_before: TargetStamp,
    source: String,
    source_len: Option<u64>,
    source_mtime_secs: Option<u64>,
    title: usize,
    streams: StreamsRecord,
    size: u64,
    runtime_secs: Option<f64>,
    expected_secs: Option<f64>,
    writing_app: Option<String>,
    engine_version: String,
    created_at: u64,
    attempts: u32,
    last_attempt_at: u64,
    failed_phase: String,
    error: String,
    error_code: Option<u16>,
    mux: MuxRecord,
}

#[derive(Deserialize)]
struct FormatOnly {
    format: u32,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

impl Sidecar {
    fn failed(&mut self, phase: &str, cause: &io::Error) {
        self.last_attempt_at = now_secs();
        self.failed_phase = phase.to_string();
        self.error = cause.to_string();
        self.error_code = crate::error_code(cause);
    }

    fn info(self, staged: PathBuf, sidecar: PathBuf) -> StagedInfo {
        let r = &self;
        StagedInfo {
            staged,
            sidecar,
            target: r.target.clone(),
            replace: r.replace,
            source: ImageSource::from_url(&r.source)
                .unwrap_or_else(|| ImageSource::Iso(PathBuf::from(&r.source))),
            source_len: r.source_len,
            source_modified: r.source_mtime_secs.map(at),
            title: r.title,
            streams: StreamChoice {
                audio: r.streams.audio.filter(),
                subtitles: SubtitleFilter::split(
                    r.streams.subtitles.filter(),
                    r.streams.forced_subtitles.filter(),
                ),
            },
            size: r.size,
            runtime_secs: r.runtime_secs,
            expected_secs: r.expected_secs,
            writing_app: r.writing_app.clone(),
            engine_version: r.engine_version.clone(),
            created_at: at(r.created_at),
            attempts: r.attempts,
            last_attempt_at: at(r.last_attempt_at),
            failed_phase: r.failed_phase.clone(),
            error: r.error.clone(),
            error_code: r.error_code,
            record: self,
        }
    }

    fn outcome(&self) -> libfreemkv::MuxOutcome {
        libfreemkv::MuxOutcome {
            completed: true,
            halted: false,
            output_opened: true,
            bytes_written: self.mux.bytes_written,
            errors: self.mux.errors,
            lost_bytes: self.mux.lost_bytes,
            streams: self.mux.streams,
            undelivered_streams: self.mux.undelivered_streams.clone(),
        }
    }
}

/// What a failed staged delivery keeps: everything the sidecar records.
pub(super) struct Kept<'a> {
    pub(super) job: &'a RemuxJob,
    pub(super) idx: usize,
    pub(super) title: &'a libfreemkv::DiscTitle,
    pub(super) outcome: &'a libfreemkv::MuxOutcome,
    pub(super) verified: &'a libfreemkv::MkvProbe,
    pub(super) target_before: TargetStamp,
}

impl Kept<'_> {
    fn record(&self, size: u64, phase: &str, cause: &io::Error) -> Sidecar {
        let (job, o) = (self.job, self.outcome);
        let source = std::fs::metadata(job.iso.path()).ok();
        let expected = self.title.duration_secs;
        let now = now_secs();
        let mut record = Sidecar {
            format: SIDECAR_FORMAT,
            target: job.target.clone(),
            replace: job.replace,
            target_before: self.target_before,
            source: job.iso.url(),
            source_len: source.as_ref().map(std::fs::Metadata::len),
            source_mtime_secs: source
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            title: self.idx,
            streams: StreamsRecord {
                audio: FilterRecord::of(&job.streams.audio),
                subtitles: FilterRecord::of(&job.streams.subtitles.normal),
                forced_subtitles: FilterRecord::of(&job.streams.subtitles.forced),
            },
            size,
            runtime_secs: muxed_runtime(self.verified).filter(|r| r.is_finite()),
            expected_secs: (expected.is_finite() && expected > 0.0).then_some(expected),
            writing_app: self.verified.writing_app.clone(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: now,
            attempts: 1,
            last_attempt_at: now,
            failed_phase: String::new(),
            error: String::new(),
            error_code: None,
            mux: MuxRecord {
                bytes_written: o.bytes_written,
                errors: o.errors,
                lost_bytes: o.lost_bytes,
                streams: o.streams,
                undelivered_streams: o.undelivered_streams.clone(),
            },
        };
        record.failed(phase, cause);
        record
    }
}

/// Whether a failure after the local verify keeps the local file: not a Stop (by its error or
/// a cancel that arrived meanwhile), and not a target that appeared (nowhere left to land).
pub(super) fn keeps(e: &io::Error, halt: &EngineHalt<'_>) -> bool {
    !libfreemkv::is_halt(e)
        && !halt.is_cancelled()
        && crate::error_code(e) != Some(libfreemkv::error::E_REMUX_TARGET_EXISTS)
}

// The kept pair for `target` in `stage_dir`: the target's stem and a hash of its whole path.
fn kept_paths(stage_dir: &Path, target: &Path) -> (PathBuf, PathBuf) {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(target.as_os_str().as_encoded_bytes());
    let hash: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    let stem = target.file_stem().unwrap_or_default().to_string_lossy();
    let stem: String = stem
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(STEM_MAX)
        .collect();
    let name = format!("{stem}.{hash}");
    (
        stage_dir.join(format!("{name}{KEPT_MKV}")),
        stage_dir.join(format!("{name}{KEPT_JSON}")),
    )
}

// The pair `path` (either half) belongs to; `None` for a name that is not a kept one.
fn pair_of(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let name = path.file_name()?.to_str()?;
    let base = [KEPT_MKV, KEPT_JSON]
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .filter(|b| !b.is_empty())?;
    Some((
        path.with_file_name(format!("{base}{KEPT_MKV}")),
        path.with_file_name(format!("{base}{KEPT_JSON}")),
    ))
}

fn tmp_of(sidecar: &Path) -> PathBuf {
    let mut name = sidecar.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    sidecar.with_file_name(name)
}

fn folder_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

// Best effort: the staging folder is local, and the files in it are already durable.
fn sync_folder(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// Temp file, fsync, rename, folder sync: a reader sees the old sidecar or the new one.
fn write_sidecar(path: &Path, record: &Sidecar) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    let tmp = tmp_of(path);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    sync_folder(folder_of(path));
    Ok(())
}

// Empties `path` before removing it: a copy worker left running may still hold it open.
fn drop_file(path: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_len(0);
    }
    let _ = std::fs::remove_file(path);
}

/// Keep the verified `partial` (already durable) under its kept name and write its sidecar.
/// Returns `cause` wrapped in a [`StagedKept`], or `cause` itself if keeping failed (the
/// partial is then removed as before).
pub(super) fn keep(
    partial: &Path,
    guard: &mut PartialFile<'_>,
    kept: &Kept<'_>,
    phase: &'static str,
    cause: io::Error,
    sink: &dyn Sink,
) -> io::Error {
    let (path, sidecar) = kept_paths(folder_of(partial), &kept.job.target);
    let size = match std::fs::metadata(partial) {
        Ok(m) => m.len(),
        Err(e) => return not_kept(partial, guard, &e, cause, sink),
    };
    let record = kept.record(size, phase, &cause);
    if let Err(e) = std::fs::rename(partial, &path) {
        return not_kept(partial, guard, &e, cause, sink);
    }
    guard.disarm();
    sync_folder(folder_of(&path));
    if let Err(e) = write_sidecar(&sidecar, &record) {
        let mut moved = PartialFile(Some(path.as_path()));
        return not_kept(&path, &mut moved, &e, cause, sink);
    }
    sink.log(
        Level::Warn,
        &format!(
            "the {phase} step failed ({cause}); kept the verified file at {} to finish later",
            path.display()
        ),
    );
    wrap(path, sidecar, phase, cause)
}

fn not_kept(
    at: &Path,
    guard: &mut PartialFile<'_>,
    why: &io::Error,
    cause: io::Error,
    sink: &dyn Sink,
) -> io::Error {
    sink.log(
        Level::Warn,
        &format!("could not keep the verified file {}: {why}", at.display()),
    );
    guard.disarm();
    drop_file(at);
    cause
}

fn wrap(path: PathBuf, sidecar: PathBuf, phase: &'static str, cause: io::Error) -> io::Error {
    let kind = cause.kind();
    let kept = StagedKept {
        path,
        sidecar,
        phase,
        cause,
    };
    io::Error::new(kind, kept)
}

enum ReadError {
    // Not a kept name, or a sidecar of a newer format: left alone.
    NotOurs,
    Io(io::Error),
    // The sidecar exists but is not one.
    Corrupt,
}

fn staging_invalid() -> io::Error {
    libfreemkv::Error::RemuxStagingInvalid.into()
}

fn read_pair(path: &Path) -> Result<StagedInfo, ReadError> {
    let (staged, sidecar) = pair_of(path).ok_or(ReadError::NotOurs)?;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(&sidecar).map_err(ReadError::Io)?;
    file.take(SIDECAR_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > SIDECAR_MAX {
        return Err(ReadError::Corrupt);
    }
    let format = serde_json::from_slice::<FormatOnly>(&bytes).map_err(|_| ReadError::Corrupt)?;
    if format.format > SIDECAR_FORMAT {
        return Err(ReadError::NotOurs);
    }
    let record: Sidecar = serde_json::from_slice(&bytes).map_err(|_| ReadError::Corrupt)?;
    let target_ok = record.target.is_absolute() && record.target.file_name().is_some();
    if record.format != SIDECAR_FORMAT || !target_ok {
        return Err(ReadError::Corrupt);
    }
    Ok(record.info(staged, sidecar))
}

/// The kept remux whose MKV or sidecar is `path`. A name that is not a kept one, or a damaged
/// or newer-format sidecar, is `RemuxStagingInvalid` (E9079); a missing sidecar is `NotFound`.
pub fn read_staged(path: &Path) -> io::Result<StagedInfo> {
    match read_pair(path) {
        Ok(info) => Ok(info),
        Err(ReadError::Io(e)) => Err(e),
        Err(ReadError::NotOurs | ReadError::Corrupt) => Err(staging_invalid()),
    }
}

/// Whether `path` is half of a kept remux: a `*.staged.mkv` / `*.staged.json` whose MKV exists
/// and whose sidecar reads (or is of a newer format). A staging cleanup skips these; anything
/// else of those names is debris (see [`staged_orphans`]).
pub fn is_kept_staged(path: &Path) -> bool {
    let Some((staged, _)) = pair_of(path) else {
        return false;
    };
    let complete = || std::fs::metadata(&staged).is_ok_and(|m| m.is_file());
    match read_pair(path) {
        Ok(_) => complete(),
        Err(ReadError::NotOurs) => complete(),
        Err(ReadError::Io(_) | ReadError::Corrupt) => false,
    }
}

fn kept_names(stage_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(stage_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                [KEPT_MKV, KEPT_JSON, KEPT_TMP]
                    .iter()
                    .any(|s| n.len() > s.len() && n.ends_with(s))
            })
        })
        .collect();
    paths.sort();
    paths
}

/// Every kept remux in `stage_dir`, oldest first. Unreadable folders, damaged sidecars and
/// sidecars without their MKV are skipped (see [`staged_orphans`]).
pub fn staged_pending(stage_dir: &Path) -> Vec<StagedInfo> {
    let mut found: Vec<StagedInfo> = kept_names(stage_dir)
        .iter()
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(KEPT_JSON)))
        .filter_map(|p| read_pair(p).ok())
        .filter(|i| std::fs::metadata(&i.staged).is_ok_and(|m| m.is_file()))
        .collect();
    found.sort_by_key(|i| (i.created_at, i.staged.clone()));
    found
}

/// Files in `stage_dir` named like a kept remux that are not one: an MKV without a readable
/// sidecar, a sidecar without its MKV, a sidecar left half-written. Safe to delete while no
/// remux runs over this folder.
pub fn staged_orphans(stage_dir: &Path) -> Vec<PathBuf> {
    kept_names(stage_dir)
        .into_iter()
        .filter(|p| !is_kept_staged(p))
        .collect()
}

/// Delete the kept remux `path` names (its MKV or its sidecar): both files and any half-written
/// sidecar. A name that is not a kept one is refused (`RemuxStagingInvalid`) and nothing is
/// touched; files already gone are not an error.
pub fn discard_staged(path: &Path) -> io::Result<()> {
    let (staged, sidecar) = pair_of(path).ok_or_else(staging_invalid)?;
    let mut first = Ok(());
    for p in [staged, tmp_of(&sidecar), sidecar] {
        if let Err(e) = remove_stale_partial(&p) {
            first = first.and(Err(e));
        }
    }
    first
}

/// Whether `info` was first kept more than `max_age` ago. A clock that moved backwards
/// expires nothing.
pub fn staged_expired(info: &StagedInfo, max_age: Duration) -> bool {
    SystemTime::now()
        .duration_since(info.created_at)
        .is_ok_and(|age| age > max_age)
}

/// The oldest of `pending` to discard so the rest hold at most `max_bytes` on the staging
/// disk (none when they already fit).
pub fn staged_over_budget(pending: &[StagedInfo], max_bytes: u64) -> Vec<&StagedInfo> {
    let mut by_age: Vec<&StagedInfo> = pending.iter().collect();
    by_age.sort_by_key(|i| i.created_at);
    let mut total: u64 = pending.iter().map(|i| i.size).sum();
    by_age
        .into_iter()
        .take_while(|i| {
            let over = total > max_bytes;
            total = total.saturating_sub(i.size);
            over
        })
        .collect()
}

/// Deliver a kept remux (`path` is its MKV or its sidecar) as the failed [`remux_iso_staged`]
/// would have: re-check the kept file (size, then a probe of its header and Cues), take
/// `<target>.lock`, copy it to `<target>.partial`, sync and verify that, land it on the target
/// and sync its folder. Phases and progress are those of the staged remux from `"verify"` on.
/// Success removes the kept pair. A storage failure keeps it again ([`StagedKept`], `attempts`
/// counted); a Stop, or a target that changed since the remux began (E9084), leaves it as is.
/// A kept file that is short or no longer verifies is deleted with its sidecar.
pub fn finish_staged(path: &Path, sink: &dyn Sink) -> io::Result<RemuxReport> {
    finish_staged_with(path, sink, &Halt::new())
}

/// [`finish_staged`] under the op token `halt`: it and [`Sink::should_cancel`] each stop it.
pub fn finish_staged_with(path: &Path, sink: &dyn Sink, halt: &Halt) -> io::Result<RemuxReport> {
    let halt = EngineHalt::new(halt, None).with_sink(sink);
    let sink = &HaltSink {
        inner: sink,
        halt: &halt,
    };
    finish_at(path, sink, &halt, &OsRemuxIo)
}

pub(super) fn finish_at(
    path: &Path,
    sink: &dyn Sink,
    halt: &EngineHalt<'_>,
    rio: &dyn RemuxIo,
) -> io::Result<RemuxReport> {
    let info = match read_pair(path) {
        Ok(info) => info,
        Err(ReadError::Io(e)) => return Err(e),
        Err(ReadError::NotOurs) => return Err(staging_invalid()),
        Err(ReadError::Corrupt) => {
            sink.log(
                Level::Warn,
                &format!("discarding {}: damaged sidecar", path.display()),
            );
            let _ = discard_staged(path);
            return Err(staging_invalid());
        }
    };
    let have = match std::fs::metadata(&info.staged) {
        Ok(m) => m.len(),
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                let _ = discard_staged(&info.sidecar);
            }
            return Err(e);
        }
    };
    if have != info.size {
        sink.log(
            Level::Warn,
            &format!("discarding {}: size changed", info.staged.display()),
        );
        let _ = discard_staged(&info.staged);
        let want = info.size;
        return Err(libfreemkv::Error::StagedCopySizeMismatch { have, want }.into());
    }
    let job = RemuxJob {
        iso: info.source.clone(),
        title: Some(info.title),
        streams: info.streams.clone(),
        target: info.target.clone(),
        replace: info.replace,
    };
    let target_partial = partial_path(&job.target);
    if info.staged == job.target || info.staged == target_partial {
        return Err(staging_invalid());
    }
    let title = libfreemkv::DiscTitle {
        duration_secs: info.expected_secs.unwrap_or(0.0),
        ..libfreemkv::DiscTitle::empty()
    };
    let timing = rio.timing();
    let watch = [info.staged.as_path()];
    let lock = halt.linked(|h| ArtifactLock::acquire(&job.target, &watch, h))?;
    let _lock = DeleteOnDrop(Some(lock));

    let now = match std::fs::symlink_metadata(&job.target) {
        Ok(m) => TargetStamp::present(&m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => TargetStamp::Absent,
        Err(e) => return Err(keep_again(&info, "target", e, sink)),
    };
    let before = info.record.target_before;
    if matches!(now, TargetStamp::Present { .. }) && before != TargetStamp::Unknown && now != before
    {
        return Err(target_exists(&job.target));
    }

    let verify = |p: &Path| verify_phase(p, &title, halt, sink, rio, timing);
    let verified = match verify(&info.staged) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::InvalidData && !libfreemkv::is_halt(&e) => {
            sink.log(
                Level::Warn,
                &format!("discarding {}: {e}", info.staged.display()),
            );
            let _ = discard_staged(&info.staged);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let step = Step {
        halt,
        sink,
        rio,
        timing,
    };
    let mut no_guard = PartialFile(None);
    let delivered = deliver(
        &job,
        &info.staged,
        Some(&target_partial),
        &verify,
        &step,
        false,
        &mut no_guard,
    );
    match delivered {
        Ok(replaced) => {
            if let Err(e) = discard_staged(&info.staged) {
                let shown = info.staged.display();
                sink.log(Level::Warn, &format!("could not remove {shown}: {e}"));
            }
            if replaced {
                sink.event(&Event::Replaced { path: &job.target });
            }
            Ok(RemuxReport {
                outcome: info.record.outcome(),
                writing_app: verified.writing_app.clone(),
                verified,
                replaced,
            })
        }
        Err((phase, e)) if keeps(&e, halt) => Err(keep_again(&info, phase, e, sink)),
        Err((_, e)) => Err(e),
    }
}

// A resume failed for a storage reason: the pair stays, its sidecar counting the attempt.
fn keep_again(
    info: &StagedInfo,
    phase: &'static str,
    cause: io::Error,
    sink: &dyn Sink,
) -> io::Error {
    let mut record = info.record.clone();
    record.attempts = record.attempts.saturating_add(1);
    record.failed(phase, &cause);
    if let Err(e) = write_sidecar(&info.sidecar, &record) {
        let shown = info.sidecar.display();
        sink.log(Level::Warn, &format!("could not update {shown}: {e}"));
    }
    sink.log(
        Level::Warn,
        &format!(
            "the {phase} step failed again ({cause}); {} is kept",
            info.staged.display()
        ),
    );
    wrap(info.staged.clone(), info.sidecar.clone(), phase, cause)
}

#[cfg(test)]
mod tests;
