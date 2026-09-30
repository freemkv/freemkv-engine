//! Stop design v5 §5.4 engine tests ET2-ET6 and ET18: the `_with` entries observe the op
//! token directly, through every read, pause and pass boundary.
//! Per spec; do not change without a spec citation proving otherwise.

mod common;

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
    let files = common::rs_files(&src);
    let test_only = test_only_modules(&files);
    let (mut hits, mut used) = (Vec::new(), [false; NOT_A_HALT.len()]);
    for path in &files {
        let rel = path
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if HALT_EXEMPT.contains(&rel.as_str()) || test_only.contains(path) {
            continue;
        }
        let text = std::fs::read_to_string(path).unwrap();
        for (line, receiver) in atomic_loads(&text) {
            let entry = (rel.as_str(), receiver.as_str());
            match NOT_A_HALT.iter().position(|e| *e == entry) {
                Some(i) => used[i] = true,
                None => hits.push(format!("{rel}:{line}: {receiver}.load(")),
            }
        }
    }
    assert!(
        hits.is_empty(),
        "raw atomic loads: poll EngineHalt::is_cancelled, or list a non-halt load in \
         NOT_A_HALT:\n{}",
        hits.join("\n")
    );
    let stale: Vec<_> = NOT_A_HALT.iter().zip(used).filter(|(_, u)| !u).collect();
    assert!(
        stale.is_empty(),
        "NOT_A_HALT entries no load matches: {stale:?}"
    );
}

// `EngineHalt` itself and the patch latch (§4.2) read the raw flag by design.
const HALT_EXEMPT: [&str; 2] = ["engine_halt.rs", "recovery/section_recover.rs"];

// Production atomic loads that are not a halt, as (file, receiver); list a new one here.
// remux.rs `quit` is the copy worker's give-up flag, set by its halt-aware watcher.
const NOT_A_HALT: [(&str, &str); 6] = [
    ("run.rs", "watcher_done"),
    ("mux.rs", "watcher_done"),
    ("image.rs", "help"),
    ("remux.rs", "moved"),
    ("remux.rs", "quit"),
    ("recovery/mapfile.rs", "disowned"),
];

// Files of the modules declared `#[cfg(test)] mod name;` anywhere under `src/`.
fn test_only_modules(files: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for path in files {
        let stem = path.file_stem().unwrap().to_string_lossy();
        let dir = path.parent().unwrap();
        let base = match stem.as_ref() {
            "mod" | "lib" | "main" => dir.to_path_buf(),
            s => dir.join(s),
        };
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        for w in lines.windows(2) {
            let decl = w[1].strip_prefix("mod ").and_then(|m| m.strip_suffix(';'));
            if let (true, Some(name)) = (w[0] == "#[cfg(test)]", decl) {
                out.push(base.join(format!("{name}.rs")));
                out.push(base.join(name).join("mod.rs"));
            }
        }
    }
    out
}

// (1-based line, receiver) of every `.load(` outside `#[cfg(test)]` items and `//` comments;
// a `.load(` that opens its line takes the receiver ending the code line above.
fn atomic_loads(text: &str) -> Vec<(usize, String)> {
    let ident = |s: &str| {
        let t = s.trim_end();
        let head = t.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
        t[head.len()..].to_string()
    };
    let (mut out, mut prev) = (Vec::new(), "");
    for (i, line) in production_lines(text) {
        let code = line.split("//").next().unwrap_or("");
        for (at, _) in code.match_indices(".load(") {
            let before = &code[..at];
            let receiver = if before.trim().is_empty() {
                ident(prev)
            } else {
                ident(before)
            };
            out.push((i + 1, receiver));
        }
        if !code.trim().is_empty() {
            prev = code;
        }
    }
    out
}

// The guard sees a load on any flag name and one split across lines; skips tests, comments.
#[test]
fn the_halt_guard_sees_any_flag_and_split_loads() {
    let text = "fn f() {
    if stop.load(Ordering::Relaxed) {}
    let x = self
        .flag
        .load(Ordering::SeqCst);
    // halt.load(Ordering::SeqCst)
}
#[cfg(test)]
mod tests {
    fn g() { halt.load(Ordering::SeqCst); }
}
";
    let found = atomic_loads(text);
    assert_eq!(found, [(2, "stop".to_string()), (5, "flag".to_string())]);
}

// The file's lines outside every `#[cfg(test)]` item (skipped by brace depth).
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let (mut out, mut skip, mut depth, mut armed) = (Vec::new(), false, 0i32, false);
    let mut in_str = false;
    for (i, line) in text.lines().enumerate() {
        let (opens, closes) = braces(line, &mut in_str);
        if !skip && line.trim_start().starts_with("#[cfg(test)]") {
            (skip, armed, depth) = (true, false, 0);
            continue;
        }
        if !skip {
            out.push((i, line));
            continue;
        }
        depth += opens - closes;
        armed |= opens > 0;
        if (armed && depth <= 0) || (!armed && line.trim_end().ends_with(';')) {
            skip = false;
        }
    }
    out
}

