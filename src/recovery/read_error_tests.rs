use super::*;
use libfreemkv::error::Error;
use libfreemkv::scsi::ScsiSense;

fn medium_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_MEDIUM_ERROR,
            asc: 0x11,
            ascq: 0x05,
        }),
    }
}

fn hardware_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_HARDWARE_ERROR,
            asc: 0x44,
            ascq: 0x00,
        }),
    }
}

fn illegal_request_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_ILLEGAL_REQUEST,
            asc: 0x24,
            ascq: 0x00,
        }),
    }
}

fn recovered_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_RECOVERED_ERROR,
            asc: 0x17,
            ascq: 0x01,
        }),
    }
}

// KU §2.4: "No held key opens U → loud stop: E7022 (title) or E7032 (image or folder)".
// U was read, so the stop is never damage: no retry, skip, jump or count.
#[test]
fn a_key_stop_aborts_the_pass_and_leaves_the_damage_state_untouched() {
    let stops = [
        Error::NoDiscKey {
            disc_hash: "ab".repeat(20),
        },
        Error::WholeDiscKeyMissing,
    ];
    for err in stops {
        for mut ctx in [ReadCtx::for_sweep(32), ReadCtx::for_patch(32)] {
            assert_eq!(
                handle_read_error(&err, &mut ctx),
                ReadAction::AbortPass,
                "{err}"
            );
            assert_eq!(ctx.total_errors, 0, "{err}: not counted as a read error");
            assert_eq!(ctx.consecutive_failures, 0);
            assert_eq!(ctx.consecutive_outer_failures, 0);
            assert!(
                ctx.damage_window.is_empty(),
                "{err}: no damage-window entry"
            );
            assert_eq!((ctx.jumps_taken, ctx.wedge_count), (0, 0));
            assert!(ctx.last_error_at.is_none());
        }
    }
}

#[test]
fn recovered_error_skips_block_not_jump_pass_1() {
    // A recovered (marginal) read on Pass 1 must NOT trigger the damage-jump
    // (would nuke good regions); it SkipBlocks for a Pass N re-read instead,
    // counted as marginal_recovered.
    let mut ctx = ReadCtx::for_sweep(32);
    let action = handle_read_error(&recovered_err(), &mut ctx);
    assert!(
        matches!(action, ReadAction::SkipBlock { .. }),
        "recovered error must SkipBlock, not JumpAhead; got {action:?}"
    );
    assert_eq!(ctx.marginal_recovered, 1);
    // It must not have inflated the damage-jump multiplier (not damage signal).
    assert_eq!(ctx.jump_multiplier, 1);
    assert_eq!(ctx.jumps_taken, 0);
}

#[test]
fn recovered_error_does_not_consume_zone_entry() {
    // A recovered (marginal) read must NOT latch in_damage_zone — otherwise a
    // genuine hard error that follows would not be seen as the zone entry and
    // would skip the 30s wedge cooldown.
    let mut ctx = ReadCtx::for_sweep(32);
    handle_read_error(&recovered_err(), &mut ctx);
    assert!(
        !ctx.in_damage_zone,
        "recovered read is not damage-zone signal"
    );
    assert_eq!(ctx.zones_entered, 0);
    // The following genuine hard error IS the real zone entry.
    handle_read_error(&hardware_err(), &mut ctx);
    assert!(ctx.in_damage_zone);
    assert_eq!(ctx.zones_entered, 1, "hard error registers the zone entry");
}

// A NOT_READY retry must not consume the zone-entry transition either:
// that transition buys the drive its 30s wedge-avoidance cooldown, and
// a NOT_READY retry shouldn't spend it before the real hard error does.
#[test]
fn a_not_ready_retry_does_not_consume_the_zone_entry() {
    let mut ctx = ReadCtx::for_sweep(32);
    for _ in 0..NOT_READY_MAX_RETRIES {
        let a = handle_read_error(&not_ready_err(), &mut ctx);
        assert!(matches!(a, ReadAction::Retry { .. }), "got {a:?}");
    }
    assert!(
        !ctx.in_damage_zone,
        "a transient NOT_READY retry is not evidence of a damage zone"
    );
    assert_eq!(
        ctx.zones_entered, 0,
        "the zone counter must not tick for retries that never reach the cooldown"
    );

    // The error that DOES reach the pause selection is the real zone
    // entry, and it must get the long cooldown. A MEDIUM error: a wedge-family
    // one would pass on the wedge arm's own 30 s pause, whatever the latch did.
    let a = handle_read_error(&medium_err(), &mut ctx);
    assert!(ctx.in_damage_zone);
    assert_eq!(ctx.zones_entered, 1);
    let pause = match a {
        ReadAction::JumpAhead { pause_secs, .. } => pause_secs,
        ReadAction::SkipBlock { pause_secs } => pause_secs,
        ReadAction::Retry { pause_secs } => pause_secs,
        other => panic!("expected a paused action, got {other:?}"),
    };
    // 30 s is the documented cooldown (2026-05-11 incident: BU40N wedged
    // after 7 errors in 6.5 s). Literal, not `>= the constant` — that would
    // pass even if the constant were 0, reintroducing the wedge.
    assert!(
        pause >= 30,
        "the real zone entry must get the 30 s wedge cooldown, got {pause}s \
             — the NOT_READY retries had eaten the transition"
    );
}

