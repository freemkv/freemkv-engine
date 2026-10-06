use super::*;

// `sample_at` must report real throughput, not a constant — kills the six
// "replace sample -> (0, None)" mutants (each asserts a value no constant
// return satisfies). These tests pin mutation gaps found across the module.
#[test]
fn sample_reports_the_actual_rate_and_a_finite_eta() {
    let t0 = Instant::now();
    let mut est = SpeedEstimator::new();

    // 100 MiB in the first second, then another 100 MiB in the next.
    let mib = 1024 * 1024u64;
    est.sample_at(t0, 0, 1000 * mib);
    let (speed_bps, eta) = est.sample_at(t0 + Duration::from_secs(1), 100 * mib, 1000 * mib);

    assert!(
        speed_bps > 0,
        "a moving rip must report a non-zero speed; got {speed_bps}"
    );
    // ~100 MiB/s. Generous bounds: the point is that it tracks the input,
    // not that it hits an exact figure.
    assert!(
        (50 * mib..=200 * mib).contains(&speed_bps),
        "speed should track the observed ~100 MiB/s, got {} MiB/s",
        speed_bps / mib
    );
    // 900 MiB left at ~100 MiB/s: an ETA in minutes, and crucially SOME.
    let eta = eta.expect("forward progress must produce an ETA");
    assert!(
        (1..=120).contains(&eta),
        "ETA should be roughly 9s-ish for 900MiB at 100MiB/s, got {eta}s"
    );
}

/// A stalled rip yields no ETA rather than an astronomical one.
#[test]
fn a_stalled_rip_reports_no_eta() {
    let t0 = Instant::now();
    let mut est = SpeedEstimator::new();
    est.sample_at(t0, 0, 1_000_000_000);
    // Time passes, nothing is read.
    let (_speed, eta) = est.sample_at(t0 + Duration::from_secs(30), 0, 1_000_000_000);
    assert_eq!(
        eta, None,
        "a dead stall must not divide toward a multi-year ETA"
    );
}

/// A finished rip reports no ETA: there is nothing left to wait for.
#[test]
fn a_finished_rip_reports_no_eta() {
    let t0 = Instant::now();
    let mut est = SpeedEstimator::new();
    est.sample_at(t0, 0, 1000);
    let (_speed, eta) = est.sample_at(t0 + Duration::from_secs(1), 1000, 1000);
    assert_eq!(
        eta, None,
        "bytes_total == bytes_done leaves nothing to estimate"
    );
}

// The display window's phase boundaries are exact. Both `<` -> `<=` mutants in
// `display_window_secs` are equivalent (the curve is continuous there); these pin the rest.
#[test]
fn the_display_window_boundaries_are_exact() {
    // Static phase: [0, 60) is a flat 10s window.
    assert_eq!(display_window_secs(0.0), STATIC_WINDOW_SECS);
    assert_eq!(display_window_secs(59.9), STATIC_WINDOW_SECS);

    // AT 60.0 the growth phase begins with a zero term, so either operator
    // yields STATIC_WINDOW_SECS. The `<` -> `<=` mutant here is EQUIVALENT,
    // not a coverage gap — recorded so nobody chases it again.
    assert_eq!(
        display_window_secs(STATIC_PHASE_SECS),
        STATIC_WINDOW_SECS,
        "the two phases agree at the boundary"
    );
    assert!(
        display_window_secs(STATIC_PHASE_SECS + 0.1) > STATIC_WINDOW_SECS,
        "just past the boundary the window must be growing"
    );

    // Growth is monotonic and lands exactly on MAX at the end — so the SECOND `<` -> `<=`
    // mutant is equivalent for the same reason as the first.
    let end = STATIC_PHASE_SECS + GROWTH_PHASE_SECS;
    assert!(display_window_secs(end - 0.1) < MAX_WINDOW_SECS);
    assert_eq!(display_window_secs(end), MAX_WINDOW_SECS);
    assert_eq!(display_window_secs(end + 10_000.0), MAX_WINDOW_SECS);
}

// ─── Display speed (observe) ────────────────────────────────────────────

#[test]
fn first_sample_returns_zero() {
    // A single sample can't yield a rate — and must not synthesize one from
    // already-copied bytes (the "2197.8 MB/s on a resumed BD rip" bug).
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let speed = s.observe(t0, 20 * 1024 * 1024 * 1024);
    assert_eq!(speed, 0.0, "first sample must not synthesize a speed");
    assert_eq!(s.samples.len(), 1, "first sample is recorded for the next");
}

