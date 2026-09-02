# `resolve.rs` — decrypt-gate rationale

## `ensure_decryptable_strict`

The decrypt gate the executors use: [`libfreemkv::Disc::ensure_decryptable`]
plus this crate's own [`resolve_keys`] judgement.

The library gate only errors when it has an AACS or CSS state to judge. A
disc that reports `encrypted` but resolved NEITHER — which `Disc::scan`
produces when the volume carries `/AACS` but the VID probe failed, leaving
`aacs: None, aacs_error: Some(..)` — therefore passed it, the decrypt
wrapper degraded to a pass-through, and the copy finished at
`complete: true`, exit 0, having written an unplayable ciphertext image.

`preflight` already blocks that disc, via `resolve_keys`. The two predicates
disagreed, and only the one NOT on the execution path was strict. Sharing
the judgement here means "can this disc be decrypted" has a single answer
whether it is asked before the rip or during it.

## `every_emitted_key_summary_is_documented`

`summary` is a contract with a UI that renders nothing but the key: an
undocumented value is an unlocalised string in front of the user, on the
strip whose whole job is to explain why a disc will not decrypt. The set
was presented as seven for as long as there have been ten — the three
`key-service-*` values (a key SOURCE could not answer, which is NOT the
claim "this disc has no key") were emitted and listed nowhere.

Derived from the SOURCE — the body of `resolve_keys` itself — not from a
hand-kept list here, so adding an eleventh summary without documenting it
fails this test rather than quietly repeating the same omission. Mirrors
`preflight.rs`'s `every_emitted_reason_key_is_documented` and
`multipass.rs`'s `every_multipass_result_field_is_documented`.
