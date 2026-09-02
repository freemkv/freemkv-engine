# `fragmentation_peaks_then_collapses_as_damage_is_recovered` rationale

The `Mapfile.entries` bound, measured rather than asserted from a doc.

A patch pass that recovers alternate sectors inside a damaged region is
the worst case coalescing can face — it is the ONLY thing that defeats
the merge, since a record that repeats or extends an existing run
never adds an entry. Even so the list is not a ratchet: pass 3
recovers the interleaved remainder and 193 entries collapse back to 1.

So fragmentation tracks the damage topology, not the pass count, and
it is bounded by `2 * (interleaved damaged sectors) + 1`. Archived
mapfiles from real damaged UHD media hold 19 entries and do not grow
between passes (see `real_shaped_mapfile_round_trips`).
