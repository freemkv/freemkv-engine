use super::*;

// Pins this crate's `mapfile_path_for` to libfreemkv's `Disc::mapfile_for` (duplicated by
// necessity — libfreemkv can't depend back on this crate).
#[test]
fn agrees_with_libfreemkv_disc_mapfile_for() {
    let disc = libfreemkv::Disc {
        volume_id: "TEST_VOL".into(),
        meta_title: Some("TEST_VOL".into()),
        format: libfreemkv::DiscFormat::Uhd,
        capacity_sectors: 1024,
        capacity_bytes: 1024 * 2048,
        layers: 1,
        titles: Vec::new(),
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    };
    for p in [
        "/tmp/movie.iso",
        "/staging/Some Disc (2024).iso",
        "relative.iso",
        "/tmp/no_extension",
        "/tmp/dots.in.name.iso",
    ] {
        let path = Path::new(p);
        assert_eq!(
            mapfile_path_for(path),
            disc.mapfile_for(path),
            "mapfile naming drifted from libfreemkv for {p}"
        );
    }
}

fn tmpfile(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "libfreemkv-mapfile-test-{}-{}-{}.mapfile",
        std::process::id(),
        tag,
        n
    );
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

// Three-pass patch-shaped workload; returns the entry count after each
// pass. Pass 2 is the worst case for `record()`'s coalescing: alternate
// sectors come back, so every recovered sector is its own bracketed run.
fn fragmenting_multipass(mf: &mut Mapfile) -> Vec<usize> {
    const SEC: u64 = 2048;
    let mut counts = Vec::new();
    // Pass 1 (sweep): the readable bulk lands Finished, three regions of
    // 64 sectors each fail as NonTrimmed.
    mf.record(0, 1000 * SEC, SectorStatus::Finished).unwrap();
    for base in [100u64, 400, 700] {
        mf.record(base * SEC, 64 * SEC, SectorStatus::NonTrimmed)
            .unwrap();
    }
    counts.push(mf.entries().len());
    // Pass 2 (scrape): every other sector inside each bad region comes
    // back; the rest stay NonTrimmed. Worst-case interleave.
    for base in [100u64, 400, 700] {
        for i in 0..64u64 {
            if i % 2 == 0 {
                mf.record((base + i) * SEC, SEC, SectorStatus::Finished)
                    .unwrap();
            }
        }
    }
    counts.push(mf.entries().len());
    // Pass 3: the remaining sectors come back too.
    for base in [100u64, 400, 700] {
        for i in 0..64u64 {
            if i % 2 == 1 {
                mf.record((base + i) * SEC, SEC, SectorStatus::Finished)
                    .unwrap();
            }
        }
    }
    counts.push(mf.entries().len());
    counts
}

// `record()` leaves `entries` as the CANONICAL maximal-run partition of `[0, total_size)`
// (contiguous, gapless, no two adjacent entries sharing a status).
fn assert_canonical(mf: &Mapfile) {
    let es = mf.entries();
    assert!(!es.is_empty());
    assert_eq!(es[0].pos, 0, "partition must start at 0");
    let mut expect_pos = 0u64;
    for (i, e) in es.iter().enumerate() {
        assert_eq!(e.pos, expect_pos, "gap or overlap before entry {i}");
        assert!(e.size > 0, "zero-size entry {i}");
        if i > 0 {
            assert_ne!(
                es[i - 1].status,
                e.status,
                "entries {} and {i} share a status and were not coalesced",
                i - 1
            );
        }
        expect_pos += e.size;
    }
    assert_eq!(
        expect_pos,
        mf.total_size(),
        "partition must cover the whole image"
    );
}

// R14: a record that does not change the run count (a sweep extending its Finished run
// into NonTried) edits `entries` in place instead of rebuilding the whole Vec.
#[test]
fn a_record_that_keeps_the_run_count_edits_in_place() {
    const SEC: u64 = 2048;
    let p = tmpfile("record_in_place");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 20_000 * SEC, "test").unwrap();
    // Runs `* + ?` per 4-sector group: `*` at 4i, `+` at 4i+1, `?` over 4i+2..4i+4.
    for i in 0..5_000u64 {
        mf.record(i * 4 * SEC, SEC, SectorStatus::NonTrimmed)
            .unwrap();
        mf.record((i * 4 + 1) * SEC, SEC, SectorStatus::Finished)
            .unwrap();
    }
    let (ptr, len) = (mf.entries().as_ptr(), mf.entries().len());
    for i in 0..5_000u64 {
        // Grows each `+` run by one sector into the `?` run after it.
        mf.record((i * 4 + 2) * SEC, SEC, SectorStatus::Finished)
            .unwrap();
        assert_eq!(mf.entries().as_ptr(), ptr, "record() rebuilt the entry Vec");
    }
    assert_eq!(mf.entries().len(), len);
    assert_canonical(&mf);
    let _ = std::fs::remove_file(&p);
}

// The pre-R14 `record()` (full rebuild, sort, global coalesce): the reference the
// localized splice must agree with.
fn reference_record(
    entries: &[MapEntry],
    pos: u64,
    size: u64,
    status: SectorStatus,
) -> Vec<MapEntry> {
    if size == 0 {
        return entries.to_vec();
    }
    let end = pos + size;
    let mut out = Vec::new();
    for e in entries.iter().cloned() {
        let e_end = e.pos + e.size;
        if e_end <= pos || e.pos >= end {
            out.push(e);
            continue;
        }
        if e.pos < pos {
            out.push(MapEntry {
                pos: e.pos,
                size: pos - e.pos,
                status: e.status,
            });
        }
        if e_end > end {
            out.push(MapEntry {
                pos: end,
                size: e_end - end,
                status: e.status,
            });
        }
    }
    out.push(MapEntry { pos, size, status });
    out.sort_by_key(|e| e.pos);
    let mut merged: Vec<MapEntry> = Vec::new();
    for e in out {
        if let Some(last) = merged.last_mut()
            && last.pos + last.size == e.pos
            && last.status == e.status
        {
            last.size += e.size;
            continue;
        }
        merged.push(e);
    }
    merged
}

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ALL_STATUSES: [SectorStatus; 5] = [
    SectorStatus::NonTried,
    SectorStatus::NonTrimmed,
    SectorStatus::NonScraped,
    SectorStatus::Unreadable,
    SectorStatus::Finished,
];

