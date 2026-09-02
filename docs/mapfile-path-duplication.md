# `agrees_with_libfreemkv_disc_mapfile_for` test rationale

This crate's `mapfile_path_for` must agree with libfreemkv's
`Disc::mapfile_for` for any ordinary output path.

The rule genuinely exists twice: libfreemkv needs it to back
`Disc::mapfile_for`, and it cannot call into this crate (the dependency
runs the other way). So the duplication is structural, not an
oversight — but two copies of a naming rule are exactly how a rip ends
up writing its mapfile to one path and looking for it at another,
which reads downstream as "no mapfile" and silently restarts a
recovered disc from sector 0. This test pins them together so a
change to either side fails here.

`/dev/null` is deliberately NOT compared: `Disc::mapfile_for`
special-cases benchmark output to a temp-dir path derived from the
disc title, which is a `Disc`-dependent rule this plain-path helper
does not and should not reproduce.
