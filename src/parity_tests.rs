//! Parity goldens for the engine paths: image mux, remux, folder extract, whole-disc copy,
//! sweep and patch over a damaged drive, multipass, and the pure gate and loop policies.
//! Pinned as output hashes, counters and verdicts (`libfreemkv::test_util::Golden`); cells
//! are `parity_engine_*`. Re-bless: `FREEMKV_BLESS_GOLDENS=1 cargo test parity_engine`.

use crate::image::{ImageSource, OpenImageOptions, open_image_with};
use crate::remux::MuxPlan;
use crate::test_fixtures::{Answer, Calls, Damage, Drive, Fx, K1, K2, bd_image, factory};
use crate::{
    CopyOptions, Job, MultipassOpts, NoopSink, PatchOptions, RemuxJob, Selection, StreamChoice,
    SweepOptions,
};
use libfreemkv::keys::{KeyRing, KeyScope};
use libfreemkv::test_util::Golden;
use std::path::Path;

fn golden(cell: &str) -> Golden {
    Golden::new(env!("CARGO_MANIFEST_DIR"), cell)
}

// A file's length and hash, its build stamp (version and commit) masked to a same-length run.
fn record_file(g: &mut Golden, key: &str, path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let stamp = format!("freemkv {}", libfreemkv::VERSION_LABEL).into_bytes();
    let mut i = 0;
    while i + stamp.len() <= bytes.len() {
        if bytes[i..i + stamp.len()] == stamp[..] {
            bytes[i..i + stamp.len()].fill(b'#');
            i += stamp.len();
        } else {
            i += 1;
        }
    }
    g.bytes(key, &bytes);
}

fn keys_for(fx: &Fx, pool: &[[u8; 16]], scope: KeyScope) -> libfreemkv::Result<KeyRing> {
    let f = factory(&[(Answer::Keydb, pool)], &Calls::default());
    KeyRing::acquire_for_disc(
        &fx.disc,
        &mut fx.source(),
        scope,
        &f,
        libfreemkv::keys::AcquireOptions::default(),
        &libfreemkv::Ctx::default(),
    )
    .map(|r| r.keys)
}

// Open the image under `pool` (a keydb), mux every title through `plan`, record the outcome
// and each title's file.
fn image_mux(g: &mut Golden, tag: &str, fx: &Fx, iso: &Path, pool: &[[u8; 16]], plan: MuxPlan) {
    let n = fx.disc.titles.len();
    let f = factory(&[(Answer::Keydb, pool)], &Calls::default());
    let opts = OpenImageOptions {
        scope: Some(KeyScope::Titles((0..n).collect())),
        ..OpenImageOptions::resolve(f)
    };
    let opened = match open_image_with(&ImageSource::Iso(iso.to_path_buf()), opts) {
        Ok(o) => o,
        Err(e) => {
            g.kv(
                &format!("{tag} open refused"),
                format_args!("E{}", e.code()),
            );
            return;
        }
    };
    let dir = tempfile::tempdir().unwrap();
    let dest = |idx: usize| format!("mkv://{}", dir.path().join(format!("t{idx}.mkv")).display());
    let out = crate::mux_image_titles(&opened, &plan, &dest, &NoopSink);
    g.kv(&format!("{tag} outcome"), format_args!("{out:?}"));
    for idx in &plan.titles {
        let p = dir.path().join(format!("t{idx}.mkv"));
        if p.exists() {
            record_file(g, &format!("{tag} t{idx}.mkv"), &p);
        } else {
            g.kv(&format!("{tag} t{idx}.mkv"), "absent");
        }
    }
}

fn plan(titles: &[usize], raw: bool) -> MuxPlan {
    MuxPlan {
        mux: crate::mux_options(raw),
        ..MuxPlan::new(titles.to_vec())
    }
}

