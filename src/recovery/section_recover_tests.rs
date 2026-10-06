use super::*;
use libfreemkv::error::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

// Synthetic disc: dead LBAs, an optional transport-fault LBA, and an
// injectable per-read time cost advancing a shared fake clock (an
// AtomicU64 of nanoseconds) — no real sleeps in tests.
struct FakeDisc {
    dead: HashSet<u32>,
    /// LBAs that return a wedge-family sense (IllegalRequest) — the drive
    /// fast-fail state, distinct from an ordinary dead sector (which carries
    /// no sense). Used to exercise wedge detection.
    wedge: HashSet<u32>,
    transport_at: Option<u32>,
    clock_nanos: Arc<AtomicU64>,
    per_read: Duration,
    reads: Arc<AtomicU64>,
    // ── Physical failure-mode models (all default-empty) ─────────────────
    // Each conditional sector reads only when the drive state the handler
    // manipulates matches — so recovering it proves the technique was exercised.
    /// Current `SET CD SPEED` value (updated by `set_speed`); max at build.
    speed: u16,
    /// Reads ONLY at min speed (fails at max) → min-speed Linear / SpeedSweep.
    slow_only: HashSet<u32>,
    /// Reads ONLY on the Nth *physical* (FUA) attempt; a cached (non-FUA)
    /// re-read never gets it → FUA Linear / Bisect. Maps LBA → attempts required.
    fua_need: HashMap<u32, u32>,
    /// Physical (FUA) attempts observed so far, per LBA.
    fua_seen: HashMap<u32, u32>,
    /// Reads ONLY when approached from ABOVE (the previous physical access
    /// was a higher LBA) → Oscillate's reverse-into pass.
    dir_reverse_only: HashSet<u32>,
    /// Reads ONLY when the immediately-preceding sector was the previous
    /// physical access (PLL/servo primed) → CachePrime.
    prime_only: HashSet<u32>,
    /// LBA of the last sector physically accessed (success or fail) — the
    /// approach-direction / priming signal the specialists drive.
    last_lba: Option<u32>,
    /// Disc capacity in sectors, or 0 for "unknown" (the trait default, and
    /// what every pre-existing test here uses).
    capacity: u32,
    /// Reads that asked for an LBA at or past `capacity`. A real drive
    /// answers those with ILLEGAL REQUEST — a wedge-family sense — so a
    /// handler that issues one is feeding its own wedge detector.
    past_end: Arc<AtomicU64>,
    /// `(recovery, fua)` of every read, in order — the timeout / cache flags
    /// the handler actually asked the drive for.
    modes: Vec<(bool, bool)>,
    /// Every `SET CD SPEED` value issued, in order.
    speed_sets: Vec<u16>,
}

impl SectorSource for FakeDisc {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> Result<usize> {
        // Bulk (non-FUA) path.
        self.read_sectors_fua(lba, count, buf, recovery, false)
    }

    fn capacity_sectors(&self) -> u32 {
        self.capacity
    }

    fn read_sectors_fua(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
        fua: bool,
    ) -> Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.modes.push((recovery, fua));
        if self.capacity != 0 && lba + count as u32 > self.capacity {
            self.past_end.fetch_add(1, Ordering::Relaxed);
        }
        self.clock_nanos
            .fetch_add(self.per_read.as_nanos() as u64, Ordering::Relaxed);
        // The head moved across this span; record where it ended so the NEXT
        // read can see the approach direction / priming (both success and
        // failure move the head).
        let prev = self.last_lba;
        self.last_lba = Some(lba + count as u32 - 1);
        if let Some(t) = self.transport_at
            && (lba..lba + count as u32).contains(&t)
        {
            return Err(Error::ScsiError {
                opcode: libfreemkv::scsi::SCSI_READ_10,
                status: libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE,
                sense: None,
            });
        }
        for l in lba..lba + count as u32 {
            if self.wedge.contains(&l) {
                // Fast-fail wedge sense: ILLEGAL REQUEST 0x05/0x24, the real BU40N
                // signature. Non-transport status so it isn't a bus fault, but
                // carries sense so the wedge classifier sees it.
                return Err(Error::ScsiError {
                    opcode: libfreemkv::scsi::SCSI_READ_10,
                    status: 0x02,
                    sense: Some(libfreemkv::scsi::ScsiSense {
                        sense_key: libfreemkv::scsi::SENSE_KEY_ILLEGAL_REQUEST,
                        asc: 0x24,
                        ascq: 0x00,
                    }),
                });
            }
            if self.dead.contains(&l) {
                // Non-transport bad-sector error (CHECK CONDITION, 0x02).
                return Err(Error::DiscRead {
                    sector: l as u64,
                    status: Some(0x02),
                    sense: None,
                });
            }
            // Marginal sector: reads only at min spindle speed.
            if self.slow_only.contains(&l) && self.speed != SPEED_MIN_KBS {
                return Err(bad_sector(l));
            }
            // Stochastic sector: needs N physical (FUA) reads; a cached read
            // can never land it (cache masks the good re-read).
            if let Some(need) = self.fua_need.get(&l).copied() {
                if !fua {
                    return Err(bad_sector(l));
                }
                let seen = self.fua_seen.entry(l).or_insert(0);
                *seen += 1;
                if *seen < need {
                    return Err(bad_sector(l));
                }
            }
            // Direction-dependent tracking: reads only when approached from
            // above (previous physical access was a higher LBA).
            if self.dir_reverse_only.contains(&l) && prev.is_none_or(|p| p <= l) {
                return Err(bad_sector(l));
            }
            // Boundary sector: reads only when the preceding sector was the
            // previous physical access (servo primed).
            if self.prime_only.contains(&l) && prev != l.checked_sub(1) {
                return Err(bad_sector(l));
            }
        }
        let bytes = count as usize * SECTOR as usize;
        for (i, b) in buf[..bytes].iter_mut().enumerate() {
            *b = (lba as usize + i / SECTOR as usize) as u8;
        }
        Ok(bytes)
    }

    fn set_speed(&mut self, kbs: u16) {
        self.speed = kbs;
        self.speed_sets.push(kbs);
    }
}

