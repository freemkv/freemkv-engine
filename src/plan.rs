//! The engine's one request type and its one entry (pipeline design §2.5, slice 8).
//!
//! A front end (CLI, desktop app, server) only builds a [`Plan`] and renders the [`Sink`]
//! events; [`run`] owns the work: it opens the source, acquires the keys the plan needs,
//! picks the chain from the destination's granularity (a sector image, a folder tree, or
//! PES titles), runs it and reports. No front end decides how a disc is read, keyed or
//! written.
//!
//! [`Plan`] is plain data with public fields; a front end destructures it exhaustively
//! (no `..`), so a field added here fails to compile until every front end handles it.

use crate::job::{Selection, StreamChoice};
use crate::keys::KeyParams;
use crate::multipass::{MultipassOpts, MultipassResult, PassHost};
use crate::recovery::CopyResult;
use crate::sink::{Event, Sink};
use std::path::PathBuf;

/// One rip request: what to read, what to write, and how. Pure data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Input URL: `disc://[device]`, `iso://`, `dir://`, or a container/stream URL.
    pub source: String,
    /// Output URL: `iso://`, `null://`, `dir://`, or a PES sink (`mkv://`, …).
    pub dest: String,
    /// Which titles a title rip reads.
    pub titles: Selection,
    /// Which audio and subtitle streams a title rip keeps.
    pub streams: StreamChoice,
    /// Keep the ciphertext (`--raw`, "Keep encrypted"): nothing is decrypted and no key
    /// is acquired. Default decrypts.
    pub raw: bool,
    /// Recover over passes: a sweep that skips past bad sectors, then patch passes over
    /// what it skipped, against the image's mapfile. Orthogonal to `raw`.
    pub multipass: bool,
    /// Where keys are looked up (never the keys themselves): the keydb and key service.
    pub keys: KeyParamsData,
    /// Write into a non-empty folder (`dir://`).
    pub force: bool,
}

/// [`KeyParams`] as plain comparable data, so a [`Plan`] can be compared across front ends.
/// Its `Debug` never prints the bearer token.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct KeyParamsData {
    /// The local keydb (already resolved by the front end), or `None` to skip it.
    pub keydb_path: Option<String>,
    /// The online key service, or `None`.
    pub key_url: Option<String>,
    /// Its bearer token.
    pub key_auth: Option<String>,
    /// Skip the local keydb even when `keydb_path` is set.
    pub online_only: bool,
    /// The keydb a live drive's AACS handshake takes host certificates from.
    pub cert_keydb: Option<String>,
}

impl std::fmt::Debug for KeyParamsData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyParamsData")
            .field("keydb_path", &self.keydb_path)
            .field("key_url", &self.key_url)
            .field("key_auth", &self.key_auth.as_ref().map(|_| "<redacted>"))
            .field("online_only", &self.online_only)
            .field("cert_keydb", &self.cert_keydb)
            .finish()
    }
}

impl KeyParamsData {
    /// The key-source parameters.
    pub fn params(&self) -> KeyParams {
        KeyParams {
            keydb_path: self.keydb_path.clone(),
            key_url: self.key_url.clone(),
            key_auth: self.key_auth.clone(),
            online_only: self.online_only,
        }
    }

    // The drive handshake's host certificates, from `cert_keydb`.
    fn credentials(&self) -> Option<libfreemkv::DriveCredentials> {
        let path = self.cert_keydb.as_ref()?;
        let host_certs = freemkv_keysources::KeydbSource::new(path.as_str()).host_certs();
        if host_certs.is_empty() {
            tracing::warn!(target: "freemkv::engine", "cert keydb {path}: no host certificate read");
            return None;
        }
        Some(libfreemkv::DriveCredentials { host_certs })
    }
}

