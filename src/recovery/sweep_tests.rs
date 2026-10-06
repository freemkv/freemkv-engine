use super::*;
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