// `{` and `}` in code only: not in "strings" (which may span lines), 'c' chars or comments.
fn braces(line: &str, in_str: &mut bool) -> (i32, i32) {
    let (mut opens, mut closes) = (0, 0);
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' if *in_str => {
                it.next();
            }
            '"' => *in_str = !*in_str,
            _ if *in_str => {}
            '/' if it.peek() == Some(&'/') => break,
            '\'' => {
                let rest: String = it.clone().take(3).collect();
                if rest.starts_with('\\') {
                    it.nth(2);
                } else if rest.chars().nth(1) == Some('\'') {
                    it.nth(1);
                }
            }
            '{' => opens += 1,
            '}' => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

// The mapfile parses, its ranges tile the whole disc, and it keeps `good` sectors Finished:
// a Stop neither resets nor loses the recorded state (§5.4 ET4 "Asserts").
fn mapfile_is_sane(iso: &std::path::Path, sectors: u32, good: std::ops::RangeInclusive<u32>) {
    let map = Mapfile::load(&mapfile_path_for(iso)).expect("the mapfile parses");
    let total = sectors as u64 * SECTOR as u64;
    assert_eq!(map.total_size(), total);
    let all = [
        SectorStatus::NonTried,
        SectorStatus::NonTrimmed,
        SectorStatus::NonScraped,
        SectorStatus::Unreadable,
        SectorStatus::Finished,
    ];
    let end = map.ranges_with(&all).iter().fold(0, |at, &(pos, len)| {
        assert_eq!(pos, at, "a gap or overlap at byte {at}");
        pos + len
    });
    assert_eq!(end, total, "the ranges stop short of the disc");
    let len = std::fs::metadata(iso).map(|m| m.len()).unwrap_or(0);
    let s = map.stats();
    assert!(s.bytes_good <= len, "good {} > file {len}", s.bytes_good);
    let good_sectors = (s.bytes_good / SECTOR as u64) as u32;
    assert!(
        good.contains(&good_sectors),
        "{good_sectors} good sectors, want {good:?}"
    );
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
    mapfile_is_sane(&iso, 256, 0..=0);
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
    mapfile_is_sane(&iso, 4096, 512..=4096);
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
    mapfile_is_sane(&iso, 256, 0..=0);
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

fn patch_opts() -> PatchOptions<'static> {
    PatchOptions::for_patch_pass(false, None, None)
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
    mapfile_is_sane(&iso, 256, 192..=192);
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
    mapfile_is_sane(&iso, 256, 192..=256);
}

// ET6 `multipass_stop_between_passes`: a Stop that lands as the sweep ends runs no patch pass.
// Sectors 64..72 fail (a recovered-error sense: no 30 s zone-entry pause), so the pass has
// damage to retry and only the Stop keeps it from running.
#[test]
fn multipass_stop_between_passes() {
    const CAP: u32 = 2048;
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let mut reader = Script::new(CAP, 64..72);
    reader.sense_key = libfreemkv::scsi::SENSE_KEY_RECOVERED_ERROR;
    let (op, after, late) = (Halt::new(), reader.after.clone(), reader.late.clone());
    let hook_op = op.clone();
    reader.hook = Box::new(move |lba, count| {
        if lba + count as u32 >= CAP && !hook_op.is_cancelled() {
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
        &disc(CAP),
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
    assert!(
        r.pending_bytes > 0,
        "no damage left for a patch pass: {r:?}"
    );
}

// ET18 `remux_iso_signature_is_stable` (compile-time only) — §4.4: "`remux_iso(job: &RemuxJob, keys: &KeyParams,
// sink: &dyn Sink) -> io::Result<RemuxReport>` … **unchanged** signature and behaviour".
#[test]
fn remux_iso_signature_is_stable() {
    use freemkv_engine::{KeyParams, RemuxJob, RemuxReport, Sink};
    let legacy: fn(&RemuxJob, &KeyParams, &dyn Sink) -> std::io::Result<RemuxReport> =
        freemkv_engine::remux_iso;
    let with: fn(&RemuxJob, &KeyParams, &dyn Sink, &Halt) -> std::io::Result<RemuxReport> =
        freemkv_engine::remux_iso_with;
    let _ = (legacy, with);
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
    mapfile_is_sane(&iso, 256, 192..=192);
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
    let opts = PatchOptions::for_patch_pass(false, Some(&stop), None);
    let op = Halt::new();
    let t0 = Instant::now();
    let out = freemkv_engine::patch_with(&op, &disc(256), &mut reader, &iso, &opts);
    assert!(t0.elapsed() < STOP_LATENCY, "{:?}", t0.elapsed());
    assert!(!op.is_cancelled());
    let r = out.value().expect("an artifact result, not an error");
    assert!(r.halted && out.is_stopped(), "{out:?}");
    mapfile_is_sane(&iso, 256, 192..=192);
}