// R14 equivalence: random records (inside, straddling and past the end, leaving gaps;
// zero-size too) give exactly the old algorithm's entries, and the delta-maintained
// stats equal a full recount after every step.
#[test]
fn localized_record_matches_the_full_rebuild() {
    let p = tmpfile("record_equivalence");
    let _ = std::fs::remove_file(&p);
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for round in 0..200 {
        let total = 1 + rng.below(400);
        let mut mf = Mapfile::create(&p, total, "test").unwrap();
        let mut reference = mf.entries().to_vec();
        for step in 0..60 {
            let pos = rng.below(total + 40);
            let size = match rng.below(6) {
                0 => 0,
                1 => rng.below(total + 40) + 1,
                _ => rng.below(12) + 1,
            };
            let status = ALL_STATUSES[rng.below(5) as usize];
            mf.record(pos, size, status).unwrap();
            reference = reference_record(&reference, pos, size, status);
            assert_eq!(
                mf.entries(),
                reference.as_slice(),
                "round {round} step {step}: record({pos}, {size}, {status:?})"
            );
            assert_eq!(
                mf.stats(),
                Mapfile::compute_stats(&reference, total),
                "round {round} step {step}: stats drifted"
            );
        }
        mf.dirty = false;
    }
    let _ = std::fs::remove_file(&p);
}

// The `# freemkv-raw:` stamp round-trips through promote(), the writer and strict load().
#[test]
fn the_raw_stamp_survives_promote_and_a_strict_reload() {
    for raw in [false, true] {
        let p = tmpfile("raw_stamp_promote");
        let mut mf = Mapfile::create(&p, 8 * 2048, "test").unwrap();
        mf.set_raw(raw);
        mf.record(0, 2048, SectorStatus::NonTrimmed).unwrap();
        mf.record(2048, 2048, SectorStatus::Finished).unwrap();
        mf.promote(&[SectorStatus::NonTrimmed], SectorStatus::Unreadable)
            .unwrap();
        mf.flush().unwrap();
        let back = Mapfile::load(&p).map_err(|e| e.to_string());
        let _ = std::fs::remove_file(&p);
        let back = back.unwrap();
        assert_eq!(back.raw(), Some(raw));
        assert_eq!(back.entries(), mf.entries());
    }
}

// P6: `promote()` is the per-range `record()` loop in one pass: same entries, same stats.
#[test]
fn promote_matches_recording_each_range() {
    let p = tmpfile("promote_equivalence");
    let q = tmpfile("promote_equivalence_ref");
    let from = [SectorStatus::NonTrimmed, SectorStatus::NonScraped];
    let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
    for round in 0..100 {
        let total = 1 + rng.below(300);
        let mut mf = Mapfile::create(&p, total, "test").unwrap();
        for _ in 0..40 {
            let pos = rng.below(total);
            let size = (rng.below(10) + 1).min(total - pos);
            mf.record(pos, size, ALL_STATUSES[rng.below(5) as usize])
                .unwrap();
        }
        let to = ALL_STATUSES[rng.below(5) as usize];
        let mut looped = Mapfile::create(&q, total, "test").unwrap();
        looped.entries = mf.entries.clone();
        looped.stats = mf.stats;
        for (pos, size) in looped.ranges_with(&from) {
            looped.record(pos, size, to).unwrap();
        }
        mf.promote(&from, to).unwrap();
        assert_eq!(mf.entries(), looped.entries(), "round {round} to {to:?}");
        assert_eq!(mf.stats(), looped.stats(), "round {round} to {to:?}");
        assert_canonical(&mf);
    }
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&q);
}

// The `Mapfile.entries` bound, measured rather than asserted from a doc.
#[test]
fn fragmentation_peaks_then_collapses_as_damage_is_recovered() {
    let p = tmpfile("fragmentation_peaks_then_collapses");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000 * 2048, "test").unwrap();
    let counts = fragmenting_multipass(&mut mf);
    assert_canonical(&mf);
    let _ = std::fs::remove_file(&p);
    // Literals, not recomputed from the code under test: pass 1 alternates
    // +/*/+/*/+/*/+ = 7 runs; pass 2 is 3 regions x 64 alternating sectors with
    // the leading + merging into the bulk = 1 + 3*64 = 193; pass 3 collapses to 1.
    assert_eq!(counts, vec![7, 193, 1]);
    assert!(
        counts[2] < counts[1],
        "fragmentation must be reversible, not a ratchet: {counts:?}"
    );
}

/// A record that lands inside an existing run of the same status is free:
/// the partition is unchanged, so repeated passes over already-known
/// territory cannot fragment the list at all.
#[test]
fn repeat_records_inside_a_run_do_not_fragment() {
    let p = tmpfile("repeat_records_inside_a_run");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000 * 2048, "test").unwrap();
    mf.record(0, 1000 * 2048, SectorStatus::Finished).unwrap();
    mf.record(100 * 2048, 8 * 2048, SectorStatus::Unreadable)
        .unwrap();
    let before: Vec<MapEntry> = mf.entries().to_vec();
    assert_eq!(before.len(), 3);
    for _ in 0..500 {
        mf.record(100 * 2048, 8 * 2048, SectorStatus::Unreadable)
            .unwrap();
        mf.record(0, 100 * 2048, SectorStatus::Finished).unwrap();
    }
    assert_eq!(mf.entries(), before.as_slice());
    assert_canonical(&mf);
    let _ = std::fs::remove_file(&p);
}

