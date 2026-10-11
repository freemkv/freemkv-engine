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

fn with(f: libfreemkv::KeySourceFactory) -> RunWith<'static> {
    RunWith {
        sources: Some(f),
        ..RunWith::default()
    }
}

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
    let report = run_with(&p, with(f), &crate::NoopSink).expect("the image copies");
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
    run_with(&p, with(f), &crate::NoopSink).expect("a raw copy");
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
    let Report::Tree { extract } = run_with(&p, with(f), &crate::NoopSink).expect("extracts")
    else {
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
    run_with(&p, with(f), &crate::NoopSink).expect_err("no key opens the disc");
    assert!(!dst.exists(), "refused before any output");
}

// iso:// → mkv:// through `run_with`: the title's keys are acquired over it and the one
// title is muxed into the plan's destination.
#[test]
fn a_title_plan_muxes_its_one_title() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = fx.write(dir.path(), "src.iso");
    let out = dir.path().join("t1.mkv");
    let p = Plan {
        titles: Selection::Titles(vec![1]),
        ..plan(
            &format!("iso://{}", src.display()),
            &format!("mkv://{}", out.display()),
        )
    };
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    let Report::Title { outcome } = run_with(&p, with(f), &crate::NoopSink).expect("muxes") else {
        panic!("a title report")
    };
    assert!(outcome.completed, "{outcome:?}");
    assert!(std::fs::metadata(&out).unwrap().len() > 0);
}

// A title run muxes one title: a plan naming several is refused before any output.
#[test]
fn a_title_plan_naming_several_titles_is_refused() {
    let err = run_with(
        &Plan {
            titles: Selection::Titles(vec![0, 1]),
            ..plan("iso:///nowhere.iso", "mkv:///o/a.mkv")
        },
        with(std::sync::Arc::new(Vec::new)),
        &crate::NoopSink,
    )
    .expect_err("several titles");
    assert!(matches!(err, libfreemkv::Error::StreamUrlInvalid { .. }));
}

// Ciphertext belongs in `iso://`: a raw folder tree is refused, never written decrypted.
#[test]
fn a_raw_tree_plan_is_refused() {
    let err = run_with(
        &Plan {
            raw: true,
            ..plan("iso:///nowhere.iso", "dir:///o/tree")
        },
        with(std::sync::Arc::new(Vec::new)),
        &crate::NoopSink,
    )
    .expect_err("a raw tree");
    assert!(matches!(err, libfreemkv::Error::DirRawRejected), "{err:?}");
}

// An image's title is scanned for the plan's stream filter: an unknown language is its
// own refusal (E9083), not an invalid source URL.
#[test]
fn a_title_plan_resolves_its_stream_filter_against_the_image() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = fx.write(dir.path(), "src.iso");
    let out = dir.path().join("t1.mkv");
    let p = Plan {
        titles: Selection::Titles(vec![1]),
        streams: StreamChoice {
            audio: crate::StreamFilter::Langs(vec!["Klingonish".into()]),
            ..StreamChoice::default()
        },
        ..plan(
            &format!("iso://{}", src.display()),
            &format!("mkv://{}", out.display()),
        )
    };
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    let err = run_with(&p, with(f), &crate::NoopSink).expect_err("an unknown language");
    assert!(
        matches!(err, libfreemkv::Error::StreamLanguageUnknown { .. }),
        "{err:?}"
    );
    assert!(!out.exists(), "nothing is written");
}

// A raw title of an image reads the ciphertext: no key source is asked.
#[test]
fn a_raw_title_plan_asks_no_key_source() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = fx.write(dir.path(), "src.iso");
    let out = dir.path().join("raw.mkv");
    let p = Plan {
        raw: true,
        titles: Selection::Titles(vec![1]),
        ..plan(
            &format!("iso://{}", src.display()),
            &format!("mkv://{}", out.display()),
        )
    };
    let calls = Calls::default();
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &calls);
    let _ = run_with(&p, with(f), &crate::NoopSink);
    assert_eq!(calls.len(), 0, "no key call for a raw title");
}

