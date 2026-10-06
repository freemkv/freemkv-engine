use super::stall_fixtures::{Gate, StalledSink};
use super::*;
use libfreemkv::halt::Halt;
use libfreemkv::io::pipeline::{Flow, Pipeline, Sink, WRITE_THROUGH_DEPTH};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Cancel is flipped this long after the producer parks in `send` — long
// enough it's definitely parked, short enough to keep the test quick.
const HALT_AFTER: Duration = Duration::from_millis(500);
// Expected return is ~750ms after parking (one 250ms POLL_INTERVAL); 3s
// is 4x that. The regression this guards is UNBOUNDED, not "slower", so
// there's no window where the two could be confused.
const MAX_RETURN: Duration = Duration::from_secs(3);
/// Bound on the whole experiment so a regression FAILS instead of hanging
/// the suite.
const WATCHDOG: Duration = Duration::from_secs(10);

// A consumer that DIES on its first item, distinguishable from a stalled
// one. Must panic: `Flow::Stop`/an `apply` error both leave the consumer
// thread alive, so neither disconnects the channel.
struct DyingSink;

impl Sink<u32> for DyingSink {
    type Output = ();
    fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
        panic!("test fixture: consumer thread dies here");
    }
    fn close(self) -> std::result::Result<(), Error> {
        Ok(())
    }
}

// Park a producer against a stalled consumer, then flip the halt: it
// must come back promptly and report HALTED, not `PipelineConsumerGone`
// (a lie about a consumer that is alive).
#[test]
fn a_stop_lands_on_a_producer_parked_on_a_stalled_consumer() {
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Gate::shut();
    let pipe = Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, StalledSink::new(&entered, &gate))
        .expect("spawn consumer");
    let halt = Halt::new();

    // Item 1 is taken by the consumer, which then wedges inside `apply`.
    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let waited = Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            waited.elapsed() < WATCHDOG,
            "consumer never picked up the first item"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Item 2 fills the depth-1 channel. Now the pipeline is saturated and
    // the consumer is not coming back.
    assert_eq!(send_bounded(&pipe, 2, &halt), Ok(()));

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(HALT_AFTER);
            halt.cancel();
        });
        scope.spawn(|| {
            let t0 = Instant::now();
            // Item 3 has nowhere to go: this is the send that used to be
            // unkillable.
            let result = send_bounded(&pipe, 3, &halt);
            let _ = tx.send((result, t0.elapsed()));
        });

        let observed = rx.recv_timeout(WATCHDOG);
        // Release the consumer whatever happened, so both scoped threads
        // can join and the failure below is a FAILURE, not a hang.
        gate.open();

        let (result, elapsed) = observed.unwrap_or_else(|_| {
            panic!(
                "producer did not return within {WATCHDOG:?} of a halt \
                     raised {HALT_AFTER:?} in: it is parked in send() on a \
                     consumer that is alive but stalled, which is exactly the \
                     wedge Stop has to be able to break"
            )
        });
        assert_eq!(
            result,
            Err(SendStall::Halted),
            "a stalled consumer plus a halt is a HALT; reporting \
                 ConsumerGone would blame a thread that is alive"
        );
        assert!(
            elapsed < MAX_RETURN,
            "producer took {elapsed:?} to observe a halt raised at \
                 {HALT_AFTER:?}; budget is {MAX_RETURN:?}"
        );
    });

    drop(pipe);
}

// R7: a consumer whose `apply` failed keeps draining and discarding, so the non-blocking
// fast path would accept every later item and the producer read the whole disc.
#[test]
fn a_consumer_whose_apply_failed_refuses_every_later_item() {
    struct FailingSink;
    impl Sink<u32> for FailingSink {
        type Output = ();
        fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
            Err(Error::IoError {
                source: std::io::Error::other("ENOSPC"),
            })
        }
        fn close(self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }
    let pipe = Pipeline::<u32, ()>::spawn(WRITE_THROUGH_DEPTH, FailingSink).expect("spawn");
    let halt = Halt::new();
    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let t0 = Instant::now();
    while !pipe.consumer_failed() {
        assert!(t0.elapsed() < WATCHDOG, "the apply never failed");
        std::thread::sleep(Duration::from_millis(5));
    }
    for i in 2..10 {
        assert_eq!(
            send_bounded(&pipe, i, &halt),
            Err(SendStall::ConsumerGone),
            "item {i} was handed to a consumer that discards everything"
        );
    }
    assert!(
        pipe.finish().is_err(),
        "the apply error still reaches finish"
    );
}

// ...and the pass then fails with the consumer's error (the cause), not the
// producer's "consumer gone" that it stopped on.
#[test]
fn a_failed_consumer_s_own_error_fails_the_pass() {
    let enospc = || Error::IoError {
        source: std::io::Error::other("ENOSPC"),
    };
    let gone = || SendStall::ConsumerGone.into_error();
    let e = pass_failure(gone(), Err::<(), _>(enospc()), true);
    assert!(matches!(e, Error::IoError { .. }), "got {e:?}");
    // A producer failure of its own (the consumer still healthy) keeps precedence.
    let e = pass_failure(Error::DecryptFailed, Err::<(), _>(enospc()), false);
    assert!(matches!(e, Error::DecryptFailed), "got {e:?}");
    let e = pass_failure(gone(), Ok(()), true);
    assert!(matches!(e, Error::PipelineConsumerGone), "got {e:?}");
}

