# `multipass.rs` — the multipass rip strategy loop

## Module origin

The multipass *loop* — call `crate::recovery::copy`'s dispatch step
repeatedly until the disc is clean or recovery stops making progress, then
decide whether the residual loss is acceptable — is the strategy that lived
duplicated inside autorip's `rip_disc` and (partially) the CLI. It moved
here so every front-end shares one implementation instead of carrying its
own copy of the loop.

## `loss_is_unscopable`

An mkv-scoped rip measures damage inside the main title's extents. A title
with no extents — a scan that could not read the playlist metadata, or the
`DiscTitle::empty()` fallback `multipass_rip_inner` uses when the disc
reports no titles at all — makes that measurement return 0, which is
indistinguishable from "clean". Whole-disc (ISO) scope sums the bad ranges
directly and needs no extents, so it is never unscopable.

Shared so the two loss paths cannot drift: `abort_lost_ms` had this guard
and the live gate in `multipass_rip_inner` did not, which is precisely how a
damaged rip kept being delivered as clean after the guard was written.

## `abort_lost_ms`

Originally a verbatim port of autorip's `abort_lost_ms`; it no longer is. It
now fails safe to NaN when the loss exists but cannot be measured — see
`loss_is_unscopable`.

NOTE for callers offering a loss override: NaN aborts under EVERY threshold,
including `u64::MAX`. autorip's `.accept-loss` escape hatch therefore does
NOT apply to a disc whose title has no extents. That is deliberate — an
override should waive a loss you can measure, not one you cannot — but it is
a behaviour change worth knowing about.

## `PatchDecision` / `patch_pass_decision`

The unified per-pass convergence decision, composed from the two gates the
loop applies (`scope_converged` at the top of each iteration and
`patch_made_progress` at the bottom). This is the single canonical multipass
strategy fn every front-end shares.

`mux_scope_bad` is the scope-aware bad-byte count observed BEFORE a pass
runs (0 ⇒ the scope is already clean ⇒ `Converged`). `recovered` is what a
pass recovered, consulted only when the scope is still bad: `None` models
the loop-top evaluation (no pass has run yet ⇒ `Continue`); `Some(0)` is a
pass that recovered nothing ⇒ `NoProgress`; `Some(n>0)` ⇒ `Continue`.

## `end_of_recovery_promotion`

BOTH maybe-states are promoted. `NonScraped` ('/') is as much a failed read
as `NonTrimmed`: `damage_sector_statuses` includes it, so every patch pass
retries it, and `bad_sector_statuses` counts it as damage. Promoting only
NonTrimmed left any surviving NonScraped range invisible to the abort gate,
which reads `Unreadable` alone — so genuinely lost bytes could not fire the
abort, and the rip was delivered as good. Such ranges arrive from an
imported or ddrescue-written mapfile, an interop this module advertises.

The patch loop — `recovery::patch` in this crate since 1.6.0, not
libfreemkv — intentionally defers this final-verdict step to the
orchestrator: a range still `NonTrimmed` after pass N may still read on
pass N+1, so only the loop that knows there is no pass N+1 can call it lost.

## `pass_should_decrypt`

FOUR pass-option sites spelled this `!job.raw` inline — the single-pass
copy, the Pass 1 sweep and every patch pass here, plus `recover_to_iso`'s
own `CopyOptions` in `run.rs`. Four copies of one policy is the shape that
drifts, and a lone `!` is the easiest character in the file to lose. It
matters in both directions: `raw` exists to preserve the ciphertext for
out-of-band decryption later, so a raw pass that decrypts defeats the mode —
and a non-raw pass that does NOT decrypt writes an ISO of ciphertext and
returns success, which is the silent-garbage-success class the pre-flight
gate in `copy` exists to stop.

## `bad_sector_count`

The second term is RETRYABLE bytes, not `bytes_pending`. They are not the
same thing: `bytes_pending` also counts `NonTried` — the un-attempted
remainder of the disc ahead of the sweep head — so on any interrupted run it
is dominated by sectors nobody has looked at yet. Feeding that in scored a
rip cancelled ten seconds into a 66 GB disc as ~32 million bad sectors and
stamped it Serious, when what was actually known was zero damage. `run.rs`'s
live progress tick already refuses that aggregate for the same reason and
says so; this is the same rule, applied to the same number, at the other end
of the run.

## `end_of_recovery_bad_sectors`