#[test]
fn parity_engine_image_mux() {
    let mut g = golden("parity_engine_image_mux");
    let dir = tempfile::tempdir().unwrap();
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let iso = fx.write(dir.path(), "keyed.iso");
    image_mux(&mut g, "keyed", &fx, &iso, &[K1, K2], plan(&[0, 1], false));
    image_mux(
        &mut g,
        "keyed-raw",
        &fx,
        &iso,
        &[K1, K2],
        plan(&[0, 1], true),
    );
    let mut one = plan(&[1], false);
    one.explicit_selection = true;
    image_mux(&mut g, "keyed-title1", &fx, &iso, &[K1, K2], one);
    // Audio dropped: nothing left to mux in a title with one audio stream.
    let mut none = plan(&[0], false);
    none.streams = vec![(
        0,
        libfreemkv::StreamSelection {
            audio: libfreemkv::PidFilter::Only(vec![]),
            subtitle: libfreemkv::PidFilter::All,
        },
    )];
    image_mux(&mut g, "keyed-no-audio", &fx, &iso, &[K1, K2], none);
    image_mux(
        &mut g,
        "one-key-held",
        &fx,
        &iso,
        &[K1],
        plan(&[0, 1], false),
    );
    image_mux(&mut g, "no-key-held", &fx, &iso, &[], plan(&[0, 1], false));

    let clear = bd_image(&[None, None], 2);
    let ciso = clear.write(dir.path(), "clear.iso");
    image_mux(&mut g, "clear", &clear, &ciso, &[], plan(&[0, 1], false));

    // Seed-damaged units in clip 1: blanked and counted, the title still muxes.
    let dfx = bd_image(&[Some(K1), Some(K2)], 2);
    let diso = dfx.write(dir.path(), "damaged.iso");
    let mut bytes = std::fs::read(&diso).unwrap();
    let (s, _) = dfx.clip(1);
    for u in [2u32, 3, 5] {
        let at = (s + u * 3) as usize * 2048;
        libfreemkv::test_util::damage_unit_seed(&mut bytes[at..at + 6144]);
    }
    std::fs::write(&diso, bytes).unwrap();
    image_mux(
        &mut g,
        "damaged",
        &dfx,
        &diso,
        &[K1, K2],
        plan(&[0, 1], false),
    );
    g.check();
}

#[test]
fn parity_engine_remux_iso() {
    let mut g = golden("parity_engine_remux_iso");
    let dir = tempfile::tempdir().unwrap();
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let iso = fx.write(dir.path(), "d.iso");
    let target = dir.path().join("out.mkv");
    let mk = |title: Option<usize>, replace: bool| RemuxJob {
        iso: ImageSource::Iso(iso.clone()),
        title,
        streams: StreamChoice::default(),
        target: target.clone(),
        replace,
    };
    for (tag, job) in [
        ("main", mk(None, false)),
        ("again-refused", mk(None, false)),
        ("replace-title1", mk(Some(1), true)),
        ("bad-title", mk(Some(9), true)),
    ] {
        let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
        match crate::remux::remux_iso_sources(&job, f, &NoopSink, &libfreemkv::Halt::new()) {
            Ok(r) => {
                let o = &r.outcome;
                g.kv(
                    &format!("{tag} report"),
                    format_args!(
                        "completed={} bytes={} errors={} lost={} streams={} replaced={} tracks={} cue={:?}",
                        o.completed,
                        o.bytes_written,
                        o.errors,
                        o.lost_bytes,
                        o.streams,
                        r.replaced,
                        r.verified.tracks.len(),
                        r.verified.last_cue_secs
                    ),
                );
                record_file(&mut g, &format!("{tag} file"), &target);
            }
            Err(e) => {
                g.kv(
                    &format!("{tag} refused"),
                    format_args!("E{}", libfreemkv::error_code(&e).unwrap_or(0)),
                );
            }
        }
    }
    g.check();
}

