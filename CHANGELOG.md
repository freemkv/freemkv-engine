# Changelog

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

All notable changes to `freemkv-engine` are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the
project follows semantic versioning.

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
