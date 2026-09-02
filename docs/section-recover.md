# `recovery/section_recover.rs` — design rationale

## Module overview (why handler chains instead of one grind loop)

The pre-existing patch loop grinds one bad range end-to-end, and when the
drive wedges it aborts the WHOLE pass — so a dead cluster at the *front* of
a range starves every later range of any attempt. This module replaces that
with a chain of time-bounded recovery *handlers*, each a single recovery
*idea* (read backwards, forwards, fast, slow, bisect...). A coordinator runs
them in sequence over one section's still-bad sub-ranges:

- each handler gets a hard wall-clock `deadline` and MUST return promptly
  once it passes — no handler ever blocks unbounded (that is the whole
  point);
- a handler recovers what it can, shrinking the shared `SubRanges` via
  `SubRanges::remove`, and returns `HandlerOutcome::Remaining` with the
  rest still bad — the NEXT handler then tries a different idea on what is
  left;
- whatever is still bad after every handler is the residue the caller
  records as loss (NonTrimmed) before MOVING ON to the next section.

Adding a new recovery idea is one new `SectionHandler` impl pushed onto the
chain — nothing else changes.

This module is deliberately decoupled from the live `patch` machinery
(`PatchSink`, `PatchItem`, mapfile locks): recovered bytes flow through the
tiny `RecoverySink` trait, and the clock is injected as `&dyn Fn`, so every
handler and the coordinator are unit-testable against a synthetic
`SectorSource` with a fake clock — no live drive, no real sleeps.

