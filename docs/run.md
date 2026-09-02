# `run.rs` cancellation and watcher design notes

## `SignalDone` — why every scoped watcher must use it

A plain `done.store(true)` placed after the watched call is skipped by a
panic unwind, and `std::thread::scope` joins its threads BEFORE
propagating a panic — so the watcher keeps looping on a flag nobody will
ever set, the join never finishes, and a panic becomes a permanent hang.
For a service holding a drive open that is strictly worse than the crash
it replaces.

Every scoped watcher in this crate must hold a `SignalDone` rather than
storing the flag by hand. Two hand-rolled copies of that pattern existed
and both had the bug; this type is the single place the rule lives.

## `with_cancel_watcher` — why cancellation needs a halt token, not just the progress callback

Cancellation needs BOTH channels, not just the progress callback.
`libfreemkv::progress::Progress::report` returns `!should_cancel()`,
which stops the library — but only on a tick, and a damaged disc
produces no ticks while it is sitting in a cooldown
(`ReadAction::Retry { pause_secs }`: 3 s on a NOT READY retry, 30 s on
zone entry). With no halt token the user's Stop went unheard for the
whole pause, so the button looked broken exactly when the rip was going
badly and someone most wanted to abort it.

`sleep_secs_or_halt` polls the token every 100 ms, so wiring one bounds
Stop latency at ~100 ms regardless of pause length. The watcher is the
same `should_cancel` → halt bridge `mux_with_input` and `extract_tree`
already use, and the scope joins it before returning.

## `ProgressBridge` — why it exists and why it's `pub(crate)`

Bridges libfreemkv's low-level `libfreemkv::progress::Progress` callback
onto the engine `Sink`. Recovery calls `report(&PassProgress) -> bool`
frequently; this translates each tick to a `Progress` and forwards it,
returning `!sink.should_cancel()` so the library's cooperative-
cancellation bool (`false` = stop) is driven by the front-end's
Cancel/Ctrl-C exactly as it is today.

It is `pub(crate)` so `crate::multipass::multipass_rip` can reuse the
exact same bridge — one speed/ETA derivation, shared by every caller
that drives a recovery primitive directly.

## Test: the watcher, not the check-before-starting

`with_cancel_watcher` does two things: it asks once before the work
begins, and it polls on a thread while the work runs. The existing
`cancel_via_sink_halts_recovery` uses a sink that is cancelled from the
very first call, so the entry check alone satisfies it — and a mutation
run duly deleted the `!` from the watcher's loop condition
(`while !done` -> `while done`, so the loop body never runs) with the
suite still green. A cancel arriving AFTER start would then be missed
entirely. `a_cancel_raised_after_the_work_starts_is_still_observed`
uses a sink that stays uncancelled for the first few polls, so only a
live watcher can set the halt flag.

## Test: `sectors_bad` must count damage, not un-swept territory

The bridge used to derive `sectors_bad` from `bytes_pending_total`,
which folds in NonTried — the part of the disc Pass 1 has not reached
yet. On the first progress tick of a flawless disc that reported
essentially the whole disc as bad sectors, with the number falling as
the sweep advanced. An operator watching a damage counter start at
twelve million and count down has no way to tell a pristine disc from a
failing one.

The companion test, `sectors_bad_converts_bad_bytes_into_a_sector_count`,
uses a nonzero mix (4096 unreadable + 2952 retryable = 7048 bytes,
deliberately not a multiple of 2048) because an all-zero `PassProgress`
lets `/`, `%` and `*` all agree on `0`, so it can't distinguish a correct
conversion from a broken one. The number this produces is the
bad-sector count an operator reads off the progress line while deciding
whether to stop a failing rip.

## Test: a panic inside the watched call must propagate, not hang

`with_cancel_watcher` sets `done` after `f` returns. On a panic the
unwind skips that store, and `thread::scope` joins its threads before
propagating — so the watcher, still looping on `done`, is joined
forever and the panic becomes a permanent hang. A ripping service that
would have crashed and restarted instead sits there holding the drive.

The failure mode is a deadlock, so the test cannot simply call and
assert: a regression would hang the whole test binary rather than fail
it. The call runs on its own thread and the assertion is on a receive
timeout.

## Test: Stop must be honoured during a damage cooldown, not after it

The recovery cooldowns (`ReadAction::Retry { pause_secs }`) are 3 s for
a NOT READY retry and 30 s on zone entry. Cancellation reaches the
library two ways: the progress callback's return value, which is only
consulted when a tick happens, and the halt token, which
`sleep_secs_or_halt` polls every 100 ms. A damaged disc produces no
ticks while it is sleeping, so with no halt token wired the user's Stop
sat unheard for the full pause — up to 30 s of a button that looks
broken.

The test's reader fails every read with the BU40N bad-sector signature
(NOT READY / 0x04 / 0x3E), so the first batch enters a cooldown
immediately. The assertion is wall-clock: with the halt token wired the
call returns in well under one pause.
