//! AACS key-source orchestration and the rip's up-front key set, shared by every front-end.
//!
//! Both the CLI and the desktop UI build the SAME local-first ordered
//! [`freemkv_keysources::KeySource`] list ([`key_source_factory`]) and resolve a rip's keys
//! through ONE call, [`resolve_for_rip`], before any output (keys-upfront design, KU §2.1):
//! the resulting [`KeyRing`] lives in memory only and is handed to every pass and
//! mux of the rip, which never ask a source again.
//!
//! [`KeyParams`] is a thin, already-resolved shape, never re-interpreted here.

use libfreemkv::aacs::trace::ResolutionTrace;
use libfreemkv::keys::{AcquireOptions, DecryptStatus, KeyRing, KeyScope};

/// Already-resolved key configuration, boundary-normalized by the calling
/// shell. Each field's doc says what it means and does NOT mean.
#[derive(Clone, Default)]
pub struct KeyParams {
    /// The local keydb path to consult, or `None` to skip the local source
    /// entirely. The caller has already applied whatever fallback/search
    /// chain or `shellexpand` its shell uses — this module does no further
    /// resolution of the path itself.
    pub keydb_path: Option<String>,
    /// The online key-service URL, or `None` to skip it. One wrong on its face is dropped
    /// here without DNS (see [`key_url_rejection`]); the local source still applies. Its host
    /// is looked up and SSRF-guarded at the first query (J10).
    pub key_url: Option<String>,
    /// Bearer token for the online service, if any.
    pub key_auth: Option<String>,
    /// Skip the local keydb source even when `keydb_path` is `Some` — an
    /// explicit, independent toggle (not re-derived from `keydb_path`).
    pub online_only: bool,
}

/// Build the ordered `KeySource` list, **local-first**: the keydb (unless
/// `online_only`) then the online service (unless its URL is absent or
/// permanently SSRF-rejected). Prints nothing: a dropped URL is a `freemkv::keys`
/// tracing warning and [`key_url_rejection`]'s typed verdict.
///
/// KU J10: no DNS lookup here (a factory build has no Stop). The host is looked up at the
/// first query, which is not yet Stop-aware (ST-K1b wires `ctx.halt()` into it); `resolve`'s
/// own retry waits are. A lookup with no answer is retried up front until 60 s pass with no
/// answer (J13); one that finds a non-public address is refused.
pub fn key_sources(p: &KeyParams) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    warn_dropped_url(p);
    sources_quiet(p)
}

// `key_sources` without the dropped-URL warning (a factory warns once, at build).
fn sources_quiet(p: &KeyParams) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    let mut sources: Vec<Box<dyn freemkv_keysources::KeySource>> = Vec::new();

    if !p.online_only
        && let Some(path) = &p.keydb_path
    {
        sources.push(Box::new(freemkv_keysources::KeydbSource::new(path.clone())));
    }

    if let (Some(url), None) = (&p.key_url, key_url_rejection(p)) {
        sources.push(Box::new(freemkv_keysources::OnlineSource::new(
            url.clone(),
            p.key_auth.clone().unwrap_or_default(),
        )));
    }

    sources
}

// Neither the URL nor the check's text: a URL may carry credentials.
fn warn_dropped_url(p: &KeyParams) {
    if let Some(r) = key_url_rejection(p) {
        tracing::warn!(
            target: "freemkv::keys",
            fault = ?r.fault,
            "key_url dropped by the static check"
        );
    }
}

/// Why [`key_sources`] leaves `p.key_url` out, if it does: the static check's typed verdict
/// (no DNS lookup), for a front-end to localize. `None` when no URL is set or it passes.
pub fn key_url_rejection(p: &KeyParams) -> Option<freemkv_keysources::KeyserverUrlRejection> {
    let url = p.key_url.as_deref()?;
    freemkv_keysources::check_keyserver_url_static(url).err()
}

