//! Stop design v5 §5.4 engine tests ET2-ET6: the `_with` entries observe the op token
//! directly, through every read, pause and pass boundary.
//! Per spec; do not change without a spec citation proving otherwise.

use freemkv_engine::{
    CopyOptions, Halt, Job, Mapfile, MultipassOpts, NoopSink, PatchOptions, RipMode, SectorStatus,
    SweepOptions, mapfile_path_for,
};
use libfreemkv::disc::DiscRegion;
use libfreemkv::error::{Error, Result};
use libfreemkv::{ContentFormat, Disc, DiscFormat, SectorSource};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SECTOR: usize = 2048;

fn disc(sectors: u32) -> Disc {
    Disc {
        volume_id: String::new(),
        meta_title: None,
        format: DiscFormat::BluRay,
        capacity_sectors: sectors,
        capacity_bytes: sectors as u64 * SECTOR as u64,
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

// SPC-4 sense key 4h HARDWARE ERROR: the firmware-wedge family the 30 s pause follows;
// 3h MEDIUM ERROR: plain damage the patch handlers keep re-reading.
fn read_error(lba: u32, sense_key: u8) -> Error {
    Error::DiscRead {
        sector: lba as u64,
        status: Some(2),
        sense: Some(libfreemkv::ScsiSense {
            sense_key,
            asc: 0x44,
            ascq: 0x00,
        }),
    }
}

// Zeros everywhere but `bad` (a hardware error); counts reads, reads after `after` is set,
// and runs `hook` with the read's LBA first.
struct Script {
    cap: u32,
    bad: std::ops::Range<u32>,
    reads: Arc<AtomicU64>,
    after: Arc<AtomicBool>,
    late: Arc<AtomicU64>,
    hook: Box<dyn FnMut(u32, u16) + Send>,
    sense_key: u8,
}

impl Script {
    fn new(cap: u32, bad: std::ops::Range<u32>) -> Self {
        Self {
            cap,
            bad,
            reads: Arc::default(),
            after: Arc::default(),
            late: Arc::default(),
            hook: Box::new(|_, _| {}),
            sense_key: libfreemkv::scsi::SENSE_KEY_HARDWARE_ERROR,
        }
    }
}

impl SectorSource for Script {
    fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], _r: bool) -> Result<usize> {
        if self.after.load(Ordering::SeqCst) {
            self.late.fetch_add(1, Ordering::SeqCst);
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        (self.hook)(lba, count);
        if (lba..lba + count as u32).any(|l| self.bad.contains(&l)) {
            return Err(read_error(lba, self.sense_key));
        }
        let n = count as usize * SECTOR;
        buf[..n].fill(0);
        Ok(n)
    }
    fn capacity_sectors(&self) -> u32 {
        self.cap
    }
}

fn skip_on_error() -> SweepOptions<'static> {
    SweepOptions {
        skip_on_error: true,
        ..Default::default()
    }
}

// Cancel `op` from another thread after `after`.
fn cancel_later(op: &Halt, after: Duration) {
    let op = op.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        op.cancel();
    });
}

// §5.0 (B): "Tests assert **≤ 1 s** wall beyond the injected in-flight time."
const STOP_LATENCY: Duration = Duration::from_secs(1);

// ET2 `default_copy_options_sleep_is_cancellable` — §4.2: "`sleep_secs_or_halt`'s `None`
// arm … is deleted. This fixes the CLI-reachable 30 s uninterruptible sleep".
#[test]
fn default_copy_options_sleep_is_cancellable() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(64, 0..64);
    let op = Halt::new();
    let opts = CopyOptions {
        multipass: true,
        ..Default::default()
    };
    assert!(
        opts.halt.is_none(),
        "the CLI's options wire no narrower flag"
    );
    let delay = Duration::from_millis(300);
    cancel_later(&op, delay);
    let t0 = Instant::now();
    let out = freemkv_engine::copy_with(&op, &disc(64), &mut reader, &iso, &opts);
    let took = t0.elapsed();
    assert!(
        took < delay + STOP_LATENCY,
        "a Stop during the 30 s wedge pause took {took:?}"
    );
    assert!(out.is_stopped(), "{out:?}");
}

