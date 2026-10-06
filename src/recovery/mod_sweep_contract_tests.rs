use super::*;
use libfreemkv::disc::DiscRegion;
use libfreemkv::{ContentFormat, Disc, DiscFormat};

type FailAt = Box<dyn FnMut(u32) -> Option<Error> + Send>;

// A reader that fills every sector with 0xAA unless `fail` answers an error for the
// batch starting at that LBA; records every `SET CD SPEED`.
struct Reader {
    sectors: u32,
    fail: FailAt,
    speeds: Vec<u16>,
}

impl SectorSource for Reader {
    fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], _: bool) -> Result<usize> {
        if let Some(e) = (self.fail)(lba) {
            return Err(e);
        }
        let n = count as usize * 2048;
        buf[..n].fill(0xAA);
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.sectors
    }
    fn set_speed(&mut self, kbs: u16) {
        self.speeds.push(kbs);
    }
}

fn reader(sectors: u32, fail: impl FnMut(u32) -> Option<Error> + Send + 'static) -> Reader {
    Reader {
        sectors,
        fail: Box::new(fail),
        speeds: Vec::new(),
    }
}

fn disc(sectors: u32) -> Disc {
    Disc {
        volume_id: "SWEEPCONTRACT".into(),
        meta_title: None,
        format: DiscFormat::Uhd,
        capacity_sectors: sectors,
        capacity_bytes: sectors as u64 * 2048,
        layers: 1,
        titles: Vec::new(),
        region: DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

fn opts(resume: bool, skip_on_error: bool) -> SweepOptions<'static> {
    SweepOptions {
        resume,
        skip_on_error,
        ..Default::default()
    }
}

// R10: only a corrupt mapfile (InvalidData) downgrades a resume to a fresh sweep. An
// unreadable one (a directory in its place here, read as EISDIR even by root; EACCES or
// EIO on a flaky mount) must fail the pass, not be deleted along with a truncated ISO.
#[cfg(unix)]
#[test]
fn a_resume_whose_mapfile_cannot_be_read_fails_instead_of_starting_over() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("ioerr.iso");
    let sectors = 500u32;
    let total = sectors as u64 * 2048;
    let d = disc(sectors);
    let mf = d.mapfile_for(&iso);
    std::fs::write(&iso, vec![0x5Au8; total as usize]).unwrap();
    std::fs::create_dir(&mf).unwrap();
    std::fs::write(mf.join("keep"), b"x").unwrap();

    let r = sweep(&d, &mut reader(sectors, |_| None), &iso, &opts(true, true));
    assert!(
        r.is_err(),
        "an unreadable mapfile must fail the resume: {r:?}"
    );
    assert!(mf.exists(), "the unreadable mapfile must not be deleted");
    let img = std::fs::read(&iso).unwrap();
    assert_eq!(img.len() as u64, total, "the ISO must not be truncated");
    assert!(
        img.iter().all(|&b| b == 0x5A),
        "the ISO must not be rewritten"
    );
}

// R6: a Stop that lands mid-read comes back from the drive as `Error::Halted`. That
// is a stop, not damage: no zero-fill, no NonTrimmed, no drop to minimum speed, and
// the pass ends halted (not `Err(Halted)`) whether or not it skips errors.
#[test]
fn a_read_the_stop_interrupts_is_a_stop_not_damage() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    for skip_on_error in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("stop.iso");
        let sectors = 4096u32;
        let d = disc(sectors);
        let flag = Arc::new(AtomicBool::new(false));
        let raised = flag.clone();
        let mut r = reader(sectors, move |lba| {
            (lba >= 1024).then(|| {
                raised.store(true, Ordering::SeqCst);
                Error::Halted
            })
        });
        let mut o = opts(false, skip_on_error);
        o.halt = Some(flag);
        o.batch_sectors = Some(32); // 1024 is a batch boundary in both modes
        let res = sweep(&d, &mut r, &iso, &o);
        let res = res.unwrap_or_else(|e| panic!("skip={skip_on_error}: a stop is not Err: {e:?}"));
        assert!(res.halted, "skip={skip_on_error}: the pass must end halted");
        assert!(
            !r.speeds.contains(&0),
            "skip={skip_on_error}: a stop is not a damage zone"
        );
        let map = mapfile::Mapfile::load(&d.mapfile_for(&iso)).unwrap();
        assert!(
            map.ranges_with(&mapfile::damage_sector_statuses())
                .is_empty(),
            "skip={skip_on_error}: the stop was recorded as damage"
        );
        assert_eq!(
            map.ranges_with(&[mapfile::SectorStatus::NonTried]),
            vec![(1024 * 2048, (sectors as u64 - 1024) * 2048)],
            "skip={skip_on_error}: everything from the interrupted read on stays NonTried"
        );
    }
}

// Every tick a sweep reports, in order.
#[derive(Default)]
struct Ticks(std::sync::Mutex<Vec<libfreemkv::progress::PassProgress>>);