`MapStats` publishes three overlapping totals and only one of them is
damage: `bytes_pending` = `bytes_nontried` + `bytes_retryable`, and
`bytes_nontried` is disc nobody has attempted, not disc that failed. The
end-of-recovery gate read `bytes_pending`, which is the aggregate
`bad_sector_count`'s own doc forbids — so any state that reaches the gate
with un-attempted sectors would be scored as though every one of them were a
bad sector. (Today the sweep leaves none there; that is a property of the
pass order, not of this arithmetic, and it is not the sort of thing a
severity badge should depend on.)

## `UNMEASURED_ON_AN_INTERRUPTED_PASS`

A `CopyResult` carries one `bytes_pending` total and cannot say how much of
it is retryable damage versus never-attempted disc, so an interrupted path
has nothing honest to pass. Zero, and the name says why: the unreadable
count is what is actually KNOWN, and it still scores — a cancel that had
already found 300 MB unreadable does not come back Clean.

## `PassExit` / `pass_exit`

A pure function over the two flags a `PatchOutcome` carries, because the
branch that consumes them needs a live USB-bridge crash to reach and so
could not be tested where it sits. `wedged_exit` was simply not read at all:
a pass killed by a transport fault fell through to the exhaustion gate,
ended the recovery, and its never-attempted ranges were promoted to
permanently `Unreadable` — after which a re-run took `recovery::copy`'s
terminal shortcut and never retried them. Recoverable sectors, written off
on the strength of a flag nobody looked at.

`halted` wins when both are set: the user asked to stop, and that is the
more specific thing to tell them.

## `interrupted_severity`

Two rules meet here and both are load-bearing:

- The score must not fold in `NonTried`, or a cancel ten seconds into a
  66 GB disc reports ~32 million bad sectors and stamps Serious on a disc
  nobody has looked at.
- The badge must not read **Clean** beside a non-zero pending count. The
  front-end draws them together, and Clean is the reading that gets
  believed — that contradiction is what the halted branch was written to
  stop, and it stays stopped.

So: damage is scored from the unreadable bytes alone, and outstanding work
merely denies the Clean badge rather than inventing a tier for it. `Cosmetic`
is the floor because it is the mildest thing that is not Clean; an
interrupted run makes no claim about how bad the rest of the disc is.

## `recovery_is_complete`

`aborted_for_loss` is load-bearing on its own: the unreadable-mapfile
fail-safe sets `main_lost_ms = NaN` (so the gate fires) while the carried
in-flight counters can both legitimately be zero — a disc that looked clean
all the way through the loop but whose damage record could not be re-read at
the final verification step. Without that term such a rip reports
`complete: true` on the exact run the abort gate just refused. Extracted
because reaching that branch from outside needs the mapfile corrupted
mid-function, which no black-box fixture can do.

## `main_title_lost_ms`

Scales bad bytes by the SAME title's OWN size and runtime (the
dimensionally-correct figure the CLI switched to — a whole-disc ratio ×
first-title duration was wrong once bonus content made the disc larger than
the main title).

The divisor is taken from the very `title` its caller scoped `main_bad_bytes`
to, NOT from `disc.titles.first()`. Those agree for today's only caller (the
main title IS the first title), but re-deriving the divisor from the disc
re-opened, inside the helper written to close it, exactly the title/count
mismatch `end_of_recovery_lost_ms` exists to prevent: a future
main-title-by-longest/largest selection would scope bytes to one title and
silently divide by another. Threading `title` makes the two share one
parameter, so they cannot diverge.

## `end_of_recovery_lost_ms`

Pure and separate from `multipass_rip_inner` deliberately. This decision
lived inline inside a function that needs a drive, a mapfile and a live
sink, so nothing could reach it — which is exactly how it shipped answering
`0.0` for damage it could not measure. A test of the PREDICATE alone does
not guard the gate: the bug was that the gate did not consult the predicate.

SCOPE — this figure is ALWAYS main-title-scoped, whatever the deliverable
is, and it derives its own byte count from `title` + `bad_ranges` rather
than accepting the ABORT GATE's count. Those two counts are deliberately
different things and conflating them was a real defect: the gate's count
(`abort_lost_bytes`) is whole-disc for an ISO deliverable, and feeding that
whole-disc figure into `main_title_lost_ms` scaled every unreadable byte on
the disc — scratched menus, trailers, a damaged bonus feature — by the MAIN
TITLE's size and duration. A disc whose main title was untouched reported
tens of seconds of "main-title playback loss", and since `classify_damage`
escalates at 30 s that inflated number could stamp `Serious` on an intact
feature. It failed safe (an ISO run has `effective_abort_secs == 0`, so
`lost_bytes > 0` refuses the rip anyway), but the reported number and the
badge were both wrong, and `main_lost_ms` carried a value its own doc says
it does not. Scoping is always possible here — `bytes_bad_in_title` needs
only the extents and the bad ranges, both of which the ISO caller already
has in hand — so there is no reason to reach for the NaN escape hatch except
when the extents are missing, which is the branch below.

