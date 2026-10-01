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
use crate::recovery::{CopyOptions, CopyResult};
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
        (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs })
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
    /// A sector image (or a null read test): the copy's byte accounting, the scanned disc
    /// (its titles and mapfile locate any loss) and the image path.
    Image {
        copy: CopyResult,
        disc: Box<libfreemkv::Disc>,
        path: PathBuf,
    },
    /// A file tree: per-file accounting.
    Tree { extract: libfreemkv::ExtractResult },
}

/// Run `plan`, reporting through `sink`; its [`Sink::should_cancel`] stops the run. The
/// key sources come from `plan.keys`.
pub fn run(plan: &Plan, sink: &dyn Sink) -> crate::Result<Report> {
    run_with(
        plan,
        crate::keys::key_source_factory(&plan.keys.params()),
        sink,
    )
}

/// [`run`] asking `sources` for keys (a front end's test seam, or a seeded factory).
pub fn run_with(
    plan: &Plan,
    sources: libfreemkv::KeySourceFactory,
    sink: &dyn Sink,
) -> crate::Result<Report> {
    crate::run::with_cancel_watcher(sink, |halt| {
        let halt = libfreemkv::Halt::from_arc(halt.clone());
        match plan.output() {
            Output::Image { path, null } => image(plan, &path, null, &sources, &halt, sink),
            Output::Tree { path } => tree(plan, &path, &sources, &halt, sink),
            Output::Titles => Err(libfreemkv::Error::StreamUrlInvalid {
                url: plan.dest.clone(),
            }),
        }
    })
}

// An opened source: the scanned disc, its raw reader, and the drive's device path.
struct Opened {
    disc: libfreemkv::Disc,
    reader: Box<dyn libfreemkv::SectorSource>,
    device: Option<String>,
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
            let target = device.map_or(
                libfreemkv::DeviceTarget::Autodetect,
                libfreemkv::DeviceTarget::Path,
            );
            let progress = libfreemkv::halt::Liveness::new();
            let mut session = crate::mux::open_scan_with(
                target,
                plan.keys.credentials(),
                plan.raw,
                halt,
                &progress,
            )?;
            let device = session.device_path().to_string();
            let disc = session.take_disc().expect("the scan populated the disc");
            session.stage_drive_as_reader();
            let mut reader = session.take_reader().expect("the drive was just staged");
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
            Ok(Opened {
                disc,
                reader,
                device: Some(device),
                keys,
            })
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
                device: None,
                keys: opened.keys,
            })
        }
        _ => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.source.clone(),
        }),
    }
}

