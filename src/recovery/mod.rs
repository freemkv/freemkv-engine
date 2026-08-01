//! freemkv's recovery strategy — relocated here from libfreemkv per the
//! engine-split design (see this crate's top-level docs).
//!
//! Mirrors the original `disc/` module topology 1:1 so the relocation is a
//! faithful move, not a rewrite: `mapfile.rs`, `read_error.rs`,
//! `section_recover.rs`, `patch.rs`, and the private `sweep.rs`
//! producer/consumer plumbing are unchanged in logic — only crate-external
//! references became `libfreemkv::`, and `Disc` methods became free
//! functions taking `&libfreemkv::Disc`.
//!
//! This file (`copy`/`sweep_internal`/`patch_internal`/`sweep` + the option/
//! result types + `ecc_sectors`) is the relocated `impl Disc` multipass-
//! dispatch block. Behavior unchanged — only the receiver
//! (`&self` -> `disc: &libfreemkv::Disc`) and crate-external paths changed.

use libfreemkv::disc::{bytes_bad_in_title, locate_ranges};
use libfreemkv::error::{Error, Result};
use libfreemkv::extract_scsi_context;
use libfreemkv::sector::SectorSource;

pub use patch::patch;

pub fn copy(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
) -> Result<CopyResult> {
    // Pre-flight decrypt gate. A decrypting copy (`opts.decrypt == true`,
    // i.e. NOT `--raw`) of an encrypted disc with no usable key would wrap
    // the reader in a pass-through `DecryptingSectorSource` and write
    // ciphertext to the ISO, then return `Ok` (bytes_good > 0) — a silent
    // garbage success at exit 0. Refuse here, BEFORE any sweep/patch reads a
    // single sector, so the failure is pre-flight and no partial ISO is
    // written. `opts.decrypt == false` is `--raw`: the gate is a no-op (the
    // user wants the encrypted image), and an unencrypted disc passes too.
    crate::resolve::ensure_decryptable_strict(disc, !opts.decrypt)?;
    // Mapfile-driven resume dispatch. This runs for BOTH plain and
    // `--multipass` copies: an interrupted plain `disc:// → iso://` writes
    // a per-block-flushed mapfile (crash-safe), and re-issuing the SAME
    // command must pick up where it stopped rather than re-sweep from
    // sector 0 (the help/CLI examples promise "auto-resumes if
    // interrupted"). The ONLY multipass-specific behaviour is the patch
    // (Pass N) dispatch on retryable bytes — plain mode has no patch pass,
    // so it returns a terminal result there instead.
    let mf_path = disc.mapfile_for(path);
    if mf_path.exists() {
        let map = mapfile::Mapfile::load(&mf_path).map_err(|e| Error::IoError { source: e })?;
        // BEFORE any resume decision — including the "already complete, return
        // without reading a sector" branch below, which is the worst case to
        // get wrong: a wrong disc whose predecessor finished would report the
        // job done having never touched the disc in the drive.
        mapfile::check_mapfile_identity(&map, disc).map_err(|e| Error::IoError { source: e })?;
        let stats = map.stats();
        let disc_size = disc.capacity_bytes;
        let covers_disc = map.total_size() == disc_size;
        let bad_bytes = stats.bytes_pending + stats.bytes_unreadable;
        tracing::info!(
            "copy dispatch: disc={} map={} covers={} multipass={} good={} nontried={} pending={} unreadable={}",
            disc_size,
            map.total_size(),
            covers_disc,
            opts.multipass,
            stats.bytes_good,
            stats.bytes_nontried,
            stats.bytes_pending,
            stats.bytes_unreadable,
        );
        // The mapfile and the ISO are two separate files and the shortcut
        // below was checking only one of them. A staging cleanup, a remount,
        // or an operator freeing space can remove or truncate the image while
        // the mapfile survives — and then "every range is Finished" describes
        // an ISO that is no longer there. `sweep()` already guards this exact
        // inconsistency (see its inconsistent-resume guard); the dispatch
        // shortcut needs it too, or a rip reports a full disc of good bytes
        // having written nothing and the caller muxes from a missing file.
        let iso_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let iso_is_intact = iso_len == disc_size;
        if covers_disc && bad_bytes == 0 && stats.bytes_nontried == 0 && !iso_is_intact {
            // Complete mapfile, but the image it describes is gone or short.
            // A resume cannot repair this: the producer builds work only from
            // NonTried ranges and there are none, so it would return a
            // terminal success having written nothing. Force a fresh full
            // sweep, exactly as the covers_disc=false case below does.
            tracing::info!(
                "copy dispatch: → sweep (mapfile complete but ISO is {} — {} of {} bytes)",
                if iso_len == 0 {
                    "missing/empty"
                } else {
                    "truncated"
                },
                iso_len,
                disc_size,
            );
            return sweep_internal(disc, reader, path, opts, false);
        }
        if covers_disc && bad_bytes == 0 && stats.bytes_nontried == 0 && iso_is_intact {
            // Every sector is Finished AND the image it describes is intact —
            // a prior copy completed. Re-issuing the command is a no-op
            // (don't re-sweep a finished ISO).
            return Ok(CopyResult::new(
                disc_size,
                stats.bytes_good,
                stats.bytes_unreadable,
                0,
                0,
                false,
            ));
        }
        if !covers_disc {
            // Mapfile capacity != disc capacity. Force a full (non-
            // resume) sweep on ANY mismatch so [0, disc_size) is covered
            // as one fresh region (the non-resume path also set_len's the
            // ISO to the full capacity).
            //
            // UNDER-cover (map.total_size() < disc_size): a resume sweep
            // builds its region list only from the mapfile's NonTried
            // entries and would silently never read the tail
            // [map.total_size(), disc_size) — abandoning readable data
            // and the ISO's tail.
            //
            // OVER-cover (map.total_size() > disc_size): a resume sweep's
            // NonTried regions extend past the disc; `reader.read_sectors`
            // would then read LBAs beyond capacity (the promised
            // capacity clamp was never actually applied). A fresh sweep
            // sized to the real disc avoids reading past the end.
            tracing::info!(
                "copy dispatch: → sweep (covers_disc=false, resume=false, map={}, disc={})",
                map.total_size(),
                disc_size,
            );
            return sweep_internal(disc, reader, path, opts, false);
        }
        // NonTried bytes mean a prior sweep was halted mid-way (Ctrl-C /
        // crash) and the mapfile still has un-attempted ranges (the un-swept
        // tail). The sweep pass's job is to read those — route to a resume
        // sweep FIRST, even when retryable bytes also exist. Checking
        // retryable before this (and routing straight to patch) would
        // silently abandon the un-swept tail: patch only revisits the
        // mapfile's bad ranges, never the NonTried ones. The retry
        // (patch) passes run after, driven separately by the caller's
        // pass loop, and pick up the retryable bytes the sweep leaves.
        // This is the plain-copy resume path too: a clean disc interrupted
        // by Ctrl-C leaves exactly this state (NonTried tail), so a re-run
        // resumes the sweep instead of restarting from sector 0.
        if stats.bytes_nontried > 0 {
            tracing::info!(
                "copy dispatch: → sweep resume (covers_disc=true, \
                 nontried={}, retryable={})",
                stats.bytes_nontried,
                stats.bytes_retryable,
            );
            return sweep_internal(disc, reader, path, opts, true);
        }
        // From here covers_disc=true and nontried=0: the whole disc was
        // attempted. Only the retry/patch decision differs by mode.
        if opts.multipass {
            if stats.bytes_retryable > 0 {
                tracing::info!(
                    "copy dispatch: → patch (retryable={})",
                    stats.bytes_retryable,
                );
                return patch_internal(disc, reader, path, opts);
            }
            // Fallthrough: covers_disc=true, nontried=0, retryable=0.
            // All sectors were attempted; any remaining bad bytes are
            // already Unreadable. A resume sweep would visit zero new
            // sectors and patch has nothing retryable — return the
            // terminal result immediately.
            tracing::info!(
                "copy dispatch: all bad sectors already Unreadable \
                 (retryable=0, nontried=0) — returning terminal result",
            );
            return Ok(CopyResult::new(
                disc_size,
                stats.bytes_good,
                stats.bytes_unreadable,
                0,
                0,
                false,
            ));
        }
        // Plain (non-multipass) copy: there is no patch pass and the sweep
        // aborts on the first read error, so a fully-attempted mapfile with
        // bad bytes is terminal. Re-running must NOT restart from sector 0
        // (that re-reads the whole disc and re-hits the same bad sector);
        // return the terminal result so the caller surfaces the failure.
        // (`complete` is true only when no bad bytes remain.)
        tracing::info!(
            "copy dispatch: plain copy, disc fully attempted (bad={}) — terminal result",
            bad_bytes,
        );
        return Ok(CopyResult::new(
            disc_size,
            stats.bytes_good,
            stats.bytes_unreadable,
            stats.bytes_pending,
            0,
            false,
        ));
    }
    sweep_internal(disc, reader, path, opts, false)
}