/// What a plan writes, decided by the destination's granularity (sink caps, design X-6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// A sector image (`iso://`), or the null device (`null://`: a read test).
    Image { path: PathBuf, null: bool },
    /// The disc's decrypted file tree (`dir://`).
    Tree { path: PathBuf },
    /// PES titles into a container or stream sink.
    Titles,
}

impl Plan {
    /// What this plan writes.
    pub fn output(&self) -> Output {
        match libfreemkv::parse_url(&self.dest) {
            libfreemkv::StreamUrl::Iso { path } => Output::Image { path, null: false },
            libfreemkv::StreamUrl::Null if self.whole_disc_source() => Output::Image {
                path: libfreemkv::io::null_device().to_path_buf(),
                null: true,
            },
            libfreemkv::StreamUrl::Dir { path } => Output::Tree { path },
            _ => Output::Titles,
        }
    }

    // A live drive: `null://` from it is a whole-disc read test, not a title mux.
    fn whole_disc_source(&self) -> bool {
        matches!(
            libfreemkv::parse_url(&self.source),
            libfreemkv::StreamUrl::Disc { .. }
        )
    }
}

/// What [`run`] produced.
#[derive(Debug)]
pub enum Report {
    /// A sector image (or a null read test): the copy's byte accounting, the recovery's
    /// verdict when it ran passes ([`RunWith::passes`]), the scanned disc when the run
    /// opened the source itself (its titles and mapfile locate any loss) and the image path.
    Image {
        copy: CopyResult,
        recovery: Option<MultipassResult>,
        disc: Option<Box<libfreemkv::Disc>>,
        path: PathBuf,
    },
    /// A file tree: per-file accounting.
    Tree { extract: libfreemkv::ExtractResult },
    /// One title muxed into the plan's destination.
    Title { outcome: libfreemkv::MuxOutcome },
}

/// A source the front end already opened, handed to [`run_with`] so the run does not open
/// [`Plan::source`] again (one drive open and scan per rip, KU §3.2).
pub enum Held<'a> {
    /// A live drive, opened and scanned: a title run reads it as the session's reader.
    Session(&'a mut libfreemkv::DiscSession),
    /// A scanned disc and its raw reader (a drive, an opened image or folder).
    Disc {
        disc: &'a libfreemkv::Disc,
        reader: &'a mut dyn libfreemkv::SectorSource,
    },
    /// A scanned disc whose reader a [`PassHost`] owns (a recovery with the host's hooks
    /// between the passes).
    Host {
        disc: &'a libfreemkv::Disc,
        host: &'a mut dyn PassHost,
    },
    /// One scanned title over a raw reader (a held drive): a title run muxes it.
    Title {
        reader: Box<dyn libfreemkv::SectorSource>,
        title: Box<libfreemkv::ScannedTitle>,
    },
}

/// How a title run reads and writes, beyond the [`Plan`].
#[derive(Clone, Debug, Default)]
pub struct TitleOptions {
    /// The streams to keep as PIDs the front end already resolved against the title.
    /// `None` resolves [`Plan::streams`] against the scanned title.
    pub selection: Option<libfreemkv::StreamSelection>,
    /// Zero-fill and count a read error instead of failing the title (a live drive).
    pub skip_errors: bool,
    /// A live drive's read batch in sectors; 0 is the drive's detected maximum.
    pub batch_sectors: u16,
}