impl libfreemkv::Events for Ticks {
    fn event(&self, e: &libfreemkv::Event<'_>) {
        if let libfreemkv::Event::Pass(p) = e {
            self.0.lock().unwrap().push((*p).clone());
        }
    }
}

// R4: `work_done / work_total` is the pass's own 0..=100% — a scoped (MKV-staging)
// sweep's bar runs over its scope, not the absolute disc position.
#[test]
fn a_scoped_sweep_s_bar_runs_from_zero_to_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("scoped.iso");
    let d = disc(8192);
    let ticks = Ticks::default();
    let mut o = opts(false, true);
    o.progress = Some(&ticks);
    sweep_scoped(&d, &mut reader(8192, |_| None), &iso, &o, &[(4096, 2048)]).unwrap();
    let t = ticks.0.lock().unwrap();
    let scope = 2048 * 2048;
    assert!(
        t.iter().all(|p| p.work_total == scope),
        "the bar's total is the scope"
    );
    assert!(
        t[0].work_done <= 64 * 2048,
        "starts at 0, not the scope's disc offset: {}",
        t[0].work_done
    );
    assert_eq!(
        t.last().unwrap().work_done,
        scope,
        "a finished pass ends at 100%"
    );
}

// R8: a resumed sweep's first ticks (before any consumer snapshot) start from what the
// mapfile already holds, not from zero.
#[test]
fn a_resumed_sweep_s_first_tick_counts_the_prior_progress() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("resume.iso");
    let sectors = 8192u32;
    let total = sectors as u64 * 2048;
    let d = disc(sectors);
    std::fs::write(&iso, vec![0xAAu8; total as usize]).unwrap();
    {
        let mut m = mapfile::Mapfile::create(&d.mapfile_for(&iso), total, "t").unwrap();
        m.record(0, total / 2, mapfile::SectorStatus::Finished)
            .unwrap();
        m.flush().unwrap();
    }
    let ticks = Ticks::default();
    let mut o = opts(true, true);
    o.progress = Some(&ticks);
    sweep(&d, &mut reader(sectors, |_| None), &iso, &o).unwrap();
    let first = ticks.0.lock().unwrap()[0].clone();
    assert!(
        first.bytes_good_total >= total / 2,
        "first tick claims {} good; the mapfile already held {}",
        first.bytes_good_total,
        total / 2
    );
    assert!(
        first.bytes_pending_total <= total / 2,
        "pending {}",
        first.bytes_pending_total
    );
    assert!(
        first.work_done >= total / 2,
        "the bar restarts at 0: {}",
        first.work_done
    );
}

// R12: the reporter is a UI tick, throttled like patch's; not one call per batch.
#[test]
fn a_sweep_reports_progress_at_a_tick_not_per_batch() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("tick.iso");
    let sectors = 32 * 4000;
    let ticks = Ticks::default();
    let mut o = opts(false, true);
    o.progress = Some(&ticks);
    o.batch_sectors = Some(32);
    let t0 = std::time::Instant::now();
    sweep(&disc(sectors), &mut reader(sectors, |_| None), &iso, &o).unwrap();
    let bound = t0.elapsed().as_millis() as usize / 250 + 2;
    let n = ticks.0.lock().unwrap().len();
    assert!(
        n <= bound,
        "{n} reports for 4000 batches; a 250 ms tick allows {bound}"
    );
}

// R5: a staged image passes only when its title sectors were read. A staging sweep
// stopped part-way leaves them NonTried (zeros from `set_len`): refused like out-of-scope
// ones. Damage (read and failed) still passes: blank + warn + count, never fatal.
#[test]
fn a_staged_title_with_unread_sectors_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("staged.iso");
    let mut d = disc(8192);
    let mut title = libfreemkv::DiscTitle::empty();
    title.extents = vec![libfreemkv::disc::Extent {
        start_lba: 4096,
        sector_count: 1024,
    }];
    d.titles = vec![title];
    let total = 8192u64 * 2048;
    let (start, len) = (4096u64 * 2048, 1024u64 * 2048);
    let stage = |status: Option<mapfile::SectorStatus>| {
        let mut m = mapfile::Mapfile::create(&d.mapfile_for(&iso), total, "t").unwrap();
        m.set_scope(vec![(start, len)]);
        m.record(start, len / 2, mapfile::SectorStatus::Finished)
            .unwrap();
        if let Some(st) = status {
            m.record(start + len / 2, len / 2, st).unwrap();
        }
        m.flush().unwrap();
    };
    stage(None);
    assert!(
        matches!(
            ensure_titles_staged(&iso, &d, &[0]),
            Err(Error::ImageScoped { .. })
        ),
        "half the title was never read: muxing it would emit zeros"
    );
    stage(Some(mapfile::SectorStatus::NonTrimmed));
    ensure_titles_staged(&iso, &d, &[0]).expect("read-but-damaged sectors are not unread");
    stage(Some(mapfile::SectorStatus::Finished));
    ensure_titles_staged(&iso, &d, &[0]).expect("a fully staged title passes");
}
