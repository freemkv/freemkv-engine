# `MapfileDisown`

Revokes a `Mapfile`'s right to write its path — the owner's way of saying
"whatever you still hold is no longer the record of that file".

Obtainable ONLY from a live `Mapfile` (`Mapfile::disown_handle`), so it
cannot be conjured for a pipeline that owns no mapfile.

## Why it exists

A consumer thread that is wedged inside a write on a hung mount is
ABANDONED (detached, not killed) by `super::finish_bounded` and the pass
returns. The caller is then free to resume the rip against the same
mapfile path. When the abandoned thread's write finally returns it still
owns a `Mapfile` — a snapshot from before the abandonment — and both
`record()`'s interval flush and `Mapfile`'s `Drop` flush would then
rewrite the WHOLE file from that stale snapshot, silently reverting
progress the resumed pass has already persisted (and racing the live
writer over the shared `<path>.tmp` staging name). Disowning is what
makes the abandoned writer harmless.
