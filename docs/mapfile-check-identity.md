# `check_mapfile_identity` rationale

Does this mapfile describe the disc currently in the drive?

Resume used to be gated on ONE fact: `map.total_size() ==
disc.capacity_bytes`. Two different discs authored at the same size —
box-set reprints, or the same title pressed twice — satisfy that, so a
mapfile left by disc A was trusted for disc B: A's `Finished` ranges
were never re-read, and the output ISO silently spliced sectors from
two physical discs while passing every completeness check.

The mapfile already persists a disc identity; nothing ever read it
back. `Mapfile::vid()` had no caller outside this module's own tests.

Identity is keys-XOR-vid, matching how the mapfile stores it
(`set_unit_keys` clears the VID — keys are the final answer, a VID is
the "still unresolved" marker). Checking only the VID would have been
nearly inert: a normally ripped AACS disc resolves unit keys, so it
stores keys and NO vid, which is precisely the box-set case above.

A mapfile carrying neither is `Ok` — legacy files, unencrypted discs
and CSS DVDs record no AACS identity, and refusing them would strand
every existing in-flight rip. That residual gap is real and
deliberate: this closes the AACS case, not the unencrypted one, which
would need a new identity header.
