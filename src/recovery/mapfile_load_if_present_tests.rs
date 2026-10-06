use super::*;

#[test]
fn absent_is_none_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.map");
    assert!(load_if_present(&p).unwrap().is_none());
}

/// The distinction the three call sites kept re-deriving: a mapfile that
/// EXISTS but cannot be parsed is an error, never an indistinguishable
/// "nothing here".
#[test]
fn corrupt_is_an_error_not_none() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bad.map");
    std::fs::write(&p, b"# Rescue Logfile. Created by test\n0x00 0xZZZZ +\n").unwrap();
    match load_if_present(&p) {
        Err(e) => assert_ne!(
            e.kind(),
            io::ErrorKind::NotFound,
            "corruption must not masquerade as absence"
        ),
        Ok(_) => panic!("a corrupt mapfile must not load, nor read as absent"),
    }
}