## `MultipassResult::wedged`

Distinct from `MultipassResult::halted`, which is the user pressing Stop,
and it has to be: the ranges a wedged pass never reached are still
RETRYABLE, so the end-of-recovery promotion that writes surviving ranges off
as permanently `Unreadable` must not run. Dropping this signal meant a
bridge crash was recorded as permanent loss, and a later re-run then took
`recovery::copy`'s terminal shortcut (nothing retryable, nothing
un-attempted) and never touched those sectors again.

The front-end's cue to spin-cycle the drive and resume from the mapfile,
which is exactly what `patch` documents the fault for.

## `MultipassOpts::is_iso_output`

It does NOT widen the MILLISECOND figure. `end_of_recovery_lost_ms` stays
main-title-scoped whatever the deliverable is (see its doc, and
`MultipassResult::main_lost_ms`) — widening it meant off-title damage was
reported as lost feature playback. Neither path goes through
`abort_lost_ms`, which no longer sits on it at all.

## `multipass_rip`

Mirrors autorip's `rip_disc` pass sequence exactly (sweep via
`recovery::sweep`, then `for` patch passes via `recovery::patch`, gated by
`patch_pass_decision` at loop-top and loop-bottom, promoted via
`end_of_recovery_promotion`, gated by `loss_aborts`) with the hardware/UI
touch-points (transport-crash retry, `spin_cycle`, watchdogs, per-pass STATE
painting) omitted — those are autorip-shell concerns, not strategy.

## Test rationale (`#[cfg(test)] mod tests`)

**`every_multipass_result_field_is_documented`** — `USING_THE_ENGINE.md` is
the GUI contract, and its §4 field list is what an implementer builds the
result page from. The list was presented as complete for as long as there
have been two more fields than it names: `wedged` — whose own rustdoc says
it exists to be the front-end's cue to power-cycle the drive and resume —
and `complete`, the only field that says whether the rip actually finished.
A GUI following the list verbatim renders a bridge crash as an ordinary
partial rip and has no correct test for "done". Derived from the SOURCE, not
a hand-kept list, so an undocumented new field fails this test rather than
quietly repeating the omission. Mirrors `preflight.rs`'s
`every_emitted_reason_key_is_documented`.

**`abort_lost_ms_fails_safe_when_loss_cannot_be_quantified`** — not
reachable from today's callers (autorip guards one site and feeds a fallback
bitrate at the other), so this pins an exported API against a future
front-end — and against the mutation run, which replaced this whole body
with a constant and kept the suite green.

**`unmeasurable_in_title_loss_is_never_reported_as_zero`** — the previous
round put this guard in `abort_lost_ms`, which has no production callers.
`multipass_rip_inner` hand-rolls `abort_lost_bytes` + `main_title_lost_ms`
instead, and that pair had the hole: an extents-less title makes
`bytes_bad_in_title` return 0, so `main_title_lost_ms` returns 0.0 on its
FIRST line and never reaches its own NaN branch. Under a non-zero tolerance
that ships a damaged rip as clean. Reachable because `preflight` is advisory
and the gate falls back to `DiscTitle::empty()` when the disc reports no
titles.

**`lost_ms_needs_both_a_size_and_a_duration`** — the mutation run flipped
`t.size_bytes > 0 && t.duration_secs > 0.0` to `true`, to `||`, and each `>`
to `>=`, and the suite stayed green — so nothing constrained the difference
between "quantify it" and "admit we cannot". Getting that wrong in the
permissive direction divides by zero and yields inf or NaN by accident
rather than by decision.

**`abort_lost_ms_converts_bytes_to_milliseconds_exactly`** — the run mutated
`/` to `*`/`%` and `*` to `+`/`/` in the conversion and nothing failed — the
ms figure feeds the abort gate, so a wrong operator is a wrong abort
decision.

**`end_of_recovery_lost_ms_scopes_divisor_to_the_passed_title`** — pins the
round-2 fix: before it, the ms divisor was re-derived from
`disc.titles.first()` while the bytes were scoped to `title`, so a caller
that passed a title other than the first would scope bytes to one title and
divide by another.

