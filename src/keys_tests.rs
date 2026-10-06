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
    let set = resolve_for_rip(&fx.disc, &mut fx.source(), KeyScope::None, &f, None, None).unwrap();
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
    let seed = resolve_for_rip(&fx.disc, &mut fx.source(), scope.clone(), &f, None, None).unwrap();
    let before = calls.len();
    let set = resolve_for_rip(&fx.disc, &mut fx.source(), scope, &f, Some(&seed), None).unwrap();
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
