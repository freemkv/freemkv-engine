# Using `freemkv-engine` (for the desktop UI)

`freemkv-engine` is the shared rip layer between `libfreemkv` (SCSI, parse,
decrypt, mux, raw reads) and the front-ends. **The UI needs no direct
libfreemkv dependency for the disc model and cancellation**: the engine
re-exports them. A few types are not re-exported (`DeviceTarget`,
`DriveCredentials`, `SectorSource`, `keys::ResolvedKeySet`, `DecryptStatus`), so
add `libfreemkv = "1.7"` (with the same `[patch.crates-io]` block) to name them.

```
libfreemkv        ← primitives (unchanged)
freemkv-engine    ← THIS: recovery strategy + rip orchestration + the Sink seam
   └── freemkv-gui ← you
```

Status: the engine is built, green, and tested at its MSRV floor of Rust 1.88
(declared once in `Cargo.toml`'s `rust-version`; CI reads it from there rather
than pinning a hardcoded version, and lints on stable). It is
**off crates.io** — depend on it by git tag like the other freemkv crates.

```toml
[dependencies]
freemkv-engine = "1.7"

# REQUIRED. freemkv-engine, libfreemkv and freemkv-keysources are NOT published
# on crates.io — they are consumed by git tag. This block must live in YOUR OWN
# workspace-root manifest: Cargo only honours [patch] in the root of the crate
# being built, so the engine's own patch table redirects nothing for you. Use
# the tag of the engine version you pin (v1.7.7 here) for every entry; add
# freemkv-i18n at the same tag if you depend on it.
[patch.crates-io]
libfreemkv         = { git = "https://github.com/freemkv/libfreemkv",         tag = "v1.7.7" }
freemkv-engine     = { git = "https://github.com/freemkv/freemkv-engine",     tag = "v1.7.7" }
freemkv-keysources = { git = "https://github.com/freemkv/freemkv-keysources", tag = "v1.7.7" }
```

> **Heads-up on the `libfreemkv` name:** an old `libfreemkv` **1.1.0** still sits
> on crates.io. It is unmaintained and unrelated to current releases. Without the
> `[patch.crates-io]` above, `libfreemkv = "1.7"` fails to resolve and
> `libfreemkv = "1"` silently pulls that abandoned 1.1.0. The patch block is what
> makes the git-tag source authoritative — do not omit it.

---

## The one rule: everything goes through the `Sink`

The engine **never prints and never blocks on the UI thread**. Every diagnostic,
progress tick, and milestone is delivered to a `Sink` you implement. Cancellation
is a `Sink` method the engine polls. Implement it once:

```rust
use freemkv_engine::{Sink, Level, Progress, Event, DiscTitle};
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
        // Called by remux_iso* with the title it is about to mux. Recovery never
        // calls it: populate your tree from `disc.titles` after the scan.
    }
    fn progress(&self, p: &Progress) {
        // Called frequently. Marshal to the UI thread; keep cheap.
        // p.pass: "sweep" | "patch-scrape" | "patch-trim" | "mux" | "sync"
        //        | "verify" | "copy" (remux)  — stable keys you localize,
        // p.bytes_done/bytes_total, p.sectors_bad,
        // p.speed_bps, p.eta_secs  ← all DERIVED BY THE ENGINE.
    }
    fn event(&self, e: &Event<'_>) {
        // Typed milestones from the mux/remux paths: Phase, TitleStart,
        // TitleDone (the MuxOutcome or error), Verify, Replaced, OutputOpened.
        // `Event` is #[non_exhaustive]: keep a `_ => {}` arm.
    }
    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)   // your Cancel button sets this
    }
}
```

Every method has a default no-op, so implement only what you render. The trait
is `Send + Sync` and object-safe (the engine takes `&dyn Sink`).
`Sink::completed` exists but the engine never calls it today: build your result
page from the value each entry point returns.

**Do not recompute progress.** `p.speed_bps` and `p.eta_secs` are computed once
by the engine. Format them however you like (`"1:23"` vs `"0:01:23"`) but never
re-derive them from byte deltas — that's the exact drift class the engine exists
to kill.

---

## The public API

Import from the crate root (the items this guide uses; see `src/lib.rs` for the
full list):

