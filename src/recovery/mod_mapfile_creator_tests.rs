use super::MAPFILE_CREATOR;

// The provenance header must name THIS crate — a wrong literal produces a
// plausible-looking lie ("libfreemkv v1.6.4"), not a compile error.
#[test]
fn the_mapfile_header_names_the_crate_that_writes_it() {
    assert!(
        MAPFILE_CREATOR.starts_with("freemkv-engine v"),
        "mapfile provenance header is {MAPFILE_CREATOR:?}"
    );
    assert!(
        !MAPFILE_CREATOR.contains("libfreemkv"),
        "recovery has lived in this crate since 1.6.0: {MAPFILE_CREATOR:?}"
    );
    // A version is actually interpolated — not the literal `env!` call, and
    // not an empty tail.
    let version = MAPFILE_CREATOR
        .strip_prefix("freemkv-engine v")
        .expect("prefix asserted above");
    assert!(
        version.starts_with(|c: char| c.is_ascii_digit()),
        "version tail is {version:?}"
    );
}