// ET3 `no_raw_halt_loads_in_production` — §4.2: "Every raw halt load becomes
// `EngineHalt::is_cancelled()`". The patch latch and `EngineHalt` itself are exempt.
#[test]
fn no_raw_halt_loads_in_production() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let exempt = ["engine_halt.rs", "section_recover.rs"];
    let mut hits = Vec::new();
    for path in rust_files(&src) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if exempt.contains(&name.as_str()) || name.contains("test") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (i, line) in production_lines(&text) {
            let raw = line.contains(".load(")
                && (line.contains("halt") || line.contains("h.load(") || line.contains("cancel"));
            if raw && !line.trim_start().starts_with("//") {
                hits.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(hits.is_empty(), "raw halt loads:\n{}", hits.join("\n"));
}

// The file's lines outside every `#[cfg(test)]` item (skipped by brace depth).
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let (mut out, mut skip, mut depth, mut armed) = (Vec::new(), false, 0i32, false);
    for (i, line) in text.lines().enumerate() {
        if !skip && line.trim_start().starts_with("#[cfg(test)]") {
            (skip, armed, depth) = (true, false, 0);
            continue;
        }
        if !skip {
            out.push((i, line));
            continue;
        }
        let opens = line.matches('{').count() as i32;
        depth += opens - line.matches('}').count() as i32;
        armed |= opens > 0;
        if (armed && depth <= 0) || (!armed && line.trim_end().ends_with(';')) {
            skip = false;
        }
    }
    out
}

fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(rust_files(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}

// The mapfile parses and describes no more than the image holds (§5.4 ET4 "Asserts").
fn mapfile_is_sane(iso: &std::path::Path) {
    let map = Mapfile::load(&mapfile_path_for(iso)).expect("the mapfile parses");
    let len = std::fs::metadata(iso).map(|m| m.len()).unwrap_or(0);
    let s = map.stats();
    assert!(s.bytes_good <= len, "good {} > file {len}", s.bytes_good);
}

// ET4 `sweep_stop_at_each_phase`, case "before the first READ".
#[test]
fn sweep_stop_before_the_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(256, 0..0);
    let reads = reader.reads.clone();
    let op = Halt::new();
    op.cancel();
    let out = freemkv_engine::sweep_with(&op, &disc(256), &mut reader, &iso, &skip_on_error());
    assert!(out.is_stopped(), "{out:?}");
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no READ after a Stop");
    mapfile_is_sane(&iso);
}

// ET4, case "mid-READ": the READ in flight completes, then no further READ is issued.
#[test]
fn sweep_stop_mid_read() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(4096, 0..0);
    let (op, after, late) = (Halt::new(), reader.after.clone(), reader.late.clone());
    let hook_op = op.clone();
    reader.hook = Box::new(move |lba, _| {
        if lba >= 512 && !hook_op.is_cancelled() {
            hook_op.cancel();
            after.store(true, Ordering::SeqCst);
        }
    });
    let out = freemkv_engine::sweep_with(&op, &disc(4096), &mut reader, &iso, &skip_on_error());
    assert!(out.is_stopped(), "{out:?}");
    assert_eq!(late.load(Ordering::SeqCst), 0, "a READ after the Stop");
    let r = out.value().expect("a sweep returns its partial result");
    assert!(r.halted && !r.complete);
    mapfile_is_sane(&iso);
}

// ET4, case "during the error pause".
#[test]
fn sweep_stop_during_the_error_pause() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(256, 0..256);
    let op = Halt::new();
    let delay = Duration::from_millis(300);
    cancel_later(&op, delay);
    let t0 = Instant::now();
    let out = freemkv_engine::sweep_with(&op, &disc(256), &mut reader, &iso, &skip_on_error());
    assert!(t0.elapsed() < delay + STOP_LATENCY, "{:?}", t0.elapsed());
    assert!(out.is_stopped(), "{out:?}");
    mapfile_is_sane(&iso);
}

// A swept image whose mapfile leaves sectors 64..128 NonTrimmed, for the patch cases.
fn swept_with_damage(iso: &std::path::Path) {
    let total = 256 * SECTOR as u64;
    std::fs::write(iso, vec![0u8; total as usize]).unwrap();
    let mut map = Mapfile::create(&mapfile_path_for(iso), total, "t").unwrap();
    map.record(0, total, SectorStatus::Finished).unwrap();
    let (pos, len) = (64 * SECTOR as u64, 64 * SECTOR as u64);
    map.record(pos, len, SectorStatus::NonTrimmed).unwrap();
    map.flush().unwrap();
}

// ET5 `patch_stop_at_each_phase`, case "before the first READ".
#[test]
fn patch_stop_before_the_first_read() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    swept_with_damage(&iso);
    let mut reader = Script::new(256, 0..0);
    let reads = reader.reads.clone();
    let op = Halt::new();
    op.cancel();
    let out = freemkv_engine::patch_with(&op, &disc(256), &mut reader, &iso, &patch_opts());
    assert!(out.is_stopped(), "{out:?}");
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no READ after a Stop");
    mapfile_is_sane(&iso);
}

