use super::*;

// Every `MultipassResult` field must be listed in `USING_THE_ENGINE.md`'s §4 (the GUI
// contract), derived from the SOURCE so a new field can't repeat the omission that once hid
// `wedged`/`complete`.
#[test]
fn every_multipass_result_field_is_documented() {
    let src = include_str!("multipass.rs");
    let guide = include_str!("../USING_THE_ENGINE.md");
    // The struct body: declaration to the closing brace in column 0.
    // `MultipassResult` has no `impl` block to terminate on (unlike
    // `Reason`), and no field/doc line inside contains a brace, so `\n}` is the end.
    let body = src
        .split_once("pub struct MultipassResult {")
        .expect("the file declares MultipassResult")
        .1
        .split_once("\n}")
        .expect("the struct body is closed")
        .0;

    let fields: Vec<&str> = body
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pub "))
        .filter_map(|rest| rest.split_once(':'))
        .map(|(name, _)| name.trim())
        .collect();

    // Fixture checks: a parser that silently extracts nothing (or loses
    // the two fields this test was written for) must fail LOUDLY rather
    // than pass vacuously.
    assert!(
        fields.len() >= 10,
        "fixture check: expected at least the ten known fields, found {fields:?}"
    );
    for expected in ["wedged", "complete", "halted"] {
        assert!(
            fields.contains(&expected),
            "fixture check: the field this test was written for is gone: {fields:?}"
        );
    }

    for field in fields {
        // `mp.<field>` — the notation the guide's own example establishes.
        // Matching the bare word would let "completed" (an unrelated sink
        // method in §1) pass for `complete`.
        assert!(
            guide.contains(&format!("mp.{field}")),
            "MultipassResult field {field:?} is public but not listed in \
                 USING_THE_ENGINE.md — a front-end reading that guide will \
                 never know it exists"
        );
    }
}

// ── Abort-gate: ported verbatim from autorip's loss_aborts_zero_threshold
//    _is_byte_exact so the engine keeps identical semantics. ──
#[test]
fn loss_aborts_zero_threshold_is_byte_exact() {
    assert!(
        loss_aborts(1, 0.0, 0),
        "1 lost byte must abort at threshold 0"
    );
    assert!(
        !loss_aborts(0, 12_345.0, 0),
        "0 lost bytes proceeds at threshold 0 even if seconds estimate nonzero"
    );
    assert!(
        loss_aborts(0, f64::NAN, 0),
        "NaN loss fails safe to abort even at threshold 0"
    );
    assert!(
        !loss_aborts(9_999_999, 999.0, 1),
        "999ms under a 1s threshold proceeds (bytes ignored on the seconds path)"
    );
    assert!(
        loss_aborts(0, 1001.0, 1),
        "1001ms over a 1s threshold aborts"
    );
    assert!(
        !loss_aborts(0, 1000.0, 1),
        "exactly 1000ms at a 1s threshold proceeds (strictly greater-than aborts)"
    );
    assert!(
        !loss_aborts(0, f64::NAN, 30),
        "an untimeable loss never exceeds a seconds tolerance"
    );
    assert!(
        loss_aborts(4096, f64::NAN, 0),
        "the perfect gate aborts on its bytes"
    );
}

#[test]
fn effective_abort_secs_forces_iso_to_zero() {
    assert_eq!(
        effective_abort_secs(true, 30),
        0,
        "ISO output requires 100%"
    );
    assert_eq!(
        effective_abort_secs(false, 30),
        30,
        "muxed keeps configured"
    );
    assert_eq!(effective_abort_secs(false, 0), 0);
}

// `abort_lost_ms` must never answer "0 ms lost" when loss exists but
// cannot be measured — 0.0 reads as "within tolerance" and ships a
// damaged rip as good. Not reachable from today's callers; pins the API.
#[test]
fn abort_lost_ms_fails_safe_when_loss_cannot_be_quantified() {
    let mut t = test_title(0, 100);
    t.size_bytes = 0;
    t.duration_secs = 0.0;
    let damage = [(0u64, 4096u64)];

    // Zero bitrate + real in-title loss -> unquantifiable, not zero.
    let ms = abort_lost_ms(false, &t, &damage, 0.0);
    assert!(ms.is_nan(), "zero-bitrate loss must be NaN, got {ms}");
    assert!(
        loss_aborts(abort_lost_bytes(false, &t, &damage), ms, 0),
        "an unquantifiable loss aborts the perfect gate"
    );

    // The scope hole: a title with NO EXTENTS can't be scoped, so
    // `bytes_bad_in_title` answers 0 — indistinguishable from "clean". A
    // naive `lost_bytes == 0 -> 0.0` guard would wave this through.
    let mut no_extents = libfreemkv::DiscTitle::empty();
    no_extents.size_bytes = 1_000_000;
    no_extents.duration_secs = 100.0;
    assert!(no_extents.extents.is_empty());
    let ms = abort_lost_ms(false, &no_extents, &damage, 8_250_000.0);
    assert!(
        ms.is_nan(),
        "an unscopable title with damage must be NaN, got {ms}"
    );

    // ISO scope is whole-disc, so it never needs extents: still quantified.
    let ms_iso = abort_lost_ms(true, &no_extents, &damage, 8_250_000.0);
    assert!(ms_iso > 0.0 && ms_iso.is_finite(), "iso scope: {ms_iso}");
}

/// The other direction: genuinely no loss must stay 0.0, or every clean rip
/// aborts. This is the guard that makes the NaN above safe to add.
#[test]
fn abort_lost_ms_reports_zero_for_a_genuinely_clean_rip() {
    let t = test_title(0, 100);
    assert_eq!(
        abort_lost_ms(false, &t, &[], 0.0),
        0.0,
        "no damage, no bitrate"
    );
    assert_eq!(abort_lost_ms(true, &t, &[], 0.0), 0.0, "iso, no damage");
    // Damage entirely OUTSIDE the title is not this title's loss (the
    // autorip test `mkv_resume_ignores_out_of_title_loss` pins this).
    let outside = [(500_000_000u64, 2048u64)];
    assert_eq!(abort_lost_ms(false, &t, &outside, 8_250_000.0), 0.0);
}

// The LIVE abort gate must not answer "0 ms lost" for damage it cannot measure —
// `multipass_rip_inner`'s hand-rolled pair had a hole where an extents-less title made it
// return 0.0.
#[test]
fn unmeasurable_in_title_loss_is_never_reported_as_zero() {
    let empty = libfreemkv::DiscTitle::empty();
    let damage = [(0u64, 8192u64)];

    // The exact shape the live gate builds.
    assert!(empty.extents.is_empty());
    assert!(loss_is_unscopable(false, &empty, &damage));

    // And the two functions the live gate actually calls still answer the
    // misleading zero — which is why the gate needs the predicate, not a
    // change to either of them.
    assert_eq!(
        abort_lost_bytes(false, &empty, &damage),
        0,
        "extents-less scoping still answers 0; the guard is what catches it"
    );

    // Now the GATE'S OWN decision function, not the predicate in isolation.
    // An earlier version asserted only `loss_is_unscopable(..)` and stayed
    // green with the guard deleted — the bug was the gate not consulting it.
    let (lost_ms, why) = end_of_recovery_lost_ms(/* promotion_intact */ true, &empty, &damage);
    assert!(lost_ms.is_nan(), "gate answered {lost_ms}, not NaN");
    assert!(why.is_some(), "an unquantifiable verdict must say why");
    // The verdict reports it in bytes: the whole disc's, since it cannot be scoped.
    let v = loss_verdict(false, &[&empty], &damage, 0);
    assert_eq!(v.lost_bytes, 8192);
    assert!(v.aborts, "the perfect gate aborts on the unscoped bytes");
}

/// The gate must still produce a real number when the loss IS measurable —
/// the guard must not swallow the normal path.
#[test]
fn the_gate_still_quantifies_a_measurable_loss() {
    let mut t = test_title(0, 100);
    t.size_bytes = 1_000_000;
    t.duration_secs = 100.0;
    // 100_000 bad bytes, all of them INSIDE the title's 0..100-sector
    // extent, so the gate's scoping and the millisecond scoping agree:
    // 100_000 / 1_000_000 * 100 s = 10 s.
    let (ms, why) = end_of_recovery_lost_ms(true, &t, &[(0, 100_000)]);
    assert!(
        why.is_none(),
        "measurable loss must not be flagged: {why:?}"
    );
    assert!((ms - 10_000.0).abs() < 1e-6, "expected 10s, got {ms}");
}

/// A failed promotion still wins over everything else.
#[test]
fn the_gate_reports_an_incomplete_damage_record_first() {
    let t = test_title(0, 100);
    let (ms, why) = end_of_recovery_lost_ms(false, &t, &[]);
    assert!(ms.is_nan());
    assert!(why.unwrap().contains("damage record is incomplete"));
}

/// ISO scope sums the bad ranges whole-disc and needs no extents, so it is
/// never unscopable — the guard must not fire there.
#[test]
fn iso_scope_is_never_unscopable() {
    let empty = libfreemkv::DiscTitle::empty();
    let damage = [(0u64, 8192u64)];
    assert!(!loss_is_unscopable(true, &empty, &damage));
    assert_eq!(abort_lost_bytes(true, &empty, &damage), 8192);
}

/// And the direction that matters most: no damage means the guard cannot
/// fire, so a clean rip is never turned into an abort.
#[test]
fn a_clean_rip_is_never_made_unscopable() {
    let empty = libfreemkv::DiscTitle::empty();
    assert!(!loss_is_unscopable(false, &empty, &[]));
    let t = test_title(0, 100);
    assert!(!loss_is_unscopable(false, &t, &[]));
    assert!(!loss_is_unscopable(false, &t, &[(0, 4096)]));
}

// The title's own size/duration wins; missing metadata falls back to an estimated rate
// (never NaN), so a rip is never stopped just because a title lacks a size or duration.
#[test]
fn lost_ms_uses_the_title_rate_else_an_estimate() {
    let damage = 4096u64;

    let mut ok = libfreemkv::DiscTitle::empty();
    ok.size_bytes = 1_000_000;
    ok.duration_secs = 100.0;
    // 4096 bytes at 10_000 B/s = 409.6 ms.
    let ms = main_title_lost_ms(&ok, damage);
    assert!((ms - 409.6).abs() < 1e-6, "expected 409.6 ms, got {ms}");

    let mut size_only = libfreemkv::DiscTitle::empty();
    size_only.size_bytes = 1_000_000;
    let mut dur_only = libfreemkv::DiscTitle::empty();
    dur_only.duration_secs = 100.0;
    let mut zero_dur = libfreemkv::DiscTitle::empty();
    zero_dur.size_bytes = 1_000_000;
    for (name, t) in [
        ("size only", size_only),
        ("duration only", dur_only),
        ("zero duration", zero_dur),
        ("neither", libfreemkv::DiscTitle::empty()),
    ] {
        let ms = main_title_lost_ms(&t, damage);
        assert!(
            ms.is_finite() && ms > 0.0,
            "{name}: expected an estimate, got {ms}"
        );
    }

    // No damage is genuinely zero regardless of the title.
    assert_eq!(main_title_lost_ms(&libfreemkv::DiscTitle::empty(), 0), 0.0);
}

