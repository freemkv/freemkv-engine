# `load_if_present` rationale

Loads a mapfile, distinguishing "there isn't one" from "there is one
and it is unreadable".

Three call sites classified this themselves and reached three
different fail-safes: `progress_snapshot_from_mapfile` collapsed both
cases into `None` via `.ok()?`, so a corrupt sidecar rendered as "no
data yet" instead of surfacing that the damage record is unreadable;
`bytes_bad_in_title_from_mapfile` correctly reports the whole title
bad; and `multipass_rip` forces an abort. The CLASSIFICATION is shared
here so corruption is never silently indistinguishable from absence —
the fail-safe VALUE stays per-caller, because "assume clean", "assume
all bad" and "abort" are genuinely different correct answers for those
three questions.
