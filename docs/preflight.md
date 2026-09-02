# `preflight` — design notes and rationale

Overflow detail for comments in `src/preflight.rs` that would otherwise
exceed the repo's comment-length guard. Each section is pointed to from a
short in-code comment.

## `Reason.key` — the full key set

Stable reason key. The complete set this crate emits — a front-end that maps
only part of it renders a blocked Start with no explanation:

* `"no-titles"` — the scan found no titles at all.
* `"empty-selection"` — the selection resolves to no title.
* `"title-out-of-range"` — an explicit index past the last title; `detail`
  carries the offending index.
* `"multipass-requires-raw"` — `RipMode::Multi` without `job.raw`.
* `"language-unmatched"` — a language-filtered stream class the job asks for
  is carried by no selected title; `detail` carries the class key (`audio`,
  `subtitle`, `subtitle_forced`).
* `"encrypted-no-key"` — an encrypted disc, not raw, with no usable key.

## `preflight` — why each check exists

1. The disc has at least one title.
2. The selection resolves to a non-empty set of in-range title indices.
3. Every language-filtered stream class the job asks for is carried by at
   least one selected title (so a rip cannot silently ship a file without
   the audio or subtitle track the user asked for).
4. If the disc is encrypted and the job is NOT `raw`, a usable key exists
   (so a decrypting rip cannot silently write ciphertext — the same class
   of guard `Disc::ensure_decryptable` enforces at execution time, surfaced
   here earlier as data).

## Test: `a_decrypting_multipass_job_is_blocked`

`decrypt` and `multipass` were independent fields with no relationship
encoded anywhere: not in the CLI, which passes them as separate booleans,
and not in the engine, whose three recovery call sites all set
`decrypt: !job.raw` whatever the mode. Every front-end — including a GUI
that does not exist yet — could therefore construct a rip the product does
not support, and nothing would say so.

## Test: `every_emitted_reason_key_is_documented`

`Reason.key` is a contract with a UI that renders nothing but the key: a key
the guide does not list is a blocked Start button with no message. The set
was presented as four for as long as there have been five —
`multipass-requires-raw` was emitted but listed nowhere, and it is the key
`USING_THE_ENGINE.md`'s own §1 example triggers. Derived from the SOURCE,
not from a hand-kept list here, so adding a sixth key without documenting it
fails this test rather than quietly repeating the same omission.

## Test: `is_ready_agrees_with_the_variant_in_both_directions`

Every other test in this module that touches `is_ready` asserts the BLOCKED
direction, so a constant `false` was indistinguishable from a correct
implementation — and a permanently greyed-out Start with no reason shown is
a UI nobody can use. This test asserts both directions against the same
accessor, and against `reasons()`, which has to agree with it.

## Test: `ready_implies_the_selection_resolves_to_at_least_one_title`

The selection→indices policy lives in `mux::resolve_selection`, and
preflight used to carry a SECOND copy of the "does it resolve to anything?"
question — an assumption, written as a comment, that MainMovie / All /
Longest always yield at least one title on a disc that has titles. That
stopped being true when `resolve_selection` was hardened: `Longest` now
drops non-finite durations BEFORE folding (a NaN reaching the accumulator
first was never displaced and won outright), so a disc whose every playlist
has an unparseable runtime resolves to NO title.

The two copies then disagreed on exactly that input: preflight said
`Ready`, `resolve_selection` said `[]`, and `run_titles([])` returns
`Ok { titles_written: 0 }` — a rip that reports success, exits 0, and
writes nothing. Preflight now asks the owner instead of assuming.

## Test: `a_language_no_selected_title_carries_is_refused`

`StreamChoice::unmatched` computes exactly this and had no caller anywhere
in the crate: the resolved selection simply kept no audio PIDs, the mux
wrote a video-only file, and the run exited 0. The user asked for a track,
got a file without one, and nothing said so.