#[test]
fn second_sample_matches_physical_rate() {
    // 70 MiB delta in 1 s → ~70 MB/s.
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let _ = s.observe(t0, 1_000_000_000);
    let speed = s.observe(t0 + Duration::from_secs(1), 1_000_000_000 + 70 * 1_048_576);
    assert!((speed - 70.0).abs() < 1.0, "expected ~70 MB/s, got {speed}");
}

#[test]
fn caps_absurd_instantaneous() {
    // An 80 GB jump in 1 s (mapfile replay on a resumed disc) must cap at
    // 1 GB/s, not blast a nonsense number to the display.
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let _ = s.observe(t0, 0);
    let speed = s.observe(t0 + Duration::from_secs(1), 80 * 1024 * 1024 * 1024);
    assert!(speed <= MAX_PLAUSIBLE_MBS, "speed {speed} MB/s not capped");
}

#[test]
fn steady_state_converges() {
    let mut s = SpeedEstimator::new();
    let mut t = Instant::now();
    let mut bytes: u64 = 1_000_000_000;
    let _ = s.observe(t, bytes);
    let mut last = 0.0;
    for _ in 0..20 {
        t += Duration::from_secs(1);
        bytes += 70 * 1_048_576;
        last = s.observe(t, bytes);
    }
    assert!((last - 70.0).abs() < 2.0, "expected ~70 MB/s, got {last}");
}

#[test]
fn stall_drops_out_of_display_within_window() {
    // 2026-05-08 scenario: a 12 s stall must age out of the display window
    // within STATIC_WINDOW_SECS of recovery, not stick like the old EWMA.
    let mut s = SpeedEstimator::new();
    let mut t = Instant::now();
    let mut bytes: u64 = 0;
    let _ = s.observe(t, bytes);
    for _ in 0..10 {
        t += Duration::from_secs(1);
        bytes += 70 * 1_048_576;
        let _ = s.observe(t, bytes);
    }
    let healthy = s.observe(t, bytes);
    assert!((healthy - 70.0).abs() < 2.0, "pre-stall ~70, got {healthy}");

    for _ in 0..12 {
        t += Duration::from_secs(1);
        bytes += 1_048_576;
        let _ = s.observe(t, bytes);
    }
    let during = s.observe(t, bytes);
    assert!(during < 20.0, "stall must dip display, got {during} MB/s");

    for _ in 0..(STATIC_WINDOW_SECS as i32 + 2) {
        t += Duration::from_secs(1);
        bytes += 70 * 1_048_576;
        let _ = s.observe(t, bytes);
    }
    let recovered = s.observe(t, bytes);
    assert!(
        (recovered - 70.0).abs() < 2.0,
        "display must return to ~70 once stall ages out, got {recovered}"
    );
}

#[test]
fn responsive_mode_holds_fixed_window() {
    // In responsive (patch) mode the window stays at 10 s even well past the
    // point the steady curve would have grown it.
    let mut s = SpeedEstimator::new();
    s.set_responsive(true);
    let mut t = Instant::now();
    let mut bytes: u64 = 0;
    let _ = s.observe(t, bytes);
    // 3 minutes in: steady mode would use a ~34 s window; responsive holds 10.
    for _ in 0..180 {
        t += Duration::from_secs(1);
        bytes += 70 * 1_048_576;
        let _ = s.observe(t, bytes);
    }
    // At most STATIC_WINDOW_SECS of samples retained (+1 for the boundary).
    assert!(
        s.samples.len() <= STATIC_WINDOW_SECS as usize + 2,
        "responsive window kept {} samples, expected ~10",
        s.samples.len()
    );
}

#[test]
fn counter_reset_restarts_cleanly() {
    // A new pass drops bytes_done to a lower value; must not spike or panic.
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let _ = s.observe(t0, 900 * 1_048_576);
    let _ = s.observe(t0 + Duration::from_secs(1), 1000 * 1_048_576);
    let speed = s.observe(t0 + Duration::from_secs(2), 0);
    assert_eq!(speed, 0.0, "reset re-anchors → no rate yet");
    assert_eq!(s.samples.len(), 1, "reset cleared the window");
}