// ET5, case "mid-READ": a Stop inside a handler ends the pass, not the handler's budget.
#[test]
fn patch_stop_mid_read() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    swept_with_damage(&iso);
    let mut reader = Script::new(256, 0..0);
    let (op, after, late) = (Halt::new(), reader.after.clone(), reader.late.clone());
    let hook_op = op.clone();
    reader.hook = Box::new(move |_, _| {
        if !hook_op.is_cancelled() {
            hook_op.cancel();
            after.store(true, Ordering::SeqCst);
        }
    });
    let t0 = Instant::now();
    let out = freemkv_engine::patch_with(&op, &disc(256), &mut reader, &iso, &patch_opts());
    assert!(t0.elapsed() < STOP_LATENCY, "{:?}", t0.elapsed());
    assert!(out.is_stopped(), "{out:?}");
    assert_eq!(late.load(Ordering::SeqCst), 0, "a READ after the Stop");
    mapfile_is_sane(&iso);
}

fn patch_opts() -> PatchOptions<'static> {
    PatchOptions::for_patch_pass(false, None, None, None)
}

// ET6 `multipass_stop_between_passes`: a Stop that lands as the sweep ends runs no patch pass.
#[test]
fn multipass_stop_between_passes() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(512, 0..0);
    let (op, after, late) = (Halt::new(), reader.after.clone(), reader.late.clone());
    let hook_op = op.clone();
    reader.hook = Box::new(move |lba, count| {
        if lba + count as u32 >= 512 && !hook_op.is_cancelled() {
            hook_op.cancel();
            after.store(true, Ordering::SeqCst);
        }
    });
    let job = Job {
        mode: RipMode::Multi,
        raw: true,
        ..Job::new("iso://d.iso", "d.iso")
    };
    let opts = MultipassOpts {
        max_passes: 3,
        abort_on_lost_secs: 0,
        is_iso_output: true,
    };
    let out = freemkv_engine::multipass_rip_with(
        &op,
        &disc(512),
        &mut reader,
        &iso,
        &job,
        &opts,
        &NoopSink,
    );
    assert!(out.is_stopped(), "{out:?}");
    assert_eq!(
        late.load(Ordering::SeqCst),
        0,
        "a patch pass READ after the Stop"
    );
    let r = out.value().expect("the partial result");
    assert!(r.halted && r.passes == 1, "{r:?}");
}

// ET18 `remux_iso_signature_is_stable` — §4.4: "`remux_iso(job: &RemuxJob, keys: &KeyParams,
// sink: &dyn Sink) -> io::Result<RemuxReport>` … **unchanged** signature and behaviour".
#[test]
fn remux_iso_signature_is_stable() {
    use freemkv_engine::{KeyParams, RemuxJob, RemuxReport, Sink};
    let legacy: fn(&RemuxJob, &KeyParams, &dyn Sink) -> std::io::Result<RemuxReport> =
        freemkv_engine::remux_iso;
    let with: fn(&RemuxJob, &KeyParams, &dyn Sink, &Halt) -> std::io::Result<RemuxReport> =
        freemkv_engine::remux_iso_with;
    let _ = (legacy, with);
    // `Sink::should_cancel` stays a defaulted cancel input (§4.4, `sink.rs`).
    assert!(!NoopSink.should_cancel());
}

// ET5, case "during the error pause": every READ fails slowly, so the handlers grind their
// per-handler budgets (60 s each, §3.1 "INTENTIONAL"); a Stop ends the pass, not the budget.
#[test]
fn patch_stop_during_the_error_pause() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    swept_with_damage(&iso);
    let mut reader = Script::new(256, 0..256);
    reader.sense_key = libfreemkv::scsi::SENSE_KEY_MEDIUM_ERROR;
    // A damaged drive answers slowly: without a Stop this pass grinds for many seconds.
    reader.hook = Box::new(|_, _| std::thread::sleep(Duration::from_millis(20)));
    let op = Halt::new();
    let delay = Duration::from_millis(300);
    cancel_later(&op, delay);
    let t0 = Instant::now();
    let out = freemkv_engine::patch_with(&op, &disc(256), &mut reader, &iso, &patch_opts());
    assert!(t0.elapsed() < delay + STOP_LATENCY, "{:?}", t0.elapsed());
    assert!(out.is_stopped(), "{out:?}");
    mapfile_is_sane(&iso);
}

// ET5 "(+ the latch is exempt)" — §4.2: "The patch latch is exempt": a Stop that reaches the
// pass only through its progress reporter (no token cancelled) still ends it as halted.
#[test]
fn patch_latch_is_exempt() {
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    swept_with_damage(&iso);
    let mut reader = Script::new(256, 0..256);
    let stop = |_: &libfreemkv::progress::PassProgress| false;
    let opts = PatchOptions::for_patch_pass(false, Some(&stop), None, None);
    let op = Halt::new();
    let t0 = Instant::now();
    let out = freemkv_engine::patch_with(&op, &disc(256), &mut reader, &iso, &opts);
    assert!(t0.elapsed() < STOP_LATENCY, "{:?}", t0.elapsed());
    assert!(!op.is_cancelled());
    let r = out.value().expect("an artifact result, not an error");
    assert!(r.halted && out.is_stopped(), "{out:?}");
    mapfile_is_sane(&iso);
}
