# `keys.rs` — AACS key-source orchestration

Both the CLI and the desktop UI build the SAME local-first ordered
[`freemkv_keysources::KeySource`] list and extract "which source won" from
a resolution trace — this is that logic, hoisted once. Each shell keeps its
own boundary-specific bits: the CLI's implicit online-only derivation and
default-keydb-location search chain, the GUI's `shellexpand` of the
configured path and its explicit online-only toggle, and (on both) English
presentation (trace rendering, the SSRF warning, the `"unlocked via …"`
string) — none of that belongs here.

`KeyParams` is intentionally a thin, already-resolved shape: `keydb_path`
is `Some(path)` exactly when a local keydb source should be tried (the
shell has already applied its own fallback/search/expansion policy, or
decided to omit it), never a signal this module re-interprets. `online_only`
is carried explicitly (rather than re-derived from the other fields) because
the GUI's is an independent user toggle that can be true even with a
configured `keydb_path` — collapsing it back into "keydb_path is None"
would silently change GUI behavior.

## `resolve_disc_keys_is_none_and_reads_nothing_for_an_unencrypted_disc`

An unencrypted disc resolves nothing and READS nothing.

`resolve_disc_keys` is a three-line wire — factory, `resolve_keys_for`,
`won_source` — and every piece of it is well covered on its own, so the
whole function could be replaced with `Some("xyzzy")` or
`Some(String::new())` with the suite green. A fabricated winning source
label is what a front-end prints as "unlocked by keydb" and what
autorip records against the rip; inventing one for a disc that never
resolved a key is a lie about provenance.

The panicking reader also pins the short-circuit: an unencrypted disc
has no AACS inputs, so nothing may sample ciphertext off it.