// `abort_lost_ms`'s arithmetic, pinned so the operators cannot drift: the
// mutation run swapped `/`/`*` in the conversion and nothing failed — a
// wrong operator here is a wrong abort decision.
#[test]
fn abort_lost_ms_converts_bytes_to_milliseconds_exactly() {
    let mut t = test_title(0, 100);
    t.size_bytes = 1_000_000;
    t.duration_secs = 100.0;
    // Whole-disc scope so the figure is the bad-byte sum, not extent-scoped.
    // 2 MB at 1 MB/s = 2 s = 2000 ms.
    let ms = abort_lost_ms(true, &t, &[(0, 2_000_000)], 1_000_000.0);
    assert!((ms - 2_000.0).abs() < 1e-6, "expected 2000 ms, got {ms}");
    // Halving the rate doubles the time — pins the division, not just the
    // magnitude.
    let ms_slow = abort_lost_ms(true, &t, &[(0, 2_000_000)], 500_000.0);
    assert!(
        (ms_slow - 4_000.0).abs() < 1e-6,
        "expected 4000 ms, got {ms_slow}"
    );
    // Doubling the bytes doubles the time — pins the multiplication.
    let ms_more = abort_lost_ms(true, &t, &[(0, 4_000_000)], 1_000_000.0);
    assert!(
        (ms_more - 4_000.0).abs() < 1e-6,
        "expected 4000 ms, got {ms_more}"
    );
}

#[test]
fn classify_damage_tiers() {
    use crate::DamageSeverity::*;
    assert_eq!(classify_damage(0, 0.0), Clean);
    assert_eq!(classify_damage(1, 5.0), Cosmetic);
    assert_eq!(classify_damage(50, 999.0), Cosmetic);
    assert_eq!(classify_damage(51, 0.0), Moderate);
    assert_eq!(classify_damage(10, 1_000.0), Moderate);
    // Both sides of the sector boundary, because the tier doc used to
    // claim 500 for Moderate ("51–500") AND for Serious ("500+").
    assert_eq!(classify_damage(499, 0.0), Moderate);
    assert_eq!(classify_damage(500, 0.0), Serious);
    assert_eq!(classify_damage(10, 30_000.0), Serious);
}

#[test]
fn main_title_lost_ms_scales_by_own_size_and_runtime() {
    let mut t = libfreemkv::DiscTitle::empty();
    t.size_bytes = 1_000_000;
    t.duration_secs = 100.0;
    // 10% of the title bad → 10% of 100s = 10s = 10_000 ms.
    assert!((main_title_lost_ms(&t, 100_000) - 10_000.0).abs() < 1e-6);
    // No loss → 0.
    assert_eq!(main_title_lost_ms(&t, 0), 0.0);
}

// `end_of_recovery_lost_ms` must scope BOTH the bad-byte count AND its ms divisor to the
// passed `title`, never to `disc.titles.first()`.
#[test]
fn end_of_recovery_lost_ms_scopes_divisor_to_the_passed_title() {
    let mut title = test_title(0, 100);
    title.size_bytes = 1_000_000;
    title.duration_secs = 100.0;
    let (ms, why) = end_of_recovery_lost_ms(true, &title, &[(0, 100_000)]);
    assert!(
        why.is_none(),
        "measurable loss must not be flagged: {why:?}"
    );
    assert!((ms - 10_000.0).abs() < 1e-6, "expected 10s, got {ms}");
}

/// Minimal `DiscTitle` whose single extent spans `[start_lba, start_lba +
/// sector_count)`. Mirrors autorip's `test_title` helper — only `extents`
/// matters for `bytes_bad_in_title` / the scope-aware gates.
fn test_title(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: "00800.mpls".to_string(),
        playlist_id: 800,
        duration_secs: 7200.0,
        size_bytes: (sector_count as u64) * 2048,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::disc::Extent {
            start_lba,
            sector_count,
        }],
        content_format: libfreemkv::disc::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

// ── Multipass strategy decisions — relocated from autorip's char_* tests.
//    autorip keeps its own characterization coverage via its call path;
//    these are the engine's own coverage now that it owns the implementation. ──

#[test]
fn plan_passes_single_vs_multipass() {
    let single = plan_passes(0);
    assert!(!single.multipass);
    assert_eq!(single.sweep_passes, 0);
    assert_eq!(single.patch_passes, 0);
    assert_eq!(single.total_passes, 0);

    for n in 1u8..=10 {
        let plan = plan_passes(n);
        assert!(plan.multipass);
        assert_eq!(plan.sweep_passes, 1);
        assert_eq!(plan.patch_passes, n);
        assert_eq!(plan.total_passes, n + 2);
    }
}

#[test]
fn scope_bad_bytes_mkv_scopes_to_title() {
    let title = test_title(0, 48_829);

    let out_of_title = [(500_000_000u64, 2048u64)];
    let bad = scope_bad_bytes(false, &out_of_title, &title);
    assert_eq!(bad, 0);
    assert!(scope_converged(bad));

    let in_title = [(1_000_000u64, 2048u64)];
    let bad_in = scope_bad_bytes(false, &in_title, &title);
    assert_eq!(bad_in, 2048);
    assert!(!scope_converged(bad_in));
}

#[test]
fn scope_bad_bytes_iso_scopes_whole_disc() {
    let title = test_title(0, 48_829);

    let out_of_title = [(500_000_000u64, 2048u64)];
    let bad = scope_bad_bytes(true, &out_of_title, &title);
    assert_eq!(bad, 2048);
    assert!(!scope_converged(bad));

    assert!(scope_converged(scope_bad_bytes(true, &[], &title)));
}

#[test]
fn patch_made_progress_zero_vs_nonzero() {
    assert!(!patch_made_progress(0));
    assert!(patch_made_progress(1));
    assert!(patch_made_progress(2048));
}

#[test]
fn patch_pass_decision_matrix() {
    assert_eq!(patch_pass_decision(0, None), PatchDecision::Converged);
    assert_eq!(patch_pass_decision(0, Some(0)), PatchDecision::Converged);
    assert_eq!(patch_pass_decision(0, Some(999)), PatchDecision::Converged);
    assert_eq!(patch_pass_decision(2048, None), PatchDecision::Continue);
    assert_eq!(
        patch_pass_decision(2048, Some(0)),
        PatchDecision::NoProgress
    );
    assert_eq!(
        patch_pass_decision(2048, Some(4096)),
        PatchDecision::Continue
    );
}

// An unquantifiable loss must classify as SERIOUS, not Cosmetic: every
// NaN comparison in Rust is false, so both tier tests used to fall
// through while `loss_aborts` was simultaneously refusing to deliver it.
#[test]
fn an_unquantifiable_loss_is_serious_not_cosmetic() {
    // Few enough bad sectors that every sector-based tier is false, so the
    // verdict rests entirely on the NaN.
    assert_eq!(
        classify_damage(10, f64::NAN),
        crate::DamageSeverity::Serious,
        "an untimeable loss badges Serious"
    );
    // And a quantified small loss still classifies normally.
    assert_eq!(classify_damage(10, 0.0), crate::DamageSeverity::Cosmetic);
}

// The pass count must not overflow at the top of the u8 range: 255 is reachable via
// `.min(u8::MAX as u32)`, and `max_retries + 2` used to panic in dev / wrap to 1 in
// release.
#[test]
fn the_pass_count_saturates_instead_of_wrapping() {
    let plan = plan_passes(u8::MAX);
    assert_eq!(
        plan.patch_passes,
        u8::MAX - 2,
        "capped so every pass is counted"
    );
    assert_eq!(
        plan.total_passes,
        u8::MAX,
        "total_passes wrapped: in release this silently becomes 1"
    );
    // (No `total_passes >= patch_passes` check: both operands are pinned
    // to `u8::MAX`, so it could only read `255 >= 255` — an assertion
    // that can't fail is noise for a test about overflow.) Ordinary case unchanged.
    assert_eq!(plan_passes(5).total_passes, 7);
}

// ── A cancelled or wedged pass has not MEASURED the disc ──────────────
// `bytes_pending` counts NonTried — the un-attempted remainder ahead of the
// sweep head — folding it into the damage score made an interruption look catastrophic.

/// A rip cancelled seconds into a 66 GB disc has found nothing bad. It
/// must not be scored as though the whole disc were unreadable.
#[test]
fn an_early_cancel_is_not_scored_as_a_destroyed_disc() {
    const DISC: u64 = 66_000_000_000;
    // Nothing unreadable; essentially the whole disc still un-attempted.
    assert_eq!(
        interrupted_severity(0, DISC),
        crate::DamageSeverity::Cosmetic,
        "an immediate cancel must not claim damage nobody measured — but \
             it must not read Clean beside a pending count either"
    );
    // The number the old aggregate handed to classify_damage, for contrast.
    assert!(
        bad_sector_count(0, DISC) > 30_000_000,
        "this is what used to be scored: the entire un-read disc"
    );
    assert_eq!(
        classify_damage(bad_sector_count(0, DISC), 0.0),
        crate::DamageSeverity::Serious,
        "and it stamped Serious on a disc nobody had looked at"
    );
}

/// Damage that WAS found still scores. A cancel is not an amnesty:
/// 300 MB of unreadable sectors on the record must reach a real tier, not
/// merely the not-Clean floor.
#[test]
fn a_cancel_does_not_erase_damage_already_found() {
    assert_eq!(
        interrupted_severity(300 * 1024 * 1024, 0),
        crate::DamageSeverity::Serious,
        "unreadable bytes are KNOWN damage and must still score in full"
    );
}

/// Nothing outstanding and nothing bad really is Clean — the floor must
/// not deny a badge that was earned.
#[test]
fn an_interrupted_run_with_nothing_outstanding_is_still_clean() {
    assert_eq!(interrupted_severity(0, 0), crate::DamageSeverity::Clean);
}

/// The wedge branch itself. It needs a live USB-bridge crash to reach in
/// place, which is why the decision was extracted: `wedged_exit` was not
/// read at all, and nothing in the suite could have said so.
#[test]
fn a_transport_fault_ends_the_pass_instead_of_exhausting_the_recovery() {
    assert_eq!(pass_exit(false, false), PassExit::Continue);
    assert_eq!(
        pass_exit(false, true),
        PassExit::Wedged,
        "a wedged pass must end the loop — falling through to the \
             exhaustion gate promotes its never-attempted ranges to \
             permanently Unreadable, and a re-run then skips them forever"
    );
    assert_eq!(
        pass_exit(true, false),
        PassExit::Cancelled,
        "a cancel is the operator's, and is reported as such"
    );
    assert_eq!(
        pass_exit(true, true),
        PassExit::Cancelled,
        "both at once is the user's Stop: the more specific thing to say"
    );
}