// Pins the on-disk format against a literal mapfile written by an older
// release (v0.14.0) for damaged UHD media: it must load, its 19 entries
// must be understood exactly, and re-writing it must reproduce the bytes.
#[test]
fn real_shaped_mapfile_round_trips() {
    const ARCHIVED: &str = "\
# Rescue Logfile. Created by libfreemkv v0.14.0
# Current pos / status / pass / pass_time
0x000000000  ?  1  0
#      pos        size  status
0x000000000  0x34b630000    +
0x34b630000  0x000010000    *
0x34b640000  0x2b0d50000    +
0x5fc390000  0x007010000    *
0x6033a0000  0x000070000    +
0x603410000  0x008000000    *
0x60b410000  0x371030000    +
0x97c440000  0x000010000    *
0x97c450000  0x0001f0000    +
0x97c640000  0x000010000    *
0x97c650000  0x0000c0000    +
0x97c710000  0x001000000    *
0x97d710000  0x000090000    +
0x97d7a0000  0x002000000    *
0x97f7a0000  0x005480000    +
0x984c20000  0x003010000    *
0x987c30000  0x000080000    +
0x987cb0000  0x004000000    *
0x98bcb0000  0xa24550000    +
";
    let p = tmpfile("real_shaped_mapfile_round_trips");
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, ARCHIVED).unwrap();
    let mut mf = Mapfile::load(&p).unwrap();
    // Nineteen entries — the real fragmentation ceiling this format has
    // been observed to reach, and unchanged between that disc's two passes.
    assert_eq!(mf.entries().len(), 19);
    assert_eq!(mf.total_size(), 0x1_3B0_200_000);
    assert_eq!(mf.entries()[1].pos, 0x34b630000);
    assert_eq!(mf.entries()[1].size, 0x10000);
    assert_eq!(mf.entries()[1].status, SectorStatus::NonTrimmed);
    // Re-write it: same build, same bytes.
    mf.record(0, 0x34b630000, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    let rewritten = std::fs::read_to_string(&p).unwrap();
    assert_eq!(rewritten, ARCHIVED);
    // And it still loads to the same entries.
    let reloaded = Mapfile::load(&p).unwrap();
    assert_eq!(reloaded.entries(), mf.entries());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn create_has_one_nontried_region() {
    let p = tmpfile("create_has_one_nontried_region");
    let _ = std::fs::remove_file(&p);
    let mf = Mapfile::create(&p, 1000, "test").unwrap();
    assert_eq!(mf.entries().len(), 1);
    assert_eq!(mf.entries()[0].pos, 0);
    assert_eq!(mf.entries()[0].size, 1000);
    assert_eq!(mf.entries()[0].status, SectorStatus::NonTried);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn record_splits_overlap() {
    let p = tmpfile("record_splits_overlap");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(200, 100, SectorStatus::Finished).unwrap();
    let es = mf.entries();
    assert_eq!(es.len(), 3);
    assert_eq!(
        (es[0].pos, es[0].size, es[0].status),
        (0, 200, SectorStatus::NonTried)
    );
    assert_eq!(
        (es[1].pos, es[1].size, es[1].status),
        (200, 100, SectorStatus::Finished)
    );
    assert_eq!(
        (es[2].pos, es[2].size, es[2].status),
        (300, 700, SectorStatus::NonTried)
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn record_coalesces_adjacent_same_status() {
    let p = tmpfile("record_coalesces_adjacent_same_status");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 100, SectorStatus::Finished).unwrap();
    mf.record(200, 100, SectorStatus::Finished).unwrap();
    // Entries: [0..100 NonTried, 100..300 Finished (merged), 300..1000 NonTried]
    let es = mf.entries();
    assert_eq!(es.len(), 3);
    assert_eq!(
        (es[1].pos, es[1].size, es[1].status),
        (100, 200, SectorStatus::Finished)
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn record_replaces_existing_status() {
    let p = tmpfile("record_replaces_existing_status");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(200, 100, SectorStatus::Unreadable).unwrap();
    mf.record(200, 100, SectorStatus::Finished).unwrap();
    let es = mf.entries();
    // The overwrite should result in all finished at 200..300, NonTried elsewhere — 3 entries.
    assert_eq!(es.len(), 3);
    assert_eq!(es[1].status, SectorStatus::Finished);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn round_trip_load() {
    let p = tmpfile("round_trip_load");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 200, SectorStatus::Finished).unwrap();
    mf.record(500, 100, SectorStatus::Unreadable).unwrap();
    // record() batches; explicit flush before reading back from disk.
    mf.flush().unwrap();
    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.entries(), mf.entries());
    // The entry list is the one part of state written verbatim; total_size and
    // stats are supplied on create and re-derived on load, so a writer that
    // dropped the trailing extent could round-trip "correctly" on entries alone.
    assert_eq!(
        loaded.total_size(),
        1000,
        "the extent must survive a reload"
    );
    assert_eq!(loaded.total_size(), mf.total_size());
    assert_eq!(loaded.stats(), mf.stats(), "in-memory and reloaded stats");
    let st = loaded.stats();
    assert_eq!(st.bytes_total, 1000);
    assert_eq!(st.bytes_good, 200, "record(100, 200, Finished)");
    assert_eq!(st.bytes_unreadable, 100, "record(500, 100, Unreadable)");
    assert_eq!(st.bytes_pending, 700, "1000 - 200 - 100 still outstanding");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn write_to_disk_fsyncs_and_leaves_no_tmp() {
    // Regression: write_to_disk must recover the File and sync_all() it before
    // rename (NFS durability). The .tmp file must not survive a successful
    // write, and the renamed mapfile must load back identically.
    let p = tmpfile("write_to_disk_fsyncs");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 200, SectorStatus::Finished).unwrap();
    mf.write_to_disk().unwrap();

    let mut tmp = p.clone().into_os_string();
    tmp.push(".tmp");
    assert!(
        !PathBuf::from(&tmp).exists(),
        "tmp file should be renamed away after a successful write"
    );

    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.entries(), mf.entries());
    // Same reason as `round_trip_load`: the derived state has to survive
    // too, or a durable write of a truncated extent still passes.
    assert_eq!(loaded.total_size(), 1000);
    assert_eq!(loaded.stats(), mf.stats());
    assert_eq!(loaded.stats().bytes_good, 200);
    assert_eq!(loaded.stats().bytes_pending, 800);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn write_to_disk_fsyncs_parent_dir() {
    // Regression: after rename(2), write_to_disk must fsync the parent dir so
    // the new dirent is durable. Can't observe power loss in a unit test, but
    // exercise the fsync branch and confirm it neither errors nor corrupts.
    let dir = tmpfile("write_to_disk_fsyncs_parent_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("disc.mapfile");
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(0, 400, SectorStatus::Finished).unwrap();
    mf.record(400, 100, SectorStatus::Unreadable).unwrap();
    mf.write_to_disk().unwrap();

    // The directly-called dir fsync helper must be a no-op-on-error,
    // never a panic, even for a nonexistent directory.
    libfreemkv::io::fsync::dir(&dir.join("does-not-exist"));

    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.entries(), mf.entries());
    assert_eq!(loaded.total_size(), 1000);
    assert_eq!(loaded.stats(), mf.stats());
    assert_eq!(loaded.stats().bytes_good, 400);
    assert_eq!(loaded.stats().bytes_unreadable, 100);
    assert_eq!(loaded.stats().bytes_pending, 500);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stats_sum_correctly() {
    let p = tmpfile("stats_sum_correctly");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(0, 400, SectorStatus::Finished).unwrap();
    mf.record(400, 100, SectorStatus::Unreadable).unwrap();
    let s = mf.stats();
    assert_eq!(s.bytes_good, 400);
    assert_eq!(s.bytes_unreadable, 100);
    assert_eq!(s.bytes_pending, 500);
    assert_eq!(s.bytes_total, 1000);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn ranges_with_filters() {
    let p = tmpfile("ranges_with_filters");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 50, SectorStatus::Unreadable).unwrap();
    mf.record(300, 50, SectorStatus::Unreadable).unwrap();
    let bad = mf.ranges_with(&[SectorStatus::Unreadable]);
    assert_eq!(bad, vec![(100, 50), (300, 50)]);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn stats_consistent_after_overlapping_records() {
    let p = tmpfile("stats_consistent_after_overlapping");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    // Record some finished, some unreadable, some nontrimmed
    mf.record(0, 300, SectorStatus::Finished).unwrap();
    mf.record(300, 200, SectorStatus::NonTrimmed).unwrap();
    mf.record(500, 100, SectorStatus::Unreadable).unwrap();
    mf.record(600, 400, SectorStatus::Finished).unwrap();

    // Final entries: [0..300 Finished, 300..500 NonTrimmed, 500..600 Unreadable, 600..1000 Finished]
    let s = mf.stats();
    assert_eq!(s.bytes_good, 700); // 300 + 400
    assert_eq!(s.bytes_unreadable, 100); // 100
    assert_eq!(s.bytes_pending, 200); // NonTrimmed only (NonTried=0)
    assert_eq!(s.bytes_nontried, 0);
    assert_eq!(s.bytes_retryable, 200); // NonTrimmed
    assert_eq!(s.bytes_total, 1000);

    // Overwrite a NonTrimmed range with Finished
    mf.record(300, 100, SectorStatus::Finished).unwrap();
    // Entries: [0..400 Finished, 400..500 NonTrimmed, 500..600 Unreadable, 600..1000 Finished]
    let s2 = mf.stats();
    assert_eq!(s2.bytes_good, 800); // 400 + 400
    assert_eq!(s2.bytes_unreadable, 100);
    assert_eq!(s2.bytes_pending, 100); // NonTrimmed only
    assert_eq!(s2.bytes_retryable, 100);

    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_rejects_entry_whose_range_overflows_u64() {
    let p = tmpfile("load_overflow");
    let _ = std::fs::remove_file(&p);
    // pos near u64::MAX with a nonzero size overflows pos+size.
    let body = format!("0x{:x} 0x10 +\n", u64::MAX - 4);
    std::fs::write(&p, body).unwrap();
    let kind = match Mapfile::load(&p) {
        Ok(_) => panic!("overflowing entry must be rejected"),
        Err(e) => e.kind(),
    };
    assert_eq!(kind, io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn record_rejects_range_overflowing_u64() {
    let p = tmpfile("record_overflow");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    let err = mf
        .record(u64::MAX - 4, 16, SectorStatus::Finished)
        .expect_err("overflowing record must be rejected");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn parse_hex16_does_not_panic_on_multibyte_32_byte_input() {
    // A 32-BYTE comment containing a multi-byte char would make the
    // old `&s[i*2..i*2+2]` slice fall inside a char boundary and
    // panic. Must return None instead.
    let s = "中".to_string() + &"a".repeat(29); // 3 + 29 = 32 bytes
    assert_eq!(s.len(), 32);
    assert_eq!(parse_hex16(&s), None);
    // A valid 32-char ASCII hex string still parses.
    assert_eq!(
        parse_hex16("00112233445566778899aabbccddeeff"),
        Some([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ])
    );
}

#[test]
fn load_rejects_overflowing_pos_plus_size() {
    let p = tmpfile("load_rejects_overflow");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0xfffffffffffffff0  0x20    +\n",
    )
    .unwrap();
    assert!(
        Mapfile::load(&p).is_err(),
        "a pos+size that overflows u64 must be rejected, not wrap"
    );
    let _ = std::fs::remove_file(&p);
}

#[test]
fn load_rejects_overlapping_ranges() {
    let p = tmpfile("load_rejects_overlap");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000080  0x00000100    -\n",
    )
    .unwrap();
    assert!(
        Mapfile::load(&p).is_err(),
        "overlapping ranges must be rejected so stats can't double-count"
    );
    let _ = std::fs::remove_file(&p);
}

// Regression: an INTERNAL hole (byte range no entry covers) must load
// filled as NonTried so it's visible to resume, else total_size still
// equals the disc size and copy()'s complete-check misreports a hole.
#[test]
fn load_fills_internal_gap_as_nontried() {
    let p = tmpfile("load_fills_internal_gap");
    let _ = std::fs::remove_file(&p);
    // Two Finished entries: [0,0x100) and [0x200,0x300). The hole at
    // [0x100,0x200) is never covered.
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000200  0x00000100    +\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).expect("holed mapfile must load (gap filled, not rejected)");
    // The hole [0x100,0x200) must now be a NonTried entry.
    let hole = mf
        .entries()
        .iter()
        .find(|e| e.pos == 0x100)
        .expect("internal gap must be filled with a synthetic entry");
    assert_eq!(hole.size, 0x100, "filled gap covers the whole hole");
    assert_eq!(
        hole.status,
        SectorStatus::NonTried,
        "filled gap must be NonTried so resume reads it"
    );
    // total_size unchanged (last entry end), but the hole is now pending.
    assert_eq!(mf.total_size(), 0x300);
    // EXACTLY the hole, not "at least" it: doubling the NonTried contribution
    // in compute_stats left this assertion green (satisfied by `>=`) while
    // six other mapfile tests went red.
    assert_eq!(
        mf.stats().bytes_pending,
        0x100,
        "the hole must count as pending — exactly once — so copy() doesn't \
             report complete"
    );
    let _ = std::fs::remove_file(&p);
}

/// Regression: a LEADING gap (first entry doesn't start at 0) is filled
/// as NonTried too, so resume reads the head of the disc.
#[test]
fn load_fills_leading_gap_as_nontried() {
    let p = tmpfile("load_fills_leading_gap");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000080  0x00000100    +\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).expect("leading-gap mapfile must load");
    let head = mf
        .entries()
        .first()
        .expect("must have a leading fill entry");
    assert_eq!(head.pos, 0, "fill must start at byte 0");
    assert_eq!(head.size, 0x80);
    assert_eq!(head.status, SectorStatus::NonTried);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn num_bad_ranges_counts_unreadable_entries() {
    let p = tmpfile("num_bad_ranges");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 50, SectorStatus::Unreadable).unwrap();
    mf.record(300, 50, SectorStatus::Unreadable).unwrap();
    assert_eq!(mf.stats().num_bad_ranges, 2);
    let _ = std::fs::remove_file(&p);
}

// ── status char round-trip (ddrescue alphabet ?*/-+) ──────────
// Every SectorStatus must round-trip to_char/from_char with the exact
// alphabet — a swapped mapping would silently misclassify resume state.
#[test]
fn status_char_round_trip_is_ddrescue_alphabet() {
    let pairs = [
        (SectorStatus::NonTried, '?'),
        (SectorStatus::NonTrimmed, '*'),
        (SectorStatus::NonScraped, '/'),
        (SectorStatus::Unreadable, '-'),
        (SectorStatus::Finished, '+'),
    ];
    for (st, ch) in pairs {
        assert_eq!(st.to_char(), ch, "{st:?} must map to '{ch}'");
        assert_eq!(SectorStatus::from_char(ch), Some(st));
    }
    // Any char outside the alphabet is rejected. This list used to end in
    // `'?'.to_ascii_uppercase()` (just `'?'`, a valid status char), asserting
    // nothing the `pairs` loop above hadn't already covered.
    for bad in ['x', ' ', '0', '#', '!'] {
        assert_eq!(
            SectorStatus::from_char(bad),
            None,
            "'{bad}' is not a status"
        );
    }
}

// ── parse_hex / parse_uk_line / parse_hex16 error paths ─────

/// parse_hex accepts both `0x`-prefixed and bare hex (ddrescue writes
/// `0x`-prefixed). A non-hex field is a MapfileInvalid{kind:"hex"}.
#[test]
fn parse_hex_accepts_prefixed_and_bare_rejects_garbage() {
    assert_eq!(parse_hex("0x10").unwrap(), 16);
    assert_eq!(parse_hex("10").unwrap(), 16);
    assert_eq!(parse_hex("0xffffffff").unwrap(), 0xffff_ffff);
    let err = parse_hex("0xzz").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

// The scope survives a flush/load as a ddrescue-safe comment, and on load a scoped
// map's never-read rest is not pending (it is not this image's job).
#[test]
fn scope_round_trips_and_scopes_the_pending_stats() {
    let p = tmpfile("scope_round_trip");
    let mut mf = Mapfile::create(&p, 64 * 2048, "test").unwrap();
    mf.set_scope(vec![
        (32 * 2048, 8 * 2048),
        (0, 8 * 2048),
        (4 * 2048, 2 * 2048),
    ]);
    mf.record(0, 8 * 2048, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        text.contains("# freemkv-scope: 0x0+0x4000,0x10000+0x4000"),
        "{text}"
    );
    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.scope(), Some(&[(0, 0x4000), (0x10000, 0x4000)][..]));
    let st = loaded.stats();
    assert_eq!((st.bytes_pending, st.bytes_nontried), (8 * 2048, 8 * 2048));
    assert_eq!(st.bytes_total, 64 * 2048);
    let mut widened = loaded;
    widened.clear_scope();
    assert_eq!(
        widened.stats().bytes_pending,
        56 * 2048,
        "the rest is pending again"
    );
    widened.flush().unwrap();
    assert!(
        !std::fs::read_to_string(&p)
            .unwrap()
            .contains("freemkv-scope")
    );
    let _ = std::fs::remove_file(&p);
}

// M7: a scoped stats() is one merge walk over entries and scope, not entries x scope.
#[test]
fn scoped_stats_is_linear_in_entries_plus_scope() {
    const N: u64 = 40_000;
    let p = tmpfile("scoped_stats_linear");
    let mut mf = Mapfile::create(&p, 2 * N * 2048, "test").unwrap();
    let _ = std::fs::remove_file(&p);
    mf.entries = (0..2 * N)
        .map(|i| MapEntry {
            pos: i * 2048,
            size: 2048,
            status: if i % 2 == 0 {
                SectorStatus::Finished
            } else {
                SectorStatus::NonTried
            },
        })
        .collect();
    mf.stats = Mapfile::compute_stats(&mf.entries, mf.total_size);
    // Scope: the first half of every NonTried run, plus one range spanning two runs.
    let mut scope: Vec<(u64, u64)> = (0..N).map(|i| ((2 * i + 1) * 2048, 1024)).collect();
    scope.push((3 * 2048 + 1024, 2 * 2048));
    mf.set_scope(scope);
    let t = Instant::now();
    let st = mf.stats();
    let took = t.elapsed();
    assert_eq!(st.bytes_nontried, N * 1024 + 1024);
    assert_eq!(st.bytes_pending, st.bytes_nontried);
    // Loose enough for a loaded debug runner; a quadratic walk takes seconds.
    assert!(took < Duration::from_secs(1), "stats() took {took:?}");
    mf.dirty = false;
}

// Dropping a malformed scope would present a partial image as a whole one.
#[test]
fn a_malformed_scope_header_is_refused() {
    for bad in ["zz", "0x0", "0x0+", "0xffffffffffffffff+0x2"] {
        let p = tmpfile("scope_bad");
        std::fs::write(
            &p,
            format!("# freemkv-scope: {bad}\n0x0 ? 1\n0x0 0x800 ?\n"),
        )
        .unwrap();
        let e = Mapfile::load(&p).map(|_| ()).unwrap_err();
        let _ = std::fs::remove_file(&p);
        assert_eq!(invalid_kind(&e), Some("scope"), "{bad}: {e}");
    }
}

#[test]
fn intersect_keeps_only_the_overlap() {
    assert_eq!(
        intersect(&[(0, 100)], &[(10, 5), (90, 20)]),
        [(10, 5), (90, 10)]
    );
    assert!(intersect(&[(0, 10)], &[(10, 5)]).is_empty());
    assert_eq!(merge_byte_ranges(&mut [(5, 5), (0, 5), (20, 0)]), [(0, 10)]);
}

// Many ranges against many scope runs, both directions of overlap, against a brute force.
#[test]
fn intersect_matches_every_pair_overlap() {
    let ranges: Vec<(u64, u64)> = (0..50).map(|i| (i * 100, 60)).collect();
    let scope: Vec<(u64, u64)> = (0..20).map(|i| (i * 250 + 30, 170)).collect();
    let mut brute = Vec::new();
    for &(p, n) in &ranges {
        for &(sp, sn) in &scope {
            let (a, b) = (p.max(sp), (p + n).min(sp + sn));
            if a < b {
                brute.push((a, b - a));
            }
        }
    }
    assert_eq!(intersect(&ranges, &scope), brute);
}

// `is_finished` is the one definition; every status a mapfile can hold sits in exactly
// the sets it implies.
#[test]
fn status_sets_agree_with_is_finished() {
    for s in (0u8..=127).filter_map(|c| SectorStatus::from_char(c as char)) {
        assert_eq!(
            bad_sector_statuses().contains(&s),
            !s.is_finished(),
            "{s:?}"
        );
        let damage = !s.is_finished() && s != SectorStatus::NonTried;
        assert_eq!(damage_sector_statuses().contains(&s), damage, "{s:?}");
    }
}

#[test]
fn a_zero_size_entry_is_refused() {
    let e = load_text("zero_size_entry", "0x0 0x800 +\n0x800 0x0 -\n")
        .map(|_| ())
        .unwrap_err();
    assert_eq!(invalid_kind(&e), Some("zero_size"), "{e}");
}

// `record` persists on its own once the flush interval has passed, with no flush call.
#[test]
fn record_persists_once_the_interval_elapses() {
    let p = tmpfile("record_interval");
    let mut mf = Mapfile::create(&p, 0x10000, "test").unwrap();
    std::thread::sleep(FLUSH_INTERVAL + Duration::from_millis(50));
    mf.record(0, 0x800, SectorStatus::Finished).unwrap();
    let on_disk = Mapfile::load(&p).unwrap();
    assert_eq!(on_disk.stats().bytes_good, 0x800);
    mf.dirty = false;
    let _ = std::fs::remove_file(&p);
}

#[test]
fn parse_uk_line_rejects_malformed() {
    assert_eq!(parse_uk_line("no-colon"), None);
    assert_eq!(
        parse_uk_line("notanumber:11111111111111111111111111111111"),
        None
    );
    // 30 hex chars (15 bytes) — wrong length.
    assert_eq!(parse_uk_line("0:1111111111111111111111111111"), None);
    // Valid.
    assert_eq!(
        parse_uk_line("3:000102030405060708090a0b0c0d0e0f"),
        Some((3u32, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))
    );
}

/// parse_hex16 tolerates an optional `0x` prefix and uppercase hex,
/// but a 31- or 33-char string (not 32) is rejected — a VID is exactly
/// 16 bytes = 32 hex chars.
#[test]
fn parse_hex16_length_and_case() {
    assert_eq!(
        parse_hex16("0xAABBCCDDEEFF00112233445566778899"),
        Some([
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99
        ])
    );
    assert_eq!(parse_hex16(&"a".repeat(31)), None);
    assert_eq!(parse_hex16(&"a".repeat(33)), None);
}

// ── next_with / ranges_with semantics ─────────────────────────

/// next_with returns the first matching range AT OR AFTER `from`,
/// clipping the returned start to `from` when `from` lands inside a
/// matching range (the patch loop relies on resuming mid-range).
#[test]
fn next_with_clips_start_to_from() {
    let p = tmpfile("next_with_clips");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(200, 300, SectorStatus::NonTrimmed).unwrap();
    // from inside the NonTrimmed range [200,500): start clips to 350,
    // size is 500-350 = 150.
    assert_eq!(
        mf.next_with(350, SectorStatus::NonTrimmed),
        Some((350, 150))
    );
    // from before the range: returns the whole range from its pos.
    assert_eq!(mf.next_with(0, SectorStatus::NonTrimmed), Some((200, 300)));
    // from at/after the range end: no match.
    assert_eq!(mf.next_with(500, SectorStatus::NonTrimmed), None);
    // status with no entries: None.
    assert_eq!(mf.next_with(0, SectorStatus::Unreadable), None);
    let _ = std::fs::remove_file(&p);
}

/// ranges_with matches ANY of the supplied statuses, preserving
/// position order. Used to build the Pass-N retry queue (NonTrimmed +
/// NonScraped together).
#[test]
fn ranges_with_multiple_statuses_in_order() {
    let p = tmpfile("ranges_with_multi");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(100, 100, SectorStatus::NonTrimmed).unwrap();
    mf.record(300, 100, SectorStatus::NonScraped).unwrap();
    mf.record(500, 100, SectorStatus::Unreadable).unwrap();
    let retry = mf.ranges_with(&[SectorStatus::NonTrimmed, SectorStatus::NonScraped]);
    assert_eq!(retry, vec![(100, 100), (300, 100)]);
    let _ = std::fs::remove_file(&p);
}

// ── record edge cases ─────────────────────────────────────────

/// A zero-size record is a no-op (record() early-returns on size==0):
/// entries and stats are unchanged.
#[test]
fn record_zero_size_is_noop() {
    let p = tmpfile("record_zero");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    let before = mf.entries().to_vec();
    mf.record(500, 0, SectorStatus::Finished).unwrap();
    assert_eq!(mf.entries(), before.as_slice());
    assert_eq!(mf.stats().bytes_good, 0);
    let _ = std::fs::remove_file(&p);
}

/// Recording the FULL disc with one status collapses to a single
/// coalesced entry (record splits then merges adjacent same-status).
#[test]
fn record_full_span_coalesces_to_one_entry() {
    let p = tmpfile("record_full_span");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(0, 500, SectorStatus::Finished).unwrap();
    mf.record(500, 500, SectorStatus::Finished).unwrap();
    let es = mf.entries();
    assert_eq!(es.len(), 1, "two adjacent Finished must coalesce");
    assert_eq!((es[0].pos, es[0].size), (0, 1000));
    assert_eq!(mf.stats().bytes_good, 1000);
    let _ = std::fs::remove_file(&p);
}

/// A record that exactly overwrites the whole previous entry leaves the
/// partition disjoint and total coverage invariant. bytes_total stays
/// constant; good+pending+unreadable always sums to total.
#[test]
fn record_partition_invariant_total_coverage() {
    let p = tmpfile("record_invariant");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(0, 250, SectorStatus::Finished).unwrap();
    mf.record(250, 250, SectorStatus::Unreadable).unwrap();
    mf.record(500, 250, SectorStatus::NonTrimmed).unwrap();
    // [750, 1000) is still NonTried.
    let s = mf.stats();
    assert_eq!(
        s.bytes_good + s.bytes_unreadable + s.bytes_pending,
        s.bytes_total,
        "coverage must partition the disc exactly"
    );
    // Entries must be disjoint and sorted.
    let es = mf.entries();
    for w in es.windows(2) {
        assert!(
            w[0].pos + w[0].size <= w[1].pos,
            "entries must stay disjoint and sorted"
        );
    }
    let _ = std::fs::remove_file(&p);
}

// ── load() current-line heuristic ─────────────────────────────

/// load() skips the ddrescue "current pos" status line (2nd field is a
/// status char, not a 0x size) and parses the data lines that follow.
/// The header doc shows `0x000000000  ?  1  0` as the status line.
#[test]
fn load_skips_current_status_line() {
    let p = tmpfile("load_skips_current");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000100  0x00000100    -\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).unwrap();
    assert_eq!(mf.entries().len(), 2);
    assert_eq!(mf.entries()[0].status, SectorStatus::Finished);
    assert_eq!(mf.entries()[1].status, SectorStatus::Unreadable);
    let _ = std::fs::remove_file(&p);
}

/// A mapfile written WITHOUT a current-line (first non-comment line is
/// already a data entry: 2nd field starts `0x`) must still parse that
/// first line as an entry — the heuristic detects it and falls through.
#[test]
fn load_treats_leading_data_line_as_entry() {
    let p = tmpfile("load_leading_entry");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  0x00000200    +\n\
             0x000000200  0x00000100    ?\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).unwrap();
    // First line is NOT a status line; both lines are entries.
    assert_eq!(mf.entries().len(), 2);
    assert_eq!(mf.entries()[0].size, 0x200);
    let _ = std::fs::remove_file(&p);
}

// Regression: a leading DATA line with NO `0x` size prefix must still
// parse as an entry, not get misclassified as the current-status line
// and dropped (discriminator keys off field shape, not the prefix).
#[test]
fn load_treats_leading_data_line_without_0x_prefix_as_entry() {
    let p = tmpfile("load_leading_entry_no_0x");
    let _ = std::fs::remove_file(&p);
    // Note: sizes/positions written WITHOUT the `0x` prefix.
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             000000000  200    +\n\
             000000200  100    ?\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).unwrap();
    // The old `0x`-prefix heuristic would have skipped the first line as a
    // "current line" and lost a valid `+` entry. Both lines are entries.
    assert_eq!(mf.entries().len(), 2);
    assert_eq!(mf.entries()[0].size, 0x200);
    assert_eq!(mf.entries()[0].status, SectorStatus::Finished);
    assert_eq!(mf.entries()[1].status, SectorStatus::NonTried);
    let _ = std::fs::remove_file(&p);
}

/// load() parses the version from the `# Rescue Logfile. Created by`
/// header and exposes it (round-trips through write_to_disk).
#[test]
fn load_parses_version_header() {
    let p = tmpfile("load_version");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by libfreemkv v9.9.9\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).unwrap();
    assert_eq!(mf.version, "libfreemkv v9.9.9");
    let _ = std::fs::remove_file(&p);
}

/// load() rejects an entry with a non-hex pos/size field
/// (MapfileInvalid{kind:"hex"}) rather than silently skipping it —
/// a corrupt data line must not be dropped, masking missing coverage.
#[test]
fn load_rejects_non_hex_field() {
    let p = tmpfile("load_nonhex");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0xZZZ  0x100    +\n",
    )
    .unwrap();
    assert!(Mapfile::load(&p).is_err());
    let _ = std::fs::remove_file(&p);
}

// A truncated data line is REFUSED, not skipped (skipping would shrink total_size and hide
// missing coverage).
#[test]
fn load_rejects_a_data_line_with_too_few_fields() {
    let p = tmpfile("load_shortline");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x0  ?  1  0\n\
             0x0        0x2800  +\n\
             0x2800     0x800\n",
    )
    .unwrap();
    let err = match Mapfile::load(&p) {
        Ok(map) => panic!(
            "a short line must not be skipped; loaded total_size={:#x} good={:#x}",
            map.total_size(),
            map.stats().bytes_good
        ),
        Err(e) => e,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

/// A single-field line is the same case one field shorter.
#[test]
fn load_rejects_a_data_line_with_one_field() {
    let p = tmpfile("load_onefield");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x0  ?  1  0\n\
             0x0        0x2800  +\n\
             0x2800\n",
    )
    .unwrap();
    let err = match Mapfile::load(&p) {
        Ok(_) => panic!("a one-field line must be refused, not skipped"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

// A malformed legacy VID header must FAIL the load, not drop
// silently — that would turn "carries an identity" into "carries none",
// which reopens the cross-disc resume splice the identity guard stops.
#[test]
fn load_rejects_a_malformed_vid_header() {
    let p = tmpfile("load_bad_vid");
    let _ = std::fs::remove_file(&p);
    // The legacy prefix is assembled so only `parse_legacy_key_lines` spells it (EK9).
    let vid_line = format!("# {}vid: 00112233445566778899aabbccddeezz", "freemkv-");
    std::fs::write(
        &p,
        format!("# Rescue Logfile. Created by test\n{vid_line}\n0x0  ?  1  0\n0x0  0x800    +\n"),
    )
    .unwrap();
    let err = match Mapfile::load(&p) {
        Ok(mf) => panic!(
            "a corrupt VID header must not load as 'no identity' (vidfp={:?})",
            mf.vid_fingerprint()
        ),
        Err(e) => e,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

/// load() rejects an unknown status char (MapfileInvalid{kind:
/// "status_char"}). A `~` is not in the ddrescue alphabet.
#[test]
fn load_rejects_unknown_status_char() {
    let p = tmpfile("load_badstatus");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x100    ~\n",
    )
    .unwrap();
    let err = match Mapfile::load(&p) {
        Ok(_) => panic!("unknown status char must be rejected"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    let _ = std::fs::remove_file(&p);
}

/// An empty mapfile (only comments / blank lines) loads with zero
/// entries and total_size 0 — never panics on the `entries.last()` None.
#[test]
fn load_empty_mapfile_is_zero_total() {
    let p = tmpfile("load_empty");
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, "# Rescue Logfile. Created by test\n\n   \n").unwrap();
    let mf = Mapfile::load(&p).unwrap();
    assert!(mf.entries().is_empty());
    assert_eq!(mf.total_size(), 0);
    assert_eq!(mf.stats().bytes_total, 0);
    let _ = std::fs::remove_file(&p);
}

/// load() sorts entries by pos even when the file lists them out of
/// order, and total_size derives from the highest end (entries are
/// sorted then last().pos+size).
#[test]
fn load_sorts_out_of_order_entries() {
    let p = tmpfile("load_sort");
    let _ = std::fs::remove_file(&p);
    std::fs::write(
        &p,
        "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000200  0x00000100    -\n\
             0x000000000  0x00000200    +\n",
    )
    .unwrap();
    let mf = Mapfile::load(&p).unwrap();
    assert_eq!(mf.entries()[0].pos, 0);
    assert_eq!(mf.entries()[1].pos, 0x200);
    assert_eq!(mf.total_size(), 0x300);
    let _ = std::fs::remove_file(&p);
}

fn invalid_kind(e: &io::Error) -> Option<&'static str> {
    match e.get_ref()?.downcast_ref::<libfreemkv::error::Error>()? {
        libfreemkv::error::Error::MapfileInvalid { kind } => Some(kind),
        _ => None,
    }
}

fn load_text(tag: &str, text: &str) -> io::Result<Mapfile> {
    let p = tmpfile(tag);
    std::fs::write(&p, text).unwrap();
    let r = Mapfile::load(&p);
    let _ = std::fs::remove_file(&p);
    r
}

// M28: a hand-edited or foreign (ddrescue) file with split same-status runs loads as the
// canonical maximal-run partition `record()` relies on.
#[test]
fn load_coalesces_adjacent_same_status_runs() {
    let mf = load_text(
        "load_coalesce",
        "0x0  ?  1\n0x0 0x100 +\n0x100 0x100 +\n0x200 0x80 -\n0x280 0x80 -\n0x300 0x100 +\n",
    )
    .unwrap();
    let got: Vec<_> = mf
        .entries()
        .iter()
        .map(|e| (e.pos, e.size, e.status))
        .collect();
    assert_eq!(
        got,
        [
            (0, 0x200, SectorStatus::Finished),
            (0x200, 0x100, SectorStatus::Unreadable),
            (0x300, 0x100, SectorStatus::Finished),
        ]
    );
    assert_eq!(mf.stats().num_bad_ranges, 1);
    assert_canonical(&mf);
}

// M28: an oversized file (a wrong file at the mapfile path) is refused before it is read.
#[test]
fn load_refuses_an_oversized_file() {
    let p = tmpfile("load_oversized");
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(MAX_MAPFILE_BYTES + 1).unwrap();
    drop(f);
    let e = Mapfile::load(&p).map(|_| ()).unwrap_err();
    let _ = std::fs::remove_file(&p);
    assert_eq!(invalid_kind(&e), Some("too_large"), "{e}");
}

// M3: ddrescue's current-line status also has `F` (filling) and `G` (generating).
#[test]
fn load_accepts_ddrescue_fill_and_generate_current_lines() {
    for cur in ["0x00100000  F  1", "0x00100000  G  1", "0x0  F"] {
        let mf = load_text(
            "load_current_fg",
            &format!("# Rescue Logfile. Created by GNU ddrescue\n{cur}\n0x0 0x800 +\n"),
        )
        .unwrap_or_else(|e| panic!("{cur}: {e}"));
        assert_eq!(mf.entries().len(), 1, "{cur}");
        assert_eq!(mf.total_size(), 0x800, "{cur}");
    }
    // A bare-hex data line whose size is `F` is still a data line.
    let mf = load_text("load_size_f", "0 F +\nF 1 -\n").unwrap();
    assert_eq!(mf.total_size(), 0x10);
    assert_eq!(mf.stats().bytes_good, 0xF);
}

// N12: a zero-size mapfile round-trips (no zero-size entry that load() refuses).
#[test]
fn a_zero_size_mapfile_round_trips() {
    let p = tmpfile("zero_size_create");
    let mf = Mapfile::create(&p, 0, "test").unwrap();
    let back = Mapfile::load(&p).map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&p);
    let back = back.unwrap();
    assert!(mf.entries().is_empty());
    assert_eq!((back.total_size(), back.entries().len()), (0, 0));
    assert_eq!(back.stats(), mf.stats());
}

// M26: a data line is exactly `pos size status`, the status one character.
#[test]
fn load_refuses_a_malformed_status_field() {
    for (line, kind) in [
        ("0x0 0x800 +garbage", "status_char"),
        ("0x0 0x800 +?", "status_char"),
        ("0x0 0x800 + extra", "long_line"),
    ] {
        let e = load_text("load_status_field", &format!("0x0 ? 1\n{line}\n"))
            .map(|_| ())
            .unwrap_err();
        assert_eq!(invalid_kind(&e), Some(kind), "{line}: {e}");
    }
}

// ── write_to_disk format ──────────────────────────────────────
// Entries round-trip through load() with the fixed header block
// (Created by / Current pos / column header) intact for external tools.
#[test]
fn write_to_disk_format_round_trips_and_has_headers() {
    let p = tmpfile("write_format");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 0x1000, "vTEST").unwrap();
    mf.record(0x100, 0x200, SectorStatus::Finished).unwrap();
    mf.record(0x500, 0x100, SectorStatus::Unreadable).unwrap();
    mf.flush().unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("# Rescue Logfile. Created by vTEST"));
    assert!(text.contains("# Current pos / status / pass / pass_time"));
    assert!(text.contains("0x000000100  0x000000200    +"));
    assert!(text.contains("0x000000500  0x000000100    -"));
    let reloaded = Mapfile::load(&p).unwrap();
    assert_eq!(reloaded.entries(), mf.entries());
    let _ = std::fs::remove_file(&p);
}

/// create() persists immediately so a resume sees the fresh mapfile
/// even if record() is never called (load right after create matches).
#[test]
fn create_persists_eagerly() {
    let p = tmpfile("create_eager");
    let _ = std::fs::remove_file(&p);
    let mf = Mapfile::create(&p, 4096, "test").unwrap();
    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.entries(), mf.entries());
    assert_eq!(loaded.total_size(), 4096);
    let _ = std::fs::remove_file(&p);
}

/// open_or_create returns a fresh NonTried mapfile when the path does
/// not exist (NotFound → create), not an error.
#[test]
fn open_or_create_creates_when_absent() {
    let p = tmpfile("open_or_create_absent");
    let _ = std::fs::remove_file(&p);
    let mf = Mapfile::open_or_create(&p, 2048, "test").unwrap();
    assert_eq!(mf.entries().len(), 1);
    assert_eq!(mf.entries()[0].status, SectorStatus::NonTried);
    assert_eq!(mf.total_size(), 2048);
    let _ = std::fs::remove_file(&p);
}

/// open_or_create loads an existing file (and does NOT reset it to
/// NonTried) even when the supplied total_size differs from the loaded
/// coverage — the warn path must still return the loaded state.
#[test]
fn open_or_create_loads_existing_despite_size_mismatch() {
    let p = tmpfile("open_or_create_mismatch");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    mf.record(0, 500, SectorStatus::Finished).unwrap();
    mf.flush().unwrap();
    // Supply a DIFFERENT total; must still load the existing entries.
    let reopened = Mapfile::open_or_create(&p, 999_999, "test").unwrap();
    assert_eq!(reopened.stats().bytes_good, 500);
    // Loaded total reflects the file, not the supplied arg.
    assert_eq!(reopened.total_size(), 1000);
    let _ = std::fs::remove_file(&p);
}

/// ONLY a missing file means "create a fresh one". A file that exists but
/// does not parse is a real error and must propagate: creating over it
/// silently DESTROYS the record of what was already read, and the rip
/// restarts from sector 0 reporting a clean resume.
#[test]
fn open_or_create_propagates_a_corrupt_file_instead_of_overwriting_it() {
    let p = tmpfile("open_or_create_corrupt");
    let _ = std::fs::remove_file(&p);
    // Overlapping ranges — `load` rejects these (MapfileInvalid), which is
    // NOT io::ErrorKind::NotFound.
    let corrupt = "# Rescue Logfile. Created by test\n\
             0x000000000  ?  1  0\n\
             0x000000000  0x00000100    +\n\
             0x000000080  0x00000100    -\n";
    std::fs::write(&p, corrupt).unwrap();
    let err = Mapfile::open_or_create(&p, 0x100, "test")
        .expect_err("a corrupt mapfile is an error, not a reason to start over");
    assert_ne!(
        err.kind(),
        io::ErrorKind::NotFound,
        "the file is present; only NotFound may route to create()"
    );
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        corrupt,
        "the unparseable mapfile was overwritten — the prior rip's progress is gone"
    );
    let _ = std::fs::remove_file(&p);
}