/// Build the [`libfreemkv::KeySourceFactory`] a rip's [`resolve_for_rip`] calls to build
/// the ordered sources from `p`, with the J10 transient-URL rule of [`key_sources`].
/// A dropped `key_url` is warned once, here, not on every call.
pub fn key_source_factory(p: &KeyParams) -> libfreemkv::KeySourceFactory {
    warn_dropped_url(p);
    let p = p.clone();
    std::sync::Arc::new(move || sources_quiet(&p))
}

/// What a rip writes, which decides what it must decrypt (KU §2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RipOutput {
    /// MKV / M2TS / MP4 / demux / json / fvi: the selected titles.
    Streams,
    /// A decrypted ISO or folder: every stream file.
    DecryptedImage,
    /// A raw copy (`--raw`, "Keep encrypted"): nothing is decrypted.
    RawImage,
}

/// The [`KeyScope`] a rip of `titles` (indices into `disc.titles`) to `output` resolves
/// (KU §2.5): `Titles(selected)`, or `Titles([main])` for a plain rip with no selection;
/// `WholeDisc` for a decrypted image; `None` for a raw copy, which makes no key call.
pub fn rip_scope(disc: &libfreemkv::Disc, titles: &[usize], output: RipOutput) -> KeyScope {
    match output {
        RipOutput::RawImage => KeyScope::None,
        RipOutput::DecryptedImage => KeyScope::WholeDisc,
        RipOutput::Streams if titles.is_empty() => {
            KeyScope::Titles(crate::resolve_selection(disc, &crate::Selection::MainMovie))
        }
        RipOutput::Streams => KeyScope::Titles(titles.to_vec()),
    }
}

/// Resolve a rip's keys ONCE, before any output (KU §2.1, §2.3): the one engine call every
/// front-end makes. `seed` is a set the rip already holds (its keys join the pool first);
/// `halt` stops the resolve and its retry waits. The factory is dropped by the resolve, so
/// nothing can ask a source after this returns; the set is memory-only.
///
/// `Err` refuses the rip before any output (E7022/E7032 Missing, E7013, E7026, E7028–30,
/// `Halted`).
pub fn resolve_for_rip(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    seed: Option<&KeyRing>,
    halt: Option<&libfreemkv::Halt>,
) -> crate::Result<KeyRing> {
    resolve_for_rip_traced(disc, reader, scope, sources, seed, halt).0
}

/// [`resolve_for_rip_traced`] reporting to an open's `progress` (stop design v5 §4.3, T29):
/// each source call holds it `busy()` and hands it out as `ResolveCtx::progress()`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_for_rip_observed(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    seed: Option<&KeyRing>,
    halt: Option<&libfreemkv::Halt>,
    progress: &libfreemkv::halt::Liveness,
) -> (crate::Result<KeyRing>, ResolutionTrace) {
    resolve_traced(disc, reader, scope, sources, seed, halt, Some(progress))
}

/// [`resolve_for_rip`], also returning the per-source walk ("keydb > matched disc > online >
/// …") on success AND on a refusal, for the device log and the "why no key" answer. It holds
/// source labels, node outcomes and counts only, never a key, VID or MKB byte.
pub fn resolve_for_rip_traced(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    seed: Option<&KeyRing>,
    halt: Option<&libfreemkv::Halt>,
) -> (crate::Result<KeyRing>, ResolutionTrace) {
    resolve_traced(disc, reader, scope, sources, seed, halt, None)
}

