use super::{Error, classify_pass_abort, is_damage_candidate};

#[test]
fn only_read_faults_and_halt_enter_damage_handling() {
    let io = || Error::IoError {
        source: std::io::Error::other("EIO"),
    };
    assert!(is_damage_candidate(&io()));
    assert!(is_damage_candidate(&Error::Halted));
    assert!(!is_damage_candidate(&Error::DecryptFailed));
    assert!(!is_damage_candidate(&Error::SourceTerminated));
    assert!(!is_damage_candidate(&Error::WholeDiscKeyMissing));
    let e7022 = Error::NoDiscKey {
        disc_hash: String::new(),
    };
    assert!(!is_damage_candidate(&e7022));
}

#[test]
fn non_read_errors_keep_their_own_code() {
    for e in [Error::DecryptFailed, Error::Halted, Error::SourceTerminated] {
        let code = e.code();
        assert_eq!(classify_pass_abort(e, 9).code(), code);
    }
}
