use super::*;

/// A minimal disc for the image-length gate. Only capacity matters here.
fn guard_disc(sectors: u32) -> libfreemkv::Disc {
    libfreemkv::Disc {
        volume_id: "TESTDISC".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: sectors,
        capacity_bytes: sectors as u64 * 2048,
        layers: 1,
        titles: vec![],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

struct NoReader;
impl libfreemkv::sector::SectorSource for NoReader {
    fn read_sectors(
        &mut self,
        _lba: u32,
        _count: u16,
        _buf: &mut [u8],
        _decrypt: bool,
    ) -> std::result::Result<usize, libfreemkv::Error> {
        panic!("the image-length gate must refuse BEFORE any sector is read");
    }
}

// A patch pass walks only BAD mapfile ranges, so a Finished range past a
// truncation point looks like recovered data. `patch` must refuse before
// reading a single sector, like `copy`/`sweep` already do (NoReader panics).
#[test]
fn patch_refuses_an_image_shorter_than_the_mapfile_describes() {
    let dir = std::env::temp_dir().join(format!("fmkv-patch-trunc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("short.iso");

    let sectors = 64u32;
    let disc = guard_disc(sectors);
    let full = disc.capacity_bytes;

    // The mapfile describes the WHOLE disc, with one bad range to patch.
    let mapfile_path = disc.mapfile_for(&iso);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
    mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
    mf.record(2048, 2048, mapfile::SectorStatus::NonTrimmed)
        .unwrap();
    mf.flush().unwrap();

    // …but the image on disk is half that long.
    std::fs::write(&iso, vec![0u8; (full / 2) as usize]).unwrap();

    let opts = PatchOptions::for_patch_pass(true, None, None);
    let err = match patch(&disc, &mut NoReader, &iso, &opts) {
        Err(e) => e,
        Ok(_) => panic!("a truncated image must not be patched and called good"),
    };
    match err {
        Error::ImageTruncated { have, want } => {
            assert_eq!(have, full / 2, "reports the length actually found");
            assert_eq!(want, full, "reports the length the mapfile describes");
        }
        other => panic!("expected ImageTruncated, got {other:?}"),
    }

    let _ = std::fs::remove_file(&mapfile_path);
    let _ = std::fs::remove_dir_all(&dir);
}

// A mapfile covering only part of the disc must be REFUSED, not patched
// and reported complete: its last entry becomes the denominator for a
// disc whose other half was never read. `copy` refuses this; `patch` did not.
#[test]
fn patch_refuses_a_mapfile_that_does_not_cover_the_disc() {
    let dir = std::env::temp_dir().join(format!("fmkv-patch-cover-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("half.iso");

    let sectors = 64u32;
    let disc = guard_disc(sectors);
    // Literal, not derived from the disc: 64 sectors of 2048 = 131072.
    assert_eq!(disc.capacity_bytes, 131_072);
    let half = 65_536u64;

    let mapfile_path = disc.mapfile_for(&iso);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, half, "vTEST").unwrap();
    mf.record(0, half, mapfile::SectorStatus::Finished).unwrap();
    mf.record(2048, 2048, mapfile::SectorStatus::NonTrimmed)
        .unwrap();
    mf.flush().unwrap();
    // The image matches the MAPFILE exactly, so the truncated-image gate
    // is satisfied — this must still be refused.
    std::fs::write(&iso, vec![0u8; half as usize]).unwrap();

    let opts = PatchOptions::for_patch_pass(true, None, None);
    let err = match patch(&disc, &mut NoReader, &iso, &opts) {
        Err(e) => e,
        Ok(out) => panic!(
            "a mapfile covering {} of {} bytes must not be patched (reported total {})",
            half, 131_072, out.bytes_total
        ),
    };
    match err {
        Error::MapfileInvalid { kind } => assert_eq!(kind, "coverage"),
        other => panic!("expected MapfileInvalid{{coverage}}, got {other:?}"),
    }

    let _ = std::fs::remove_file(&mapfile_path);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate must not fire on a healthy image — otherwise every normal
/// resume would be refused.
#[test]
fn patch_accepts_an_image_of_the_length_the_mapfile_describes() {
    let dir = std::env::temp_dir().join(format!("fmkv-patch-intact-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("full.iso");

    let sectors = 64u32;
    let disc = guard_disc(sectors);
    let full = disc.capacity_bytes;

    let mapfile_path = disc.mapfile_for(&iso);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
    mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    std::fs::write(&iso, vec![0u8; full as usize]).unwrap();

    // Nothing bad to patch, so this returns without reading a sector; any
    // refusal at all (not just ImageTruncated) fails the healthy resume.
    let opts = PatchOptions::for_patch_pass(true, None, None);
    let out = match patch(&disc, &mut NoReader, &iso, &opts) {
        Ok(out) => out,
        Err(e) => panic!("an image of exactly the right length must be accepted: {e:?}"),
    };
    assert_eq!((out.bytes_total, out.bytes_good), (full, full));
    assert!(!out.halted && !out.wedged_exit);

    let _ = std::fs::remove_file(&mapfile_path);
    let _ = std::fs::remove_dir_all(&dir);
}

// A NON-REGULAR destination is exempt from the truncation gate: a
// character device like `/dev/null` always stat's as 0 bytes regardless
// of writes, so an unconditional length gate would refuse every such pass.
#[test]
#[cfg(unix)]
fn patch_does_not_apply_the_truncation_gate_to_a_non_regular_destination() {
    let dev_null = std::path::Path::new("/dev/null");

    // Unique volume id → a unique temp mapfile name (`mapfile_for` derives
    // the /dev/null mapfile name from the disc), so this cannot collide
    // with a concurrently-running test's mapfile.
    let mut disc = guard_disc(64);
    disc.volume_id = format!("fmkv-devnull-gate-{}", std::process::id());
    let full = disc.capacity_bytes;

    let mapfile_path = disc.mapfile_for(dev_null);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
    mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
    mf.flush().unwrap();

    // /dev/null reports len 0 while the mapfile describes `full` bytes:
    // the short-image condition is satisfied, and must NOT fire. Nothing is
    // bad, so the pass reads no sector (NoReader would panic).
    assert_eq!(
        std::fs::metadata(dev_null).unwrap().len(),
        0,
        "precondition: the character device measures as zero-length"
    );
    let opts = PatchOptions::for_patch_pass(false, None, None);
    let r = patch(&disc, &mut NoReader, dev_null, &opts);
    let _ = std::fs::remove_file(&mapfile_path);
    match r {
        Err(Error::ImageTruncated { have, want }) => panic!(
            "a /dev/null patch pass must not be refused as truncated \
                 (have={have}, want={want}) — a character device has no meaningful length"
        ),
        Err(other) => panic!("unexpected patch failure: {other:?}"),
        Ok(_) => {}
    }
}

/// The flat bandit pool must contain every handler from every tier, with a
/// UNIQUE name per config — the scoreboard keys on the name, so any two
/// handlers sharing a name would blur each other's decayed-yield ranking.
#[test]
fn flat_pool_is_all_tiers_with_unique_names() {
    let flat = build_flat_pool();
    let tiered: usize = (0..PATCH_TIERS).map(|t| build_tier_handlers(t).len()).sum();
    assert_eq!(
        flat.len(),
        tiered,
        "flat pool must equal the sum of all tier rosters"
    );
    let mut names: Vec<String> = flat.iter().map(|h| h.name()).collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "every flat-pool handler must have a unique scoreboard name"
    );
}

// Scheduler knobs (`FREEMKV_PATCH_FLAT`, `FREEMKV_PATCH_FLAT_BUDGET`). Tests
// used to WRITE real env vars under a mutex, unsound since sibling tests touch
// the environment concurrently regardless; now parsing uses pure fns / a thread-local.

/// Restores both thread-local knob overrides on drop, including on panic,
/// so a failing assertion cannot leak a knob into the next test that runs
/// on this thread.
struct KnobGuard;
impl Drop for KnobGuard {
    fn drop(&mut self) {
        flat_mode_override(None);
        flat_budget_override(None);
    }
}

// The flat-mode toggle's parsing rule: unset / empty / "0" → tier ladder,
// anything else → flat bandit. Calls the real function, not a
// locally-defined copy that could drift from it silently.
#[test]
fn flat_mode_toggle_reads_the_env_value() {
    assert!(
        !flat_mode_from_value(None),
        "unset must keep the proven tier ladder"
    );
    assert!(!flat_mode_from_value(Some("")), "empty is not an opt-in");
    assert!(
        !flat_mode_from_value(Some("0")),
        "0 is the explicit off switch"
    );

    assert!(flat_mode_from_value(Some("1")), "1 selects the flat bandit");
    assert!(
        flat_mode_from_value(Some("true")),
        "any non-empty, non-zero value opts in — not just \"1\""
    );
}

// `patch_flat_mode` — the function production calls — really is that
// rule plus the env lookup, not a second copy of it.
#[test]
fn patch_flat_mode_defaults_to_the_tier_ladder() {
    let _g = KnobGuard;
    flat_mode_override(Some(true));
    assert!(
        patch_flat_mode(),
        "the override reaches the production reader"
    );
    flat_mode_override(Some(false));
    assert!(!patch_flat_mode());
    flat_mode_override(None);
    assert_eq!(
        patch_flat_mode(),
        flat_mode_from_value(std::env::var("FREEMKV_PATCH_FLAT").ok().as_deref()),
        "with no override, the reader is exactly the parsed env value"
    );
}

// The flat per-handler EXPLORE budget, exercised through the real
// function: a wrong default or missing floor either starves the pool or
// spins forever on a handler with a zero-second deadline.
#[test]
fn flat_handler_budget_defaults_parses_and_floors() {
    assert_eq!(flat_budget_from_value(None), 12, "shipped default is 12 s");
    assert_eq!(
        flat_budget_from_value(Some("30")),
        30,
        "an override is honoured"
    );
    assert_eq!(
        flat_budget_from_value(Some("  7 ")),
        7,
        "surrounding space is trimmed"
    );

    // A zero/negative budget would make every deadline already-expired, so
    // handlers would be entered and abandoned without a single read.
    assert_eq!(flat_budget_from_value(Some("0")), 1, "floored at 1 s");

    // Garbage must fall back to the default, not to 0 and not to a panic.
    for junk in ["", "abc", "-5", "12s"] {
        assert_eq!(
            flat_budget_from_value(Some(junk)),
            12,
            "unparseable {junk:?} must fall back to the default"
        );
    }
}

// A misconfigured budget must not take the pass down with it: a 19-digit
// typo in an env var previously aborted the pass via `Instant + Duration`
// overflow. Asserts the requirement, not the fix's specific ceiling.
#[test]
fn an_absurd_handler_budget_degrades_instead_of_panicking() {
    let now = std::time::Instant::now();
    for v in [u64::MAX.to_string(), "9999999999999999999".to_string()] {
        let secs = flat_budget_from_value(Some(&v));
        let deadline = handler_deadline(now, secs);
        assert!(
            deadline >= now,
            "a deadline in the past is not a degradation, it is a different bug"
        );
    }
    // The ordinary case is untouched: a sane budget is exactly `now + secs`.
    assert_eq!(
        handler_deadline(now, 12),
        now + std::time::Duration::from_secs(12),
        "a workable budget must not be capped, clamped or rounded"
    );
}

/// A reader that fails every read and remembers the LBA order it was asked
/// for. Every handler therefore yields on its own dead-read limit, and the
/// recorded sequence is a direct trace of the SCHEDULER's walk.
struct TraceReader {
    lbas: Vec<u32>,
}
impl libfreemkv::sector::SectorSource for TraceReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        _count: u16,
        _buf: &mut [u8],
        _decrypt: bool,
    ) -> std::result::Result<usize, libfreemkv::Error> {
        self.lbas.push(lba);
        // An ORDINARY bad sector (CHECK CONDITION / medium error): must not
        // be mistaken for a transport fault, which would end the pass early
        // and destroy the trace.
        Err(Error::DiscRead {
            sector: lba as u64,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(libfreemkv::scsi::ScsiSense {
                sense_key: 0x03,
                asc: 0x11,
                ascq: 0x00,
            }),
        })
    }
}

/// Run one real patch pass over two bad ranges and return the LBA trace.
/// The caller owns the flat-mode knob (via `flat_mode_override`) around this.
fn trace_two_range_pass(tag: &str, a: (u32, u32), b: (u32, u32)) -> Vec<u32> {
    let dir = std::env::temp_dir().join(format!("fmkv-flat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("trace.iso");

    let disc = guard_disc(2000);
    let full = disc.capacity_bytes;
    let mapfile_path = disc.mapfile_for(&iso);
    let _ = std::fs::remove_file(&mapfile_path);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
    mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
    for &(lba, count) in &[a, b] {
        mf.record(
            lba as u64 * 2048,
            count as u64 * 2048,
            mapfile::SectorStatus::NonTrimmed,
        )
        .unwrap();
    }
    mf.flush().unwrap();
    std::fs::write(&iso, vec![0u8; full as usize]).unwrap();

    let mut reader = TraceReader { lbas: Vec::new() };
    let opts = PatchOptions::for_patch_pass(false, None, None);
    patch(&disc, &mut reader, &iso, &opts).expect("the pass itself must complete");

    let _ = std::fs::remove_dir_all(&dir);
    reader.lbas
}

// END-TO-END through the FLAT scheduler: with `FREEMKV_PATCH_FLAT` set,
// `PatchCtx::run` finishes each range's whole handler pool before moving on;
// the tier ladder instead sweeps every range at tier 0, then again at tier 1.
#[test]
fn the_flat_scheduler_finishes_a_range_before_starting_the_next() {
    // Range A is the LARGER one, so the (size desc, pos asc) sort puts it
    // first in BOTH schedulers — the only variable left is the walk shape.
    let a = (100u32, 16u32);
    let b = (1000u32, 4u32);
    let in_a = |l: &u32| *l >= a.0 && *l < a.0 + a.1;
    let in_b = |l: &u32| *l >= b.0 && *l < b.0 + b.1;

    let _g = KnobGuard;
    // A 1 s per-handler budget keeps the pass quick; the handlers all yield
    // on dead reads long before it, so it does not change the walk.
    flat_budget_override(Some(1));

    flat_mode_override(Some(true));
    let flat = trace_two_range_pass("flat", a, b);
    flat_mode_override(Some(false));
    let tiered = trace_two_range_pass("tier", a, b);
    drop(_g);

    // Both schedulers must actually visit both ranges, or the ordering
    // assertions below would hold vacuously.
    for (name, trace) in [("flat", &flat), ("tiered", &tiered)] {
        assert!(
            trace.iter().any(in_a) && trace.iter().any(in_b),
            "{name}: both bad ranges must be attempted (trace: {trace:?})"
        );
    }

    let last_a = flat.iter().rposition(in_a).unwrap();
    let first_b = flat.iter().position(in_b).unwrap();
    assert!(
        last_a < first_b,
        "FLAT: the whole handler pool must finish range A before range B is \
             touched — range A was read again at trace index {last_a}, after \
             range B started at {first_b} (trace: {flat:?})"
    );

    // The contrast that proves the trace really observes the scheduler:
    // the default ladder DOES come back to range A after range B.
    let last_a_t = tiered.iter().rposition(in_a).unwrap();
    let first_b_t = tiered.iter().position(in_b).unwrap();
    assert!(
        last_a_t > first_b_t,
        "TIERED: the breadth-first ladder must revisit range A on a later \
             tier, after range B's tier-0 pass (trace: {tiered:?})"
    );
}

// Transport failure (status=0xFF, USB-bridge crash) must be recognised
// and abort the pass rather than being hammered as an ordinary bad
// sector. Also checks an ordinary read error is NOT misclassified.
#[test]
fn transport_failure_is_recognised_for_patch_abort() {
    use libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE;

    // The exact shape Drive::read surfaces on a bridge crash.
    let tf = Error::DiscRead {
        sector: 1_392_314,
        status: Some(SCSI_STATUS_TRANSPORT_FAILURE),
        sense: None,
    };
    assert!(
        tf.is_scsi_transport_failure(),
        "a DiscRead with status=0xFF must classify as a transport failure so \
             patch aborts the pass"
    );

    // The raw ScsiError form (e.g. straight from the transport) too.
    let tf_raw = Error::ScsiError {
        opcode: 0x28,
        status: SCSI_STATUS_TRANSPORT_FAILURE,
        sense: None,
    };
    assert!(tf_raw.is_scsi_transport_failure());

    // An ordinary recoverable bad sector (CHECK CONDITION with sense) must
    // NOT trip the transport-failure abort — it should still be retried /
    // marked NonTrimmed, not abort the whole pass.
    let bad_sector = Error::DiscRead {
        sector: 1_392_314,
        status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
        sense: Some(libfreemkv::scsi::ScsiSense {
            sense_key: 0x03,
            asc: 0x11,
            ascq: 0x00,
        }),
    };
    assert!(
        !bad_sector.is_scsi_transport_failure(),
        "an ordinary bad-sector CHECK CONDITION must not be misclassified as \
             a transport failure"
    );
}

// A reader that under-delivers must not have its buffer believed.
#[test]
fn recovery_read_rejects_a_short_transfer() {
    /// Reports `Ok(full)` while filling only the FIRST sector.
    struct ShortReader;
    impl SectorSource for ShortReader {
        fn read_sectors(
            &mut self,
            _lba: u32,
            _count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> Result<usize> {
            buf[..2048].fill(0x11);
            Ok(2048)
        }
    }

    // 4 sectors requested, 1 delivered.
    let mut buf = vec![0xAAu8; 4 * 2048];
    let err = recovery_read(&mut ShortReader, 9, 4, &mut buf, true, false)
        .expect_err("a short transfer is a failed read, not a partial success");
    assert!(
        matches!(
            err,
            Error::DiscRead {
                sector: 9,
                status: None,
                sense: None
            }
        ),
        "classified exactly as Drive::read_one classifies a residual \
             underrun, got {err:?}"
    );
}

// SubRanges — the still-bad work-list the per-section recovery phases (#50)
// shrink. Pure data structure; exhaustively tested so each future phase
// helper can assert on its residual ranges.

#[test]
fn subranges_from_section_and_basics() {
    let s = SubRanges::from_section(2048, 10 * 2048);
    assert!(!s.is_empty());
    assert_eq!(s.total_len(), 10 * 2048);
    assert_eq!(s.ranges(), &[(2048, 10 * 2048)]);
    assert!(SubRanges::from_section(2048, 0).is_empty());
    assert!(SubRanges::default().is_empty());
}

#[test]
fn subranges_remove_middle_splits() {
    // [0,20k) minus [8k,12k) -> [0,8k) + [12k,20k)
    let mut s = SubRanges::from_section(0, 20 * 1024);
    s.remove(8 * 1024, 4 * 1024);
    assert_eq!(s.ranges(), &[(0, 8 * 1024), (12 * 1024, 8 * 1024)]);
    assert_eq!(s.total_len(), 16 * 1024);
}

#[test]
fn subranges_remove_prefix_suffix_and_whole() {
    // prefix
    let mut s = SubRanges::from_section(1000, 1000);
    s.remove(900, 200); // [1000,1100) trimmed off the front
    assert_eq!(s.ranges(), &[(1100, 900)]);
    // suffix
    let mut s = SubRanges::from_section(1000, 1000);
    s.remove(1800, 500); // [1800,2000) trimmed off the back
    assert_eq!(s.ranges(), &[(1000, 800)]);
    // whole (exact + over-cover both clear it)
    let mut s = SubRanges::from_section(1000, 1000);
    s.remove(1000, 1000);
    assert!(s.is_empty());
    let mut s = SubRanges::from_section(1000, 1000);
    s.remove(0, 100_000);
    assert!(s.is_empty());
}

#[test]
fn subranges_remove_gap_and_zero_are_noops() {
    let mut s = SubRanges::from_section(1000, 1000);
    s.remove(5000, 1000); // disjoint, after
    s.remove(0, 500); // disjoint, before
    s.remove(1200, 0); // zero-len
    assert_eq!(s.ranges(), &[(1000, 1000)]);
}

#[test]
fn subranges_remove_spanning_two_ranges() {
    // two sub-ranges, removal straddling the gap trims the inner edges
    let mut s = SubRanges::from_section(0, 4096);
    s.remove(1024, 1024); // -> [0,1024) + [2048,4096)
    assert_eq!(s.ranges(), &[(0, 1024), (2048, 2048)]);
    s.remove(512, 2048); // covers tail of first + head of second
    assert_eq!(s.ranges(), &[(0, 512), (2560, 1536)]);
}

// A typed failure from the writeback flusher (SyncTimeout, Halted) must reach the
// caller typed, not re-wrapped as IoError (read downstream as a dead USB bridge).
#[test]
fn a_typed_sync_failure_surfaces_typed_not_as_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("out.iso");
    std::fs::write(&iso, vec![0u8; 8192]).unwrap();
    let map = Mapfile::create(&dir.path().join("out.map"), 8192, "test").unwrap();
    let (mut sink, _shared) = PatchSink::new(&iso, map, true, None).unwrap();
    let halt = libfreemkv::halt::Halt::new();
    halt.cancel();
    sink.file.set_halt(halt);
    let err = sink.close().err().expect("a halted fsync must fail");
    assert!(matches!(err, Error::Halted), "got {err:?}");
}

// The snapshot is replaced whole, so a panic elsewhere while holding its lock
// leaves nothing half-written: publishing and reporting keep going, no cascade.
#[test]
fn a_poisoned_snapshot_lock_does_not_cascade_the_panic() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("out.iso");
    std::fs::write(&iso, vec![0u8; 8192]).unwrap();
    let map = Mapfile::create(&dir.path().join("out.map"), 8192, "test").unwrap();
    let (sink, shared) = PatchSink::new(&iso, map, true, None).unwrap();
    let poisoner = shared.clone();
    let _ = std::thread::spawn(move || {
        let _g = poisoner.lock().unwrap();
        panic!("poison the snapshot lock");
    })
    .join();
    assert!(shared.is_poisoned());

    sink.publish_now();
    let calls = std::sync::atomic::AtomicU32::new(0);
    let reporter = |e: &libfreemkv::Event<'_>| {
        if let libfreemkv::Event::Pass(_) = e {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    };
    let opts = PatchOptions::for_patch_pass(true, Some(&reporter), None);
    let state = PatchLoopState::new(0, 1, 0);
    assert!(!report_patch_progress(
        &guard_disc(4),
        &state,
        &opts,
        8192,
        &shared,
        &EngineHalt::legacy(None),
    ));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the reporter still hears the snapshot"
    );
}

/// Fails every read with one fixed error, counting the reads.
struct FixedErrReader {
    err: fn(u32) -> Error,
    reads: u32,
}
impl libfreemkv::sector::SectorSource for FixedErrReader {
    fn read_sectors(
        &mut self,
        lba: u32,
        _count: u16,
        _buf: &mut [u8],
        _decrypt: bool,
    ) -> std::result::Result<usize, libfreemkv::Error> {
        self.reads += 1;
        Err((self.err)(lba))
    }
}

/// One real patch pass over single-sector bad ranges at `lbas` (a 2000-sector disc).
fn patch_bad_sectors(tag: &str, lbas: &[u32], reader: &mut FixedErrReader) -> PatchOutcome {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join(format!("{tag}.iso"));
    let disc = guard_disc(2000);
    let full = disc.capacity_bytes;
    let mapfile_path = disc.mapfile_for(&iso);
    let mut mf = mapfile::Mapfile::create(&mapfile_path, full, "vTEST").unwrap();
    mf.record(0, full, mapfile::SectorStatus::Finished).unwrap();
    for &lba in lbas {
        mf.record(lba as u64 * 2048, 2048, mapfile::SectorStatus::NonTrimmed)
            .unwrap();
    }
    mf.flush().unwrap();
    std::fs::write(&iso, vec![0u8; full as usize]).unwrap();
    let opts = PatchOptions::for_patch_pass(false, None, None);
    patch(&disc, reader, &iso, &opts).expect("a dead bus ends the pass, it does not fail it")
}

// A bridge crash (status 0xFF) on the first read ends the whole pass wedged, with
// no further reads: the orchestrator spin-cycles the drive before the next pass.
#[test]
fn a_transport_failure_ends_the_patch_pass_after_one_read() {
    let mut reader = FixedErrReader {
        err: |lba| Error::DiscRead {
            sector: lba as u64,
            status: Some(libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE),
            sense: None,
        },
        reads: 0,
    };
    let out = patch_bad_sectors("transport", &[100, 200, 300], &mut reader);
    assert!(out.wedged_exit, "a dead bus must end the pass wedged");
    assert!(!out.halted);
    assert_eq!(reader.reads, 1, "no read after the transport failure");
    assert_eq!(out.bytes_recovered_this_pass, 0);
}

// The wedge streak is carried across ranges: 1-sector ranges each see only a
// few fast wedge senses, yet the pass aborts once 16 accumulate in total.
#[test]
fn the_wedge_streak_carries_across_ranges_to_abort_the_pass() {
    let mut reader = FixedErrReader {
        err: |lba| Error::DiscRead {
            sector: lba as u64,
            status: Some(libfreemkv::scsi::SCSI_STATUS_CHECK_CONDITION),
            sense: Some(libfreemkv::scsi::ScsiSense {
                sense_key: libfreemkv::scsi::SENSE_KEY_ILLEGAL_REQUEST,
                asc: 0x21,
                ascq: 0x00,
            }),
        },
        reads: 0,
    };
    let lbas: Vec<u32> = (1..=10).map(|i| i * 100).collect();
    let out = patch_bad_sectors("wedge", &lbas, &mut reader);
    assert!(out.wedged_exit, "a wedged drive must end the pass");
    // Tier 0 reads each 1-sector range 4 times (one per scout): the 16th wedge
    // sense lands in the 4th range. Without the carry nothing trips until tier 2.
    assert_eq!(reader.reads, 16);
}

// R7: once the consumer's write failed, the producer stops on "consumer gone"; the
// pass must fail with the write's own error (ENOSPC/EIO), not that.
#[test]
fn a_failed_consumer_write_is_the_error_the_patch_reports() {
    let enospc = || Error::IoError {
        source: std::io::Error::other("ENOSPC"),
    };
    let gone = || Err(super::super::SendStall::ConsumerGone.into_error());
    let e = settle(gone(), Err(enospc()), true).err().unwrap();
    assert!(matches!(e, Error::IoError { .. }), "got {e:?}");
    // The consumer healthy: the producer's own failure stands.
    let e = settle(Err(Error::DecryptFailed), Err(enospc()), false)
        .err()
        .unwrap();
    assert!(matches!(e, Error::DecryptFailed), "got {e:?}");
}

// An `apply` write error may be the latched writeback failure of an earlier span: the
// dropped sink must not flush those spans' Finished records.
#[test]
fn a_failed_write_does_not_persist_earlier_finished_records() {
    retry_inside_flush_window(a_failed_write_does_not_persist_earlier_finished_records_case);
}

// One attempt; `false` when the periodic persist ran first (inconclusive).
fn a_failed_write_does_not_persist_earlier_finished_records_case() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("out.iso");
    std::fs::write(&iso, vec![0u8; 8192]).unwrap();
    let mapfile_path = dir.path().join("out.iso.mapfile");
    let mut mf = Mapfile::create(&mapfile_path, 8192, "vTEST").unwrap();
    mf.record(0, 8192, SectorStatus::NonTrimmed).unwrap();
    mf.flush().unwrap();
    let (mut sink, _shared) = PatchSink::new(&iso, mf, true, None).unwrap();
    let read_only = std::fs::File::open(&iso).unwrap();
    sink.file = libfreemkv::io::WritebackFile::new(read_only).unwrap();
    sink.map.set_disc_hash(&"ab".repeat(20));
    sink.map.flush().unwrap();
    let fresh = std::time::Instant::now();
    sink.map.record(0, 2048, SectorStatus::Finished).unwrap();
    let r = sink.apply(PatchItem::Recovered {
        pos: 2048,
        buf: vec![7u8; 2048],
    });
    if fresh.elapsed() >= std::time::Duration::from_millis(900) {
        return false; // inconclusive
    }
    assert!(r.is_err(), "a write to a read-only handle must fail");
    drop(sink);
    let reloaded = Mapfile::load(&mapfile_path).unwrap();
    assert!(
        reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
        "a failed write left earlier, possibly lost, spans persisted Finished"
    );
    true
}

// A timing-guarded case retries rather than passing silently on a slow runner.
fn retry_inside_flush_window(case: fn() -> bool) {
    assert!(
        (0..5).any(|_| case()),
        "no attempt stayed inside the mapfile's flush window"
    );
}

// R3: a failed `sync_all` means the recovered data is not durable, so the dropped
// sink must not flush a Finished record for it.
#[test]
fn sync_failure_on_close_does_not_persist_finished() {
    retry_inside_flush_window(sync_failure_on_close_does_not_persist_finished_case);
}

// One attempt; `false` when the periodic persist ran first (inconclusive).
fn sync_failure_on_close_does_not_persist_finished_case() -> bool {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("out.iso");
    std::fs::write(&iso, vec![0u8; 4096]).unwrap();
    let mapfile_path = dir.path().join("out.iso.mapfile");
    let mut mf = Mapfile::create(&mapfile_path, 4096, "vTEST").unwrap();
    mf.record(0, 4096, SectorStatus::NonTrimmed).unwrap();
    mf.flush().unwrap();

    let (mut sink, _shared) = PatchSink::new(&iso, mf, true, None).unwrap();
    sink.map.set_disc_hash(&"ab".repeat(20));
    sink.map.flush().unwrap();
    let fresh = std::time::Instant::now();
    sink.apply(PatchItem::Recovered {
        pos: 0,
        buf: vec![7u8; 2048],
    })
    .unwrap();
    if fresh.elapsed() >= std::time::Duration::from_millis(900) {
        return false; // the 1 s periodic persist already ran: inconclusive
    }
    let halt = libfreemkv::halt::Halt::new();
    halt.cancel();
    sink.file.set_halt(halt);
    assert!(sink.close().is_err(), "sync_all must fail under a halt");

    let reloaded = Mapfile::load(&mapfile_path).unwrap();
    assert!(
        reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
        "a non-durable sector must not be recorded Finished"
    );
    true
}