/// What a front end hands [`run_with`] beside the plan. `Default` holds nothing: the run
/// looks keys up from [`Plan::keys`], opens [`Plan::source`] itself, and reports only
/// through the [`Sink`].
#[derive(Default)]
pub struct RunWith<'a> {
    /// The key sources to ask instead of [`Plan::keys`] (a seeded factory, a test seam).
    pub sources: Option<libfreemkv::KeySourceFactory>,
    /// The rip's key ring, acquired once up front (KU §2.1): the run reads through it and
    /// asks no key source.
    pub keys: Option<libfreemkv::keys::KeyRing>,
    /// A source the front end already opened.
    pub held: Option<Held<'a>>,
    /// A title run's read and stream options.
    pub title: TitleOptions,
    /// An image run's recovery passes (sweep, patch passes, promotion and the loss gate).
    /// `None` is one pass: a plain copy, or with [`Plan::multipass`] one resumable pass
    /// (sweep, then each re-run patches).
    pub passes: Option<MultipassOpts>,
    /// An image run's staged scope (`(lba, sectors)`): the passes read only these sectors.
    pub scope: Option<&'a [(u32, u32)]>,
    /// The front end already holds the image's artifact lock.
    pub locked: bool,
    /// The front end's own stop token, used as the run's halt; the [`Sink`]'s
    /// `should_cancel` still cancels it.
    pub halt: Option<libfreemkv::Halt>,
    /// The front end's own listener for the library's run events (read progress, skipped
    /// sectors, the output opening), beside the [`Sink`].
    pub events: Option<std::sync::Arc<dyn libfreemkv::Events>>,
}

/// Run `plan`, reporting through `sink`; its [`Sink::should_cancel`] stops the run. The
/// key sources come from `plan.keys`.
pub fn run(plan: &Plan, sink: &dyn Sink) -> crate::Result<Report> {
    run_with(plan, RunWith::default(), sink)
}

/// [`run`] with what the front end already holds ([`RunWith`]): its key sources or key
/// ring, an opened source, a title run's options, an image run's passes, its stop token
/// and its own event listener.
pub fn run_with(plan: &Plan, with: RunWith<'_>, sink: &dyn Sink) -> crate::Result<Report> {
    let sources = with
        .sources
        .clone()
        .unwrap_or_else(|| crate::keys::key_source_factory(&plan.keys.params()));
    with_run_halt(sink, with.halt.clone(), |halt| {
        let with = RunWith {
            halt: Some(halt.clone()),
            ..with
        };
        match plan.output() {
            Output::Titles => title(plan, with, &sources, sink),
            Output::Image { path, null } => image(plan, with, &path, null, &sources, halt, sink),
            Output::Tree { path } => tree(plan, with, &path, &sources, halt, sink),
        }
    })
}

// Run `f` under the run's halt: the front end's own token, or a fresh one. The Sink's
// `should_cancel` cancels it either way, polled by a watcher for the life of the run.
fn with_run_halt<T>(
    sink: &dyn Sink,
    halt: Option<libfreemkv::Halt>,
    f: impl FnOnce(&libfreemkv::Halt) -> T,
) -> T {
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(halt) = halt else {
        return crate::run::with_cancel_watcher(sink, |flag| {
            f(&libfreemkv::Halt::from_arc(flag.clone()))
        });
    };
    // Ask once before starting: a watcher alone makes cancellation a race the work can win.
    if sink.should_cancel() {
        halt.cancel();
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let watcher = s.spawn(|| {
            while !done.load(Ordering::Acquire) {
                if sink.should_cancel() {
                    halt.cancel();
                    return;
                }
                std::thread::park_timeout(std::time::Duration::from_millis(100));
            }
        });
        let _wake = crate::run::WakeOnDrop(watcher.thread().clone());
        let _signal_done = crate::run::SignalDone(&done);
        f(&halt)
    })
}

// An opened source: the scanned disc, its raw reader, and the keys the run acquired.
struct Opened {
    disc: libfreemkv::Disc,
    reader: Box<dyn libfreemkv::SectorSource>,
    keys: libfreemkv::keys::KeyRing,
}