```rust
use freemkv_engine::{
    // request (pure data)
    Job, RipMode, Selection, StreamChoice, StreamFilter,
    // validate without executing
    preflight, Preflight, Reason,
    // the rip's keys, up front (keys-upfront KU §2.1)
    keys::{resolve_for_rip, key_status, key_source_factory, rip_scope, RipOutput, KeyParams},
    // the seam
    Sink, Level, Progress, Event, NoopSink,
    // scanning
    open_scan, scan_image, ImageSource,
    // recovery / multipass
    recover_to_iso, multipass_rip, MultipassOpts, MultipassResult,
    multipass_rip_staged, mkv_staging_scope, ensure_whole_image, ensure_titles_staged,
    classify_damage, DamageSeverity,
    // muxing / remuxing
    open_image_with, OpenImageOptions, KeyInput, OpenedImage,
    mux_image_titles, MuxPlan, RipOutcome, resolve_selection,
    remux_iso, RemuxJob, RemuxReport,
    // re-exported disc model (no direct libfreemkv dep needed for these)
    Disc, DiscTitle, DiscFormat, Codec, Resolution, VideoStream, AudioStream,
    SubtitleStream, Stream, Halt,
    Result,   // = std::result::Result<T, libfreemkv::Error>
};
```

The `*_with` variants add a `Halt` op token as a second cancel input next to
`Sink::should_cancel`: `mux_image_titles_with` and `remux_iso_with` take it as
the LAST argument (same return types as the plain fns); `multipass_rip_with`
takes it FIRST and returns `EngineOutcome<MultipassResult>`, not a `Result`.
`OpenImageOptions::halt` does the same for the image open. (Import them by
name; they are not in the list above.)

### 1. Build a `Job` (the request, pure data)

```rust
let mut job = Job::new("iso:///path/to/disc.iso", "/output/dir")
    .with_mode(RipMode::Multi)      // Single = one pass; Multi = sweep+patch+abort-gate
    .with_selection(Selection::MainMovie);
    // MainMovie | All | Longest | Titles(vec![0,2]) | Episodes (a TV disc's episode set)
job.raw = true;                     // Multi requires raw (see preflight below)
// Also: .with_audio / .with_subtitles / .with_streams (StreamChoice) and .with_keys.
// The loss tolerance lives on MultipassOpts, not the Job.
```

`resolve_selection(&disc, &job.selection)` turns the selection into concrete
0-based title indices.

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
            // "language-unmatched": a language-filtered stream class the job
            // asked for is carried by no selected title; r.detail is the class
            // key ("audio", "subtitle", "subtitle_forced").
            // "multipass-requires-raw": RipMode::Multi without job.raw. A
            // multipass rip is whole-disc image recovery; decryption happens
            // later, in the mux from that image.
            // r.detail is an optional machine value (e.g. the bad index).
        }
    }
}
```

### 3. `keys::resolve_for_rip` — the rip's keys, once, before any output

```rust
let sources = key_source_factory(&KeyParams { keydb_path, key_url, ..Default::default() });
let scope = rip_scope(&disc, &titles, RipOutput::Streams);  // or DecryptedImage / RawImage
let set = resolve_for_rip(&disc, &mut *reader, scope, &sources, None, Some(&halt))?;
// Err refuses the rip before any output: map its code (E7022/E7032 no key,
// E7026 forensic keys pending, E7028–30 the key service, E7013, Halted).
let status = key_status(&disc, &set);  // libfreemkv DecryptStatus: Ready | AacsKeysMissing | ForensicPending | …
```

The set lives in memory only. Hand it to a decrypting job with
`job.with_keys(set)`, or to the image mux (§5). Without one, a decrypting rip of
an AACS disc refuses (E7022) whatever keys the scan found on the disc.

### 4. Recover + report

Two entry points, matching the two rip modes:

```rust
// Single pass (RipMode::Single): one read, no retries.
let copy_result = recover_to_iso(&disc, &mut *reader, iso_path, &job, &sink)?;