// A transport fault is not an exhausted pass, and must not be reported as a cancel either —
// drives a real `multipass_rip` end to end, since the old hand-built-result version tested
// nothing.
#[test]
fn a_wedged_result_is_distinguishable_from_a_cancelled_one() {
    // Marginal (RECOVERED) errors at `bad_lba` while the sweep walks
    // forward, then a TRANSPORT FAILURE (status 0xFF) once the sweep
    // reaches the end and the patch pass returns for the leftover range.
    struct WedgeOnPatchReader {
        capacity: u32,
        bad_lba: u32,
        sweep_done: bool,
    }
    impl libfreemkv::SectorSource for WedgeOnPatchReader {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::Result<usize> {
            let end = lba + count as u32;
            if end >= self.capacity {
                self.sweep_done = true;
            }
            if lba <= self.bad_lba && self.bad_lba < end {
                return Err(libfreemkv::Error::DiscRead {
                    sector: self.bad_lba as u64,
                    status: Some(if self.sweep_done {
                        libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE
                    } else {
                        libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION
                    }),
                    sense: if self.sweep_done {
                        None
                    } else {
                        // RECOVERED (marginal): the sweep marks NonTrimmed
                        // without the 30s damage-zone cooldown a hard error
                        // would earn — the pass this test is about is the patch one.
                        Some(libfreemkv::scsi::ScsiSense {
                            sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                            asc: 0x17,
                            ascq: 0x01,
                        })
                    },
                });
            }
            let n = ((count as usize) * 2048).min(buf.len());
            buf[..n].fill(0);
            // BYTES, per `SectorSource::read_sectors`' contract.
            Ok(n)
        }
        fn capacity_sectors(&self) -> u32 {
            self.capacity
        }
    }

    let (_dir, iso) = scratch_iso("wedged-patch-pass");
    let sectors = 8192u32;
    let disc = test_disc(sectors, vec![test_title(0, sectors)]);
    let mut reader = WedgeOnPatchReader {
        capacity: sectors,
        bad_lba: 4000,
        sweep_done: false,
    };
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    // Logs only; nothing cancels, so `halted` can only come from the code
    // under test.
    let sink = HookSink::new("never-logged-trigger", false, Box::new(|| {}));
    let result = multipass_rip(&disc, &mut reader, &iso, &raw_job(&iso), &opts, &sink)
        .expect("a wedged pass is a partial result, not an Err");

    assert!(
        result.wedged,
        "a patch pass killed by a bridge crash must be reported as wedged"
    );
    assert!(
        !result.halted,
        "nobody pressed Stop — reporting a wedge as a cancel tells the \
             operator to do the wrong thing"
    );
    assert!(
        !result.complete,
        "a wedged pass left retryable damage behind, so the run is not done"
    );
    assert!(
        result.pending_bytes > 0,
        "the ranges the crashed pass never reached must still be pending, \
             not promoted to permanently Unreadable — a re-run has to retry them"
    );
    assert_eq!(
        result.unreadable_bytes, 0,
        "nothing was CONFIRMED lost: the end-of-recovery promotion must not \
             have run on the wedged exit"
    );
    assert!(
        sink.logged(Level::Warn, "transport fault"),
        "the operator has to be told the drive needs a power-cycle"
    );
    assert!(
        result.main_lost_ms.is_nan(),
        "nothing measured the loss beside {} pending bytes; 0.0 claims none was lost",
        result.pending_bytes
    );
}

#[test]
fn end_of_recovery_promotion_covers_every_maybe_state() {
    let (from, to) = end_of_recovery_promotion();
    assert_eq!(to, SectorStatus::Unreadable);
    assert!(from.contains(&SectorStatus::NonTrimmed));
    assert!(
        from.contains(&SectorStatus::NonScraped),
        "NonScraped survives every patch pass as a failed read; if it is \
             not promoted it never reaches the abort gate, which reads only \
             Unreadable, and the loss is delivered as a clean rip"
    );
    assert!(!from.contains(&SectorStatus::Finished));
    assert!(!from.contains(&SectorStatus::NonTried));

    // The promotion source set must stay a subset of what the rest of the
    // module already calls damage, or the two rules drift apart.
    let damage = crate::recovery::mapfile::damage_sector_statuses();
    for st in from {
        assert!(
            damage.contains(st),
            "{st:?} promoted but not counted as damage"
        );
    }

    let bad_set = bad_sector_statuses();
    assert!(bad_set.contains(&SectorStatus::NonTrimmed));
    assert!(bad_set.contains(&SectorStatus::Unreadable));
    assert!(!bad_set.contains(&SectorStatus::Finished));
}

#[test]
fn a_title_without_metadata_gets_an_estimated_loss_not_an_abort() {
    // No size, no duration: timed at the format's typical rate, so the loss is measured
    // and a positive tolerance decides, rather than a NaN forcing an abort.
    let ms = main_title_lost_ms(&libfreemkv::DiscTitle::empty(), 4096);
    assert!(ms.is_finite() && ms > 0.0, "expected an estimate, got {ms}");
}

// ── multipass_rip strategy LOOP, exercised headlessly (hard rule #2) ── Every double must
// honour the contract: `read_sectors` returns BYTES written, not sectors.
#[test]
fn the_doubles_return_a_byte_count_like_the_trait_says() {
    use libfreemkv::SectorSource as _;
    let mut buf = vec![0u8; 4 * 2048];
    let mut zero = ZeroReader { capacity: 64 };
    assert_eq!(
        zero.read_sectors(0, 4, &mut buf, false).unwrap(),
        8192,
        "4 sectors is 8192 BYTES"
    );
    let mut spots = MultiSpotReader {
        capacity: 64,
        spots: vec![],
    };
    assert_eq!(spots.read_sectors(0, 4, &mut buf, false).unwrap(), 8192);
}

