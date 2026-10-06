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

// An OS "not found": a folder that is not a disc, never a read failure.
fn is_not_found(e: &libfreemkv::Error) -> bool {
    matches!(*e, libfreemkv::Error::IoError { source: ref s } if s.kind() == std::io::ErrorKind::NotFound)
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
        Err(e @ libfreemkv::Error::IoError { .. }) if !is_not_found(&e) => {
            return (Err(e), ResolutionTrace::new());
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
#[path = "keys_tests.rs"]
mod tests;
