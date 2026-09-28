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

Status: the engine is built, green, and tested at its MSRV floor of Rust 1.88
(declared once in `Cargo.toml`'s `rust-version`; CI reads it from there rather
than pinning a hardcoded version, and lints on stable). It is
**off crates.io** — depend on it by path/git tag like the other freemkv crates.

```toml
[dependencies]
freemkv-engine = "1.6"

# REQUIRED. freemkv-engine, libfreemkv, freemkv-keysources and freemkv-i18n are
# NOT published on crates.io — they are consumed by git tag. This block must live
# in YOUR OWN workspace-root manifest: Cargo only honours [patch] in the root of
# the crate being built, so the patch table committed in a dependency's Cargo.toml
# redirects nothing for you. Keep all four tags on the SAME version.
[patch.crates-io]
libfreemkv         = { git = "https://github.com/freemkv/libfreemkv",         tag = "v1.6.14" }
freemkv-engine     = { git = "https://github.com/freemkv/freemkv-engine",     tag = "v1.6.14" }
freemkv-keysources = { git = "https://github.com/freemkv/freemkv-keysources", tag = "v1.6.14" }
freemkv-i18n       = { git = "https://github.com/freemkv/freemkv-i18n",       tag = "v1.6.14" }
```

> **Heads-up on the `libfreemkv` name:** an old `libfreemkv` **1.1.0** still sits
> on crates.io. It is unmaintained and unrelated to current releases. Without the
> `[patch.crates-io]` above, `libfreemkv = "1.6"` fails to resolve and
> `libfreemkv = "1"` silently pulls that abandoned 1.1.0. The patch block is what
> makes the git-tag source authoritative — do not omit it.

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
    // the rip's keys, up front (keys-upfront KU §2.1)
    keys::{resolve_for_rip, key_status, key_source_factory, rip_scope, RipOutput, KeyParams},
    // the seam
    Sink, Level, Progress, NoopSink,
    // recovery / multipass
    recover_to_iso, multipass_rip, MultipassOpts, MultipassResult,
    multipass_rip_staged, mkv_staging_scope, ensure_whole_image, ensure_titles_staged,
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

### 3. `keys::resolve_for_rip` — the rip's keys, once, before any output

```rust
let sources = key_source_factory(&KeyParams { keydb_path, key_url, ..Default::default() });
let scope = rip_scope(&disc, &titles, RipOutput::DecryptedImage);
let set = resolve_for_rip(&disc, &mut *reader, scope, &sources, None, Some(&halt))?;
// Err refuses the rip before any output: map its code (E7022/E7032 no key,
// E7026 forensic keys pending, E7028–30 the key service, E7013, Halted).
let status = key_status(&disc, &set);    // Ready | AacsKeysMissing | ForensicPending | …
let job = job.with_keys(set);            // every pass and gate reads this set, nothing else
```

The set lives in memory only. Without one, a decrypting rip of an AACS disc
refuses (E7022) whatever keys the scan found on the disc.

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

**MKV through a staged image.** When the deliverable is MKV (a multipass MKV
rip that stages an image first), stage only what the mux needs:

```rust
let scope = mkv_staging_scope(&disc, &mut *reader, &title_indices, keep_iso)?;
let mp = multipass_rip_staged(&disc, &mut *reader, iso_path, &job, &opts,
                              scope.as_deref(), &sink)?;
if scope.is_some() {
    // Not a whole-disc image: delete it after a successful mux, even with keep_iso.
}
```

`title_indices` index `disc.titles` of THIS scan. With a scope the passes read
only UDF, nav/AACS files and those titles (none of it bus-encrypted), so a disc
whose drive could not locate a bus-encrypted stream file still rips; the plain
`multipass_rip` refuses such a disc with E6021. The mapfile records the scope:
`copy`/`recover_to_iso` over it refuses (E6021) until every stream file is
located, then fills the rest. Before using an image as a whole-disc source
(`iso://` → `iso://`, `dir://`), call `ensure_whole_image(path)`: it returns
E6022 for a staged image. Before muxing titles from an image, call
`ensure_titles_staged(path, &disc, &indices)`: it returns E6022 when a staged
image's scope does not hold every sector of those titles.

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
localized strings, exactly as the CLI does. A key refusal (no key, no keydb, a
key service that failed) comes back typed from `keys::resolve_for_rip`; map its
code, don't string-match the error.

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
let set = resolve_for_rip(&disc, &mut *reader, scope, &sources, None, None)?;
update_keydb_strip(key_status(&disc, &set));
let job = Job::new(src, dst).with_mode(RipMode::Multi).with_keys(set);
if let Preflight::Blocked(reasons) = preflight(&disc, &job) {
    return show_blocked(reasons);
}
let sink = UiSink::new();                // your impl
std::thread::spawn(move || {
    let mp = multipass_rip(&disc, &mut *reader, iso_path, &job, &opts, &sink);
    // Progress fired through the sink during the run. `mp` is the terminal
    // result — build your result page from it (or call your own
    // sink.completed(...) with a mapped Outcome). Handle Err for hard errors.
});
```

That's the whole contract: build a `Job`, resolve its keys once and show
`key_status`, `preflight` it, run recovery with your `Sink`, render the `Outcome`.