/// A `SectorSource` whose entire capacity reads back as zeros — the
/// clean-disc path. Mirrors `run.rs`'s private `ZeroReader`.
struct ZeroReader {
    capacity: u32,
}
impl libfreemkv::SectorSource for ZeroReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::Result<usize> {
        let n = ((count as usize) * 2048).min(buf.len());
        buf[..n].fill(0);
        // BYTES, per `SectorSource::read_sectors`' contract — not `count`.
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

// One deliberately-bad single-sector LBA: fails every read overlapping it until touched
// `heal_after` times, then reads clean forever. `heal_after: u32::MAX` never heals —
// permanent loss.
struct Spot {
    lba: u32,
    heal_after: u32,
    attempts: u32,
}

/// A `SectorSource` that is clean everywhere except a fixed set of
/// [`Spot`]s.
struct MultiSpotReader {
    capacity: u32,
    spots: Vec<Spot>,
}
impl libfreemkv::SectorSource for MultiSpotReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::Result<usize> {
        let end = lba + count as u32;
        for spot in &mut self.spots {
            if lba <= spot.lba && spot.lba < end {
                spot.attempts += 1;
                if spot.attempts <= spot.heal_after {
                    return Err(libfreemkv::Error::DiscRead {
                        sector: spot.lba as u64,
                        status: Some(2),
                        sense: Some(libfreemkv::scsi::ScsiSense {
                            sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                            asc: 0x17,
                            ascq: 0x01,
                        }),
                    });
                }
            }
        }
        let n = ((count as usize) * 2048).min(buf.len());
        buf[..n].fill(0);
        // BYTES, per `SectorSource::read_sectors`' contract — not `count`.
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

/// A clean `SectorSource` that stamps each sector with its own LBA (u32 LE
/// in the first 4 bytes), so a read-back of the output ISO can prove the
/// orchestration loop wrote the right bytes at the right offset.
struct PatternReader {
    capacity: u32,
}
impl libfreemkv::SectorSource for PatternReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::Result<usize> {
        let n = ((count as usize) * 2048).min(buf.len());
        for (i, chunk) in buf[..n].chunks_mut(2048).enumerate() {
            chunk.fill(0);
            if chunk.len() >= 4 {
                chunk[..4].copy_from_slice(&(lba + i as u32).to_le_bytes());
            }
        }
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

/// A minimal unencrypted `sectors`-sized disc with the given titles (may
/// be empty — several tests don't need a title at all).
fn test_disc(sectors: u32, titles: Vec<libfreemkv::DiscTitle>) -> libfreemkv::Disc {
    libfreemkv::Disc {
        volume_id: "TESTDISC".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: sectors,
        capacity_bytes: sectors as u64 * 2048,
        layers: 1,
        titles,
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

/// A fresh scratch dir + `out.iso` path for one test. The dir is removed when the returned
/// guard drops, so a failing assertion cannot leak the image.
fn scratch_iso(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("fmkv-engine-multipass-rip-{tag}-"))
        .tempdir()
        .unwrap();
    let iso = dir.path().join("out.iso");
    (dir, iso)
}

// The only single-pass test ran a CLEAN disc, where `main_lost_ms: 0.0` is
// indistinguishable from a hard-coded constant. On a DAMAGED disc a constant claims loss
// nothing measured.
#[test]
fn single_pass_reports_loss_as_unquantified_on_a_damaged_disc() {
    let (_dir, iso) = scratch_iso("single-damaged");
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![test_title(0, sectors)]);
    let total = sectors as u64 * 2048;

    // The reachable damaged-single-pass state is the RESUME one: a plain
    // copy aborts at the first read error, so damage only returns as a
    // RESULT after a prior run attempted the whole disc — build that mapfile.
    let mapfile_path = disc.mapfile_for(&iso);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = crate::recovery::mapfile::Mapfile::create(&mapfile_path, total, "vTEST").unwrap();
    mf.record(0, total, crate::recovery::mapfile::SectorStatus::Finished)
        .unwrap();
    mf.record(
        1000 * 2048,
        2048 * 8,
        crate::recovery::mapfile::SectorStatus::Unreadable,
    )
    .unwrap();
    mf.flush().unwrap();
    std::fs::write(&iso, vec![0u8; total as usize]).unwrap();

    // This path must not read the disc at all — it is terminal.
    let mut reader = ZeroReader { capacity: sectors };
    let mut job = raw_job(&iso);
    job.mode = crate::RipMode::Single;
    let opts = MultipassOpts {
        max_passes: 0,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let r = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("a fully-attempted mapfile with bad bytes is terminal, not an error");

    assert_eq!(r.passes, 1, "single-pass is one dispatch");
    assert!(
        r.unreadable_bytes + r.pending_bytes > 0,
        "fixture check: this run must come back damaged, else every \
             assertion below is vacuous"
    );
    assert!(
        !r.main_lost_ms.is_finite(),
        "single-pass measured nothing, so it must not report a NUMBER of \
             milliseconds lost beside {} unreadable + {} pending bytes",
        r.unreadable_bytes,
        r.pending_bytes,
    );
    // Severity comes from the sector count, not escalated merely because
    // loss is unquantified (that's the abort gate's rule, unused here).
    // The badge is spelled out, not recomputed via the producer's own fns.
    assert_eq!(
        r.severity,
        crate::DamageSeverity::Cosmetic,
        "8 bad sectors and no quantified time loss is a Cosmetic rip: not \
             Clean (bytes ARE missing), and not escalated by the unquantifiable \
             NaN either"
    );
    assert!(!r.aborted_for_loss, "single-pass has no abort gate");

    let _ = std::fs::remove_file(&mapfile_path);
}

#[test]
fn multipass_rip_single_pass_mode_is_one_dispatch_no_retry_loop() {
    // max_passes == 0 -> plan_passes(0).multipass == false: one
    // `recovery::copy` dispatch, no sweep/patch split, no abort gate.
    let (_dir, iso) = scratch_iso("single-pass");
    let sectors = 256u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = ZeroReader { capacity: sectors };
    let job = Job::new("disc:///dev/null", iso.to_string_lossy());
    let opts = MultipassOpts {
        max_passes: 0,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("single-pass dispatch should succeed on a clean synthetic disc");

    assert_eq!(result.passes, 1, "single-pass mode is exactly one pass");
    assert_eq!(result.unreadable_bytes, 0);
    assert_eq!(result.pending_bytes, 0);
    assert_eq!(result.good_bytes, sectors as u64 * 2048);
    assert!(result.complete);
    assert!(!result.halted);
    assert!(
        !result.aborted_for_loss,
        "single-pass never applies the abort gate"
    );
    // No mapfile-driven patch pass ever ran — no NonTrimmed/Unreadable
    // promotion logic touched, no bad_ranges built.
    assert_eq!(result.main_lost_ms, 0.0);
}

#[test]
fn multipass_rip_clean_disc_converges_with_zero_patch_passes() {
    // A fully-readable disc: Pass 1 sweep finds nothing bad, so the
    // patch loop's very first top-of-loop `scope_bad_bytes` check is
    // already 0 -> Converged -> break before any `recovery::patch` call.
    let (_dir, iso) = scratch_iso("clean");
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = ZeroReader { capacity: sectors };
    // Multipass implies raw (enforced in `multipass_rip`); a multipass
    // fixture must say so.
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("clean multipass recovery should succeed");

    assert_eq!(result.passes, 1, "sweep only — no patch pass needed");
    assert_eq!(result.unreadable_bytes, 0);
    assert_eq!(result.pending_bytes, 0);
    assert_eq!(result.good_bytes, sectors as u64 * 2048);
    assert!(result.complete);
    assert!(!result.halted);
    assert!(!result.aborted_for_loss);
    assert_eq!(result.main_lost_ms, 0.0);
}

#[test]
fn multipass_rip_writes_each_sector_to_its_own_lba() {
    // Counters can't tell right-bytes-at-right-LBA from zeros-at-wrong-LBA.
    // Stamp each sector with its LBA, then read the output ISO back: a
    // wrong-offset or stale cursor in the loop would mismatch the stamp.
    let (_dir, iso) = scratch_iso("lba-pattern");
    let sectors = 256u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = PatternReader { capacity: sectors };
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("clean patterned multipass recovery should succeed");
    assert_eq!(result.good_bytes, sectors as u64 * 2048);
    assert!(result.complete);

    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&iso).unwrap();
    let mut mismatches = Vec::new();
    for &lba in &[0u32, 1, 100, 200, 255] {
        f.seek(SeekFrom::Start(lba as u64 * 2048)).unwrap();
        let mut sector = [0u8; 2048];
        f.read_exact(&mut sector).unwrap();
        let stamp = u32::from_le_bytes(sector[..4].try_into().unwrap());
        if stamp != lba {
            mismatches.push((lba, stamp));
        }
    }

    assert!(
        mismatches.is_empty(),
        "each LBA's stamp must land at byte LBA*2048; (expected, got) \
             mismatches = {mismatches:?}"
    );
}

#[test]
fn multipass_rip_recoverable_bad_sector_converges_after_a_patch_pass() {
    // One sector fails Pass 1's touch, then reads clean from Pass 2 on:
    // the patch pass recovers it, muxable scope hits 0 bad bytes, and the
    // NEXT loop-top check (Converged) stops early, well under the 5-pass cap.
    let (_dir, iso) = scratch_iso("recoverable");
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: vec![Spot {
            lba: 1000,
            heal_after: 1,
            attempts: 0,
        }],
    };
    // Multipass implies raw (enforced in `multipass_rip`); a multipass
    // fixture must say so.
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("recoverable bad sector should converge");

    assert_eq!(result.passes, 2, "sweep + exactly 1 patch pass to converge");
    assert_eq!(result.unreadable_bytes, 0, "fully recovered — nothing lost");
    assert_eq!(result.pending_bytes, 0);
    assert!(result.complete);
    assert!(!result.halted);
    assert!(!result.aborted_for_loss);
}

#[test]
fn measured_scope_bad_is_unmeasured_only_when_unscopable() {
    let empty = libfreemkv::DiscTitle::empty();
    let damage = [(0u64, 4096u64)];
    assert_eq!(measured_scope_bad(false, &damage, &empty), None);
    assert!(!pre_pass_converged(
        measured_scope_bad(false, &damage, &empty),
        4096
    ));
    assert_eq!(measured_scope_bad(false, &[], &empty), Some(0));
    assert_eq!(measured_scope_bad(true, &damage, &empty), Some(4096));
    let t = test_title(0, 2_000);
    assert_eq!(measured_scope_bad(false, &damage, &t), Some(4096));
}

// MKV scope with no main-title extents: scoped bad bytes read 0 whatever the
// damage. That must not pass as "converged" and skip the pass that recovers it.
#[test]
fn multipass_rip_unscopable_mkv_loss_still_runs_patch_passes() {
    let (_dir, iso) = scratch_iso("unscopable-mkv");
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: vec![Spot {
            lba: 1000,
            heal_after: 1,
            attempts: 0,
        }],
    };
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: false,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("recoverable damage must not fail the rip");

    assert_eq!(
        result.passes, 2,
        "unscopable loss must still earn a patch pass"
    );
    assert_eq!(result.unreadable_bytes, 0, "the patch pass recovered it");
    assert!(!result.aborted_for_loss, "nothing is lost once recovered");
    assert!(result.complete);
}

#[test]
fn multipass_rip_permanent_loss_past_tolerance_aborts() {
    // A sector that NEVER heals: Pass 1 marks it NonTrimmed, the patch
    // pass recovers nothing (NoProgress -> stop early), promotion turns it
    // Unreadable, and — whole-disc ISO scope + zero tolerance — the gate fires.
    let (_dir, iso) = scratch_iso("permanent-loss");
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: vec![Spot {
            lba: 1000,
            heal_after: u32::MAX,
            attempts: 0,
        }],
    };
    // Multipass implies raw (enforced in `multipass_rip`); a multipass
    // fixture must say so.
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("a permanently-bad sector is a reported result, not an Err");

    // NoProgress stops the retry loop before the 5-pass cap: the first
    // patch pass still recovers the bad sector's readable ECC-block
    // neighbours, so pin "stopped early", not an exact ECC-dependent count.
    assert!(
        result.passes > 1 && result.passes < 1 + opts.max_passes,
        "expected the retry loop to exhaust progress before the pass cap, got {} passes",
        result.passes
    );
    // The exact count matters: a loose range let `== NoProgress` be
    // mutated to `!=` and stay green (breaking after the first, progress-
    // making pass). Pass 1 heals ECC neighbours; pass 2 must stop the loop.
    assert_eq!(
        result.passes, 3,
        "1 sweep + 2 patch passes: the loop must stop on the pass that \
             recovered nothing, not on the pass that made progress"
    );
    assert!(
        result.unreadable_bytes > 0,
        "the sector was never recovered"
    );
    assert!(!result.halted);
    assert!(
        result.aborted_for_loss,
        "zero tolerance + confirmed loss must abort"
    );
    assert!(!result.complete);
}

#[test]
fn multipass_rip_respects_max_passes_bound_even_with_ongoing_progress() {
    // Two bad sectors: one heals next touch (progress, so NoProgress never
    // fires); the other never heals (scope never hits 0, so Converged never
    // fires). With max_passes == 1 the loop must stop purely on budget.
    let (_dir, iso) = scratch_iso("max-passes-bound");
    let sectors = 8_192u32;
    let disc = test_disc(sectors, vec![]);
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: vec![
            Spot {
                lba: 1_000,
                heal_after: 1,
                attempts: 0,
            },
            Spot {
                lba: 6_000,
                heal_after: u32::MAX,
                attempts: 0,
            },
        ],
    };
    // Multipass implies raw (enforced in `multipass_rip`).
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    let opts = MultipassOpts {
        max_passes: 1,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("bounded run is a reported result, not an Err");

    assert_eq!(
        result.passes, 2,
        "sweep + exactly the 1 allowed patch pass, no more"
    );
    assert!(
        !result.complete,
        "the permanent spot is still bad — never converged"
    );
    assert!(result.unreadable_bytes > 0);
}

#[test]
fn multipass_rip_scope_bad_bytes_wiring_drives_convergence_by_output_kind() {
    // A permanently-bad sector OUTSIDE the muxed title's extents. MKV/M2TS
    // scope sees 0 bad bytes and converges immediately, without calling
    // `recovery::patch`; ISO's whole-disc scope grinds a pass, then aborts.
    let title = test_title(0, 2_000); // extents [0, 2000)
    let bad_lba = 6_000; // outside the title's extents
    let sectors = 8_192u32;
    // Multipass implies raw (enforced in `multipass_rip`).
    let mut job = Job::new("disc:///dev/null", "placeholder");
    job.raw = true;

    // MKV/M2TS scope: out-of-title damage doesn't earn a retry pass.
    {
        let (_dir, iso) = scratch_iso("scope-mkv");
        let disc = test_disc(sectors, vec![title.clone()]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![Spot {
                lba: bad_lba,
                heal_after: u32::MAX,
                attempts: 0,
            }],
        };
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: false,
        };
        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("out-of-title loss must not fail the rip");
        assert_eq!(
            result.passes, 1,
            "muxable scope was already 0 bad bytes — no patch pass ran"
        );
        assert!(!result.aborted_for_loss, "loss is entirely out of scope");
    }

    // ISO scope: the SAME out-of-title byte counts whole-disc and aborts.
    {
        let (_dir, iso) = scratch_iso("scope-iso");
        let disc = test_disc(sectors, vec![title.clone()]);
        let mut reader = MultiSpotReader {
            capacity: sectors,
            spots: vec![Spot {
                lba: bad_lba,
                heal_after: u32::MAX,
                attempts: 0,
            }],
        };
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: true,
        };
        let result = multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("whole-disc-scoped loss is still a reported result");
        // Pinned exactly, as above: a loose `passes > 1 && < 1 + max_passes`
        // range let `== NoProgress` mutate to `!=` and stay green (breaking
        // after the first, progress-making pass). Pass 2 must stop the loop.
        assert_eq!(
            result.passes, 3,
            "whole-disc scope must see the bad byte and keep patching until \
                 a pass recovers nothing: 1 sweep + 2 patch passes"
        );
        assert!(
            result.aborted_for_loss,
            "ISO scope counts every byte — this loss must abort"
        );
    }
}

