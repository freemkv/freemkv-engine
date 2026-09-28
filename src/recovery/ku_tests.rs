//! KU-E1 recovery tests (KU §3.2, §3.5, §2.4): the passes read through the rip's one
//! up-front key set, the gates accept it and nothing else (KU-X1: never legacy disc-banked
//! keys), and the on-arrival proof's side reads never reach the damage classifier.

use super::read_error::CLASSIFIED;
use crate::test_fixtures::{Answer, Calls, Damage, Drive, Fx, K1, bd_image, factory, resolve};
use crate::{CopyOptions, Job, PatchOptions, SweepOptions};
use libfreemkv::keys::{KeyScope, ResolvedKeySet};
use libfreemkv::test_util::CountingSource;
use std::sync::Arc;

// Two clips of one key, K1: clip 0 proves it; clip 1 is left Lazy when every probe of it
// faults at resolve time (KU §2.3 step 9.5), to be proven on arrival (§2.4).
fn lazy_fixture() -> (Fx, Drive, ResolvedKeySet) {
    let fx = bd_image(&[Some(K1), Some(K1)], 2);
    let drive = Drive::new(&fx.img.image);
    let (s, n) = fx.clip(1);
    drive.set(Damage::Range(s, s + n));
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let set = ResolvedKeySet::resolve(
        &fx.disc,
        &mut drive.clone(),
        KeyScope::WholeDisc,
        &f,
        Default::default(),
    )
    .unwrap()
    .keys;
    assert_eq!(set.lazy(), &[(s, s + n)], "fixture: clip 1 is Lazy");
    (fx, drive, set)
}

// The decrypted image, CPI masked (a decrypted unit's CPI is cleared, KS-5).
fn masked(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    v.chunks_mut(192).for_each(|p| p[0] &= 0x3F);
    v
}

fn assert_image_is_plain(fx: &Fx, iso: &std::path::Path, clip: usize) {
    let (s, n) = fx.clip(clip);
    let (at, len) = (s as usize * 2048, n as usize * 2048);
    let got = std::fs::read(iso).unwrap();
    assert_eq!(
        masked(&got[at..at + len]),
        masked(&fx.img.plain[at..at + len]),
        "clip {clip} decrypted"
    );
}

fn sweep_opts<'a>(keys: Option<ResolvedKeySet>) -> SweepOptions<'a> {
    SweepOptions {
        decrypt: true,
        batch_sectors: Some(1),
        skip_on_error: true,
        keys,
        ..Default::default()
    }
}

/// EK11 (KU4-6, from LK15(g)): a decrypting copy over a Lazy piece whose side reads all
/// fault. Those failures are "not an error of the requested read … never reach the
/// engine's read_error.rs classifier": 0 classifier calls, and every unit is decrypted.
#[test]
fn side_read_failures_never_reach_the_damage_classifier() {
    let (fx, drive, set) = lazy_fixture();
    let (s, n) = fx.clip(1);
    drive.set(Damage::LongReads(s, s + n));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    CLASSIFIED.with(|c| c.set(0));
    let opts = sweep_opts(Some(set.clone()));
    let r = super::sweep(&fx.disc, &mut drive.clone(), &iso, &opts);
    let r = r.expect("a readable unit is never a read error");
    assert!(r.complete, "nothing withheld");
    assert_eq!(
        set.proof_cache().len(),
        1,
        "proven on arrival despite dead side reads"
    );
    assert_eq!(CLASSIFIED.with(|c| c.get()), 0, "no damage classification");
    assert_image_is_plain(&fx, &iso, 1);
}