/// A transport failure aborts the pass, so it must not spend the
/// transition on its way out either.
#[test]
fn a_transport_failure_does_not_consume_the_zone_entry() {
    let mut ctx = ReadCtx::for_sweep(32);
    assert!(matches!(
        handle_read_error(&transport_failure_err(), &mut ctx),
        ReadAction::AbortPass
    ));
    assert!(!ctx.in_damage_zone);
    assert_eq!(ctx.zones_entered, 0);
}

#[test]
fn recovered_error_skips_block_pass_n_too() {
    // Pass N sees the same: a recovered read is distrusted → SkipBlock (the
    // outer patch loop re-reads the range with FUA).
    let mut ctx = ReadCtx::for_patch(1);
    let action = handle_read_error(&recovered_err(), &mut ctx);
    assert!(
        matches!(action, ReadAction::SkipBlock { .. }),
        "got {action:?}"
    );
    assert_eq!(ctx.marginal_recovered, 1);
}

#[test]
fn many_recovered_errors_never_jump() {
    // Even a run of recovered errors must never escalate to a damage-jump —
    // they're marginal reads, not a hard-damage cluster.
    let mut ctx = ReadCtx::for_sweep(32);
    for _ in 0..40 {
        let a = handle_read_error(&recovered_err(), &mut ctx);
        assert!(matches!(a, ReadAction::SkipBlock { .. }), "got {a:?}");
    }
    assert_eq!(ctx.jumps_taken, 0, "recovered errors never jump");
    assert_eq!(ctx.marginal_recovered, 40);
}

#[test]
fn pass_1_marginal_jumps_immediately() {
    // 2026-05-11 rewrite: Pass 1 jumps on the FIRST marginal error
    // (fast_jump_threshold=1) instead of SkipBlock — retrying the same
    // LBA quickly triggers the BU40N firmware fast-fail; Pass N revisits later.
    let mut ctx = ReadCtx::for_sweep(32);
    let action = handle_read_error(&medium_err(), &mut ctx);
    match action {
        ReadAction::JumpAhead { .. } => {}
        other => panic!("expected JumpAhead on first Pass 1 marginal error, got {other:?}"),
    }
}

#[test]
fn medium_error_with_batch_1_skips() {
    let mut ctx = ReadCtx::for_patch(1);
    let action = handle_read_error(&medium_err(), &mut ctx);
    match action {
        ReadAction::SkipBlock { pause_secs } => assert!(pause_secs >= 1),
        other => panic!("expected SkipBlock, got {other:?}"),
    }
}

#[test]
fn pass_1_jumps_immediately_on_first_outer_failure() {
    // 2026-05-11 rewrite: fast_jump_threshold=1, not 4 — even ONE error
    // triggers a jump since firmware fast-fail is sensitive to retry cadence
    // (the observed wedge hit at 7 errors/6.5s); jumping on #1 avoids the cascade.
    let mut ctx = ReadCtx::for_sweep(32);
    let a = handle_read_error(&medium_err(), &mut ctx);
    assert!(
        matches!(a, ReadAction::JumpAhead { .. }),
        "expected JumpAhead on first outer failure (fast_jump_threshold=1), got {a:?}"
    );
}

#[test]
fn pass_n_does_not_fast_jump() {
    // Pass N's fast-entry trigger must stay OFF (it grinds on bad ranges),
    // however long the run gets. Suppress the window trigger (unsatisfiable
    // density) and run a long streak, since 4 failures never filled the window.
    let mut ctx = ReadCtx::for_patch(32);
    assert_eq!(
        ctx.fast_jump_threshold,
        u64::MAX,
        "Pass N's fast-entry trigger must be disabled outright, not merely \
             set high"
    );
    ctx.damage_threshold_pct = 101;
    for i in 1..=256 {
        let a = handle_read_error(&medium_err(), &mut ctx);
        assert!(
            !matches!(a, ReadAction::JumpAhead { .. }),
            "Pass N must not fast-jump, and did on failure {i}: {a:?}"
        );
    }
    assert_eq!(ctx.jumps_taken, 0, "no jump of any kind should have fired");
    assert_eq!(
        ctx.consecutive_outer_failures, 256,
        "the outer-failure counter must keep climbing — a reset would mean \
             a jump fired and the assertions above were vacuous"
    );
}