// ── The three decisions lifted out of `multipass_rip_inner`. ──

#[test]
fn pass_should_decrypt_is_the_negation_of_raw() {
    assert!(
        !pass_should_decrypt(true),
        "a raw pass must never decrypt — raw exists to keep the ciphertext"
    );
    assert!(
        pass_should_decrypt(false),
        "a non-raw pass must decrypt, or the ISO is unplayable ciphertext"
    );
}

#[test]
fn bad_sector_count_divides_bytes_into_whole_sectors() {
    assert_eq!(bad_sector_count(0, 0), 0);
    assert_eq!(bad_sector_count(2048, 0), 1);
    assert_eq!(bad_sector_count(0, 2048), 1);
    assert_eq!(
        bad_sector_count(4096, 2048),
        3,
        "both counters are summed, then converted once"
    );
    assert_eq!(
        bad_sector_count(2047, 0),
        0,
        "a partial sector rounds down, it does not become a whole bad sector"
    );
    assert_eq!(
        bad_sector_count(u64::MAX, u64::MAX),
        u64::MAX / 2048,
        "the sum saturates instead of wrapping to a tiny damage count"
    );
}

// The final verdict must score DAMAGE, never un-attempted disc: a 66 GB disc with 64 GB
// never attempted must not read ~33M bad sectors (`Serious`) via `bytes_pending`.
#[test]
fn the_final_score_ignores_un_attempted_sectors() {
    let nothing_failed_much_unread = MapStats {
        bytes_total: 66_000_000_000,
        bytes_good: 2_000_000_000,
        bytes_unreadable: 0,
        // 64 GB pending, ALL of it never attempted.
        bytes_pending: 64_000_000_000,
        bytes_nontried: 64_000_000_000,
        bytes_retryable: 0,
        num_bad_ranges: 0,
        main_lost_ms: 0.0,
    };
    assert_eq!(
        end_of_recovery_bad_sectors(&nothing_failed_much_unread),
        0,
        "un-attempted disc is not damage"
    );
    assert_eq!(
        classify_damage(
            end_of_recovery_bad_sectors(&nothing_failed_much_unread),
            0.0
        ),
        crate::DamageSeverity::Clean,
    );
}

/// The other side: real damage still scores, and both damage kinds count.
/// 8 unreadable sectors + 4 retryable = 12 → `Cosmetic` (1..=50), a
/// literal read off `classify_damage`'s documented boundaries.
#[test]
fn the_final_score_counts_unreadable_and_retryable_damage() {
    let damaged = MapStats {
        bytes_total: 66_000_000_000,
        bytes_good: 65_000_000_000,
        bytes_unreadable: 8 * 2048,
        bytes_pending: 4 * 2048 + 1_000_000_000,
        bytes_nontried: 1_000_000_000,
        bytes_retryable: 4 * 2048,
        num_bad_ranges: 2,
        main_lost_ms: 0.0,
    };
    assert_eq!(end_of_recovery_bad_sectors(&damaged), 12);
    assert_eq!(
        classify_damage(end_of_recovery_bad_sectors(&damaged), 0.0),
        crate::DamageSeverity::Cosmetic,
    );
}

// An UNMEASURED muxable scope must never read as a converged one: a failed mapfile load
// used to fall back to zero, the ONE value meaning "converged, stop retrying".
#[test]
fn an_unmeasured_scope_never_converges() {
    assert_eq!(
        patch_pass_decision_measured(None, None),
        PatchDecision::Continue,
        "unknown scope must run the pass, not declare victory"
    );
    // Zero is still convergence when it was actually MEASURED — the
    // distinction this function exists to draw.
    assert_eq!(
        patch_pass_decision_measured(Some(0), None),
        PatchDecision::Converged,
    );
    // A pass that recovered nothing is exhausted whether or not the scope
    // could be measured: that fact comes from the pass, not the mapfile.
    assert_eq!(
        patch_pass_decision_measured(None, Some(0)),
        PatchDecision::NoProgress,
    );
    assert_eq!(
        patch_pass_decision_measured(None, Some(1_000_000)),
        PatchDecision::Continue,
    );
    // And it still defers to the measured answer when there is one.
    assert_eq!(
        patch_pass_decision_measured(Some(4096), Some(0)),
        PatchDecision::NoProgress,
    );
    assert_eq!(
        patch_pass_decision_measured(Some(4096), Some(1_000_000)),
        PatchDecision::Continue,
    );
}

// FAIL-OPEN GUARD: an empty mapfile (Pass 1 read nothing) is `Some(0)` bad bytes with zero
// good — measured reads it as Converged and fakes "100%". The loop-top gate adds `last_good
// > 0`.
#[test]
fn char_pre_pass_converged_requires_real_coverage() {
    // Empty mapfile: 0 good, Some(0) bad. Bare decision says Converged, but
    // the guarded gate must NOT — nothing was ripped, so run the pass.
    assert_eq!(
        patch_pass_decision_measured(Some(0), None),
        PatchDecision::Converged,
    );
    assert!(
        !pre_pass_converged(Some(0), 0),
        "empty mapfile (0 good, 0 bad) must NOT be treated as converged"
    );
    // Genuinely-complete scope: good spans the scope, zero bad → converged,
    // so redundant patch passes are still skipped.
    assert!(
        pre_pass_converged(Some(0), 4096),
        "complete scope (good>0, bad==0) must still converge"
    );
    // Scope still bad → never converged regardless of good coverage.
    assert!(!pre_pass_converged(Some(2048), 4096));
    assert!(!pre_pass_converged(Some(2048), 0));
    // Unreadable mapfile (`None`) never converges, good bytes or not — the
    // measured gate already refuses `None`, and the guard preserves that.
    assert!(!pre_pass_converged(None, 4096));
    assert!(!pre_pass_converged(None, 0));
}

// ── Three `multipass_rip_inner` fail-safes no black-box fixture reached
// (promotion, unreadable mapfile, mid-loop cancel) — `HookSink` below
// uses log lines as the clock that lets a test change the world mid-loop. ──
struct HookSink {
    trigger: &'static str,
    action: Box<dyn Fn() + Send + Sync>,
    cancel_after_trigger: bool,
    fired: std::sync::atomic::AtomicBool,
    logs: std::sync::Mutex<Vec<(Level, String)>>,
}

impl HookSink {
    fn new(
        trigger: &'static str,
        cancel_after_trigger: bool,
        action: Box<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            trigger,
            action,
            cancel_after_trigger,
            fired: std::sync::atomic::AtomicBool::new(false),
            logs: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn cancelling(trigger: &'static str) -> Self {
        Self::new(trigger, true, Box::new(|| {}))
    }

    fn did_fire(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Did the loop log a line containing `needle` at `level`?
    fn logged(&self, level: Level, needle: &str) -> bool {
        self.logs
            .lock()
            .unwrap()
            .iter()
            .any(|(l, m)| *l == level && m.contains(needle))
    }
}

impl Sink for HookSink {
    fn log(&self, level: Level, msg: &str) {
        self.logs.lock().unwrap().push((level, msg.to_string()));
        if msg.contains(self.trigger) && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            (self.action)();
        }
    }
    fn should_cancel(&self) -> bool {
        self.cancel_after_trigger && self.did_fire()
    }
}

// A 4096-sector disc whose ONE title spans the whole image, so an
// MKV-scoped gate sees damage at LBA 1000 and runs patch passes.
// Returns (scratch dir, ISO path, mapfile path, disc).
fn in_title_damage_fixture(
    tag: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    libfreemkv::Disc,
) {
    let (dir, iso) = scratch_iso(tag);
    let sectors = 4096u32;
    let disc = test_disc(sectors, vec![test_title(0, sectors)]);
    let mapfile = disc.mapfile_for(&iso);
    (dir, iso, mapfile, disc)
}

/// The reader half of [`in_title_damage_fixture`]: LBA 1000 never heals.
fn never_healing_reader() -> MultiSpotReader {
    MultiSpotReader {
        capacity: 4096,
        spots: vec![Spot {
            lba: 1000,
            heal_after: u32::MAX,
            attempts: 0,
        }],
    }
}

/// A raw (multipass-legal) job writing to `iso`.
fn raw_job(iso: &std::path::Path) -> Job {
    let mut job = Job::new("disc:///dev/null", iso.to_string_lossy());
    job.raw = true;
    job
}

// A generous tolerance: the real residual loss on the fixture above is
// ~35s of a 7200s title, so an HOUR accepts it — every abort asserted
// below comes from the fail-safe under test and nothing else.
const GENEROUS_TOLERANCE_SECS: u64 = 3600;

// CONTROL for the two sabotage tests below: same disc/damage/tolerance, mapfile untouched.
// Must NOT abort, or the sabotage tests could pass for the wrong reason.
#[test]
fn multipass_rip_accepts_a_measurable_loss_under_a_generous_tolerance() {
    let (_dir, iso, _mapfile, disc) = in_title_damage_fixture("gate-control");
    let mut reader = never_healing_reader();
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
        is_iso_output: false,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("a permanently-bad sector is a reported result, not an Err");

    assert!(!result.halted);
    assert!(
        result.unreadable_bytes > 0,
        "the fixture must actually end with confirmed loss"
    );
    assert!(
        result.main_lost_ms.is_finite() && result.main_lost_ms > 0.0,
        "the loss must be quantifiable when the mapfile is intact, got {}",
        result.main_lost_ms
    );
    assert!(
        !result.aborted_for_loss,
        "{} ms of loss is well inside a {GENEROUS_TOLERANCE_SECS}s tolerance",
        result.main_lost_ms
    );
}

// On an ISO rip, damage entirely OUTSIDE the main title must not be reported as main-title
// playback loss (the whole-disc `abort_lost_bytes` count once got scaled by the main
// title's size/duration).
#[test]
fn iso_damage_outside_the_main_title_is_not_reported_as_main_title_loss() {
    let (_dir, iso) = scratch_iso("iso-off-title-loss");
    let sectors = 4096u32;
    // The title occupies sectors 0..100 ONLY. The never-healing spot is at
    // LBA 1000, comfortably outside it.
    let disc = test_disc(sectors, vec![test_title(0, 100)]);
    let mut reader = never_healing_reader();
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
        is_iso_output: true,
    };

    let result = multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("permanent off-title damage is a reported result, not an Err");

