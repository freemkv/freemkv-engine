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
