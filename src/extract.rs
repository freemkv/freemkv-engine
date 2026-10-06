//! `extract_tree` — shared orchestration around `Disc::extract_tree`.
//!
//! Bridges a shell's cooperative-cancel `Sink` into the `Halt` token `Disc::extract_tree`
//! polls, and hands back the per-file + aggregate [`libfreemkv::ExtractResult`] for the caller
//! to render. Used by both the CLI's `dir://` destination and the desktop GUI's "decrypted
//! folder" output. Nothing here prints, formats a locale string, or picks an exit code — those
//! stay in the shell.

use crate::sink::Sink;
use std::path::Path;

/// Extract `disc`'s decrypted UDF file tree to `dest`, with the legacy disc-banked keys:
/// [`extract_tree_with`] with no key set (until KU-F1 moves both shells to the set).
pub fn extract_tree(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    dest: &Path,
    force: bool,
    sink: &dyn Sink,
) -> crate::Result<libfreemkv::ExtractResult> {
    extract_tree_with(disc, reader, dest, force, None, sink)
}

/// Extract `disc`'s decrypted UDF file tree to `dest`, reading every AACS file through
/// `keys` (the rip's up-front set, scope `WholeDisc`, KU §3.1): proven files by its map,
/// the rest proven on arrival; a readable unit no held key opens stops the run (E7032).
///
/// `reader` is consumed for content reads (see [`libfreemkv::Disc::extract_tree`]). `force`
/// mirrors the CLI's `--force`: without it, a non-empty `dest` is refused. `sink`'s
/// [`Sink::should_cancel`] is polled by a watcher thread that cancels a fresh
/// [`libfreemkv::Halt`]. Cancel can stop mid-file: the interrupted file stays
/// `<name>.partial` (never renamed to its final name) and the result reports `halted`.
pub fn extract_tree_with(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    dest: &Path,
    force: bool,
    keys: Option<&libfreemkv::keys::KeyRing>,
    sink: &dyn Sink,
) -> crate::Result<libfreemkv::ExtractResult> {
    // One should_cancel → halt bridge for the whole engine (see `with_cancel_watcher`), so
    // cancelling even a small extraction is deterministic. Both shells poll the result.
    crate::run::with_cancel_watcher(sink, |halt| {
        let opts = libfreemkv::ExtractOptions { force, keys };
        let ctx = crate::run::ctx(&libfreemkv::Halt::from_arc(halt.clone()));
        disc.extract_tree(reader, dest, &opts, &ctx)
    })
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