#[test]
fn display_window_grows_with_elapsed_time() {
    assert_eq!(display_window_secs(0.0), 10.0);
    assert_eq!(display_window_secs(30.0), 10.0);
    assert_eq!(display_window_secs(59.9), 10.0);
    assert_eq!(display_window_secs(60.0), 10.0);
    assert!((display_window_secs(210.0) - 35.0).abs() < 0.1);
    assert!((display_window_secs(360.0) - 60.0).abs() < 0.1);
    assert_eq!(display_window_secs(3600.0), 60.0);
}

// ─── ETA rate (eta_speed_mbs) ───────────────────────────────────────────

#[test]
fn eta_speed_stays_stable_through_a_stall() {
    // Display can dip during a stall; the ETA rate must not.
    let mut s = SpeedEstimator::new();
    let mut t = Instant::now();
    let mut bytes: u64 = 0;
    let _ = s.observe(t, bytes);
    for _ in 0..300 {
        t += Duration::from_secs(1);
        bytes += 70 * 1_048_576;
        let _ = s.observe(t, bytes);
    }
    let disp = s.observe(t, bytes);
    let eta_before = s.eta_speed_mbs(t, disp);
    assert!((eta_before - 70.0).abs() < 2.0, "ETA ~70, got {eta_before}");

    for _ in 0..12 {
        t += Duration::from_secs(1);
        bytes += 1_048_576;
        let _ = s.observe(t, bytes);
    }
    let disp2 = s.observe(t, bytes);
    let eta_during = s.eta_speed_mbs(t, disp2);
    assert!(
        (eta_during - 70.0).abs() < 5.0,
        "ETA rate must stay near true average through a stall, got {eta_during}"
    );
}

#[test]
fn eta_falls_back_to_display_during_warmup() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let _ = s.observe(t0, 0);
    let display = s.observe(t0 + Duration::from_secs(2), 100 * 1_048_576);
    let eta = s.eta_speed_mbs(t0 + Duration::from_secs(2), display);
    assert_eq!(eta, display, "before warmup ETA rate mirrors display");
}

// ─── Unified convenience (sample_at) ────────────────────────────────────

#[test]
fn sample_at_first_call_has_no_speed_or_eta() {
    let mut s = SpeedEstimator::new();
    let t = Instant::now();
    assert_eq!(s.sample_at(t, 0, 1000), (0, None));
}

#[test]
fn sample_at_reports_speed_and_eta() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    s.sample_at(t0, 0, 200 * 1_048_576);
    // 100 MiB over 1 s → ~100 MB/s; but ETA rate is in warmup (<10 s) so it
    // falls back to display; 100 MiB left → ~1 s ETA.
    let (bps, eta) = s.sample_at(
        t0 + Duration::from_secs(1),
        100 * 1_048_576,
        200 * 1_048_576,
    );
    let mbs = bps as f64 / BYTES_PER_MIB;
    assert!((mbs - 100.0).abs() < 1.0, "expected ~100 MB/s, got {mbs}");
    assert_eq!(eta, Some(1));
}

#[test]
fn sample_at_no_eta_once_complete() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    s.sample_at(t0, 500, 1000);
    let (_, eta) = s.sample_at(t0 + Duration::from_secs(1), 1000, 1000);
    assert_eq!(eta, None, "no work left → no ETA");
}

#[test]
fn sample_at_zero_interval_does_not_panic() {
    let mut s = SpeedEstimator::new();
    let t = Instant::now();
    s.sample_at(t, 100, 1000);
    let (speed, _) = s.sample_at(t, 200, 1000);
    assert_eq!(speed, 0, "same instant → no rate");
}