// Open `plan.source` and acquire the keys of `scope` (none for a raw plan): a live drive is
// brought up and scanned under `halt`; an image or folder through the one image door.
fn open(
    plan: &Plan,
    scope: libfreemkv::keys::KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    halt: &libfreemkv::Halt,
    sink: &dyn Sink,
) -> crate::Result<Opened> {
    sink.event(&Event::Phase { name: "open" });
    match libfreemkv::parse_url(&plan.source) {
        libfreemkv::StreamUrl::Disc { device } => {
            let mut session = open_drive(device, plan, halt)?;
            let device = session.device_path().to_string();
            let disc = session.take_disc().ok_or(libfreemkv::Error::NoStreams)?;
            session.stage_drive_as_reader();
            let mut reader =
                session
                    .take_reader()
                    .ok_or_else(|| libfreemkv::Error::DeviceNotReady {
                        path: device.clone(),
                    })?;
            sink.event(&Event::SourceOpened {
                device: Some(&device),
                disc: &disc,
            });
            let (keys, trace) = crate::keys::resolve_for_rip_traced(
                &disc,
                reader.as_mut(),
                scope.clone(),
                sources,
                None,
                Some(halt),
            );
            sink.event(&Event::Keys {
                trace: &trace,
                ring: keys.as_ref().ok(),
            });
            let keys = keys?;
            libfreemkv::keys::check_decryptable(&disc, plan.raw, Some(&keys), &scope)?;
            Ok(Opened { disc, reader, keys })
        }
        libfreemkv::StreamUrl::Iso { .. } | libfreemkv::StreamUrl::Dir { .. } => {
            let src = crate::ImageSource::from_url(&plan.source).ok_or_else(|| {
                libfreemkv::Error::StreamUrlInvalid {
                    url: plan.source.clone(),
                }
            })?;
            if let crate::ImageSource::Iso(path) = &src {
                crate::ensure_whole_image(path)?;
            }
            let opts = crate::OpenImageOptions {
                scope: Some(scope.clone()),
                halt: Some(halt.clone()),
                ..crate::OpenImageOptions::resolve(sources.clone())
            };
            let (opened, trace) = crate::open_image_with_traced(&src, opts);
            if let Ok(o) = &opened {
                sink.event(&Event::SourceOpened {
                    device: None,
                    disc: &o.disc,
                });
            }
            sink.event(&Event::Keys {
                trace: &trace,
                ring: opened.as_ref().ok().map(|o| &o.keys),
            });
            let opened = opened?;
            libfreemkv::keys::check_decryptable(
                &opened.disc,
                plan.raw,
                Some(&opened.keys),
                &scope,
            )?;
            Ok(Opened {
                disc: opened.disc,
                reader: opened.reader,
                keys: opened.keys,
            })
        }
        _ => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.source.clone(),
        }),
    }
}

// Bring up and scan the drive at `device` (autodetect when `None`), locking its tray.
fn open_drive(
    device: Option<std::path::PathBuf>,
    plan: &Plan,
    halt: &libfreemkv::Halt,
) -> crate::Result<libfreemkv::DiscSession> {
    let target = device.map_or(
        libfreemkv::DeviceTarget::Autodetect,
        libfreemkv::DeviceTarget::Path,
    );
    let progress = libfreemkv::halt::Liveness::new();
    crate::mux::open_scan_with(target, plan.keys.credentials(), plan.raw, halt, &progress)
}

// What the run reads for an image or tree: what the front end holds, or what `open` opened.
enum Source<'a> {
    Held(&'a libfreemkv::Disc, Reading<'a>),
    Opened(Box<Opened>),
}

enum Reading<'a> {
    Reader(&'a mut dyn libfreemkv::SectorSource),
    Host(&'a mut dyn PassHost),
}

// The run's source for a whole-disc output of `scope`: the held one (keyed by the
// front end's ring), or `plan.source` opened and keyed here.
fn whole_source<'a>(
    plan: &Plan,
    held: Option<Held<'a>>,
    scope: libfreemkv::keys::KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    halt: &libfreemkv::Halt,
    sink: &dyn Sink,
) -> crate::Result<Source<'a>> {
    match held {
        Some(Held::Disc { disc, reader }) => Ok(Source::Held(disc, Reading::Reader(reader))),
        Some(Held::Host { disc, host }) => Ok(Source::Held(disc, Reading::Host(host))),
        Some(Held::Session(_) | Held::Title { .. }) => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.dest.clone(),
        }),
        None => open(plan, scope, sources, halt, sink).map(|o| Source::Opened(Box::new(o))),
    }
}