/// EK10 (KU §2.4, KS-1 "encryption is applied to every Aligned Unit in the file", KS-7):
/// pass 1 reads the first unit of a Lazy piece whose every neighbour is dead → decrypted
/// on a provisional one-unit proof, never `NonTrimmed`; pass 2 recovers the rest and
/// confirms the proof from the set's `ProofCache`.
#[test]
fn decrypting_copy_patch_pass_proves_lazy_piece() {
    let (fx, drive, set) = lazy_fixture();
    let (s, n) = fx.clip(1);
    let u = s;
    drive.set(Damage::RangeExcept(s, s + n, u, u + 3));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    let opts = sweep_opts(Some(set.clone()));
    super::sweep(&fx.disc, &mut drive.clone(), &iso, &opts).unwrap();
    let map = super::mapfile::Mapfile::load(&crate::mapfile_path_for(&iso)).unwrap();
    let bad = map.ranges_with(&crate::bad_sector_statuses());
    let unit = (u as u64 * 2048, 3 * 2048);
    assert!(
        bad.iter()
            .all(|&(p, l)| p + l <= unit.0 || unit.0 + unit.1 <= p),
        "the readable unit is not NonTrimmed: {bad:?}"
    );
    assert!(!bad.is_empty(), "its dead neighbours are");
    assert_eq!(set.proof_cache().len(), 1, "a provisional proof, in memory");

    drive.set(Damage::None);
    let popts = PatchOptions {
        decrypt: true,
        keys: Some(set.clone()),
        ..PatchOptions::for_patch_pass(true, None, None)
    };
    let out = super::patch(&fx.disc, &mut drive.clone(), &iso, &popts).unwrap();
    assert_eq!(out.bytes_pending + out.bytes_unreadable, 0);
    assert_eq!(
        set.proof_cache().len(),
        1,
        "confirmed in place, not re-proven"
    );
    assert_image_is_plain(&fx, &iso, 1);
}

/// EK4 (KU §2.1 invariant 4): the passes share the one set and never ask a source; the
/// resolve's probe reads happen once (no pass reads a stream sector twice).
#[test]
fn passes_share_one_set_and_never_ask() {
    let fx = bd_image(&[Some(K1), Some(K1)], 2);
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1])], &calls);
    let set = ResolvedKeySet::resolve(
        &fx.disc,
        &mut fx.source(),
        KeyScope::WholeDisc,
        &f,
        Default::default(),
    )
    .unwrap()
    .keys;
    let asked = calls.len();
    assert!(asked >= 1);
    let src = CountingSource::new(fx.source());
    let log = src.log();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    let opts = CopyOptions {
        decrypt: true,
        multipass: true,
        keys: Some(set),
        ..Default::default()
    };
    let mut src = src;
    let r = super::copy(&fx.disc, &mut src, &iso, &opts).unwrap();
    assert!(r.complete);
    assert_eq!(calls.len(), asked, "no request after the resolve");
    assert_eq!(Arc::strong_count(&f), 1, "nothing kept the factory");
    for clip in 0..2 {
        let (s, n) = fx.clip(clip);
        let hits: usize = log
            .reads()
            .iter()
            .map(|&(l, c)| (l..l + c as u32).filter(|x| (s..s + n).contains(x)).count())
            .sum();
        assert_eq!(hits, n as usize, "clip {clip}: every sector read once");
        assert_image_is_plain(&fx, &iso, clip);
    }
}

/// EK7 (KU §3.5): a decrypting copy of an AACS disc with neither a set nor a banked key
/// refuses E7022 before any output.
#[test]
fn copy_refuses_decrypting_aacs_without_a_set() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    let opts = CopyOptions {
        decrypt: true,
        keys: None,
        ..Default::default()
    };
    let err = super::copy(&fx.disc, &mut fx.source(), &iso, &opts).unwrap_err();
    assert_eq!(err.code(), libfreemkv::error::E_NO_DISC_KEY, "{err}");
    assert!(!iso.exists(), "refused before any output");
}

