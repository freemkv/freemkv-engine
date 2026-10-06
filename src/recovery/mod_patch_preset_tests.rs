use super::*;

/// The preset's values, pinned — including the two that are inert, so a
/// future change to them is at least deliberate.
#[test]
fn for_patch_pass_carries_the_shipped_tuning() {
    let o = PatchOptions::for_patch_pass(false, None, None);
    assert_eq!(o.block_sectors, Some(32));
    assert!(o.full_recovery, "diagnostics-only, but pinned");
    assert!(o.reverse);
    assert_eq!(o.wedged_threshold, 50);
    assert!(!o.decrypt, "decrypt is the caller's, forwarded verbatim");

    // And `decrypt` really is forwarded, not hard-coded.
    assert!(PatchOptions::for_patch_pass(true, None, None).decrypt);
}

/// The behaviour `block_sectors` + `reverse` still have: the pass label the
/// operator sees on every progress tick. With the shipped preset that is a
/// REVERSE TRIM pass; `Some(1)` would relabel the same pass as a scrape.
#[test]
fn the_preset_reports_a_reverse_trim_pass() {
    use libfreemkv::progress::PassKind;
    let o = PatchOptions::for_patch_pass(false, None, None);

    let kind = patch::pass_kind(patch::initial_batch_of(&o), o.reverse);
    assert!(
        matches!(kind, PassKind::Trim { reverse: true }),
        "the shipped preset must render as a reverse TRIM pass, got {kind:?}"
    );

    // The contrast, so the assertion above is not just "whatever it does":
    // a single-sector batch is a SCRAPE pass, and `reverse` really is the
    // flag that decorates it.
    let mut scrape = PatchOptions::for_patch_pass(false, None, None);
    scrape.block_sectors = Some(1);
    scrape.reverse = false;
    assert!(matches!(
        patch::pass_kind(patch::initial_batch_of(&scrape), scrape.reverse),
        PassKind::Scrape { reverse: false }
    ));

    // `Some(0)` must not underflow into the scrape label by accident.
    let mut zero = PatchOptions::for_patch_pass(false, None, None);
    zero.block_sectors = Some(0);
    assert_eq!(
        patch::initial_batch_of(&zero),
        1,
        "clamped to a valid batch"
    );
}

// `wedged_threshold` is REPORTED, verbatim, in the outcome — it does not, by itself, make
// the pass look wedged.
#[test]
fn the_wedged_threshold_is_reported_not_enforced() {
    let o = PatchOptions::for_patch_pass(false, None, None);

    let mut state = patch::PatchLoopState::new(0, patch::initial_batch_of(&o), 4096);
    let summary = patch::PatchSummary {
        stats: mapfile::MapStats::default(),
    };
    let outcome_of = |state: &patch::PatchLoopState| {
        patch::build_outcome(
            state,
            &summary,
            std::path::Path::new("/nonexistent/for-outcome-only"),
            4096,
            0,
            o.wedged_threshold,
        )
    };
    let outcome = outcome_of(&state);
    assert_eq!(
        outcome.wedged_threshold, 50,
        "the preset's threshold must reach the caller's outcome verbatim"
    );
    // `wedged_exit` is the pass's own transport-fault flag, echoed; the threshold
    // neither sets nor clears it.
    assert!(!outcome.wedged_exit);
    state.wedged_exit = true;
    let wedged = outcome_of(&state);
    assert!(
        wedged.wedged_exit,
        "a transport-fault exit must reach the outcome"
    );
    assert_eq!(wedged.wedged_threshold, 50);
}