// A whole-disc copy (decrypted unless raw) into `path`: one plain pass, one resumable pass
// (`plan.multipass` with no `with.passes`), or the full recovery loop; resumable against
// the image's mapfile.
fn image(
    plan: &Plan,
    with: RunWith<'_>,
    path: &std::path::Path,
    null: bool,
    sources: &libfreemkv::KeySourceFactory,
    halt: &libfreemkv::Halt,
    sink: &dyn Sink,
) -> crate::Result<Report> {
    let scope = if plan.raw {
        libfreemkv::keys::KeyScope::None
    } else {
        libfreemkv::keys::KeyScope::WholeDisc
    };
    let RunWith {
        keys,
        held,
        passes,
        scope: staged,
        locked,
        ..
    } = with;
    let mut source = whole_source(plan, held, scope, sources, halt, sink)?;
    // The image and its mapfile are held under `<image>.lock` for the whole write (stop
    // design §2.5); a null read test writes nothing to guard.
    let lock = match null || locked {
        true => None,
        false => {
            let mapfile = crate::mapfile_path_for(path);
            Some(libfreemkv::io::ArtifactLock::acquire(
                path,
                &[mapfile.as_path()],
                halt,
            )?)
        }
    };
    sink.event(&Event::Phase { name: "copy" });
    let (disc, mut reading, keys): (&libfreemkv::Disc, Reading<'_>, _) = match &mut source {
        Source::Held(disc, r) => {
            let r = match r {
                Reading::Reader(r) => Reading::Reader(&mut **r),
                Reading::Host(h) => Reading::Host(&mut **h),
            };
            (*disc, r, keys)
        }
        // A raw copy of what the run opened reads with no keys.
        Source::Opened(o) => {
            let keys = keys.or_else(|| (!plan.raw).then(|| o.keys.clone()));
            (&o.disc, Reading::Reader(o.reader.as_mut()), keys)
        }
    };
    let job = crate::Job {
        selection: plan.titles.clone(),
        raw: plan.raw,
        keys,
        ..crate::Job::new(plan.source.clone(), path.display().to_string())
    };
    let eh = crate::EngineHalt::new(halt, None).with_sink(sink);
    let (copied, recovery) = match passes {
        Some(opts) => {
            let r = match &mut reading {
                Reading::Reader(r) => crate::multipass::recover(
                    disc,
                    &mut crate::ReaderHost(&mut **r),
                    path,
                    &job,
                    &opts,
                    staged,
                    sink,
                    &eh,
                ),
                Reading::Host(h) => {
                    crate::multipass::recover(disc, &mut **h, path, &job, &opts, staged, sink, &eh)
                }
            };
            match r {
                Ok(r) => (
                    Ok(CopyResult::new(
                        disc.capacity_bytes,
                        r.good_bytes,
                        r.unreadable_bytes,
                        r.pending_bytes,
                        0,
                        r.halted,
                    )),
                    Some(r),
                ),
                Err(e) => (Err(e), None),
            }
        }
        None => {
            let reader: &mut dyn libfreemkv::SectorSource = match &mut reading {
                Reading::Reader(r) => &mut **r,
                Reading::Host(h) => h.reader(),
            };
            let copied = crate::multipass::one_pass(
                disc,
                reader,
                path,
                &job,
                plan.multipass,
                sink,
                (&eh, halt),
            );
            (copied, None)
        }
    };
    let done = matches!(&copied, Ok(r) if r.bytes_good > 0 && !r.halted);
    if let Some(lock) = lock
        && done
        && let Err(e) = lock.delete()
    {
        tracing::warn!(target: "freemkv::engine", "could not delete the artifact lock: {e}");
    }
    let copy = copied?;
    let disc = match source {
        Source::Opened(o) => Some(Box::new(o.disc)),
        Source::Held(..) => None,
    };
    Ok(Report::Image {
        copy,
        recovery,
        disc,
        path: path.to_path_buf(),
    })
}