/// Drop flushes pending in-memory state (a sweep that returns early
/// must not lose records). After dropping a dirty Mapfile, a fresh
/// load() sees the last record.
#[test]
fn drop_flushes_pending_state() {
    let p = tmpfile("drop_flush");
    let _ = std::fs::remove_file(&p);
    {
        let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
        // record may or may not flush (time-batched); ensure dirty.
        mf.record(0, 400, SectorStatus::Finished).unwrap();
        // Drop here flushes.
    }
    let loaded = Mapfile::load(&p).unwrap();
    assert_eq!(loaded.stats().bytes_good, 400);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn stats_consistent_after_split_record() {
    let p = tmpfile("stats_consistent_after_split");
    let _ = std::fs::remove_file(&p);
    let mut mf = Mapfile::create(&p, 1000, "test").unwrap();
    // Mark middle as NonTrimmed
    mf.record(200, 400, SectorStatus::NonTrimmed).unwrap();
    // Entries: [0..200 NonTried, 200..600 NonTrimmed, 600..1000 NonTried]
    let s = mf.stats();
    assert_eq!(s.bytes_pending, 1000); // NonTried(600) + NonTrimmed(400)
    assert_eq!(s.bytes_retryable, 400); // NonTrimmed only
    assert_eq!(s.bytes_nontried, 600); // 200 + 400

    // Overwrite the NonTrimmed with Finished (splitting the remaining NonTried)
    mf.record(200, 400, SectorStatus::Finished).unwrap();
    // Entries: [0..200 NonTried, 200..600 Finished, 600..1000 NonTried]
    let s2 = mf.stats();
    assert_eq!(s2.bytes_good, 400);
    assert_eq!(s2.bytes_pending, 600); // NonTried(200 + 400)
    assert_eq!(s2.bytes_nontried, 600);
    assert_eq!(s2.bytes_retryable, 0);

    let _ = std::fs::remove_file(&p);
}