    assert!(
        result.unreadable_bytes > 0,
        "the fixture must actually end with confirmed loss"
    );
    assert_eq!(
        libfreemkv::disc::bytes_bad_in_title(&test_title(0, 100), &[(1000 * 2048, 2048)]),
        0,
        "sanity: the damaged LBA really is outside the title's extents"
    );
    assert_eq!(
        result.main_lost_ms, 0.0,
        "the main title was read perfectly; its playback loss is zero"
    );
    assert_eq!(
        result.severity,
        crate::DamageSeverity::Cosmetic,
        "a handful of off-title bad sectors is Cosmetic, not Serious"
    );
    assert!(
        result.aborted_for_loss,
        "an ISO deliverable still refuses ANY unreadable byte — the honest \
             millisecond figure must not weaken the whole-disc gate"
    );
    assert!(!result.complete, "a rip the gate refused is never complete");
}

// The LIVE end-of-recovery gate must abort when the mapfile cannot be read at the
// abort-decision point: sabotages it into a directory right as the patch loop breaks and
// expects the NaN fail-safe.
#[test]
fn multipass_rip_aborts_when_the_mapfile_cannot_be_read_at_the_gate() {
    let (_dir, iso, mapfile, disc) = in_title_damage_fixture("gate-unreadable-mapfile");
    let mut reader = never_healing_reader();
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
        is_iso_output: false,
    };

    // Sabotage: the mapfile becomes a DIRECTORY, so `read_to_string`
    // fails with EISDIR no matter which user runs the suite.
    let victim = mapfile.clone();
    let sink = HookSink::new(
        "exhausted",
        false,
        Box::new(move || {
            let _ = std::fs::remove_file(&victim);
            std::fs::create_dir_all(&victim).expect("sabotage: mapfile -> directory");
        }),
    );

    let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
        .expect("an unreadable mapfile is a fail-safe verdict, not an Err");

    assert!(
        sink.did_fire(),
        "the sabotage never ran — test proves nothing"
    );
    assert!(mapfile.is_dir(), "the mapfile must still be unreadable");
    assert!(
        sink.logged(
            Level::Error,
            "mapfile could not be loaded to verify loss — forcing abort"
        ),
        "the gate must say it is failing safe: {:?}",
        sink.logs.lock().unwrap()
    );
    let cause = Mapfile::load(&mapfile)
        .expect_err("still unreadable")
        .to_string();
    assert!(
        sink.logged(Level::Error, &cause),
        "the fail-safe log must carry the load error ({cause}): {:?}",
        sink.logs.lock().unwrap()
    );
    assert!(
        result.main_lost_ms.is_nan(),
        "an unreadable damage record is unquantifiable loss, got {}",
        result.main_lost_ms
    );
    assert!(
        result.aborted_for_loss,
        "the abort must fire even under a {GENEROUS_TOLERANCE_SECS}s tolerance"
    );
    assert!(!result.complete, "a rip the gate refused is never complete");
    assert_eq!(
        result.severity,
        crate::DamageSeverity::Serious,
        "an unquantifiable loss is Serious, not a lower tier"
    );
    assert!(!result.halted, "this is the gate firing, not a cancel");
}

// A failed end-of-recovery PROMOTION must abort the rip: `Mapfile::load` SUCCEEDS here
// (unlike the test above) but `<mapfile>.tmp` is sabotaged into a directory so `flush()`
// fails.
#[test]
fn multipass_rip_aborts_when_the_end_of_recovery_promotion_cannot_be_persisted() {
    let (_dir, iso, mapfile, disc) = in_title_damage_fixture("gate-promotion-failure");
    let mut reader = never_healing_reader();
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: GENEROUS_TOLERANCE_SECS,
        is_iso_output: false,
    };

    let tmp_path = {
        let mut s = mapfile.clone().into_os_string();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    let victim = tmp_path.clone();
    let sink = HookSink::new(
        "exhausted",
        false,
        Box::new(move || {
            let _ = std::fs::remove_file(&victim);
            std::fs::create_dir_all(&victim).expect("sabotage: mapfile.tmp -> directory");
        }),
    );

    let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
        .expect("a failed promotion is a fail-safe verdict, not an Err");

    assert!(
        sink.did_fire(),
        "the sabotage never ran — test proves nothing"
    );
    assert!(tmp_path.is_dir(), "the mapfile must still be unwritable");
    assert!(
        !sink.logged(Level::Error, "mapfile could not be loaded"),
        "the mapfile must LOAD fine — this is the promotion branch, not \
             the unreadable-mapfile branch: {:?}",
        sink.logs.lock().unwrap()
    );
    assert!(
        sink.logged(Level::Warn, "failed to flush promoted mapfile")
            || sink.logged(Level::Warn, "end-of-recovery promotion failed"),
        "a failed promotion must be reported: {:?}",
        sink.logs.lock().unwrap()
    );
    assert!(
        sink.logged(Level::Error, "damage record is incomplete"),
        "the gate must say WHY the loss is unquantifiable: {:?}",
        sink.logs.lock().unwrap()
    );
    assert!(
        result.main_lost_ms.is_nan(),
        "an incomplete damage record is unquantifiable loss, got {}",
        result.main_lost_ms
    );
    assert!(
        result.aborted_for_loss,
        "the abort must fire even under a {GENEROUS_TOLERANCE_SECS}s tolerance"
    );
    assert!(!result.complete);
    assert_eq!(result.severity, crate::DamageSeverity::Serious);
}

// A rip cancelled mid-loop, AFTER damage has been found, is halted and never Clean —
// severity there was once hard-coded `Clean`. Cancel is armed from the "pass N recovered"
// log line.
#[test]
fn multipass_rip_cancelled_mid_loop_is_halted_and_never_reported_clean() {
    let (_dir, iso) = scratch_iso("mid-loop-cancel");
    let sectors = 8_192u32;
    let disc = test_disc(sectors, vec![]);
    // One spot that heals on the next touch (so patch pass 1 makes real
    // progress and the loop-bottom NoProgress gate does NOT break for us)
    // and one that never heals (so the scope never converges either).
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: vec![
            Spot {
                lba: 1_000,
                heal_after: 1,
                attempts: 0,
            },
            Spot {
                lba: 6_000,
                heal_after: u32::MAX,
                attempts: 0,
            },
        ],
    };
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let sink = HookSink::cancelling("multipass_rip: pass ");
    let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &sink)
        .expect("a cancelled rip is a partial result, not an Err");

    assert!(
        sink.did_fire(),
        "the cancel was never armed — test proves nothing"
    );
    assert!(result.halted, "a cancelled rip must report halted");
    assert_eq!(
        result.passes, 2,
        "sweep + the one patch pass that ran before the cancel: the loop \
             must stop at its own top-of-loop cancel check"
    );
    assert!(
        result.unreadable_bytes + result.pending_bytes > 0,
        "the fixture must have found damage BEFORE the cancel, or the \
             severity assertion below is vacuous"
    );
    assert_ne!(
        result.severity,
        crate::DamageSeverity::Clean,
        "a cancelled rip holding {} unreadable + {} pending bytes is not \
             Clean — that badge contradicted the counters next to it",
        result.unreadable_bytes,
        result.pending_bytes
    );
    // Expects the literal tier, not a re-run of the old formula
    // (`classify_damage(bad_sector_count(unreadable, pending), 0.0)`),
    // which `interrupted_severity` deliberately avoids — see the wide-pending test.
    assert_eq!(
        result.unreadable_bytes, 0,
        "fixture: nothing was CONFIRMED lost, so the tier below is the \
             not-Clean floor an interrupted run gets for outstanding work"
    );
    assert_eq!(
        result.severity,
        crate::DamageSeverity::Cosmetic,
        "a cancel holding {} pending bytes and nothing unreadable is \
             Cosmetic: not Clean, and not a damage claim nobody measured",
        result.pending_bytes
    );
    assert!(!result.complete, "an interrupted rip is never complete");
    assert!(
        !result.aborted_for_loss,
        "the abort gate is not reached on the halted path"
    );
    assert!(
        result.main_lost_ms.is_nan(),
        "a cancel measured no loss beside {} pending bytes; 0.0 claims none was lost",
        result.pending_bytes
    );
}

// The halted exit must score damage it MEASURED, not work not got to — a wide unrecovered
// region where folding pending in would wrongly stamp Serious on confirmed-zero loss.
#[test]
fn a_cancel_with_a_wide_pending_region_is_not_scored_from_it() {
    /// Cancels on the FIRST progress tick, so the sweep stops with the bulk
    /// of the disc never attempted — the "cancelled ten seconds into a
    /// 66 GB disc" case, at fixture scale.
    struct CancelAtOnce;
    impl Sink for CancelAtOnce {
        fn should_cancel(&self) -> bool {
            true
        }
    }

    let (_dir, iso) = scratch_iso("wide-pending-cancel");
    let sectors = 8192u32;
    let disc = test_disc(sectors, vec![test_title(0, sectors)]);
    let mut reader = MultiSpotReader {
        capacity: sectors,
        spots: Vec::new(), // a PERFECT disc: nothing is unreadable
    };
    let job = raw_job(&iso);
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };

    let result = multipass_rip(&disc, &mut reader, &iso, &job, &opts, &CancelAtOnce)
        .expect("a cancelled rip is a partial result, not an Err");

    assert!(result.halted);
    assert_eq!(
        result.unreadable_bytes, 0,
        "nothing is promoted to Unreadable on the halted path, so nothing \
             is CONFIRMED lost"
    );
    assert!(
        result.pending_bytes / 2048 >= 500,
        "fixture: the pending region must clear the Serious threshold or \
             the two formulas agree again — got {} sectors",
        result.pending_bytes / 2048
    );
    assert_eq!(
        result.severity,
        crate::DamageSeverity::Cosmetic,
        "an interrupted run scores only what it measured: {} unreadable \
             bytes beside {} pending",
        result.unreadable_bytes,
        result.pending_bytes
    );
}

#[test]
fn an_interrupted_loss_is_zero_only_when_nothing_is_outstanding() {
    assert_eq!(interrupted_lost_ms(0, 0), 0.0);
    assert!(interrupted_lost_ms(2048, 0).is_nan());
    assert!(interrupted_lost_ms(0, 2048).is_nan());
}

#[test]
fn recovery_is_complete_requires_all_three() {
    assert!(recovery_is_complete(false, 0, 0));
    assert!(
        !recovery_is_complete(true, 0, 0),
        "a rip the abort gate refused is never complete, however clean the counters look"
    );
    assert!(
        !recovery_is_complete(false, 1, 0),
        "unreadable bytes remain"
    );
    assert!(!recovery_is_complete(false, 0, 1), "pending bytes remain");
    assert!(!recovery_is_complete(true, 1, 1));
}

// ── Re-running multipass on the same image resumes from its mapfile ──