// The disc's decrypted file tree into `path` (every AACS file read through the whole-disc
// key set), through the `dir://` tree sink. A raw tree is refused: ciphertext belongs in `iso://`.
fn tree(
    plan: &Plan,
    with: RunWith<'_>,
    path: &std::path::Path,
    sources: &libfreemkv::KeySourceFactory,
    halt: &libfreemkv::Halt,
    sink: &dyn Sink,
) -> crate::Result<Report> {
    if plan.raw {
        return Err(libfreemkv::Error::DirRawRejected);
    }
    let scope = libfreemkv::keys::KeyScope::WholeDisc;
    let RunWith { keys, held, .. } = with;
    let mut source = whole_source(plan, held, scope, sources, halt, sink)?;
    sink.event(&Event::Phase { name: "extract" });
    let mut out = libfreemkv::io::TreeSink::create(path, plan.force)?;
    let ctx = crate::run::ctx(halt);
    let extract = match &mut source {
        Source::Held(disc, r) => {
            let reader: &mut dyn libfreemkv::SectorSource = match r {
                Reading::Reader(r) => &mut **r,
                Reading::Host(h) => h.reader(),
            };
            disc.extract_into(reader, &mut out, keys.as_ref(), &ctx)?
        }
        Source::Opened(o) => {
            let keys = keys.as_ref().unwrap_or(&o.keys);
            o.disc
                .extract_into(o.reader.as_mut(), &mut out, Some(keys), &ctx)?
        }
    };
    Ok(Report::Tree { extract })
}

// One title of `plan.source` muxed into `plan.dest`: off a held drive or title, or the
// source opened here (a drive scanned and keyed; an image, folder or container by URL).
fn title(
    plan: &Plan,
    with: RunWith<'_>,
    sources: &libfreemkv::KeySourceFactory,
    sink: &dyn Sink,
) -> crate::Result<Report> {
    let RunWith {
        keys,
        held,
        title: topts,
        halt,
        events,
        ..
    } = with;
    let watch = Watch {
        sink,
        dest: &plan.dest,
        halt,
        events,
    };
    let opts = |idx: usize, title: Option<&libfreemkv::DiscTitle>| {
        Ok::<_, libfreemkv::Error>(libfreemkv::MuxOptions {
            skip_errors: topts.skip_errors,
            batch_sectors: topts.batch_sectors,
            raw: plan.raw,
            selection: selection(plan, &topts, title)?,
            title_index: idx,
        })
    };
    let outcome = match held {
        Some(Held::Session(session)) => {
            let disc = session.disc().ok_or(libfreemkv::Error::NoStreams)?;
            let idx = title_index(plan, Some(disc))?;
            let mut mux = opts(idx, disc.titles.get(idx))?;
            if mux.batch_sectors == 0 {
                mux.batch_sectors =
                    libfreemkv::disc::detect_max_batch_sectors(session.device_path());
            }
            let hint = disc.titles.get(idx).map_or(0, |t| t.size_bytes);
            let label = format!("disc title {}", idx + 1);
            session.stage_drive_as_reader();
            let source = libfreemkv::Source::from_session(session);
            watch.mux(&label, hint, |ctx| {
                libfreemkv::mux_with_keys(source, keys.as_ref(), &plan.dest, &mux, ctx)
            })
        }
        Some(Held::Title { reader, title }) => {
            let mux = opts(0, Some(&title.title))?;
            let hint = title.title.size_bytes;
            let label = format!("disc title {}", title.title.playlist);
            let source = libfreemkv::Source::from_reader(reader, *title);
            watch.mux(&label, hint, |ctx| {
                libfreemkv::mux_with_keys(source, keys.as_ref(), &plan.dest, &mux, ctx)
            })
        }
        Some(Held::Disc { .. } | Held::Host { .. }) => {
            return Err(libfreemkv::Error::StreamUrlInvalid {
                url: plan.source.clone(),
            });
        }
        None => match libfreemkv::parse_url(&plan.source) {
            libfreemkv::StreamUrl::Disc { device } => {
                return title_off_drive(plan, device, keys, sources, &topts, &watch);
            }
            _ => {
                let idx = title_index(plan, None)?;
                let need_keys = keys.is_none() && !plan.raw;
                let need_title = topts.selection.is_none() && !plan.streams.is_all();
                let (found, scanned) = match need_keys || need_title {
                    true => url_source(plan, idx, need_keys, sources, &watch)?,
                    false => (None, None),
                };
                let keys = keys.or(found);
                let mux = opts(idx, scanned.as_ref())?;
                watch.mux(&plan.source, 0, |ctx| {
                    libfreemkv::mux_url(&plan.source, keys.as_ref(), &plan.dest, &mux, ctx)
                })
            }
        },
    };
    Ok(Report::Title {
        outcome: outcome.map_err(libfreemkv::Error::from)?,
    })
}