// The key-service token never reaches a `Debug` rendering of a plan.
#[test]
fn a_plan_debug_never_prints_the_key_token() {
    let p = Plan {
        keys: KeyParamsData {
            key_auth: Some("s3cret-token".into()),
            ..KeyParamsData::default()
        },
        ..Plan::default()
    };
    let shown = format!("{p:?}");
    assert!(!shown.contains("s3cret-token"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
}

// A title run whose selection needs review is refused with its coded reason (E9086), not
// as an invalid destination URL.
#[test]
fn a_title_run_needing_review_is_its_coded_refusal() {
    let disc = crate::test_fixtures::disc_of(crate::test_fixtures::branch_titles());
    let p = Plan {
        titles: Selection::Episodes,
        ..plan("disc://", "mkv:///o/a.mkv")
    };
    let err = title_index(&p, Some(&disc)).expect_err("needs review");
    assert!(
        matches!(
            err,
            libfreemkv::Error::SelectionReviewRequired {
                reason: "missing-episode-roster"
            }
        ),
        "{err:?}"
    );
    let movie = Plan {
        titles: Selection::MainMovie,
        ..p
    };
    assert_eq!(title_index(&movie, Some(&disc)).ok(), Some(0));
}

// An image or folder title run resolves its selection against the SCANNED disc, never as
// title 0 before the scan: a selection that needs review is refused (E9086) before output.
#[test]
fn an_image_title_run_resolves_its_selection_after_the_scan() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "src.iso");
    let folder = dir.path().join("tree");
    fx.write_folder(&folder);
    let out = dir.path().join("t.mkv");
    for source in [
        format!("iso://{}", iso.display()),
        format!("dir://{}", folder.display()),
    ] {
        let p = Plan {
            titles: Selection::Episodes,
            ..plan(&source, &format!("mkv://{}", out.display()))
        };
        let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
        let err = run_with(&p, with(f), &crate::NoopSink).expect_err("needs review");
        assert!(
            matches!(
                err,
                libfreemkv::Error::SelectionReviewRequired {
                    reason: "missing-episode-roster"
                }
            ),
            "{source}: {err:?}"
        );
        assert!(!out.exists(), "nothing is written");
    }
    // A proved main title still runs from the scan.
    let p = Plan {
        titles: Selection::MainMovie,
        ..plan(
            &format!("iso://{}", iso.display()),
            &format!("mkv://{}", out.display()),
        )
    };
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    let Report::Title { outcome } = run_with(&p, with(f), &crate::NoopSink).expect("muxes") else {
        panic!("a title report")
    };
    assert!(outcome.completed, "{outcome:?}");
}

// How a title run over an image is keyed: from the key sources, or raw / with the keys
// held, each with a stream filter (so the run opens the image for its title), or raw
// keeping every stream (so nothing is opened but a non-explicit selection's scan).
#[derive(Clone, Copy, Debug)]
enum Keying {
    Resolve,
    RawFilter,
    HeldFilter,
    RawAll,
}

const KEYINGS: [Keying; 4] = [
    Keying::Resolve,
    Keying::RawFilter,
    Keying::HeldFilter,
    Keying::RawAll,
];

// A Sink that notes whether a run reached its mux (the engine logs `mux: …` as it starts).
#[derive(Default)]
struct MuxSeen(std::sync::atomic::AtomicBool);

