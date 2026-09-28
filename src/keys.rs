//! AACS key-source orchestration and the rip's up-front key set, shared by every front-end.
//!
//! Both the CLI and the desktop UI build the SAME local-first ordered
//! [`freemkv_keysources::KeySource`] list ([`key_source_factory`]) and resolve a rip's keys
//! through ONE call, [`resolve_for_rip`], before any output (keys-upfront design, KU §2.1):
//! the resulting [`ResolvedKeySet`] lives in memory only and is handed to every pass and
//! mux of the rip, which never ask a source again.
//!
//! [`KeyParams`] is a thin, already-resolved shape, never re-interpreted here.

use libfreemkv::aacs::trace::ResolutionTrace;
use libfreemkv::keys::{DecryptStatus, KeyScope, ResolveKeysOptions, ResolvedKeySet};

/// Already-resolved key configuration, boundary-normalized by the calling
/// shell. See the module docs for what each field means and does NOT mean.
#[derive(Clone, Default)]
pub struct KeyParams {
    /// The local keydb path to consult, or `None` to skip the local source
    /// entirely. The caller has already applied whatever fallback/search
    /// chain or `shellexpand` its shell uses — this module does no further
    /// resolution of the path itself.
    pub keydb_path: Option<String>,
    /// The online key-service URL, or `None` to skip it. Checked here without DNS
    /// ([`freemkv_keysources::check_keyserver_url_static`]); a URL wrong on its face is
    /// silently dropped (the local source, if any, still applies). Its host is looked up and
    /// SSRF-guarded at the first query (J10). The visible warning is the CLI's job.
    pub key_url: Option<String>,
    /// Bearer token for the online service, if any.
    pub key_auth: Option<String>,
    /// Skip the local keydb source even when `keydb_path` is `Some` — an
    /// explicit, independent toggle (not re-derived from `keydb_path`).
    pub online_only: bool,
}

/// Build the ordered `KeySource` list, **local-first**: the keydb (unless
/// `online_only`) then the online service (unless its URL is absent or
/// permanently SSRF-rejected). Quiet: it emits no warnings.
///
/// KU J10: no DNS lookup here (a factory build has no Stop). The host is looked up at the
/// first query, halt-aware; a lookup that gives no answer is retried by `resolve` up front
/// until 60 s pass with no answer (J13), one that finds a non-public address is refused.
pub fn key_sources(p: &KeyParams) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    let mut sources: Vec<Box<dyn freemkv_keysources::KeySource>> = Vec::new();

    if !p.online_only
        && let Some(path) = &p.keydb_path
    {
        sources.push(Box::new(freemkv_keysources::KeydbSource::new(path.clone())));
    }

    if let Some(url) = &p.key_url
        && freemkv_keysources::check_keyserver_url_static(url).is_ok()
    {
        sources.push(Box::new(freemkv_keysources::OnlineSource::new(
            url.clone(),
            p.key_auth.clone().unwrap_or_default(),
        )));
    }

    sources
}

/// Build the [`libfreemkv::KeySourceFactory`] a rip's [`resolve_for_rip`] (and the legacy
/// `resolve_keys_for` / `DiscSession::resolve_keys`) calls to build the ordered sources
/// from `p`, with the J10 transient-URL rule of [`key_sources`].
pub fn key_source_factory(p: &KeyParams) -> libfreemkv::KeySourceFactory {
    let p = p.clone();
    std::sync::Arc::new(move || key_sources(&p))
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
    seed: Option<&ResolvedKeySet>,
    halt: Option<&libfreemkv::Halt>,
) -> crate::Result<ResolvedKeySet> {
    resolve_for_rip_traced(disc, reader, scope, sources, seed, halt).0
}