#[test]
fn outer_success_resets_consecutive_outer_failures() {
    // `on_success` must clear the outer-failure counter, or scattered
    // failures across clean regions would fire a fast-entry jump. Use a Pass
    // N ctx (fast-jump off, window suppressed) so the counter stays non-zero.
    let mut ctx = ReadCtx::for_patch(1);
    ctx.damage_threshold_pct = 101; // no window jump; nothing else resets it
    for _ in 0..3 {
        handle_read_error(&medium_err(), &mut ctx);
    }
    assert_eq!(
        ctx.consecutive_outer_failures, 3,
        "fixture invalid: the failures must have accumulated, or the reset \
             below is asserted against a counter that was already 0"
    );
    ctx.on_success();
    assert_eq!(
        ctx.consecutive_outer_failures, 0,
        "a successful read ends the outer-failure streak"
    );
}

#[test]
fn pass_1_hardware_error_jumps_ahead_not_aborts() {
    // Pass 1 should JumpAhead with a 1 GB skip + cooldown instead of
    // aborting immediately — the pre-fix behavior killed rips at 48%
    // on damaged discs.
    let mut ctx = ReadCtx::for_sweep(32);
    let action = handle_read_error(&hardware_err(), &mut ctx);
    match action {
        ReadAction::JumpAhead {
            sectors,
            pause_secs,
        } => {
            // Literals, not the constants themselves: comparing a value to
            // the constant that produced it holds for any value, so shrinking
            // the jump or zeroing the cooldown would still pass. 1 GiB = 524_288 sectors.
            assert_eq!(sectors, 1_073_741_824 / 2048);
            assert_eq!(pause_secs, 30);
        }
        other => panic!("expected JumpAhead, got {other:?}"),
    }
    assert_eq!(ctx.wedge_count, 1);
}

#[test]
fn pass_1_hardware_error_aborts_after_threshold() {
    // After WEDGE_ABORT_THRESHOLD consecutive wedges with no good read
    // between them, autorip should see a real AbortPass to surface "drive
    // stuck, power-cycle required" rather than looping forever.
    let mut ctx = ReadCtx::for_sweep(32);
    for i in 0..WEDGE_ABORT_THRESHOLD - 1 {
        let action = handle_read_error(&hardware_err(), &mut ctx);
        assert!(
            matches!(action, ReadAction::JumpAhead { .. }),
            "iter {i}: expected JumpAhead, got {action:?}"
        );
    }
    // The Nth wedge crosses the threshold.
    let action = handle_read_error(&hardware_err(), &mut ctx);
    assert_eq!(action, ReadAction::AbortPass);
}

#[test]
fn pass_1_good_read_resets_wedge_count() {
    // A single successful read between wedges must clear the skip
    // counter, or a disc with scattered bad zones would eventually run
    // out of skip budget even though the drive kept recovering.
    let mut ctx = ReadCtx::for_sweep(32);
    for _ in 0..(WEDGE_ABORT_THRESHOLD - 1) {
        handle_read_error(&hardware_err(), &mut ctx);
    }
    assert_eq!(ctx.wedge_count, WEDGE_ABORT_THRESHOLD - 1);
    ctx.on_success();
    assert_eq!(ctx.wedge_count, 0);
    // After the success, we should still get JumpAhead (not
    // AbortPass) on the next wedge.
    let action = handle_read_error(&hardware_err(), &mut ctx);
    assert!(matches!(action, ReadAction::JumpAhead { .. }));
}

#[test]
fn pass_n_hardware_error_also_skips_not_aborts() {
    // 2026-05-11 reframe: skip+pause+continue applies to Pass N too
    // (previously it AbortPass'd on first wedge, same bug Pass 1 had).
    // Pass N's skip is smaller than Pass 1's 1 GB — over-skipping would abandon its target range.
    let mut ctx = ReadCtx::for_patch(1);
    let action = handle_read_error(&hardware_err(), &mut ctx);
    match action {
        ReadAction::JumpAhead {
            sectors,
            pause_secs,
        } => {
            // Literals, per `pass_1_hardware_error_jumps_ahead_not_aborts`.
            // 64 sectors is "past the bricked LBA + small buffer", deliberately
            // not the 1 GiB Pass-1 jump, which would blow past Pass N's target range.
            assert_eq!(sectors, 64);
            assert_eq!(pause_secs, 30);
        }
        other => panic!("expected JumpAhead, got {other:?}"),
    }
    assert_eq!(ctx.wedge_count, 1);
}