/// The ordinary recoverable bad-sector error (CHECK CONDITION, no sense) the
/// conditional failure modes return when their precondition isn't met.
fn bad_sector(l: u32) -> Error {
    Error::DiscRead {
        sector: l as u64,
        status: Some(0x02),
        sense: None,
    }
}

/// Records every recovered span so a test can assert which sectors came back.
#[derive(Default)]
struct RecordSink {
    got: HashMap<u64, usize>, // pos -> bytes
}
impl RecoverySink for RecordSink {
    fn recovered(&mut self, pos: u64, buf: &[u8]) -> Result<()> {
        self.got.insert(pos, buf.len());
        Ok(())
    }
}

/// A fake clock plus a disc sharing its timeline.
struct Harness {
    clock_nanos: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    base: Instant,
}

impl Harness {
    fn build(dead: &[u32], transport_at: Option<u32>, per_read: Duration) -> (Self, FakeDisc) {
        let clock_nanos = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        let disc = FakeDisc {
            dead: dead.iter().copied().collect(),
            wedge: HashSet::new(),
            transport_at,
            clock_nanos: clock_nanos.clone(),
            per_read,
            reads: reads.clone(),
            speed: SPEED_MAX_KBS,
            slow_only: HashSet::new(),
            fua_need: HashMap::new(),
            fua_seen: HashMap::new(),
            dir_reverse_only: HashSet::new(),
            prime_only: HashSet::new(),
            last_lba: None,
            capacity: 0,
            past_end: Arc::new(AtomicU64::new(0)),
            modes: Vec::new(),
            speed_sets: Vec::new(),
        };
        (
            Harness {
                clock_nanos,
                reads,
                base: Instant::now(),
            },
            disc,
        )
    }

    fn now_fn(&self) -> impl Fn() -> Instant {
        let c = self.clock_nanos.clone();
        let base = self.base;
        move || base + Duration::from_nanos(c.load(Ordering::Relaxed))
    }

    fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

/// Build a ctx over `disc` with the fake clock — the common per-test setup.
macro_rules! ctx {
    ($disc:expr, $sink:expr, $now:expr) => {
        HandlerCtx {
            reader: &mut $disc,
            sink: &mut $sink,
            now: &$now,
            halt: None,
            tick: None,
            unproductive: 0,
            fatal: None,
            wedge_streak: 0,
            cur_speed: SPEED_MAX_KBS,
        }
    };
}

fn lba(pos: u64) -> u32 {
    (pos / SECTOR) as u32
}

// read_span must refuse an empty (count == 0) span as a FAILED read in EVERY build, not
// just debug (pins a real Good-path-with-no-read bug).
#[test]
fn read_span_refuses_an_empty_span_as_a_failed_read() {
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut buf = [0u8; SECTOR as usize];
    let hit = read_span(&mut ctx, &mut buf, 0, 0, ReadParams::fast());
    assert!(
        matches!(hit, ReadHit::Bad),
        "an empty span must be classified as a failed read"
    );
    assert!(
        sink.got.is_empty(),
        "an empty span must NEVER be recorded as recovered"
    );
    assert_eq!(
        h.read_count(),
        0,
        "the guard must fire before any read is issued"
    );
}

#[test]
fn chain_recovers_readable_in_a_dead_batch_leaving_only_dead() {
    // Section [0, 10 sectors). Dead: sectors 3 and 7. Linear reads it as one
    // failing batch and leaves it whole (no per-sector grind). Bisect then
    // probes/expands, salvaging the 8 readable sectors and leaving only 3, 7.
    let dead = [3u32, 7u32];
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 10 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    // Linear leaves the failed 10-sector batch whole.
    Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(
        bad.total_len(),
        10 * SECTOR,
        "linear leaves the dead batch whole"
    );
    // Bisect salvages the readable sectors around the dead ones.
    ctx.unproductive = 0;
    let out = Bisect {
        params: ReadParams::fast(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    // Exactly the two dead sectors remain.
    assert_eq!(bad.total_len(), 2 * SECTOR);
    for &(p, l) in bad.ranges() {
        assert_eq!(l, SECTOR);
        assert!(
            lba(p) == 3 || lba(p) == 7,
            "unexpected bad sector {}",
            lba(p)
        );
    }
}

#[test]
fn linear_forward_front_dead_still_reaches_readable_tail() {
    // THE bug: front dead, tail readable. Section [0, 40 sectors). First 32
    // (one whole batch) are dead; the tail 8 are readable. Forward linear
    // must recover the tail — it does not hang at the front.
    let dead: Vec<u32> = (0..32).collect();
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 40 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut lin = Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    };
    let out = lin.recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    // The 32 dead front sectors remain; the 8-sector readable tail is
    // recovered as one clean batch (one sink span covering 8 sectors).
    assert_eq!(bad.total_len(), 32 * SECTOR);
    assert_eq!(sink.got.len(), 1, "tail is one clean 8-sector batch");
    assert_eq!(
        sink.got.get(&(32 * SECTOR)).copied(),
        Some(8 * SECTOR as usize),
        "tail batch not recovered"
    );
}

#[test]
fn linear_honors_deadline_and_returns_promptly() {
    // 1000 clean sectors, but each read costs 1 s and the budget is 3 s. The
    // handler must stop after ~3 reads, NOT drain all 1000 — proving bounded
    // wall-clock even on a huge range.
    let (h, disc) = Harness::build(&[], None, Duration::from_secs(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 1000 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(3);
    let mut lin = Linear {
        direction: Direction::Forward,
        params: ReadParams::fast(),
    };
    let out = lin.recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    // Batch=32 clean sectors per read: a handful of reads at most, not 1000.
    assert!(
        h.read_count() <= 5,
        "ran {} reads, expected <=5",
        h.read_count()
    );
    assert!(bad.total_len() > 0, "should not have drained the range");
}

#[test]
fn bisect_finds_good_middle_in_mostly_dead_range() {
    // 9 sectors, only the middle (sector 4) readable. Bisect probes the
    // middle first, recovers it, and the recursive halves' middles are dead.
    let dead: Vec<u32> = (0..9).filter(|&l| l != 4).collect();
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 9 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut bis = Bisect {
        params: ReadParams::fast(),
    };
    let out = bis.recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert!(
        sink.got.contains_key(&(4 * SECTOR)),
        "good middle not found"
    );
    assert_eq!(
        bad.total_len(),
        8 * SECTOR,
        "only the middle should recover"
    );
}

// Pins Linear's DIRECTION axis, which nothing else exercised.
#[test]
fn linear_reverse_recovers_a_batch_forward_cannot_approach() {
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    // Sector 40 sits in the middle batch [32, 64) and reads only from above.
    disc.dir_reverse_only = [40u32].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let section = 96 * SECTOR; // 3 batches of BATCH_SECTORS (32)
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // Forward: batches [0,32) and [64,96) read, [32,64) cannot — the head
    // always arrives at sector 40 from below.
    let mut bad = SubRanges::from_section(0, section);
    let out = Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining, "forward cannot finish");
    assert_eq!(
        bad.total_len(),
        32 * SECTOR,
        "exactly the middle batch must be left behind"
    );
    assert_eq!(bad.ranges()[0].0, 32 * SECTOR, "and it is that batch");

    // Reverse: the walk reaches [32,64) having just read [64,96), so the
    // head comes from above and the batch reads.
    let mut bad = SubRanges::from_section(0, section);
    let out = Linear {
        direction: Direction::Reverse,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(
        out,
        HandlerOutcome::Complete,
        "reverse approaches from above and must clear the section"
    );
    assert!(bad.is_empty());
    assert_eq!(
        sink.got.get(&(32 * SECTOR)).copied(),
        Some(32 * SECTOR as usize),
        "the middle batch was handed to the sink whole"
    );
}

#[test]
fn coordinator_drains_the_readable_set_through_the_chain() {
    // Two dead sectors at opposite ends of a section shorter than one batch, so
    // both Linear arms fail and Bisect recovers everything. Tests the
    // coordinator (handler-after-handler drain), not the direction axis.
    let dead = [0u32, 15u32]; // ends of a 16-sector section
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 16 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Linear {
            direction: Direction::Reverse,
            params: ReadParams::deep(),
        }),
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::deep(),
        }),
        Box::new(Bisect {
            params: ReadParams::fast(),
        }),
    ];
    let deadline_base = (ctx.now)();
    let mut scoreboard = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut scoreboard, |_| {
        deadline_base + Duration::from_secs(30)
    });
    assert_eq!(out, HandlerOutcome::Remaining);
    // 14 readable sectors recovered, only the two dead ends remain.
    assert_eq!(bad.total_len(), 2 * SECTOR);
    for &(p, _) in bad.ranges() {
        assert!(lba(p) == 0 || lba(p) == 15);
    }
}

