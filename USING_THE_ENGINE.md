# Using `freemkv-engine` (for the desktop UI)

`freemkv-engine` is the shared rip layer between `libfreemkv` (SCSI, parse,
decrypt, mux, raw reads) and the front-ends. **The UI depends on the engine
alone** — you do not need a direct `libfreemkv` dependency for the disc model
or cancellation; the engine re-exports what you need.

```
libfreemkv        ← primitives (unchanged)
freemkv-engine    ← THIS: recovery strategy + rip orchestration + the Sink seam
   └── freemkv-gui ← you
```

Status: the engine is built, green, and tested on Rust 1.97 (the toolchain CI
pins and `Cargo.toml`'s `rust-version`). It is
**off crates.io** — depend on it by path/git tag like the other freemkv crates.

```toml
[dependencies]
freemkv-engine = "1.6"   # + the same [patch.crates-io] git-tag redirect the
                         # other crates use; local dev path-patches it.
```

---

## The one rule: everything goes through the `Sink`

The engine **never prints and never blocks on the UI thread**. Every diagnostic,
progress tick, and completion is delivered to a `Sink` you implement. Cancellation
is a `Sink` method the engine polls. Implement it once:

```rust
use freemkv_engine::{Sink, Level, Progress, Outcome, DiscTitle};
use std::sync::atomic::{AtomicBool, Ordering};

struct UiSink {
    cancel: AtomicBool,
    // ... channels / handles to marshal onto your UI thread ...
}

impl Sink for UiSink {
    fn log(&self, level: Level, msg: &str) {
        // Route to your log pane. `msg` is engine/English; library errors
        // arrive as codes you localize (see "Errors" below).
    }
    fn title_opened(&self, t: &DiscTitle) {
        // Reserved for the future combined run(); NOT called by recover_to_iso/
        // multipass_rip today. Populate your tree from `disc.titles` after scan.
    }
    fn progress(&self, p: &Progress) {
        // Called frequently during recovery. Marshal to the UI thread; keep cheap.
        // p.pass, p.bytes_done/bytes_total, p.sectors_bad,
        // p.speed_bps, p.eta_secs  ← all DERIVED BY THE ENGINE.
    }
    fn completed(&self, outcome: &Outcome) {
        // Reserved for the future combined run(); NOT called by recover_to_iso/
        // multipass_rip today. For now, build your result page from the
        // MultipassResult these return (see below).
    }
    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)   // your Cancel button sets this
    }
}
```

Every method has a default no-op, so implement only what you render. The trait
is `Send + Sync` and object-safe (the engine takes `&dyn Sink`).

**Do not recompute progress.** `p.speed_bps` and `p.eta_secs` are computed once
by the engine. Format them however you like (`"1:23"` vs `"0:01:23"`) but never
re-derive them from byte deltas — that's the exact drift class the engine exists
to kill.

---

## The public API

Import everything from the crate root:

```rust
use freemkv_engine::{
    // request + result (pure data)
    Job, RipMode, Selection, Outcome, RipFile, KeyStatus, DamageSeverity,
    // validate without executing
    preflight, Preflight, Reason,
    // key status as data
    resolve_keys,
    // the seam
    Sink, Level, Progress, NoopSink,
    // recovery / multipass
    recover_to_iso, multipass_rip, MultipassOpts, MultipassResult,
    classify_damage, loss_aborts, effective_abort_secs,
    // re-exported disc model (no direct libfreemkv dep needed)
    Disc, DiscTitle, DiscFormat, Codec, Resolution, VideoStream, AudioStream,
    SubtitleStream, Stream, Halt,
    Result,   // = std::result::Result<T, libfreemkv::Error>
};
```

### 1. Build a `Job` (the request, pure data)

```rust
let job = Job::new("iso:///path/to/disc.iso", "/output/dir")
    .with_mode(RipMode::Multi)              // Single = one pass; Multi = sweep+patch+abort-gate
    .with_selection(Selection::MainMovie);  // MainMovie | All | Longest | Titles(vec![0,2])
// fields you can also set: job.raw (ciphertext passthrough),
// opts.abort_on_lost_secs (Multi only; 0 = require perfect rip).
```

### 2. `preflight(&Disc, &Job) -> Preflight` — keep Start honest

Pure and side-effect free. Call it on **every selection change** to grey out the
Start button and say why. Never touches a drive or disk.

```rust
match preflight(&disc, &job) {
    Preflight::Ready => { /* enable Start */ }
    Preflight::Blocked(reasons) => {
        for r in &reasons {
            // r.key is a STABLE identifier you localize (never English).
            // The COMPLETE set the engine emits — map all six, or a
            // blocked Start renders with no explanation:
            //   "no-titles" | "empty-selection" | "title-out-of-range"
            //   | "multipass-requires-raw" | "language-unmatched"
            //   | "encrypted-no-key"
            // "language-unmatched" means a language-filtered stream class the
            // job asked for is carried by no selected title; r.detail is the
            // class key ("audio", "subtitle", "subtitle_forced").
            // "multipass-requires-raw" is the one §1's own example above
            // triggers: RipMode::Multi without job.raw is refused, because a
            // multipass rip is a whole-disc image recovery and decryption has
            // no place in that loop. Set job.raw for Multi.
            // r.detail is an optional machine value (e.g. the bad index).
        }
    }
}
```

### 3. `resolve_keys(&Disc) -> KeyStatus` — the keydb strip, as data

```rust
let ks = resolve_keys(&disc);
// ks.resolved: bool
// ks.origin:   Option<KeyOrigin>   (where the key came from)
// ks.summary:  stable key you localize. The COMPLETE set the engine emits —
//              map all ten, or the strip renders a raw key at the user:
//   "unencrypted" | "resolved-keydb" | "resolved-external" | "resolved-derived"
//   | "resolved-css" | "no-key" | "no-keydb"   ← render the red "no KEYDB.cfg"
//                                                 strip on "no-keydb"
//   | "key-service-unavailable" | "key-service-unauthorized"
//   | "key-service-rate-limited"
// "resolved-external" is source-agnostic (an externally supplied unit key —
// not necessarily online); "resolved-derived" means the key was derived from
// device/processing keys. There is no "resolved-online".
// The three "key-service-*" values are NOT "no-key": a key SOURCE failed to
// answer, so nothing was learned about whether this disc has a key. The user
// action is retry / fix the token / back off — not "go find a VUK" — so give
// them their own message rather than folding them into the no-key case.
```

No scraping logs for key state — it's here as data.

### 4. Recover + report

Two entry points, matching the two rip modes:

```rust
// Single pass (RipMode::Single): one read, no retries.
let copy_result = recover_to_iso(&disc, &mut reader, iso_path, &job, &sink)?;

// Multipass (RipMode::Multi): sweep -> N patch passes -> abort-on-loss gate.
let mp: MultipassResult = multipass_rip(
    &disc, &mut reader, iso_path, &job,
    &MultipassOpts {
        max_passes: 5,
        abort_on_lost_secs: 0,   // 0 = require a perfect rip
        is_iso_output: false,    // true forces 100% (an ISO backup ignores the tolerance)
    },
    &sink,
)?;
// mp.unreadable_bytes, mp.pending_bytes, mp.good_bytes,
// mp.main_lost_ms (NaN = unquantifiable; always scoped to the MAIN TITLE's
//                  extents, even when is_iso_output widens the abort gate to
//                  the whole disc), mp.severity (DamageSeverity),
// mp.passes, mp.aborted_for_loss, mp.halted,
// mp.wedged, mp.complete
```

Three of those decide which result page you show, and they are not
interchangeable:

* `mp.complete` — the rip actually finished (nothing unreadable, nothing
  pending, not halted, not aborted). This is the ONLY "done" test; a run that
  merely used up `max_passes` is not complete.
* `mp.halted` — the user pressed Stop. Resume from the mapfile whenever they
  ask.
* `mp.wedged` — a pass ended on a TRANSPORT FAULT (the USB-bridge / firmware
  crash). NOT a user Stop and NOT permanent damage: the ranges that pass never
  reached are still retryable. Tell the operator to power-cycle the drive, then
  resume from the mapfile. Without this field a bridge crash is
  indistinguishable from an ordinary partial rip — `halted` and
  `aborted_for_loss` are both false and the byte counts look unremarkable.

`reader` is a `&mut dyn libfreemkv::SectorSource`. `scan_iso` hands back a
`Box<dyn SectorSource>`, so reborrow it through the box: `&mut *reader`. For a
live drive you get the reader from a `libfreemkv::DiscSession` (see "Scanning a
disc" below). Progress flows through your `sink` the whole time;
`sink.should_cancel()` stops it (same mechanism as Ctrl-C in the CLI).

### 5. Severity for the result badge

```rust
let sev = classify_damage(bad_sectors, lost_ms); // Clean | Cosmetic | Moderate | Serious
```

---

## Scanning a disc (to get the `Disc` + reader)

Scanning is a `libfreemkv` primitive; the engine composes it but doesn't wrap it
yet. For the UI:

```rust
// ISO source (fully synthetic-testable, no drive):
let (disc, mut reader) = libfreemkv::scan_iso(path, libfreemkv::ScanOptions::default())?;

// Live drive:
let mut session = libfreemkv::DiscSession::open(target, key_spec)?;
session.scan(libfreemkv::ScanOptions::default())?;
let disc = session.disc().unwrap();
// stage the drive as the reader for recover_to_iso via session.take_reader()
```

`disc.titles` is your tree source. Each `DiscTitle` carries the video/audio/
subtitle streams and metadata (codec, resolution, duration, size) — the Info
panel fields. All `Disc` fields are public.

> The ISO→MKV **mux** stage (turning the recovered ISO into the final MKV) is
> currently driven via `libfreemkv::mux_stream` directly; a single engine
> `run()` that chains recover→mux is the next addition. For a first UI, drive
> recovery through the engine and mux via `libfreemkv::mux_stream` (or target an
> ISO and skip mux). Ask if you want the combined entry point prioritized.

---

## Errors

`freemkv_engine::Result<T>` is `Result<T, libfreemkv::Error>`. `Error` is a typed
enum with a numeric `.code()` and **no English text** — map codes to your
localized strings, exactly as the CLI does. The sentinel for a missing keydb is
surfaced as the `"no-keydb"` `KeyStatus` summary (don't string-match the error
for that case — use `resolve_keys`).

---

## What NOT to do

- **Don't print.** Nothing in the engine writes to stdout/stderr; neither should
  your Sink impl in a GUI (route to the log pane).
- **Don't recompute speed/ETA** — use `Progress.speed_bps` / `.eta_secs`.
- **Don't add a direct `libfreemkv` dep for the disc model** — the engine
  re-exports `Disc`/`DiscTitle`/stream types/`Halt`. (You still touch
  `libfreemkv` directly for `scan_iso`/`DiscSession`/`mux_stream` until the
  combined `run()` lands.)
- **Don't call recovery on the UI thread** — it blocks; run it on a worker and
  marshal `Sink` callbacks back.

---

## Minimal end-to-end shape

```rust
let (disc, mut reader) = libfreemkv::scan_iso(path, Default::default())?;  // reader: Box<dyn SectorSource>
let job = Job::new(src, dst).with_mode(RipMode::Multi);
if let Preflight::Blocked(reasons) = preflight(&disc, &job) {
    return show_blocked(reasons);
}
let key = resolve_keys(&disc);
update_keydb_strip(key);
let sink = UiSink::new();                // your impl
std::thread::spawn(move || {
    let mp = multipass_rip(&disc, &mut *reader, iso_path, &job, &opts, &sink);
    // Progress fired through the sink during the run. `mp` is the terminal
    // result — build your result page from it (or call your own
    // sink.completed(...) with a mapped Outcome). Handle Err for hard errors.
});
```

That's the whole contract: build a `Job`, `preflight` it, show `resolve_keys`
state, run recovery with your `Sink`, render the `Outcome`.