/// [`resolve_for_rip`], also returning the per-source walk ("keydb > matched disc > online >
/// …") on success AND on a refusal, for the device log and the "why no key" answer. It holds
/// source labels, node outcomes and counts only, never a key, VID or MKB byte.
pub fn resolve_for_rip_traced(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: KeyScope,
    sources: &libfreemkv::KeySourceFactory,
    seed: Option<&ResolvedKeySet>,
    halt: Option<&libfreemkv::Halt>,
) -> (crate::Result<ResolvedKeySet>, ResolutionTrace) {
    let scope_log = format!("{scope:?}");
    let walk = std::sync::Mutex::new(ResolutionTrace::new());
    let opts = ResolveKeysOptions {
        halt,
        seed,
        vid: None,
        vid_would_help: None,
        trace: Some(&walk),
    };
    let r = ResolvedKeySet::resolve(disc, reader, scope, sources, opts).map(|r| r.keys);
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

// The qa key log line (KU §7.6): counts only, never a key or the VID.
pub(crate) fn log_status(keys: &ResolvedKeySet, scope: &str) {
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
pub fn key_status(disc: &libfreemkv::Disc, set: &ResolvedKeySet) -> DecryptStatus {
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

    // An unencrypted disc resolves nothing and READS nothing; guards against a fabricated
    // winning-source label and pins the read short-circuit.
    #[test]
    fn resolve_disc_keys_is_none_and_reads_nothing_for_an_unencrypted_disc() {
        struct NeverRead;
        impl libfreemkv::SectorSource for NeverRead {
            fn read_sectors(
                &mut self,
                _lba: u32,
                _count: u16,
                _buf: &mut [u8],
                _recovery: bool,
            ) -> libfreemkv::Result<usize> {
                panic!("an unencrypted disc must not be sampled for key material");
            }
            fn capacity_sectors(&self) -> u32 {
                1
            }
        }

        let mut disc = libfreemkv::Disc {
            volume_id: "PLAIN".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: None,
            key_auth: None,
            online_only: false,
        };
        assert_eq!(
            resolve_disc_keys(&mut disc, &mut NeverRead, &p),
            None,
            "no AACS inputs means no resolution, and therefore no winning source"
        );
    }

    /// ...and the other half: a disc the local keydb DOES hold a unit key for
    /// resolves, the key is banked onto the disc, and the label of the source
    /// that won comes back. Without this the whole verb is indistinguishable
    /// from a body that always answers `None` — which reads as "no key found",
    /// and sends the operator hunting for a keydb they already have.
    #[test]
    fn resolve_disc_keys_names_the_source_that_won_and_banks_the_key() {
        struct NeverRead;
        impl libfreemkv::SectorSource for NeverRead {
            fn read_sectors(
                &mut self,
                _lba: u32,
                _count: u16,
                _buf: &mut [u8],
                _recovery: bool,
            ) -> libfreemkv::Result<usize> {
                // No parsed titles: there is nothing to sample, so validation
                // is skipped and the stored unit key is taken as terminal.
                panic!("a title-less disc has no ciphertext to sample");
            }
            fn capacity_sectors(&self) -> u32 {
                1
            }
        }

        let disc_hash = "ab".repeat(20);
        let unit_key = "5a".repeat(16);
        let dir = tempfile::tempdir().unwrap();
        let keydb = dir.path().join("keydb.cfg");
        std::fs::write(
            &keydb,
            format!("0x{disc_hash} = TESTDISC | U | 1-0x{unit_key}\n"),
        )
        .unwrap();

        let mut disc = libfreemkv::Disc {
            volume_id: "TESTDISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: Some(
                libfreemkv::test_util::aacs_state()
                    .disc_hash(disc_hash.clone())
                    .build(),
            ),
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        let p = KeyParams {
            keydb_path: Some(keydb.to_string_lossy().into_owned()),
            key_url: None,
            key_auth: None,
            online_only: false,
        };

        assert_eq!(
            resolve_disc_keys(&mut disc, &mut NeverRead, &p),
            Some("keydb".to_string()),
            "the local keydb held the unit key; it must be named as the winner"
        );
        assert_eq!(
            disc.aacs.as_ref().unwrap().unit_keys,
            vec![(1u32, [0x5au8; 16])],
            "the winning key must be banked onto the disc, not merely reported"
        );
    }

    // ── KU-E1: the front door (KU §3.2) ─────────────────────────────────────

    use crate::test_fixtures::{Answer, Calls, K1, K2, bd_image, factory};
    use libfreemkv::keys::{DecryptStatus, KeyScope, ResolvedKeySet};

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

    /// EK6 (J10): a key-service URL whose host lookup fails TRANSIENTLY keeps the online
    /// source (resolve retries it, J13); a permanently rejected one is still dropped.
    #[test]
    fn transient_url_keeps_online_source() {
        let p = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("https://keys.freemkv-ku-e1.invalid/keys".into()),
            key_auth: None,
            online_only: false,
        };
        let url = p.key_url.as_deref().unwrap();
        let rejected = freemkv_keysources::check_keyserver_url(url).unwrap_err();
        assert!(
            rejected.is_temporary(),
            "`.invalid` never resolves: {rejected}"
        );
        let labels: Vec<&str> = key_source_factory(&p)().iter().map(|s| s.label()).collect();
        assert_eq!(labels, ["keydb", "online"], "the online source is kept");
        let loopback = KeyParams {
            key_url: Some("https://127.0.0.1:8443/keys".into()),
            ..p
        };
        let labels: Vec<&str> = key_source_factory(&loopback)()
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(labels, ["keydb"], "a permanent rejection still drops it");
    }

    /// Stop rule (stall-based only; Stop can interrupt every wait): a factory build has no
    /// Halt, so it does no DNS lookup. keysources' static check drops a URL that is wrong
    /// without one; the host lookup runs at the first query, halt-aware.
    #[test]
    fn a_factory_build_does_no_dns_lookup() {
        let src = include_str!("keys.rs").replace("\r\n", "\n");
        let start = src.find("pub fn key_sources(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        assert!(body.contains("check_keyserver_url_static("), "{body}");
        assert!(
            !body.contains("check_keyserver_url("),
            "no lookup at build time"
        );
        let blocked = KeyParams {
            key_url: Some("https://169.254.169.254/keys".into()),
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
            key_status(&fx.disc, &ResolvedKeySet::none()),
            DecryptStatus::AacsKeysMissing(_)
        ));
        let mut clear = bd_image(&[None], 1).disc;
        clear.aacs = None;
        clear.encrypted = false;
        assert!(matches!(
            key_status(&clear, &ResolvedKeySet::none()),
            DecryptStatus::NotEncrypted
        ));
    }

    /// `open_scan` (KU §3.2): the raw drive bring-up, no key call; signature pinned.
    #[test]
    fn open_scan_signature() {
        let _: fn(
            libfreemkv::DeviceTarget,
            Option<libfreemkv::DriveCredentials>,
            bool,
        ) -> Result<libfreemkv::DiscSession, libfreemkv::Error> = crate::mux::open_scan;
    }
}