/// Sectors in one AACS aligned unit (6144 bytes = 3 sectors).
const UNIT_SECTORS: u16 = (libfreemkv::aacs::content::ALIGNED_UNIT_LEN / 2048) as u16;

/// Round a sweep's batch size up to a whole number of AACS aligned units.
///
/// `decrypt_sectors` anchors units at buffer offset 0, so every read handed to
/// the decrypting reader must span a whole number of units — otherwise units
/// straddle batch boundaries, decrypt under the wrong unit alignment, and the
/// verify gate either leaves the content encrypted or aborts `DecryptFailed`.
/// `ecc_sectors()` is 32 for UHD/BD and 32 is not a multiple of 3, so without
/// this every batch after the first would start mid-unit.
///
/// Pure and separate from [`sweep_internal`] because it is a real decision with
/// a real failure mode, and inside a function that needs a live drive nothing
/// could reach it: the mutation run replaced the `-` with `+` and the `%` with
/// `/` here and the suite stayed green either way.
pub(crate) fn aacs_aligned_batch(batch: u16, decrypt_is_aacs: bool) -> u16 {
    if decrypt_is_aacs && !batch.is_multiple_of(UNIT_SECTORS) {
        return batch.saturating_add(UNIT_SECTORS - (batch % UNIT_SECTORS));
    }
    batch
}

/// Anchor a region's read cursor DOWN to the nearest AACS unit boundary.
///
/// A resume `NonTried` region can begin mid-unit. `decrypt_sectors` anchors
/// units at buffer offset 0, so a read that STARTS mid-unit decrypts under the
/// wrong alignment. Re-reading the few head sectors is idempotent, so aligning
/// down is free. Sibling of [`aacs_aligned_batch`], which handles the other
/// half of the same invariant (a whole number of units per read).
pub(crate) fn aacs_aligned_region_start(region_pos: u64, decrypt_is_aacs: bool) -> u64 {
    if !decrypt_is_aacs {
        return region_pos;
    }
    // Same source of truth as `aacs_aligned_batch`'s `UNIT_SECTORS`, expressed
    // in bytes rather than sectors — deriving it twice from the raw constant is
    // how the two halves of one invariant drift apart.
    let unit = UNIT_SECTORS as u64 * 2048;
    region_pos - (region_pos % unit)
}

#[cfg(test)]
mod sleep_secs_or_halt_tests {
    use super::sleep_secs_or_halt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// The pause must actually happen. The mutation run replaced this whole
    /// function with `()` and the suite stayed green — so the wedge-avoidance
    /// inter-error pause, the thing that stops a damaged disc being hammered,
    /// was unconstrained. Same for mutating the loop condition to `==` or `>`,
    /// both of which make the loop body unreachable.
    #[test]
    fn it_actually_sleeps_when_not_halted() {
        let halt = Arc::new(AtomicBool::new(false));
        let t0 = Instant::now();
        sleep_secs_or_halt(1, Some(&halt));
        let e = t0.elapsed();
        // Generous lower bound: the point is "roughly a second", not precision.
        assert!(
            e >= Duration::from_millis(800),
            "returned after {e:?} — the sleep did not happen"
        );
    }

    /// And it must break out early when halt is already set, rather than
    /// serving the full pause. This is the difference between Stop being
    /// honoured and the operator waiting out a multi-second cooldown.
    #[test]
    fn an_already_set_halt_returns_promptly() {
        let halt = Arc::new(AtomicBool::new(true));
        let t0 = Instant::now();
        sleep_secs_or_halt(30, Some(&halt));
        let e = t0.elapsed();
        assert!(
            e < Duration::from_millis(500),
            "waited {e:?} on an already-halted sleep"
        );
    }

    /// A halt raised WHILE the pause is in progress must also cut it short —
    /// the polling loop, not just the entry check.
    #[test]
    fn a_halt_raised_mid_sleep_cuts_it_short() {
        let halt = Arc::new(AtomicBool::new(false));
        let h = halt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            h.store(true, Ordering::Relaxed);
        });
        let t0 = Instant::now();
        sleep_secs_or_halt(30, Some(&halt));
        let e = t0.elapsed();
        assert!(
            e < Duration::from_secs(3),
            "waited {e:?} — the loop did not observe the halt"
        );
        assert!(
            e >= Duration::from_millis(150),
            "returned in {e:?} — suspiciously early, did it sleep at all?"
        );
    }

    /// Zero seconds is a no-op either way; pinned so the early return cannot
    /// silently become a real sleep.
    #[test]
    fn zero_seconds_returns_immediately() {
        let t0 = Instant::now();
        sleep_secs_or_halt(0, None);
        assert!(t0.elapsed() < Duration::from_millis(200));
    }
}

#[cfg(test)]
mod aacs_aligned_batch_tests {
    use super::{UNIT_SECTORS, aacs_aligned_batch};

    /// The case that actually happens: BD/UHD ecc_sectors() is 32.
    #[test]
    fn rounds_the_real_bd_batch_up_to_a_unit_boundary() {
        assert_eq!(aacs_aligned_batch(32, true), 33);
    }

    /// Rounding UP, never down and never over a whole extra unit.
    #[test]
    fn result_is_the_next_multiple_of_a_unit() {
        for batch in 1u16..=256 {
            let aligned = aacs_aligned_batch(batch, true);
            assert!(aligned >= batch, "{batch} rounded down to {aligned}");
            assert!(
                aligned.is_multiple_of(UNIT_SECTORS),
                "{batch} → {aligned}, not a whole number of units"
            );
            assert!(
                aligned - batch < UNIT_SECTORS,
                "{batch} → {aligned} overshot by a whole unit or more"
            );
        }
    }

    /// Already aligned means untouched — a `+` in place of the `-` would push
    /// an aligned batch off the boundary it is already on.
    #[test]
    fn an_aligned_batch_is_left_alone() {
        for batch in [3u16, 33, 96, 300] {
            assert_eq!(aacs_aligned_batch(batch, true), batch);
        }
    }

    /// A non-AACS sweep has no unit geometry to respect; the batch is the
    /// drive's ECC size and must not be altered.
    #[test]
    fn a_non_aacs_sweep_keeps_its_batch() {
        for batch in [1u16, 32, 64, 65535] {
            assert_eq!(aacs_aligned_batch(batch, false), batch);
        }
    }

    /// Saturating, not wrapping: a batch near u16::MAX must not wrap to a tiny
    /// read.
    #[test]
    fn near_max_saturates_instead_of_wrapping() {
        let aligned = aacs_aligned_batch(u16::MAX - 1, true);
        assert!(aligned >= u16::MAX - 1, "wrapped to {aligned}");
    }
}

fn sweep_internal(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
    resume: bool,
) -> Result<CopyResult> {
    let sweep_opts = SweepOptions {
        decrypt: opts.decrypt,
        resume,
        batch_sectors: None,
        skip_on_error: opts.multipass,
        progress: opts.progress,
        halt: opts.halt.clone(),
        vid: opts.vid,
        unit_keys: opts.unit_keys.clone(),
        key_fetch: opts.key_fetch.clone(),
    };
    sweep(disc, reader, path, &sweep_opts)
}

fn patch_internal(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
) -> Result<CopyResult> {
    let patch_opts = PatchOptions::for_patch_pass(
        opts.decrypt,
        opts.progress,
        opts.halt.clone(),
        opts.key_fetch.clone(),
    );
    let pr = patch(disc, reader, path, &patch_opts)?;
    tracing::info!(
        target: "freemkv::disc",
        phase = "patch_done",
        bytes_recovered = pr.bytes_recovered_this_pass,
        halted = pr.halted,
        wedged_exit = pr.wedged_exit,
        "Patch completed"
    );
    Ok(CopyResult::new(
        pr.bytes_total,
        pr.bytes_good,
        pr.bytes_unreadable,
        pr.bytes_pending,
        pr.bytes_recovered_this_pass,
        pr.halted,
    ))
}