#[test]
fn coordinator_completes_when_no_dead_sectors() {
    // A clean section drains to Complete on the first handler.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 64 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![Box::new(Linear {
        direction: Direction::Forward,
        params: ReadParams::fast(),
    })];
    let base = (ctx.now)();
    let mut scoreboard = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut scoreboard, |_| {
        base + Duration::from_secs(30)
    });
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(bad.is_empty());
}

#[test]
fn transport_fault_short_circuits() {
    // A transport fault mid-range returns TransportFault immediately so the
    // caller can un-wedge the drive.
    let (h, disc) = Harness::build(&[], Some(5), Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    // One 8-sector batch (shorter than BATCH_SECTORS) that contains the transport LBA.
    let mut bad = SubRanges::from_section(0, 8 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut lin = Linear {
        direction: Direction::Forward,
        params: ReadParams::fast(),
    };
    let out = lin.recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::TransportFault);
}

#[test]
fn wedged_drive_aborts_fast_instead_of_grinding() {
    // Regression for the 2026-07-01 incident: a fast-fail wedge was classified
    // as ordinary bad sectors, grinding a dead drive for 28 min. A wholly-wedged
    // 1000-sector section must now bail after ~WEDGE_ABORT_STREAK reads.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.wedge = (0..1000u32).collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 1000 * SECTOR);
    // The full tier-0 chain: the wedge streak persists across handlers (only
    // `unproductive` resets per handler), so it reaches the abort threshold
    // even though each handler yields early on the dead streak.
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Bisect {
            params: ReadParams::fast(),
        }),
        Box::new(Jump {
            params: ReadParams::fast(),
        }),
        Box::new(Linear {
            direction: Direction::Reverse,
            params: ReadParams::fast(),
        }),
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::fast(),
        }),
    ];
    let mut scoreboard = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut scoreboard, |_| {
        (h.now_fn())() + Duration::from_secs(60)
    });
    assert_eq!(
        out,
        HandlerOutcome::TransportFault,
        "a wholly-wedged section must escalate to TransportFault"
    );
    // The whole point: bailed after exactly the documented streak (16, derived
    // from WEDGE_ABORT_STREAK), not after grinding all 1000 sectors. Used to
    // read `< 100`, an arbitrary bound that stayed green even quartered.
    assert_eq!(
        h.read_count(),
        16,
        "wedge must abort on the 16th consecutive fast-fail — no sooner \
             (that abandons a recoverable disc) and no later (that grinds a \
             dead drive)"
    );
}

