//! AACS key-source orchestration, shared by every front-end.
//!
//! Both the CLI and the desktop UI build the SAME local-first ordered
//! [`freemkv_keysources::KeySource`] list and extract "which source won" from
//! a resolution trace — this is that logic, hoisted once. Each shell keeps its
//! own boundary-specific bits: the CLI's implicit online-only derivation and
//! default-keydb-location search chain, the GUI's `shellexpand` of the
//! configured path and its explicit online-only toggle, and (on both) English
//! presentation (trace rendering, the SSRF warning, the `"unlocked via …"`
//! string) — none of that belongs here.
//!
//! [`KeyParams`] is intentionally a thin, already-resolved shape: `keydb_path`
//! is `Some(path)` exactly when a local keydb source should be tried (the
//! shell has already applied its own fallback/search/expansion policy, or
//! decided to omit it), never a signal this module re-interprets. `online_only`
//! is carried explicitly (rather than re-derived from the other fields) because
//! the GUI's is an independent user toggle that can be true even with a
//! configured `keydb_path` — collapsing it back into "keydb_path is None"
//! would silently change GUI behavior.

/// Already-resolved key configuration, boundary-normalized by the calling
/// shell. See the module docs for what each field means and does NOT mean.
#[derive(Clone, Default)]
pub struct KeyParams {
    /// The local keydb path to consult, or `None` to skip the local source
    /// entirely. The caller has already applied whatever fallback/search
    /// chain or `shellexpand` its shell uses — this module does no further
    /// resolution of the path itself.
    pub keydb_path: Option<String>,
    /// The online key-service URL, or `None` to skip it. SSRF-validated here
    /// via [`freemkv_keysources::validate_keyserver_url`] before use; a
    /// rejected URL is silently dropped (the local source, if any, still
    /// applies) — the visible warning is the CLI's job, not this module's.
    pub key_url: Option<String>,
    /// Bearer token for the online service, if any.
    pub key_auth: Option<String>,
    /// Skip the local keydb source even when `keydb_path` is `Some` — an
    /// explicit, independent toggle (not re-derived from `keydb_path`).
    pub online_only: bool,
}

/// Build the ordered `KeySource` list, **local-first**: the keydb (unless
/// `online_only`) then the online service (unless its URL is absent or
/// SSRF-rejected). Pure / quiet — safe to call repeatedly (e.g. once per
/// on-decrypt-miss fetch); it emits no warnings.
pub fn key_sources(p: &KeyParams) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    let mut sources: Vec<Box<dyn freemkv_keysources::KeySource>> = Vec::new();

    if !p.online_only
        && let Some(path) = &p.keydb_path
    {
        sources.push(Box::new(freemkv_keysources::KeydbSource::new(path.clone())));
    }

    if let Some(url) = &p.key_url
        && freemkv_keysources::validate_keyserver_url(url).is_ok()
    {
        sources.push(Box::new(freemkv_keysources::OnlineSource::new(
            url.clone(),
            p.key_auth.clone().unwrap_or_default(),
        )));
    }

    sources
}

/// Build the [`libfreemkv::KeySourceFactory`] the library's key resolution
/// (`resolve_keys_for` / `DiscSession::resolve_keys`) calls to (re)build the
/// ordered sources from `p`.
pub fn key_source_factory(p: &KeyParams) -> libfreemkv::KeySourceFactory {
    let p = p.clone();
    std::sync::Arc::new(move || key_sources(&p))
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

/// Resolve an AACS key for a keyless-scanned `disc` from `p`'s sources,
/// reading ciphertext samples through `reader`, and return the label of the
/// source that won (or `None` if nothing resolved). No-op for an unencrypted
/// disc (no AACS inputs).
pub fn resolve_disc_keys(
    disc: &mut libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    p: &KeyParams,
) -> Option<String> {
    let factory = key_source_factory(p);
    let resolved = libfreemkv::resolve_keys_for(reader, disc, factory);
    won_source(&resolved.trace)
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
        // Metadata / loopback endpoints fail `validate_keyserver_url` and must
        // not be added as a source; the keydb (if any) still applies.
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("http://169.254.169.254/latest/meta-data".into()),
            key_auth: None,
            online_only: false,
        };
        let s = key_sources(&p);
        assert_eq!(s.len(), 1, "rejected url dropped; keydb remains");
        assert_eq!(s[0].label(), "keydb");

        let p_url_only = KeyParams {
            keydb_path: None,
            key_url: Some("https://127.0.0.1:8443/keys".into()),
            key_auth: None,
            online_only: false,
        };
        assert!(
            key_sources(&p_url_only).is_empty(),
            "loopback url-only must yield zero sources"
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
                },
                KeyStep {
                    who: "online".to_string(),
                    path: vec![KeyNode::MatchedDisc, KeyNode::FoundUnitKeys],
                    outcome: KeyOutcome::Resolved,
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
            }],
        };
        assert_eq!(won_source(&trace), None);
    }
}