/// Pass 1 of a multipass rip: walk the disc forward, write
/// every readable sector into `path`, and record the result
/// in the sidecar mapfile. With `skip_on_error: true`, a bad
/// sector zero-fills + marks `NonTrimmed` and the sweep keeps
/// going (jumping ahead through dense damage); without it,
/// the first read failure aborts.
///
/// This is one of the two flat verbs the library exposes
/// for rip orchestration. Multipass + retry decisions are the
/// caller's job — see [`PatchOptions`] for the retry primitive.
pub fn sweep(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
) -> Result<CopyResult> {
    use libfreemkv::io::{DEFAULT_PIPELINE_DEPTH, Pipeline};
    use libfreemkv::sector::{DecryptingSectorSource, SectorSource};
    use sweep::{ProgressSnapshot, SweepSink, WorkItem, try_recv_progress};

    // Pre-flight decrypt gate (also enforced in `copy`; re-checked here so a
    // direct `sweep` caller can't bypass it). A decrypting sweep of an
    // encrypted disc with no usable key would write ciphertext to the ISO at
    // exit 0; refuse before reading any sector. No-op for `--raw`
    // (`opts.decrypt == false`) and unencrypted discs.
    crate::resolve::ensure_decryptable_strict(disc, !opts.decrypt)?;

    let total_bytes = disc.capacity_sectors as u64 * 2048;
    // Decrypt-aware read.
    //
    // A decrypting sweep (`opts.decrypt`, e.g. `disc:// → iso://` without
    // `--raw`) decrypts each unit IN PLACE → the ISO holds plaintext.
    //
    // Every other sweep (`!opts.decrypt`: the autorip / `--multipass` path and
    // plain `--raw`) writes the ISO as CIPHERTEXT verbatim — keys = `None`, a
    // pure pass-through. Bad sectors are found by PHYSICAL read success (a SCSI
    // read error → skip / NonTrimmed → patch re-read), NOT by decrypt structure.
    // (The old decrypt-VERIFY read gate — which mis-aligned the disc-absolute
    // unit grid against clip-file-anchored AACS units and false-failed good
    // clips like Dunkirk's orphan-CPS clip — was removed. There is no scratch
    // verify and no post-sweep clip-anchored pass; decryptability is proven at
    // mux time, not at capture time.)
    let mut keys = if opts.decrypt {
        disc.decrypt_keys()
    } else {
        libfreemkv::decrypt::DecryptKeys::None
    };
    let decrypt_is_aacs = matches!(keys, libfreemkv::decrypt::DecryptKeys::Aacs { .. });
    // AACS decrypting sweep: resolve a WHOLE-DISC key map up front (the fetch
    // secures any missing CPS-unit key, fail-loud) and decrypt via the map —
    // a clear nav/filesystem sector is in no range and passes through, so no
    // separate content gate is needed. CSS keeps the content-gated
    // self-descramble path (the map path is AACS-only).
    let key_map = if opts.decrypt && decrypt_is_aacs {
        let halt = opts.halt.clone().map(libfreemkv::halt::Halt::from_arc);
        Some(std::sync::Arc::new(disc.resolve_content_key_map(
            reader,
            &mut keys,
            opts.key_fetch.as_ref(),
            halt.as_ref(),
        )?))
    } else {
        None
    };
    let content_ranges = disc.encrypted_content_ranges();
    let can_gate = !content_ranges.is_empty();

    let mut reader = {
        let mut dec = DecryptingSectorSource::new(reader, keys);
        if let Some(map) = key_map {
            dec = dec.with_key_map(map);
        } else if opts.decrypt && can_gate {
            // CSS / clear decrypt: content-gate the self-descramble path.
            dec = dec.with_content_ranges(std::sync::Arc::from(content_ranges));
        }
        dec
    };
    let reader = &mut reader;

    // Mapfile: load if resuming, else wipe + recreate.
    let mapfile_path = disc.mapfile_for(path);
    // covers_disc reconciliation. A resume against a mapfile whose total
    // size != the real disc size is unsafe — exactly the case copy()'s
    // dispatch forces to a fresh sweep (see Disc::copy). Under-cover
    // (map < disc) abandons the disc tail [map.total_size(), disc);
    // over-cover (map > disc) reads LBAs past capacity. When sweep() is
    // called directly (not via copy()), apply the same downgrade: drop the
    // stale mapfile and sweep [0, total_bytes) fresh.
    let mut resume = opts.resume;
    if resume && mapfile_path.exists() {
        match mapfile::Mapfile::load(&mapfile_path) {
            Ok(existing) => {
                // Identity first, and crucially BEFORE the unconditional
                // set_vid/set_unit_keys overwrite further down: that overwrite
                // stamps the CURRENT job's identity onto the loaded mapfile, so
                // a check placed after it would compare a value against itself
                // and never fire.
                mapfile::check_mapfile_identity(&existing, disc)?;
                if existing.total_size() != total_bytes {
                    tracing::info!(
                        "sweep: mapfile total_size {} != disc {}; forcing fresh sweep",
                        existing.total_size(),
                        total_bytes,
                    );
                    resume = false;
                } else {
                    // Inconsistent-resume guard. The mapfile claims prior
                    // progress (some range past NonTried) but the ISO is
                    // missing or zero-length — the ISO was deleted or
                    // truncated while the mapfile survived (reachable via
                    // autorip ResumeMode::Require). The producer only builds
                    // work from NonTried ranges, so any Finished range would
                    // never be re-read and would stay ZERO in the fresh ISO,
                    // silently holed. Downgrade to a fresh full sweep (mirror
                    // the total_size-mismatch case) so the rip self-heals.
                    // The comment above says "missing OR TRUNCATED", but this
                    // only ever tested for zero length, so an ISO truncated to
                    // a non-zero length — a partial copy, a full disk, a
                    // half-finished transfer — passed the guard and resumed.
                    // The producer builds work only from NonTried ranges, so
                    // every Finished range beyond the truncation point is
                    // never re-read and stays a hole in the final image. Short
                    // is as inconsistent as absent; compare against the size
                    // the mapfile claims to describe.
                    //
                    // A metadata ERROR is likewise not a length. Treating it
                    // as 0 silently threw away a good resume and re-ripped
                    // hours of work on a transient stat failure.
                    let iso_len = match std::fs::metadata(path) {
                        Ok(m) => Some(m.len()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
                        Err(e) => return Err(Error::IoError { source: e }),
                    };
                    let claims_progress = existing.stats().bytes_pending != existing.total_size();
                    if iso_len.is_some_and(|len| len < existing.total_size()) && claims_progress {
                        tracing::info!(
                            "sweep: mapfile claims prior progress (pending {} of {}) but the ISO is {} of {} bytes; forcing fresh sweep",
                            existing.stats().bytes_pending,
                            existing.total_size(),
                            iso_len.unwrap_or(0),
                            existing.total_size(),
                        );
                        resume = false;
                    }
                }
            }
            Err(_) => {
                // The mapfile exists but is corrupt / unparseable. Proceeding
                // with resume=true would hand a garbage (or empty) mapfile to
                // open_or_create and silently skip already-Finished ranges or
                // mis-track progress. Downgrade to a fresh sweep — consistent
                // with the total_size-mismatch branch above — so the `!resume`
                // path below drops the corrupt mapfile and the rip restarts
                // clean.
                tracing::info!(
                    "sweep: mapfile at {} is corrupt/unparseable; forcing fresh sweep",
                    mapfile_path.display(),
                );
                resume = false;
            }
        }
    }
    if !resume {
        // A fresh sweep MUST start from an empty mapfile. If the stale file
        // can't be removed, open_or_create would load it and the new disc
        // would inherit the old Finished ranges → silently zero-filled ISO.
        // ENOENT is fine (nothing to remove); any other error aborts.
        match std::fs::remove_file(&mapfile_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::IoError { source: e }),
        }
    }
    let mut map = mapfile::Mapfile::open_or_create(
        &mapfile_path,
        total_bytes,
        concat!("libfreemkv v", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|e| Error::IoError { source: e })?;

    // Persist the disc's decryption state into the mapfile header so it
    // survives to deferred-mux / resume. ddrescue-safe (comment lines);
    // does not touch the ISO payload. KEYS XOR VID: a keyed disc writes its
    // unit keys (the final answer — deferred-mux decrypts directly, no key
    // service); an unresolved disc writes only the VID (the retry marker).
    if !opts.unit_keys.is_empty() {
        map.set_unit_keys(&opts.unit_keys);
    } else if let Some(vid) = opts.vid {
        map.set_vid(vid);
    }

    // ISO file: if resuming and mapfile has Finished ranges, open existing;
    // otherwise create fresh and pre-size to total_bytes (sparse holes for
    // non-tried regions).
    //
    // `is_regular` MUST be read from the OPEN file handle, not from
    // `metadata(path)` — on a fresh rip the path does not exist yet, so a
    // pre-create `metadata(path)` always fails (is_regular=false), which both
    // skips the pre-size AND makes `SweepSink::close` swallow a real
    // `sync_all()` failure on the just-written ISO as if it were /dev/null.
    // A metadata ERROR is not "the file is empty". Collapsing the two with
    // unwrap_or(false) meant a transient stat failure on a populated ISO —
    // an EIO from a flaky USB/NFS staging volume, a momentary permissions
    // problem — fell through to the create-and-truncate branch below and
    // permanently zeroed bytes the mapfile still records as Finished. A
    // resume would then never re-read them, because the producer only builds
    // work from NonTried ranges. Silent, total loss of the recovered data.
    //
    // NotFound is the one error that genuinely means "no file yet" (the
    // fresh-rip case). Anything else is unknown, and destroying data on an
    // unknown is not a decision this code gets to make.
    let existing_len = match std::fs::metadata(path) {
        Ok(m) => Some(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::IoError { source: e }),
    };
    let (file, is_regular) = if resume && existing_len.is_some_and(|len| len > 0) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| Error::IoError { source: e })?;
        let reg = f
            .metadata()
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        (f, reg)
    } else {
        let f = std::fs::File::create(path).map_err(|e| Error::IoError { source: e })?;
        let reg = f
            .metadata()
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if reg {
            f.set_len(total_bytes)
                .map_err(|e| Error::IoError { source: e })?;
        }
        (f, reg)
    };

    // Wrap the raw `File` in our bounded-cache `WritebackFile`
    // (drains dirty pages continuously instead of bursting; see
    // `libfreemkv::io`). The `WritebackFile` moves into the consumer
    // thread.
    let file =
        libfreemkv::io::WritebackFile::new(file).map_err(|e| Error::IoError { source: e })?;
    let mut batch: u16 = match opts.batch_sectors {
        // A zero batch makes `block_bytes` 0 every iteration, so `pos` never
        // advances and the producer spins forever emitting zero-length reads.
        // Clamp rather than error: the caller asked for "as small as possible".
        Some(b) => b.max(1),
        None if opts.skip_on_error => ecc_sectors(disc.format),
        None => DEFAULT_BATCH_SECTORS_OPTICAL,
    };

    // AACS unit alignment for a DECRYPTING sweep. AACS aligned units are 3
    // sectors (6144 bytes); `decrypt_sectors` anchors units at buffer offset
    // 0, so every read handed to the decrypting reader MUST start on a unit
    // boundary AND span a whole number of units — otherwise units straddle
    // batch/region boundaries and decrypt under the wrong CBC/unit alignment
    // (the verify-gate then leaves content encrypted or aborts DecryptFailed).
    //
    // ecc_sectors() is 32 for UHD/BD, which is NOT a multiple of 3, so the
    // default batch would start every batch-after-the-first mid-unit. Round
    // the batch UP to the next multiple of 3 (32 → 33) when this sweep both
    // decrypts and is AACS-keyed. Region read-starts are aligned DOWN to a
    // unit boundary in the loop below; a fresh sweep starts at LBA 0 (already
    // aligned), so alignment only bites on resume NonTried regions.
    batch = aacs_aligned_batch(batch, decrypt_is_aacs);

    // Pre-compute the list of NonTried regions before handing the
    // mapfile to the consumer thread. Each region is processed by
    // the producer in order; the consumer mutates the mapfile per
    // work-item. Any regions left as NonTrimmed/Unreadable after
    // sweep finishes are the patch pass's job.
    let regions: Vec<(u64, u64)> = map.ranges_with(&[mapfile::SectorStatus::NonTried]);

    // Spawn the consumer. It owns WritebackFile + Mapfile; the producer
    // (this thread) keeps `reader`, `read_ctx`, halt + set_speed.
    // The thread name is preserved from the 0.17.x sweep_pipeline so it
    // stays identifiable in stack traces / `top -H`.
    let (sink, prog_rx) = SweepSink::new(file, map, is_regular);
    let pipe: Pipeline<WorkItem, sweep::ConsumerSummary> =
        Pipeline::spawn_named("freemkv-sweep-consumer", DEFAULT_PIPELINE_DEPTH, sink)?;

    // Translate `Pipeline::send` failure (consumer gone) into a
    // numeric library error so the producer-error semantics are
    // unchanged but no English leaks into an io::Error.
    fn consumer_gone() -> Error {
        Error::PipelineConsumerGone
    }

    let mut buf = vec![0u8; batch as usize * 2048];
    let mut bytes_done = 0u64;
    let mut halt_requested = false;
    let copy_t0 = std::time::Instant::now();
    tracing::info!(
        target: "freemkv::scan",
        phase = "sweep",
        total_bytes,
        skip_on_error = opts.skip_on_error,
        resume,
        "begin"
    );
    let mut iter_count: u64 = 0;
    let mut read_ok_count: u64 = 0;
    let mut read_err_count: u64 = 0;
    let mut last_log_iter: u64 = 0;
    // Sweep heartbeat: fire every 5s OR every 100 iterations, whichever
    // comes first, so a slow-but-alive sweep on a marginal disc keeps
    // emitting "no silent hang" liveness even between the 100-iter marks.
    let mut last_log_time = std::time::Instant::now();
    let mut read_ctx = read_error::ReadCtx::for_sweep(batch);
    let mut in_damage_zone = false;
    const DAMAGE_ZONE_EXIT_THRESHOLD: u64 = 16;
    let mut cached_snapshot: Option<ProgressSnapshot> = None;
    // Derived from `cached_snapshot.bad_ranges` + the main title ONLY, so they
    // change exactly when a new snapshot lands — not once per batch. Computing
    // them in the per-iteration reporter block re-ran `bytes_bad_in_title`
    // (O(ranges x extents), twice: once directly and once inside
    // `locate_ranges`) 400k-1.6M times per rip over a value that had not moved.
    let mut cached_main_title_bad: u64 = 0;
    let mut cached_located = libfreemkv::progress::LocatedProgress::default();
    let mut producer_err: Option<Error> = None;

    tracing::trace!(
        target: "freemkv::disc",
        phase = "copy_start",
        total_bytes,
        batch,
        skip_on_error = opts.skip_on_error,
        regions = regions.len(),
        "Disc::sweep entered (producer/consumer)"
    );

    // Request the drive's max read speed for the whole sweep — removes
    // riplock. BD/UHD get their speed from the drive unlock/init, but a
    // DVD skips that path (the stock-mode gate, `Drive::disc_is_dvd`), so
    // without this explicit SET CD SPEED a DVD rip sweeps at the drive's
    // default (riplocked) speed. The damage-recovery branch below also
    // re-asserts max speed after slowing on bad sectors; this sets it once
    // up front so a clean disc never pays the riplock penalty.
    reader.set_speed(0xFFFF);

    'outer: for (region_pos, region_size) in regions {
        // Snap to whole sectors before the range becomes a read/write cursor.
        // Mapfile ranges are BYTE ranges with no alignment guarantee (the
        // format interoperates with ddrescue, whose `-b 512` emits
        // 512-granular ranges), and an unaligned offset truncates to the wrong
        // LBA and shifts real payload — which is then recorded Finished.
        // `patch` has always snapped its ingress; this path did not, and the
        // only alignment it had was gated on a decrypting AACS rip, a branch a
        // multipass resume never takes because multipass implies raw.
        let (region_pos, region_size) = snap_to_sectors(region_pos, region_size);
        let region_end = region_pos + region_size;
        // AACS unit alignment: anchor the region's read cursor DOWN to the
        // nearest 6144-byte unit boundary so the decrypting reader never gets
        // a buffer that starts mid-unit. Re-reading the few already-covered
        // head sectors is idempotent (they re-decrypt identically and the
        // consumer overwrites the same ISO offsets / mapfile ranges). A fresh
        // sweep's NonTried region starts at 0, already unit-aligned; this only
        // shifts resume regions that begin mid-unit.
        let mut pos = if decrypt_is_aacs {
            aacs_aligned_region_start(region_pos, true)
        } else {
            region_pos
        };
        tracing::trace!(
            target: "freemkv::disc",
            phase = "region_enter",
            region_pos,
            region_size,
            region_end,
            "entering NonTried region"
        );

        while pos < region_end {
            if let Some(ref h) = opts.halt
                && h.load(std::sync::atomic::Ordering::Relaxed)
            {
                halt_requested = true;
                break 'outer;
            }

            let block_bytes = (region_end - pos).min(batch as u64 * 2048);
            let block_lba = (pos / 2048) as u32;
            let block_count = (block_bytes / 2048) as u16;
            let recovery = !opts.skip_on_error;

            let read_result = reader.read_sectors(
                block_lba,
                block_count,
                &mut buf[..block_bytes as usize],
                recovery,
            );

            match read_result {
                Ok(_) => {
                    read_ok_count += 1;
                    read_ctx.on_success();

                    if read_ctx.consecutive_good >= DAMAGE_ZONE_EXIT_THRESHOLD {
                        read_ctx.jump_multiplier = 1;
                        if in_damage_zone {
                            in_damage_zone = false;
                            reader.set_speed(0xFFFF);
                            tracing::debug!(
                                target: "freemkv::disc",
                                phase = "damage_exit",
                                lba = block_lba,
                                "Exited damage zone; restoring max read speed"
                            );
                        }
                    }
                    // bridge_degradation_count is reset inside on_success()
                    // (called above); no separate reset needed here.

                    // Plaintext: the wrapped reader (DecryptingSectorSource)
                    // applied AACS / CSS in-place during read_sectors above.
                    // The consumer thread sees decrypted bytes; the
                    // pre-0.18 inline decrypt_sectors call lived here.

                    // Move the batch into the channel via fresh
                    // owned Vec. The producer's `buf` is reused
                    // for the next read.
                    let send_buf = buf[..block_bytes as usize].to_vec();
                    if pipe.send(WorkItem::Good { pos, buf: send_buf }).is_err() {
                        producer_err = Some(consumer_gone());
                        break 'outer;
                    }
                    bytes_done = bytes_done.saturating_add(block_bytes);
                    pos += block_bytes;
                }
                Err(err) if !opts.skip_on_error => {
                    let (status, sense) = extract_scsi_context(&err);
                    producer_err = Some(Error::DiscRead {
                        sector: block_lba as u64,
                        status: Some(status),
                        sense,
                    });
                    break 'outer;
                }
                Err(err) => {
                    read_err_count += 1;
                    let action = read_error::handle_read_error(&err, &mut read_ctx);

                    match action {
                        read_error::ReadAction::Retry { pause_secs } => {
                            sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                        }
                        read_error::ReadAction::Bisect => {
                            read_ctx.bisecting = true;
                            let saved_batch = read_ctx.batch;
                            read_ctx.batch = 1;
                            let mut bisect_aborted = false;
                            for sector_offset in 0..block_count {
                                if let Some(ref h) = opts.halt
                                    && h.load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    halt_requested = true;
                                    bisect_aborted = true;
                                    break;
                                }
                                let sector_lba = block_lba + (sector_offset as u32);
                                let mut sector_buf = [0u8; 2048];
                                let write_pos = pos + (sector_offset as u64 * 2048);
                                match reader.read_sectors(sector_lba, 1, &mut sector_buf[..], true)
                                {
                                    Ok(_) => {
                                        read_ctx.on_success();
                                        // Plaintext via the wrapping
                                        // DecryptingSectorSource — same
                                        // decrypt path the batch read takes.
                                        if pipe
                                            .send(WorkItem::BisectGood {
                                                pos: write_pos,
                                                buf: Box::new(sector_buf),
                                            })
                                            .is_err()
                                        {
                                            producer_err = Some(consumer_gone());
                                            bisect_aborted = true;
                                            break;
                                        }
                                    }
                                    Err(inner_err) => {
                                        let inner_action = read_error::handle_read_error(
                                            &inner_err,
                                            &mut read_ctx,
                                        );
                                        match inner_action {
                                            read_error::ReadAction::Retry { pause_secs } => {
                                                // Transient (NOT_READY / bridge
                                                // degradation): honour the
                                                // cooldown pause, then mark
                                                // BisectBad and move on. We
                                                // are already inside a
                                                // single-sector retry; a
                                                // second bisect would be
                                                // nonsensical (ctx.bisecting
                                                // is true, so handle_read_error
                                                // can't return Bisect).
                                                sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                                            }
                                            read_error::ReadAction::AbortPass => {
                                                // Transport failure or
                                                // wedge-abort threshold
                                                // reached: stop immediately.
                                                let (status, sense) =
                                                    extract_scsi_context(&inner_err);
                                                producer_err = Some(Error::DiscRead {
                                                    sector: sector_lba as u64,
                                                    status: Some(status),
                                                    sense,
                                                });
                                                bisect_aborted = true;
                                                break;
                                            }
                                            // JumpAhead / SkipBlock: honour
                                            // any indicated pause; the
                                            // bisect-inner loop's job is just
                                            // to classify this specific sector,
                                            // so we still mark BisectBad and
                                            // continue to the next sector.
                                            read_error::ReadAction::JumpAhead {
                                                pause_secs,
                                                ..
                                            }
                                            | read_error::ReadAction::SkipBlock { pause_secs } => {
                                                sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                                            }
                                            // Bisect cannot recurse: ctx.bisecting
                                            // is true so handle_read_error will
                                            // never return Bisect here.
                                            read_error::ReadAction::Bisect => {}
                                        }
                                        if pipe
                                            .send(WorkItem::BisectBad { pos: write_pos })
                                            .is_err()
                                        {
                                            producer_err = Some(consumer_gone());
                                            bisect_aborted = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            read_ctx.bisecting = false;
                            read_ctx.batch = saved_batch;
                            if bisect_aborted {
                                break 'outer;
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);
                            pos += block_bytes;
                        }
                        read_error::ReadAction::SkipBlock { pause_secs } => {
                            if pipe
                                .send(WorkItem::SkipFill {
                                    pos,
                                    len: block_bytes,
                                })
                                .is_err()
                            {
                                producer_err = Some(consumer_gone());
                                break 'outer;
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);
                            sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                            pos += block_bytes;
                        }
                        read_error::ReadAction::JumpAhead {
                            sectors,
                            pause_secs,
                        } => {
                            if pipe
                                .send(WorkItem::SkipFill {
                                    pos,
                                    len: block_bytes,
                                })
                                .is_err()
                            {
                                producer_err = Some(consumer_gone());
                                break 'outer;
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);

                            if !in_damage_zone {
                                in_damage_zone = true;
                                reader.set_speed(0x0000);
                                tracing::debug!(
                                    target: "freemkv::disc",
                                    phase = "damage_enter",
                                    lba = block_lba,
                                    "Entered damage zone; dropping to minimum read speed"
                                );
                            }

                            // Saturating throughout — the read_error side
                            // computes the sector count with saturating_mul as
                            // "defence in depth"; honor the same guarantee at
                            // the consuming multiply/add so a pathological jump
                            // distance can't wrap.
                            let jump_pos = pos
                                .saturating_add(block_bytes)
                                .saturating_add(sectors.saturating_mul(2048))
                                .min(region_end);
                            let gap_start = pos + block_bytes;
                            let gap_bytes = jump_pos.saturating_sub(gap_start);
                            if gap_bytes > 0 {
                                if pipe
                                    .send(WorkItem::GapFill {
                                        pos: gap_start,
                                        len: gap_bytes,
                                    })
                                    .is_err()
                                {
                                    producer_err = Some(consumer_gone());
                                    break 'outer;
                                }
                                bytes_done = bytes_done.saturating_add(gap_bytes);
                            }
                            tracing::warn!(
                                target: "freemkv::disc",
                                phase = "damage_jump",
                                from_lba = block_lba,
                                to_lba = (jump_pos / 2048) as u32,
                                jump_mb = gap_bytes / 1_048_576,
                                "damage-jump"
                            );
                            pos = jump_pos;
                            sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                        }
                        read_error::ReadAction::AbortPass => {
                            let (status, sense) = extract_scsi_context(&err);
                            producer_err = Some(Error::DiscRead {
                                sector: block_lba as u64,
                                status: Some(status),
                                sense,
                            });
                            break 'outer;
                        }
                    }
                }
            }

            iter_count += 1;

            // Drain any consumer-side stats snapshot.
            if let Some(snap) = try_recv_progress(&prog_rx) {
                if let Some(t) = disc.titles.first() {
                    cached_main_title_bad = bytes_bad_in_title(t, &snap.bad_ranges);
                    cached_located = locate_ranges(&snap.bad_ranges, t);
                }
                cached_snapshot = Some(snap);
            }

            let time_due = last_log_time.elapsed() >= std::time::Duration::from_secs(5);
            if iter_count - last_log_iter >= 100 || time_due {
                last_log_iter = iter_count;
                last_log_time = std::time::Instant::now();
                // Promoted trace -> debug ("no silent hangs"): the sweep
                // heartbeat must be visible at the standard debug level, not
                // only the trace firehose. Carries lba/pos/region_end and
                // bytes_good when a consumer snapshot is available.
                let lba = (pos / 2048) as u32;
                if let Some(ref snap) = cached_snapshot {
                    tracing::debug!(
                        target: "freemkv::disc",
                        phase = "iter_progress",
                        iter_count,
                        read_ok_count,
                        read_err_count,
                        lba,
                        pos,
                        region_end,
                        bytes_good = snap.stats.bytes_good,
                        bytes_pending = snap.stats.bytes_pending,
                        copy_elapsed_ms = copy_t0.elapsed().as_millis() as u64,
                        "Disc::sweep inner iter"
                    );
                } else {
                    tracing::debug!(
                        target: "freemkv::disc",
                        phase = "iter_progress",
                        iter_count,
                        read_ok_count,
                        read_err_count,
                        lba,
                        pos,
                        region_end,
                        copy_elapsed_ms = copy_t0.elapsed().as_millis() as u64,
                        "Disc::sweep inner iter"
                    );
                }
                // Throttled stats refresh request — best-effort
                // try_send so a busy consumer doesn't stall the
                // producer; the cached snapshot stays current
                // enough for one more iteration.
                let _ = pipe.try_send(WorkItem::StatsRequest);
            }

            if let Some(reporter) = opts.progress {
                // Use the latest consumer snapshot if we have
                // one; otherwise synthesise a producer-side
                // placeholder. On a fresh sweep, before the
                // first stats round-trip lands, this means
                // bytes_good ≈ bytes_done (producer's notion of
                // good-so-far) and the bad-range list is empty —
                // close enough for an early UI tick; the next
                // real snapshot replaces it.
                let main_title = disc.titles.first();
                let main_title_bad = cached_main_title_bad;
                // The consumer's snapshot is the source of truth for
                // bytes_unreadable / bytes_pending (the producer doesn't
                // see them), but its bytes_good lags producer-side
                // `bytes_done` whenever the consumer is behind on draining
                // the work channel. Take the max so the user-visible
                // counter never regresses below what the producer has
                // already sent — Anomaly B in the 0.18.1 prod test was
                // this regression: a stale early snapshot pinned the
                // display to 0 GB while bytes_done was already advancing.
                let (bytes_good, bytes_unreadable, bytes_pending, bytes_retryable) =
                    match &cached_snapshot {
                        Some(snap) => (
                            snap.stats.bytes_good.max(bytes_done),
                            snap.stats.bytes_unreadable,
                            snap.stats.bytes_pending,
                            snap.stats.bytes_retryable,
                        ),
                        None => (
                            bytes_done,
                            0u64,
                            total_bytes.saturating_sub(bytes_done),
                            0u64,
                        ),
                    };
                let pp = libfreemkv::progress::PassProgress {
                    kind: libfreemkv::progress::PassKind::Sweep,
                    work_done: pos,
                    work_total: total_bytes,
                    bytes_good_total: bytes_good,
                    bytes_unreadable_total: bytes_unreadable,
                    bytes_pending_total: bytes_pending,
                    bytes_retryable_total: bytes_retryable,
                    bytes_total_disc: total_bytes,
                    disc_duration_secs: main_title.map(|t| t.duration_secs),
                    bytes_bad_in_main_title: main_title_bad,
                    main_title_duration_secs: main_title.map(|t| t.duration_secs),
                    main_title_size_bytes: main_title.map(|t| t.size_bytes),
                    // Rendered drilldown from the consumer's in-memory
                    // snapshot (bad ranges) + title; empty until the first
                    // snapshot arrives.
                    located: cached_located.clone(),
                };
                if !reporter.report(&pp) {
                    halt_requested = true;
                    break 'outer;
                }
            }
        }
    }

    // Producer side is done. Drop the channel and let the
    // consumer drain whatever's still in flight, then run its
    // close() (drain writeback, fsync, mapfile.flush) and return
    // the final stats. On consumer panic `pipe.finish` returns
    // the wrapped panic message via Error::IoError — same shape
    // the previous `consumer_handle.join().map_err(...)` produced.
    let summary = pipe.finish();

    // Producer-side error wins over consumer-side (the read failure
    // is what motivated quitting; the consumer's flush error, if
    // any, is downstream).
    if let Some(e) = producer_err {
        // Drop the consumer's result if we already have a producer
        // error, but propagate consumer-panic on top of nothing
        // since that's strictly informative.
        let _ = summary;
        return Err(e);
    }
    let summary = summary?;

    let stats = summary.stats;
    tracing::debug!(
        target: "freemkv::disc",
        phase = "sweep_done",
        iter_count,
        read_ok_count,
        read_err_count,
        bytes_good = stats.bytes_good,
        bytes_pending = stats.bytes_pending,
        halted = halt_requested,
        copy_elapsed_ms = copy_t0.elapsed().as_millis() as u64,
        "Disc::sweep returning"
    );

    // End-of-pass diagnostic summary (added 2026-05-10 alongside
    // the per-error timing instrumentation in read_error.rs).
    // One INFO line per sweep that lets a post-mortem analyst tell
    // at a glance how much damage the disc + drive saw, without
    // grepping through the per-error WARN log. The PassSummary
    // counters come from `ReadCtx`'s accumulated state.
    let pass_sum = read_ctx.pass_summary();
    tracing::info!(
        target: "freemkv::disc",
        phase = "pass1_summary",
        total_reads_ok = pass_sum.total_reads_ok,
        total_errors = pass_sum.total_errors,
        zones_entered = pass_sum.zones_entered,
        jumps_taken = pass_sum.jumps_taken,
        marginal_recovered = pass_sum.marginal_recovered,
        bytes_good = stats.bytes_good,
        bytes_pending = stats.bytes_pending,
        copy_elapsed_ms = copy_t0.elapsed().as_millis() as u64,
        "Pass 1 complete"
    );
    Ok(CopyResult::new(
        total_bytes,
        stats.bytes_good,
        stats.bytes_unreadable,
        stats.bytes_pending,
        0,
        halt_requested,
    ))
}

#[derive(Default)]
pub struct CopyOptions<'a> {
    pub decrypt: bool,
    pub multipass: bool,
    pub progress: Option<&'a dyn libfreemkv::progress::Progress>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// AACS Volume ID (16 bytes) to persist into the mapfile during
    /// Pass 1 so it survives to deferred-mux / resume. `None` for
    /// unencrypted / non-AACS discs. Caller wires this from
    /// `Disc::aacs.volume_id`.
    ///
    /// Persisted ONLY when `unit_keys` is empty (the disc didn't resolve a
    /// key): the VID is the "still unresolved, retry-able" marker.
    pub vid: Option<[u8; 16]>,
    /// Resolved AACS unit keys `(CPS unit, key)` to persist into the mapfile
    /// during Pass 1. When non-empty these are written (the final answer, so
    /// deferred-mux/resume decrypts directly) and the VID is NOT — keys XOR VID.
    /// Caller wires this from `Disc::aacs.unit_keys`.
    pub unit_keys: Vec<(u32, [u8; 16])>,
    /// On-decrypt-miss key fetch (see [`libfreemkv::keysource::key_fetch_factory`]).
    /// When set, a read that hits AACS ciphertext no held key opens asks the
    /// application's key sources for the CPS unit's key, caches it, and retries —
    /// recovering an orphan CPS unit never sampled at resolve time. `None`
    /// disables it (the prior behaviour). Threaded into sweep + patch.
    pub key_fetch: Option<libfreemkv::sector::KeyFetch>,
}

