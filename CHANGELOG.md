# Changelog

All notable changes to `freemkv-engine` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the
project follows semantic versioning.

## [Unreleased]

### Changed

- Mapfiles no longer store keys or the raw Volume ID: only the disc hash (`# freemkv-disc:`) and a Volume ID fingerprint (`# freemkv-vidfp:`). Key and VID lines in an older mapfile become fingerprints and are removed the next time it is written. `build_key_fetch` and `OpenedImage::key_fetch` are removed; `OpenedImage` carries `keys`, `sources` and `prescanned`.
- A key service whose address lookup fails for a moment is kept and retried before the rip starts, instead of being dropped.
- The on-arrival key stop (E7022, E7032) ends every pass: it is never retried, skipped or recorded as disc damage.
- `RipOutcome::Failed` gains `data`: the failing error's language-neutral data (`E<code>: <data>`, e.g. a disc hash). A key refusal before any title is reported as the first title's `TitleStart` / `TitleDone(Err)` with the typed error.

- Whole-disc decrypt (disc → ISO) uses libfreemkv's shared `whole_disc` reader, the same one as freemkv's image → ISO path. A stream file no title plays that is encrypted, with no held key opening it, refuses before the copy with E7032 (rip to MKV or make a raw copy) instead of E7013. A key must open two probed units before it keys such a file. A file with no proven key on a multi-key disc stops the pass at its first encrypted unit with the same E7032, and the scan log says why. An AACS disc with titles but no stream folder now fails with E6003, naming the folder, instead of E7013.

- `copy`, `sweep`, `patch`, `ensure_whole_image` and `ensure_titles_staged` return libfreemkv's typed error instead of the generic I/O error (E5000) when an I/O error carries one: a mapfile for another disc is E6011 (`disc-mismatch`), a damaged mapfile E6011, and a stalled or lost output flush E9056/E9057. An output flush stall is no longer read as a drive transport failure.

