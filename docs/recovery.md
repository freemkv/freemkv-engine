# `recovery/mod.rs` — relocation notes and design rationale

## Relocation fidelity

This file (`copy`/`sweep_internal`/`patch_internal`/`sweep` + the option/
result types + `ecc_sectors`) is the relocated `impl Disc` multipass-dispatch
block from libfreemkv's `disc/` module. Behavior is unchanged — only the
receiver (`&self` -> `disc: &libfreemkv::Disc`) and crate-external paths
changed. `mapfile.rs`, `read_error.rs`, `section_recover.rs`, `patch.rs`, and
the private `sweep.rs` producer/consumer plumbing are unchanged in logic —
only crate-external references became `libfreemkv::`, and `Disc` methods
became free functions taking `&libfreemkv::Disc`.

## `require_full_read`

A SHORT transfer is a FAILED read, never a partial success.
`SectorSource::read_sectors` answers with the number of BYTES it actually
transferred, and every read site in this module ships `buf[..requested]`
onward out of a buffer that is REUSED across iterations. If a reader ever
answered `Ok(n)` with `n < requested`, the tail of that slice would still
hold the PREVIOUS LBA's bytes — and those bytes would be written into the
ISO and recorded `Finished`. That is the worst shape of failure this crate
has: a rip that reports rc=0 and a clean mapfile while carrying somebody
else's sectors inside the movie. (The same class already shipped once, as
~9 MB of ciphertext at rc=0.)

The live-drive path already makes exactly this call — `Drive::read_one`
gates its success arm on `bytes_transferred == count * 2048` and documents
"treat a short transfer as a failed read", answering the very
`DiscRead { status: None, sense: None }` reproduced here so the two paths
classify identically. The engine-side sites matched `Ok(_)` and discarded
the count, so they would have believed a short read.

No `SectorSource` in the tree can trip this today (`FileSectorSource` uses
`read_exact`; `Drive` gates as above; `PrefetchedSectorSource` *can* return
a short count but the engine never builds one). The trait itself neither
permits nor forbids it. This is therefore hardening, not a live bug fix: it
makes it impossible for a FUTURE reader implementation to corrupt output
silently, and it costs one comparison per read.

## `MAPFILE_CREATOR`

It must name the crate that actually wrote the file. `env!` expands in the
crate being compiled, so the literal `"libfreemkv v"` this used to be
concatenated with stamped libfreemkv's NAME onto freemkv-engine's VERSION —
a provenance line naming a crate/version pair that has never existed, in the
one artifact an operator reads to work out which build produced a rip.
Recovery has lived in this crate since 1.6.0.

Header TEXT only: no parser keys off it (`load` stores the remainder of the
line verbatim as `version`), so the on-disk format and its
ddrescue-compatibility are untouched.

## `aacs_aligned_batch`

Round a sweep's batch size up to a whole number of AACS aligned units.
`decrypt_sectors` anchors units at buffer offset 0, so every read handed to
the decrypting reader must span a whole number of units — otherwise units
straddle batch boundaries, decrypt under the wrong unit alignment, and the
verify gate either leaves the content encrypted or aborts `DecryptFailed`.
`ecc_sectors()` is 32 for UHD/BD and 32 is not a multiple of 3, so without
this every batch after the first would start mid-unit.

Pure and separate from `sweep_internal` because it is a real decision with
a real failure mode, and inside a function that needs a live drive nothing
could reach it: the mutation run replaced the `-` with `+` and the `%` with
`/` here and the suite stayed green either way.

## `aacs_aligned_region_start`

Anchor a region's read cursor DOWN to the nearest AACS unit boundary. A
resume `NonTried` region can begin mid-unit. `decrypt_sectors` anchors
units at buffer offset 0, so a read that STARTS mid-unit decrypts under the
wrong alignment. Re-reading the few head sectors is idempotent, so aligning
down is free. Sibling of `aacs_aligned_batch`, which handles the other half
of the same invariant (a whole number of units per read).

## `aacs_aligned_read_bytes`

Widen the PHYSICAL read of a region's LAST block out to whole AACS units.
The third corner of the same invariant, and the one that was missing.
`aacs_aligned_batch` makes a full batch a whole number of units and
`aacs_aligned_region_start` anchors the cursor to a unit boundary, but the
sweep loop takes `min(region_end - pos, batch * 2048)` — and `region_end`
is only SECTOR-snapped (`snap_to_sectors`), never unit-snapped. So the
final read of every region is a whole number of sectors that is generally
NOT a whole number of units: 6144-byte units straddle its top edge, which
is exactly what the two siblings exist to prevent (`decrypt_sectors`
anchors units at buffer offset 0, so a partial trailing unit decrypts
under the wrong alignment and the verify gate either leaves it encrypted
or aborts `DecryptFailed`).