impl crate::Sink for MuxSeen {
    fn log(&self, _: crate::Level, msg: &str) {
        if msg.starts_with("mux: ") {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// A title run of `titles` from `source` to `out`, keyed by `keying`, under `halt`; and
// whether it reached the mux, past every check of the open.
fn image_title_run(
    fx: &crate::test_fixtures::Fx,
    source: &str,
    out: &std::path::Path,
    titles: Selection,
    keying: Keying,
    halt: Option<libfreemkv::Halt>,
) -> (crate::Result<Report>, bool) {
    let filtered = crate::StreamChoice {
        subtitles: crate::SubtitleFilter {
            forced: crate::StreamFilter::None,
            ..Default::default()
        },
        ..Default::default()
    };
    // Ciphertext muxes to a transport stream, never demuxed into Matroska.
    let raw = matches!(keying, Keying::RawFilter | Keying::RawAll);
    let scheme = if raw { "m2ts" } else { "mkv" };
    let p = Plan {
        titles,
        raw,
        streams: match keying {
            Keying::RawAll => Default::default(),
            _ => filtered,
        },
        ..plan(source, &format!("{scheme}://{}", out.display()))
    };
    let keys = matches!(keying, Keying::HeldFilter).then(|| held_keys(fx, source));
    let with = RunWith {
        keys,
        halt,
        ..with(factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default()))
    };
    let sink = MuxSeen::default();
    let r = run_with(&p, with, &sink);
    (r, sink.0.load(std::sync::atomic::Ordering::Relaxed))
}

// What a run over a sound image comes to: a completed mux, but a raw one, whose ciphertext
// yields no PES (MuxEmpty, after every open check).
fn sound(keying: Keying) -> &'static str {
    match keying {
        Keying::RawFilter | Keying::RawAll => "MuxEmpty",
        _ => "ok",
    }
}

// The keys a front end holds for `source`: acquired over its own scan's titles (a folder is
// its own disc), else the fixture disc's (a swept image has no key file of its own to scan).
fn held_keys(fx: &crate::test_fixtures::Fx, source: &str) -> libfreemkv::keys::KeyRing {
    let scope = |n: usize| libfreemkv::keys::KeyScope::Titles((0..n).collect());
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    // libfreemkv's scan, not the engine's: the scan-count test counts only the run's.
    let scanned = match crate::ImageSource::from_url(source).unwrap() {
        crate::ImageSource::Iso(p) => libfreemkv::scan_iso(&p, Default::default()),
        crate::ImageSource::Dir(p) => libfreemkv::scan_dir(&p, Default::default()),
    };
    let own = scanned.and_then(|(disc, mut reader)| {
        let opts = libfreemkv::keys::AcquireOptions::default();
        let ctx = libfreemkv::Ctx::default();
        let scope = scope(disc.titles.len());
        libfreemkv::keys::KeyRing::acquire_for_disc(&disc, &mut *reader, scope, &f, opts, &ctx)
    });
    own.map(|r| r.keys).unwrap_or_else(|_| {
        crate::test_fixtures::resolve(
            fx,
            scope(2),
            &[(Answer::Keydb, &[K1, K2])],
            &Calls::default(),
        )
        .expect("the fixture's keys")
    })
}

// A run's result as compared across selections: "ok" for a completed mux, else the error.
fn verdict(r: crate::Result<Report>) -> String {
    match r {
        Ok(Report::Title { outcome }) if outcome.completed => "ok".into(),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("{e:?}"),
    }
}

// `fx` as a disc folder under `root` whose only key file is `/AACS/DUPLICATE/Unit_Key_RO.inf`.
fn duplicate_only_folder(fx: &crate::test_fixtures::Fx, root: &std::path::Path) {
    fx.write_folder(root);
    std::fs::create_dir_all(root.join("AACS/DUPLICATE")).unwrap();
    std::fs::rename(
        root.join("AACS/Unit_Key_RO.inf"),
        root.join("AACS/DUPLICATE/Unit_Key_RO.inf"),
    )
    .unwrap();
}

// The UDF image of the disc folder `root`, written to `iso`.
fn iso_of_folder(root: &std::path::Path, iso: &std::path::Path) {
    use libfreemkv::SectorSource;
    let mut img = libfreemkv::DirImage::open(root).unwrap();
    let mut bytes = vec![0u8; img.capacity_sectors() as usize * 2048];
    for (lba, sector) in bytes.chunks_mut(2048).enumerate() {
        img.read_sectors(lba as u32, 1, sector, false).unwrap();
    }
    std::fs::write(iso, bytes).unwrap();
}

// A sweep's sidecar for `iso`, of `disc`: its hash, every sector read but `unread`.
fn swept_sidecar(disc: &libfreemkv::Disc, iso: &std::path::Path, unread: Option<(u32, u32)>) {
    let total = std::fs::metadata(iso).unwrap().len();
    let mut map = crate::Mapfile::create(&crate::mapfile_path_for(iso), total, "t").unwrap();
    map.record(0, total, crate::SectorStatus::Finished).unwrap();
    if let Some((lba, sectors)) = unread {
        let (pos, len) = (lba as u64 * 2048, sectors as u64 * 2048);
        map.record(pos, len, crate::SectorStatus::NonTrimmed)
            .unwrap();
    }
    map.set_disc_hash(&disc.aacs.as_ref().unwrap().disc_hash);
    map.flush().unwrap();
}

// What a keyed run (one that opens the image) of a parity image comes to.
#[derive(Clone, Copy, Debug)]
enum Expect {
    // The image's mux: [`sound`].
    Sound,
    // Its sidecar does not load (E6011).
    BadSidecar,
    // Its sidecar names another disc (E6011 disc-mismatch).
    Mismatch,
    // Past every check of the open into the mux, where the synthetic HD DVD clip does not
    // demux (MkvInvalid).
    ClipFails,
}

// The parity table's images: (name, source URL, expected), each built under `dir` from `fx`.
fn parity_images(
    fx: &crate::test_fixtures::Fx,
    dir: &std::path::Path,
) -> Vec<(&'static str, String, Expect)> {
    let iso = |name: &str| fx.write(dir, name);
    let url = |scheme: &str, p: &std::path::Path| format!("{scheme}://{}", p.display());
    let key_file = *fx.metadata.last().unwrap();
    let mut images = vec![("valid iso", url("iso", &iso("valid.iso")), Expect::Sound)];
    let folder = dir.join("valid");
    fx.write_folder(&folder);
    images.push(("valid dir", url("dir", &folder), Expect::Sound));
    let dup = dir.join("dup");
    duplicate_only_folder(fx, &dup);
    images.push(("duplicate-only dir", url("dir", &dup), Expect::Sound));
    let dup_iso = dir.join("dup.iso");
    iso_of_folder(&dup, &dup_iso);
    images.push(("duplicate-only iso", url("iso", &dup_iso), Expect::Sound));
    let garbage = iso("garbage.iso");
    std::fs::write(crate::mapfile_path_for(&garbage), "not a mapfile\n\x00\x7f").unwrap();
    images.push((
        "garbage mapfile iso",
        url("iso", &garbage),
        Expect::BadSidecar,
    ));
    let garbage_dir = dir.join("garbage");
    fx.write_folder(&garbage_dir);
    std::fs::write(crate::mapfile_path_for(&garbage_dir), "not a mapfile\n").unwrap();
    images.push((
        "garbage mapfile dir",
        url("dir", &garbage_dir),
        Expect::BadSidecar,
    ));
    let unread = iso("unread.iso");
    swept_sidecar(&fx.disc, &unread, Some(key_file));
    images.push((
        "sidecar marks key file unread",
        url("iso", &unread),
        Expect::Sound,
    ));
    let swept = iso("swept.iso");
    let mut bytes = std::fs::read(&swept).unwrap();
    bytes[key_file.0 as usize * 2048..(key_file.0 + key_file.1) as usize * 2048].fill(0);
    std::fs::write(&swept, bytes).unwrap();
    swept_sidecar(&fx.disc, &swept, Some(key_file));
    images.push((
        "swept key file, sidecar marks it unread",
        url("iso", &swept),
        Expect::Mismatch,
    ));
    let hd = dir.join("hd");
    hddvd_folder(&hd);
    let hd_iso = dir.join("hd.iso");
    iso_of_folder(&hd, &hd_iso);
    images.push((
        "hd dvd iso (VTKF key file)",
        url("iso", &hd_iso),
        Expect::ClipFails,
    ));
    let hd_map = dir.join("hd-map.iso");
    std::fs::copy(&hd_iso, &hd_map).unwrap();
    let (hd_disc, _) = crate::scan_image(&crate::ImageSource::Iso(hd_iso)).unwrap();
    swept_sidecar(&hd_disc, &hd_map, None);
    images.push((
        "hd dvd iso with its sidecar",
        url("iso", &hd_map),
        Expect::ClipFails,
    ));
    images
}

// A non-explicit selection meets exactly the checks and errors of the explicit title it
// resolves to: MainMovie and `Titles([main])` give the same result over every image and
// keying, and each row gives the result it should.
#[test]
fn an_image_title_run_selection_does_not_change_its_checks() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let main = title_index(
        &Plan {
            titles: Selection::MainMovie,
            ..Plan::default()
        },
        Some(&fx.disc),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (image, source, expect) in parity_images(&fx, dir.path()) {
        for keying in KEYINGS {
            let mut row = Vec::new();
            for titles in [Selection::MainMovie, Selection::Titles(vec![main])] {
                let out = dir.path().join("out.mkv");
                let (r, muxed) = image_title_run(&fx, &source, &out, titles, keying, None);
                row.push((verdict(r), muxed, out.exists()));
                let _ = std::fs::remove_file(&out);
            }
            assert_eq!(
                row[0], row[1],
                "{image}, {keying:?}: MainMovie vs Titles([{main}])"
            );
            // A raw run keeping every stream of an explicit title opens nothing (no sidecar
            // read), so neither selection reads it: the BD image's ciphertext mux, MuxEmpty.
            // A refusal of the open never reaches the mux.
            let want = match (expect, keying) {
                (Expect::ClipFails, _) => ("MkvInvalid", true),
                (_, Keying::RawAll) => ("MuxEmpty", true),
                (Expect::Sound, _) => (sound(keying), true),
                (Expect::BadSidecar, _) => ("MapfileInvalid { kind: \"hex\" }", false),
                (Expect::Mismatch, _) => ("MapfileInvalid { kind: \"disc-mismatch\" }", false),
            };
            let got = (row[0].0.as_str(), row[0].1);
            assert_eq!(got, want, "{image}, {keying:?}: (verdict, reached the mux)");
        }
    }
}

// A title run on an image scans it once for a non-explicit selection, whatever it is keyed
// by; an explicit title, or MainMovie keeping every audio (title 0), with neither keys nor a
// title's streams needed (raw, every stream) scans nothing.
#[test]
fn an_image_title_run_scans_the_image_once() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "src.iso");
    let folder = dir.path().join("tree");
    fx.write_folder(&folder);
    let out = dir.path().join("o.mkv");
    let scans = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = scans.clone();
    crate::image::scan_hook::set(move |at| {
        if at == crate::image::scan_hook::At::Start {
            seen.set(seen.get() + 1)
        }
    });
    for source in [
        format!("iso://{}", iso.display()),
        format!("dir://{}", folder.display()),
    ] {
        for keying in KEYINGS {
            for titles in [
                Selection::MainMovie,
                Selection::Longest,
                Selection::Titles(vec![0]),
            ] {
                scans.set(0);
                let (r, _) = image_title_run(&fx, &source, &out, titles.clone(), keying, None);
                assert_eq!(verdict(r), sound(keying), "{source} {titles:?} {keying:?}");
                let want = match (&titles, keying) {
                    (Selection::Titles(_) | Selection::MainMovie, Keying::RawAll) => 0,
                    _ => 1,
                };
                assert_eq!(scans.get(), want, "{source} {titles:?} {keying:?}");
                let _ = std::fs::remove_file(&out);
            }
        }
    }
    crate::image::scan_hook::clear();
}