- Remux: a Stop before the rename ends Halted with the target untouched; once the rename has happened, a failed folder sync is only a warning and the remux is Done (`replaced`, `Event::Replaced`). The staged copy to the library folder stops on Stop and fails E9073 after 60 s with no bytes written; a stopped or stalled copy empties the local staging file at once. A non-UTF-8 mux destination (the target's `.partial`, or the staging file) is refused with E9002 before anything is written, as is a non-UTF-8 disc folder source (before it is opened). A remux waiting on `<target>.lock` no longer times out (E9073) while the holder's sync or verify is making progress. Refusing an existing target (`AlreadyExists`) now carries only the quoted path as its message, so a file name beginning `E<digits>` is not read as that error code.

- `copy`, `sweep` and `patch` refuse up front with E6021, naming the file(s), when the drive's bus map could not locate a bus-encrypted stream file (its File Entry was unreadable), raw copies included: the image would carry those sectors still bus-encrypted. MKV rips and `extract_tree` still run on such a disc.

- A mux's final progress tick (sync/mux 100%) is delivered instead of dropped; disc-borne text (playlist name, failure detail) reaches `Sink::log` with control characters escaped.

- A per-title key top-up keeps its original refusal (e.g. E7028, or an OS error's kind and errno) instead of E7022 or E6011 on later titles; an OS error reading an image's key file is reported as that error, not a disc mismatch.

- A multipass re-run on the same image resumes from its mapfile instead of wiping it: sectors already recovered are not read again. A mapfile this version wrote for another disc (known disc hash or VID fingerprint) is refused (E6011 `disc-mismatch`) while its image exists; with the image missing or empty it is dropped and the rip starts fresh. Multipass still starts fresh over a mapfile written by an older version or by a decrypting rip.
- Mapfiles record whether the image is raw or decrypted (`# freemkv-raw:`). A rip in the other mode overwrites the existing image, with a warning, instead of mixing raw and decrypted sectors in it.
- Multipass loss and convergence are measured over the titles `Job::selection` picks, not always the first title.
- Halted or wedged multipass results report `main_lost_ms` as NaN (unmeasured) when damaged or pending bytes remain, instead of 0.0.

### Fixed

- `preflight` counts a job's key set as usable only when it covers the selected titles, instead of reporting Ready for a title the set cannot decrypt.
- `preflight` refuses a requested language tag that names no language with the new reason `unknown-language` (detail = the tag), instead of passing the job to fail after the image opens, or blaming the disc with `language-unmatched`.
- `episode_titles` picks the largest group of similar lengths; between equal-size groups it keeps the longer unless that one is a play-all of the other (it plays their extents or runs their summed length), so an episode beside a play-all is kept and equal numbers of extras don't displace episodes. Titles with no extents are never dropped as duplicates.
- A `Halted` error with no cancel behind it is `Failed` in every build profile; debug builds no longer panic on it.
- Patch passes report speed over the fixed 10 s window, so a recovery burst shows promptly.

### Added

- `multipass_rip_staged`, `mkv_staging_scope`, `sweep_scoped` and `ensure_whole_image`: an image staged for an MKV rip reads only UDF, nav/AACS files and the chosen titles (AACS BD Pre-recorded 0.953 §3.7: none of it bus-encrypted), so a disc with an unlocatable bus-encrypted stream file still rips to MKV. The scope is recorded in the mapfile (`# freemkv-scope:`), where `stats()` leaves the unread rest out of pending. `copy` and a whole-disc `sweep` over a scoped mapfile refuse with E6021 until every stream file is located, then fill the rest; `patch` re-reads only in-scope damage. `ensure_whole_image` refuses a staged image as a whole-disc source with E6022, and `ensure_titles_staged` refuses (E6022) muxing a title from it whose extents lie outside its scope. A staging image is whole only when it is kept and every stream file was located.
- `ImageSource`, `scan_image` and `open_image`: one path to scan an ISO or disc folder and resolve its keys.
- Keys up front, memory only: `keys::resolve_for_rip` resolves a rip's keys once, before any output, into an in-memory `ResolvedKeySet` that every pass and mux reads; nothing asks a key source after it. `keys::rip_scope`, `keys::key_status` and `open_scan` (a drive scan with no key call) go with it. `extract_tree_with` extracts a decrypted folder through the set. `Job`, `CopyOptions`, `SweepOptions` and `PatchOptions` gain `keys`.
- `open_image_with(src, OpenImageOptions)`: open an image with a key set the caller already holds (`KeyInput::Known`, no key-service call), with a set plus a top-up (`Seeded`), or by resolving (`Resolve`), and with an already-scanned disc (`disc`), in which case an `iso://` image is not rescanned. A `dir://` folder is always scanned, so a pre-scanned disc given with one is refused (E7013). `open_image` calls it. `mux_image_titles` muxes each title from the disc the open already has instead of rescanning the image per title, so a staged ISO whose playlists were never read still muxes.
- E7034: an image whose keys need the disc's Volume ID (its sidecar mapfile has a VID fingerprint) stops before writing anything and asks for the disc, instead of E7022.
- `mux_image_titles` (the desktop app's image mux loop, with `MuxPlan` and `mux_options`), `verify_mkv` (size, tracks, and the muxed runtime from the file's Cues against the title) and `remux_iso`: mux one title to `<target>.partial`, fsync, verify, then rename over the target; on failure the partial file is removed and the target left untouched.
- `Sink::event` with typed `Event`s (phase, title start/done, verify, replaced, output opened); the default ignores them.
- `error_code` and `parse_error_code`: the one reader of libfreemkv's `E<code>[: data]` error form; they read codes exactly as `libfreemkv::error_code` does.
- `open_scan(.., raw_copy)`: a raw disc→ISO copy scans on past an unreadable AACS key file (E7031), as the CLI's `--raw` does.
- `keys::key_url_rejection`: why a configured `key_url` was dropped (it is also logged once per factory, fault kind only).

### Removed

- The legacy key path. `copy`, `sweep`, `patch`, `preflight`, `recover_to_iso` and `multipass_rip` decrypt AACS only through the rip's key set (`keys` / `Job::with_keys`); without one, a decrypting rip of an AACS disc refuses (E7022) whatever keys the scan left on the disc. With no set, a mapfile's old key fingerprints are not checked against the disc's keys.
- `CopyOptions`/`SweepOptions` `vid` and `unit_keys`, and `key_fetch` on all three options; `PatchOptions::for_patch_pass` drops its `key_fetch` argument.
- `Mapfile::{vid, set_vid, set_unit_keys, unit_keys}`; `Mapfile::{set_vid_fingerprint, vid_fingerprint}` remain.
- `resolve_disc_keys`, `resolve_keys`, `open_scan_resolve` and `open_scan_resolve_with`: use `keys::resolve_for_rip`, `keys::key_status` and `open_scan`.

## [1.7.7] — 2026-09-26

### Maintenance

- Replace comment-overflow documentation with concise source contracts and README instructions; enforce the shared comment policy in CI.

## [1.7.6] — 2026-09-26

### Fixed

- Whole-disc decrypt (ISO → ISO) now declares its content extents, so clear filesystem sectors pass through instead of failing the sweep; a decrypt refusal that does occur keeps its own `DecryptFailed` code instead of the misleading `E6000: <lba> 0x00` (#55).

### Maintenance

- Killed 11 surviving mutants in the recovery/mux/resolve paths; the remaining 27 are documented as equivalent.

### Changed

- Version aligned to 1.7.6 for the unified release alongside the freemkv 1.7.6 Linux desktop shell (issue #56).

## [1.7.5] — 2026-09-23

### Changed

- Version aligned to 1.7.5 for the unified release. No functional changes to this crate; the release is driven by freemkv-unlock mirroring the freemkv-firmware 0.9.0 ABI (the drive's `Ake` and `Bus` levers retired into a single `Encryption` lever).

## [1.7.4] — 2026-09-21

### Changed

- Aligned to libfreemkv 1.7.4 `KeyStep` (new `matched_entry` / `store_entries` fields).

### Maintenance

- CI moved to the central reusable workflows.

## [1.7.3] — 2026-09-19

### Changed

- Unified release with freemkv 1.7.3 (`info --share` disc-structure capture for bug reports + the multi-angle UHD main-title fix in libfreemkv; see the libfreemkv/freemkv 1.7.3 notes). No functional changes to this crate.

## [1.7.2] — 2026-09-18

### Changed

- Unified release with freemkv-unlock 1.7.2 (firmware ABI v2). No functional changes to this crate.

## [1.7.1] — 2026-09-14

### Changed

- Version aligned to 1.7.1 for the unified release (freemkv-unlock 1.7.1 LibreDrive unlock fix); no functional changes.

## [1.7.0] — 2026-09-02

### Changed

- Version aligned to 1.7.0 for the unified release. Internal CI/lint hardening (stable-clippy MSRV split, cargo-deny dependency-audit gate, audience-based comment-guard).
- Internal: de-duplicated the drive-info privacy-masking policy into one `mask_char` shared by `mask_string`/`mask_bytes`, and added coverage for the strict decrypt gate (a key-source failure surfaces rather than passing as decryptable). No API change.

### Docs

- Corrected USING_THE_ENGINE.md: the MSRV is 1.88 (single-sourced from `Cargo.toml`, resolved by CI rather than pinned), and the dependency snippet reflects the git-tag patch layout.

## [1.6.14] — 2026-08-31

### Changed

- Version aligned to 1.6.14 for the unified release.

## [1.6.13] — 2026-08-28

### Changed

- Version aligned to 1.6.13 for the unified release.

## [1.6.12] — 2026-08-27

### Fixed

- A mid-rip UNIT ATTENTION (media changed / disc remounted or swapped / bus reset) now aborts the pass to reacquire and re-verify the disc, instead of treating the change as a bad-sector skip that silently drops data.

### Changed

- Comment and documentation cleanup.

## [1.6.11] — 2026-08-26

### Changed

- Version aligned to 1.6.11 for the unified release. No functional changes to
  this crate; the release was driven by the libfreemkv main-feature selection
  improvements and the autorip mux-quarantine fix (see the libfreemkv and
  autorip 1.6.11 notes).

### Added

- Codecov coverage reporting and badge.
- Substantially expanded unit-test coverage.

## [1.6.10] — 2026-08-23

### Changed

- Version aligned to 1.6.10 for the unified release. No functional changes to
  this crate; the release was driven by libfreemkv (TrueHD/MLP audio now resyncs
  to the next major-sync access unit after a source transport-stream
  discontinuity, instead of splicing post-gap audio mid-stream — fixing
  decoder-choking seams on discs whose stream carries a continuity-counter gap;
  see the libfreemkv 1.6.10 notes).
- MSRV lowered from 1.97 to 1.90 (`rust-version` in `Cargo.toml` and the CI
  toolchain pin). 1.90 was the lowest toolchain that built, linted, and passed
  the test suite clean; 1.85 fails because libfreemkv's `build.rs` uses
  let-chains, stabilized in 1.88.

## [1.6.9] — 2026-08-22

### Changed

- Version aligned to 1.6.9 for the unified release. No functional changes to
  this crate; the release was driven by autorip (automatic per-episode TV
  ripping — each episode named `S{NN}E{MM}`, with TMDB runtime-aligned episode
  numbering across multi-disc seasons — a Manual Rename option, and a unified
  per-disc staging state file — see the autorip 1.6.9 notes).

## [1.6.8] — 2026-08-21

### Changed

- Version aligned to 1.6.8 for the unified release. No functional changes to
  this crate; the release was driven by autorip (webhooks now fire per pipeline
  stage — Rip / Mux / Move — with the Rip hook firing the moment the drive is
  free again, plus a Ripper-tab activity-banner fix so it also shows during
  moves — see the autorip 1.6.8 notes).

## [1.6.7] — 2026-08-21

### Changed

- Version aligned to 1.6.7 for the unified release. No functional changes to
  this crate; the release was driven by autorip (per-webhook event selection,
  a progress bar per moved artifact, and move-queue / webhook-error fixes —
  see the autorip 1.6.7 notes).

## [1.6.6] — 2026-08-20

### Changed

- Version aligned to 1.6.6 for the unified release. No functional changes
  to this crate; the release was driven by autorip (webhooks may now target
  private/LAN addresses — see the autorip 1.6.6 notes).

## [1.6.5] — 2026-08-20

### Security

- **A rip could write sectors the drive never delivered into your disc image
  and still exit clean.** Three read sites reused one buffer, shipped a
  fixed-length slice of it downstream, and ignored the byte count the read
  returned — so a source that answered "OK" with a short transfer would leave
  the tail of the *previous* sector inside the ISO and record the range as
  Finished, a success exit over somebody else's data. Reads are now checked
  against the length requested: a short read is treated as a failed read, the
  range stays bad and is retried, and nothing partial is ever committed as
  good.

### Fixed

- **An ISO backup of a disc whose movie was untouched could be flagged
  seriously damaged.** For an ISO rip the damage gate counts every unreadable
  byte on the whole disc, which is correct — but that whole-disc figure was
  then scaled against the main title's size and runtime, so damage sitting
  entirely in menus, trailers, or a bonus feature came back as seconds of lost
  *feature* playback. One bad off-title sector could report over a minute of
  loss and push an intact movie past the threshold where it is badged Serious.
  The playback-loss figure is now always measured against the main title's own
  extents, whatever the deliverable, so off-title damage no longer inflates it.

- **A disc where every read failed could report itself as fully recovered.**
  The live progress accounting let read positions count as recovered bytes, so
  a rip that salvaged nothing still showed a clean, complete result. Recovered,
  pending, retryable, and unreadable bytes now partition the disc so every byte
  lands in exactly one bucket, and the reported bad-sector count and truncated
  bad-range list no longer silently under-report a heavily damaged disc. A
  mapfile that fails to load is now reported as Unknown rather than Converged.

- **Cancelling a rip could throw away sectors the drive had just spent minutes
  recovering.** A Stop request could discard an in-flight recovered span
  instead of handing it off, and could hang on a stalled teardown. Stop now
  preserves already-recovered work, bounds the teardown so it lands promptly,
  and is honoured between the two long deep-read passes that previously ran
  back to back with no cancellation check.

- **A corrupt mapfile was silently skipped instead of refused.** A malformed
  identity header or a truncated data line now fails loudly rather than being
  quietly ignored, and a resume refuses when the mapfile's recorded disc total
  disagrees with the disc in the drive, so a stale or mismatched mapfile can no
  longer be trusted into a bad recovery.

### Changed

- **Hardened several latent traps that logged nothing when they fired.** A
  zero-length recovery span is now refused as a failed read in release builds
  too (previously only caught in debug, where it could otherwise mark a
  never-read span as recovered); the playback-loss helper now derives its
  divisor from the same title it scopes damage to, so the two cannot drift
  apart; and a consumer close() failure on an already-failing pass is now
  logged, since it is the one signal that the mapfile on disk may be
  incomplete.

## [1.6.4] — 2026-08-15

### Fixed

- **A power-cycle-recoverable drive wedge was written off as permanent data
  loss.** A patch pass killed by a USB-bridge transport fault was treated as a
  completed pass, and the end-of-recovery step then marked every surviving range
  — including ones the wedged pass never reached — permanently unreadable, so a
  re-run skipped them for good. A wedged pass now returns partial and reports
  itself as wedged (distinct from a user pressing Stop), so the recovery can be
  resumed after a power cycle.

- **A rip cancelled seconds in no longer reports a healthy disc as seriously
  damaged.** The damage score folded in the un-attempted remainder ahead of the
  read head, scoring tens of millions of "bad" sectors on a disc nobody had read
  yet. The score now counts only genuinely unreadable bytes; outstanding work
  merely withholds the "Clean" badge rather than inventing damage.

## [1.6.3] — 2026-08-10

### Changed

- **No functional change.** This crate ships alongside the rest of freemkv at a
  matching version. Its build and release checks were updated; ripping behaviour
  is untouched.

## [1.6.2] — 2026-08-08

Version sync with the workspace. No functional change in this crate.

## [1.6.1] — 2026-08-07

Version sync with the workspace. No functional change in this crate.

## [1.6.0] — 2026-08-03

Initial release. `freemkv-engine` is the shared rip-orchestration layer between
`libfreemkv` (SCSI, parse, decrypt, mux highway, raw reads) and the front-ends
(the `freemkv` CLI, autorip, and a future desktop UI). It owns freemkv's
recovery *strategy* and the rip *orchestration* that used to live duplicated in
the consumers.

### Added

- **Recovery strategy**, relocated from libfreemkv: the sweep and patch passes,
  the retry-decision state machine, mapfile bookkeeping, damage classification,
  and the multipass sweep → patch → abort-on-loss loop (`multipass_rip`, with
  the `abort_on_lost_secs` gate).
- **Rip orchestration**: `run_titles` / `decide_title` (the single multi-title
  loop policy — fail-fast on a disc-level no-key, Ctrl-C = full stop, skip an
  empty/uncrackable non-feature title), `mux_title`, `resolve_selection`.
- **The `Sink` seam** — the one engine→front-end interface (log / progress /
  title_opened / completed / should_cancel), so nothing in the engine prints
  and cancellation is one bit.
- **`Job` / `preflight` / `resolve_keys`** — the front-end's request as pure
  data, a validate-without-executing check, and key-resolution status as data.
  `resolve_keys` reports `resolved` only when there is real key material
  (non-empty unit keys or a VUK), so a VID-only placeholder scan
  (`KeyOrigin::ExternalUk`, empty unit keys) reads as unresolved rather than
  falsely "resolved"; `ExternalUk` is summarized `resolved-external` (it is
  source-agnostic — not necessarily online).
- **Mapfile-backed reporting helpers** for front-ends: `Mapfile` / `SectorStatus`
  / `MapStats`, `bytes_bad_in_title_from_mapfile` (bad bytes in a title from a
  mapfile path), and `progress_snapshot_from_mapfile` (a one-shot
  `PassProgress` for the pass-boundary paint / done-card). `DamageSeverity` is
  owned here alongside `classify_damage`.
- **Stream selection policy**: `StreamChoice` / `StreamFilter` on the `Job` and
  `resolve_stream_selection` (language tags → the library's PID selection, via
  isolang).

See `USING_THE_ENGINE.md` for the integration guide.
