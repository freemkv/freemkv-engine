use super::sleep_secs_or_halt;
use crate::engine_halt::EngineHalt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// The pause must actually happen (a mutation run once replaced the whole function with `()`
// and the suite stayed green).
#[test]
fn it_actually_sleeps_when_not_halted() {
    let halt = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    sleep_secs_or_halt(1, &EngineHalt::legacy(Some(halt)));
    let e = t0.elapsed();
    // Generous lower bound: the point is "roughly a second", not precision.
    assert!(
        e >= Duration::from_millis(800),
        "returned after {e:?} — the sleep did not happen"
    );
}

/// And it must break out early when halt is already set, rather than
/// serving the full pause. This is the difference between Stop being
/// honoured and the operator waiting out a multi-second cooldown.
#[test]
fn an_already_set_halt_returns_promptly() {
    let halt = Arc::new(AtomicBool::new(true));
    let t0 = Instant::now();
    sleep_secs_or_halt(30, &EngineHalt::legacy(Some(halt.clone())));
    let e = t0.elapsed();
    assert!(
        e < Duration::from_secs(5),
        "waited {e:?} on an already-halted sleep"
    );
}

/// A halt raised WHILE the pause is in progress must also cut it short —
/// the polling loop, not just the entry check.
#[test]
fn a_halt_raised_mid_sleep_cuts_it_short() {
    let halt = Arc::new(AtomicBool::new(false));
    let h = halt.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        h.store(true, Ordering::Relaxed);
    });
    let t0 = Instant::now();
    sleep_secs_or_halt(30, &EngineHalt::legacy(Some(halt.clone())));
    let e = t0.elapsed();
    assert!(
        e < Duration::from_secs(3),
        "waited {e:?} — the loop did not observe the halt"
    );
    assert!(
        e >= Duration::from_millis(150),
        "returned in {e:?} — suspiciously early, did it sleep at all?"
    );
}

/// Zero seconds is a no-op either way; pinned so the early return cannot
/// silently become a real sleep.
#[test]
fn zero_seconds_returns_immediately() {
    let t0 = Instant::now();
    sleep_secs_or_halt(0, &EngineHalt::legacy(None));
    assert!(t0.elapsed() < Duration::from_millis(800));
}