#[test]
fn pass_n_hardware_error_aborts_after_threshold() {
    // Same threshold as Pass 1 — after WEDGE_ABORT_THRESHOLD
    // consecutive wedges with no good read in between, give up.
    let mut ctx = ReadCtx::for_patch(1);
    for _ in 0..WEDGE_ABORT_THRESHOLD - 1 {
        let action = handle_read_error(&hardware_err(), &mut ctx);
        assert!(matches!(action, ReadAction::JumpAhead { .. }));
    }
    let action = handle_read_error(&hardware_err(), &mut ctx);
    assert_eq!(action, ReadAction::AbortPass);
}

#[test]
fn pass_1_illegal_request_also_routes_to_wedge_skip() {
    // ILLEGAL_REQUEST is the other half of the wedge family:
    // drive saying "I won't parse your CDB" after entering the
    // fast-fail state. Same treatment as HARDWARE_ERROR.
    let mut ctx = ReadCtx::for_sweep(32);
    let action = handle_read_error(&illegal_request_err(), &mut ctx);
    assert!(matches!(action, ReadAction::JumpAhead { .. }));
}

#[test]
fn long_failure_streak_extends_pause_on_pass_n() {
    // Pass N keeps the cooldown behaviour: pauses extend after many
    // consecutive failures. Pass 1 pays MORE, not less (it alone takes the
    // 30s zone-entry cooldown); see the two tests named below for both sides.
    let mut ctx = ReadCtx::for_patch(1);
    for _ in 0..15 {
        handle_read_error(&medium_err(), &mut ctx);
    }
    let final_action = handle_read_error(&medium_err(), &mut ctx);
    // Literal 5 s: `>= CONSECUTIVE_FAIL_LONG_PAUSE_SECS` is the constant
    // compared against itself and held for any value, including 0.
    match final_action {
        ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
        ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 5 + 2),
        other => panic!("expected long-pause action, got {other:?}"),
    }
    assert!(
        ctx.long_pause_escalations > 0,
        "a 16-failure streak must have taken the escalation branch"
    );
}

#[test]
fn pass_1_zone_entry_uses_long_cooldown() {
    // Pass 1's FIRST error (zone entry) gets 30s cooldown + 2s post-jump
    // extra, preventing the retry cadence that triggers firmware fast-fail.
    // Literal 32s, not the constants summed (that would pass even at cooldown=0).
    let mut ctx = ReadCtx::for_sweep(32);
    let action = handle_read_error(&medium_err(), &mut ctx);
    match action {
        ReadAction::JumpAhead { pause_secs, .. } => {
            assert_eq!(
                pause_secs, 32,
                "first-error pause is the 30 s zone-entry cooldown plus the \
                     2 s post-jump extra"
            );
        }
        other => panic!("expected JumpAhead on first Pass 1 error, got {other:?}"),
    }
    // And the cooldown is a real pause, not merely "some number": it must
    // dwarf the ordinary 5 s inter-error pause, which is the whole reason
    // the constant exists.
    assert_eq!(
        ZONE_ENTRY_COOLDOWN_SECS, 30,
        "the zone-entry cooldown is 30 s — the value the 2026-05-11 wedge \
             incident was tuned against"
    );
}

#[test]
fn pass_1_subsequent_in_zone_errors_skip_long_cooldown() {
    // Regression: fast-jump resets consecutive_outer_failures after each
    // jump, so zone-entry must key off in_damage_zone, not that counter —
    // otherwise every error in a damaged region pays the 30s cooldown.
    let mut ctx = ReadCtx::for_sweep(32);
    // First error: genuine zone entry, gets the long cooldown.
    let first = handle_read_error(&medium_err(), &mut ctx);
    match first {
        // Literal 30 + 2, for the reason given in
        // `pass_1_zone_entry_uses_long_cooldown`.
        ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 32),
        other => panic!("expected JumpAhead on first error, got {other:?}"),
    }
    // We are now still in the damage zone; the jump reset the outer
    // counter. A second error must NOT re-arm the 30 s cooldown.
    assert!(ctx.in_damage_zone);
    let second = handle_read_error(&medium_err(), &mut ctx);
    let pause = match second {
        ReadAction::JumpAhead { pause_secs, .. } => pause_secs,
        ReadAction::SkipBlock { pause_secs } => pause_secs,
        other => panic!("expected pausing action, got {other:?}"),
    };
    assert_ne!(
        pause, 32,
        "subsequent in-zone error must not pay the 30 s zone-entry cooldown"
    );
    assert!(
        pause <= 7,
        "subsequent in-zone pause should be the standard 5 s fail pause \
             (+2 s post-jump), got {pause}"
    );
}

