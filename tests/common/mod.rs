//! Helpers shared by the source-scanning guard tests.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir`, recursively. A directory that cannot be read panics: a
/// guard that silently scans nothing passes vacuously.
pub fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let p = e
            .unwrap_or_else(|e| panic!("entry in {}: {e}", dir.display()))
            .path();
        if p.is_dir() {
            out.extend(rs_files(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out.sort();
    out
}