// The decrypted folder (and the raw one, with no key set) of a keyed disc, and the key gate.
#[test]
fn parity_engine_extract_tree() {
    let mut g = golden("parity_engine_extract_tree");
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let set = keys_for(&fx, &[K1, K2], KeyScope::WholeDisc).unwrap();
    let held_one = keys_for(&fx, &[K1], KeyScope::WholeDisc);
    g.kv(
        "one-key whole-disc resolve",
        match &held_one {
            Ok(s) => format!("{:?}", s.status()),
            Err(e) => format!("refused E{}", e.code()),
        },
    );
    for (tag, keys) in [
        ("keyed", Some(&set)),
        ("no-set", None),
        ("one-key", held_one.as_ref().ok()),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tree");
        let r = crate::extract_tree_with(&fx.disc, &mut fx.source(), &dest, false, keys, &NoopSink);
        match r {
            Ok(r) => {
                g.kv(
                    &format!("{tag} result"),
                    format_args!(
                        "good={} unreadable={} complete={} halted={} files={}",
                        r.bytes_good,
                        r.bytes_unreadable,
                        r.complete,
                        r.halted,
                        r.files.len()
                    ),
                );
                for f in &r.files {
                    g.bytes(
                        &format!("{tag} {}", f.path.display()),
                        &std::fs::read(dest.join(&f.path)).unwrap(),
                    );
                }
            }
            Err(e) => {
                g.kv(&format!("{tag} refused"), format_args!("E{}", e.code()));
            }
        }
    }
    g.check();
}

fn copy_opts(decrypt: bool, keys: Option<KeyRing>) -> CopyOptions<'static> {
    CopyOptions {
        decrypt,
        multipass: false,
        progress: None,
        halt: None,
        keys,
    }
}