#[test]
fn pass_n_pauses_uniformly_on_failed_read() {
    // Pass N is exempt from the zone-entry long pause — it retries single
    // sectors on already-known-bad LBAs, and a 30s pause per failure would
    // pointlessly slow recovery. Keeps the standard 5s FAIL_PAUSE_SECS.
    let mut ctx = ReadCtx::for_patch(1);
    let action = handle_read_error(&medium_err(), &mut ctx);
    // Literals, like the rest of this module's pause assertions: written
    // against `FAIL_PAUSE_SECS` these held for any value of it, INCLUDING
    // 0 — and 0 is precisely the un-paced hammering that wedged the BU40N.
    match action {
        ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
        ReadAction::JumpAhead { pause_secs, .. } => assert_eq!(pause_secs, 5 + 2),
        other => panic!("expected pausing action, got {other:?}"),
    }
}

// The WINDOW trigger, isolated from the fast-entry trigger: disable the fast path
// (`u64::MAX`, as Pass N does) so only the window can fire, then pin both
// no-jump-while-short and jump-on-fill.
#[test]
fn damage_window_fills_then_jumps() {
    let mut ctx = ReadCtx::for_sweep(1);
    ctx.fast_jump_threshold = u64::MAX;
    ctx.damage_window_max = 4;
    ctx.damage_threshold_pct = 50;

    // Reads 1-3: the window is not full yet, so the window trigger must
    // NOT fire — a partly-filled window is not evidence of a damage zone.
    for i in 1..=3 {
        let a = handle_read_error(&medium_err(), &mut ctx);
        assert!(
            matches!(a, ReadAction::SkipBlock { .. }),
            "read {i} filled only {}/4 of the window and must skip in \
                 place, got {a:?}",
            ctx.damage_window.len()
        );
    }

    // Read 4 fills the window at 100% bad, which clears the 50% threshold.
    let a = handle_read_error(&medium_err(), &mut ctx);
    assert!(
        matches!(a, ReadAction::JumpAhead { .. }),
        "a full window at 100% bad against a 50% threshold must jump, \
             got {a:?}"
    );
}

// The threshold is a real comparison, not a formality: an unreachable
// threshold must never jump, so an inverted/dropped comparison in the
// impl doesn't only get caught by the test above.
#[test]
fn an_unreachable_damage_threshold_never_jumps() {
    let mut ctx = ReadCtx::for_sweep(1);
    ctx.fast_jump_threshold = u64::MAX;
    ctx.damage_window_max = 4;
    ctx.damage_threshold_pct = 101; // unsatisfiable: bad_pct maxes at 100
    for i in 1..=12 {
        let a = handle_read_error(&medium_err(), &mut ctx);
        assert!(
            matches!(a, ReadAction::SkipBlock { .. }),
            "read {i}: no density can reach 101%, so the window trigger \
                 must stay silent, got {a:?}"
        );
    }
}

#[test]
fn jump_multiplier_resets_after_damage_zone_exit() {
    // A zone that doubles the multiplier must not carry the inflated
    // value into the next zone — otherwise the next zone's first
    // jump is up to 64x oversized and skips recoverable data.
    let mut ctx = ReadCtx::for_sweep(32);
    // First zone: a few errors push jumps and double the multiplier.
    for _ in 0..4 {
        handle_read_error(&medium_err(), &mut ctx);
    }
    assert!(
        ctx.jump_multiplier > 1,
        "expected the multiplier to inflate inside a damage zone"
    );
    // Exit the zone: damage_window_max consecutive good reads.
    for _ in 0..ctx.damage_window_max {
        ctx.on_success();
    }
    assert!(!ctx.in_damage_zone, "zone should have exited");
    assert_eq!(
        ctx.jump_multiplier, 1,
        "jump_multiplier must reset to 1 on zone exit"
    );
}

#[test]
fn bridge_degradation_count_resets_on_success() {
    // After a good read the bridge recovered; the 15s-cooldown retry
    // budget must be available again instead of staying saturated
    // for the whole pass.
    let mut ctx = ReadCtx::for_patch(1);
    ctx.bridge_degradation_count = BRIDGE_DEGRADATION_MAX_RETRIES;
    ctx.on_success();
    assert_eq!(ctx.bridge_degradation_count, 0);
}

#[test]
fn wedge_abort_threshold_is_reachable() {
    // A permanently wedged drive must reach the abort threshold
    // rather than burning a WEDGE_PAUSE cooldown per read forever.
    let mut ctx = ReadCtx::for_patch(32);
    let mut aborted = false;
    for _ in 0..WEDGE_ABORT_THRESHOLD {
        if matches!(
            handle_read_error(&hardware_err(), &mut ctx),
            ReadAction::AbortPass
        ) {
            aborted = true;
            break;
        }
    }
    assert!(
        aborted,
        "a permanently wedged drive must reach the abort threshold"
    );
}