#[derive(Debug, Clone, Copy)]
pub struct CopyResult {
    pub bytes_total: u64,
    pub bytes_good: u64,
    pub bytes_unreadable: u64,
    pub bytes_pending: u64,
    pub recovered_this_pass: u64,
    /// Nothing pending AND nothing permanently lost AND not interrupted.
    /// Derived by [`CopyResult::new`] — never set independently, so it can
    /// never contradict the byte counts it ships beside.
    pub complete: bool,
    pub halted: bool,
}

impl CopyResult {
    /// THE definition of a finished copy. Every construction site goes
    /// through here so "complete" has exactly one meaning: no bytes left to
    /// retry, no bytes permanently lost, and the pass was not interrupted.
    ///
    /// Previously each of the five call sites re-derived this from whichever
    /// local happened to be in scope, and they disagreed on both the
    /// unreadable and the halted term — reporting a lossy or cancelled rip as
    /// complete.
    pub(crate) fn new(
        bytes_total: u64,
        bytes_good: u64,
        bytes_unreadable: u64,
        bytes_pending: u64,
        recovered_this_pass: u64,
        halted: bool,
    ) -> Self {
        CopyResult {
            bytes_total,
            bytes_good,
            bytes_unreadable,
            bytes_pending,
            recovered_this_pass,
            complete: bytes_pending == 0 && bytes_unreadable == 0 && !halted,
            halted,
        }
    }
}