/// The keys for a loose Blu-ray clip (`m2ts://`), looked up the only way a loose file allows:
/// walk up to its disc folder ([`libfreemkv::disc_root_of`]), scan it, and resolve once over
/// the titles that play the clip (every title when none names it, so an unrelated keyless
/// title can then refuse it). `Ok(None)`: no readable disc
/// structure or no AACS, so the clip reads keyless and an encrypted one refuses (E7022).
pub fn resolve_loose_clip(
    clip: &std::path::Path,
    sources: &libfreemkv::KeySourceFactory,
    halt: Option<&libfreemkv::Halt>,
) -> (crate::Result<Option<KeyRing>>, ResolutionTrace) {
    let Some(root) = libfreemkv::disc_root_of(clip) else {
        return (Ok(None), ResolutionTrace::new());
    };
    // A folder that is not a readable disc looks up nothing: a clear clip still opens, an
    // encrypted one refuses E7022 in `input()`. An OS read error (EIO, EACCES) surfaces.
    let scanned = match crate::image::scan_image(&crate::ImageSource::Dir(root.clone())) {
        Ok((disc, _)) if disc.aacs.is_none() => return (Ok(None), ResolutionTrace::new()),
        r => r,
    };
    let (disc, mut reader) = match scanned {
        Ok(d) => d,
        Err(libfreemkv::Error::IoError { source })
            if !matches!(source.kind(), std::io::ErrorKind::NotFound) =>
        {
            return (
                Err(libfreemkv::Error::IoError { source }),
                ResolutionTrace::new(),
            );
        }
        Err(e) => {
            tracing::warn!(target: "freemkv::keys", error = %e, "loose clip: disc folder unreadable");
            return (Ok(None), ResolutionTrace::new());
        }
    };
    let stem = clip
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let plays =
        |t: &libfreemkv::DiscTitle| t.clips.iter().any(|c| c.clip_id.eq_ignore_ascii_case(stem));
    let mut titles: Vec<usize> = (0..disc.titles.len())
        .filter(|&i| plays(&disc.titles[i]))
        .collect();
    if titles.is_empty() {
        titles = (0..disc.titles.len()).collect();
    }
    let scope = KeyScope::Titles(titles);
    let (set, trace) = resolve_traced(&disc, reader.as_mut(), scope, sources, None, halt, None);
    (set.map(Some), trace)
}

#[allow(clippy::too_many_arguments)]
fn resolve_traced(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    seed: Option<&KeyRing>,
    halt: Option<&libfreemkv::Halt>,
    progress: Option<&libfreemkv::halt::Liveness>,
) -> (crate::Result<KeyRing>, ResolutionTrace) {
    let scope_log = format!("{scope:?}");
    let walk = std::sync::Mutex::new(ResolutionTrace::new());
    let req = Acquire {
        seed,
        vid: None,
        halt,
        progress,
    };
    let r = acquire(disc, reader, &scope, sources, req, &walk)
        .map(|r| r.keys)
        .map_err(|(e, _)| e);
    let walk = walk.into_inner().unwrap_or_else(|e| e.into_inner());
    match &r {
        Ok(keys) => log_status(keys, &scope_log),
        Err(e) => tracing::info!(
            target: "freemkv::keys",
            error = %e,
            scope = scope_log,
            walk = ?walk.keys,
            "keys: refused up front"
        ),
    }
    (r, walk)
}

// What one acquisition is asked with, beyond the disc and its scope.
#[derive(Default)]
pub(crate) struct Acquire<'a> {
    pub(crate) seed: Option<&'a KeyRing>,
    pub(crate) vid: Option<[u8; 16]>,
    pub(crate) halt: Option<&'a libfreemkv::Halt>,
    pub(crate) progress: Option<&'a libfreemkv::halt::Liveness>,
}

// The engine's one key acquisition (drive and image rips alike): evidence from the scanned
// disc over its raw reader, then `KeyRing::acquire`. `Err` carries whether a VID would
// have helped (KU J23).
pub(crate) fn acquire(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: &KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    req: Acquire<'_>,
    walk: &std::sync::Mutex<ResolutionTrace>,
) -> Result<libfreemkv::keys::KeyResolution, (libfreemkv::Error, bool)> {
    let help = std::sync::atomic::AtomicBool::new(false);
    let opts = AcquireOptions {
        seed: req.seed,
        vid: req.vid,
        vid_would_help: Some(&help),
        trace: Some(walk),
        liveness: req.progress,
    };
    let ctx = libfreemkv::Ctx::new(req.halt.cloned().unwrap_or_default());
    KeyRing::acquire_for_disc(disc, reader, scope.clone(), sources, opts, &ctx)
        .map_err(|e| (e, help.load(std::sync::atomic::Ordering::SeqCst)))
}