#[test]
fn on_success_resets_failure_counters_and_pushes_window() {
    let mut ctx = ReadCtx::for_sweep(32);
    for _ in 0..3 {
        handle_read_error(&medium_err(), &mut ctx);
    }
    assert!(ctx.consecutive_failures > 0);
    ctx.on_success();
    assert_eq!(ctx.consecutive_good, 1);
    assert_eq!(ctx.consecutive_failures, 0);
    assert!(*ctx.damage_window.last().unwrap());
}

// Additional hardening: retry-budget boundaries, transport-abort
// precedence, and the bounded-jump invariant — guards against off-by-one
// retry caps and an unbounded jump multiplier skipping the rest of the disc.

/// NOT_READY check-condition (status 0x02 so it is NOT classified as
/// bridge degradation, which keys off non-standard status bytes).
/// sense_key=2 with a generic ASC routes to the NOT_READY retry path.
fn not_ready_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_NOT_READY,
            asc: 0x04,
            ascq: 0x00,
        }),
    }
}

/// Transport failure: SCSI status 0xFF (bridge crash). Step 1 of
/// `handle_read_error`: this aborts the copy.
fn transport_failure_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE),
        sense: None,
    }
}

/// Bridge degradation: a non-standard status byte (0x04 - neither
/// GOOD/CHECK/TRANSPORT) with empty sense, per `Error::is_bridge_degradation`.
fn bridge_degradation_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(0x04),
        sense: None,
    }
}

/// UNIT ATTENTION (sense key 6, ASC 28h "medium may have changed"),
/// status 0x02 so it is a real CHECK CONDITION, not a transport failure.
fn unit_attention_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_UNIT_ATTENTION,
            asc: 0x28,
            ascq: 0x00,
        }),
    }
}

#[test]
fn not_ready_retries_capped_at_three_then_falls_through() {
    // Step 3 of `handle_read_error`: NOT READY -> pause 3s, retry
    // up to 3x (NOT_READY_MAX_RETRIES), then mark NonTrimmed. The 1st-3rd
    // must Retry, the 4th must fall through to skip.
    let mut ctx = ReadCtx::for_patch(1);
    for i in 0..NOT_READY_MAX_RETRIES {
        let a = handle_read_error(&not_ready_err(), &mut ctx);
        assert!(
            matches!(a, ReadAction::Retry { .. }),
            "NOT_READY attempt {i} should Retry, got {a:?}"
        );
    }
    // Budget exhausted: the next NOT_READY must not Retry.
    let a = handle_read_error(&not_ready_err(), &mut ctx);
    assert!(
        !matches!(a, ReadAction::Retry { .. }),
        "NOT_READY past the retry cap must fall through, got {a:?}"
    );
}

#[test]
fn transport_failure_aborts_on_both_passes() {
    // Step 1 of `handle_read_error`: transport failure (bridge
    // crash, status 0xFF) aborts the pass to re-enumerate the bridge — must
    // hold on both passes, not get swallowed into a JumpAhead by wedge-skip.
    let mut ctx = ReadCtx::for_patch(32);
    assert_eq!(
        handle_read_error(&transport_failure_err(), &mut ctx),
        ReadAction::AbortPass
    );
    // And on a fresh Pass 1 context, still AbortPass.
    let mut ctx1 = ReadCtx::for_sweep(32);
    assert_eq!(
        handle_read_error(&transport_failure_err(), &mut ctx1),
        ReadAction::AbortPass
    );
}

#[test]
fn unit_attention_aborts_the_pass_not_treated_as_a_bad_sector() {
    // A mid-read medium change must abort so the outer loop reacquires,
    // not fall through to the bad-sector path (SkipBlock/JumpAhead) and
    // stitch post-change sectors into the ISO. RED before the UA branch.
    let mut ctx = ReadCtx::for_sweep(32);
    assert_eq!(
        handle_read_error(&unit_attention_err(), &mut ctx),
        ReadAction::AbortPass
    );
    // Pass N too: the wedge/jump arms must not swallow it into a JumpAhead.
    let mut ctx_n = ReadCtx::for_patch(32);
    assert_eq!(
        handle_read_error(&unit_attention_err(), &mut ctx_n),
        ReadAction::AbortPass
    );
}