**`an_unquantifiable_loss_is_serious_not_cosmetic`** — every NaN comparison
in Rust is false, so before this both tier tests fell through and a rip
whose damage record could not be read was badged Cosmetic — while
`loss_aborts`, which handles NaN explicitly, was simultaneously refusing to
deliver it. The two halves of the same decision disagreed.

**`the_pass_count_saturates_instead_of_wrapping`** — `multipass_rip_inner`
clamps `max_passes` with `.min(u8::MAX as u32)`, so 255 is reachable by
construction. `max_retries + 2` then panicked in dev and, with
debug-assertions off in release, wrapped to 1 — leaving the UI a total-pass
denominator smaller than the number of passes about to run. Saturating is
the only answer correct in both profiles.

**`a_wedged_result_is_distinguishable_from_a_cancelled_one`** and its
`WedgeOnPatchReader` fixture — this test used to assert
`wedged.wedged && !wedged.halted` over a `MultipassResult` written out by
hand in the test body. It executed no production code at all: the
`PassExit::Wedged` arm — the one that returns early so the never-reached
ranges are NOT promoted to permanently `Unreadable` — was unreached by the
whole suite, and deleting it left every test green. It now drives a real
`multipass_rip` whose patch pass dies on a bridge crash. The reader
simulates marginal (RECOVERED) errors while the sweep walks forward, then a
transport failure (status 0xFF, a USB bridge crash) once the sweep reaches
the end and the patch pass comes back for the range it left behind.

**`the_doubles_return_a_byte_count_like_the_trait_says`** — all three test
doubles in this module filled `count * 2048` bytes and then returned
`count` — a lie that costs nothing while every in-crate consumer matches
`Ok(_)`, and costs a silently desynced stream the moment one is handed to a
consumer that believes the number (libfreemkv's own `PrefetchedSectorSource`
advances its cursor by `n / 2048`).