// A title of the drive at `device`, opened, scanned and keyed here (no ring in hand: one
// acquisition over the title).
fn title_off_drive(
    plan: &Plan,
    device: Option<std::path::PathBuf>,
    keys: Option<libfreemkv::keys::KeyRing>,
    sources: &libfreemkv::KeySourceFactory,
    topts: &TitleOptions,
    watch: &Watch<'_>,
) -> crate::Result<Report> {
    let halt = watch.halt.clone().unwrap_or_default();
    let mut session = open_drive(device, plan, &halt)?;
    let idx = title_index(plan, session.disc())?;
    let scope = libfreemkv::keys::KeyScope::Titles(vec![idx]);
    // A raw title reads the ciphertext: no key is acquired.
    let keys = match keys {
        Some(k) => Some(k),
        None if plan.raw => None,
        None => {
            let walk = std::sync::Mutex::new(libfreemkv::aacs::trace::ResolutionTrace::new());
            let opts = libfreemkv::keys::AcquireOptions {
                trace: Some(&walk),
                ..Default::default()
            };
            let ctx = crate::run::ctx(&halt);
            let r = session
                .acquire_keys(scope.clone(), sources, opts, &ctx)
                .map(|r| r.keys);
            let trace = walk.into_inner().unwrap_or_else(|e| e.into_inner());
            watch.sink.event(&Event::Keys {
                trace: &trace,
                ring: r.as_ref().ok(),
            });
            Some(r?)
        }
    };
    let disc = session.disc().ok_or(libfreemkv::Error::NoStreams)?;
    libfreemkv::keys::check_decryptable(disc, plan.raw, keys.as_ref(), &scope)?;
    let with = RunWith {
        keys,
        held: Some(Held::Session(&mut session)),
        title: topts.clone(),
        halt: Some(halt),
        events: watch.events.clone(),
        ..RunWith::default()
    };
    title(plan, with, sources, watch.sink)
}