// A Stop raised before the image scan, as it starts (past the engine's own check, so only
// libfreemkv's scan can stop it) or as it completes ends a non-explicit title run `Halted`
// with no output: the scan itself honours it, and no work follows it.
#[test]
fn a_stop_around_the_image_scan_halts_the_title_run() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum When {
        BeforeScan,
        AtScanStart,
        AtScanEnd,
    }
    use crate::image::scan_hook::{self, At};
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "src.iso");
    let folder = dir.path().join("tree");
    fx.write_folder(&folder);
    let out = dir.path().join("o.mkv");
    for source in [
        format!("iso://{}", iso.display()),
        format!("dir://{}", folder.display()),
    ] {
        for keying in [Keying::Resolve, Keying::RawAll] {
            // A raw MainMovie run keeping every stream is title 0 with no scan to stop.
            let selections = match keying {
                Keying::RawAll => vec![Selection::Longest, Selection::Episodes],
                _ => vec![
                    Selection::MainMovie,
                    Selection::Longest,
                    Selection::Episodes,
                ],
            };
            for titles in selections {
                for when in [When::BeforeScan, When::AtScanStart, When::AtScanEnd] {
                    let halt = libfreemkv::Halt::new();
                    if when == When::BeforeScan {
                        halt.cancel();
                    }
                    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
                    let (log, stop) = (seen.clone(), halt.clone());
                    scan_hook::set(move |at| {
                        log.borrow_mut().push(at);
                        match (when, at) {
                            (When::AtScanStart, At::Start) | (When::AtScanEnd, At::Done) => {
                                stop.cancel()
                            }
                            _ => {}
                        }
                    });
                    let r =
                        image_title_run(&fx, &source, &out, titles.clone(), keying, Some(halt)).0;
                    scan_hook::clear();
                    let case = format!("{source} {titles:?} {keying:?} {when:?}");
                    assert!(
                        matches!(r, Err(libfreemkv::Error::Halted)),
                        "{case}: {}",
                        verdict(r)
                    );
                    let want: &[At] = match when {
                        When::BeforeScan => &[],
                        When::AtScanStart => &[At::Start],
                        When::AtScanEnd => &[At::Start, At::Done],
                    };
                    assert_eq!(*seen.borrow(), want, "{case}");
                    assert!(!out.exists(), "{case}: nothing is written");
                }
            }
        }
    }
}

