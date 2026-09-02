# `Mapfile::total_size` — why "opened for" and "last entry now" coincide

`total_size` is fixed at construction and never recomputed by
`Mapfile::record`, which neither touches it nor bounds a range against
it. So conceptually it means "the coverage this mapfile was opened
for", not "the end of the last entry as it stands now" — those two
things coincide only because every in-crate producer stays inside the
coverage: `recovery::copy` forces a fresh, correctly-sized mapfile on
ANY mapfile/disc size mismatch rather than recording past the old one.
