//! KU-E1 mapfile rules (KU §4.1, §4.4, §4.5; J6): no key bytes and no raw VID on disk,
//! legacy lines become fingerprints, and the identity check. The VID derives the keys
//! (KS-16: "Kvu = AES-G(Km, IDv)"), so it is secret-class and only its hash is kept.

use super::*;
use crate::test_fixtures::{Answer, Calls, K1, K2, VID, bd_image, resolve};
use libfreemkv::keys::{AcquireOptions, KeyRing, KeyScope};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn scratch(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(format!("{tag}.mapfile"));
    (dir, p)
}

/// The engine's fingerprints are exactly the set's (KU §4.1, J18): `SHA-256(
/// "freemkv-vid-fp-v1" ‖ VID)` and `SHA-256("freemkv-key-fp-v1" ‖ key)[..8]`.
#[test]
fn fingerprints_match_the_key_set() {
    let fx = bd_image(&[Some(K1)], 1);
    let f = crate::test_fixtures::factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let set = KeyRing::acquire_for_disc(
        &fx.disc,
        &mut fx.source(),
        KeyScope::Titles(vec![0]),
        &f,
        AcquireOptions {
            vid: Some(VID),
            ..Default::default()
        },
        &libfreemkv::Ctx::default(),
    )
    .unwrap()
    .keys;
    assert_eq!(set.vid_fingerprint(), Some(vid_fingerprint(&VID)));
    assert_eq!(set.proven_key_fingerprints(), vec![key_fingerprint(&K1)]);
}

/// EK1 (KU §4.1, J6): "From KU-E1 the engine writes neither `# freemkv-uk:` nor a raw
/// `# freemkv-vid:` line": only `# freemkv-disc:` and `# freemkv-vidfp:`, from a disc
/// whose scan banked a key and whose VID is in hand.
#[test]
fn mapfile_never_writes_key_or_raw_vid_lines() {
    let mut fx = bd_image(&[Some(K1)], 1);
    let aacs = fx.disc.aacs.as_mut().unwrap();
    aacs.volume_id = VID;
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    let opts = crate::SweepOptions::default();
    crate::sweep(&fx.disc, &mut fx.source(), &iso, &opts).unwrap();
    let text = std::fs::read_to_string(mapfile_path_for(&iso)).unwrap();
    for banned in ["freemkv-uk", "freemkv-vid:", &hex(&K1), &hex(&VID)] {
        assert!(!text.contains(banned), "{banned} on disk:\n{text}");
    }
    let disc_hash = libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash)
        .to_ascii_lowercase();
    assert!(
        text.contains(&format!("# freemkv-disc: {disc_hash}\n")),
        "{text}"
    );
    let fp = hex(&vid_fingerprint(&VID));
    assert!(text.contains(&format!("# freemkv-vidfp: {fp}\n")), "{text}");
}

/// EK2 (KU §4.1, §4.5): legacy `# freemkv-uk:` / `# freemkv-vid:` lines load as
/// fingerprints through `parse_legacy_key_lines`; a load alone writes nothing, and the
/// first write scrubs them: no 32-hex key and no raw VID left in the file.
#[test]
fn legacy_lines_become_fingerprints_on_first_write() {
    let (_d, p) = scratch("legacy");
    let legacy = format!(
        "# Rescue Logfile. Created by freemkv 1.7\n\
             # freemkv-uk: 1:{}\n\
             # freemkv-uk: 2:{}\n\
             # freemkv-uk: 16777216:{}\n\
             # freemkv-uk: not-a-line\n\
             # freemkv-vid: {}\n\
             0x0  ?  1  0\n\
             0x0  0x1000    ?\n",
        hex(&K1),
        hex(&K2),
        hex(&[0x77; 16]),
        hex(&VID)
    );
    std::fs::write(&p, &legacy).unwrap();
    let mut mf = Mapfile::load(&p).expect("a malformed uk line never fails the load");
    assert_eq!(
        mf.legacy_key_fingerprints(),
        [key_fingerprint(&K1), key_fingerprint(&K2)],
        "base keys only (CPS id < 2^24); the malformed line is dropped"
    );
    assert_eq!(mf.vid_fingerprint(), Some(vid_fingerprint(&VID)));
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        legacy,
        "load wrote nothing"
    );

    mf.record(0, 2048, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    for banned in [
        "freemkv-uk",
        "freemkv-vid:",
        &hex(&K1),
        &hex(&K2),
        &hex(&VID),
    ] {
        assert!(
            !text.contains(banned),
            "{banned} survived the scrub:\n{text}"
        );
    }
    for k in [K1, K2] {
        let line = format!("# freemkv-legacy-keyfp: {}\n", hex(&key_fingerprint(&k)));
        assert!(text.contains(&line), "{text}");
    }
    let again = Mapfile::load(&p).unwrap();
    assert_eq!(
        again.legacy_key_fingerprints(),
        mf.legacy_key_fingerprints()
    );
    assert_eq!(again.vid_fingerprint(), Some(vid_fingerprint(&VID)));
}

