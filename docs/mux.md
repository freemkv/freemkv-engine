# mux.rs — design notes

## Module history (2026-07-28 user feedback)

This module is the orchestration that lived in the CLI's `run()` and
autorip's `rip_disc`: resolve which titles to rip, mux each through
`libfreemkv::mux_stream`, and decide when a failure is fatal vs skippable.
It carries three behaviours that were wrong or duplicated in the consumers:

1. **Fail-fast on a disc-level key failure.** If the disc needs decryption
   and no key resolved (and not `raw`), EVERY title will fail — so we
   refuse once, up front, instead of printing a "no key" error for all N
   titles.
2. **Cancel is a full stop.** One halt breaks the whole title loop; it
   does NOT cancel each remaining title individually and carry on.
3. **Main-title default** (via `Selection`) so an obfuscated disc with 50+
   similar-length playlists doesn't rip everything by accident.

## `RipOutcome::Failed` — why it carries both `code` and `kind`

Without a cause this variant named only WHICH title died and never WHY, so
a front-end could not tell a full disk from a permission denial from a
malformed source — it had a title index and a log line, and the log line
is prose, not something it can branch on.

The cause needs BOTH fields because libfreemkv reports it two ways.
`From<Error> for io::Error` stringifies a typed error as `E<code>: …`, but
it deliberately passes an `Error::IoError` straight through unwrapped so
the original `ErrorKind` and OS errno survive instead of being flattened.
So a genuine I/O failure — the full disk — has NO `E<code>` prefix to
parse and arrives as `code: None` with the truth in `kind`, while a typed
failure like a malformed stub arrives as `code: Some(_)` with `kind`
merely `Other`. Carrying one field would have blinded the front-end to
exactly one half of the failures.

## `run_titles` — the three loop behaviours, in detail