// `sample` is the real production entry point; this test targets its real-clock wrapper
// specifically.
#[test]
fn sample_derives_from_the_real_clock_not_a_constant() {
    // Spin until the monotonic clock reports a later instant than `from`.
    fn wait_for_the_clock_to_tick(from: Instant) {
        while Instant::now() <= from {
            std::hint::spin_loop();
        }
    }

    let mut s = SpeedEstimator::new();
    let total = 100 * 1024 * 1024u64;

    // (0, None) is genuinely correct for the first call — one sample can't
    // make a rate, and nothing is done yet.
    assert_eq!(
        s.sample(0, total),
        (0, None),
        "the first sample has no prior point to measure against"
    );

    // Taken AFTER the call, so it is at or past the instant `sample` read
    // internally: once `now()` passes this, the next sample is strictly
    // later than the first and the interval is real.
    wait_for_the_clock_to_tick(Instant::now());
    let (bps, eta) = s.sample(total / 2, total);
    assert!(
        bps > 0,
        "half the disc over real elapsed time is a non-zero speed, got {bps}"
    );
    assert!(
        eta.is_some(),
        "half done with real throughput must produce an ETA"
    );

    // And it is finished-aware, which no single constant tuple can be at
    // the same time as the assertions above.
    wait_for_the_clock_to_tick(Instant::now());
    let (_, done_eta) = s.sample(total, total);
    assert_eq!(done_eta, None, "nothing left to estimate once complete");
}

// ─── Boundary comparisons the round-6 mutation run left unpinned ────────

// A byte count that has not moved is NOT a new pass.
#[test]
fn a_flat_byte_count_does_not_re_anchor_the_pass_clock() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    s.observe(t0, 1000);
    s.observe(t0 + Duration::from_secs(5), 1000);
    s.observe(t0 + Duration::from_secs(15), 5000);

    let eta = s.eta_speed_mbs(t0 + Duration::from_secs(15), 0.0);
    // 4000 bytes over the FULL 15 s since the true pass start. The `<=`
    // mutant re-anchors at t0+5s and divides by 10 s instead.
    let expected = (4000.0 / BYTES_PER_MIB) / 15.0;
    assert!(
        (eta - expected).abs() < 1e-9,
        "expected the rate over the true 15s pass, got {eta} (expected {expected})"
    );
}

// A sample sitting exactly ON the window cutoff stays in the window.
#[test]
fn the_window_cutoff_is_inclusive_of_a_sample_on_the_boundary() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    let mib = 1024 * 1024u64;
    s.observe(t0, 0);
    s.observe(t0 + Duration::from_secs(1), 0);
    s.observe(t0 + Duration::from_secs(2), 100 * mib);
    // Window is the static 10 s, so cutoff == t0 + 1s exactly.
    let speed = s.observe(t0 + Duration::from_secs(11), 100 * mib);

    // Kept the t0+1s sample: 100 MiB over 10 s.
    //   `<=` would drop it   → 0 MiB over 9 s  → 0.0
    //   `==` would drop none → 100 MiB over 11 s → ~9.09
    assert!(
        (speed - 10.0).abs() < 1e-6,
        "the sample exactly on the cutoff must be retained, got {speed} MB/s"
    );
}

/// At exactly `ETA_WARMUP_SECS` the running average is already in use.
#[test]
fn eta_leaves_warmup_exactly_at_the_boundary() {
    let mut s = SpeedEstimator::new();
    let t0 = Instant::now();
    s.observe(t0, 0);
    s.observe(
        t0 + Duration::from_secs_f64(ETA_WARMUP_SECS),
        700 * 1024 * 1024,
    );
    // A sentinel no real rate can equal, so "fell back to display" is
    // unambiguous.
    let eta = s.eta_speed_mbs(t0 + Duration::from_secs_f64(ETA_WARMUP_SECS), 999.0);
    assert_ne!(
        eta, 999.0,
        "at exactly the warmup boundary the running average must be used, not the display fallback"
    );
    assert!(
        (eta - 70.0).abs() < 0.5,
        "700 MiB over 10 s is ~70 MB/s, got {eta}"
    );
}

/// The ETA floor is strict: exactly 0.0001 MB/s (1 MiB over 10000 s) is a stall.
#[test]
fn eta_floor_is_exclusive_at_exactly_the_threshold() {
    let mib = 1024 * 1024u64;
    let t0 = Instant::now();
    let at = t0 + Duration::from_secs(10_000);

    let mut s = SpeedEstimator::new();
    s.sample_at(t0, 0, 100 * mib);
    let (_, eta) = s.sample_at(at, mib, 100 * mib);
    assert_eq!(eta, None, "a rate exactly on the floor must yield no ETA");

    let mut s = SpeedEstimator::new();
    s.sample_at(t0, 0, 100 * mib);
    let (_, eta) = s.sample_at(at, 2 * mib, 100 * mib);
    assert!(
        eta.is_some(),
        "a rate just above the floor must yield an ETA"
    );
}