// An HD DVD-shaped disc folder under `root`: `/HVDVD_TS/FEATURE.EVO` (a clear head pack
// with H.264 video and DD+ audio, then scrambled packs) and an `/AACS` directory whose
// only unit-key file is `VTKF000.AACS`, so the disc hash comes from it.
fn hddvd_folder(root: &std::path::Path) {
    fn pes(stream_id: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, stream_id];
        v.extend_from_slice(&((3 + payload.len()) as u16).to_be_bytes());
        v.extend_from_slice(&[flags, 0, 0]);
        v.extend_from_slice(payload);
        v
    }
    let pack_header = [0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8];
    let mut head = pack_header.to_vec();
    let video = [
        0, 0, 1, 0x67, 0x42, 0, 0x1E, 0xAB, 0xCD, 0, 0, 1, 0x65, 0x88, 0,
    ];
    head.extend(pes(0xE2, 0x80, &video));
    for sub in [0xC0u8, 0xC1] {
        head.extend(pes(0xBD, 0x80, &[sub, 1, 0, 0, 0x0B, 0x77, 0xDE, 0xAD]));
    }
    head.resize(2048, 0xFF);
    let mut evo = head;
    for _ in 0..63 {
        let mut pack = pack_header.to_vec();
        pack.extend(pes(0xE2, 0x80 | 0x30, &[0x5A; 64]));
        pack.resize(2048, 0xFF);
        evo.extend(pack);
    }
    let write = |rel: &str, bytes: &[u8]| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    write("HVDVD_TS/FEATURE.EVO", &evo);
    let vtkf = libfreemkv::test_util::unit_key_ro(
        libfreemkv::aacs::mkb::AacsVersion::V10,
        &[[0xEE; 16]],
        &[1],
    );
    write("AACS/VTKF000.AACS", &vtkf);
    write("AACS/MKBROM.AACS", &[0u8; 2048]);
    // libfreemkv lays out a BD or DVD tree only; the scan reads HVDVD_TS before VIDEO_TS.
    std::fs::create_dir_all(root.join("VIDEO_TS")).unwrap();
}