// Multipass (RipMode::Multi, job.raw): sweep -> N patch passes -> abort-on-loss gate.
let mp: MultipassResult = multipass_rip(
    &disc, &mut *reader, iso_path, &job,
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

`reader` is a `&mut dyn libfreemkv::SectorSource`; the scan hands back a
`Box<dyn SectorSource>`, so reborrow it through the box: `&mut *reader`.
Progress flows through your `sink` the whole time; `sink.should_cancel()` stops
it (same mechanism as Ctrl-C in the CLI).

Classify the damage for the result badge with
`classify_damage(bad_sectors, lost_ms)` → `Clean | Cosmetic | Moderate | Serious`
(`mp.severity` already carries it for a multipass rip).

### 5. Mux the image to MKV

```rust
let opened = open_image_with(
    &ImageSource::Iso(iso_path.into()),
    OpenImageOptions { disc: Some(disc), ..OpenImageOptions::known(set) },  // no rescan
)?;
let plan = MuxPlan::new(title_indices);            // or fill streams / explicit_selection
let outcome = mux_image_titles(&opened, &plan, &|idx| dest_url(idx), &sink);
// RipOutcome::Ok { titles_written } (0 written is NOT a rip) | NoKey | Failed { .. } | Halted
```

`OpenImageOptions::resolve(sources)` resolves keys here instead of taking a set;
`KeyInput::Seeded` tops up a set that left forensic keys pending. To re-mux one
title of an existing image in place, `remux_iso(&RemuxJob { .. }, &key_params,
&sink)` muxes to `<target>.partial`, verifies the MKV (`verify_mkv`) and only
then renames it over the target; it returns a `RemuxReport`.

---

## Scanning a disc (to get the `Disc` + reader)

```rust
// ISO file or extracted folder (fully synthetic-testable, no drive):
let (disc, mut reader) = scan_image(&ImageSource::from_path(path))?;

// Live drive: scan, then lock the tray, with no key call.
let mut session = open_scan(target, credentials, /* raw_copy */ false)?;
let disc = session.take_disc();         // Option<Disc>
session.stage_drive_as_reader();
let mut reader = session.take_reader();   // Option<Box<dyn SectorSource>>
```

`disc.titles` is your tree source. Each `DiscTitle` carries the video/audio/
subtitle streams and metadata (codec, resolution, duration, size) — the Info
panel fields. All `Disc` fields are public. Pass `raw_copy = true` to
`open_scan` only for a raw (never-decrypting) disc→ISO copy.

---

## Errors

`freemkv_engine::Result<T>` is `Result<T, libfreemkv::Error>`. `Error` is a typed
enum with a numeric `.code()` and **no English text** — map codes to your
localized strings, exactly as the CLI does. A key refusal (no key, no keydb, a
key service that failed) comes back typed from `keys::resolve_for_rip`; map its
code, don't string-match the error. `remux_iso*` returns `std::io::Error`, `mux_image_titles*` a `RipOutcome` (`Failed { code, kind, data }`), `open_image_with` a `libfreemkv::Error`;
`freemkv_engine::error_code(&e)` recovers the code (`None` for an OS error).

---

## Environment knobs

Two process-environment variables change the patch-pass scheduler for every
caller in the process; they are read during every patch pass, and unset keeps
the default scheduler.

- `FREEMKV_PATCH_FLAT` — any value other than empty or `0` replaces the default
  tiered handler ladder with one flat, scorecard-ordered pass over every
  recovery technique per bad range.
- `FREEMKV_PATCH_FLAT_BUDGET` — seconds each handler gets per range in flat
  mode (default 12; `0` becomes 1; a non-integer falls back to 12). Ignored
  unless `FREEMKV_PATCH_FLAT` is on; the tiered ladder always uses 60 s.

---

## What NOT to do

- **Don't print.** Nothing in the engine writes to stdout/stderr; neither should
  your Sink impl in a GUI (route to the log pane).
- **Don't recompute speed/ETA** — use `Progress.speed_bps` / `.eta_secs`.
- **Don't add a direct `libfreemkv` dep just for the disc model** — the engine
  re-exports `Disc`/`DiscTitle`/stream types/`Halt`, and wraps scanning and
  muxing (`scan_image`, `open_scan`, `mux_image_titles`, `remux_iso`).
- **Don't call recovery on the UI thread** — it blocks; run it on a worker and
  marshal `Sink` callbacks back.

---

## Minimal end-to-end shape (multipass rip to MKV)

```rust
fn rip(src: PathBuf, iso_out: PathBuf, opts: MultipassOpts) -> Result<JoinHandle<Result<()>>> {
    let (disc, mut reader) = scan_image(&ImageSource::from_path(&src))?;
    let sources = key_source_factory(&KeyParams::default());
    let titles = resolve_selection(&disc, &Selection::MainMovie);
    let scope = rip_scope(&disc, &titles, RipOutput::Streams);
    let set = resolve_for_rip(&disc, &mut *reader, scope, &sources, None, None)?;
    update_keydb_strip(key_status(&disc, &set));
    let mut job = Job::new(src.to_string_lossy(), "/output/dir").with_mode(RipMode::Multi);
    job.raw = true;                      // the image is ciphertext; the mux decrypts
    if let Preflight::Blocked(reasons) = preflight(&disc, &job) {
        show_blocked(reasons);
        return Ok(std::thread::spawn(|| Ok(())));
    }
    let sink = UiSink::new();            // your impl
    Ok(std::thread::spawn(move || -> Result<()> {
        let mp = multipass_rip(&disc, &mut *reader, &iso_out, &job, &opts, &sink)?;
        if !mp.complete {
            show_partial(mp);
            return Ok(());
        }
        let opened = open_image_with(
            &ImageSource::Iso(iso_out),
            OpenImageOptions { disc: Some(disc), ..OpenImageOptions::known(set) },
        )?;
        let outcome = mux_image_titles(&opened, &MuxPlan::new(titles), &dest_for, &sink);
        show_result(mp, outcome);
        Ok(())
    }))
}
```

That's the whole contract: build a `Job`, resolve its keys once and show
`key_status`, `preflight` it, run recovery and the mux with your `Sink`, and
render the results they return (`MultipassResult`, `RipOutcome`, `RemuxReport`).
