# `Mapfile::entries` invariant

`entries` is the CANONICAL maximal-run partition of `[0, total_size)`:
contiguous, gapless, and (after any `record()`) with no two adjacent
entries sharing a status. There is deliberately no cap, and none is
needed — that invariant IS the bound. The length is exactly the number
of status runs the disc's damage actually has, so:

* it does not grow with the number of passes, or with the number of
  `record()` calls. A record that repeats or extends an existing run
  leaves the partition untouched.
* it is not a ratchet. Recovering the damage between two same-status
  runs merges all three, so fragmentation FALLS as a rip succeeds — in
  the pathological interleave (`fragmentation_peaks_then_collapses…`)
  the count goes 7 → 193 → 1.
* the only thing that grows it is genuinely finer interleaving of
  statuses, bounded by `2 * (interleaved damaged sectors) + 1`.

Measured ceiling on real media: archived mapfiles for a genuinely
damaged UHD disc hold 19 entries, unchanged between passes. `record()`
is O(entries), so that is a handful of comparisons per sector.