/// Options for [`Disc::sweep`] (Pass 1 / forward sequential pass).
pub struct SweepOptions<'a> {
    pub decrypt: bool,
    pub resume: bool,
    pub batch_sectors: Option<u16>,
    pub skip_on_error: bool,
    pub progress: Option<&'a dyn libfreemkv::progress::Progress>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// AACS Volume ID (16 bytes) persisted into the mapfile when the
    /// sweep creates / opens it. `None` for unencrypted discs. Written ONLY
    /// when `unit_keys` is empty (keys XOR VID — the VID is the retry marker).
    pub vid: Option<[u8; 16]>,
    /// Resolved AACS unit keys persisted into the mapfile when the sweep
    /// creates / opens it. When non-empty these win over `vid`.
    pub unit_keys: Vec<(u32, [u8; 16])>,
    /// On-decrypt-miss key fetch (see [`CopyOptions::key_fetch`]).
    pub key_fetch: Option<libfreemkv::sector::KeyFetch>,
}

/// Options for [`Disc::patch`] (Pass N retry pass over bad ranges).
pub struct PatchOptions<'a> {
    pub decrypt: bool,
    pub block_sectors: Option<u16>,
    pub full_recovery: bool,
    /// Labels the reported [`PassKind`](libfreemkv::progress::PassKind) only.
    /// It does NOT order the walk: `PatchCtx::run` sorts the bad ranges by
    /// (size desc, pos asc), a total order over disjoint runs, so any
    /// pre-ordering is unobservable.
    pub reverse: bool,
    pub wedged_threshold: u64,
    pub progress: Option<&'a dyn libfreemkv::progress::Progress>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// On-decrypt-miss key fetch (see [`CopyOptions::key_fetch`]). Lets Pass N
    /// recover an orphan CPS unit's key when re-reading its bad range.
    pub key_fetch: Option<libfreemkv::sector::KeyFetch>,
}
impl<'a> PatchOptions<'a> {
    /// THE tuning preset for a Pass-N patch pass.
    ///
    /// Both entry points — `patch_internal` (copy's resume dispatch) and
    /// `multipass_rip`'s patch loop — used to spell these four values out as
    /// literals, so the two routes into the same underlying pass could drift
    /// apart on a future tuning change with nothing to catch it. Only one of
    /// the two copies even carried the rationale for `block_sectors`.
    ///
    /// Adaptive batching: patch reads at 32 sectors when the drive is healthy,
    /// drops to 1 on failure to probe each sector individually, then climbs
    /// back after 16 consecutive clean singles. Walks NonTrimmed regions ~32x
    /// faster in clean stretches without sacrificing per-sector recovery
    /// quality — the drop-to-1 retry from the same position guarantees every
    /// sector in a failed batch is individually probed.
    pub fn for_patch_pass(
        decrypt: bool,
        progress: Option<&'a dyn libfreemkv::progress::Progress>,
        halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        key_fetch: Option<libfreemkv::sector::KeyFetch>,
    ) -> Self {
        PatchOptions {
            decrypt,
            block_sectors: Some(32),
            full_recovery: true,
            reverse: true,
            wedged_threshold: 50,
            progress,
            halt,
            key_fetch,
        }
    }
}