#[test]
fn bridge_degradation_retries_to_budget_then_falls_through() {
    // Bridge-degradation cooldown retry is bounded by
    // BRIDGE_DEGRADATION_MAX_RETRIES (=5): first 5 errors Retry with the
    // long cooldown, the 6th falls through rather than retrying forever.
    let mut ctx = ReadCtx::for_patch(1);
    for i in 0..BRIDGE_DEGRADATION_MAX_RETRIES {
        let a = handle_read_error(&bridge_degradation_err(), &mut ctx);
        match a {
            ReadAction::Retry { pause_secs } => {
                assert_eq!(
                    pause_secs, 15,
                    "bridge retry {i} should use the bridge cooldown"
                );
            }
            other => panic!("bridge degradation attempt {i} should Retry, got {other:?}"),
        }
    }
    let a = handle_read_error(&bridge_degradation_err(), &mut ctx);
    assert!(
        !matches!(a, ReadAction::Retry { .. }),
        "bridge degradation past the retry budget must fall through, got {a:?}"
    );
}

// The documented BU40N bad-sector signature: NOT_READY (sense_key=2,
// ASC=0x04, ASCQ=0x3E) as a CHECK CONDITION (status 0x02) — NOT the
// case `is_bridge_degradation` matches, despite an old comment's claim.
fn not_ready_04_3e_err() -> Error {
    Error::DiscRead {
        sector: 100,
        status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
        sense: Some(ScsiSense {
            sense_key: scsi::SENSE_KEY_NOT_READY,
            asc: 0x04,
            ascq: 0x3E,
        }),
    }
}

#[test]
fn not_ready_04_3e_does_not_take_bridge_branch() {
    // Regression guard: the bridge branch keys on the status byte alone,
    // NOT the NOT_READY 04/3E sense — a real 04/3E error arrives as CHECK
    // CONDITION, so it must route to the NOT_READY retry, not bridge cooldown.
    let err = not_ready_04_3e_err();
    assert!(
        !err.is_bridge_degradation(),
        "04/3E arrives as CHECK CONDITION (0x02); it is not bridge degradation"
    );

    let mut ctx = ReadCtx::for_patch(1);
    match handle_read_error(&err, &mut ctx) {
        ReadAction::Retry { pause_secs } => {
            assert_eq!(
                pause_secs, 3,
                "04/3E must use the generic NOT_READY pause, not the bridge cooldown"
            );
            // Confirm it really went through the NOT_READY path.
            assert_eq!(ctx.not_ready_retries, 1);
            assert_eq!(ctx.bridge_degradation_count, 0);
        }
        other => panic!("04/3E should Retry via the NOT_READY path, got {other:?}"),
    }
}

// A block whose NOT_READY outlasts the budget is skipped; the next one gets its own.
#[test]
fn a_not_ready_budget_is_per_block() {
    let mut ctx = ReadCtx::for_sweep(32);
    for _ in 0..3 {
        assert!(matches!(
            handle_read_error(&not_ready_04_3e_err(), &mut ctx),
            ReadAction::Retry { pause_secs: 3 }
        ));
    }
    let moved_on = handle_read_error(&not_ready_04_3e_err(), &mut ctx);
    assert!(
        !matches!(moved_on, ReadAction::Retry { .. }),
        "{moved_on:?}"
    );
    assert!(matches!(
        handle_read_error(&not_ready_04_3e_err(), &mut ctx),
        ReadAction::Retry { pause_secs: 3 }
    ));
}

#[test]
fn jump_multiplier_caps_and_jump_distance_stays_bounded() {
    // Step 6 of `handle_read_error`: multiplier doubles per jump but is capped at
    // 64 (the "4 GiB cap"), so a single jump can never grow unbounded and skip
    // the rest of the disc; verify saturation holds.
    const MAX_JUMP_MULTIPLIER: u64 = 64;
    let batch: u16 = 32;
    let mut ctx = ReadCtx::for_sweep(batch);
    // Small window + 0% threshold so every failure can window-trigger
    // a jump and keep doubling the multiplier toward the cap.
    ctx.damage_window_max = 2;
    ctx.damage_threshold_pct = 0;
    let mut last_jump_sectors = 0u64;
    for _ in 0..40 {
        if let ReadAction::JumpAhead { sectors, .. } = handle_read_error(&medium_err(), &mut ctx) {
            last_jump_sectors = sectors;
        }
        assert!(
            ctx.jump_multiplier <= MAX_JUMP_MULTIPLIER,
            "jump_multiplier {} exceeded the cap {}",
            ctx.jump_multiplier,
            MAX_JUMP_MULTIPLIER
        );
    }
    // After saturation the jump distance is a LITERAL sector count, not
    // the production expression itself (which would agree with any base).
    // 1024 base × 32 batch × 64 = 2_097_152 sectors = 4 GiB, the documented cap.
    assert_eq!(
        last_jump_sectors, 2_097_152,
        "the saturated jump is 4 GiB (2_097_152 sectors at batch=32)"
    );
}

