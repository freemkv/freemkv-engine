# `recovery::read_error` — rationale and design notes

Long-form background for comments trimmed out of
`src/recovery/read_error.rs` by the comment-guard cap. See the source
file for the current, authoritative contract; this file is history and
rationale only.

## Module scope

Pass N does not route through `handle_read_error`. Its recovery is the
time-bounded handler chain in `section_recover.rs`, driven by
`patch.rs`. `ReadCtx::for_patch` and the Pass-N constants in this
module are the relocated Pass-N tuning table, exercised only by this
module's own tests — there is no production call site for them, and no
`src/disc/` module in this crate (an older doc comment used to point at
a `handle_read_failure` in `disc/patch.rs` that no longer exists).

## `for_sweep` — wedge-prevention principle (2026-05-11)

Pass 1's job is "fast and accurate, get the most data in the shortest
time" — Pass N is the one that grinds on the bad ranges. The
damage-jump fast path triggers after just 1 consecutive outer-batch
failure because once the drive returns ANY recoverable error, retrying
the same LBA quickly is what triggers the firmware fast-fail
transition. Pass N owns the heavy retries — it gets per-sector timeouts
that don't hammer the firmware the same way.

## `for_patch` — no production caller

Pass N's read effort is owned by the handler chain in
`section_recover.rs`, which does its own read sizing and error
classification and never routes through `handle_read_error`. This
constructor is kept as the Pass-N half of this module's tuning table —
`for_sweep`'s values only mean anything next to it, and the wedge and
zone-entry branches in `handle_read_error` still read `patch_pass`.

## Pause budget constants (2026-05-07 BU40N traces)

Tuned from traces showing bridge wedges 524 ms after a 5.4-second
internal ECC retry. The post-failure pauses give the drive — and the
bridge — time to settle: error → drive ECC retry (5-10s internal) →
return → cooldown pause → next read.

- `FAIL_PAUSE_SECS` (5s): 2026-05-11 reframe — a failed read is a failed
  read, regardless of which pass is running. The prior split (1s for
  Pass N, 5s for Pass 1 via a since-removed `PASS_1_FAIL_PAUSE_SECS`)
  was solving an imaginary cost problem: real damaged-disc cases mark
  <50 MB NonTrimmed, and the extra 5s/error is single-digit minutes per
  pass, not hours. The cost of NOT pausing — a drive wedge that aborts
  the entire multi-pass recovery — is much worse.
- `ZONE_ENTRY_COOLDOWN_SECS` (30s): a 2026-05-11 wedge incident showed 7
  medium errors in 6.5 seconds (~1s per attempt + ~1s pause) push the
  BU40N's firmware into IllegalRequest fast-fail mode permanently. Once
  there, only physical eject + reload clears it. Giving the drive 30s
  of breathing room after the FIRST error in a zone — before more error
  counts accumulate in the firmware's internal window — prevents the
  transition. Cost on clean discs: zero. Cost on damaged discs: ~30s ×
  N damage zones (2.5 min extra on a 5-zone disc).
- `CONSECUTIVE_FAIL_LONG_PAUSE_SECS` (5s): same value as
  `FAIL_PAUSE_SECS` because empirically 5s is enough; kept as a
  separate name so the escalation policy is explicit at the call site.

## `JUMP_BASE_SECTORS` tuning history

Bumped 2026-05-10 from 256 → 1024 (4×) so the first damage-jump at
batch=32 covers 64 MB instead of 16 MB. Empirically the BU40N's damage
clusters are 100+ MB wide; 16 MB jumps landed inside the cluster and
the re-read added to the firmware wedge counter. 64 MB → 128 MB (after
one doubling) clears almost any single-cluster damage in 2 jumps.

## Firmware-wedge skip policy

A damaged drive's firmware can latch into returning
HARDWARE_ERROR/ILLEGAL_REQUEST for every later read; instead of
aborting immediately the handler does JumpAhead + cooldown, aborting
only after `WEDGE_ABORT_THRESHOLD` consecutive wedges.

- `WEDGE_JUMP_SECTORS` (1 GiB): big enough to clear almost any
  single-cluster damage zone observed.
- `WEDGE_PAUSE_SECS` (30s): a wedged drive needs a significant
  cool-down to leave fast-fail; balances giving the drive a chance to
  recover against not stalling the rip if it's permanently stuck.
- `WEDGE_ABORT_THRESHOLD` (16): at 1 GB jumps this scans ~16 GB of
  fully wedged area before giving up — generous enough to clear most
  physical-damage clusters, bounded enough to not loop forever on a
  permanently bricked drive.
- `WEDGE_PASS_N_SKIP_SECTORS` (64): Pass N's batch=1 reads target
  specific NonTrimmed sectors from Pass 1, so a 1 GB skip would blow
  past the current NonTrimmed range and abandon many sectors that
  might still recover. A small skip just moves past the bricked LBA
  plus a small buffer.

## `ReadCtx::long_pause_escalations`

The long-streak pause escalation (`consecutive_failures >=
CONSECUTIVE_FAIL_LONG_PAUSE_THRESHOLD`) currently resolves to the same
number of seconds as the ordinary inter-error pause (see
`CONSECUTIVE_FAIL_LONG_PAUSE_SECS`), so it has NO effect a caller could
observe from the returned `ReadAction` — deleting the branch outright
changed nothing any test could see. `long_pause_escalations` counts it
so the policy is observable in its own right: the pass summary can say
how often the drive was in a long failure streak, and the branch
cannot be removed without a test noticing.

## Test rationale notes

- `a_not_ready_retry_does_not_consume_the_zone_entry`: the zone entry
  is what buys the drive the 30s cooldown, and the whole point of that
  constant is the firmware fast-fail wedge. The BU40N's documented
  bad-sector signature IS a NOT_READY, so on the discs this matters
  most for, the first error must not spend the transition on a 3s
  retry while the genuine hard error that follows only gets the
  ordinary 5s pause.
- `damage_window_fills_then_jumps`: this test used to build its ctx
  with `for_sweep`, which sets `fast_jump_threshold = 1` — so the very
  first error jumped via the fast path and the loop broke on iteration
  0. `damage_window_max` and `damage_threshold_pct` were never
  consulted: replacing the whole `window_trigger` expression with
  `false` left this test (and all others in the module) green. The one
  test named for the damage window was pinning the path that bypasses
  it.
- `the_long_streak_escalation_fires_at_its_threshold`: nothing could
  see the long-streak escalation until `long_pause_escalations` existed
  — `CONSECUTIVE_FAIL_LONG_PAUSE_SECS` is deliberately the same 5s as
  `FAIL_PAUSE_SECS`, so the branch returns a value indistinguishable
  from the ordinary path, and deleting it outright used to leave the
  whole suite green.