/// Result returned by [`Disc::patch`].
pub struct PatchOutcome {
    pub bytes_total: u64,
    pub bytes_good: u64,
    pub bytes_unreadable: u64,
    pub bytes_pending: u64,
    pub bytes_recovered_this_pass: u64,
    pub halted: bool,
    pub wedged_exit: bool,
    pub wedged_threshold: u64,
}

/// Snap a mapfile byte-range out to whole sectors: start down, end up.
///
/// Mapfile ranges are BYTE ranges and nothing guarantees they land on 2048-byte
/// boundaries — the format interoperates with ddrescue, whose `-b 512` writes
/// 512-byte-granular ranges, and a mapfile can be hand-edited or imported.
/// Feeding an unaligned offset to a sector-addressed reader truncates the LBA
/// and shifts real payload to the wrong place, which then gets recorded
/// `Finished`: silent corruption presented as recovery.
///
/// `patch` has always snapped its ingress; `sweep`'s resume path did not, and
/// its only alignment was gated on a decrypting AACS rip — a branch a
/// multipass resume never takes, since multipass implies raw. One
/// implementation here so the two ingresses cannot drift apart.
///
/// Saturating: `(pos + len).div_ceil(SECTOR) * SECTOR` overflows u64 for a
/// range ending in the last sector of the address space, which `Mapfile::load`
/// accepts (it checks `checked_add`, and that does not wrap).
pub(super) fn snap_to_sectors(pos: u64, len: u64) -> (u64, u64) {
    use section_recover::SECTOR;
    let start = pos - pos % SECTOR;
    if len == 0 {
        return (start, 0);
    }
    let end = pos
        .saturating_add(len)
        .div_ceil(SECTOR)
        .saturating_mul(SECTOR);
    (start, end.saturating_sub(start))
}