Self-contained — no `Disc` needed. `mux_one(idx) -> io::Result<()>` muxes
a single title; injecting it keeps the loop's control flow (fail-fast,
skip, halt-break) unit-testable without a real ISO. Production passes
`mux_title` (or the consumer's own single-title mux).

1. **Fail-fast on a disc-level key failure**: the FIRST title that fails
   with a whole-disc key error (`is_disc_level_no_key`) stops the whole
   rip — every remaining title would fail identically. No 54× error spew.
2. **Cancel is a full stop**: `should_cancel()` between titles, OR a halt
   error from inside a title's mux, returns `RipOutcome::Halted` and does
   NOT continue to the next title.
3. Skippable stubs (empty/uncrackable non-feature titles) are skipped in
   a multi-title, non-explicit rip; fatal on the feature / explicit `-t`
   / single-title.

`explicit_selection` is `true` when the user named specific titles (so a
stub there is what they asked for → fatal). For an all-titles rip pass
`false`.

## `with_mux_watcher` — why it's split out and what can go wrong

`mux_with_input` itself cannot be unit-tested: everything it adds on top
of this is a call into `libfreemkv::mux_stream`, which needs real media.
But the two things that go WRONG here are not about media at all — a
cancel that never reaches the muxer (the user presses Stop and watches
the rip run to completion anyway), and a watcher that is never told to
exit (the scope joins a thread that loops forever, so the mux hangs
instead of returning). Both are exercisable against a closure standing in
for the mux, so they are.

Runs `f` with:
- a `libfreemkv::Halt` mirroring `sink.should_cancel()` — asked ONCE
  before `f` starts and then polled every 100 ms by a scoped watcher;
- a `'static` `libfreemkv::MuxEvents` handle (`mux_stream` takes it as an
  `Arc`, so it cannot borrow the `&dyn Sink`) whose write-progress is
  forwarded to `Sink::progress` by that same watcher.

## Test rationale (kept here so the test bodies stay short)

- `selection_longest_breaks_a_tie_towards_the_first_title`: playlist
  obfuscation authors decoys with the SAME runtime as the feature, so a
  tie is the normal case on exactly the discs `Longest` is for, and the
  real playlist is conventionally the lowest index. `Iterator::max_by`
  returns the LAST of equal maxima, which would pick a decoy, rip it, and
  report success.
- `selection_longest_ignores_a_leading_title_with_no_measurable_duration`:
  a non-finite duration must never win, INCLUDING when it is the first
  title. Rejecting NaN only inside the comparison looks correct and is
  not: `t > NaN` is false for every `t`, so a NaN that reaches the
  accumulator first is never displaced. A disc whose first playlist has
  an unparseable runtime would rip that playlist every time.
- `selection_explicit_index_equal_to_the_title_count_is_out_of_range`:
  every other out-of-range case in this suite uses an index comfortably
  past the end (9 against 2), so `<` and `<=` agree and the boundary was
  never pinned. `<=` admits `disc.titles[n]`, the classic off-by-one that
  either panics on the index or hands the muxer a title the disc doesn't
  have.
- `a_single_non_explicit_title_stub_is_fatal_not_skipped`: `multi_title =
  indices.len() > 1` is the flag `decide_title` consults to decide
  whether a non-feature stub may be silently skipped. Widened to `>=`, a
  single selected non-feature title whose mux comes back as a skippable
  stub is swallowed: the rip returns `Ok { titles_written: 0 }` — success
  that wrote no file — instead of reporting the failure. The existing
  single-title stub test passes `explicit_selection: true`, which is
  fatal by a different clause and so hides this one.
- `an_already_cancelled_sink_halts_the_mux_before_it_starts`: a watcher
  alone makes cancellation a race the work can win: on a short title the
  mux can finish before the watcher thread is first scheduled. Only
  asking before the work begins closes that window. The sink answers
  `true` exactly ONCE — that's the pre-check — and `false` after, so
  nothing but the pre-check can be what cancelled the token.
- `a_cancel_during_the_mux_reaches_the_halt_token`: the sink only starts
  cancelling AFTER the mux has begun, so the pre-check cannot be what
  sets the token — the watcher's poll is the only path left. Without it
  a user's Stop is swallowed and the mux runs to completion.
- `write_progress_reaches_the_sink_as_a_mux_progress_tick`: `mux_stream`
  reports through an `Arc<dyn MuxEvents>` that cannot borrow the `&dyn
  Sink`, so the bridge is a channel plus the watcher drain. If that drain
  is broken the rip shows no movement at all for its whole duration.
- `a_panicking_mux_still_releases_the_watcher`: `mux_stream` runs on
  damaged and malformed media, so a panic there is in scope.
  `thread::scope` joins the watcher before it resumes the unwind — if
  `done` is only stored after the call returns, an unwind skips it and
  the watcher loops forever: the rip HANGS instead of failing, and no
  error ever reaches the front-end. Bounded here, because the failure
  mode is a hang.
- `a_rip_that_writes_nothing_says_so_rather_than_returning_a_silent_ok`:
  `Ok { titles_written: 0 }` is success, exit 0, no file — and it is
  reachable two ways: an empty `indices` (a caller that skipped
  `preflight` — the shape that shipped once already: preflight said
  Ready, `resolve_selection` said `[]`, the rip "succeeded" and wrote
  nothing), and a selection whose every title turned out to be a
  skippable stub. In both, every other arm of this loop reports through
  the Sink and this one returned silently, so nothing told the user why
  the disc produced no output. The outcome stays `Ok` — the count is the
  machine-readable half of the answer and changing the variant would
  break every front-end's `match` — but it must not be SILENT.
- `the_failure_cause_says_which_failure_it_was`: the assertions derive
  the cause from the same fixture they compare against, so on their own
  they'd still pass if every failure reported nothing. Three failures,
  three distinguishable causes: a malformed stub is typed (code present,
  bland kind); a full disk is a passthrough OS error (NO code, kind is
  the whole story — the case a code-only field would have reported as an
  anonymous failure); the two must not be confusable.
- `duplicate_title_indices_are_deduped_preserving_first_seen_order`: a
  repeated `-t` index must produce ONE entry. Muxing the same title twice
  counts it twice in `titles_written` while a front-end naming files from
  `title_index` writes a single file — a success count that doesn't match
  the disk — and flips `multi_title` on what is really a single-title rip.
</content>
</invoke>