#[test]
fn slow_hardware_error_media_does_not_false_trip_wedge_abort() {
    // A genuine uncorrectable sector reports a wedge-family sense but comes back
    // slow (real ECC recovery), which must not count toward the abort. Each read
    // costs 600ms (> WEDGE_FASTFAIL_MS), so it never escalates to TransportFault.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(600));
    let mut disc = disc;
    disc.wedge = (0..1000u32).collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 1000 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Bisect {
            params: ReadParams::fast(),
        }),
        Box::new(Jump {
            params: ReadParams::fast(),
        }),
        Box::new(Linear {
            direction: Direction::Reverse,
            params: ReadParams::fast(),
        }),
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::fast(),
        }),
    ];
    let mut scoreboard = HandlerScoreboard::default();
    // Long per-handler deadline so the deadline (not the wedge) is never the
    // reason a handler stops — we're isolating the wedge-escalation decision.
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut scoreboard, |_| {
        (h.now_fn())() + Duration::from_secs(3600)
    });
    assert_ne!(
        out,
        HandlerOutcome::TransportFault,
        "slow (ECC-recovery) Hardware-error reads must NOT trip the fast-fail wedge abort"
    );
    assert_eq!(
        ctx.wedge_streak, 0,
        "slow wedge-family reads must not accumulate the streak"
    );
}

#[test]
fn wedge_streak_persists_across_sections_for_tier1() {
    // Tier 1 is only two handlers, so one section builds at most 8 streak (below
    // WEDGE_ABORT_STREAK=16); caught only because wedge_streak persists across
    // sections. Simulate PatchCtx by carrying it across run_handlers calls.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.wedge = (0..4000u32).collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut carried = 0u32; // the pass-level wedge_streak
    let mut caught_on: Option<usize> = None;
    for section in 0..6usize {
        let mut ctx = HandlerCtx {
            reader: &mut disc,
            sink: &mut sink,
            now: &now,
            halt: None,
            tick: None,
            unproductive: 0,
            fatal: None,
            wedge_streak: carried,
            cur_speed: SPEED_MAX_KBS,
        };
        // Distinct 100-sector section per iteration, all within the wedge set.
        let pos = (section as u64) * 100 * SECTOR;
        let mut bad = SubRanges::from_section(pos, 100 * SECTOR);
        // Tier-1 shape: two slow Linear handlers, nothing that reaches 16 alone.
        let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
            Box::new(Linear {
                direction: Direction::Reverse,
                params: ReadParams::deep(),
            }),
            Box::new(Linear {
                direction: Direction::Forward,
                params: ReadParams::deep(),
            }),
        ];
        let mut sb = HandlerScoreboard::default();
        let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| {
            (h.now_fn())() + Duration::from_secs(60)
        });
        carried = ctx.wedge_streak;
        if out == HandlerOutcome::TransportFault {
            caught_on = Some(section);
            break;
        }
    }
    let caught = caught_on.expect("a two-handler tier must still catch the wedge");
    assert!(
        caught >= 1,
        "one 2-handler section can't reach the streak alone; the wedge must be \
             caught via cross-section accumulation, not on section 0 (caught on {caught})"
    );
}

#[test]
fn halt_token_returns_promptly() {
    // Halt set before the call: the handler returns Halted on its first
    // check, having done no reads.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let halt = AtomicBool::new(true);
    let mut ctx = HandlerCtx {
        reader: &mut disc,
        sink: &mut sink,
        now: &now,
        halt: Some(&halt),
        tick: None,
        unproductive: 0,
        fatal: None,
        wedge_streak: 0,
        cur_speed: SPEED_MAX_KBS,
    };
    let mut bad = SubRanges::from_section(0, 100 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut lin = Linear {
        direction: Direction::Forward,
        params: ReadParams::fast(),
    };
    let out = lin.recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Halted);
    assert_eq!(h.read_count(), 0, "halt must precede any read");
}

/// A `SectorSource` wrapper that flips a shared halt flag right after the
/// `after`-th underlying read completes — used to prove a handler observes
/// `ctx.halted()` BETWEEN reads, not just once at the top of its loop.
struct HaltAfterN<'a> {
    inner: &'a mut FakeDisc,
    halt: &'a AtomicBool,
    after: u64,
    seen: u64,
}

impl SectorSource for HaltAfterN<'_> {
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> Result<usize> {
        self.read_sectors_fua(lba, count, buf, recovery, false)
    }

    fn capacity_sectors(&self) -> u32 {
        self.inner.capacity_sectors()
    }

    fn read_sectors_fua(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
        fua: bool,
    ) -> Result<usize> {
        let r = self.inner.read_sectors_fua(lba, count, buf, recovery, fua);
        self.seen += 1;
        if self.seen == self.after {
            self.halt.store(true, Ordering::Relaxed);
        }
        r
    }

    fn set_speed(&mut self, kbs: u16) {
        self.inner.set_speed(kbs)
    }
}