/// Sleep `secs` seconds, but break early if `halt` flips to true.
/// Used by Pass 1's wedge-avoidance inter-error pause so halt
/// remains responsive regardless of how long the pause is.
/// Polling granularity 100 ms — bounded latency on halt regardless
/// of pause length.
pub(crate) fn sleep_secs_or_halt(
    secs: u64,
    halt: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
) {
    if secs == 0 {
        return;
    }
    let Some(h) = halt else {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        return;
    };
    let total = std::time::Duration::from_secs(secs);
    let slice = std::time::Duration::from_millis(100);
    let start = std::time::Instant::now();
    while start.elapsed() < total {
        if h.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let remaining = total.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(slice));
    }
}

const DEFAULT_BATCH_SECTORS_OPTICAL: u16 = 60;

pub(crate) fn ecc_sectors(format: libfreemkv::DiscFormat) -> u16 {
    match format {
        // BD-family 64 KiB ECC block (32 × 2048). FMTS is a UHD BD disc.
        libfreemkv::DiscFormat::Uhd
        | libfreemkv::DiscFormat::Fmts
        | libfreemkv::DiscFormat::BluRay => 32,
        // 32 KiB ECC block (16 × 2048) — DVD and HD-DVD.
        libfreemkv::DiscFormat::Dvd | libfreemkv::DiscFormat::HdDvd => 16,
        libfreemkv::DiscFormat::Unknown => 32,
    }
}

pub(crate) mod mapfile;
mod patch;
mod read_error;
mod section_recover;
mod sweep;

// The mapfile-backed main-title bad-byte reader, used by the multipass
// abort-on-loss gate (the same figure the CLI/autorip abort gate reads). `pub`
// so the engine can re-export it: a front-end computing "how much of the main
// title is bad" after a rip reads it here rather than the (now-removed)
// libfreemkv method.
pub use patch::bytes_bad_in_title_from_mapfile;

