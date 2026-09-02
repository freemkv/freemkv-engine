# Pass-N read-error handler A/B golden fixture (`tests/passn_handler_ab.rs`)

## What this pins, in today's code

The end-to-end behaviour of `freemkv_engine::patch` over eight canonical
damage profiles against a synthetic `ScriptedSectorReader`. Each profile
asserts the exact observable outcome — final mapfile byte counts, plus an
upper bound on the number of reads — so any change to the Pass-N failure
path either preserves the goldens or fails loudly. They are the evidence
that a refactor did not move shipped recovery behaviour.

## Where that behaviour lives

`recovery::read_error::handle_read_error` is the Pass-1 (sweep) error
policy and is the sweep loop's only production caller. Pass N does not
route through it: its recovery is the chain of time-bounded handlers in
`recovery/section_recover.rs`, driven per bad section by
`recovery/patch.rs`. The Pass-N damage threshold is
`read_error::PATCH_DAMAGE_THRESHOLD_PCT` (6, against the sweep's 12).

## History (2026-05-13, v0.20.8) — the golden VALUES below date from it

This file began as the A/B fixture for unifying Pass N onto
`handle_read_error`, when Pass N had its own inline `handle_read_failure`
in `recovery/patch.rs` producing a `FailureAction`, its own damage window,
and its own `compute_damage_skip` with a size-aware `range_remaining/4`
cap and a `MAX_SKIPS_PER_RANGE` bound. NONE of those five names exists in
`src/` any more — the per-section handler chain replaced that loop
wholesale — so do not go looking for them. What survived the replacement,
unchanged, is the observable contract this file asserts.

## `single_dead_sector_patch_stats` coverage gap

These HARDWARE_ERROR / ILLEGAL_REQUEST cases assert only the
persistent-sense RECOVERY CONTRACT (never Unreadable, byte conservation,
dead sector stays pending) — they do NOT exercise the patch WEDGE-EXIT
path. A single dead sector in one range structurally cannot reach either
exit: `WEDGE_ABORT_THRESHOLD=16` needs 16 CONSECUTIVE wedge-family senses
within a range, and the `wedged_threshold=50` exit additionally needs
`range_idx > 0` (a prior range already processed). A real wedge-exit
fixture (a first throwaway range, then a second range of >=16 sectors
that ALL always-fail with HARDWARE_ERROR, in reverse mode) is a separate,
larger synthetic build; left out here rather than bent into this shared
single-sector helper.

This comment used to end "no test anywhere asserts
`PatchOutcome::wedged_exit == true`", and that is NO LONGER TRUE — do not
act on it. `multipass::tests::a_wedged_result_is_distinguishable_from_a_
cancelled_one` (in `src/multipass.rs`) drives a real `multipass_rip` whose
patch pass meets a transport failure and asserts `result.wedged`, reaching
the `PassExit::Wedged` arm end to end. The gap left here is this HELPER's,
not the crate's, and the wedge arm is live code with a live test behind it.