// Whole-disc copy to an ISO: decrypting, raw, no key set, and over a dead range.
#[test]
fn parity_engine_copy() {
    let mut g = golden("parity_engine_copy");
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let set = keys_for(&fx, &[K1, K2], KeyScope::WholeDisc).unwrap();
    let (s, _) = fx.clip(0);
    for (tag, decrypt, keys, damage) in [
        ("decrypt", true, Some(set.clone()), Damage::None),
        ("raw", false, None, Damage::None),
        ("decrypt-no-set", true, None, Damage::None),
        (
            "decrypt-dead-range",
            true,
            Some(set.clone()),
            Damage::Range(s + 6, s + 12),
        ),
        ("raw-dead-range", false, None, Damage::Range(s + 6, s + 12)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("d.iso");
        let drive = Drive::new(&fx.img.image);
        drive.set(damage);
        match crate::copy(
            &fx.disc,
            &mut drive.clone(),
            &iso,
            &copy_opts(decrypt, keys),
        ) {
            Ok(r) => {
                g.kv(&format!("{tag} result"), format_args!("{r:?}"));
                g.bytes(&format!("{tag} iso"), &std::fs::read(&iso).unwrap());
            }
            Err(e) => {
                g.kv(&format!("{tag} refused"), format_args!("E{}", e.code()));
            }
        }
    }
    g.check();
}

// A clean sweep, then a range the sweep could not read (zeroed, marked retryable in the
// mapfile) patched from a drive that now reads it: counters and the finished image. (A drive
// that keeps failing sleeps through real cooldowns, so no failing pass is run here.)
#[test]
fn parity_engine_patch() {
    let mut g = golden("parity_engine_patch");
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let set = keys_for(&fx, &[K1, K2], KeyScope::WholeDisc).unwrap();
    for (tag, decrypt, keys) in [("decrypt", true, Some(set.clone())), ("raw", false, None)] {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("d.iso");
        let drive = Drive::new(&fx.img.image);
        let sweep = SweepOptions {
            decrypt,
            batch_sectors: Some(30),
            skip_on_error: true,
            keys: keys.clone(),
            ..Default::default()
        };
        let r = crate::sweep(&fx.disc, &mut drive.clone(), &iso, &sweep).unwrap();
        g.kv(&format!("{tag} sweep"), format_args!("{r:?}"));
        let (s, _) = fx.clip(1);
        let (pos, len) = (u64::from(s + 9) * 2048, 6u64 * 2048);
        let mut bytes = std::fs::read(&iso).unwrap();
        bytes[pos as usize..(pos + len) as usize].fill(0);
        std::fs::write(&iso, bytes).unwrap();
        let mut map = crate::Mapfile::load(&crate::mapfile_path_for(&iso)).unwrap();
        map.record(pos, len, crate::SectorStatus::NonTrimmed)
            .unwrap();
        map.flush().unwrap();
        g.kv(
            &format!("{tag} damaged map"),
            format_args!("{:?}", map.stats()),
        );
        let popts = PatchOptions {
            decrypt,
            keys,
            ..PatchOptions::for_patch_pass(decrypt, None, None)
        };
        let out = crate::patch(&fx.disc, &mut drive.clone(), &iso, &popts).unwrap();
        g.kv(&format!("{tag} patch"), format_args!("{out:?}"));
        g.bytes(&format!("{tag} iso"), &std::fs::read(&iso).unwrap());
    }
    g.check();
}

// `multipass_rip`: a decrypting job is refused when it asks for passes, a raw job converges
// on a clean drive for muxed and ISO scope, and single-pass over a dead range fails like copy.
#[test]
fn parity_engine_multipass() {
    let mut g = golden("parity_engine_multipass");
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let set = keys_for(&fx, &[K1, K2], KeyScope::WholeDisc).unwrap();
    let (s, _) = fx.clip(0);
    let cases = [
        ("multi-decrypt-job", 2, false, false, Damage::None),
        ("multi-raw-mkv", 2, true, false, Damage::None),
        ("multi-raw-iso", 2, true, true, Damage::None),
        ("single-decrypt", 0, false, false, Damage::None),
        (
            "single-dead-range",
            0,
            false,
            false,
            Damage::Range(s + 9, s + 15),
        ),
    ];
    for (tag, max_passes, raw, is_iso, damage) in cases {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("d.iso");
        let drive = Drive::new(&fx.img.image);
        drive.set(damage);
        let mut job = Job::new("iso://x.iso", "mkv://out.mkv");
        job.selection = Selection::MainMovie;
        job.raw = raw;
        job.keys = Some(set.clone());
        let opts = MultipassOpts {
            max_passes,
            abort_on_lost_secs: 0,
            is_iso_output: is_iso,
        };
        match crate::multipass_rip(&fx.disc, &mut drive.clone(), &iso, &job, &opts, &NoopSink) {
            Ok(r) => {
                g.kv(
                    &format!("{tag} result"),
                    format_args!(
                        "unreadable={} pending={} good={} lost_ms={:.1} severity={:?} passes={} aborted={} halted={} wedged={} complete={}",
                        r.unreadable_bytes,
                        r.pending_bytes,
                        r.good_bytes,
                        r.main_lost_ms,
                        r.severity,
                        r.passes,
                        r.aborted_for_loss,
                        r.halted,
                        r.wedged,
                        r.complete
                    ),
                );
                g.bytes(&format!("{tag} iso"), &std::fs::read(&iso).unwrap());
            }
            Err(e) => {
                g.kv(&format!("{tag} refused"), format_args!("E{}", e.code()));
            }
        }
    }
    g.check();
}

// The pure policies the later slices move: the loss gate, damage tiers, pass plans and the
// title loop's verdicts.
#[test]
fn parity_engine_policies() {
    use crate::{TitleResult, decide_title};
    let mut g = golden("parity_engine_policies");
    for (bytes, ms, secs) in [
        (0u64, 0.0f64, 0u64),
        (1, 10.0, 0),
        (0, f64::NAN, 0),
        (5, 999.0, 1),
        (5, 1000.0, 1),
        (5, 1001.0, 1),
        (5, f64::NAN, u64::MAX),
    ] {
        g.kv(
            &format!("loss_aborts({bytes}, {ms}, {secs})"),
            crate::loss_aborts(bytes, ms, secs),
        );
    }
    for (bad, ms) in [
        (0u64, 0.0f64),
        (1, 0.0),
        (50, 0.0),
        (51, 0.0),
        (499, 999.0),
        (499, 1000.0),
        (499, 29_999.0),
        (499, 30_000.0),
        (500, 0.0),
        (7, f64::NAN),
    ] {
        g.kv(
            &format!("classify_damage({bad}, {ms})"),
            format_args!("{:?}", crate::classify_damage(bad, ms)),
        );
    }
    for n in [0u8, 1, 2, 255] {
        g.kv(
            &format!("plan_passes({n})"),
            format_args!("{:?}", crate::plan_passes(n)),
        );
    }
    for (iso, configured) in [(true, 60u64), (false, 60)] {
        g.kv(
            &format!("effective_abort_secs({iso}, {configured})"),
            crate::effective_abort_secs(iso, configured),
        );
    }
    for (bad, rec) in [(0u64, None), (10, None), (10, Some(0u64)), (10, Some(5))] {
        g.kv(
            &format!("patch_pass_decision({bad}, {rec:?})"),
            format_args!("{:?}", crate::patch_pass_decision(bad, rec)),
        );
    }
    for (halted, wedged) in [(false, false), (true, false), (false, true), (true, true)] {
        g.kv(
            &format!("pass_exit({halted}, {wedged})"),
            format_args!("{:?}", crate::pass_exit(halted, wedged)),
        );
    }
    for r in [
        TitleResult::Ok,
        TitleResult::DiscLevelNoKey,
        TitleResult::SkippableStub,
        TitleResult::Failed,
        TitleResult::Halted,
    ] {
        for (feature, multi, explicit) in [
            (true, false, false),
            (false, true, false),
            (false, true, true),
        ] {
            g.kv(
                &format!(
                    "decide_title({r:?}, feature={feature}, multi={multi}, explicit={explicit})"
                ),
                format_args!("{:?}", decide_title(&r, feature, multi, explicit)),
            );
        }
    }
    // The loop: a stub among several titles is skipped, the feature failing is fatal.
    let run = |idx: &[usize], explicit: bool, fail: &[(usize, libfreemkv::Error)]| {
        let fails: Vec<(usize, u16)> = fail.iter().map(|(i, e)| (*i, e.code())).collect();
        crate::run_titles(idx, explicit, &NoopSink, |i| {
            match fails.iter().find(|f| f.0 == i) {
                Some(&(_, code)) => Err(coded(code)),
                None => Ok(()),
            }
        })
    };
    g.kv(
        "loop all ok",
        format_args!("{:?}", run(&[0, 1, 2], false, &[])),
    );
    g.kv(
        "loop stub skipped",
        format_args!(
            "{:?}",
            run(&[0, 1, 2], false, &[(1, libfreemkv::Error::NoStreams)])
        ),
    );
    g.kv(
        "loop stub explicit",
        format_args!(
            "{:?}",
            run(&[0, 1], true, &[(1, libfreemkv::Error::NoStreams)])
        ),
    );
    g.kv(
        "loop feature fails",
        format_args!(
            "{:?}",
            run(&[0, 1], false, &[(0, libfreemkv::Error::DecryptFailed)])
        ),
    );
    g.kv(
        "loop no key",
        format_args!(
            "{:?}",
            run(
                &[0, 1],
                false,
                &[(
                    0,
                    libfreemkv::Error::NoDiscKey {
                        disc_hash: String::new()
                    }
                )]
            )
        ),
    );
    g.kv("loop empty", format_args!("{:?}", run(&[], false, &[])));
    g.check();
}

// An `io::Error` carrying the coded libfreemkv error `code`, as a mux returns it.
fn coded(code: u16) -> std::io::Error {
    use libfreemkv::Error as E;
    match code {
        c if c == E::NoStreams.code() => E::NoStreams.into(),
        c if c == E::DecryptFailed.code() => E::DecryptFailed.into(),
        _ => E::NoDiscKey {
            disc_hash: String::new(),
        }
        .into(),
    }
}