/// KU-X1, KU §4.4 rule 3: legacy key fingerprints are "checked **only if** the set proved
/// at least one base key". With no set nothing is proven: disc-banked keys never stand in.
/// Per spec; do not change without a spec citation proving otherwise.
#[test]
fn identity_without_a_set_reads_no_banked_keys() {
    let (_d, p) = scratch("no_set");
    let banked = disc_with(HASH_A, [0; 16]);
    assert!(DiscIdentity::of(&banked, None).proven.is_empty());
    let map = map_with(&p, None, None, &[K2]);
    assert!(check_mapfile_identity(&map, &banked, None).is_ok());
}

/// The new identity lines round-trip; a malformed one fails the load (dropping it would
/// turn "names a disc" into "names none" and reopen the cross-disc resume splice).
#[test]
fn identity_lines_round_trip_and_a_malformed_one_is_refused() {
    let (_d, p) = scratch("identity");
    let mut mf = Mapfile::create(&p, 4096, "test").unwrap();
    mf.set_disc_hash("0xAABBCCDDEEFF00112233445566778899AABBCCDD");
    mf.set_vid_fingerprint(vid_fingerprint(&VID));
    mf.record(0, 2048, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    let back = Mapfile::load(&p).unwrap();
    assert_eq!(
        back.disc_hash(),
        Some("aabbccddeeff00112233445566778899aabbccdd")
    );
    assert_eq!(back.vid_fingerprint(), Some(vid_fingerprint(&VID)));
    assert_eq!(back.entries(), mf.entries());
    let text = std::fs::read_to_string(&p).unwrap();
    for (prefix, kind) in [("freemkv-disc: ", "disc"), ("freemkv-vidfp: ", "vidfp")] {
        let at = text.find(prefix).unwrap() + prefix.len();
        let mut bad = text.clone();
        bad.replace_range(at..at + 2, "zz");
        std::fs::write(&p, &bad).unwrap();
        let err = Mapfile::load(&p).map(|_| ()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{kind}");
    }
}

fn disc_with(hash: &str, vid: [u8; 16]) -> libfreemkv::Disc {
    let mut d = bd_image(&[Some(K1)], 1).disc;
    let a = d.aacs.as_mut().unwrap();
    a.disc_hash = hash.to_string();
    a.volume_id = vid;
    d
}

fn map_with(p: &Path, hash: Option<&str>, vid: Option<[u8; 16]>, keys: &[[u8; 16]]) -> Mapfile {
    let mut mf = Mapfile::create(p, 2048, "test").unwrap();
    if let Some(h) = hash {
        mf.set_disc_hash(h);
    }
    if let Some(v) = vid {
        mf.set_vid_fingerprint(vid_fingerprint(&v));
    }
    mf.legacy_keyfps = keys.iter().map(key_fingerprint).collect();
    mf
}

const HASH_A: &str = "0x1111111111111111111111111111111111111111";
const HASH_B: &str = "0x2222222222222222222222222222222222222222";

/// EK3 (KU §4.4): rule 1 disc hash, rule 2 `vidfp`, both only when both sides are known.
#[test]
fn identity_rules() {
    let (_d, p) = scratch("rules");
    let other_vid = [0x33; 16];
    let ok = |mf: &Mapfile, d: &libfreemkv::Disc| check_mapfile_identity(mf, d, None).is_ok();
    // Rule 1.
    assert!(ok(
        &map_with(&p, Some(HASH_A), None, &[]),
        &disc_with(HASH_A, [0; 16])
    ));
    assert!(!ok(
        &map_with(&p, Some(HASH_A), None, &[]),
        &disc_with(HASH_B, [0; 16])
    ));
    assert!(ok(
        &map_with(&p, None, None, &[]),
        &disc_with(HASH_B, [0; 16])
    ));
    // Rule 2: the disc's own VID, else the set's.
    assert!(ok(
        &map_with(&p, None, Some(VID), &[]),
        &disc_with(HASH_A, VID)
    ));
    assert!(!ok(
        &map_with(&p, None, Some(VID), &[]),
        &disc_with(HASH_A, other_vid)
    ));
    assert!(
        ok(
            &map_with(&p, None, Some(VID), &[]),
            &disc_with(HASH_A, [0; 16])
        ),
        "no VID in hand: cannot compare, not a mismatch"
    );
    // Neither side carries anything (a clear disc, a ddrescue import).
    let mut clear = disc_with(HASH_A, [0; 16]);
    clear.aacs = None;
    assert!(ok(&map_with(&p, None, None, &[]), &clear));
}

/// Rule 2's fallback: a disc whose scan has no VID is identified by the key set's VID,
/// both when stamped and when checked; the disc's own VID still wins when it has one.
#[test]
fn identity_falls_back_to_the_key_sets_vid() {
    let (_d, p) = scratch("set_vid");
    let fx = bd_image(&[Some(K1)], 1);
    let f = crate::test_fixtures::factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let opts = AcquireOptions {
        vid: Some(VID),
        ..Default::default()
    };
    let set = KeyRing::acquire_for_disc(
        &fx.disc,
        &mut fx.source(),
        KeyScope::Titles(vec![0]),
        &f,
        opts,
        &libfreemkv::Ctx::default(),
    )
    .unwrap()
    .keys;
    let other_vid = [0x33; 16];
    let no_vid = disc_with(HASH_A, [0; 16]);
    let check = |map_vid: [u8; 16], d: &libfreemkv::Disc| {
        check_mapfile_identity(&map_with(&p, None, Some(map_vid), &[]), d, Some(&set)).is_ok()
    };
    assert!(check(VID, &no_vid));
    assert!(!check(other_vid, &no_vid), "the set's VID is compared");
    assert!(
        check(other_vid, &disc_with(HASH_A, other_vid)),
        "the disc's own VID wins"
    );

    let mut mf = map_with(&p, None, None, &[]);
    stamp_identity(&mut mf, &no_vid, Some(&set));
    assert_eq!(mf.vid_fingerprint(), Some(vid_fingerprint(&VID)));
    stamp_identity(&mut mf, &disc_with(HASH_A, other_vid), Some(&set));
    assert_eq!(mf.vid_fingerprint(), Some(vid_fingerprint(&other_vid)));
}

/// EK3 rule 3 (KU §4.4, coord 4): legacy key fingerprints are checked ONLY when the set
/// proved a base key; a set that proved none (every piece Lazy or Clear) cannot check
/// them, and that is not a mismatch.
#[test]
fn identity_rules_legacy_fingerprints_need_a_proven_key() {
    let (_d, p) = scratch("rule3");
    let fx = bd_image(&[Some(K1)], 1);
    let calls = Calls::default();
    let proved_k1 = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &calls,
    )
    .unwrap();
    let clear = bd_image(&[None], 1);
    let proved_none = resolve(
        &clear,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[])],
        &calls,
    )
    .unwrap();
    assert!(proved_none.proven_key_fingerprints().is_empty());
    let d = &fx.disc;
    let check = |keys: &[[u8; 16]], set: Option<&KeyRing>| {
        check_mapfile_identity(&map_with(&p, None, None, keys), d, set).is_ok()
    };
    assert!(
        check(&[K1], Some(&proved_k1)),
        "a stored fingerprint matches"
    );
    assert!(check(&[K2, K1], Some(&proved_k1)));
    assert!(!check(&[K2], Some(&proved_k1)), "none matches a proven key");
    assert!(
        check(&[K2], Some(&proved_none)),
        "no proven key: cannot check"
    );
    assert!(check(&[K2], Some(&KeyRing::none())));
    assert!(
        check(&[], Some(&proved_k1)),
        "no stored fingerprint: nothing to check"
    );
}