Wired into `recover_section` (#55): `run_handlers` is the live Pass-N
recovery engine. `SubRanges` stays the shared still-bad set.

## `UNPRODUCTIVE_YIELD`

Early-yield threshold: after this many consecutive reads that recover
NOTHING, a handler hands the still-bad set to the next handler instead of
grinding out its whole time budget on a dead zone. The baton comes back — a
later handler, or the next pass, retries the same sectors from a different
angle / after the drive state has shifted (recovery is stochastic). This is
what turns a "60 s of 0 B/s" stall into a fast hand-off.

## `WEDGE_ABORT_STREAK`

Wedge abort: after this many CONSECUTIVE wedge-family senses (Hardware /
IllegalRequest — the BU40N firmware's fast-fail state, where it rejects every
CDB in <100 ms without attempting recovery) the drive is wedged. `read_span`
escalates the read to `Transport`, which every handler propagates as
`TransportFault` → the whole pass aborts and the caller spin-cycles the
drive instead of hammering all remaining sections (which only deepens the
wedge). Any Good read or non-wedge (medium-error) read resets the streak, so
only a sustained fast-fail run — never scattered bad sectors on real media —
trips it. Counted at the PASS level (persisted across sections) so a wedge
is caught even when every bad sub-range is smaller than the streak. Learned
the hard way (2026-07-01): the handler chain ground a wedged drive for
28 min at 0 B/s because a fast-fail sense was classified as an ordinary bad
sector.

Detection latency scales with how much streak one section can build. Tier
0's 4 handlers × `UNPRODUCTIVE_YIELD` = 16 reads, so a single large wedged
section trips it within one `run_handlers` call. Tier 1 has only 2 handlers
(max 8 per section), so a wedge seen only in tier 1 relies on the
pass-level streak PERSISTING across sections to reach the threshold —
regression-tested by `wedge_streak_persists_across_sections_for_tier1`.

## `WEDGE_FASTFAIL_MS`

A wedge-family failure only counts toward `WEDGE_ABORT_STREAK` if it came
back faster than this — the fast-fail wedge rejects a CDB in <100ms with no
recovery attempt, whereas a genuine uncorrectable sector on Hardware-error
media spends real time on ECC recovery before failing. Gating on latency
stops slow, real damage that happens to report a Hardware sense from
false-tripping the wedge abort. Generous (500ms) so a slow bus adds margin
without admitting a true fast-fail.

## `Bisect` handler

Bisect + expand. Probe the middle sector of a bad sub-range; when it reads,
EXPAND outward from it — forward and backward in full batches — until a
read fails, recovering the whole readable island around the good centre in
large reads. The two failing ends become smaller bad sub-ranges, pushed
back to be bisected again. A dead middle just splits into halves. This
shreds one huge bad range into precisely-located small dead clusters (a
handful of sectors) instead of leaving the whole thing bad. `params` is
normally fast reads: it LOCATES readable data; deep-recovering the dead
sectors is the slow linear handlers' job. Tier 2 also runs a Bisect at
FUA/deep params to shred islands under cache-bypass.

## `Jump` handler

Blow through a LARGE dead run fast. Reads forward in batches; after
`JUMP_AFTER_FAILS` consecutive failed batches it SKIPS AHEAD to the MIDDLE
of what is left of the range (`remaining / 2`, sector-aligned, minimum one
sector), leaving the skipped span bad, to find where readable data RESUMES
— mirroring the Pass-1 damage-jump.

Halving, not an escalating fixed distance. There is deliberately no cap:
the jump is a FRACTION of the remaining span, so it can never overshoot the
range, and a big dead run is crossed in ~log2 jumps. (An earlier version of
this doc described a `1 MiB → 2 → 4 …` escalation "capped at
`JUMP_CAP_BYTES`" — an identifier that appears nowhere else in the crate,
describing a jump rule the fixed 8 MiB version was replaced BECAUSE it
leapt clean over ranges smaller than itself.) A later handler / `Bisect`
pins the exact good/bad boundary the jump stepped over. Uses fast reads
(this is a scout, not a deep-recovery pass). Without it a linear walk pays
one up-to-10 s read per dead batch across the whole run, so a
deadline-bounded pass never reaches readable data buried behind a big dead
front (exactly the 192 MB range seen on a real disc).

## `SpeedSweep` handler

Per residual sector, try Max→Min spindle speeds until one reads. Failure
mode: speed resonance — the best speed is NOT always the slowest; some
marginal sectors hit a read-channel sweet spot at a higher speed, so a
per-sector search beats committing to min. Distinct from SlowSpin (a
`Linear` pinned to min): this searches. `params` carries the FUA / timeout
axes; the speed axis is what it sweeps. Single-sector, so it runs on the
true residual.

## `CachePrime` handler

Before reading a residual island, read the good run immediately PRECEDING
it to lock the drive's PLL/servo, then read the marginal sectors while the
channel is warm. Failure mode: a boundary sector the drive can't lock onto
from a cold seek (ddrescue's "back up, run forward"). The priming read is a
normal wedge-safe `read_span`; if the preceding sector is itself bad the
prime just fails and the island is read cold (no worse than Linear).

## `Oscillate` handler

Read each residual sector by ALTERNATING approach: forward-into (prime
from the sector below, then read) and reverse-into (prime from the sector
above, then read). Failure mode: direction-dependent tracking — a sector's
servo lock differs by approach direction, so it may read one way but not
the other. Combines the two Linear directions into a per-sector alternation
on the true residual. `params` carries the speed / FUA / timeout axes; the
alternation is the direction axis.

## `SCORE_EWMA_ALPHA`

EWMA smoothing factor for the decayed recovery rate. Each new attempt is
weighted `α`, the running average `1-α`, so a handler's score tracks its
RECENT performance and forgets its distant past at a rate set by `α`.
Higher = more reactive (leadership flips sooner); lower = steadier. 0.5
halves the weight of the previous score on every attempt — reactive enough
that a proven early winner whose territory is exhausted decays out of the
lead within a few barren attempts, while a late-starting specialist climbs
as it earns.

## `HandlerScoreboard`

Per-rip handler scorecard. Grades each handler by a DECAYED recovery rate
(an EWMA of bytes-recovered-per-second, `SCORE_EWMA_ALPHA`) so the
coordinator runs whoever is winning *now* first on later sections. The
residual shrinks and hardens mid-pass, so the best technique CHANGES: fast
scouts clean the range-fronts, then the leftovers are exactly the marginal
sectors where specialists win — and the ranking must FLIP. A cumulative
rate froze the early winner in the lead forever; the EWMA re-prices
continuously. Ephemeral — reset each rip, no persistence. A handler not
yet tried ranks top (`u64::MAX`) so every handler is calibrated once
before the ranking narrows.

## `HandlerScoreboard::record`

Records one attempt: `recovered` bytes over `elapsed`. A timed attempt
decays a fresh bytes/second sample into `ewma_rate` — a barren attempt
(recovered = 0) contributes a 0 sample that decays the score DOWN, which is
exactly what lets an exhausted early winner lose its lead. A zero-elapsed
call (handler yielded before any timed read) contributes no rate sample.

## test: `read_span_refuses_an_empty_span_as_a_failed_read`

`read_span` must refuse an empty (`count == 0`) span as a FAILED read in
EVERY build — not just debug. The round-2 fix replaced a `debug_assert!`
(compiled out under `--release`, which this crate's CI runs) with a
runtime guard. The mutation this pins is "let a `count == 0` span reach
the reader": there `bytes == 0`, `recovery_read` answers `Ok(0)`, the
`Ok(n) if n == bytes` arm sees `0 == 0` and fires the *Good* path, and
`sink.recovered(pos, &buf[..0])` marks a NEVER-READ span as recovered. The
guard must instead return `Bad` and touch neither the sink nor the reader.
`pos == 0` is sector-aligned and (in `Harness`, no dead sectors) perfectly
readable, so ONLY the `count == 0` guard can keep this span out of the
Good arm.

## test: `linear_reverse_recovers_a_batch_forward_cannot_approach`

Pins `Linear`'s DIRECTION axis, which nothing else exercised.
`coordinator_drains_the_readable_set_through_the_chain` is named for
direction but cannot see it: its section is 16 sectors against a
`BATCH_SECTORS` of 32, so forward and reverse each issue ONE identical
read of the whole section, both fail, and every recovered sector in that
test comes from `Bisect`. Swapping the two `Direction` arms there — or
making `Direction::Reverse` walk forward — changed nothing.

Here the section spans three batches and the middle one holds a sector the
drive only reads when the head arrives from ABOVE (the `dir_reverse_only`
model, the same one `Oscillate`'s test uses). Forward must leave that
batch behind; reverse must clear the section.

## test helper: `oscillate_halt_after`

Drives `Oscillate::recover` over a two-sector sub-range (lba 5 dead, lba 6
readable) with the halt flag flipping right after the `flip_after`-th
underlying read completes, and returns `(outcome, total reads performed)`.

lba 5 dead means BOTH the forward-into and reverse-into target reads fail,
so a fully unchecked Oscillate burns all four reads on it (prime-below
lba4, target lba5, prime-above lba6, target-reverse lba5) before the
top-of-loop check (ahead of lba 6) can ever catch the flag. Flipping the
flag after read N and asserting the handler stops at EXACTLY N reads pins
down each of the three inter-read gaps (after prime-below, after the
forward target, after prime-above) individually — a check missing from
just one of those gaps lets the handler run one read past where it should
have stopped.

## test helper: `speed_sweep_halt_after`

Drives `SpeedSweep::recover` over a two-sector sub-range (lba 5 dead, lba
6 readable) with the halt flag flipping right after the `flip_after`-th
underlying read completes, and returns `(outcome, total reads performed)`.

lba 5 dead means BOTH speeds fail on it, so the sweep spends its full
Max→Min pair there before the top-of-loop check (ahead of lba 6) can see
the flag. Flipping after read 1 and asserting the handler stops at
EXACTLY 1 read pins down the single inter-read gap — the one between the
Max read and the Min read, each of which is a full `TimeoutPref::Deep`
(60 s) recovery read on the tier-2 roster `build_tier_handlers` builds.

## test: `oscillate_never_leaves_a_sector_it_recovered_in_the_bad_set`

A sector whose bytes the sink has ALREADY been handed must never be left
in the residual bad set.

`read_span`'s contract is explicit: on a Good read it hands the bytes to
the sink and "does NOT touch the still-bad set — the caller removes
recovered spans". Oscillate discharged that for its two TARGET reads and
not for its two PRIME reads — and a prime is not always outside the
residual: priming above `pos` reads `pos + SECTOR`, which is the next
still-bad sector whenever the sub-range is 2+ sectors long (priming below
is symmetric on the sector just processed).

So the prime lands the sector, `RecoverySink::recovered` writes its real
bytes and marks it Finished in the live wiring — and then, because it is
still in `bad`, `recover_section` emits `PatchItem::NonTrimmed` over it on
the final tier. `Mapfile::record` is last-writer-wins, so the sector is
downgraded to damaged, the next-pass promotion turns it into `Unreadable`,
and the rip reports permanent loss for bytes that are sitting correct in
the ISO — loss that can fire the abort-on-loss gate and refuse a rip that
is actually complete.

Sector 1 is dead; sector 2 is fine. Oscillate primes above sector 1, which
reads sector 2, and the per-handler budget then expires before sector 2's
own turn comes round.

## test: `oscillate_does_not_prime_past_the_end_of_the_disc`

Oscillate must not prime from past the end of the disc.

The reverse-into approach reads the sector ABOVE the target first. For a
residual sector in the LAST sector of the image that is `capacity_sectors`
— one past the end. A drive answers ILLEGAL REQUEST, which is a
wedge-family sense, so the read lands in `wedge_streak` and the pass can
abort on a wedge that never happened; a file-backed source answers with an
IO error, which reads as a transport failure and kills the pass outright.
The forward-into prime has had the symmetric `pos >= SECTOR` guard all
along.

## test: `prime_above_is_in_range_treats_an_unknown_capacity_as_no_bound`

Pins `prime_above_is_in_range` at its three boundaries. Named for the
helper, because the helper is all it drives: it was called
`oscillate_still_primes_from_above_when_the_capacity_is_unknown` and never
built an `Oscillate` or called `.recover(..)`, so a regression in how
`Oscillate` CALLS this predicate (wrong argument order, called under the
wrong condition) was never in its reach. That end-to-end claim is covered
where it can actually be observed — flipping the unknown-capacity answer
below to `false` turns
`oscillate_recovers_a_direction_dependent_sector_forward_linear_misses`
(whose `FakeDisc` reports capacity 0 and whose target sector reads ONLY
when approached from above) from Complete to Remaining.

## `SPEED_MIN_KBS`

Min read speed (~DVD 1×; the drive clamps up to its own supported minimum).
Slower rotation gives the servo more dwell and the ECC engine more
integration time per sector — the SlowSpin / SpeedSweep lever. The exact
value only has to be well below max; the drive rounds it to a supported
step.