/// The other side of the discrimination: a consumer that is really gone
/// must still report `ConsumerGone`, not `Halted`, with the halt clear.
#[test]
fn a_departed_consumer_is_reported_gone_not_halted() {
    let pipe = Pipeline::<u32, ()>::spawn(WRITE_THROUGH_DEPTH, DyingSink).expect("spawn");
    let halt = Halt::new();
    // First send may land before the consumer exits; keep sending until
    // the channel disconnects (bounded, so a regression fails).
    let t0 = Instant::now();
    loop {
        match send_bounded(&pipe, 7, &halt) {
            Ok(()) => {
                assert!(
                    t0.elapsed() < WATCHDOG,
                    "consumer never departed: sends kept succeeding"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(stall) => {
                assert_eq!(stall, SendStall::ConsumerGone);
                break;
            }
        }
    }
    assert!(!halt.is_cancelled(), "no halt was ever raised");
}

/// The deadline branch: consumer alive, stalled, and NO halt. The producer
/// must still come back — as `Stalled`, which maps to a timeout error, not
/// to `PipelineConsumerGone`.
#[test]
fn a_stalled_consumer_with_no_halt_times_out_rather_than_blocking() {
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Gate::shut();
    let pipe = Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, StalledSink::new(&entered, &gate))
        .expect("spawn consumer");
    let halt = Halt::new();
    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let waited = Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(waited.elapsed() < WATCHDOG, "consumer never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(send_bounded(&pipe, 2, &halt), Ok(()));

    // A short deadline stands in for SEND_DEADLINE so this costs 600 ms
    // rather than 600 s. `send_with_halt` parks in POLL_INTERVAL slices,
    // so anything under one slice would not exercise the loop.
    let deadline = Duration::from_millis(600);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let t0 = Instant::now();
            let result = send_bounded_within(&pipe, 3, &halt, deadline);
            let _ = tx.send((result, t0.elapsed()));
        });
        // Same watchdog discipline as the halt test: a producer that never
        // returns must FAIL the suite, not hang it.
        let observed = rx.recv_timeout(WATCHDOG);
        gate.open();

        let (result, elapsed) = observed.unwrap_or_else(|_| {
            panic!(
                "producer did not return within {WATCHDOG:?} against a \
                     deadline of {deadline:?}: an unbounded send on a stalled \
                     consumer never comes back at all"
            )
        });
        assert_eq!(
            result,
            Err(SendStall::Stalled),
            "an alive-but-not-draining consumer is a stall, not a death"
        );
        assert!(
            matches!(SendStall::Stalled.into_error(), Error::PipelineJoinTimeout),
            "the stall must surface as a timeout, not as PipelineConsumerGone"
        );
        assert!(
            elapsed < MAX_RETURN,
            "deadline of {deadline:?} took {elapsed:?} to fire"
        );
    });
    drop(pipe);
}

// The other half of the halt contract: a raised halt must not discard a free-slot handoff
// that would not have blocked.
#[test]
fn a_raised_halt_still_delivers_when_the_channel_has_room() {
    /// Stalls inside `apply` on the FIRST item only. That reproduces the
    /// live shape — a consumer busy in a slow write — while leaving the
    /// depth-1 channel EMPTY, i.e. with room for exactly one more item.
    struct StallOnceSink {
        entered: Arc<AtomicUsize>,
        gate: Gate,
        seen: Arc<Mutex<Vec<u32>>>,
    }

    impl Sink<u32> for StallOnceSink {
        type Output = ();
        fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
            if self.entered.fetch_add(1, Ordering::SeqCst) == 0 {
                self.gate.wait();
            }
            self.seen.lock().unwrap().push(item);
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }

    let entered = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gate = Gate::shut();
    let pipe = Pipeline::<u32, ()>::spawn(
        WRITE_THROUGH_DEPTH,
        StallOnceSink {
            entered: Arc::clone(&entered),
            gate: gate.clone(),
            seen: Arc::clone(&seen),
        },
    )
    .expect("spawn consumer");
    let halt = Halt::new();

    // Item 1 is taken off the channel and the consumer wedges on it. The
    // channel is now EMPTY: one free slot, no producer can block on it.
    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let waited = Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            waited.elapsed() < WATCHDOG,
            "consumer never picked up the first item"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // Stop is pressed. Item 2 is the already-recovered span in the
    // producer's hand at that instant.
    halt.cancel();
    let result = send_bounded(&pipe, 2, &halt);

    // Release the consumer before asserting, so a failure is a FAILURE and
    // not a hang in `finish`.
    gate.open();

    assert_eq!(
        result,
        Ok(()),
        "the channel had a free slot, so this handoff could not block: a \
             raised halt must not discard bytes the drive already recovered"
    );
    pipe.finish().expect("clean close");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1, 2],
        "item 2 was accepted, so the consumer must have written it"
    );
}

/// Guard the healthy path: with a consumer that drains, `send_bounded` is
/// an ordinary send — every item lands, in order, and nothing is dropped.
#[test]
fn a_draining_consumer_still_receives_every_item() {
    struct CountingSink(Arc<AtomicUsize>);
    impl Sink<u32> for CountingSink {
        type Output = usize;
        fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
            self.0.fetch_add(item as usize, Ordering::SeqCst);
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<usize, Error> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let pipe = Pipeline::<u32, usize>::spawn(WRITE_THROUGH_DEPTH, CountingSink(Arc::clone(&seen)))
        .expect("spawn");
    let halt = Halt::new();
    for i in 1..=1000u32 {
        assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
    }
    assert_eq!(pipe.finish().expect("clean close"), 500_500);
}
