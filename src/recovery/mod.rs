//! freemkv's recovery strategy — relocated here from libfreemkv per the
//! engine-split design (`freemkv-private/audit/engine-split/DESIGN.md` §1).
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

use patch::patch;

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
    disc.ensure_decryptable(!opts.decrypt)?;
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
        if covers_disc && bad_bytes == 0 && stats.bytes_nontried == 0 {
            // Every sector is Finished — a prior copy completed. Re-issuing
            // the command is a no-op (don't re-sweep a finished ISO).
            return Ok(CopyResult {
                bytes_total: disc_size,
                bytes_good: stats.bytes_good,
                bytes_unreadable: stats.bytes_unreadable,
                bytes_pending: 0,
                recovered_this_pass: 0,
                complete: true,
                halted: false,
            });
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
            return Ok(CopyResult {
                bytes_total: disc_size,
                bytes_good: stats.bytes_good,
                bytes_unreadable: stats.bytes_unreadable,
                bytes_pending: 0,
                recovered_this_pass: 0,
                complete: false,
                halted: false,
            });
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
        return Ok(CopyResult {
            bytes_total: disc_size,
            bytes_good: stats.bytes_good,
            bytes_unreadable: stats.bytes_unreadable,
            bytes_pending: stats.bytes_pending,
            recovered_this_pass: 0,
            complete: bad_bytes == 0,
            halted: false,
        });
    }
    sweep_internal(disc, reader, path, opts, false)
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
    let patch_opts = PatchOptions {
        decrypt: opts.decrypt,
        // 0.18.13: adaptive batching. patch() reads at 32 sectors
        // when the drive is healthy, drops to 1 on failure to
        // probe each sector individually, then climbs back after
        // 16 consecutive clean singles. Walks NonTrimmed regions
        // ~32x faster in clean stretches without sacrificing any
        // per-sector recovery quality — the drop-to-1 retry from
        // the same position guarantees every sector in a failed
        // batch is individually probed. See Disc::patch body.
        block_sectors: Some(32),
        full_recovery: true,
        reverse: true,
        wedged_threshold: 50,
        progress: opts.progress,
        halt: opts.halt.clone(),
        key_fetch: opts.key_fetch.clone(),
    };
    let pr = patch(disc, reader, path, &patch_opts)?;
    tracing::info!(
        target: "freemkv::disc",
        phase = "patch_done",
        bytes_recovered = pr.bytes_recovered_this_pass,
        halted = pr.halted,
        wedged_exit = pr.wedged_exit,
        "Patch completed"
    );
    Ok(CopyResult {
        bytes_total: pr.bytes_total,
        bytes_good: pr.bytes_good,
        bytes_unreadable: pr.bytes_unreadable,
        bytes_pending: pr.bytes_pending,
        recovered_this_pass: pr.bytes_recovered_this_pass,
        complete: pr.bytes_pending == 0,
        halted: pr.halted,
    })
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
    disc.ensure_decryptable(!opts.decrypt)?;

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
                    let iso_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    let claims_progress = existing.stats().bytes_pending != existing.total_size();
                    if iso_len == 0 && claims_progress {
                        tracing::info!(
                            "sweep: mapfile claims prior progress (pending {} of {}) but ISO is missing/zero-length; forcing fresh sweep",
                            existing.stats().bytes_pending,
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
    let (file, is_regular) = if resume
        && std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    {
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
        Some(b) => b,
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
    const UNIT_SECTORS: u16 = (libfreemkv::aacs::content::ALIGNED_UNIT_LEN / 2048) as u16; // 3
    if decrypt_is_aacs && batch % UNIT_SECTORS != 0 {
        batch = batch.saturating_add(UNIT_SECTORS - (batch % UNIT_SECTORS));
    }

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
        let region_end = region_pos + region_size;
        // AACS unit alignment: anchor the region's read cursor DOWN to the
        // nearest 6144-byte unit boundary so the decrypting reader never gets
        // a buffer that starts mid-unit. Re-reading the few already-covered
        // head sectors is idempotent (they re-decrypt identically and the
        // consumer overwrites the same ISO offsets / mapfile ranges). A fresh
        // sweep's NonTried region starts at 0, already unit-aligned; this only
        // shifts resume regions that begin mid-unit.
        let mut pos = if decrypt_is_aacs {
            let unit_bytes = libfreemkv::aacs::content::ALIGNED_UNIT_LEN as u64;
            region_pos - (region_pos % unit_bytes)
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
            if let Some(ref h) = opts.halt {
                if h.load(std::sync::atomic::Ordering::Relaxed) {
                    halt_requested = true;
                    break 'outer;
                }
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
                                if let Some(ref h) = opts.halt {
                                    if h.load(std::sync::atomic::Ordering::Relaxed) {
                                        halt_requested = true;
                                        bisect_aborted = true;
                                        break;
                                    }
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
                let main_title_bad = match &cached_snapshot {
                    Some(snap) => disc
                        .titles
                        .first()
                        .map(|t| bytes_bad_in_title(t, &snap.bad_ranges))
                        .unwrap_or(0),
                    None => 0,
                };
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
                    located: match &cached_snapshot {
                        Some(snap) => main_title
                            .map(|t| locate_ranges(&snap.bad_ranges, t))
                            .unwrap_or_default(),
                        None => libfreemkv::progress::LocatedProgress::default(),
                    },
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
    Ok(CopyResult {
        bytes_total: total_bytes,
        bytes_good: stats.bytes_good,
        bytes_unreadable: stats.bytes_unreadable,
        bytes_pending: stats.bytes_pending,
        recovered_this_pass: 0,
        complete: stats.bytes_pending == 0 && !halt_requested,
        halted: halt_requested,
    })
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
    pub complete: bool,
    pub halted: bool,
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
    pub reverse: bool,
    pub wedged_threshold: u64,
    pub progress: Option<&'a dyn libfreemkv::progress::Progress>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// On-decrypt-miss key fetch (see [`CopyOptions::key_fetch`]). Lets Pass N
    /// recover an orphan CPS unit's key when re-reading its bad range.
    pub key_fetch: Option<libfreemkv::sector::KeyFetch>,
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
// abort-on-loss gate (the same figure the CLI/autorip abort gate reads).
pub(crate) use patch::bytes_bad_in_title_from_mapfile;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aacs_sweep_batch_and_region_are_unit_aligned() {
        const UNIT_SECTORS: u16 = (libfreemkv::aacs::content::ALIGNED_UNIT_LEN / 2048) as u16; // 3
        let unit_bytes = libfreemkv::aacs::content::ALIGNED_UNIT_LEN as u64; // 6144

        // (a) Batch rounding: ecc_sectors() for UHD/BD is 32, not a multiple of 3.
        // The decrypting-AACS path rounds it up to the next multiple of 3 (33).
        for format in [libfreemkv::DiscFormat::Uhd, libfreemkv::DiscFormat::BluRay] {
            let mut batch = ecc_sectors(format);
            assert_eq!(batch, 32);
            if batch % UNIT_SECTORS != 0 {
                batch = batch.saturating_add(UNIT_SECTORS - (batch % UNIT_SECTORS));
            }
            assert_eq!(batch, 33, "batch must round 32 -> 33 (a multiple of 3)");
            assert_eq!(batch % UNIT_SECTORS, 0);
            // Every full batch read is then a whole number of 6144-byte units.
            assert_eq!((batch as u64 * 2048) % unit_bytes, 0);
        }

        // (b) Region-start down-alignment. A resume NonTried region can begin
        // mid-unit; aligning the read cursor DOWN to the nearest unit boundary
        // makes block_lba % 3 == 0 for the first (and thus every) batch read.
        // Re-reading the few head sectors is idempotent.
        for region_pos in [0u64, 2048, 4096, 6144, 8192, 65536, 67_584] {
            let pos = region_pos - (region_pos % unit_bytes);
            assert_eq!(pos % unit_bytes, 0, "aligned cursor must be unit-aligned");
            assert!(pos <= region_pos, "alignment only moves the cursor down");
            // block_lba derived as pos/2048 must be a multiple of 3 sectors.
            assert_eq!((pos / 2048) % UNIT_SECTORS as u64, 0);
        }
        // An already-aligned region (fresh sweep starts at 0) is unchanged.
        assert_eq!(0u64 - (0u64 % unit_bytes), 0);
        assert_eq!(6144u64 - (6144u64 % unit_bytes), 6144);
    }
}