/// One-shot progress snapshot built from a mapfile on disk plus the title.
/// Reads + parses the mapfile HERE so a front-end (autorip) gets a fully
/// rendered [`libfreemkv::progress::PassProgress`] without ever touching
/// mapfile internals — used for the pass-boundary paint (before the live
/// callback stream begins) and the terminal done-card verdict. Returns `None`
/// if the mapfile can't be read. `work_done`/`work_total` are `0`: this is a
/// point-in-time snapshot, not a per-pass progress tick. Relocated from
/// libfreemkv in the engine split (the mapfile it reads now lives here).
pub fn progress_snapshot_from_mapfile(
    mapfile_path: &std::path::Path,
    title: Option<&libfreemkv::DiscTitle>,
    kind: libfreemkv::progress::PassKind,
    bytes_total_disc: u64,
) -> Option<libfreemkv::progress::PassProgress> {
    // `None` here means "no card to paint" and makes no cleanliness claim, so
    // absent and corrupt may both yield None — but corruption must not pass
    // unremarked, which is what `.ok()?` did.
    let map = mapfile::load_if_present(mapfile_path).ok().flatten()?;
    let stats = map.stats();
    // MAYBE set = not-yet-good (NonTrimmed/NonScraped/Unreadable), excluding
    // NonTried (the unread remainder) — same set the live patch emitter uses.
    let maybe = map.ranges_with(&mapfile::damage_sector_statuses());
    let located = title.map(|t| locate_ranges(&maybe, t)).unwrap_or_default();
    let main_bad = title.map(|t| bytes_bad_in_title(t, &maybe)).unwrap_or(0);
    Some(libfreemkv::progress::PassProgress {
        kind,
        work_done: 0,
        work_total: 0,
        bytes_good_total: stats.bytes_good,
        bytes_unreadable_total: stats.bytes_unreadable,
        bytes_pending_total: stats.bytes_pending,
        bytes_retryable_total: stats.bytes_retryable,
        bytes_total_disc,
        disc_duration_secs: title.map(|t| t.duration_secs),
        bytes_bad_in_main_title: main_bad,
        main_title_duration_secs: title.map(|t| t.duration_secs),
        main_title_size_bytes: title.map(|t| t.size_bytes),
        located,
    })
}

#[cfg(test)]
mod snap_tests {
    use super::snap_to_sectors;

    /// Both mapfile ingresses must widen a range to whole sectors.
    ///
    /// A byte range is not a sector range. ddrescue's `-b 512` writes
    /// 512-granular ranges and the mapfile format advertises interop with it,
    /// so an unaligned `pos` reaching a sector-addressed reader truncates the
    /// LBA and shifts real payload — recorded afterwards as Finished.
    #[test]
    fn an_unaligned_range_widens_to_whole_sectors() {
        // Mid-sector start: anchor down, and cover the tail.
        assert_eq!(snap_to_sectors(512, 1024), (0, 2048));
        // Spanning a boundary: cover both sectors.
        assert_eq!(snap_to_sectors(2048 - 512, 1024), (0, 4096));
        // Already aligned: unchanged.
        assert_eq!(snap_to_sectors(4096, 2048), (4096, 2048));
        // Zero length keeps the anchored start and stays empty.
        assert_eq!(snap_to_sectors(700, 0), (0, 0));
    }

    /// Rounding up must not wrap at the top of the address space.
    ///
    /// `Mapfile::load` accepts a range ending in the last sector — it checks
    /// `checked_add`, which does not wrap — and the old
    /// `(pos + len).div_ceil(SECTOR) * SECTOR` then overflowed u64: a panic in
    /// dev, and in release a wrap to 0 that hands the recovery handlers a
    /// fabricated ~2^64-byte span to walk.
    #[test]
    fn rounding_up_saturates_at_the_end_of_the_address_space() {
        let (start, len) = snap_to_sectors(u64::MAX - 1023, 1024);
        assert_eq!(start % 2048, 0, "start stays sector-aligned");
        assert!(
            start.checked_add(len).is_some(),
            "snapped range wrapped past u64::MAX: start={start} len={len}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Region-start alignment, asserted against PRODUCTION.
    ///
    /// This test used to re-derive `region_pos - (region_pos % unit_bytes)`
    /// inline and check properties of its own arithmetic, so breaking the real
    /// sweep left it green. It now calls `aacs_aligned_region_start`.
    ///
    /// The batch half is deliberately NOT retested here — `aacs_aligned_batch`
    /// has its own production-calling module above; duplicating it would be a
    /// strictly weaker second copy.
    #[test]
    fn aacs_region_start_anchors_down_to_a_unit_boundary() {
        let unit = libfreemkv::aacs::content::ALIGNED_UNIT_LEN as u64; // 6144

        // EXACT expected values. Properties alone are not enough: an
        // implementation that always returned 0 would satisfy "is unit
        // aligned" and "moved down" while silently discarding every byte of
        // resume progress.
        for (pos, want) in [
            (0u64, 0u64),
            (2048, 0),
            (4096, 0),
            (6144, 6144),
            (8192, 6144),
            (65_536, 61_440),
            (67_584, 67_584),
        ] {
            assert_eq!(
                aacs_aligned_region_start(pos, true),
                want,
                "region {pos} must anchor to {want}"
            );
        }

        // And the tightness bound the exact values encode, over a wider sweep:
        // the cursor lands on the NEAREST boundary at or below `pos`, never a
        // whole unit further back.
        for pos in (0u64..40_000).step_by(512) {
            let got = aacs_aligned_region_start(pos, true);
            assert_eq!(got % unit, 0, "{pos} -> {got} is not unit-aligned");
            assert!(got <= pos, "{pos} -> {got} moved the cursor UP");
            assert!(pos - got < unit, "{pos} -> {got} skipped a whole unit back");
        }

        // A non-AACS sweep has no unit geometry; the cursor is untouched.
        for pos in [0u64, 2048, 8192, 67_583] {
            assert_eq!(aacs_aligned_region_start(pos, false), pos);
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// The pass-boundary / done-card snapshot must actually read the mapfile.
    ///
    /// `progress_snapshot_from_mapfile` is a public engine surface autorip
    /// calls twice — once between passes and once for the terminal verdict
    /// card — and it had no test of its own. `None` means "no card to paint",
    /// which is a legitimate answer for an absent mapfile and therefore an
    /// answer the whole function could be replaced by: the operator's damage
    /// summary silently stops appearing and nothing says why.
    #[test]
    fn a_snapshot_reports_the_damage_the_mapfile_records() {
        let dir = std::env::temp_dir().join(format!("fmkv-engine-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mf = dir.join("snap.map");

        // 200 sectors: 150 good, 50 confirmed unreadable.
        let total = 200u64 * 2048;
        {
            let mut map = mapfile::Mapfile::create(&mf, total, "test").unwrap();
            map.record(0, 150 * 2048, mapfile::SectorStatus::Finished)
                .unwrap();
            map.record(150 * 2048, 50 * 2048, mapfile::SectorStatus::Unreadable)
                .unwrap();
            map.flush().unwrap();
        }

        let mut title = libfreemkv::DiscTitle::empty();
        title.duration_secs = 7200.0;
        title.size_bytes = total;
        title.extents = vec![libfreemkv::disc::Extent {
            start_lba: 0,
            sector_count: 200,
        }];

        let snap = progress_snapshot_from_mapfile(
            &mf,
            Some(&title),
            libfreemkv::progress::PassKind::Sweep,
            total,
        )
        .expect("a readable mapfile has a card to paint");

        assert_eq!(snap.bytes_good_total, 150 * 2048);
        assert_eq!(snap.bytes_unreadable_total, 50 * 2048);
        assert_eq!(
            snap.bytes_bad_in_main_title,
            50 * 2048,
            "the damage falls inside the title's only extent"
        );
        assert_eq!(snap.bytes_total_disc, total);
        assert_eq!(snap.main_title_size_bytes, Some(total));
        // A snapshot is a point in time, not a pass tick.
        assert_eq!((snap.work_done, snap.work_total), (0, 0));

        // And the absent case genuinely is None, so the assertion above is
        // about content and not merely about the file existing.
        assert!(
            progress_snapshot_from_mapfile(
                &dir.join("nope.map"),
                Some(&title),
                libfreemkv::progress::PassKind::Sweep,
                total,
            )
            .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