// The FIRST damage-jump distance, pinned as a literal (not the same
// JUMP_BASE_SECTORS expression the handler evaluates) since it's the
// amount of disc Pass 1 writes off as NonTrimmed on a zone's first error.
#[test]
fn the_first_damage_jump_clears_exactly_64_mib() {
    let mut ctx = ReadCtx::for_sweep(32);
    assert_eq!(ctx.jump_multiplier, 1, "the first jump is un-multiplied");
    match handle_read_error(&medium_err(), &mut ctx) {
        ReadAction::JumpAhead { sectors, .. } => {
            // 1024 (base) × 32 (batch) = 32_768 sectors × 2048 B = 64 MiB.
            assert_eq!(
                sectors, 32_768,
                "the first jump at batch=32 clears 64 MiB — the BU40N's \
                     damage clusters are 100+ MB wide and a shorter jump lands \
                     back inside the cluster"
            );
            assert_eq!(sectors * 2048, 67_108_864, "= 64 MiB");
        }
        other => panic!("expected JumpAhead on the first Pass 1 error, got {other:?}"),
    }
    // The second jump doubles it, and no further: the multiplier is the
    // only thing that grows.
    ctx.jump_multiplier = 2;
    match handle_read_error(&medium_err(), &mut ctx) {
        ReadAction::JumpAhead { sectors, .. } => assert_eq!(
            sectors, 65_536,
            "one doubling of the multiplier is 128 MiB, not more"
        ),
        other => panic!("expected JumpAhead, got {other:?}"),
    }
}

// The long-streak pause escalation: its pause equals the ordinary one, so
// `ReadCtx::long_pause_escalations` is what makes the branch and its threshold
// observable/pinnable at all.
#[test]
fn the_long_streak_escalation_fires_at_its_threshold() {
    let mut ctx = ReadCtx::for_patch(1);
    ctx.damage_threshold_pct = 101; // window jumps off; pause path only

    // Failures 1..=9 are ordinary: the standard 5 s pause, no escalation.
    for i in 1..=9 {
        match handle_read_error(&medium_err(), &mut ctx) {
            ReadAction::SkipBlock { pause_secs } => assert_eq!(
                pause_secs, 5,
                "failure {i} is below the streak threshold and gets the \
                     ordinary 5 s pause"
            ),
            other => panic!("failure {i}: expected SkipBlock, got {other:?}"),
        }
        assert_eq!(
            ctx.long_pause_escalations, 0,
            "the escalation must not fire before its 10-failure threshold \
                 (fired on failure {i})"
        );
    }

    // The 10th consecutive failure crosses CONSECUTIVE_FAIL_LONG_PAUSE_
    // THRESHOLD and every failure after it stays escalated.
    for i in 10..=13u64 {
        match handle_read_error(&medium_err(), &mut ctx) {
            ReadAction::SkipBlock { pause_secs } => assert_eq!(pause_secs, 5),
            other => panic!("failure {i}: expected SkipBlock, got {other:?}"),
        }
        assert_eq!(
            ctx.long_pause_escalations,
            i - 9,
            "failure {i} is inside the streak and must take the escalation"
        );
    }

    // A good read ends the streak, so the next failure is ordinary again.
    ctx.on_success();
    handle_read_error(&medium_err(), &mut ctx);
    assert_eq!(
        ctx.long_pause_escalations, 4,
        "the streak ended at the successful read; the next failure is not \
             an escalation"
    );
    assert_eq!(
        ctx.pass_summary().long_pause_escalations,
        4,
        "the pass summary reports the escalations, or the operator cannot \
             see them"
    );
}

/// A zone entry outranks a long streak: the 30 s cooldown is the pause
/// that prevents the firmware wedge, and a streak must not downgrade it
/// to 5 s.
#[test]
fn the_zone_entry_cooldown_outranks_the_streak_escalation() {
    let mut ctx = ReadCtx::for_sweep(1);
    ctx.fast_jump_threshold = u64::MAX;
    ctx.damage_threshold_pct = 101;
    // Pre-load a long failure streak WITHOUT entering the zone, so both
    // conditions hold on the next error.
    ctx.consecutive_failures = 50;
    match handle_read_error(&medium_err(), &mut ctx) {
        ReadAction::SkipBlock { pause_secs } => assert_eq!(
            pause_secs, 30,
            "the zone-entry cooldown wins over the streak escalation"
        ),
        other => panic!("expected SkipBlock, got {other:?}"),
    }
    assert_eq!(
        ctx.long_pause_escalations, 0,
        "the escalation branch must not also run"
    );
}
