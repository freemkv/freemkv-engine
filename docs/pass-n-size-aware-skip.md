# `tests/pass_n_size_aware_skip.rs` history

The user's failure mode (2026-05-07): "what if we have a 100 sector zone
and its really 2 25 sector zones and we keep jumping over the good in
the middle." The pre-fix patch loop escalated skip-distance based on
`consecutive_skips_without_recovery` with hardcoded 32 → 4096 sector
caps. A 100-sector bad range whose actual layout is 25 bad + 50 good +
25 bad would have the patch skip 32-4096 sectors after a couple of
failures, leaping over the entire range AND the good middle.

The fix at the time: cap each skip at `range_remaining/4`.

`range_remaining`, `compute_damage_skip` and `MAX_SKIPS_PER_RANGE` do
not exist in `src/` any more. The per-section handler chain in
`recovery/section_recover.rs` (driven per bad range by
`recovery/patch.rs`) replaced that loop wholesale, and it is what
recovers the good middle of a bad range today — by walking the range
from several angles under a time budget, not by bounding a skip
distance. The same note is on `tests/passn_handler_ab.rs`, the sibling
fixture from the same rework.

What survived, and what these tests still pin, is the observable
contract: a 100-sector bad range whose middle 50 sectors are readable
must come back with that middle recovered, not leapt over. That
assertion is independent of which mechanism does it, which is why the
file kept its value when the mechanism it was named for was deleted.

## `patch_recovers_good_middle_of_a_bad_range`

THE critical test. A 100-sector "bad" range hides 50 good sectors in
the middle (LBAs 125-174). The pre-fix patch loop would skip-escalate
at 32+ sectors and leap over the whole range, good middle included.
The recovered middle is the contract; the `range_remaining/4` cap that
first delivered it is long gone (see above) and the handler chain
delivers it now.

## `patch_pipeline_split_recovers_and_records_correctly`

0.18 Pass N pipeline split: exercises the new producer/consumer path
end-to-end on a synthetic patterned reader. Bad range layout is small
(5 bad LBAs surrounded by good middle) so the producer emits a mix of
`Recovered` and `NonTrimmed` items and the consumer thread must apply
both kinds. Verifies:

- `bytes_good` advances (good sectors flow producer→consumer→file
  →mapfile with the data preserved).
- The recovered LBAs end up Finished; the bad LBAs end up NonTrimmed
  (NOT Unreadable — promotion to Unreadable is the orchestrator's job
  after the final pass).
- Bytes written at the recovered offsets match what the producer read
  from the patterned source (proves the channel hand-off didn't drop
  or reorder buffers, and the consumer's seek+write landed at the
  right offsets).