// A title run over a source whose scan found no titles is the source's refusal (E_NO_STREAMS,
// as libfreemkv's mux of an empty title list), never an invalid destination URL.
#[test]
fn a_title_run_over_a_disc_with_no_titles_is_no_streams() {
    let empty = crate::test_fixtures::disc_of(Vec::new());
    for titles in [
        Selection::MainMovie,
        Selection::Longest,
        Selection::Episodes,
    ] {
        let p = Plan {
            titles: titles.clone(),
            ..plan("iso:///in.iso", "mkv:///o/a.mkv")
        };
        let err = title_index(&p, Some(&empty)).expect_err("no titles");
        assert!(
            matches!(err, libfreemkv::Error::NoStreams),
            "{titles:?}: {err:?}"
        );
    }
}

// A Sink recording each `Event::Keys`: whether it carried a ring.
#[derive(Default)]
struct KeysSeen(std::sync::Mutex<Vec<bool>>);

impl crate::Sink for KeysSeen {
    fn event(&self, e: &crate::Event<'_>) {
        if let crate::Event::Keys { ring, .. } = e {
            self.0.lock().unwrap().push(ring.is_some());
        }
    }
}

// A title run that acquires its keys reports the walk as one `Event::Keys`: with the ring on
// success, without it on a refusal (of the resolve, or of the open before it) and for a loose
// clip's folder lookup. A run that acquires nothing (raw, or keys held) reports none.
#[test]
fn a_title_run_reports_its_key_acquisition() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "src.iso");
    let source = format!("iso://{}", iso.display());
    let out = dir.path().join("t.mkv");
    let run = |p: &Plan, with: RunWith<'_>| {
        let sink = KeysSeen::default();
        let r = run_with(p, with, &sink);
        let _ = std::fs::remove_file(&out);
        (r.is_ok(), sink.0.into_inner().unwrap())
    };
    let title = |source: &str, raw: bool| Plan {
        titles: Selection::Titles(vec![1]),
        raw,
        ..plan(source, &format!("mkv://{}", out.display()))
    };
    let keyed = || factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    assert_eq!(
        run(&title(&source, false), with(keyed())),
        (true, vec![true])
    );
    let none = factory(&[(Answer::Keydb, &[])], &Calls::default());
    assert_eq!(
        run(&title(&source, false), with(none)),
        (false, vec![false])
    );
    // A sidecar that does not load refuses the open before any key is asked.
    let garbage = fx.write(dir.path(), "garbage.iso");
    std::fs::write(crate::mapfile_path_for(&garbage), "not a mapfile\n").unwrap();
    let bad = format!("iso://{}", garbage.display());
    assert_eq!(
        run(&title(&bad, false), with(keyed())),
        (false, vec![false])
    );
    // A loose clip of an encrypted disc folder: its folder's keys.
    let clip = fx.write_folder(&dir.path().join("folder"));
    let m2ts = format!("m2ts://{}", clip.display());
    let (_, seen) = run(&title(&m2ts, false), with(keyed()));
    assert_eq!(seen, vec![true]);
    // Raw, or with the keys held: nothing acquired, nothing reported.
    assert_eq!(
        run(&title(&source, true), with(keyed())).1,
        Vec::<bool>::new()
    );
    let held = RunWith {
        keys: Some(held_keys(&fx, &source)),
        ..with(keyed())
    };
    assert_eq!(run(&title(&source, false), held), (true, Vec::new()));
}

// A MainMovie title run prefers the cut carrying the plan's audio: on a disc whose two
// titles are cuts of one programme, German audio picks the German cut, every audio title 0.
#[test]
fn a_main_movie_title_run_picks_the_audio_preferred_cut() {
    let disc = crate::test_fixtures::disc_of(crate::test_fixtures::branch_titles());
    let movie = |audio| Plan {
        titles: Selection::MainMovie,
        streams: StreamChoice {
            audio,
            ..StreamChoice::default()
        },
        ..plan("iso:///in.iso", "mkv:///o/a.mkv")
    };
    let de = crate::StreamFilter::Langs(vec!["de".into()]);
    assert_eq!(title_index(&movie(de), Some(&disc)).ok(), Some(1));
    let all = crate::StreamFilter::All;
    assert_eq!(title_index(&movie(all), Some(&disc)).ok(), Some(0));
}