/// Stamps every sector with `marker` (byte 4) and its LBA (bytes 0..4), records each LBA it
/// is asked for, fails `bad` while `heal_after` touches remain, and cancels `halt_at`'s token
/// once the read head reaches its LBA.
struct StampReader {
    capacity: u32,
    marker: u8,
    bad: Option<Spot>,
    halt_at: Option<(u32, libfreemkv::Halt)>,
    reads: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
}
impl libfreemkv::SectorSource for StampReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        _recovery: bool,
    ) -> libfreemkv::Result<usize> {
        let end = lba + count as u32;
        self.reads.lock().unwrap().extend(lba..end);
        if let Some((at, halt)) = &self.halt_at
            && end > *at
        {
            halt.cancel();
        }
        if let Some(spot) = &mut self.bad
            && lba <= spot.lba
            && spot.lba < end
        {
            spot.attempts += 1;
            if spot.attempts <= spot.heal_after {
                return Err(libfreemkv::Error::DiscRead {
                    sector: spot.lba as u64,
                    status: Some(2),
                    sense: Some(libfreemkv::scsi::ScsiSense {
                        sense_key: libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR,
                        asc: 0x17,
                        ascq: 0x01,
                    }),
                });
            }
        }
        let n = ((count as usize) * 2048).min(buf.len());
        for (i, chunk) in buf[..n].chunks_mut(2048).enumerate() {
            chunk.fill(self.marker);
            chunk[..4].copy_from_slice(&(lba + i as u32).to_le_bytes());
        }
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }
}

fn stamp_reader(marker: u8) -> (StampReader, std::sync::Arc<std::sync::Mutex<Vec<u32>>>) {
    let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let r = StampReader {
        capacity: 4096,
        marker,
        bad: None,
        halt_at: None,
        reads: reads.clone(),
    };
    (r, reads)
}

// The sectors a re-run must neither re-read nor overwrite: every Finished range run 1 left.
fn finished_lbas(mapfile: &std::path::Path) -> Vec<u32> {
    let map = Mapfile::load(mapfile).expect("run 1 left a mapfile");
    map.ranges_with(&[SectorStatus::Finished])
        .iter()
        .flat_map(|&(pos, size)| (pos / 2048) as u32..((pos + size) / 2048) as u32)
        .collect()
}

fn marker_at(iso: &std::path::Path, lba: u32) -> u8 {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(iso).unwrap();
    f.seek(SeekFrom::Start(lba as u64 * 2048 + 4)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0]
}

// Run 2 must touch nothing run 1 already recovered, and must recover the rest.
fn assert_resumed(
    finished: &[u32],
    reads: &std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    iso: &std::path::Path,
    r: &MultipassResult,
) {
    assert!(
        finished.len() > 100,
        "fixture check: run 1 must have recovered something, got {} sectors",
        finished.len()
    );
    let reads = reads.lock().unwrap();
    let reread: Vec<u32> = finished
        .iter()
        .copied()
        .filter(|l| reads.contains(l))
        .take(5)
        .collect();
    assert!(
        reread.is_empty(),
        "run 2 re-read sectors run 1 had already recovered, e.g. {reread:?}"
    );
    for &lba in [
        finished[0],
        finished[finished.len() / 2],
        finished[finished.len() - 1],
    ]
    .iter()
    {
        assert_eq!(
            marker_at(iso, lba),
            0xA1,
            "LBA {lba} recovered by run 1 was wiped or overwritten by run 2"
        );
    }
    assert!(r.complete, "run 2 must finish the recovery: {r:?}");
    assert_eq!(r.good_bytes, 4096 * 2048, "{r:?}");
}