// A whole-disc copy (decrypted unless raw) into `path`, recovered over passes when
// `plan.multipass`; resumable against the image's mapfile. An image source is copied the
// same way: read errors are skipped and recorded, never a hard stop with no map.
fn image(
    plan: &Plan,
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
    let mut opened = open(plan, scope, sources, halt, sink)?;
    // The image and its mapfile are held under `<image>.lock` for the whole write (stop
    // design §2.5); a null read test writes nothing to guard.
    let lock = match null {
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
    let bridge = crate::run::ProgressBridge::new(sink);
    let opts = CopyOptions {
        decrypt: !plan.raw,
        multipass: plan.multipass,
        progress: Some(&bridge),
        halt: Some(halt.as_arc().clone()),
        keys: (!plan.raw).then(|| opened.keys.clone()),
    };
    let copied = crate::recovery::copy(&opened.disc, opened.reader.as_mut(), path, &opts);
    let done = matches!(&copied, Ok(r) if r.bytes_good > 0 && !r.halted);
    if let Some(lock) = lock
        && done
        && let Err(e) = lock.delete()
    {
        tracing::warn!(target: "freemkv::engine", "could not delete the artifact lock: {e}");
    }
    let _ = &opened.device;
    Ok(Report::Image {
        copy: copied?,
        disc: Box::new(opened.disc),
        path: path.to_path_buf(),
    })
}

// The disc's decrypted file tree into `path` (every AACS file read through the whole-disc
// key set).
fn tree(
    plan: &Plan,
    path: &std::path::Path,
    sources: &libfreemkv::KeySourceFactory,
    halt: &libfreemkv::Halt,
    sink: &dyn Sink,
) -> crate::Result<Report> {
    let scope = libfreemkv::keys::KeyScope::WholeDisc;
    let mut opened = open(plan, scope, sources, halt, sink)?;
    sink.event(&Event::Phase { name: "extract" });
    let opts = libfreemkv::ExtractOptions {
        force: plan.force,
        keys: Some(&opened.keys),
    };
    let ctx = crate::run::ctx(halt);
    let extract = opened
        .disc
        .extract_tree(opened.reader.as_mut(), path, &opts, &ctx)?;
    Ok(Report::Tree { extract })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(source: &str, dest: &str) -> Plan {
        Plan {
            source: source.into(),
            dest: dest.into(),
            ..Plan::default()
        }
    }

    #[test]
    fn the_destination_decides_the_chain() {
        assert_eq!(
            plan("disc://", "iso:///o/a.iso").output(),
            Output::Image {
                path: "/o/a.iso".into(),
                null: false
            }
        );
        assert!(matches!(
            plan("disc://", "null://").output(),
            Output::Image { null: true, .. }
        ));
        // `null://` from an image or file is a title mux to the bit bucket.
        assert_eq!(plan("iso:///i.iso", "null://").output(), Output::Titles);
        assert_eq!(
            plan("iso:///i.iso", "dir:///o/tree").output(),
            Output::Tree {
                path: "/o/tree".into()
            }
        );
        assert_eq!(plan("disc://", "mkv:///o/a.mkv").output(), Output::Titles);
    }

    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};

    // iso:// → iso:// through `run`: the decrypted image equals the engine's decrypting
    // copy of the same image, with a mapfile beside it (read errors are recorded, not fatal).
    #[test]
    fn an_image_plan_copies_the_image_decrypted() {
        let fx = bd_image(&[Some(K1), Some(K2)], 2);
        let dir = tempfile::tempdir().unwrap();
        let src = fx.write(dir.path(), "src.iso");
        let dst = dir.path().join("out.iso");
        let p = plan(
            &format!("iso://{}", src.display()),
            &format!("iso://{}", dst.display()),
        );
        let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
        let report = run_with(&p, f, &crate::NoopSink).expect("the image copies");
        let Report::Image { copy, .. } = report else {
            panic!("an image report")
        };
        assert!(copy.complete, "{copy:?}");
        assert_eq!(
            std::fs::metadata(&dst).unwrap().len(),
            fx.img.image.len() as u64
        );
        assert!(
            crate::mapfile_path_for(&dst).exists(),
            "a resumable copy keeps a mapfile"
        );
        assert!(
            !dst.with_extension("iso.lock").exists(),
            "a finished copy deletes its lock"
        );
    }

    // A raw image plan asks no key source and copies the ciphertext.
    #[test]
    fn a_raw_image_plan_asks_no_key_source() {
        let fx = bd_image(&[Some(K1)], 1);
        let dir = tempfile::tempdir().unwrap();
        let src = fx.write(dir.path(), "src.iso");
        let dst = dir.path().join("raw.iso");
        let p = Plan {
            raw: true,
            ..plan(
                &format!("iso://{}", src.display()),
                &format!("iso://{}", dst.display()),
            )
        };
        let calls = Calls::default();
        let f = factory(&[(Answer::Keydb, &[K1])], &calls);
        run_with(&p, f, &crate::NoopSink).expect("a raw copy");
        assert_eq!(calls.len(), 0, "no key call for a raw copy");
        assert_eq!(std::fs::read(&dst).unwrap(), fx.img.image);
    }

    // iso:// → dir:// through `run`: the decrypted tree.
    #[test]
    fn a_tree_plan_extracts_the_decrypted_folder() {
        let fx = bd_image(&[Some(K1)], 1);
        let dir = tempfile::tempdir().unwrap();
        let src = fx.write(dir.path(), "src.iso");
        let out = dir.path().join("tree");
        let p = plan(
            &format!("iso://{}", src.display()),
            &format!("dir://{}", out.display()),
        );
        let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
        let Report::Tree { extract } = run_with(&p, f, &crate::NoopSink).expect("extracts") else {
            panic!("a tree report")
        };
        let clip = extract
            .files
            .iter()
            .find(|f| f.path.ends_with("00000.m2ts"))
            .expect("the stream file");
        assert!(
            !extract.halted && clip.complete && clip.bytes_unreadable == 0,
            "{extract:?}"
        );
    }

    // A decrypting plan with no key opening the disc is refused before any output.
    #[test]
    fn a_decrypting_plan_with_no_key_writes_nothing() {
        let fx = bd_image(&[Some(K1)], 1);
        let dir = tempfile::tempdir().unwrap();
        let src = fx.write(dir.path(), "src.iso");
        let dst = dir.path().join("none.iso");
        let p = plan(
            &format!("iso://{}", src.display()),
            &format!("iso://{}", dst.display()),
        );
        let f = factory(&[(Answer::Keydb, &[K2])], &Calls::default());
        run_with(&p, f, &crate::NoopSink).expect_err("no key opens the disc");
        assert!(!dst.exists(), "refused before any output");
    }

    #[test]
    fn a_title_plan_is_not_run_here_yet() {
        let err = run_with(
            &plan("iso:///nowhere.iso", "mkv:///o/a.mkv"),
            std::sync::Arc::new(Vec::new),
            &crate::NoopSink,
        )
        .expect_err("a title rip goes through the title loop");
        assert!(matches!(err, libfreemkv::Error::StreamUrlInvalid { .. }));
    }
}
