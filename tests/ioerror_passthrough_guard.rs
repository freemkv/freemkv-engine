//! Typed errors must survive the `io::Error` -> `libfreemkv::Error` conversion.
//!
//! libfreemkv carries typed errors (`MapfileInvalid`, `SyncTimeout`, `Halted`, ...) inside
//! `io::Error`; its `From<io::Error> for Error` downcasts them back and wraps only genuine
//! OS errors as `IoError`. Re-wrapping a variable by hand, `IoError { source: e }`, skips
//! that downcast and turns every typed refusal into a generic I/O (transport) failure.
//!
//! Every Rust file under `src/` (`#[cfg(test)]` included) is scanned for the pass-through
//! shape `IoError\s*\{\s*source\s*(:\s*[a-z_]\w*\s*)?,?\s*\}` (field shorthand included).
//! Use `?` or `Error::from(e)` instead.
//! A fresh `io::Error` construction (`source: std::io::Error::new(..)`) and a `{ .. }`
//! pattern are not flagged.

use std::path::{Path, PathBuf};

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `s` with leading whitespace removed (`\s*`).
fn ws(s: &str) -> &str {
    s.trim_start()
}

/// Whether `rest` (text right after `IoError`) matches `\s*\{\s*source\s*(:\s*[a-z_]\w*\s*)?,?\s*\}`.
fn is_pass_through(rest: &str) -> bool {
    let Some(r) = ws(rest).strip_prefix('{') else {
        return false;
    };
    let Some(r) = ws(r).strip_prefix("source") else {
        return false;
    };
    let r = ws(r);
    if closes(r) {
        return true;
    }
    let Some(r) = r.strip_prefix(':') else {
        return false;
    };
    let r = ws(r);
    if !r.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        return false;
    }
    closes(ws(r.trim_start_matches(is_word)))
}

/// Whether `r` is `,?\s*\}`: the end of the literal, trailing comma allowed.
fn closes(r: &str) -> bool {
    ws(r.strip_prefix(',').unwrap_or(r)).starts_with('}')
}

/// 1-based line numbers of every pass-through match in `text`.
fn pass_through_lines(text: &str) -> Vec<usize> {
    text.match_indices("IoError")
        .filter(|(i, m)| is_pass_through(&text[i + m.len()..]))
        .map(|(i, _)| text[..i].matches('\n').count() + 1)
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_matcher_flags_only_the_pass_through_shape() {
    assert_eq!(pass_through_lines("Error::IoError { source: e }"), [1]);
    assert_eq!(
        pass_through_lines("x\nIoError {\n  source :\n  err_1\n}"),
        [2]
    );
    assert_eq!(pass_through_lines("IoError{source:_e}"), [1]);
    assert!(pass_through_lines("Error::IoError { .. }").is_empty());
    assert!(pass_through_lines("IoError { source: std::io::Error::other(\"x\") }").is_empty());
    assert!(pass_through_lines("IoError { source: io::Error::from(k) }").is_empty());
    assert_eq!(
        pass_through_lines(".map_err(|source| Error::IoError { source })"),
        [1]
    );
    assert_eq!(pass_through_lines("IoError {\n    source: err,\n}"), [1]);
    assert_eq!(pass_through_lines("IoError {\n    source,\n}"), [1]);
    assert!(pass_through_lines("IoError { sources }").is_empty());
    assert!(pass_through_lines("IoError { source: std::io::Error::new(k, \"x\") }").is_empty());
}

#[test]
fn no_io_error_is_rewrapped_by_hand_under_src() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "no sources scanned");
    let hits: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let text = std::fs::read_to_string(f).unwrap();
            let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
            pass_through_lines(&text)
                .into_iter()
                .map(move |n| format!("{rel}:{n}"))
        })
        .collect();
    assert!(
        hits.is_empty(),
        "`IoError {{ source: <var> }}` bypasses libfreemkv's typed downcast; use `?` or \
         `Error::from(e)`:\n{}",
        hits.join("\n")
    );
}

// A damaged sidecar mapfile is a typed refusal, not a generic I/O failure.
#[test]
fn ensure_whole_image_reports_a_corrupt_mapfile_as_mapfile_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("disc.iso");
    std::fs::write(&iso, vec![0u8; 2048]).unwrap();
    std::fs::write(
        freemkv_engine::mapfile_path_for(&iso),
        "# Rescue Logfile\n0x00000000 +\n0x00000000 0x00000800 Z\n",
    )
    .unwrap();
    let r = freemkv_engine::ensure_whole_image(&iso);
    assert!(
        matches!(r, Err(libfreemkv::Error::MapfileInvalid { .. })),
        "a corrupt mapfile must surface as MapfileInvalid, got {r:?}"
    );
}
