# `recovery::patch` producer / consumer split

## Why the split exists

Pre-0.18 patch ran strictly serial — single-sector recovery read → seek +
write recovered bytes → mapfile.record → next iteration. The drive sat idle
while the previous block's recovered bytes were committed. On a damaged disc
with many bad sectors that adds up: per-sector write + mapfile.record costs a
handful of milliseconds each, which the drive could be using to issue the
next per-sector retry.

This module decouples them. A consumer thread owns the
[`libfreemkv::io::WritebackFile`] (the ISO file) and the
[`super::mapfile::Mapfile`]. The producer thread (`Disc::patch`) keeps the
[`libfreemkv::sector::SectorSource`], the wedge / damage-window state, the
per-range watchdog, decrypt — so what enters the channel is already-clean
cleartext bytes (or an "Unreadable" terminal mark).

Producer and consumer run concurrently; the channel uses
[`libfreemkv::io::pipeline::WRITE_THROUGH_DEPTH`] (=1) so back-pressure kicks
in immediately. We want the drive's per-sector retry budget to stay in
lockstep with the writer — sweep's `DEFAULT_PIPELINE_DEPTH` (4) would let
several sectors of recovered bytes queue up between the producer's retry
decisions and the writer, and patch's recovery loop reads stats
(`bytes_good`, range progress) inline to drive its skip / wedge decisions.
WRITE_THROUGH_DEPTH gives "read N+1 while writing N", no further pipelining —
exactly the model the producer logic was written against.

## Correctness invariants preserved

- Mapfile is single-writer (consumer-only). No locking on it.
- All recovery state (damage window, consecutive_failures, skip escalation,
  range watchdog) stays on the producer thread.
- `set_speed` calls happen on the producer thread (same thread that owns the
  `SectorSource`). No new SCSI concurrency.
- Per-iteration ordering of file-write → mapfile-record is kept intact in the
  consumer (write before record), so the on-disk invariant "mapfile only
  marks Finished what the file has received" survives a crash mid-pass.
- The BU40N+Initio bridge wedge concern is unchanged: only one SCSI command
  in flight at a time, error-path timing identical, no new retry logic. The
  threading primitive only overlaps the *write* with the *next read*; the
  per-sector single-shot read budget that the bridge wedge concern was
  originally about is untouched.

## `SharedPatchState`

Excludes `NonTried` (the unread remainder, not damage) — including it
inflated the live located drilldown (at-risk movie time + range count) with
unread sectors; excluding it matches the one-shot progress path.

The snapshot holds the DERIVED figures, not the raw damage list. It used to
republish the range list itself, capped at 8192 entries, on the stated
rationale that consumers "only sample the head of the list". That rationale
was false at the only consumer: `report_patch_progress` fed the capped Vec
into `bytes_bad_in_title` and `locate_ranges` as TOTALS, so past 8192
fragments the at-risk figures under-reported by whatever damage sat in the
dropped tail — and the "+N more" count under-reported too, since
`locate_ranges` derives it from the length of what it is handed.

Deriving here instead fixes that at the source and bounds the snapshot MORE
tightly than the old cap did: `locate_ranges` keeps the 50 largest ranges
(its own `MAX_LOCATED`) and reports the rest as `truncated`, so the
republished state is O(50) regardless of fragmentation while every total is
computed over the complete damage set. (The old cap never bounded the
allocation it claimed to bound either — `ranges_with` builds the whole Vec
before `truncate` shortens it.)

## `recovery_read`

On an AACS disc a mid-unit window (start or length not unit-aligned) is
widened to the enclosing aligned 3-sector unit, decrypted, and the
originally-requested window copied back out: the decrypting reader rejects
an unaligned read (`DecryptFailed`) and the sector would be abandoned
without the drive ever being asked. Units anchor at offset 0, so the widened
start is always unit-aligned. All recovery accounting upstream (pos,
block_bytes, dispatched lba/count) is unchanged — only the physical read
widens, so the cursor cannot desync. `fua` is a Pass-N marginal-sector lever
(see [`libfreemkv::sector::SectorSource::read_sectors_fua`]).

## `snap_to_sectors`

Nothing validated the "all offsets are sector multiples" invariant before
this existed: `Mapfile::load` has no alignment check, so an imported
ddrescue mapfile written with a 512-byte block size (`-b 512`) parses fine
and yields unaligned ranges.

Two things went wrong downstream without this:
  * an unaligned `pos` — `read_span` does `lba = pos / SECTOR`, reads the
    sector CONTAINING `pos`, then writes those 2048 real bytes at byte
    offset `pos` and records them Finished. A shifted write of genuine
    payload, marked good. Silent corruption.
  * a sub-sector length — `count = (span / SECTOR) as u16` truncates to 0,
    and a zero-sector read reports Good. (Harmless in the mapfile, since
    `record` ignores a zero-size entry, but it credits the handler
    scorecard for a recovery that never happened.)

Widening is strictly conservative: the extra bytes are re-read from the disc
and written with real data, so this recovers a 512-aligned mapfile rather
than condemning its fragments as permanently unreadable — which is what
rejecting them at load time, or failing the read here, would do.

