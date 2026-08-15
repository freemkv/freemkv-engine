# Changelog

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