Widening rather than shrinking: shrinking would leave the region's tail
sectors unread forever. `limit` (the disc's total size) caps the widening
so a region ending at the disc's end never reads past capacity — a disc
whose capacity is not a whole number of units keeps its unavoidable
partial tail unit, which is the pre-existing behaviour and not something
this can fix.

Only the READ widens. The caller still accounts, sends, and records
exactly `block_bytes`, so the extra bytes cannot shift the cursor or
re-status a neighbouring range — the same widen-read-copy-back-the-window
shape `patch::recovery_read` already uses for mid-unit recovery reads.

## `IsoLen` / `iso_len_from_metadata`

`NotFound` is the ONE error that genuinely means "no file yet" — the
fresh-rip case. Every other error is unknown, and destroying data on an
unknown is not a decision this code gets to make: an EIO from a flaky
USB/NFS staging volume or a momentary permissions problem is not evidence
that a populated ISO is empty, and treating it as zero sends the image
through `File::create` and permanently zeroes bytes the mapfile still
records `Finished`. The producer only builds work from `NonTried` ranges, so
those bytes are never re-read: silent, total loss of the recovery.

Lifted out of `sweep` because the classification was written TWICE in that
one function — once for the inconsistent-resume guard and once for the
open-vs-create decision — and welded to a live `std::fs::metadata` call in
both places, which is why neither copy could be tested. Two copies of one
policy is exactly the shape that drifts.

## `ImageState` / `image_state`

ONE definition, used by `copy`, `sweep` AND `patch`. It used to be written
out at each call site — `copy` compared for equality, `sweep` for "shorter
than", and `patch` did not check at all. That third gap was the dangerous
one: a truncated staging image could be patched and reported as a whole good
disc, because the pass only walks the mapfile's bad ranges and takes
`bytes_good` from its stats, so every Finished range beyond the truncation
is a hole nothing ever re-reads.

`image_state`: a stat failure other than "not found" is an error, never
silently 0 — a transient stat failure once threw away a good resume and
re-ripped hours of work.

## `output_is_regular`

Whether the output is a REGULAR FILE, and therefore whether a `sync_all`
failure on it is a real error and whether it should be pre-sized. `/dev/null`
and pipes answer their metadata successfully and report not-a-file, so they
map to `false` correctly and always did. The default only fires when the
`metadata` call ITSELF fails — a transient NFS ESTALE on the staging volume —
and there the two copies of this decision had drifted to OPPOSITE answers:
`patch` defaulted to `true` and argued in its comment that surfacing the
error is the right side for a data-integrity guard, while `sweep` defaulted
to `false`, which silently skipped the pre-size AND made `SweepSink::close`
swallow a genuine `sync_all` failure on the just-written image — the exact
two failures the comment above sweep's call site says must not happen.
Unified on `patch`'s answer.

## `SEND_DEADLINE`

Reuses `libfreemkv::io::pipeline::JOIN_TIMEOUT_SECS` (600 s) — the budget
`Pipeline::finish_with_halt` already gives the same consumer to drain at
join — rather than inventing a second number. A producer that gave up sooner
than the joiner is willing to wait would abort passes the joiner still
considers healthy.

It is deliberately enormous relative to a real write: one item is at most a
batch of sectors (64 KiB on sweep, one recovered span on patch), so even a
pathologically slow NFS mount doing a few KiB/s clears it in seconds. The
deadline is only the no-halt backstop; responsiveness to Stop comes from the
halt poll inside `libfreemkv::Pipeline::send_with_halt`, which ticks every
`libfreemkv::halt::POLL_INTERVAL` (250 ms) regardless of this value.

## `SendStall`

Why a send failed, for producers that must tell "the operator pressed Stop"
apart from "the consumer died" apart from "the consumer is alive but has not
drained a slot in `SEND_DEADLINE`". `libfreemkv::Pipeline::send_with_halt`
collapses all three into `Err(item)` — it hands the item back and leaves the
diagnosis to the caller. Mapping them all to `PipelineConsumerGone` would
report a Stop, and a hung mount, as a dead consumer thread.

## `send_bounded`

The defect this exists for: a plain `Pipeline::send` on a bounded channel
blocks forever when the consumer is ALIVE but STALLED — e.g. wedged inside
`WritebackFile::write_all` on a hung NFS mount. The producer polls its halt
token only at the top of its loop, so a Stop issued while it is parked in
`send` cannot land at all. `SendError` only ever reports the consumer being
DEAD, which is a different failure.

Shape: try once without blocking, and only if there is no room does the halt
get a vote. `Pipeline::send_with_halt` alone would drop a handoff that could
not have blocked, which discards bytes the drive already recovered (see
`a_raised_halt_still_delivers_when_the_channel_has_room`).

HONEST LIMIT: this gets the PRODUCER out — the pass stops reading the drive
and the run reports `halted` — but the consumer thread is still inside its
write, so the subsequent `Pipeline::finish` join still blocks until the
mount answers. This buys responsiveness and a correct diagnosis; it does not
cure a D-state hang.

## `finish_bounded`

Halt-aware teardown for the recovery pipelines — the join-side sibling of
`send_bounded`, and the other half of the same guarantee. `send_bounded`
gets the PRODUCER out from under a consumer that is alive but not draining.
It does not get the CALLER out: the very next statement after the producer
unparks is the teardown, and a plain `Pipeline::finish` is a blocking
`join()` on that same stalled consumer. So a Stop that `send_bounded`
finally made land still could not return — it moved the wedge from `send`
to `finish` rather than removing it. `Pipeline::finish_with_halt` is the
bound libfreemkv ships for exactly this, and both recovery sites used the
non-halt-aware sibling.

Semantics come straight from `finish_with_halt`, and both matter here:

* Clean drain — identical to `finish`: the consumer's `close()` output is
  returned unchanged. `finish_with_halt` polls `is_finished()` BEFORE it
  looks at the halt, so even a run that ends halted still joins normally and
  returns its summary as long as the consumer is healthy. The halt path is
  reached only by a consumer that is genuinely not coming back.
* Wedged consumer plus a raised halt — a `FINISH_GRACE_SECS` (5 s) spin
  first, so a consumer that is merely slow still joins cleanly; only past
  that is the thread abandoned and `Error::Halted` returned.

MAPFILE DURABILITY, since the consumer owns it and abandoning skips
`close()` (which is where `map.flush()` lives): abandoning is still the
right trade.

* The abandoned consumer is detached, not killed, so it is still holding the
  pass's `Mapfile` when its wedged write finally returns — and it would then
  write it, from a snapshot that is now stale, over whatever a resumed pass
  has since persisted. A sink that owns a `Mapfile` must therefore tear down
  through `finish_bounded_disowning`, NOT through this function, which
  revokes that mapfile on a failed teardown. What is given up by revoking it
  is the abandoned thread's last `FLUSH_INTERVAL` of records, and only in
  the pessimistic direction.
* If the process exits before that, the loss is bounded by the mapfile's
  one-second `FLUSH_INTERVAL`, and it is loss in the SAFE direction. Both
  sinks write the file BEFORE they `record()`, so a lost tail of records can
  only make the mapfile more pessimistic than the ISO — ranges get re-read
  on resume. It can never mark Finished a range the ISO did not receive,
  which is the only direction that would corrupt a resume.
* The dangerous shape `ABANDONED` exists to stop — a leaked consumer
  finalising an output the caller already reported failed — does not arise
  here: neither recovery `close()` does anything structural, only
  `sync_all` + `map.flush()`, both idempotent.
* And the status quo is WORSE for durability, not better. A blocked `finish`
  never returns, so the consumer thread never unwinds, never drops the
  `Mapfile`, and never flushes at all. A hang preserves nothing.

## `finish_bounded_disowning`

`finish_bounded` for a pipeline whose sink owns the `Mapfile`: on any failed
teardown it DISOWNS that mapfile. Both recovery pipelines must use this, not
the bare `finish_bounded`.

The hole it closes: abandoning is detaching, not killing. The leaked
consumer is still holding the pass's `Mapfile`, and when its wedged write
finally returns it goes on to `record()` (whose `FLUSH_INTERVAL` check has
long since elapsed, so it writes immediately) and then drops the sink, whose
`Mapfile::drop` flushes again. Both rewrite the WHOLE file from a snapshot
taken before the abandonment. Meanwhile the caller has had the `Err` back
and — for a Stop, the single most likely next action, and for a wedge the
action this crate's own docs tell the operator to take — may already have
resumed the rip against that same path with a second, independent
`Mapfile`. Nothing serialises two `Mapfile`s on one path (they even share
the `<path>.tmp` staging name), so the abandoned thread's write can land
last and silently revert sectors the resumed pass confirmed `Finished` —
minutes of recovery off damaged media, thrown away with no error anywhere.

Raised on ANY `Err`, not just the leak errors, because on every OTHER error
path it is a provable no-op: those all come back through `handle.join()`,
which only observes a thread that has already terminated, so the sink — and
the `Mapfile` inside it — was dropped and flushed before this function
returned. That keeps the rule free of any dependence on which `Error`
variant libfreemkv uses for a leak, including the close-already-in-flight
leak, which has no distinct variant at all.

What it costs: an abandoned consumer no longer lands its last
`FLUSH_INTERVAL` (1 s) of records. That is loss in the SAFE direction — both
sinks write the image BEFORE they `record()`, so a mapfile missing a tail of
records is merely pessimistic and those ranges get re-read on resume. It can
never mark `Finished` a range the image did not receive.

## `PatchOptions::for_patch_pass`

Both entry points — `patch_internal` (copy's resume dispatch) and
`multipass_rip`'s patch loop — used to spell these four values out as
literals, so the two routes into the same underlying pass could drift apart
on a future tuning change with nothing to catch it. Only one of the two
copies even carried the rationale for `block_sectors`.

NOTE on `block_sectors: Some(32)`: it no longer sizes any read. The adaptive
32→1→32 batching this comment used to describe was replaced by the handler
chain (`section_recover.rs`), which owns read sizing and bisection. What
survives is the pass LABEL — >1 reports a Trim pass, 1 reports a Scrape
pass. Likewise `full_recovery` is now diagnostics-only and
`wedged_threshold` is reported in the outcome, not enforced. See
`patch_preset_tests` for what each value actually does.

## `snap_to_sectors`

Mapfile ranges are BYTE ranges and nothing guarantees they land on
2048-byte boundaries — the format interoperates with ddrescue, whose
`-b 512` writes 512-byte-granular ranges, and a mapfile can be hand-edited
or imported. Feeding an unaligned offset to a sector-addressed reader
truncates the LBA and shifts real payload to the wrong place, which then
gets recorded `Finished`: silent corruption presented as recovery.

`patch` has always snapped its ingress; `sweep`'s resume path did not, and
its only alignment was gated on a decrypting AACS rip — a branch a
multipass resume never takes, since multipass implies raw. One
implementation here so the two ingresses cannot drift apart.

`(pos + len).div_ceil(SECTOR) * SECTOR` overflows u64 for a range ending in
the last sector of the address space, which `Mapfile::load` accepts (it
checks `checked_add`, and that does not wrap). The old fix saturated the
END of that computation but not the LENGTH derived from it, so a range this
close to `u64::MAX` came back with a length that was not a multiple of
`SECTOR` (2047 instead of 0) — silently breaking the one invariant every
recovery handler relies on (`count = len / SECTOR` truncates to 0,
producing a zero-sector read that is reported as a successful recovery of
bytes nobody read).

Do the rounding-up arithmetic in u128 so `pos + len` can never overflow,
then cap the result at the largest sector-aligned value a u64 can hold.
There is no way to represent the true end of a range whose last whole
sector runs past `u64::MAX` (its top edge is `2^64`, not representable), so
that case is capped down to the last representable sector boundary — which
here coincides with `start` itself, correctly yielding a length of 0 rather
than a fabricated sub-sector remainder. Every caller (`Linear`/`Jump`/
`CachePrime` via `SubRanges::from_section`, and the sweep loop's
`while pos < region_end`) already treats a 0-length range as "nothing to do
here", so this stays safe: never a phantom read past the end, never a
non-whole-sector length.

## `a_raised_halt_still_delivers_when_the_channel_has_room`

The other half of the halt contract, and the regression that routing every
recovery send through `send_with_halt` introduced. `send_with_halt` checks
the halt BEFORE it touches the channel, so once Stop is pressed the very
next item is handed straight back — even when the channel has a free slot
and the delivery would not have blocked for a microsecond. The plain `send`
it replaced delivered that item. What gets thrown away is not a queue
entry: on patch it is a span the drive may have spent minutes clawing off a
damaged disc, and on sweep a 64 KiB batch already read. Pressing Stop asks
to stop DOING more work; it does not ask to discard work already done.

The halt's job is to decide what happens when there is NOWHERE to put the
item — that is the parked-producer case the sibling test pins. It is not a
licence to drop a free-slot handoff.

## `finish_bounded_tests`

The defect these pin: `send_bounded` gets the PRODUCER out from under a
stalled consumer, but the statement immediately after it unparks is the
teardown — and both recovery sites tore down with plain `Pipeline::finish`,
a blocking `join()` on that same stalled consumer. So a Stop that
`send_bounded` finally made land still never returned to the caller; the
wedge had only moved from `send` to `finish`. `finish_with_halt` is the
bound libfreemkv ships for it, and nothing in this crate called it.

## `an_abandoned_consumer_cannot_overwrite_a_resumed_passs_mapfile`

A STOP MUST NOT COST THE NEXT PASS ITS RECORD. The consumer abandoned on a
hung mount is detached, not killed: it still owns the pass's `Mapfile`, and
its stale snapshot must never reach the path again — because by the time
its write returns, the operator has done the ordinary next thing after a
Stop (or the thing this crate's docs tell them to do after a wedge) and
resumed the rip, whose own `Mapfile` is now the record. A whole-file
rewrite from the old snapshot silently reverts sectors the resumed pass
confirmed `Finished` — minutes of recovery off damaged media, discarded
with no error anywhere.

## `only_not_found_means_the_image_is_missing`

NotFound is the only error that means "no file yet". Every other error is
UNKNOWN, and the difference matters more than any other line in this file:
the two call sites use this to decide whether to open the existing image or
`File::create` over it. Answer "missing" on a transient EIO from a flaky
staging volume and a populated ISO is truncated to zeros — bytes the
mapfile still records `Finished`, which the producer will therefore never
re-read.

Not reachable through a filesystem fixture: any path that makes
`metadata()` fail with something other than NotFound also makes the
immediately following `File::create` fail the same way, so the original and
a mutant both come back `Err` with the same errno. Hence the decision is a
function taking the `io::Result` directly.

## `patch_preset_tests`

The shipped Pass-N patch preset, pinned. `PatchOptions::for_patch_pass`
exists so the two routes into a patch pass cannot drift, and five test
sites nevertheless hand-wrote its four values as literals — which meant the
SHIPPED preset was exercised by nothing: every value could be changed in
production and the whole suite stayed green (verified: `block_sectors:
Some(1), full_recovery: false, reverse: false, wedged_threshold: 8` passes
285 unit + 56 integration tests). The literals are gone; this is the
assertion that makes the preset load-bearing.

WHAT THESE FOUR VALUES ACTUALLY DO — read this before "fixing" a value here
because a doc comment somewhere says it tunes recovery. Three of the four
no longer steer the pass at all; the handler chain in `section_recover.rs`
owns read sizing, per-read timeouts and the wedge exit:

- `block_sectors: Some(32)` — LABEL ONLY. It does not size any read. Its one
  observable effect is the pass label the front end renders: `Some(1)`
  reads as a SCRAPE pass, anything larger as a TRIM pass
  (`patch::pass_kind`). Asserted below through that production function.
- `reverse: true` — LABEL ONLY. It decorates the same `PassKind`. It does
  NOT order the walk: `PatchCtx::run` sorts bad ranges (size desc, pos
  asc). Asserted below through the label.
- `full_recovery: true` — DIAGNOSTIC ONLY. `patch()` logs it as
  `recovery=` and nothing reads it. Pinned below as a value, not as a
  behaviour, and deliberately not dressed up as one.
- `wedged_threshold: 50` — REPORTED, NOT ENFORCED. Nothing counts wedged
  reads against it; `PatchOutcome::wedged_exit` comes from a handler's
  `TransportFault`. The threshold is echoed verbatim into the outcome for
  the caller to render, which is what the test asserts (through
  `build_outcome`, not by re-reading the field).

So this is honestly "the preset, and the labels/echoes it produces" — not
recovery tuning. The reason it still earns its place: the preset exists so
the two routes into a patch pass cannot drift, and five test sites used to
hand-write its four values as literals, which meant the SHIPPED preset was
exercised by nothing (verified: `block_sectors: Some(1), full_recovery:
false, reverse: false, wedged_threshold: 8` passed the whole suite).

## `the_wedged_threshold_is_reported_not_enforced`

The behaviour `wedged_threshold` has: it is REPORTED, verbatim, in the
outcome the caller renders — and it does not, by itself, make the pass look
wedged. Both halves matter: a caller that printed a wedge warning off this
field alone would be wrong, and a caller that never saw the number could
not explain the wedge exit when it does happen.

## `it_actually_sleeps_when_not_halted` / `an_already_set_halt_returns_promptly`

The pause must actually happen. The mutation run replaced the whole
`sleep_secs_or_halt` function with `()` and the suite stayed green — so
the wedge-avoidance inter-error pause, the thing that stops a damaged disc
being hammered, was unconstrained. Same for mutating the loop condition to
`==` or `>`, both of which make the loop body unreachable.

And it must break out early when halt is already set, rather than serving
the full pause. This is the difference between Stop being honoured and the
operator waiting out a multi-second cooldown.