## `build_tier_handlers`

Each config is named by its FULL parameterisation (`build_tier_handlers`
picks the roster; the scorecard re-orders WITHIN a tier per rip). The engine
hardcodes no conclusion: every technique is always present at its tier, and
a technique that doesn't fit this disc self-deprioritises (scores low,
yields after 4 unproductive reads) rather than being removed.

- **Tier 0 — fast scouts** (`fast`: max speed, 10 s, cache on): grab the
  readable bulk across every range.
- **Tier 1 — slow-deep** (`deep`: max speed, 60 s ECC budget): deep-recover
  the easy residual.
- **Tier 2 — marginal specialists**: the physical-failure-mode matrix
  (SlowSpin / FuaRetry / SlowFua / CachePrime / Oscillate / SpeedSweep), run
  ONLY on what tiers 0-1 leave.

## `build_flat_pool`

`run_handlers` sorts it best-first by the rip scorecard on every range, so
this is a data-driven bandit: the first ranges try them all (explore), then
the decayed-yield ranking floats whatever is actually landing sectors to the
front (exploit), re-measured per range. A handler that doesn't fit stays
last but is never dropped (floor — it can still revive if the residual's
character shifts). No fixed ordering, no "start tier" — the data picks the
order. Unset keeps the proven tier ladder.

## `flat_mode_from_value`

`std::env::set_var` is unsafe for a reason that a per-key mutex cannot
discharge: its condition is that no other thread is touching the
environment AT ALL, and this crate's test binary is full of siblings
calling `std::env::temp_dir()` (a read of `TMPDIR`) on cargo's other test
threads. A concurrent `setenv` may free the `environ` block that `getenv` is
walking. See `flat_mode_override` for the one test that needs the value to
reach a real `patch()` call.

## `handler_deadline`

`Instant + Duration` panics on overflow — it is `checked_add(..).expect(..)`
underneath — and `budget_secs` comes from an env var
(`FREEMKV_PATCH_FLAT_BUDGET`) that is floored at 1 but has no ceiling. A
19-digit value therefore took down the whole recovery pass with an
arithmetic panic, on the one code path whose entire purpose is to survive a
hostile disc. A misconfigured knob must degrade, not crash: an absurd budget
means "effectively no deadline", so it saturates to the longest deadline
this clock can express rather than exploding.

The cap is NOT applied to the knob itself: an operator who deliberately sets
a long per-handler budget on a badly damaged disc must still get it.

## `recovery_read_rejects_a_short_transfer_on_both_branches`

The mutation this catches: put back the bare `Ok(_)` / bare `?` that
`recovery_read` used to have on each of its two branches. `buf` here is
pre-filled with 0xAA — stale bytes standing in for the PREVIOUS span that a
real recovery handler's reused buffer would be holding — and the reader
writes only the first sector. With the guard removed, `recovery_read`
answers `Ok(bytes)` and `read_span` hands `ctx.sink.recovered` a span whose
tail is that stale filler, writing it to the ISO and recording it
`Finished`: a corrupt rip at rc=0. With the guard, a short transfer is a
failed read — the same `DiscRead` verdict `Drive::read_one` gives a residual
underrun on the live path — so the span stays bad and is retried.

Both branches are exercised because they fail differently: the plain branch
reads into the CALLER's buffer, and the AACS-widening branch reads into a
freshly-zeroed `scratch` and would splice zeros in.

## `PatchItem::Unreadable`

Currently unused by `Disc::patch` itself (2026-05-11 design call: patch never
marks `Unreadable` mid-multipass; bytes stay `NonTrimmed` so future passes
get another shot at them). Kept in the enum for the orchestrator-side
end-of-recovery promotion (autorip, after the final retry pass completes,
promotes still-`NonTrimmed` bytes to `Unreadable`). The orchestrator
(autorip) performs this promotion directly via `Mapfile::record()` after all
retry passes complete, not by emitting to `PatchSink`. This variant remains
unused by the library itself.

## `PatchItem::NonTrimmed`

CRITICAL: "NonTrimmed in pass N" does NOT mean "Unreadable forever." Drive
reads are stochastic: the same sector that fails 10 times in Pass 2 may
succeed on attempt 1 in Pass 3 after temperature / bus state / prior-read
patterns shift. Pre-2026-05-11 patch marked individual failures Unreadable,
which gave up on sectors that subsequent passes could have recovered
(historical: ~36% of patch-marked Unreadable sectors turned out to be
readable in re-rip experiments).

## Per-range watchdog staleness

Per-range watchdog (`range_sectors × SECONDS_PER_SECTOR`, capped at
`RANGE_BUDGET_CAP_SECS`) checks `bytes_good` for forward progress. With work
in flight on the consumer, the producer would otherwise see stale values; the
sink publishes a [`SharedPatchState`] snapshot after every record so the
producer's stall guards observe consumer side-effects with at most one item
of lag (which is fine — the watchdog uses minute-scale budgets, not
single-record latency).
