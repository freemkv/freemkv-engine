use super::*;

// The image guard is ONE definition used by copy, sweep and patch — these
// pin the two questions each used to ask via disagreeing hand-written
// comparisons (copy: equality, sweep: "shorter than", patch: nothing).
#[test]
fn image_state_answers_intact_and_short_separately() {
    let exact = ImageState {
        len: 4096,
        want: 4096,
    };
    assert!(exact.is_intact());
    assert!(!exact.is_short(), "the right length is not short");

    let short = ImageState {
        len: 2048,
        want: 4096,
    };
    assert!(!short.is_intact());
    assert!(short.is_short(), "this is the case that invents good data");

    // A LONGER file is not the image this mapfile describes either, so it
    // is not intact — but it is not the dangerous case, so not short.
    let long = ImageState {
        len: 8192,
        want: 4096,
    };
    assert!(
        !long.is_intact(),
        "a longer file is not this mapfile's image"
    );
    assert!(!long.is_short());
}

/// A missing file reports length 0 rather than erroring: absent is exactly
/// as inconsistent with a mapfile claiming progress as empty is, and both
/// self-heal the same way.
#[test]
fn image_state_treats_a_missing_file_as_zero_length() {
    let dir = std::env::temp_dir().join(format!("fmkv-imgstate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let absent = dir.join("not-here.iso");
    let _ = std::fs::remove_file(&absent);

    let st = image_state(&absent, 4096).expect("a missing image is not an error");
    assert_eq!(st.len, 0);
    assert!(st.is_short(), "absent must read as short, not as intact");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Measured against the real file, not a guess.
#[test]
fn image_state_measures_the_file_on_disk() {
    let dir = std::env::temp_dir().join(format!("fmkv-imgstate-len-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("short.iso");
    std::fs::write(&f, vec![0u8; 2048]).unwrap();

    assert!(image_state(&f, 4096).unwrap().is_short());
    assert!(image_state(&f, 2048).unwrap().is_intact());
    let _ = std::fs::remove_dir_all(&dir);
}
