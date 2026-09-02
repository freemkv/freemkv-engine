# `speed.rs` internals: why two rates, the display window curve, and the caps

See `docs/sink-progress-eta.md` for why the engine computes speed/ETA at all
instead of each front-end re-deriving it. This doc covers the internals of
how `speed.rs` does it.

## Two rates, on purpose

This estimator deliberately produces **two different** throughput figures,
because "how fast is it going *right now*" and "when will it *finish*" are
different questions and a single rate answers neither well:

- `SpeedEstimator::observe` → the **displayed** speed: a sliding window
  (`display_window_secs`, adaptive 10 s → 60 s, or a fixed 10 s in
  `responsive` mode for bursty patch passes). Responsive enough that a stall
  visibly dips it, smooth enough that steady-state jitter doesn't.
- `SpeedEstimator::eta_speed_mbs` → the **ETA** rate: a long running average
  (bytes ripped this pass / elapsed this pass). Barely moves through a
  transient stall, so the ETA doesn't whipsaw.

This split is a paid-for lesson (2026-05-08: a 12 s drive stall made a
shared-rate ETA jump 1:30:00 → 30:00:00 mid-rip). It was proven in autorip
and promoted here so every front-end inherits it.

## Display-window growth curve

`display_window_secs` adapts the sliding-window size to elapsed pass time:

- `0..STATIC_PHASE_SECS` → fixed at `STATIC_WINDOW_SECS` (10 s). Early in a
  pass we have little history; a small window stays responsive while the ETA
  hasn't settled yet.
- `STATIC_PHASE_SECS..STATIC_PHASE_SECS+GROWTH_PHASE_SECS` → linear growth
  from `STATIC_WINDOW_SECS` to `MAX_WINDOW_SECS` over `GROWTH_PHASE_SECS` of
  elapsed time. Smooths progressively as we accumulate enough samples for a
  longer window to be reliable.
- `STATIC_PHASE_SECS+GROWTH_PHASE_SECS..` → fixed at `MAX_WINDOW_SECS`
  (60 s). Steady-state averaging window.

Resulting schedule (1.5 s callback ⇒ ~40 samples in a 60 s window):

```text
  t+ 30 s → 10 s window
  t+ 60 s → 10 s window (start of growth phase)
  t+210 s → 35 s window
  t+360 s → 60 s window (cap reached)
  t+1 h  → 60 s window
```

## `ETA_WARMUP_SECS`

Minimum elapsed time before the pass-start running average is trustworthy
enough to use for ETA. Below this the running average is noisy (small
denominator, first-sample artefacts) so we fall back to the displayed speed.
10 s ≈ a few callbacks at the typical throttle, enough to settle.

## `MAX_PLAUSIBLE_MBS`

Sanity cap on any computed MB/s. Real optical drives top out around
70–140 MB/s; ≥ 1 GB/s would be a measurement artefact (clock jitter, mapfile
replay, a resumed disc's already-copied bytes counted in one interval). Drop
rather than display — this is the guard that keeps "8000 MB/s" off screens.

## `sample_derives_from_the_real_clock_not_a_constant` test

`sample` — not `sample_at` — is what `run.rs` and `mux.rs` actually call on
every progress tick, so it is the function whose whole body a mutation run
could replace with a constant tuple. Every other test in this module drives
the injectable `sample_at`, which left the real-clock wrapper completely
unconstrained.

It needs the real clock to have moved between two `sample` calls, which used
to be arranged with `thread::sleep(60ms)`. Sleeping is the wrong primitive
for "the clock ticked": it over-delivers (the assertions need one tick, not
60 ms), it costs 80 ms on every run of the suite in both profiles, and it
states the requirement in a unit the test does not care about.
`wait_for_the_clock_to_tick` expresses exactly the precondition —
`Instant::now()` returns a later value than it did — and returns as soon as
that is true.

## Boundary-comparison tests (round-6 mutation run)

Three tests pin comparison operators (`<` vs `<=` vs `==`) that a mutation
run found unpinned:

- `a_flat_byte_count_does_not_re_anchor_the_pass_clock`: `observe`'s
  re-anchor guard is `bytes_done < start_bytes`. Widened to `<=`, a stall at
  exactly the pass-start byte count — two ticks before the first byte moves,
  which is ordinary on a slow spin-up — re-anchors the running-average clock
  on every such tick. Invisible in the display speed (a stall reads 0 either
  way) but silently shortens the ETA denominator once progress resumes.
- `the_window_cutoff_is_inclusive_of_a_sample_on_the_boundary`: every other
  pruning test advances in whole seconds against a whole-second window, so a
  sample lands on the cutoff constantly and `<`/`<=`/`==` are
  indistinguishable by accident. This one is built so the answer differs.
- `eta_leaves_warmup_exactly_at_the_boundary`: pins that at exactly
  `ETA_WARMUP_SECS` the running average is already in use, not the display
  fallback.
