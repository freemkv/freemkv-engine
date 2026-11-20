//! ET1, ET8 (stop design v5 §5.4) and the `linked` bridge.
//! Per spec; do not change without a spec citation proving otherwise.

use super::*;
use libfreemkv::Error;

struct Cancels(AtomicBool);
impl Sink for Cancels {
    fn should_cancel(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

// ET1 `engine_halt_is_op_or_extra` — §4.2: "`is_cancelled() = op || extra`".
#[test]
fn engine_halt_is_op_or_extra() {
    let op = Halt::new();
    let extra = Arc::new(AtomicBool::new(false));
    let h = EngineHalt::new(&op, Some(extra.clone()));
    assert!(!h.is_cancelled());
    op.cancel();
    assert!(h.is_cancelled(), "the op token alone cancels");

    let op = Halt::new();
    let h = EngineHalt::new(&op, Some(extra.clone()));
    extra.store(true, Ordering::SeqCst);
    assert!(h.is_cancelled(), "the narrower flag alone cancels");

    // §4.2 (remux): "`op.is_cancelled() || extra || sink.should_cancel()`".
    let sink = Cancels(AtomicBool::new(false));
    let h = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    assert!(!h.is_cancelled());
    sink.0.store(true, Ordering::SeqCst);
    assert!(h.is_cancelled(), "the Sink probe alone cancels");
}

// §4.2: the op token is always observed, also with no narrower flag wired.
#[test]
fn the_op_token_is_observed_without_extra() {
    let op = Halt::new();
    let h = EngineHalt::new(&op, None);
    op.cancel();
    assert!(h.is_cancelled());
    let t0 = Instant::now();
    assert!(
        h.wait(Duration::from_secs(30)),
        "a cancelled wait reports the cancel"
    );
    assert!(t0.elapsed() < Duration::from_secs(1));
}

// The libfreemkv token `linked` hands out follows every input within about one slice.
#[test]
fn linked_follows_op_extra_and_sink() {
    let within = |cancel: &dyn Fn(), h: &EngineHalt<'_>| {
        h.linked(|lh| {
            assert!(!lh.is_cancelled());
            cancel();
            let t0 = Instant::now();
            while !lh.is_cancelled() {
                assert!(t0.elapsed() < Duration::from_secs(1), "not linked");
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let (op, extra) = (Halt::new(), Arc::new(AtomicBool::new(false)));
    within(&|| op.cancel(), &EngineHalt::new(&op, Some(extra.clone())));
    within(
        &|| extra.store(true, Ordering::SeqCst),
        &EngineHalt::new(&Halt::new(), Some(extra.clone())),
    );
    let sink = Cancels(AtomicBool::new(false));
    let h = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    within(&|| sink.0.store(true, Ordering::SeqCst), &h);
    // The legacy view hands the narrower flag over exactly: the `from_arc` bridges stay exact.
    let flag = Arc::new(AtomicBool::new(false));
    EngineHalt::legacy(Some(flag.clone())).linked(|lh| assert!(Arc::ptr_eq(lh.as_arc(), &flag)));
}

// A bridged call returns when its work ends, not after the bridge's next poll slice: most
// short calls finish well inside one slice (a stall on a loaded runner may hold up a few).
#[test]
fn a_bridged_call_returns_without_waiting_for_the_next_slice() {
    let h = EngineHalt::new(&Halt::new(), Some(Arc::new(AtomicBool::new(false))));
    let slow = (0..20)
        .filter(|_| {
            let t0 = Instant::now();
            h.linked(|_| std::thread::sleep(Duration::from_millis(2)));
            t0.elapsed() >= WAIT_SLICE
        })
        .count();
    assert!(
        slow < 10,
        "{slow} of 20 short calls waited out a {WAIT_SLICE:?} slice"
    );
}

// An op already cancelled when the bridged work starts hands it a cancelled token at once.
#[test]
fn a_cancelled_op_reaches_the_bridged_work_before_it_starts() {
    let op = Halt::new();
    op.cancel();
    let h = EngineHalt::new(&op, Some(Arc::new(AtomicBool::new(false))));
    h.linked(|lh| assert!(lh.is_cancelled(), "the work started uncancelled"));
}

// ET8 `engine_outcome_mapping` — §2.6: `Halted` → `Stopped` only on a cancel (the op token,
// or the narrower flag and Sink the engine also honours, §4.2); "`TimedOut` → **Failed** always".
#[test]
fn engine_outcome_mapping() {
    let op = Halt::new();
    let h = EngineHalt::new(&op, None);
    let never = |_: &u8| false;
    let r = EngineOutcome::from_result(Err(Error::TimedOut { op: "verify" }), &h, never);
    assert!(matches!(r, EngineOutcome::Failed(Error::TimedOut { .. })));
    let r = EngineOutcome::from_result(Err(Error::SyncTimeout), &h, never);
    assert!(matches!(r, EngineOutcome::Failed(Error::SyncTimeout)));
    assert!(matches!(
        EngineOutcome::from_result(Ok(1u8), &h, never),
        EngineOutcome::Done(1)
    ));
    // An artifact result carrying its own Stop flag is Stopped, with the partial result.
    let r = EngineOutcome::from_result(Ok(2u8), &h, |_| true);
    assert!(matches!(r, EngineOutcome::Stopped(Some(2))));
    op.cancel();
    let r = EngineOutcome::from_result(Err(Error::Halted), &h, never);
    assert!(r.is_stopped());
    let r = EngineOutcome::from_result(Err(Error::TimedOut { op: "x" }), &h, never);
    assert!(
        matches!(r, EngineOutcome::Failed(_)),
        "TimedOut stays Failed after a Stop"
    );
    let extra = Arc::new(AtomicBool::new(true));
    let h = EngineHalt::new(&Halt::new(), Some(extra));
    let r = EngineOutcome::from_result(Err::<u8, _>(Error::Halted), &h, never);
    assert!(r.is_stopped(), "a narrower-flag cancel is a Stop: {r:?}");
    let sink = Cancels(AtomicBool::new(true));
    let h = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    let r = EngineOutcome::from_result(Err::<u8, _>(Error::Halted), &h, never);
    assert!(r.is_stopped(), "a Sink cancel is a Stop: {r:?}");
}

// ET8, last case — §2.6: "Otherwise it … maps to `Failed`", in every build profile. A
// non-sticky `should_cancel` or a source's own `Halted` reach it, so it must not panic.
#[test]
fn halted_without_a_cancel_is_failed() {
    let h = EngineHalt::new(&Halt::new(), None);
    let r = EngineOutcome::from_result(Err::<u8, _>(Error::Halted), &h, |_| false);
    assert!(matches!(r, EngineOutcome::Failed(Error::Halted)), "{r:?}");
}

// A Stop the engine observed stays a Stop even if a non-sticky `should_cancel` flips back.
#[test]
fn an_observed_cancel_is_sticky() {
    struct Once(AtomicBool);
    impl Sink for Once {
        fn should_cancel(&self) -> bool {
            self.0.swap(false, Ordering::SeqCst)
        }
    }
    let sink = Once(AtomicBool::new(true));
    let h = EngineHalt::new(&Halt::new(), None).with_sink(&sink);
    assert!(h.is_cancelled());
    let r = EngineOutcome::from_result(Err::<u8, _>(Error::Halted), &h, |_| false);
    assert!(r.is_stopped(), "{r:?}");
}

// `wait` that runs out uncancelled reports `false` (a normal pause, not a Stop), after the
// whole duration; a cancel during it reports `true` within a slice.
#[test]
fn wait_reports_a_timeout_as_false_and_a_cancel_as_true() {
    let op = Halt::new();
    let h = EngineHalt::new(&op, None);
    let t0 = Instant::now();
    assert!(
        !h.wait(Duration::from_millis(30)),
        "an uncancelled wait times out"
    );
    assert!(t0.elapsed() >= Duration::from_millis(30));
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            op.cancel();
        });
        let t0 = Instant::now();
        assert!(
            h.wait(Duration::from_secs(30)),
            "a cancel cuts the wait short"
        );
        assert!(t0.elapsed() < Duration::from_secs(5));
    });
}

// Only a legacy entry with no flag and no Sink is unwired: its private token never cancels.
#[test]
fn is_wired_unless_only_a_private_token_can_cancel() {
    let sink = Cancels(AtomicBool::new(false));
    let flag = Some(Arc::new(AtomicBool::new(false)));
    assert!(!EngineHalt::legacy(None).is_wired());
    assert!(EngineHalt::legacy(flag.clone()).is_wired());
    assert!(EngineHalt::legacy(None).with_sink(&sink).is_wired());
    assert!(EngineHalt::new(&Halt::new(), None).is_wired());
    assert!(EngineHalt::new(&Halt::new(), flag).is_wired());
}

#[test]
fn debug_names_the_view() {
    let h = EngineHalt::new(&Halt::new(), None);
    assert!(format!("{h:?}").contains("EngineHalt"));
}

#[test]
fn op_is_the_callers_token() {
    let op = Halt::new();
    let h = EngineHalt::new(&op, None);
    h.op().cancel();
    assert!(op.is_cancelled());
}

#[test]
fn linked_hands_the_op_token_itself_when_it_is_the_only_input() {
    let op = Halt::new();
    let h = EngineHalt::new(&op, None);
    h.linked(|t| t.cancel());
    assert!(op.is_cancelled());
}

#[test]
fn only_a_stop_outcome_is_stopped() {
    assert!(!EngineOutcome::Done(1u8).is_stopped());
    assert!(!EngineOutcome::<u8>::Failed(Error::Halted).is_stopped());
    assert!(EngineOutcome::<u8>::Stopped(None).is_stopped());
}

#[derive(Default)]
struct Seen {
    logs: std::sync::Mutex<Vec<String>>,
    titles: std::sync::atomic::AtomicUsize,
    completed: std::sync::atomic::AtomicUsize,
}
impl Sink for Seen {
    fn log(&self, _level: crate::sink::Level, msg: &str) {
        self.logs.lock().unwrap().push(msg.to_owned());
    }
    fn title_opened(&self, _title: &libfreemkv::DiscTitle) {
        self.titles.fetch_add(1, Ordering::SeqCst);
    }
    fn completed(&self, _outcome: &crate::Outcome) {
        self.completed.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn the_halt_sink_forwards_log_title_and_completion_to_the_inner_sink() {
    let inner = Seen::default();
    let halt = EngineHalt::new(&Halt::new(), None);
    let sink = HaltSink {
        inner: &inner,
        halt: &halt,
    };
    sink.log(crate::sink::Level::Info, "hello");
    sink.title_opened(&libfreemkv::DiscTitle::empty());
    sink.completed(&crate::Outcome {
        files: Vec::new(),
        unreadable_bytes: 0,
        lost_ms: 0.0,
        severity: crate::DamageSeverity::Clean,
        elapsed_secs: 0.0,
        avg_bps: 0,
    });
    assert_eq!(*inner.logs.lock().unwrap(), ["hello"]);
    assert_eq!(inner.titles.load(Ordering::SeqCst), 1);
    assert_eq!(inner.completed.load(Ordering::SeqCst), 1);
}

// A watched sink stays a cancel input of `is_cancelled`, but `linked` hands out the op token
// itself (no bridge thread): the run's watcher is what turns the sink's cancel into the token.
#[test]
fn a_watched_sink_is_asked_but_never_bridged() {
    let op = Halt::new();
    let sink = Cancels(AtomicBool::new(true));
    let h = EngineHalt::new(&op, None).with_watched_sink(&sink);
    assert!(h.is_cancelled(), "the sink is still asked");
    h.linked(|lib| {
        assert!(!lib.is_cancelled(), "no bridge mirrored the sink");
        op.cancel();
        assert!(lib.is_cancelled(), "the op token itself, not a child");
    });
    let flag = Arc::new(AtomicBool::new(false));
    let legacy = EngineHalt::legacy(Some(flag.clone())).with_watched_sink(&sink);
    legacy.linked(|lib| assert!(Arc::ptr_eq(lib.as_arc(), &flag)));
    assert!(legacy.is_wired());
}