#[test]
fn a_rerun_after_an_abort_for_loss_resumes_instead_of_wiping_the_image() {
    let (_dir, iso) = scratch_iso("rerun-after-abort");
    let disc = test_disc(4096, vec![test_title(0, 4096)]);
    let opts = MultipassOpts {
        max_passes: 3,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let (mut r1, _) = stamp_reader(0xA1);
    r1.bad = Some(Spot {
        lba: 1000,
        heal_after: u32::MAX,
        attempts: 0,
    });
    let first = multipass_rip(
        &disc,
        &mut r1,
        &iso,
        &raw_job(&iso),
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("run 1");
    assert!(
        first.aborted_for_loss,
        "fixture check: run 1 must abort: {first:?}"
    );
    let finished = finished_lbas(&disc.mapfile_for(&iso));

    // The drive now reads the spot (cleaned disc, better drive): a re-run must retry it.
    let (mut r2, reads) = stamp_reader(0xB2);
    let second = multipass_rip(
        &disc,
        &mut r2,
        &iso,
        &raw_job(&iso),
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("run 2");
    assert_resumed(&finished, &reads, &iso, &second);
}

#[test]
fn a_rerun_after_a_stop_mid_sweep_resumes_instead_of_wiping_the_image() {
    let (_dir, iso) = scratch_iso("rerun-after-stop");
    let disc = test_disc(4096, vec![test_title(0, 4096)]);
    let opts = MultipassOpts {
        max_passes: 3,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let op = libfreemkv::Halt::new();
    let (mut r1, _) = stamp_reader(0xA1);
    r1.halt_at = Some((2000, op.clone()));
    let first = multipass_rip_with(
        &op,
        &disc,
        &mut r1,
        &iso,
        &raw_job(&iso),
        &opts,
        &crate::sink::NoopSink,
    );
    let first = first.value().expect("run 1's partial result");
    assert!(
        first.halted,
        "fixture check: run 1 must stop mid-sweep: {first:?}"
    );
    let finished = finished_lbas(&disc.mapfile_for(&iso));

    let (mut r2, reads) = stamp_reader(0xB2);
    let second = multipass_rip(
        &disc,
        &mut r2,
        &iso,
        &raw_job(&iso),
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("run 2");
    assert_resumed(&finished, &reads, &iso, &second);
}

// Run 1 stops mid-sweep in `run1_raw` mode; run 2 in the other mode must re-read every
// sector run 1 wrote, never splice raw and decrypted sectors into one image.
fn assert_a_mode_switch_sweeps_fresh(run1_raw: bool) {
    let (_dir, iso) = scratch_iso(if run1_raw {
        "raw-then-dec"
    } else {
        "dec-then-raw"
    });
    let disc = test_disc(4096, vec![test_title(0, 4096)]);
    let job = |raw| Job {
        raw,
        ..raw_job(&iso)
    };
    let opts = |raw| MultipassOpts {
        max_passes: if raw { 3 } else { 0 },
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let op = libfreemkv::Halt::new();
    let (mut r1, _) = stamp_reader(0xA1);
    r1.halt_at = Some((2000, op.clone()));
    let (d, j1, o1) = (&disc, job(run1_raw), opts(run1_raw));
    let first = multipass_rip_with(&op, d, &mut r1, &iso, &j1, &o1, &crate::sink::NoopSink);
    assert!(
        first.value().is_some_and(|r| r.halted),
        "fixture check: run 1 must stop mid-sweep"
    );
    let finished = finished_lbas(&disc.mapfile_for(&iso));
    assert!(
        finished.len() > 100,
        "fixture check: run 1 recovered nothing"
    );

    let (mut r2, reads) = stamp_reader(0xB2);
    let (j2, o2) = (job(!run1_raw), opts(!run1_raw));
    let second = multipass_rip(d, &mut r2, &iso, &j2, &o2, &crate::sink::NoopSink).expect("run 2");
    let reads = reads.lock().unwrap();
    let kept: Vec<u32> = finished
        .iter()
        .copied()
        .filter(|l| !reads.contains(l))
        .take(5)
        .collect();
    assert!(
        kept.is_empty(),
        "run 2 kept run 1's other-mode sectors, e.g. {kept:?}"
    );
    assert_eq!(marker_at(&iso, finished[0]), 0xB2);
    assert!(second.complete, "{second:?}");
}

#[test]
fn a_multipass_rerun_never_resumes_a_decrypted_partial() {
    assert_a_mode_switch_sweeps_fresh(false);
}

#[test]
fn a_decrypting_rerun_never_resumes_a_raw_multipass_partial() {
    assert_a_mode_switch_sweeps_fresh(true);
}

// The mode stamp round-trips, and a malformed one is refused rather than read as unknown.
#[test]
fn the_raw_mode_stamp_round_trips_and_a_bad_one_is_refused() {
    let (_dir, iso) = scratch_iso("raw-stamp");
    let path = crate::mapfile_path_for(&iso);
    let mut map = Mapfile::create(&path, 4096, "t").unwrap();
    assert_eq!(map.raw(), None);
    map.set_raw(true);
    map.flush().unwrap();
    assert_eq!(Mapfile::load(&path).unwrap().raw(), Some(true));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("freemkv-raw: 1", "freemkv-raw: yes")).unwrap();
    assert!(Mapfile::load(&path).is_err());
}

fn whole_disc() -> libfreemkv::Disc {
    test_disc(4096, vec![test_title(0, 4096)])
}

fn single_pass(raw: bool) -> (Job, MultipassOpts) {
    let job = Job {
        raw,
        ..Job::new("disc:///dev/null", "out.iso")
    };
    let opts = MultipassOpts {
        max_passes: 0,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    (job, opts)
}

fn iso_multipass() -> MultipassOpts {
    MultipassOpts {
        max_passes: 3,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    }
}

// copy's dispatch guard alone: a FINISHED raw image re-requested as a decrypted copy must be
// read again, not reported done, and the user is told the image is overwritten.
#[test]
fn a_finished_raw_image_rerun_as_a_decrypted_copy_is_read_again() {
    let (_dir, iso) = scratch_iso("finished-raw-then-dec");
    let disc = whole_disc();
    let (mut r1, _) = stamp_reader(0xA1);
    let first = multipass_rip(
        &disc,
        &mut r1,
        &iso,
        &raw_job(&iso),
        &iso_multipass(),
        &crate::sink::NoopSink,
    )
    .expect("run 1");
    assert!(first.complete, "fixture check: {first:?}");

    let (mut r2, reads) = stamp_reader(0xB2);
    let (job, opts) = single_pass(false);
    let sink = HookSink::new("", false, Box::new(|| {}));
    let second = multipass_rip(&disc, &mut r2, &iso, &job, &opts, &sink).expect("run 2");
    assert!(second.complete, "{second:?}");
    assert_eq!(
        reads.lock().unwrap().len(),
        4096,
        "every sector must be read again"
    );
    assert_eq!(marker_at(&iso, 4095), 0xB2);
    assert!(sink.logged(Level::Warn, "other raw/decrypt mode"));
}

// Pass 1's "only a map proven raw" rule alone: a decrypted partial whose map predates the
// mode stamp must not be resumed by a raw multipass run.
#[test]
fn an_unstamped_decrypted_partial_is_not_resumed_by_multipass() {
    let (_dir, iso) = scratch_iso("unstamped-dec-then-raw");
    let disc = whole_disc();
    let op = libfreemkv::Halt::new();
    let (mut r1, _) = stamp_reader(0xA1);
    r1.halt_at = Some((2000, op.clone()));
    let (job, opts) = single_pass(false);
    let first = multipass_rip_with(
        &op,
        &disc,
        &mut r1,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    );
    assert!(first.value().is_some_and(|r| r.halted), "fixture check");
    let map = disc.mapfile_for(&iso);
    let text = std::fs::read_to_string(&map).unwrap();
    assert!(text.contains("# freemkv-raw: 0\n"), "fixture check: {text}");
    std::fs::write(&map, text.replace("# freemkv-raw: 0\n", "")).unwrap();
    let finished = finished_lbas(&map);
    assert!(finished.len() > 100, "fixture check");

    let (mut r2, reads) = stamp_reader(0xB2);
    let second = multipass_rip(
        &disc,
        &mut r2,
        &iso,
        &raw_job(&iso),
        &iso_multipass(),
        &crate::sink::NoopSink,
    )
    .expect("run 2");
    let reads = reads.lock().unwrap();
    assert!(
        finished.iter().all(|l| reads.contains(l)),
        "run 2 resumed an unproven map"
    );
    assert!(second.complete, "{second:?}");
}

// The sweep's own guard alone: a direct resuming sweep in decrypt mode over a raw partial
// must start fresh.
#[test]
fn a_resuming_decrypted_sweep_never_continues_a_raw_partial() {
    let (_dir, iso) = scratch_iso("raw-then-dec-sweep");
    let disc = whole_disc();
    let op = libfreemkv::Halt::new();
    let (mut r1, _) = stamp_reader(0xA1);
    r1.halt_at = Some((2000, op.clone()));
    let first = multipass_rip_with(
        &op,
        &disc,
        &mut r1,
        &iso,
        &raw_job(&iso),
        &iso_multipass(),
        &crate::sink::NoopSink,
    );
    assert!(first.value().is_some_and(|r| r.halted), "fixture check");
    let finished = finished_lbas(&disc.mapfile_for(&iso));
    assert!(finished.len() > 100, "fixture check");

    let (mut r2, reads) = stamp_reader(0xB2);
    let opts = SweepOptions {
        decrypt: true,
        resume: true,
        batch_sectors: None,
        skip_on_error: true,
        progress: None,
        halt: None,
        keys: None,
    };
    crate::recovery::sweep(&disc, &mut r2, &iso, &opts).expect("run 2");
    let reads = reads.lock().unwrap();
    assert!(
        finished.iter().all(|l| reads.contains(l)),
        "run 2 resumed the raw partial"
    );
    assert_eq!(marker_at(&iso, finished[0]), 0xB2);
}

// "No image" is missing or an empty regular file. A device reports length 0 whatever it
// holds, so another disc's map beside one is still refused, never dropped.
#[test]
fn only_a_missing_or_empty_regular_file_counts_as_no_image() {
    let (dir, iso) = scratch_iso("no-image");
    assert!(crate::recovery::no_image(&iso).unwrap());
    std::fs::write(&iso, b"").unwrap();
    assert!(crate::recovery::no_image(&iso).unwrap());
    std::fs::write(&iso, b"x").unwrap();
    assert!(!crate::recovery::no_image(&iso).unwrap());
    assert!(!crate::recovery::no_image(dir.path()).unwrap());
    #[cfg(unix)]
    assert!(!crate::recovery::no_image(std::path::Path::new("/dev/null")).unwrap());
}

fn disc_with_hash(c: char) -> libfreemkv::Disc {
    let mut aacs = libfreemkv::test_util::aacs_state().build();
    aacs.disc_hash = c.to_string().repeat(40);
    libfreemkv::Disc {
        aacs: Some(aacs),
        ..test_disc(4096, vec![test_title(0, 4096)])
    }
}

// A consumer that deletes the ISO after muxing leaves `<iso>.mapfile` behind. Another
// disc's map over NO image guards nothing: the next rip sweeps fresh. With the image
// still there, the refusal stands.
#[test]
fn another_discs_mapfile_is_refused_only_while_its_image_exists() {
    let (_dir, iso) = scratch_iso("stale-map-other-disc");
    let opts = MultipassOpts {
        max_passes: 3,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let rip = |disc: &libfreemkv::Disc| {
        let (mut r, _) = stamp_reader(0xA1);
        multipass_rip(
            disc,
            &mut r,
            &iso,
            &raw_job(&iso),
            &opts,
            &crate::sink::NoopSink,
        )
    };
    let (disc_a, disc_b) = (disc_with_hash('a'), disc_with_hash('b'));
    assert!(rip(&disc_a).expect("disc A").complete);

    let refused = rip(&disc_b).expect_err("disc A's image is still there");
    assert!(
        matches!(
            refused,
            libfreemkv::Error::MapfileInvalid {
                kind: "disc-mismatch"
            }
        ),
        "{refused:?}"
    );

    let (single, single_opts) = single_pass(true);
    let (mut r, _) = stamp_reader(0xA1);
    let copy = |r: &mut StampReader| {
        multipass_rip(
            &disc_b,
            r,
            &iso,
            &single,
            &single_opts,
            &crate::sink::NoopSink,
        )
    };
    assert!(copy(&mut r).is_err(), "a single-pass copy is refused too");

    std::fs::remove_file(&iso).unwrap();
    let fresh = copy(&mut r).expect("a stale map with no image must not block a copy of disc B");
    assert!(fresh.complete, "{fresh:?}");
    std::fs::remove_file(&iso).unwrap();
    let fresh = rip(&disc_b).expect("a stale map with no image must not block disc B");
    assert!(fresh.complete, "{fresh:?}");
    let map = Mapfile::load(&disc_b.mapfile_for(&iso)).unwrap();
    assert_eq!(map.disc_hash(), Some("b".repeat(40).as_str()));
}

// Loss is measured over the titles being ripped (`Job::selection`), not `disc.titles[0]`.
#[test]
fn loss_is_measured_over_the_selected_titles_not_the_first_one() {
    let rip = |bad_lba: u32, tag: &str| {
        let (_dir, iso) = scratch_iso(tag);
        // Title 0 is a short intro at [0, 100); the chosen title 1 spans [2000, 4096).
        let disc = test_disc(4096, vec![test_title(0, 100), test_title(2000, 2096)]);
        let mut reader = MultiSpotReader {
            capacity: 4096,
            spots: vec![Spot {
                lba: bad_lba,
                heal_after: u32::MAX,
                attempts: 0,
            }],
        };
        let job = raw_job(&iso).with_selection(crate::Selection::Titles(vec![1]));
        let opts = MultipassOpts {
            max_passes: 5,
            abort_on_lost_secs: 0,
            is_iso_output: false,
        };
        multipass_rip(
            &disc,
            &mut reader,
            &iso,
            &job,
            &opts,
            &crate::sink::NoopSink,
        )
        .expect("permanent loss is a reported result")
    };

    let in_chosen = rip(3000, "selected-title-damaged");
    assert!(
        in_chosen.passes > 1,
        "damage inside the chosen title must earn patch passes: {in_chosen:?}"
    );
    assert!(
        in_chosen.aborted_for_loss,
        "loss inside the chosen title must abort a perfect-rip job: {in_chosen:?}"
    );
    assert!(
        in_chosen.main_lost_ms > 0.0,
        "the chosen title's lost playback must be reported: {in_chosen:?}"
    );

    let in_intro = rip(50, "unselected-title-damaged");
    assert_eq!(
        in_intro.passes, 1,
        "damage only in a title nobody is ripping earns no patch pass: {in_intro:?}"
    );
    assert!(
        !in_intro.aborted_for_loss,
        "damage only in an unselected title must not abort: {in_intro:?}"
    );
    assert_eq!(in_intro.main_lost_ms, 0.0, "{in_intro:?}");
}

#[test]
fn the_title_set_folds_count_every_ripped_title() {
    let a = test_title(0, 100);
    let b = test_title(1000, 100);
    let bad = [(10 * 2048, 2048), (1010 * 2048, 4096), (5000 * 2048, 2048)];
    assert_eq!(titles_scope_bad(false, &bad, &[&a, &b]), Some(6144));
    assert_eq!(titles_abort_lost_bytes(false, &[&a, &b], &bad), 6144);
    // Whole-disc scope is one count, not one per title.
    assert_eq!(titles_scope_bad(true, &bad, &[&a, &b]), Some(8192));
    assert_eq!(titles_abort_lost_bytes(true, &[&a, &b], &bad), 8192);
    // One extent-less title makes the set unmeasured, whatever the others say.
    let empty = libfreemkv::DiscTitle::empty();
    assert_eq!(titles_scope_bad(false, &bad, &[&a, &empty]), None);
    let (ms, why) = titles_lost_ms(true, &[&a, &empty], &bad);
    assert!(ms.is_nan() && why.is_some());
    let (ms, why) = titles_lost_ms(true, &[&a, &b], &bad);
    let one = |t, n| end_of_recovery_lost_ms(true, t, &bad[n..=n]).0;
    assert!(why.is_none());
    assert!((ms - (one(&a, 0) + one(&b, 1))).abs() < 1e-6, "{ms}");
}

#[test]
fn measured_titles_follow_the_selection_and_never_come_back_empty() {
    let empty = libfreemkv::DiscTitle::empty();
    let disc = test_disc(4096, vec![test_title(0, 10), test_title(100, 10)]);
    let job = |sel| raw_job(std::path::Path::new("x.iso")).with_selection(sel);
    let got = measured_titles(&disc, &job(crate::Selection::Titles(vec![1])), &empty);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].extents[0].start_lba, 100);
    let main = measured_titles(&disc, &job(crate::Selection::MainMovie), &empty);
    assert_eq!(
        main[0].extents[0].start_lba, 0,
        "MainMovie is title 0, as before"
    );
    let none = measured_titles(&disc, &job(crate::Selection::Titles(vec![9])), &empty);
    assert_eq!(none.len(), 1);
    assert!(
        none[0].extents.is_empty(),
        "no title -> unmeasurable, never clean"
    );
}

// Decryption is orthogonal to the read policy (EO6): a decrypting multipass rip reads
// and writes its image like a raw one.
#[test]
fn a_decrypting_multipass_job_runs() {
    let (_dir, iso) = scratch_iso("multipass-decrypting");
    let disc = test_disc(256, vec![]);
    let (mut reader, reads) = stamp_reader(0xA1);
    let job = Job::new("disc:///dev/null", iso.to_string_lossy());
    assert!(!job.raw, "fixture check: the job must be decrypting");
    let opts = MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    multipass_rip(
        &disc,
        &mut reader,
        &iso,
        &job,
        &opts,
        &crate::sink::NoopSink,
    )
    .expect("a decrypting multipass rip is allowed");
    assert!(!reads.lock().unwrap().is_empty(), "the rip read the disc");
    assert!(iso.exists(), "the rip wrote its image");
    // The passes ran decrypting: the map says so (an AACS image's plaintext is pinned by
    // the KU gate tests).
    let map = Mapfile::load(&disc.mapfile_for(&iso)).unwrap();
    assert_eq!(map.raw(), Some(false), "a decrypting run's image");
}
