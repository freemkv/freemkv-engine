use super::stall_fixtures::{Gate, StalledSink};
use super::*;
use libfreemkv::halt::Halt;
use libfreemkv::io::pipeline::{Flow, Pipeline, Sink, WRITE_THROUGH_DEPTH};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// MARGIN: a correct teardown returns at ~5.3s (250ms halt-observe +
// FINISH_GRACE_SECS=5s spin); 15s leaves ~10s slack. The regression is
// UNBOUNDED, not "slower", so there's no confusable window.
const MAX_RETURN: Duration = Duration::from_secs(15);
// Bound on the whole experiment so a regression FAILS, not hangs, `cargo test`.
const WATCHDOG: Duration = Duration::from_secs(45);

// Park a consumer inside `apply`, raise the halt, then tear down: the
// teardown must COME BACK — the last link in the Stop chain.
#[test]
fn a_stop_lands_on_a_teardown_joining_a_stalled_consumer() {
    let entered = Arc::new(AtomicUsize::new(0));
    let gate = Gate::shut();
    let sink = StalledSink::new(&entered, &gate);
    let (closed, dropped) = (Arc::clone(&sink.closed), Arc::clone(&sink.dropped));
    let pipe = Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, sink).expect("spawn consumer");
    let halt = Halt::new();

    // Item 1 is taken; the consumer wedges inside `apply` and never
    // returns. This is the state `send_bounded` leaves behind after it
    // hands the producer back on a halt.
    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let waited = Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            waited.elapsed() < WATCHDOG,
            "consumer never picked up the first item"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // The operator's Stop. Raised BEFORE the teardown, exactly as the
    // production sites see it: the producer has already observed this bit
    // and broken out of its read loop.
    halt.cancel();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let t0 = Instant::now();
            // The teardown that used to be unkillable.
            let result = finish_bounded(pipe, &halt);
            let _ = tx.send((result.is_err(), t0.elapsed()));
        });

        let observed = rx.recv_timeout(WATCHDOG);
        // Release the stalled consumer whatever happened, so the scoped
        // thread can join and the assertion below is a FAILURE, not a hang.
        gate.open();

        let (was_err, elapsed) = observed.unwrap_or_else(|_| {
            panic!(
                "teardown did not return within {WATCHDOG:?} of a halt \
                     raised before it was even called: it is blocked in \
                     join() on a consumer that is alive but stalled. \
                     send_bounded got the producer out of exactly this wedge; \
                     leaving it here means Stop still never returns"
            )
        });
        assert!(
            was_err,
            "a consumer that never came back has no summary to report: \
                 the teardown must say Halted, not invent a result"
        );
        assert!(
            elapsed < MAX_RETURN,
            "teardown took {elapsed:?} to return after a halt; budget is \
                 {MAX_RETURN:?} (5 s grace + one 250 ms poll, ~3x margin)"
        );
    });

    // The abandoned consumer must NOT go on to run `close()`. For the
    // recovery sinks that call is `sync_all` + `map.flush()` against an
    // output the caller has already reported as interrupted.
    let waited = Instant::now();
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            waited.elapsed() < WATCHDOG,
            "the released consumer never finished"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        closed.load(Ordering::SeqCst),
        0,
        "an abandoned consumer must not finalise the output"
    );
}

/// Sets a flag when it is dropped. Declared AFTER the `Mapfile` field in
/// [`MapSink`] so it fires only once that mapfile has been dropped —
/// i.e. after its `Drop` flush has had its chance at the file.
struct SignalOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// The hung-mount consumer, in miniature: owns the pass's `Mapfile`,
// blocks inside `apply` exactly where `PatchSink`/`SweepSink` block (in
// the write, BEFORE the `record`), and records once the write returns.
struct MapSink {
    map: mapfile::Mapfile,
    _done: SignalOnDrop,
    gate: Gate,
    entered: Arc<AtomicUsize>,
}

impl Sink<u32> for MapSink {
    type Output = u32;
    fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        // The wedged write on the hung mount.
        self.gate.wait();
        // …which eventually returns, and the sink records it — the
        // interval since the last persist is long past, so this alone
        // rewrites the whole mapfile.
        self.map
            .record(0, 2048, mapfile::SectorStatus::Unreadable)
            .map_err(Error::from)?;
        Ok(Flow::Continue)
    }
    fn close(self) -> std::result::Result<u32, Error> {
        Ok(0)
    }
}

