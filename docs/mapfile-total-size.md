# `Mapfile::total_size` — why "opened for" and "last entry now" coincide

`total_size` is fixed at construction and never recomputed by
`Mapfile::record`, which neither touches it nor bounds a range against
it. So conceptually it means "the coverage this mapfile was opened
for", not "the end of the last entry as it stands now" — those two
things coincide only because every in-crate producer stays inside the
coverage: `recovery::copy` forces a fresh, correctly-sized mapfile on
ANY mapfile/disc size mismatch rather than recording past the old one.

## `open_or_create`'s size-mismatch warning (equivalent mutant)

`open_or_create` compares the loaded `total_size` against the size the caller
asked for and WARNS on a mismatch — it deliberately does not fail, because the
caller (`recovery::copy`, `recovery::sweep`) owns the decision to downgrade to
a fresh sweep and makes it with better information. Both arms therefore return
the loaded `Mapfile` unchanged, so inverting the comparison only changes which
mapfiles get a log line. A mutation run will keep reporting it; there is
nothing to test without a log-capture harness this crate doesn't carry.

By contrast the `NotFound` guard one arm down IS load-bearing, and is pinned by
`open_or_create_propagates_a_corrupt_file_instead_of_overwriting_it`: only a
MISSING file may route to `create()`. A file that exists but doesn't parse has
to propagate, because creating over it destroys the record of what was already
read and silently restarts the rip from sector 0 while reporting a clean
resume.
