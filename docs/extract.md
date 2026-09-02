# `extract_tree` — design notes

## Why this exists

Every front-end that unpacks a disc's decrypted UDF file tree (the CLI's
`dir://` destination, the desktop GUI's "decrypted folder" output) does
the exact same two things around the library call: bridge its own
cooperative-cancel signal into the `Halt` token `Disc::extract_tree`
polls at file/batch boundaries, and hand back the per-file + aggregate
result for the shell to render. That bridging (a watcher thread mirroring
`Sink::should_cancel()` into a fresh `Halt`, joined before returning even
on an unwind) was duplicated between `freemkv/src/pipe.rs` and
`freemkv/src/engine.rs`; this module is the one copy.

Mirrors [`crate::mux::mux_title`]'s should_cancel → Halt bridge.

Nothing here prints, formats a locale string, or picks an exit code —
those are presentation and stay in the shell. The CLI renders
`dir.complete` / `dir.lossy` / `dir.file_lossy` from the returned
[`libfreemkv::ExtractResult`] and turns a lossy run into a non-zero exit;
the GUI renders through its own `Sink::log`. Both keep their own
`--force` / writability gate — `extract_tree` only forwards the `force`
flag into `ExtractOptions` exactly as `Disc::extract_tree` expects it.

## Cancellation semantics

`sink`'s [`Sink::should_cancel`] is polled by a scoped watcher thread that
cancels a fresh [`libfreemkv::Halt`] the moment it returns true, so a long
extraction stops at the next file boundary (the in-flight file is left
`.partial`, never a half-written file that looks complete) — the watcher
is joined (via `thread::scope`) before this returns, on every path
including a panic unwind.