// A STOP MUST NOT COST THE NEXT PASS ITS RECORD: the abandoned consumer is detached, not
// killed, and its stale mapfile snapshot must never reach the path once a resumed pass owns
// it.
#[test]
fn an_abandoned_consumer_cannot_overwrite_a_resumed_passs_mapfile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.iso.mapfile");
    let total = 4096u64;

    // The abandoned pass's mapfile, with in-memory state that has not
    // reached disk (record() batches by FLUSH_INTERVAL).
    let mut map =
        mapfile::Mapfile::create(&path, total, "abandoned-pass").expect("create the pass mapfile");
    map.record(2048, 2048, mapfile::SectorStatus::NonTrimmed)
        .expect("record");
    let disown = map.disown_handle();

    let entered = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Gate::shut();
    let pipe = Pipeline::<u32, u32>::spawn(
        WRITE_THROUGH_DEPTH,
        MapSink {
            map,
            _done: SignalOnDrop(Arc::clone(&dropped)),
            gate: gate.clone(),
            entered: Arc::clone(&entered),
        },
    )
    .expect("spawn consumer");
    let halt = Halt::new();

    assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
    let waited = Instant::now();
    while entered.load(Ordering::SeqCst) == 0 {
        assert!(
            waited.elapsed() < WATCHDOG,
            "consumer never picked up the first item"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    // The operator's Stop, and the teardown that abandons the wedged
    // consumer — the SAME call both recovery passes make.
    halt.cancel();
    assert!(
        finish_bounded_disowning(pipe, &halt, &disown).is_err(),
        "a consumer that never came back has no summary to report"
    );

    // The resume. A second, independent `Mapfile` on the same path, as
    // `sweep`/`patch` would build it, recording real progress.
    {
        let mut resumed = mapfile::Mapfile::create(&path, total, "resumed-pass")
            .expect("create the resumed mapfile");
        resumed
            .record(0, total, mapfile::SectorStatus::Finished)
            .expect("record");
        resumed.flush().expect("flush the resumed mapfile");
    }
    let after_resume = std::fs::read_to_string(&path).expect("read the resumed mapfile");
    assert!(
        after_resume.contains("resumed-pass"),
        "fixture check: the resumed pass's mapfile is what is on disk"
    );

    // The mount comes back and the abandoned thread runs on: its
    // `record` persists, then its sink — and the mapfile in it — drops.
    gate.open();
    let waited = Instant::now();
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            waited.elapsed() < WATCHDOG,
            "the abandoned consumer never released its mapfile"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let now = std::fs::read_to_string(&path).expect("read the mapfile");
    assert_eq!(
        now, after_resume,
        "the abandoned consumer overwrote the resumed pass's mapfile with \
             its own stale snapshot — the resume's recorded progress is gone \
             from disk and those ranges will be read off the damaged disc all \
             over again"
    );
}

// THE CLEAN PATH MUST BE EXACT: a healthy consumer is joined and its
// `close()` output returned unchanged, even on a run that ended halted.
#[test]
fn a_healthy_consumer_is_still_joined_and_its_summary_returned() {
    struct CountingSink(u32);
    impl Sink<u32> for CountingSink {
        type Output = u32;
        fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
            self.0 += item;
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<u32, Error> {
            Ok(self.0)
        }
    }

    // (a) No halt raised: an ordinary end-of-pass teardown.
    let pipe = Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, CountingSink(0)).expect("spawn");
    let halt = Halt::new();
    for i in 1..=1000u32 {
        assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
    }
    assert_eq!(
        finish_bounded(pipe, &halt).expect("a draining consumer joins cleanly"),
        500_500,
        "the clean path must return the consumer's summary unchanged"
    );

    // (b) Halt RAISED, consumer healthy — the common Stop. Must not cost the
    // caller its summary: `finish_with_halt` checks `is_finished()` before the
    // halt, so a consumer that's coming back is still joined normally.
    let pipe = Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, CountingSink(0)).expect("spawn");
    let halt = Halt::new();
    for i in 1..=100u32 {
        assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
    }
    halt.cancel();
    let t0 = Instant::now();
    assert_eq!(
        finish_bounded(pipe, &halt).expect("a halted-but-healthy run still reports its summary"),
        5_050,
        "pressing Stop must not turn a completed pass's summary into an error"
    );
    assert!(
        t0.elapsed() < MAX_RETURN,
        "a healthy consumer must join promptly even with the halt up, \
             not sit out the full grace period"
    );
}