// A URL source's keys and scanned title for title `idx`: an image or folder is opened (its
// keys acquired over the title when `need_keys`); a loose clip's keys come from its disc
// folder; a container or stream has neither.
#[allow(clippy::type_complexity)]
fn url_source(
    plan: &Plan,
    idx: usize,
    need_keys: bool,
    sources: &libfreemkv::KeySourceFactory,
    watch: &Watch<'_>,
) -> crate::Result<(
    Option<libfreemkv::keys::KeyRing>,
    Option<libfreemkv::DiscTitle>,
)> {
    let halt = watch.halt.clone().unwrap_or_default();
    let (keys, title, trace) = match libfreemkv::parse_url(&plan.source) {
        libfreemkv::StreamUrl::Iso { .. } | libfreemkv::StreamUrl::Dir { .. } => {
            let src = crate::ImageSource::from_url(&plan.source).ok_or_else(|| {
                libfreemkv::Error::StreamUrlInvalid {
                    url: plan.source.clone(),
                }
            })?;
            let scope = match need_keys {
                true => libfreemkv::keys::KeyScope::Titles(vec![idx]),
                false => libfreemkv::keys::KeyScope::None,
            };
            let opts = crate::OpenImageOptions {
                scope: Some(scope),
                halt: Some(halt),
                ..crate::OpenImageOptions::resolve(sources.clone())
            };
            let (opened, trace) = crate::open_image_with_traced(&src, opts);
            let title = opened
                .as_ref()
                .ok()
                .and_then(|o| o.disc.titles.get(idx).cloned());
            (opened.map(|o| need_keys.then_some(o.keys)), title, trace)
        }
        libfreemkv::StreamUrl::M2ts { path } if need_keys => {
            let (keys, trace) = crate::keys::resolve_loose_clip(&path, sources, Some(&halt));
            (keys, None, trace)
        }
        _ => return Ok((None, None)),
    };
    if need_keys {
        watch.sink.event(&Event::Keys {
            trace: &trace,
            ring: keys.as_ref().ok().and_then(Option::as_ref),
        });
    }
    Ok((keys?, title))
}

// The one title a title run muxes: `Titles([i])` is title `i`; `MainMovie` the disc's main
// title (index 0 for a source with no scanned disc). Anything else names several titles,
// which a title run does not mux.
fn title_index(plan: &Plan, disc: Option<&libfreemkv::Disc>) -> crate::Result<usize> {
    match (&plan.titles, disc) {
        (crate::Selection::Titles(t), _) if t.len() == 1 => Ok(t[0]),
        (crate::Selection::MainMovie, None) => Ok(0),
        (sel, Some(disc)) if !matches!(sel, crate::Selection::Titles(_)) => {
            match crate::mux::resolve_selection_with_audio(disc, sel, &plan.streams.audio)
                .as_slice()
            {
                [one] => Ok(*one),
                _ => Err(libfreemkv::Error::StreamUrlInvalid {
                    url: plan.dest.clone(),
                }),
            }
        }
        _ => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.dest.clone(),
        }),
    }
}

// The PIDs a title run keeps: the front end's, else `plan.streams` against the scanned
// title (everything when the plan keeps everything).
fn selection(
    plan: &Plan,
    topts: &TitleOptions,
    title: Option<&libfreemkv::DiscTitle>,
) -> crate::Result<libfreemkv::StreamSelection> {
    if let Some(sel) = &topts.selection {
        return Ok(sel.clone());
    }
    if plan.streams.is_all() {
        return Ok(libfreemkv::StreamSelection::default());
    }
    let title = title.ok_or_else(|| libfreemkv::Error::StreamUrlInvalid {
        url: plan.source.clone(),
    })?;
    plan.streams.resolve(title).map_err(libfreemkv::Error::from)
}

// A title mux's bridge to the front end: the Sink's progress and cancel, its own token and
// event listener.
struct Watch<'a> {
    sink: &'a dyn Sink,
    dest: &'a str,
    halt: Option<libfreemkv::Halt>,
    events: Option<std::sync::Arc<dyn libfreemkv::Events>>,
}

impl Watch<'_> {
    fn mux(
        &self,
        label: &str,
        hint: u64,
        f: impl FnOnce(&libfreemkv::Ctx) -> std::io::Result<libfreemkv::MuxOutcome>,
    ) -> std::io::Result<libfreemkv::MuxOutcome> {
        crate::mux::with_mux_watcher_for(
            self.sink,
            self.dest,
            self.halt.clone(),
            self.events.clone(),
            |ctx| {
                crate::mux::log_mux_start(self.sink, label, self.dest, hint);
                f(ctx)
            },
        )
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