**`Spot` / `MultiSpotReader`** — a `Spot` is one deliberately-bad
single-sector LBA: fails every read that overlaps it until it has been
touched `heal_after` times, then reads clean forever. `heal_after: 1` is
"recoverable on the very first re-read" (Pass 1's own touch is attempt #1
and fails; Pass 2's re-read is attempt #2 and succeeds). `heal_after:
u32::MAX` never heals within a test — permanent loss. Reports
`SENSE_KEY_RECOVERED_ERROR` (a real "distrust and re-read" sense, not a
transport/wedge fault) so Pass 1 marks it NonTrimmed via a plain
`SkipBlock` — no 30s zone-entry cooldown, no wedge escalation — keeping the
test fast.

**`single_pass_reports_loss_as_unquantified_on_a_damaged_disc`** — the only
single-pass test ran a CLEAN disc, where `main_lost_ms: 0.0` is correct and
therefore indistinguishable from a hard-coded constant. On a DAMAGED disc
the constant was a claim nothing had measured: a rip returning `complete:
false` and a non-zero unreadable count also reported, in the same breath,
that no playback was lost. Single-pass never runs the gate that measures
loss, so it must say so.

**`the_final_score_ignores_un_attempted_sectors`** — `MapStats.bytes_pending`
is `bytes_nontried + bytes_retryable`, and the end-of-recovery gate used it.
The fixture is a 66 GB disc on which nothing has failed and 64 GB has never
been attempted: the pending reading is ~33 million bad sectors —
`Serious`, the badge that tells an operator to bin the disc — where the
truth is zero.

**`an_unmeasured_scope_never_converges`** — the loop-top gate loads the
mapfile to ask "is the muxable scope clean yet?". When that load fails the
answer is unknown — and the fallback it used to substitute could be zero,
the ONE value that means "converged, stop retrying". A failed read of the
rip's only damage record then ended the recovery and logged "muxable scope
100% recovered".

**`HookSink`** — records every line the loop logs and, the first time a line
contains `trigger`, runs `action` (and optionally starts cancelling). Two
trigger points are used: `"exhausted"`/`"skipping remaining"` (logged
immediately before the `break` that leaves the patch loop, so a sabotage
takes effect for the end-of-recovery `Mapfile::load` and nothing else), and
`"multipass_rip: pass "` (logged right after a patch pass returns and
before the next iteration's cancel check — the only way to arm a cancel
that the *loop-top* check, and not a recovery primitive's own halt token,
is guaranteed to observe first).

**`in_title_damage_fixture`** — a 4096-sector disc whose ONE title's extents
span the whole image, so an MKV-scoped (`is_iso_output: false`) gate sees
the damage at LBA 1000 and the loop actually runs patch passes.

**`GENEROUS_TOLERANCE_SECS`** — the real residual loss on the fixture above
is ~35 s of a 7200 s title, so an HOUR of tolerance accepts it. Every abort
asserted in the sabotage tests therefore comes from the fail-safe under test
and from nothing else.

**`multipass_rip_accepts_a_measurable_loss_under_a_generous_tolerance`** —
CONTROL for the two sabotage tests below: the same disc, the same damage,
the same tolerance, with the mapfile left alone. The gate quantifies the
loss, accepts it, and does NOT abort. Without this the sabotage tests could
pass for the wrong reason (any run of this fixture aborting).

**`iso_damage_outside_the_main_title_is_not_reported_as_main_title_loss`** —
the two byte counts at the gate are different quantities and were being
conflated. `abort_lost_bytes` is WHOLE-DISC for an ISO deliverable, and that
whole-disc total was handed straight to `main_title_lost_ms`, which divides
by the MAIN TITLE's size and multiplies by the MAIN TITLE's runtime. A
scratch in a trailer, a menu or a bonus feature therefore came back as
seconds — here, 72 of them off ONE bad sector — of lost playback in a
feature the drive read perfectly. `classify_damage` escalates at 30 s, so
the badge read `Serious` on an intact movie, and `main_lost_ms` carried a
number its own doc says it does not.

The mutation this catches: hand the gate's `lost_bytes` back to
`end_of_recovery_lost_ms` instead of letting it scope its own bytes to the
title. With that mutation `main_lost_ms` is 72_000 — one off-title sector
(2048 B) scaled by a 100-sector title's 204_800 B and 7200 s runtime — and
`severity` is `Serious`; both assertions in the test fail.

Note what does NOT change: the run is still refused. `effective_abort_secs`
forces an ISO tolerance to 0, so `loss_aborts` fires on the whole-disc
`lost_bytes > 0` term regardless of the millisecond figure. That is why this
was a wrong NUMBER and not a shipped-bad-rip — and why the fix must not
disturb `aborted_for_loss`, which is asserted here too.

**`multipass_rip_aborts_when_the_mapfile_cannot_be_read_at_the_gate`** —
`end_of_recovery_lost_ms` is tested as a pure predicate, but the shipped bug
was that the GATE did not consult a predicate. This drives `multipass_rip`
end to end and replaces the mapfile with a directory at the instant the
patch loop breaks, so `Mapfile::load` at the gate returns `Err`. The
fail-safe answers NaN, which `loss_aborts` treats as abort under EVERY
threshold — including the hour-long one that the control test above proves
accepts this disc's real loss.

**`multipass_rip_aborts_when_the_end_of_recovery_promotion_cannot_be_persisted`**
— promotion is what makes the loss visible: the gate reads `Unreadable`
ranges only, so a range that fails to promote out of `NonTrimmed` silently
drops out of the decision it should be driving — and the rip ships as good.
Here `Mapfile::load` SUCCEEDS (so this is not the unreadable-mapfile branch
above — asserted) but the promoted map cannot be persisted, because
`write_to_disk` writes `<mapfile>.tmp` first and that path has been replaced
by a directory. `flush()` fails, `promotion_intact` goes false, and the loss
becomes unquantifiable.

**`multipass_rip_cancelled_mid_loop_is_halted_and_never_reported_clean`** —
two untested things at once, because they are one user-visible event: the
loop-top cancel check (which breaks out between patch passes) and the
halted result branch. Severity there was once hard-coded `Clean`, so a
cancelled rip that had already found unreadable sectors rendered a "Clean"
badge next to a non-zero unreadable count. The cancel is armed from the
"pass N recovered" log line, i.e. after the first patch pass has returned
and before the second iteration's cancel check — the only window in which
the LOOP's own check, rather than a recovery primitive's halt token, is
guaranteed to be the thing that stops the run. Hence the exact `passes ==
2`.

**`a_cancel_with_a_wide_pending_region_is_not_scored_from_it`** — the
mid-loop cancel test above ends with one pending sector, where "unreadable
only" and "unreadable + pending" both come out Cosmetic, so neither formula
could be told from the other. Here the pass leaves a wide unrecovered
region: folding pending in scores >= 500 bad sectors and stamps **Serious**
on a rip that confirmed no loss at all, which is the badge the front-end
draws next to the counters.