// Drives Oscillate::recover with the halt flag flipping after the flip_after-th read,
// returns (outcome, total reads).
fn oscillate_halt_after(flip_after: u64) -> (HandlerOutcome, u64) {
    let dead = [5u32];
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let halt = AtomicBool::new(false);
    let mut wrapped = HaltAfterN {
        inner: &mut disc,
        halt: &halt,
        after: flip_after,
        seen: 0,
    };
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = HandlerCtx {
        reader: &mut wrapped,
        sink: &mut sink,
        now: &now,
        halt: Some(&halt),
        tick: None,
        unproductive: 0,
        fatal: None,
        wedge_streak: 0,
        cur_speed: SPEED_MAX_KBS,
    };
    let mut bad = SubRanges::from_section(5 * SECTOR, 2 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut osc = Oscillate {
        params: ReadParams::fast(),
    };
    let out = osc.recover(&mut ctx, &mut bad, deadline);
    (out, h.read_count())
}

#[test]
fn oscillate_halts_promptly_after_prime_below() {
    // Flag flips right after read 1 (prime-below lba4). A correct
    // handler checks immediately after and returns Halted having issued
    // NO further reads.
    let (out, reads) = oscillate_halt_after(1);
    assert_eq!(out, HandlerOutcome::Halted);
    assert_eq!(
        reads, 1,
        "halt flipped after the prime-below read — the handler must \
             check before the forward target read, not after it (got {reads} reads)"
    );
}

#[test]
fn oscillate_halts_promptly_after_forward_target() {
    // Flag flips right after read 2 (forward target lba5, dead, so `recovered`
    // stays false and reverse-into is entered). A correct handler checks
    // immediately and returns Halted before the prime-above read.
    let (out, reads) = oscillate_halt_after(2);
    assert_eq!(out, HandlerOutcome::Halted);
    assert_eq!(
        reads, 2,
        "halt flipped after the forward target read — the handler must \
             check before the prime-above read, not after it (got {reads} reads)"
    );
}

#[test]
fn oscillate_halts_promptly_after_prime_above() {
    // Flag flips right after read 3 (prime-above lba6). A correct
    // handler checks immediately after and returns Halted before the
    // final (reverse-into) target read.
    let (out, reads) = oscillate_halt_after(3);
    assert_eq!(out, HandlerOutcome::Halted);
    assert_eq!(
        reads, 3,
        "halt flipped after the prime-above read — the handler must \
             check before the reverse-into target read, not after it (got {reads} reads)"
    );
}

// Drives SpeedSweep::recover with the halt flag flipping after the flip_after-th read,
// returns (outcome, total reads).
fn speed_sweep_halt_after(flip_after: u64) -> (HandlerOutcome, u64) {
    let dead = [5u32];
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let halt = AtomicBool::new(false);
    let mut wrapped = HaltAfterN {
        inner: &mut disc,
        halt: &halt,
        after: flip_after,
        seen: 0,
    };
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = HandlerCtx {
        reader: &mut wrapped,
        sink: &mut sink,
        now: &now,
        halt: Some(&halt),
        tick: None,
        unproductive: 0,
        fatal: None,
        wedge_streak: 0,
        cur_speed: SPEED_MAX_KBS,
    };
    let mut bad = SubRanges::from_section(5 * SECTOR, 2 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(10);
    let mut sweep = SpeedSweep {
        params: ReadParams::fast(),
    };
    let out = sweep.recover(&mut ctx, &mut bad, deadline);
    (out, h.read_count())
}

#[test]
fn speed_sweep_halts_promptly_between_its_max_and_min_reads() {
    // Flag flips right after read 1 (Max-speed read of dead lba 5). A correct
    // handler checks immediately and returns Halted with no further reads — the
    // Min-speed read is another whole deep-timeout read (up to a minute).
    let (out, reads) = speed_sweep_halt_after(1);
    assert_eq!(out, HandlerOutcome::Halted);
    assert_eq!(
        reads, 1,
        "halt flipped after the max-speed read — the handler must check \
             before dropping the spindle for the min-speed read, not after \
             it (got {reads} reads)"
    );
}

#[test]
fn scorecard_decays_so_a_late_starter_overtakes_an_early_winner() {
    // The whole point of the DECAYED rate: the residual hardens mid-pass, so
    // leadership must hand off. A cumulative rate would freeze "early" in the
    // lead forever; the EWMA re-prices continuously.
    let mut sb = HandlerScoreboard::default();
    let dt = Duration::from_secs(1);

    // Round 1 — "early" cleans the easy bulk; "late" finds nothing yet.
    sb.record("early", 1_000_000_000, dt);
    sb.record("late", 0, dt);
    assert!(
        sb.rank("early") > sb.rank("late"),
        "early must lead once it's the only one recovering"
    );

    // The bulk is gone. Now "early"'s technique no longer fits the hardened
    // residual (barren attempts) while "late"'s specialist starts winning. By
    // cumulative rate early still leads (1e9/13 s vs 12e6/13 s); decayed, it does not.
    for _ in 0..12 {
        sb.record("early", 0, dt);
        sb.record("late", 1_000_000, dt);
    }
    assert!(
        sb.rank("late") > sb.rank("early"),
        "a handler that stops earning must LOSE its lead to a late starter \
             (late={}, early={})",
        sb.rank("late"),
        sb.rank("early")
    );

    // Calibration invariants preserved: an untried handler still ranks top
    // (one-shot calibration), and a handler attempted with no timed read
    // (zero elapsed) ranks bottom rather than crowding out proven performers.
    assert_eq!(sb.rank("never_tried"), u64::MAX, "untried → top");
    sb.record("idle", 0, Duration::ZERO);
    assert_eq!(sb.rank("idle"), 0, "attempted-but-zero-time → bottom");
}

fn min_deep() -> ReadParams {
    ReadParams {
        speed: SpeedPref::Min,
        fua: false,
        timeout: TimeoutPref::Deep,
    }
}

#[test]
fn min_speed_linear_recovers_a_min_speed_only_sector_that_max_linear_misses() {
    // Sector 5 reads ONLY at min spindle speed (weak signal / servo drift):
    // a max-speed deep Linear leaves it bad; Linear pinned to min speed
    // recovers it. Single-sector residual so Linear reads it directly.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.slow_only = [5u32].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(5 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // Max-speed deep Linear cannot read a min-only sector.
    let out = Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(
        bad.total_len(),
        SECTOR,
        "max-speed linear must leave it bad"
    );

    // Linear at min speed — recovers it.
    let out = Linear {
        direction: Direction::Forward,
        params: min_deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(
        bad.is_empty(),
        "min-speed Linear must recover the min-only sector"
    );
    assert_eq!(sink.got.get(&(5 * SECTOR)).copied(), Some(SECTOR as usize));
}

#[test]
fn speed_sweep_recovers_a_min_speed_only_sector() {
    // SpeedSweep sweeps Max→Min per sector, so it reaches the min-only
    // sector 7 that a max-only read never gets — proving the sweep actually
    // drops the spindle when the fast read fails.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.slow_only = [7u32].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(7 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    let out = SpeedSweep {
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(bad.is_empty(), "SpeedSweep must reach min and recover it");
    assert_eq!(sink.got.get(&(7 * SECTOR)).copied(), Some(SECTOR as usize));
    // It tried the fast (max) read first, then the min read — 2 reads.
    assert_eq!(h.read_count(), 2, "swept max then min");
}

fn max_fua_deep() -> ReadParams {
    ReadParams {
        speed: SpeedPref::Max,
        fua: true,
        timeout: TimeoutPref::Deep,
    }
}

#[test]
fn fua_reads_recover_a_stochastic_sector_a_cached_read_keeps_missing() {
    // Sector 9 lands only on its 2nd physical (FUA) read; a cached re-read
    // never gets it. Linear fwd+rev + Bisect at FUA params: across
    // its reads the sector gets enough physical attempts to land.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.fua_need = [(9u32, 2u32)].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(9 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // Cached (non-FUA) reads keep missing — twice, and the sector stays bad
    // (a cached miss never even counts as a physical attempt).
    for _ in 0..2 {
        let out = Linear {
            direction: Direction::Forward,
            params: ReadParams::deep(),
        }
        .recover(&mut ctx, &mut bad, deadline);
        assert_eq!(out, HandlerOutcome::Remaining);
        assert_eq!(bad.total_len(), SECTOR, "cached read must keep missing");
    }

    // FUA group: Linear fwd (FUA attempt 1) leaves it, Linear rev (FUA
    // attempt 2) lands it.
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Linear {
            direction: Direction::Forward,
            params: max_fua_deep(),
        }),
        Box::new(Linear {
            direction: Direction::Reverse,
            params: max_fua_deep(),
        }),
        Box::new(Bisect {
            params: max_fua_deep(),
        }),
    ];
    let mut sb = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(
        bad.is_empty(),
        "the FUA group must land the stochastic sector"
    );
    assert_eq!(sink.got.get(&(9 * SECTOR)).copied(), Some(SECTOR as usize));
}

#[test]
fn min_speed_fua_linear_recovers_the_hardest_sector_needing_both() {
    // Sector 11 is the hardest case: it reads ONLY at min speed AND ONLY on a
    // physical (FUA) read. Neither lever alone works — Linear at
    // {min, fua, deep} is the combination that recovers it.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.slow_only = [11u32].into_iter().collect();
    disc.fua_need = [(11u32, 1u32)].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(11 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // FUA but max speed → wrong speed, fails.
    let out = Linear {
        direction: Direction::Forward,
        params: max_fua_deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(
        bad.total_len(),
        SECTOR,
        "max+fua must miss the min-only sector"
    );

    // Min speed but cached (no FUA) → no physical attempt, fails.
    let out = Linear {
        direction: Direction::Forward,
        params: min_deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(
        bad.total_len(),
        SECTOR,
        "min+cached must miss the FUA-only sector"
    );

    // Both levers: min speed AND FUA → recovers.
    let out = Linear {
        direction: Direction::Forward,
        params: ReadParams {
            speed: SpeedPref::Min,
            fua: true,
            timeout: TimeoutPref::Deep,
        },
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(
        bad.is_empty(),
        "min+fua Linear must recover the hardest sector"
    );
    assert_eq!(sink.got.get(&(11 * SECTOR)).copied(), Some(SECTOR as usize));
}

#[test]
fn oscillate_recovers_a_direction_dependent_sector_forward_linear_misses() {
    // Sector 13 reads ONLY when approached from ABOVE (reverse-into). A plain
    // forward Linear (approaches from below) misses it; Oscillate's
    // reverse-into pass recovers it.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.dir_reverse_only = [13u32].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(13 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // Forward Linear approaches from below → misses the reverse-only sector.
    let out = Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(bad.total_len(), SECTOR, "forward linear must miss it");

    // Oscillate tries forward-into then reverse-into → the reverse-into pass
    // approaches from above and lands it.
    let out = Oscillate {
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(
        bad.is_empty(),
        "Oscillate must recover the direction-dependent sector"
    );
    assert_eq!(sink.got.get(&(13 * SECTOR)).copied(), Some(SECTOR as usize));
}

// A sector already handed to the sink must never be left in the residual bad set
// (Oscillate's prime reads, not just its target reads).
#[test]
fn oscillate_never_leaves_a_sector_it_recovered_in_the_bad_set() {
    let (h, disc) = Harness::build(&[1u32], None, Duration::from_secs(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    // The residual is sectors 1 and 2 — a two-sector sub-range, so sector 1's
    // prime-above target IS sector 2.
    let mut bad = SubRanges::from_section(SECTOR, 2 * SECTOR);
    // Four reads happen while processing sector 1 (prime below, target,
    // prime above, target again) at 1 s each; the budget ends exactly there,
    // so the loop yields before sector 2 is read on its own account.
    let deadline = (ctx.now)() + Duration::from_secs(4);

    let out = Oscillate {
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);

    // The fixture has to actually exercise the path, or the invariant below
    // is vacuous: sector 2's bytes must have reached the sink via the prime.
    assert_eq!(
        sink.got.get(&(2 * SECTOR)).copied(),
        Some(SECTOR as usize),
        "fixture invalid: the prime-above read of sector 2 did not land"
    );

    // The invariant. Stated over every recovered span rather than over
    // sector 2 alone, so the prime-below direction is covered too.
    for &pos in sink.got.keys() {
        let still_bad = bad
            .ranges()
            .iter()
            .any(|&(p, l)| pos >= p && pos < p.saturating_add(l));
        assert!(
            !still_bad,
            "sector at byte {pos} was recovered and handed to the sink, yet \
                 is still in the residual bad set — the final tier will record \
                 NonTrimmed over bytes we successfully read"
        );
    }
}

// Oscillate must not prime from past the end of the disc (reverse-into reads the sector
// ABOVE the target).
#[test]
fn oscillate_does_not_prime_past_the_end_of_the_disc() {
    const CAP: u32 = 20;
    let (h, disc) = Harness::build(&[CAP - 1], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.capacity = CAP;
    let past_end = disc.past_end.clone();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    // The residual sector is the very last one on the disc.
    let mut bad = SubRanges::from_section((CAP as u64 - 1) * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    let out = Oscillate {
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);

    assert_eq!(
        out,
        HandlerOutcome::Remaining,
        "the sector is genuinely dead"
    );
    assert_eq!(
        past_end.load(Ordering::Relaxed),
        0,
        "Oscillate asked the drive for a sector past the last LBA — that \
             comes back ILLEGAL REQUEST and feeds the wedge detector"
    );
}

// Pins prime_above_is_in_range at its three boundaries (named for the
// helper — it is all this test drives).
#[test]
fn prime_above_is_in_range_treats_an_unknown_capacity_as_no_bound() {
    assert!(
        prime_above_is_in_range(19 * SECTOR, 0),
        "an unknown capacity is not permission to invent a bound"
    );
    assert!(prime_above_is_in_range(18 * SECTOR, 20));
    assert!(
        !prime_above_is_in_range(19 * SECTOR, 20),
        "the last sector of a 20-sector disc has nothing above it"
    );
}

#[test]
fn cache_prime_recovers_a_boundary_sector_that_needs_a_warm_channel() {
    // Sector 15 reads only when the preceding sector was just read (servo
    // primed) — a boundary the drive can't lock onto from a cold seek. Cold
    // Linear misses it; CachePrime reads the preceding sector first, then lands it warm.
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    disc.prime_only = [15u32].into_iter().collect();
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(15 * SECTOR, SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);

    // Cold Linear read (never touches the preceding sector) → misses it.
    let out = Linear {
        direction: Direction::Forward,
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(
        bad.total_len(),
        SECTOR,
        "cold linear must miss the boundary sector"
    );

    // CachePrime reads the preceding run first → warm channel → lands it.
    let out = CachePrime {
        params: ReadParams::deep(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Complete);
    assert!(
        bad.is_empty(),
        "CachePrime must recover the primed boundary sector"
    );
    assert_eq!(sink.got.get(&(15 * SECTOR)).copied(), Some(SECTOR as usize));
}

/// A source whose every read fails with one fixed, non-read error.
struct FailingSource(fn() -> Error);
impl SectorSource for FailingSource {
    fn read_sectors(&mut self, _: u32, _: u16, _: &mut [u8], _: bool) -> Result<usize> {
        Err((self.0)())
    }
}

// A non-read error (here a key stop) ends the chain Fatal with the error kept,
// never as a transport fault (which the pass reads as a dead bus to spin-cycle).
#[test]
fn a_non_read_error_ends_the_chain_fatal_not_as_a_transport_fault() {
    let (h, _) = Harness::build(&[], None, Duration::from_millis(1));
    let mut src = FailingSource(|| Error::WholeDiscKeyMissing);
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(src, sink, now);
    let mut bad = SubRanges::from_section(0, 64 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::fast(),
        }),
        Box::new(Bisect {
            params: ReadParams::fast(),
        }),
    ];
    let deadline = (ctx.now)() + Duration::from_secs(30);
    let mut sb = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| deadline);
    assert_eq!(out, HandlerOutcome::Fatal);
    assert!(
        matches!(ctx.fatal, Some(Error::WholeDiscKeyMissing)),
        "the error that ended the chain is kept for the caller: {:?}",
        ctx.fatal
    );
    assert_eq!(bad.total_len(), 64 * SECTOR, "nothing claimed recovered");
}

// A Stop landing mid-read comes back as `Error::Halted`: the read stays bad and the
// halt check ends the chain Halted, never Fatal (which would fail a stopped pass).
#[test]
fn a_read_the_stop_interrupts_ends_the_chain_halted() {
    struct HaltingSource(Arc<AtomicBool>);
    impl SectorSource for HaltingSource {
        fn read_sectors(&mut self, _: u32, _: u16, _: &mut [u8], _: bool) -> Result<usize> {
            self.0.store(true, Ordering::Relaxed);
            Err(Error::Halted)
        }
    }
    let (h, _) = Harness::build(&[], None, Duration::from_millis(1));
    let flag = Arc::new(AtomicBool::new(false));
    let mut src = HaltingSource(flag.clone());
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(src, sink, now);
    ctx.halt = Some(flag.as_ref());
    let mut bad = SubRanges::from_section(0, 64 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![Box::new(Linear {
        direction: Direction::Forward,
        params: ReadParams::fast(),
    })];
    let deadline = (ctx.now)() + Duration::from_secs(30);
    let mut sb = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| deadline);
    assert_eq!(out, HandlerOutcome::Halted);
    assert!(
        ctx.fatal.is_none(),
        "a stop is not a fatal error: {:?}",
        ctx.fatal
    );
    assert_eq!(bad.total_len(), 64 * SECTOR, "nothing claimed recovered");
}

/// Refuses every span, as the patch sink does once its consumer is gone.
#[derive(Default)]
struct RefusingSink {
    calls: u32,
}
impl RecoverySink for RefusingSink {
    fn recovered(&mut self, _pos: u64, _buf: &[u8]) -> Result<()> {
        self.calls += 1;
        Err(Error::PipelineConsumerGone)
    }
}

// A sink that can no longer write ends the chain at once: no more drive reads
// into a dead sink, and the refused span stays bad (it was never written).
#[test]
fn a_refusing_sink_stops_the_chain_and_keeps_the_span_bad() {
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RefusingSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 96 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::fast(),
        }),
        Box::new(Linear {
            direction: Direction::Reverse,
            params: ReadParams::fast(),
        }),
    ];
    let deadline = (ctx.now)() + Duration::from_secs(30);
    let mut sb = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| deadline);
    assert_eq!(out, HandlerOutcome::Fatal);
    assert!(matches!(ctx.fatal, Some(Error::PipelineConsumerGone)));
    assert_eq!(h.read_count(), 1, "no read after the sink refused");
    assert_eq!(sink.calls, 1);
    assert_eq!(bad.total_len(), 96 * SECTOR, "an unwritten span stays bad");
}

// Pins the decayed rate itself: exact EWMA values (alpha 0.5) that a
// cumulative bytes/second average does not produce.
#[test]
fn scorecard_rate_is_an_ewma_of_timed_samples() {
    let mut sb = HandlerScoreboard::default();
    let dt = Duration::from_secs(1);
    sb.record("h", 1000, dt);
    assert_eq!(sb.rank("h"), 1000, "seeded to the first sample");
    sb.record("h", 0, dt);
    sb.record("h", 0, dt);
    assert_eq!(
        sb.rank("h"),
        250,
        "halved per barren sample (cumulative: 333)"
    );
    sb.record("h", 400, dt);
    assert_eq!(sb.rank("h"), 325, "0.5 * 400 + 0.5 * 250 (cumulative: 350)");
}

// Jump: after JUMP_AFTER_FAILS dead batches it skips to the middle of what is
// left, recovers the readable tail, and leaves the skipped span bad.
#[test]
fn jump_skips_half_the_remainder_and_recovers_the_tail() {
    let dead: Vec<u32> = (0..64).collect();
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 200 * SECTOR);
    let deadline = (ctx.now)() + Duration::from_secs(30);
    let out = Jump {
        params: ReadParams::fast(),
    }
    .recover(&mut ctx, &mut bad, deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    // Dead [0,32) and [32,64), then jump by half of the 168 sectors left
    // from 32: resume at 116 and read the tail in batches.
    assert_eq!(h.read_count(), 5);
    assert_eq!(bad.ranges(), &[(0, 116 * SECTOR)], "skipped span stays bad");
    let mut got: Vec<(u64, usize)> = sink.got.iter().map(|(&p, &l)| (p, l)).collect();
    got.sort();
    let s = SECTOR as usize;
    assert_eq!(
        got,
        vec![
            (116 * SECTOR, 32 * s),
            (148 * SECTOR, 32 * s),
            (180 * SECTOR, 20 * s)
        ]
    );
}

// Fast scouts ask for the short timeout, deep reads for the ECC budget, and
// FUA reaches the drive only when the params ask for it.
#[test]
fn read_params_reach_the_drive_as_recovery_and_fua_flags() {
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let deadline = (now)() + Duration::from_secs(30);
    let fua_fast = ReadParams {
        speed: SpeedPref::Max,
        fua: true,
        timeout: TimeoutPref::Fast,
    };
    for params in [ReadParams::fast(), ReadParams::deep(), fua_fast] {
        let mut ctx = ctx!(disc, sink, now);
        let mut bad = SubRanges::from_section(0, SECTOR);
        let out = Linear {
            direction: Direction::Forward,
            params,
        }
        .recover(&mut ctx, &mut bad, deadline);
        assert_eq!(out, HandlerOutcome::Complete);
    }
    assert_eq!(
        disc.modes,
        vec![(false, false), (true, false), (false, true)]
    );
}

// run_handlers restores max speed after a min-speed handler, and read_span
// programs the spindle only when the wanted speed changes (not per read).
#[test]
fn spindle_speed_is_set_on_change_and_restored_after_a_handler() {
    let dead: Vec<u32> = (0..96).collect();
    let (h, disc) = Harness::build(&dead, None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut bad = SubRanges::from_section(0, 96 * SECTOR);
    let mut handlers: Vec<Box<dyn SectionHandler>> = vec![
        Box::new(Linear {
            direction: Direction::Forward,
            params: min_deep(),
        }),
        Box::new(Linear {
            direction: Direction::Forward,
            params: ReadParams::fast(),
        }),
    ];
    let deadline = (ctx.now)() + Duration::from_secs(30);
    let mut sb = HandlerScoreboard::default();
    let out = run_handlers(&mut ctx, &mut handlers, &mut bad, &mut sb, |_| deadline);
    assert_eq!(out, HandlerOutcome::Remaining);
    assert_eq!(ctx.cur_speed, SPEED_MAX_KBS);
    assert_eq!(h.read_count(), 6, "three batches per handler");
    assert_eq!(
        disc.speed_sets,
        vec![SPEED_MIN_KBS, SPEED_MAX_KBS],
        "one SET CD SPEED for three min-speed reads, one restore after"
    );
    assert_eq!(disc.speed, SPEED_MAX_KBS);
}

// The runtime guard's second half: a pos that is not a sector multiple is a
// failed read, issued to no drive and never recorded as recovered.
#[test]
fn read_span_refuses_an_unaligned_pos_as_a_failed_read() {
    let (h, disc) = Harness::build(&[], None, Duration::from_millis(1));
    let mut disc = disc;
    let mut sink = RecordSink::default();
    let now = h.now_fn();
    let mut ctx = ctx!(disc, sink, now);
    let mut buf = [0u8; SECTOR as usize];
    let hit = read_span(&mut ctx, &mut buf, 1024, 1, ReadParams::fast());
    assert!(matches!(hit, ReadHit::Bad));
    assert!(sink.got.is_empty());
    assert_eq!(h.read_count(), 0, "the guard fires before any read");
}

// WEDGE_FASTFAIL_MS is exclusive: a wedge sense back in 499 ms counts toward
// the abort streak, one back in exactly 500 ms does not.
#[test]
fn the_wedge_fast_fail_gate_is_strictly_below_500_ms() {
    for (ms, want) in [(WEDGE_FASTFAIL_MS - 1, 1), (WEDGE_FASTFAIL_MS, 0)] {
        let (h, disc) = Harness::build(&[], None, Duration::from_millis(ms));
        let mut disc = disc;
        disc.wedge = [0u32].into_iter().collect();
        let mut sink = RecordSink::default();
        let now = h.now_fn();
        let mut ctx = ctx!(disc, sink, now);
        let mut buf = [0u8; SECTOR as usize];
        let hit = read_span(&mut ctx, &mut buf, 0, 1, ReadParams::fast());
        assert!(matches!(hit, ReadHit::Bad));
        assert_eq!(ctx.wedge_streak, want, "a wedge sense back in {ms} ms");
    }
    assert_eq!(WEDGE_FASTFAIL_MS, 500);
}
