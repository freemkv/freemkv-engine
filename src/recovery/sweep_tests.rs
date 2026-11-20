use super::*;
use crate::recovery::test_images::{MemImage, assert_crash_safe};
use std::io::Read;

/// Build a `SweepSink` over a scratch ISO pre-filled with `0xAA`, plus a
/// fresh mapfile. No drive, no producer thread — the consumer is driven
/// by hand.
fn sink_over(dir: &std::path::Path, total: u64) -> (SweepSink, std::path::PathBuf) {
    let iso = dir.join("out.iso");
    std::fs::write(&iso, vec![0xAAu8; total as usize]).unwrap();
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&iso)
        .unwrap();
    let wf = libfreemkv::io::WritebackFile::new(f).unwrap();
    let map = Mapfile::create(&dir.join("out.map"), total, "test").unwrap();
    let (sink, _rx) = SweepSink::new(wf, map, true);
    (sink, iso)
}

// `/dev/null`'s fsync genuinely fails at the OS level (macOS
// ENODEV, Linux EINVAL); `is_regular` is supplied by the caller
// so both arms of `close`'s policy can be driven over it.
#[cfg(unix)]
fn sink_over_dev_null(dir: &std::path::Path, is_regular: bool) -> SweepSink {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap();
    let wf = libfreemkv::io::WritebackFile::new(f).unwrap();
    let map = Mapfile::create(&dir.join("out.map"), 8192, "test").unwrap();
    let (sink, _rx) = SweepSink::new(wf, map, is_regular);
    sink
}

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fmkv-sweepsink-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// The zero-fill loop must write EXACTLY the skipped range — under-fill leaves stale bytes
// the mapfile now claims are NonTrimmed, over-fill clobbers good data past the gap.
#[test]
fn a_skip_fill_writes_exactly_the_gap_and_records_exactly_the_gap() {
    let dir = scratch("skipfill");
    let gap_start = 4096u64;
    // Two whole 64 KB chunks plus a short final one, so the chunking runs
    // three times with a partial tail. Sector-aligned since the PRODUCER
    // snaps every span via `snap_to_sectors` before reaching this sink.
    let len = ZERO_CHUNK as u64 * 2 + 3 * 2048;
    // Trailing slack wider than one chunk, so an overshooting fill has
    // somewhere visible to overshoot INTO.
    let total = gap_start + len + 2 * ZERO_CHUNK as u64;
    let (mut sink, iso) = sink_over(&dir, total);

    sink.apply(WorkItem::SkipFill {
        pos: gap_start,
        len,
    })
    .unwrap();
    let summary = sink.close().unwrap();

    let mut got = Vec::new();
    std::fs::File::open(&iso)
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(
        got.len() as u64,
        total,
        "the file must not have been resized"
    );
    assert!(
        got[..gap_start as usize].iter().all(|&b| b == 0xAA),
        "bytes before the gap were rewritten"
    );
    assert!(
        got[gap_start as usize..(gap_start + len) as usize]
            .iter()
            .all(|&b| b == 0),
        "the gap the mapfile is about to call NonTrimmed was not actually zeroed"
    );
    assert!(
        got[(gap_start + len) as usize..].iter().all(|&b| b == 0xAA),
        "the fill ran past the end of the gap and clobbered good data"
    );

    assert_eq!(summary.stats.bytes_good, 0);
    let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
    assert_eq!(
        reloaded.ranges_with(&[SectorStatus::NonTrimmed]),
        vec![(gap_start, len)],
        "exactly the gap is recorded NonTrimmed — no more, no less"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A gap smaller than one chunk is still written in full.
#[test]
fn a_sub_chunk_gap_fill_writes_its_whole_length() {
    let dir = scratch("gapfill");
    let len = 3072u64;
    let total = 8192u64;
    let (mut sink, iso) = sink_over(&dir, total);

    sink.apply(WorkItem::GapFill { pos: 0, len }).unwrap();
    let summary = sink.close().unwrap();

    let got = std::fs::read(&iso).unwrap();
    assert!(got[..len as usize].iter().all(|&b| b == 0));
    assert!(
        got[len as usize..].iter().all(|&b| b == 0xAA),
        "a fill shorter than one chunk still stopped at len"
    );
    assert_eq!(summary.stats.bytes_good, 0);
    let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
    assert_eq!(
        reloaded.ranges_with(&[SectorStatus::NonTrimmed]),
        vec![(0, len)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A good batch lands at its own offset and is recorded Finished there.
#[test]
fn a_good_batch_is_written_at_its_position_and_recorded_finished() {
    let dir = scratch("good");
    let total = 8192u64;
    let (mut sink, iso) = sink_over(&dir, total);

    sink.apply(WorkItem::Good {
        pos: 2048,
        buf: vec![0x5Au8; 2048],
    })
    .unwrap();
    let summary = sink.close().unwrap();

    let got = std::fs::read(&iso).unwrap();
    assert!(got[..2048].iter().all(|&b| b == 0xAA));
    assert!(got[2048..4096].iter().all(|&b| b == 0x5A));
    assert!(got[4096..].iter().all(|&b| b == 0xAA));
    assert_eq!(summary.stats.bytes_good, 2048);
    let _ = std::fs::remove_dir_all(&dir);
}

// `close` must surface a failed `sync_all` when the output is a regular file — the last
// barrier before `copy` reports a finished rip.
#[cfg(unix)]
#[test]
fn a_failed_sync_all_is_an_error_when_the_output_is_regular() {
    let dir = scratch("syncfail-regular");
    let mut sink = sink_over_dev_null(&dir, true);
    sink.apply(WorkItem::Good {
        pos: 0,
        buf: vec![0x5Au8; 2048],
    })
    .unwrap();

    let err = sink
        .close()
        .err()
        .expect("a failed fsync on a regular output must be reported, not swallowed");
    assert!(
        matches!(err, Error::IoError { .. }),
        "the underlying io error must be surfaced as-is, got {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ...and must NOT surface it when the output is not a regular file (/dev/null and pipes
// always fail `sync_all`).
#[cfg(unix)]
#[test]
fn a_failed_sync_all_is_exempt_when_the_output_is_not_regular() {
    let dir = scratch("syncfail-devnull");
    let mut sink = sink_over_dev_null(&dir, false);
    sink.apply(WorkItem::Good {
        pos: 0,
        buf: vec![0x5Au8; 2048],
    })
    .unwrap();

    let summary = sink
        .close()
        .expect("a /dev/null output always fails fsync; that is not a rip failure");
    assert_eq!(summary.stats.bytes_good, 2048);
    let reloaded =
        Mapfile::load(&dir.join("out.map")).expect("the exempt path must still flush the mapfile");
    assert_eq!(
        reloaded.ranges_with(&[SectorStatus::Finished]),
        vec![(0, 2048)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// A typed failure from the writeback flusher (SyncTimeout, Halted) must reach the
// caller typed, not re-wrapped as IoError (read downstream as a dead USB bridge).
#[test]
fn a_typed_sync_failure_surfaces_typed_not_as_io_error() {
    let dir = scratch("synctyped");
    let (mut sink, _iso) = sink_over(&dir, 8192);
    let halt = libfreemkv::halt::Halt::new();
    halt.cancel();
    sink.file.set_halt(halt);
    let err = sink.close().err().expect("a halted fsync must fail");
    assert!(matches!(err, Error::Halted), "got {err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// R3: a failed fsync means the bytes behind this close's Finished records are not
// durable, so dropping the sink must not flush those records.
#[test]
fn a_failed_close_does_not_persist_finished_for_unsynced_data() {
    for _ in 0..ATTEMPTS {
        let dir = scratch("syncfail-drop");
        let (mut sink, _iso) = sink_over(&dir, 8192);
        let fresh = freshly_flushed(&mut sink.map);
        sink.apply(WorkItem::Good {
            pos: 0,
            buf: vec![0x5Au8; 2048],
        })
        .unwrap();
        if fresh.elapsed() >= FLUSH_WINDOW {
            continue; // the periodic persist already ran: try again
        }
        failed_close_case(sink, &dir);
        return;
    }
    panic!("no attempt stayed inside the mapfile's flush window");
}

fn failed_close_case(mut sink: SweepSink, dir: &std::path::Path) {
    let halt = libfreemkv::halt::Halt::new();
    halt.cancel();
    sink.file.set_halt(halt);
    assert!(sink.close().is_err(), "a halted fsync must fail");
    let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
    assert!(
        reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
        "a range whose data never reached disk was persisted Finished"
    );
    let _ = std::fs::remove_dir_all(dir);
}

// A timing-guarded case retries rather than passing silently on a slow runner.
const ATTEMPTS: usize = 5;

// R2: once teardown abandons this consumer (mapfile disowned), a resumed pass owns the
// image; an in-flight zero-fill must not go on writing zeros over what it recovers.
#[test]
fn a_disowned_sink_writes_nothing_more_to_the_image() {
    let dir = scratch("disowned");
    let total = 4 * ZERO_CHUNK as u64;
    let (mut sink, iso) = sink_over(&dir, total);
    sink.map.disown_handle().disown();
    let _ = sink.apply(WorkItem::GapFill { pos: 0, len: total });
    let _ = sink.apply(WorkItem::Good {
        pos: 0,
        buf: vec![0x5Au8; 2048],
    });
    drop(sink);
    let got = std::fs::read(&iso).unwrap();
    assert!(
        got.iter().all(|&b| b == 0xAA),
        "an abandoned consumer overwrote the image a resumed pass now owns"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Under the mapfile's 1 s persist interval: a record made sooner stays in memory only.
const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(900);

// Persist `map` now, so the next record lands a full interval before the next persist.
fn freshly_flushed(map: &mut Mapfile) -> std::time::Instant {
    map.record(0, 2048, SectorStatus::NonTried).unwrap();
    map.set_disc_hash(&"ab".repeat(20));
    map.flush().unwrap();
    std::time::Instant::now()
}

// An `apply` write error (often the latched writeback failure of an EARLIER item) means
// the Finished records before it may not be on disk: the dropped sink must not flush them.
#[test]
fn a_failed_write_does_not_persist_earlier_finished_records() {
    for _ in 0..ATTEMPTS {
        let dir = scratch("writefail-drop");
        let (mut sink, iso) = sink_over(&dir, 8192);
        let read_only = std::fs::File::open(&iso).unwrap();
        sink.file = libfreemkv::io::WritebackFile::new(read_only).unwrap();
        let fresh = freshly_flushed(&mut sink.map);
        sink.map.record(0, 2048, SectorStatus::Finished).unwrap();
        let r = sink.apply(WorkItem::Good {
            pos: 2048,
            buf: vec![0x5Au8; 2048],
        });
        if fresh.elapsed() >= FLUSH_WINDOW {
            continue; // try again, as above
        }
        assert!(r.is_err(), "a write to a read-only handle must fail");
        drop(sink);
        let reloaded = Mapfile::load(&dir.join("out.map")).unwrap();
        assert!(
            reloaded.ranges_with(&[SectorStatus::Finished]).is_empty(),
            "a failed write left earlier, possibly lost, ranges persisted Finished"
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    panic!("no attempt stayed inside the mapfile's flush window");
}

// R2's in-loop check: a fill already under way stops when teardown disowns the map.
#[test]
fn a_fill_stops_when_disowned_mid_way() {
    const FILL: u64 = 256 << 20;
    let dir = scratch("disowned-mid");
    let (mut sink, iso) = sink_over(&dir, 8192);
    let disown = sink.map.disown_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1));
        disown.disown();
    });
    let _ = sink.apply(WorkItem::GapFill { pos: 0, len: FILL });
    t.join().unwrap();
    drop(sink);
    let len = std::fs::metadata(&iso).unwrap().len();
    assert!(len < FILL, "the fill ran on past the disown ({len} bytes)");
    let _ = std::fs::remove_dir_all(&dir);
}

// Durability over an in-memory image, whose `durable` copy is what a crash would keep.
const B: u64 = 4096;

fn mem_sink(dir: &std::path::Path, total: u64) -> (SweepSink<MemImage>, MemImage) {
    let img = MemImage::filled(total as usize, 0xAA);
    let map = Mapfile::create(&dir.join("out.map"), total, "test").unwrap();
    let (sink, _rx) = SweepSink::new(img.clone(), map, true);
    (sink, img)
}

fn batch(i: u64) -> Vec<u8> {
    vec![(i as u8).wrapping_add(1); B as usize]
}

#[test]
fn the_persist_interval_is_at_least_ten_seconds() {
    assert!(crate::recovery::IMAGE_PERSIST_INTERVAL >= std::time::Duration::from_secs(10));
}

// A crash after any item, at any persist cadence, finds no Finished range whose bytes are
// not durable; after close the whole image is durable and fully recorded.
#[test]
fn a_crash_after_any_item_finds_no_finished_range_without_its_data() {
    let hour = std::time::Duration::from_secs(3600);
    for (n, every) in [std::time::Duration::ZERO, hour].into_iter().enumerate() {
        let dir = scratch(&format!("crash-any-{n}"));
        let map_path = dir.join("out.map");
        let total = 16 * B;
        let (mut sink, img) = mem_sink(&dir, total);
        sink.persist_every = every;
        let mut expected = vec![0xAAu8; total as usize];
        for i in 0..16 {
            let (pos, at) = (i * B, (i * B) as usize..((i + 1) * B) as usize);
            if i % 5 == 4 {
                sink.apply(WorkItem::SkipFill { pos, len: B }).unwrap();
                expected[at].fill(0);
            } else {
                sink.apply(WorkItem::Good { pos, buf: batch(i) }).unwrap();
                expected[at].copy_from_slice(&batch(i));
            }
            assert_crash_safe(&map_path, &img, &expected);
        }
        sink.close().unwrap();
        assert_crash_safe(&map_path, &img, &expected);
        assert!(img.state().durable == expected, "close left data unsynced");
        let on_disk = Mapfile::load(&map_path).unwrap();
        assert_eq!(on_disk.stats().bytes_good, 13 * B);
        assert_eq!(on_disk.stats().bytes_nontried, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// The fill hole: a fill's record can be the one that persists the map, and it must sync the
// Good data recorded before it, or that data is persisted Finished while still volatile.
#[test]
fn a_fill_that_persists_the_map_syncs_the_good_data_before_it() {
    let dir = scratch("fill-persists");
    let map_path = dir.join("out.map");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    sink.apply(WorkItem::Good {
        pos: 0,
        buf: batch(0),
    })
    .unwrap();
    assert_eq!(
        img.state().syncs,
        0,
        "inside the interval nothing is synced"
    );
    sink.persist_every = std::time::Duration::ZERO;
    sink.apply(WorkItem::SkipFill { pos: B, len: B }).unwrap();
    assert_eq!(img.state().syncs, 1, "the persisting fill synced first");
    let on_disk = Mapfile::load(&map_path).unwrap();
    assert_eq!(on_disk.ranges_with(&[SectorStatus::Finished]), vec![(0, B)]);
    let mut expected = vec![0xAAu8; 4 * B as usize];
    expected[..B as usize].copy_from_slice(&batch(0));
    expected[B as usize..2 * B as usize].fill(0);
    assert_crash_safe(&map_path, &img, &expected);
    let _ = std::fs::remove_dir_all(&dir);
}

// Inside one interval a stream of batches costs no image sync and no map persist; close
// does both exactly once.
#[test]
fn the_consumer_syncs_and_persists_at_most_once_per_interval() {
    let dir = scratch("cadence");
    let n = 1000;
    let (mut sink, img) = mem_sink(&dir, n * B);
    let t0 = std::time::Instant::now();
    for i in 0..n {
        let buf = batch(i);
        sink.apply(WorkItem::Good { pos: i * B, buf }).unwrap();
    }
    if t0.elapsed() < crate::recovery::IMAGE_PERSIST_INTERVAL {
        assert_eq!(img.state().syncs, 0);
        assert_eq!(sink.map.persists, 0);
    }
    sink.close().unwrap();
    assert_eq!(img.state().syncs, 1);
    let on_disk = Mapfile::load(&dir.join("out.map")).unwrap();
    assert_eq!(on_disk.stats().bytes_good, n * B);
    let _ = std::fs::remove_dir_all(&dir);
}

// A write that fails part-way disowns the map: neither it nor the earlier, unsynced batch
// ever reaches the map on disk as Finished.
#[test]
fn a_partial_write_leaves_no_unsynced_range_finished() {
    let dir = scratch("partial-write");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    sink.apply(WorkItem::Good {
        pos: 0,
        buf: batch(0),
    })
    .unwrap();
    img.state().fail_after = Some(1000);
    let r = sink.apply(WorkItem::Good {
        pos: B,
        buf: batch(1),
    });
    assert!(r.is_err(), "the partial write must fail the item");
    assert!(sink.map.is_disowned());
    drop(sink);
    let on_disk = Mapfile::load(&dir.join("out.map")).unwrap();
    assert!(on_disk.ranges_with(&[SectorStatus::Finished]).is_empty());
    assert_eq!(on_disk.stats().bytes_nontried, 4 * B);
    let _ = std::fs::remove_dir_all(&dir);
}

// A sync that fails at a persist keeps the map off disk, now and when the sink drops.
#[test]
fn a_failed_sync_at_a_persist_keeps_the_map_off_disk() {
    let dir = scratch("sync-fails-at-persist");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    sink.persist_every = std::time::Duration::ZERO;
    img.state().fail_sync = true;
    let r = sink.apply(WorkItem::Good {
        pos: 0,
        buf: batch(0),
    });
    assert!(r.is_err(), "a failed sync must fail the item");
    drop(sink);
    let on_disk = Mapfile::load(&dir.join("out.map")).unwrap();
    assert!(on_disk.ranges_with(&[SectorStatus::Finished]).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// Benches: run with `--release -- --ignored bench_`. They print figures and never assert.
const BENCH_BATCH: u64 = 64 * 1024;

// Pushes `batches` 64 KiB `Good` items through a sink over a `NullImage` whose syncs take
// `sync_cost`, paced at `rate` bytes/s (0: flat out). Returns (MB/s, syncs, map persists).
fn drive_sweep_sink(
    dir: &std::path::Path,
    batches: u64,
    rate: u64,
    sync_cost: std::time::Duration,
) -> (f64, usize, usize) {
    use crate::recovery::test_images::NullImage;
    let total = batches * BENCH_BATCH;
    let img = NullImage {
        sync_cost,
        ..Default::default()
    };
    let map = Mapfile::create(&dir.join("bench.map"), total, "bench").unwrap();
    let (mut sink, _rx) = SweepSink::new(img.clone(), map, true);
    let pool = crate::recovery::BufPool::new(4, BENCH_BATCH as usize);
    sink.recycle_into(pool.returner());
    let t0 = std::time::Instant::now();
    for i in 0..batches {
        pace(t0, i * BENCH_BATCH, rate);
        let buf = pool.take();
        let pos = i * BENCH_BATCH;
        sink.apply(WorkItem::Good { pos, buf }).unwrap();
    }
    let persists = sink.map.persists;
    let secs = t0.elapsed().as_secs_f64();
    sink.close().unwrap();
    (total as f64 / secs / 1e6, img.counts().1, persists)
}

// Sleeps until `done` bytes are due at `rate` bytes/s since `t0` (a drive's steady pace).
fn pace(t0: std::time::Instant, done: u64, rate: u64) {
    if rate == 0 {
        return;
    }
    let due = std::time::Duration::from_secs_f64(done as f64 / rate as f64);
    if let Some(wait) = due.checked_sub(t0.elapsed()) {
        std::thread::sleep(wait);
    }
}

#[test]
#[ignore = "bench"]
fn bench_sweep_sink_throughput() {
    let dir = scratch("bench-throughput");
    let (mbs, syncs, persists) = drive_sweep_sink(&dir, 8192, 0, std::time::Duration::ZERO);
    eprintln!("bench_sweep_sink_throughput: {mbs:.0} MB/s, {syncs} syncs, {persists} persists");
    let _ = std::fs::remove_dir_all(&dir);
}

// 12 s at a BD drive's 50 MB/s against a 20 ms fsync: how often the consumer syncs and persists.
#[test]
#[ignore = "bench"]
fn bench_sweep_sink_flushes_at_drive_speed() {
    let dir = scratch("bench-flushes");
    let rate = 50_000_000;
    let batches = 12 * rate / BENCH_BATCH;
    let cost = std::time::Duration::from_millis(20);
    let (mbs, syncs, persists) = drive_sweep_sink(&dir, batches, rate, cost);
    eprintln!(
        "bench_sweep_sink_flushes_at_drive_speed: {mbs:.1} MB/s, {syncs} syncs, {persists} persists in 12 s"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// A paced producer feeds the real pipeline while every image sync (one a second here)
// takes 100 ms: the time it spends blocked in `send` is time the drive would sit idle.
#[test]
#[ignore = "bench"]
fn bench_sweep_pipeline_producer_stall() {
    use crate::recovery::test_images::NullImage;
    let dir = scratch("bench-stall");
    let rate = 50_000_000u64;
    let batches = 12 * rate / BENCH_BATCH;
    let old = libfreemkv::io::DEFAULT_PIPELINE_DEPTH;
    for depth in [
        old,
        crate::recovery::sweep_queue_depth(BENCH_BATCH as usize),
    ] {
        let img = NullImage {
            sync_cost: std::time::Duration::from_millis(100),
            ..Default::default()
        };
        let total = batches * BENCH_BATCH;
        let map = Mapfile::create(&dir.join("bench.map"), total, "bench").unwrap();
        let (mut sink, _rx) = SweepSink::new(img.clone(), map, true);
        sink.persist_every = std::time::Duration::from_secs(1);
        let pipe = libfreemkv::io::Pipeline::spawn_named("bench-sweep", depth, sink).unwrap();
        let t0 = std::time::Instant::now();
        let mut blocked = std::time::Duration::ZERO;
        for i in 0..batches {
            pace(t0, i * BENCH_BATCH, rate);
            let buf = vec![0x5Au8; BENCH_BATCH as usize];
            let sent = std::time::Instant::now();
            let pos = i * BENCH_BATCH;
            assert!(pipe.send(WorkItem::Good { pos, buf }).is_ok());
            blocked += sent.elapsed();
        }
        let took = t0.elapsed();
        pipe.finish().unwrap();
        let syncs = img.counts().1;
        eprintln!(
            "bench_sweep_pipeline_producer_stall: depth {depth}, blocked {blocked:?} of {took:?}, {syncs} syncs"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// About 8 MiB of batches at the multipass batch size, bounded at both ends.
#[test]
fn the_sweep_queue_holds_about_eight_mib() {
    use crate::recovery::sweep_queue_depth;
    let floor = libfreemkv::io::DEFAULT_PIPELINE_DEPTH;
    assert_eq!(sweep_queue_depth(64 * 1024), 128);
    assert_eq!(sweep_queue_depth(120 * 1024), (8 << 20) / (120 * 1024));
    assert_eq!(
        sweep_queue_depth(2048),
        128,
        "small batches cap at 128 items"
    );
    assert_eq!(
        sweep_queue_depth(64 << 20),
        floor,
        "huge batches keep the floor"
    );
    assert_eq!(sweep_queue_depth(0), 128);
}

// A written Good buffer goes back to the producer's pool and is the next one it reads into.
#[test]
fn a_written_batch_buffer_comes_back_for_the_next_read() {
    use crate::recovery::BufPool;
    let dir = scratch("recycle");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    let pool = BufPool::new(2, B as usize);
    sink.recycle_into(pool.returner());
    let buf = pool.take();
    let ptr = buf.as_ptr();
    sink.apply(WorkItem::Good { pos: 0, buf }).unwrap();
    let back = pool.take();
    assert_eq!(back.as_ptr(), ptr, "the same allocation came back");
    assert_eq!(back.len(), B as usize);
    sink.close().unwrap();
    assert!(img.state().durable[..B as usize].iter().all(|&b| b == 0));
    let _ = std::fs::remove_dir_all(&dir);
}

// The pool hands out `size` bytes (new or recycled, any length back), copies exactly, and
// keeps at most `cap` spares: extra returns are dropped, never queued.
#[test]
fn the_buffer_pool_sizes_copies_and_bounds_its_spares() {
    use crate::recovery::BufPool;
    let pool = BufPool::new(1, 8);
    assert_eq!(pool.take(), vec![0u8; 8]);
    let back = pool.returner();
    back.give(vec![7u8; 3]);
    back.give(vec![9u8; 3]);
    let reused = pool.take();
    assert_eq!(reused.len(), 8);
    assert_eq!(&reused[..3], &[7, 7, 7], "the first return was kept");
    assert_eq!(
        pool.take(),
        vec![0u8; 8],
        "the second was dropped, not queued"
    );
    back.give(vec![1u8; 64]);
    assert_eq!(pool.copy_of(&[4, 5]), vec![4, 5]);
    crate::recovery::BufReturn::default().give(vec![1]);
}

// On a fresh image a fill over still-NonTried bytes only records: any write would fail here.
#[test]
fn a_fill_on_a_fresh_image_records_without_writing() {
    let dir = scratch("fresh-fill");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    sink.image_is_fresh();
    img.state().fail_after = Some(0);
    sink.apply(WorkItem::GapFill { pos: B, len: 2 * B })
        .unwrap();
    img.state().fail_after = None;
    sink.close().unwrap();
    let on_disk = Mapfile::load(&dir.join("out.map")).unwrap();
    assert_eq!(
        on_disk.ranges_with(&[SectorStatus::NonTrimmed]),
        vec![(B, 2 * B)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// A fill over bytes this sink already wrote still writes zeros, fresh image or not; only
// the untouched NonTried part is left as it is.
#[test]
fn a_fill_over_written_bytes_still_writes_zeros_on_a_fresh_image() {
    let dir = scratch("fresh-written");
    let (mut sink, img) = mem_sink(&dir, 4 * B);
    sink.image_is_fresh();
    sink.apply(WorkItem::Good {
        pos: 0,
        buf: batch(0),
    })
    .unwrap();
    sink.apply(WorkItem::SkipFill { pos: 0, len: 2 * B })
        .unwrap();
    sink.apply(WorkItem::SkipFill { pos: 2 * B, len: B })
        .unwrap();
    sink.close().unwrap();
    let durable = img.state().durable.clone();
    let at = |r: std::ops::Range<u64>| durable[r.start as usize..r.end as usize].to_vec();
    assert!(at(0..2 * B).iter().all(|&b| b == 0), "written bytes zeroed");
    // A real fresh image reads zero there; this mock's 0xAA shows no write happened.
    assert!(at(2 * B..3 * B).iter().all(|&b| b == 0xAA));
    let _ = std::fs::remove_dir_all(&dir);
}

// The premise, on the real filesystem: an image truncated and sized by `set_len` reads zero
// wherever a fresh-image fill skipped its write, so the image is byte-identical to before.
#[test]
fn a_fresh_sized_image_reads_zero_where_a_fill_skipped_its_write() {
    let dir = scratch("fresh-real");
    let iso = dir.join("out.iso");
    std::fs::write(&iso, vec![0xAAu8; 64]).unwrap();
    let total = 3 * ZERO_CHUNK as u64;
    let f = std::fs::File::create(&iso).unwrap();
    f.set_len(total).unwrap();
    let wf = libfreemkv::io::WritebackFile::new(f).unwrap();
    let map = Mapfile::create(&dir.join("out.map"), total, "test").unwrap();
    let (mut sink, _rx) = SweepSink::new(wf, map, true);
    sink.image_is_fresh();
    let good = vec![0x5Au8; 2048];
    sink.apply(WorkItem::Good { pos: 0, buf: good }).unwrap();
    sink.apply(WorkItem::GapFill {
        pos: 2048,
        len: total - 2048,
    })
    .unwrap();
    sink.close().unwrap();
    let got = std::fs::read(&iso).unwrap();
    assert_eq!(got.len() as u64, total);
    assert!(got[..2048].iter().all(|&b| b == 0x5A));
    assert!(
        got[2048..].iter().all(|&b| b == 0),
        "a skipped fill must read zero"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// A stats reply carries the damage ranges only when they changed since the last delivered;
// a reply that could not be delivered does not count as delivered.
#[test]
fn stats_replies_carry_damage_only_when_it_changed() {
    let dir = scratch("stats-epoch");
    let img = MemImage::filled(8 * B as usize, 0);
    let map = Mapfile::create(&dir.join("out.map"), 8 * B, "test").unwrap();
    let (mut sink, rx) = SweepSink::new(img, map, true);
    let ask = |sink: &mut SweepSink<MemImage>| {
        sink.apply(WorkItem::StatsRequest).unwrap();
        try_recv_progress(&rx).map(|s| s.bad_ranges)
    };
    assert_eq!(
        ask(&mut sink),
        Some(Some(vec![])),
        "the first reply has the set"
    );
    assert_eq!(ask(&mut sink), Some(None), "unchanged");
    let good = batch(0);
    sink.apply(WorkItem::Good { pos: 0, buf: good }).unwrap();
    assert_eq!(ask(&mut sink), Some(None), "good data is not damage");
    sink.apply(WorkItem::SkipFill { pos: B, len: B }).unwrap();
    // The channel holds one reply: a second request before a drain is dropped.
    sink.apply(WorkItem::StatsRequest).unwrap();
    sink.apply(WorkItem::StatsRequest).unwrap();
    let snap = try_recv_progress(&rx).unwrap();
    assert_eq!(snap.bad_ranges, Some(vec![(B, B)]));
    assert_eq!(ask(&mut sink), Some(None));
    sink.apply(WorkItem::SkipFill { pos: 2 * B, len: B })
        .unwrap();
    assert_eq!(ask(&mut sink), Some(Some(vec![(B, 2 * B)])));
    let _ = std::fs::remove_dir_all(&dir);
}

// Drained together, a newer reply without ranges keeps the older reply's ranges.
#[test]
fn a_drain_keeps_the_newest_ranges_it_saw() {
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let stats = crate::recovery::mapfile::MapStats::default();
    let ranges = Some(vec![(1, 2)]);
    let first = ProgressSnapshot {
        stats,
        bad_ranges: ranges.clone(),
    };
    tx.send(first).unwrap();
    tx.send(ProgressSnapshot {
        stats,
        bad_ranges: None,
    })
    .unwrap();
    assert_eq!(try_recv_progress(&rx).unwrap().bad_ranges, ranges);
    assert!(try_recv_progress(&rx).is_none());
}