// Every engine decrypt gate over `disc`: copy, sweep, patch, preflight, recover_to_iso and
// a single-pass multipass_rip, each decrypting, each with `keys` (via `Job.keys`).
fn every_gate_passes(fx: &Fx, disc: &libfreemkv::Disc, keys: Option<ResolvedKeySet>) {
    let dir = tempfile::tempdir().unwrap();
    let iso = |n: &str| dir.path().join(n);
    let copy = CopyOptions {
        decrypt: true,
        keys: keys.clone(),
        ..Default::default()
    };
    super::copy(disc, &mut fx.source(), &iso("copy.iso"), &copy).expect("copy");
    let sweep = SweepOptions {
        decrypt: true,
        keys: keys.clone(),
        ..Default::default()
    };
    super::sweep(disc, &mut fx.source(), &iso("sweep.iso"), &sweep).expect("sweep");
    let patch = PatchOptions {
        keys: keys.clone(),
        ..PatchOptions::for_patch_pass(true, None, None)
    };
    super::patch(disc, &mut fx.source(), &iso("sweep.iso"), &patch).expect("patch");
    let mut job = Job::new("iso://x.iso", "mkv://x.mkv");
    if let Some(set) = keys {
        job = job.with_keys(set);
    }
    assert!(crate::preflight(disc, &job).is_ready(), "preflight");
    let sink = crate::NoopSink;
    crate::recover_to_iso(disc, &mut fx.source(), &iso("run.iso"), &job, &sink)
        .expect("recover_to_iso");
    let single = crate::MultipassOpts {
        max_passes: 0,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    crate::multipass_rip(disc, &mut fx.source(), &iso("mp.iso"), &job, &single, &sink)
        .expect("single-pass multipass_rip");
    assert_image_is_plain(fx, &iso("run.iso"), 0);
}

/// EK8 (KU §3.5, N-KU8): a keyed rip passes every engine gate with the set alone (no
/// disc-banked key).
#[test]
fn keyed_rip_passes_every_engine_gate() {
    let fx = bd_image(&[Some(K1)], 1);
    let set = resolve(
        &fx,
        KeyScope::WholeDisc,
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    every_gate_passes(&fx, &fx.disc, Some(set));
}

/// EK8 after KU-X1 (KU §8.2: "Remove the legacy gate fallback"): legacy disc-banked keys
/// with no set pass no engine gate; each refuses E7022 before any output.
/// Per spec; do not change without a spec citation proving otherwise.
#[test]
fn legacy_banked_keys_pass_no_engine_gate() {
    let mut fx = bd_image(&[Some(K1)], 1);
    fx.disc.aacs.as_mut().unwrap().unit_keys = vec![(1, K1)];
    let disc = &fx.disc;
    let dir = tempfile::tempdir().unwrap();
    let iso = |n: &str| dir.path().join(n);
    let no_key = |r: crate::Result<()>, gate: &str| {
        let e = r.expect_err(gate);
        assert_eq!(e.code(), libfreemkv::error::E_NO_DISC_KEY, "{gate}: {e}");
    };
    let copy = CopyOptions {
        decrypt: true,
        ..Default::default()
    };
    no_key(
        super::copy(disc, &mut fx.source(), &iso("c.iso"), &copy).map(drop),
        "copy",
    );
    let sweep = SweepOptions {
        decrypt: true,
        ..Default::default()
    };
    no_key(
        super::sweep(disc, &mut fx.source(), &iso("s.iso"), &sweep).map(drop),
        "sweep",
    );
    let patch = PatchOptions {
        decrypt: true,
        ..Default::default()
    };
    no_key(
        super::patch(disc, &mut fx.source(), &iso("s.iso"), &patch).map(drop),
        "patch",
    );
    let job = Job::new("iso://x.iso", "mkv://x.mkv");
    let pf = crate::preflight(disc, &job);
    assert!(
        pf.reasons().iter().any(|r| r.key == "encrypted-no-key"),
        "{pf:?}"
    );
    let sink = crate::NoopSink;
    let run = crate::recover_to_iso(disc, &mut fx.source(), &iso("r.iso"), &job, &sink);
    no_key(run.map(drop), "recover_to_iso");
    let single = crate::MultipassOpts {
        max_passes: 0,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let mp = crate::multipass_rip(disc, &mut fx.source(), &iso("m.iso"), &job, &single, &sink);
    no_key(mp.map(drop), "multipass_rip");
    for n in ["c.iso", "s.iso", "r.iso", "m.iso"] {
        assert!(!iso(n).exists(), "{n}: refused before any output");
    }
}