// The qa key log line (KU §7.6): counts only, never a key or the VID.
pub(crate) fn log_status(keys: &KeyRing, scope: &str) {
    let st = keys.status();
    tracing::info!(
        target: "freemkv::keys",
        requests = st.requests,
        scope,
        keyed = st.keyed,
        clear = st.clear,
        lazy = st.lazy,
        forensic = ?st.forensic,
        declared = ?st.declared,
        "keys: resolved up front"
    );
}

/// Whether `disc` can be decrypted from the rip's `set` (KU §12.2: `Ready`, Missing as
/// `AacsKeysMissing`, `ForensicPending`); CSS and clear discs as the library reads them.
pub fn key_status(disc: &libfreemkv::Disc, set: &KeyRing) -> DecryptStatus {
    libfreemkv::keys::decrypt_status(disc, Some(set))
}

/// Which key source won, from a resolution trace: the first
/// [`libfreemkv::aacs::trace::KeyOutcome::Resolved`] step's source label, or
/// `None` if nothing resolved.
pub fn won_source(trace: &libfreemkv::aacs::trace::ResolutionTrace) -> Option<String> {
    trace
        .keys
        .iter()
        .find(|step| step.outcome == libfreemkv::aacs::trace::KeyOutcome::Resolved)
        .map(|step| step.who.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keydb_before_online() {
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("https://8.8.8.8/keys".into()),
            key_auth: None,
            online_only: false,
        };
        let s = key_sources(&p);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].label(), "keydb", "local keydb is tried first");
        assert_eq!(s[1].label(), "online", "online service is the fallback");
    }

    #[test]
    fn keydb_only_when_no_url() {
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: None,
            key_auth: None,
            online_only: false,
        };
        let s = key_sources(&p);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label(), "keydb");
    }

    #[test]
    fn online_only_drops_the_keydb_source() {
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("https://8.8.8.8/keys".into()),
            key_auth: None,
            online_only: true,
        };
        let s = key_sources(&p);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label(), "online", "online_only drops the local keydb");
    }

    #[test]
    fn no_keydb_path_means_no_local_source() {
        // A `None` keydb_path (the shell decided to omit it) yields no local
        // source even though `online_only` is false — the shell's fallback
        // policy, not this module's, decides whether that ever happens.
        let p = KeyParams {
            keydb_path: None,
            key_url: None,
            key_auth: None,
            online_only: false,
        };
        assert!(key_sources(&p).is_empty());
    }

    #[test]
    fn ssrf_rejected_url_is_dropped() {
        // Unspecified/class-E endpoints fail `validate_keyserver_url` and must
        // not be added as a source; the keydb (if any) still applies.
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("http://0.0.0.0/latest/meta-data".into()),
            key_auth: None,
            online_only: false,
        };
        let s = key_sources(&p);
        assert_eq!(s.len(), 1, "rejected url dropped; keydb remains");
        assert_eq!(s[0].label(), "keydb");

        let p_url_only = KeyParams {
            keydb_path: None,
            key_url: Some("https://240.0.0.1:8443/keys".into()),
            key_auth: None,
            online_only: false,
        };
        assert!(
            key_sources(&p_url_only).is_empty(),
            "invalid-address url-only must yield zero sources"
        );
    }

    #[test]
    fn won_source_picks_first_resolved_step() {
        use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep, ResolutionTrace};

        let trace = ResolutionTrace {
            unlock: Vec::new(),
            keys: vec![
                KeyStep {
                    who: "keydb".to_string(),
                    path: vec![KeyNode::NoEntry],
                    outcome: KeyOutcome::NoKey,
                    matched_entry: None,
                    store_entries: None,
                },
                KeyStep {
                    who: "online".to_string(),
                    path: vec![KeyNode::MatchedDisc, KeyNode::FoundUnitKeys],
                    outcome: KeyOutcome::Resolved,
                    matched_entry: None,
                    store_entries: None,
                },
            ],
        };
        assert_eq!(won_source(&trace), Some("online".to_string()));
    }

    #[test]
    fn won_source_none_when_nothing_resolved() {
        use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep, ResolutionTrace};

        let trace = ResolutionTrace {
            unlock: Vec::new(),
            keys: vec![KeyStep {
                who: "keydb".to_string(),
                path: vec![KeyNode::NoEntry],
                outcome: KeyOutcome::NoKey,
                matched_entry: None,
                store_entries: None,
            }],
        };
        assert_eq!(won_source(&trace), None);
    }

    // ── KU-E1: the front door (KU §3.2) ─────────────────────────────────────

    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    use libfreemkv::keys::{DecryptStatus, KeyRing, KeyScope};

    /// EK5 (KU §2.5): MKV/M2TS/MP4/… → `Titles(selected)`, a plain rip `Titles([main])`;
    /// decrypted ISO or folder → `WholeDisc`; raw copy → `None` (no key call).
    #[test]
    fn rip_scope_table() {
        let fx = bd_image(&[Some(K1), Some(K2)], 2);
        let d = &fx.disc;
        assert_eq!(
            rip_scope(d, &[1], RipOutput::Streams),
            KeyScope::Titles(vec![1])
        );
        assert_eq!(
            rip_scope(d, &[0, 1], RipOutput::Streams),
            KeyScope::Titles(vec![0, 1])
        );
        assert_eq!(
            rip_scope(d, &[], RipOutput::Streams),
            KeyScope::Titles(vec![0]),
            "a plain rip is Titles([main])"
        );
        assert_eq!(
            rip_scope(d, &[1], RipOutput::DecryptedImage),
            KeyScope::WholeDisc
        );
        assert_eq!(rip_scope(d, &[1], RipOutput::RawImage), KeyScope::None);
    }

    /// EK6 (J10): a key-service URL whose host may not resolve yet keeps the online source
    /// (the build does no lookup; resolve retries it, J13); a permanently rejected one is
    /// still dropped. No test here depends on the machine's resolver.
    #[test]
    fn transient_url_keeps_online_source() {
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("https://keys.freemkv-ku-e1.test/keys".into()),
            key_auth: None,
            online_only: false,
        };
        assert!(key_url_rejection(&p).is_none(), "passes without a lookup");
        let labels: Vec<&str> = key_source_factory(&p)().iter().map(|s| s.label()).collect();
        assert_eq!(labels, ["keydb", "online"], "the online source is kept");
        let invalid = KeyParams {
            key_url: Some("https://240.0.0.1:8443/keys".into()),
            ..p
        };
        let labels: Vec<&str> = key_source_factory(&invalid)()
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(labels, ["keydb"], "a permanent rejection still drops it");
    }

    /// A key_url the static check rejects is dropped from the sources, and
    /// `key_url_rejection` says so (typed, no DNS); one that passes is kept.
    #[test]
    fn a_dropped_key_url_says_why() {
        let url = |u: &str| KeyParams {
            key_url: Some(u.into()),
            ..Default::default()
        };
        for bad in ["http://keys.example.test/k", "https://240.0.0.1/k"] {
            let rejected = key_url_rejection(&url(bad)).expect(bad);
            assert!(!rejected.is_temporary(), "{bad}");
            assert!(key_sources(&url(bad)).is_empty(), "{bad}");
        }
        let ok = url("https://keys.example.test/k");
        assert!(key_url_rejection(&ok).is_none());
        assert_eq!(key_sources(&ok).len(), 1);
        assert!(key_url_rejection(&KeyParams::default()).is_none());
    }

    /// Stop rule (stall-based only; Stop can interrupt every wait): a factory build has no
    /// Halt, so it does no DNS lookup. keysources' static check drops a URL that is wrong
    /// without one; the host lookup runs at the first query.
    #[test]
    fn a_factory_build_does_no_dns_lookup() {
        // A host name is kept without resolving it; the lookup happens at the first query.
        let resolves_to_loopback = KeyParams {
            key_url: Some("https://localhost/keys".into()),
            ..Default::default()
        };
        let labels: Vec<&str> = key_source_factory(&resolves_to_loopback)()
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(labels, ["online"], "no lookup at build time");
        let blocked = KeyParams {
            key_url: Some("https://240.0.0.1/keys".into()),
            ..Default::default()
        };
        assert!(
            key_source_factory(&blocked)().is_empty(),
            "a literal non-public address"
        );
    }

    /// KU §2.3 via the engine's one front door: one resolve over the scope, the set keys it,
    /// and the factory is released (LK7): nothing can ask a source after it returns.
    #[test]
    fn resolve_for_rip_resolves_once_and_keeps_no_source() {
        let fx = bd_image(&[Some(K1), Some(K2)], 2);
        let calls = Calls::default();
        let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
        let set = resolve_for_rip(
            &fx.disc,
            &mut fx.source(),
            KeyScope::Titles(vec![0, 1]),
            &f,
            None,
            None,
        )
        .unwrap();
        assert_eq!(calls.len(), 2, "one request per key group");
        assert!(set.covers(&KeyScope::Titles(vec![0, 1])));
        assert_eq!(set.status().proven, 2);
        assert_eq!(std::sync::Arc::strong_count(&f), 1, "no factory retained");
        assert!(matches!(key_status(&fx.disc, &set), DecryptStatus::Ready));
    }

    /// KU §2.5 raw copy: `KeyScope::None` makes no key-source call at all.
    #[test]
    fn resolve_for_rip_raw_scope_asks_nothing() {
        let fx = bd_image(&[Some(K1)], 1);
        let calls = Calls::default();
        let f = factory(&[(Answer::Keydb, &[K1])], &calls);
        let set =
            resolve_for_rip(&fx.disc, &mut fx.source(), KeyScope::None, &f, None, None).unwrap();
        assert_eq!(calls.len(), 0);
        assert!(set.status().proven == 0);
    }

    /// 1.8.0: a loose clip outside any disc folder has nowhere to look keys up: no set, no
    /// source asked (an encrypted clip then refuses E7022 in `input()`).
    #[test]
    fn a_loose_clip_with_no_disc_folder_asks_nothing() {
        let dir = std::env::temp_dir().join(format!("fe-loose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("00001.m2ts");
        std::fs::write(&clip, b"").unwrap();
        let calls = Calls::default();
        let f = factory(&[(Answer::Keydb, &[K1])], &calls);
        let (set, _) = resolve_loose_clip(&clip, &f, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(set.unwrap().is_none());
        assert_eq!(calls.len(), 0);
    }

    /// 1.8.0: a clip in an encrypted disc folder (which `dir://` refuses, E9063) gets the
    /// folder's keys; a decrypted folder that kept `AACS/` asks nothing.
    #[test]
    fn a_loose_clip_gets_its_encrypted_disc_folder_keys() {
        let dir = std::env::temp_dir().join(format!("fe-loose-enc-{}", std::process::id()));
        let enc = bd_image(&[Some(K1)], 1).write_folder(&dir.join("enc"));
        let clear = bd_image(&[None], 1).write_folder(&dir.join("clear"));
        let calls = Calls::default();
        let f = factory(&[(Answer::Keydb, &[K1])], &calls);
        let (clear_set, _) = resolve_loose_clip(&clear, &f, None);
        let asked_for_clear = calls.len();
        let (set, _) = resolve_loose_clip(&enc, &f, None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(clear_set.unwrap().is_none());
        assert_eq!(asked_for_clear, 0);
        assert!(set.unwrap().is_some_and(|s| s.is_aacs()));
        assert!(calls.len() > 0);
    }

    /// KU §2.3 step 13: Stop before the resolve builds no set and asks nothing.
    #[test]
    fn resolve_for_rip_honours_halt() {
        let fx = bd_image(&[Some(K1)], 1);
        let calls = Calls::default();
        let f = factory(&[(Answer::Online, &[K1])], &calls);
        let halt = libfreemkv::Halt::new();
        halt.cancel();
        let r = resolve_for_rip(
            &fx.disc,
            &mut fx.source(),
            KeyScope::Titles(vec![0]),
            &f,
            None,
            Some(&halt),
        );
        assert!(matches!(r, Err(libfreemkv::Error::Halted)), "{r:?}");
        assert_eq!(calls.len(), 0);
    }

    // A source that reports each chunk it moves on the op's progress (§2.7, T29).
    struct Trickle;
    impl libfreemkv::KeySource for Trickle {
        fn get_unit_keys(
            &self,
            ctx: &dyn libfreemkv::keysource::ResolveCtx,
        ) -> libfreemkv::Result<Vec<libfreemkv::aacs::UnitKey>> {
            if let Some(p) = ctx.progress() {
                p.bump();
            }
            Ok(Vec::new())
        }
    }

    /// Stop design v5 §4.3, "The open token": its `Progress` is threaded "into the up-front
    /// resolution (`ResolveCtx::halt()` and `ResolveCtx::progress()`)". Per spec.
    #[test]
    fn resolve_for_rip_observed_hands_the_progress_to_each_source() {
        let fx = bd_image(&[Some(K1)], 1);
        let f: libfreemkv::KeySourceFactory =
            std::sync::Arc::new(|| vec![Box::new(Trickle) as Box<dyn libfreemkv::KeySource>]);
        let progress = libfreemkv::halt::Liveness::new();
        let (_, _) = resolve_for_rip_observed(
            &fx.disc,
            &mut fx.source(),
            KeyScope::Titles(vec![0]),
            &f,
            None,
            None,
            &progress,
        );
        assert!(progress.get() > 0, "no source saw the open's progress");
    }

    /// A seed set (e.g. from `info` / the GUI open) joins the pool first: a second resolve
    /// over what it already proves asks no online source (KU §2.3 step 11).
    #[test]
    fn resolve_for_rip_seed_is_not_asked_again() {
        let fx = bd_image(&[Some(K1), Some(K2)], 2);
        let calls = Calls::default();
        let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
        let scope = KeyScope::Titles(vec![0, 1]);
        let seed =
            resolve_for_rip(&fx.disc, &mut fx.source(), scope.clone(), &f, None, None).unwrap();
        let before = calls.len();
        let set =
            resolve_for_rip(&fx.disc, &mut fx.source(), scope, &f, Some(&seed), None).unwrap();
        assert_eq!(calls.len(), before, "the seed's keys open every piece");
        assert_eq!(set.status().proven, 2);
    }

    /// `key_status` (KU §12.2 `Ready | Missing | ForensicPending`) reads the set, never the
    /// disc-banked keys; a clear disc is `NotEncrypted`.
    #[test]
    fn key_status_reads_the_set() {
        let fx = bd_image(&[Some(K1)], 1);
        assert!(matches!(
            key_status(&fx.disc, &KeyRing::none()),
            DecryptStatus::AacsKeysMissing(_)
        ));
        let mut clear = bd_image(&[None], 1).disc;
        clear.aacs = None;
        clear.encrypted = false;
        assert!(matches!(
            key_status(&clear, &KeyRing::none()),
            DecryptStatus::NotEncrypted
        ));
    }
}
