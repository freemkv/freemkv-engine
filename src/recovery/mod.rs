//! freemkv's recovery strategy — relocated here from libfreemkv per the
//! engine-split design (see this crate's top-level docs).
//!
//! `mapfile.rs` (the ddrescue-format damage record), `read_error.rs` (the read-error
//! policy), `section_recover.rs` and `patch.rs` (the patch passes), `whole_disc.rs` (the
//! decrypting whole-disc reader) and the private `sweep.rs` consumer; the sweep producer,
//! the copy dispatch and the resume/identity/raw-mode/scope rules live here.

use crate::engine_halt::{EngineHalt, EngineOutcome};
use libfreemkv::disc::{bytes_bad_in_title, locate_ranges};
use libfreemkv::error::{Error, Result};
use libfreemkv::extract_scsi_context;
use libfreemkv::sector::SectorSource;

pub(crate) use patch::patch_in;
pub use patch::{patch, patch_with};

/// A genuine read fault — the drive answered with a status/sense, or the
/// transport died under it. Everything else (a decrypt refusal, a contract
/// violation, a terminated source) is not disc damage.
pub(crate) fn is_read_fault(err: &Error) -> bool {
    matches!(
        err,
        Error::ScsiError { .. }
            | Error::DiscRead { .. }
            | Error::IoError { .. }
            | Error::DeviceNotFound { .. }
    )
}

/// The decrypting readers' on-arrival loud stop: a READ unit no held key opens (KU §2.4:
/// "No held key opens U → loud stop: E7022 (title) or E7032 (image or folder)"). Fatal in
/// every pass: never retried, skipped, zero-filled or counted as damage.
pub(crate) fn is_key_stop(err: &Error) -> bool {
    matches!(err, Error::NoDiscKey { .. } | Error::WholeDiscKeyMissing)
}

/// Whether a failed read may enter damage handling. The sweep takes `Halted` as a stop
/// before asking; `section_recover` leaves the span bad and its handler's halt check ends
/// the chain `Halted` (a Fatal there would turn a patch Stop into `Err(Halted)`).
pub(crate) fn is_damage_candidate(err: &Error) -> bool {
    !is_key_stop(err) && (is_read_fault(err) || matches!(err, Error::Halted))
}

/// Label the error that aborted a pass at `block_lba`.
///
/// Only a MEDIUM/TRANSPORT fault becomes [`Error::DiscRead`] (E6000,
/// "disc may be dirty — clean it"); every other error keeps its own code.
/// This used to relabel *every* error, misreporting decrypt refusals as
/// bogus `E6000 status 0x00` reads (freemkv/freemkv#55). A `DiscRead`
/// with status `0x00` and no sense data is that bug's signature.
fn classify_pass_abort(err: Error, block_lba: u32) -> Error {
    match err {
        // Already a DiscRead: its sector is the source's own (at least as
        // precise as the batch start) and a `None` status must stay `None`.
        e @ Error::DiscRead { .. } => e,
        // A raw SCSI or dead-bus fault, re-anchored to the block we were reading.
        e if is_read_fault(&e) => {
            let (status, sense) = extract_scsi_context(&e);
            Error::DiscRead {
                sector: block_lba as u64,
                status: Some(status),
                sense,
            }
        }
        // A decrypt refusal, a halt, a contract violation: the cause IS the
        // error. Surface it so the real code reaches the user and the logs.
        other => other,
    }
}

// A SHORT transfer is a FAILED read, never a partial success.
fn require_full_read(result: Result<usize>, requested: usize, lba: u32) -> Result<usize> {
    match result {
        Ok(n) if n == requested => Ok(n),
        Ok(n) => {
            // Mirror `Drive::read_one`'s WARN on the same event, so a hit here isn't
            // indistinguishable in the log from an ordinary bad sector. `status`/
            // `sense` are None: read succeeded, so `transferred` vs `expected` is it.
            tracing::warn!(
                target: "freemkv::disc",
                lba,
                transferred = n,
                expected = requested,
                code = libfreemkv::error::E_DISC_READ,
                "read returned success with a residual underrun; refusing the short transfer"
            );
            Err(Error::DiscRead {
                sector: lba as u64,
                status: None,
                sense: None,
            })
        }
        other => other,
    }
}

#[cfg(test)]
#[path = "mod_pass_abort_tests.rs"]
mod pass_abort_tests;

/// [`copy()`] under the op token `op` (stop design v5 §4.2): the token is observed at every
/// read, pause and hand-off, OR'd with `opts.halt`. A Stop is [`EngineOutcome::Stopped`].
pub fn copy_with(
    op: &libfreemkv::Halt,
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
) -> EngineOutcome<CopyResult> {
    let halt = EngineHalt::new(op, opts.halt.clone());
    let r = copy_in(disc, reader, path, opts, &halt);
    EngineOutcome::from_result(r, &halt, |r| r.halted)
}

pub fn copy(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
) -> Result<CopyResult> {
    copy_in(
        disc,
        reader,
        path,
        opts,
        &EngineHalt::legacy(opts.halt.clone()),
    )
}

// `copy` under `halt` (its `opts.halt` is already in it).
pub(crate) fn copy_in(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
    halt: &EngineHalt<'_>,
) -> Result<CopyResult> {
    // Pre-flight decrypt gate: without it, a decrypting copy of an encrypted disc
    // with no usable key would silently write ciphertext to the ISO and still
    // return Ok at exit 0. `--raw` (opts.decrypt == false) makes this a no-op.
    crate::resolve::ensure_decryptable_with(disc, !opts.decrypt, opts.keys.as_ref())?;
    // AACS BD Pre-recorded 0.953 §3.7 Note: "PC Host shall decrypt bus-encrypted Clip AV
    // stream file". One the drive's bus map could not locate would land here still encrypted.
    libfreemkv::sector::bus_removal::ensure_image_debussable(reader)?;
    // A zero-capacity disc (READ CAPACITY failed during scan, swallowed to 0) drives
    // every resume/complete decision below off `capacity_bytes == 0` and writes a
    // 0-byte ISO reported as done. Reject it here, before dispatch, as `Error::EmptyImage`.
    disc.image_read_sectors()?;
    // Mapfile-driven resume dispatch, shared by plain and `--multipass` copies: an
    // interrupted run leaves a crash-safe mapfile, so re-issuing must resume, not
    // re-sweep from 0. Multipass also dispatches to patch on retryable bytes.
    let mf_path = disc.mapfile_for(path);
    if mf_path.exists() {
        // A corrupt map restarts clean, as the sweep's own resume does; an unreadable one fails.
        let mut map = match mapfile::Mapfile::load(&mf_path) {
            Ok(map) => map,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                tracing::info!("copy dispatch: → sweep (mapfile is corrupt/unparseable)");
                return sweep_internal(disc, reader, path, opts, false, halt);
            }
            Err(e) => return Err(Error::from(e)),
        };
        // Another disc's map with no image beside it guards no data (a consumer deleted the
        // ISO after muxing it): start fresh instead of refusing every later rip.
        if mapfile::check_mapfile_identity(&map, disc, opts.keys.as_ref()).is_err()
            && no_image(path)?
        {
            tracing::info!("copy dispatch: → sweep (another disc's mapfile has no image)");
            return sweep_internal(disc, reader, path, opts, false, halt);
        }
        // A scoped (MKV-staging) image resumed as iso://: the gate above proved every
        // stream file is now located, so widen it and let the dispatch fill the rest.
        if map.scope().is_some() {
            mapfile::check_mapfile_identity(&map, disc, opts.keys.as_ref()).map_err(Error::from)?;
            map.clear_scope();
            map.flush().map_err(Error::from)?;
        }
        // BEFORE any resume decision, including "already complete" below: a wrong
        // disc whose predecessor finished would otherwise report the job done
        // having never touched the disc actually in the drive.
        mapfile::check_mapfile_identity(&map, disc, opts.keys.as_ref()).map_err(Error::from)?;
        if map.raw().is_some_and(|raw| raw == opts.decrypt) {
            // An image of the other raw/decrypt mode: resuming or patching it would mix the two.
            tracing::warn!("copy dispatch: image is in the other raw/decrypt mode; overwriting it");
            return sweep_internal(disc, reader, path, opts, false, halt);
        }
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
        // Mapfile and ISO are separate files; checking only the mapfile risks a false
        // "disc complete" verdict when the image was removed/truncated. Classify via
        // `iso_len_from_metadata`, not `unwrap_or(0)`, so a stat blip can't re-rip a disc.
        let image = image_state(path, disc_size)?;
        let iso_len = image.len;
        let iso_is_intact = image.is_intact();
        if covers_disc && bad_bytes == 0 && stats.bytes_nontried == 0 && !iso_is_intact {
            // Complete mapfile, but the image is gone or short. A resume can't repair
            // this — no NonTried ranges means the producer builds no work — so force
            // a fresh full sweep, as the covers_disc=false case below does.
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
            return sweep_internal(disc, reader, path, opts, false, halt);
        }
        // `bytes_nontried == 0` is implied by `bad_bytes == 0` (subset), so inverting it is an
        // equivalent mutant — don't chase it.
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
            // Mapfile capacity != disc capacity: force a full (non-resume) sweep so
            // [0, disc_size) is covered fresh. Under-cover would abandon the readable
            // tail; over-cover would let a resume sweep read LBAs past disc capacity.
            tracing::info!(
                "copy dispatch: → sweep (covers_disc=false, resume=false, map={}, disc={})",
                map.total_size(),
                disc_size,
            );
            return sweep_internal(disc, reader, path, opts, false, halt);
        }
        // NonTried bytes mean a prior sweep was halted (Ctrl-C/crash) mid-way — route
        // to resume sweep FIRST, even with retryable bytes present, since patch only
        // revisits bad ranges, never NonTried ones (also the plain-copy resume path).
        if stats.bytes_nontried > 0 {
            tracing::info!(
                "copy dispatch: → sweep resume (covers_disc=true, \
                 nontried={}, retryable={})",
                stats.bytes_nontried,
                stats.bytes_retryable,
            );
            return sweep_internal(disc, reader, path, opts, true, halt);
        }
        // From here covers_disc=true and nontried=0: the whole disc was
        // attempted. Only the retry/patch decision differs by mode.
        if opts.multipass {
            if stats.bytes_retryable > 0 {
                tracing::info!(
                    "copy dispatch: → patch (retryable={})",
                    stats.bytes_retryable,
                );
                return patch_internal(disc, reader, path, opts, halt);
            }
            // Fallthrough: nontried=0, retryable=0 — all sectors attempted, remaining
            // bad bytes are already Unreadable. Resume sweep/patch would both be
            // no-ops, so return the terminal result immediately.
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
        // Plain copy has no patch pass and the sweep aborts on the first read error,
        // so a fully-attempted mapfile with bad bytes is terminal. Re-running must
        // not restart from sector 0, so return terminal to surface the failure.
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
    sweep_internal(disc, reader, path, opts, false, halt)
}

// What goes in the mapfile's `# Rescue Logfile. Created by …` header — must name the crate that
// actually wrote the file.
pub(crate) const MAPFILE_CREATOR: &str = concat!("freemkv-engine v", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
#[path = "mod_sleep_secs_or_halt_tests.rs"]
mod sleep_secs_or_halt_tests;

#[cfg(test)]
#[path = "mod_mapfile_creator_tests.rs"]
mod mapfile_creator_tests;

// What the output image's length is, for a resume decision. `NotFound` is the ONE error meaning
// "no file yet"; every other error must propagate rather than be treated as zero.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IsoLen {
    /// The file is genuinely absent.
    Missing,
    /// The file exists and is this many bytes long.
    Len(u64),
}

pub(crate) fn iso_len_from_metadata(m: std::io::Result<std::fs::Metadata>) -> Result<IsoLen> {
    match m {
        Ok(md) => Ok(IsoLen::Len(md.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IsoLen::Missing),
        Err(e) => Err(Error::from(e)),
    }
}

// The image a mapfile describes, measured against the length it should be — ONE definition
// shared by `copy`, `sweep` AND `patch` so the "is this image trustworthy" check can't drift
// between call sites.
pub(crate) struct ImageState {
    // Length on disk; a missing file reports 0 (self-heals the same as empty).
    pub(crate) len: u64,
    /// Length the mapfile says the image should be.
    pub(crate) want: u64,
}

impl ImageState {
    // Exactly the length it should be. A LONGER file is not intact either.
    pub(crate) fn is_intact(&self) -> bool {
        self.len == self.want
    }

    // Short of what the mapfile describes — trusting it would invent
    // recovered data that was never actually read.
    pub(crate) fn is_short(&self) -> bool {
        self.len < self.want
    }
}

// Measure `path` against the length a mapfile expects of it. A stat failure other than "not
// found" is an error, never silently 0.
// A device has no length to measure (it stats as 0): it holds what was written to it.
pub(crate) fn image_state(path: &std::path::Path, want: u64) -> Result<ImageState> {
    let meta = std::fs::metadata(path);
    if meta.as_ref().is_ok_and(|m| !m.is_file()) {
        return Ok(ImageState { len: want, want });
    }
    let len = match iso_len_from_metadata(meta)? {
        IsoLen::Missing => 0,
        IsoLen::Len(n) => n,
    };
    Ok(ImageState { len, want })
}

// No recovered data at `path`: missing, or an EMPTY REGULAR file. A device reports length 0
// whatever it holds, so it never counts as empty. A stat error other than "not found" is an error.
pub(crate) fn no_image(path: &std::path::Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(m.is_file() && m.len() == 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(Error::from(e)),
    }
}

// Whether the output is a REGULAR FILE — governs sync_all-failure severity and pre-sizing. A
// metadata error defaults to `true` (patch's prior behavior).
pub(crate) fn output_is_regular(m: std::io::Result<std::fs::Metadata>) -> bool {
    m.map(|md| md.file_type().is_file()).unwrap_or(true)
}

// A fresh sweep MUST start from an empty mapfile: if the stale file
// survives, the NEW disc inherits the OLD disc's `Finished` ranges and the
// ISO is silently zero-filled there. `NotFound` is fine; anything else aborts.
pub(crate) fn stale_mapfile_removed(r: std::io::Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::from(e)),
    }
}

// The batch size a sweep reads in, before AACS unit alignment. `None` means
// "the mode's default" (skip-on-error: one ECC block; clean: the larger
// optical batch). A zero request is clamped to 1 so `pos` keeps advancing.
pub(crate) fn sweep_batch_sectors(
    requested: Option<u16>,
    skip_on_error: bool,
    format: libfreemkv::DiscFormat,
) -> u16 {
    match requested {
        Some(b) => b.max(1),
        None if skip_on_error => ecc_sectors(format),
        None => DEFAULT_BATCH_SECTORS_OPTICAL,
    }
}

// Deadline for ONE producer→consumer handoff (T9): re-armed per send, so already stall-shaped.
// Reuses `JOIN_TIMEOUT_SECS`, since ST-L2 `finish_with_halt`'s 600 s no-progress window (T7).
const SEND_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(libfreemkv::io::pipeline::JOIN_TIMEOUT_SECS);

// Why a send failed: Stop pressed vs. consumer died vs. consumer alive but stalled past
// `SEND_DEADLINE`. `send_with_halt` collapses all three into `Err(item)`
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SendStall {
    /// The halt token fired while the producer was waiting for a slot. Not an
    /// error: the caller reports the pass as halted and drops the item.
    Halted,
    /// The consumer thread is gone (panicked / receiver dropped), or its `apply`
    /// failed and it now discards every item.
    ConsumerGone,
    /// The consumer is ALIVE but has not taken the item within
    /// [`SEND_DEADLINE`] — the hung-mount case.
    Stalled,
}

impl SendStall {
    /// Numeric library error for the two fatal cases. `Halted` is included for
    /// completeness but callers handle it as an outcome, not an error.
    pub(crate) fn into_error(self) -> Error {
        match self {
            SendStall::Halted => Error::Halted,
            SendStall::ConsumerGone => Error::PipelineConsumerGone,
            // The only TimedOut-kind pipeline variant: "the consumer did not
            // drain within its deadline". `finish_with_halt` returns it for the
            // same condition observed at join instead of at send.
            SendStall::Stalled => Error::PipelineJoinTimeout,
        }
    }

    /// What a failed hand-off makes of the pass: `None` for a Stop (a parked send is a
    /// halt, not a failure), else the error that fails it.
    pub(crate) fn pass_error(self) -> Option<Error> {
        match self {
            SendStall::Halted => None,
            stall => Some(stall.into_error()),
        }
    }
}

// Halt-aware, deadline-bounded replacement for `pipe.send(item)`. Fixes a plain
// `Pipeline::send` blocking forever on a stalled-but-alive consumer.
pub(crate) fn send_bounded<I: Send + 'static, R: Send + 'static>(
    pipe: &libfreemkv::io::pipeline::Pipeline<I, R>,
    item: I,
    halt: &libfreemkv::halt::Halt,
) -> std::result::Result<(), SendStall> {
    send_bounded_within(pipe, item, halt, SEND_DEADLINE)
}

/// [`send_bounded`] with the deadline as a parameter. Exists so the
/// deadline-elapsed branch is testable in milliseconds instead of the ten real
/// minutes [`SEND_DEADLINE`] is (deliberately) set to.
fn send_bounded_within<I: Send + 'static, R: Send + 'static>(
    pipe: &libfreemkv::io::pipeline::Pipeline<I, R>,
    item: I,
    halt: &libfreemkv::halt::Halt,
    deadline: std::time::Duration,
) -> std::result::Result<(), SendStall> {
    // A failed consumer drains and discards, so the fast path below would accept every item
    // and the producer read on for a write that already failed.
    if pipe.consumer_failed() {
        return Err(SendStall::ConsumerGone);
    }
    // Try ONCE, without blocking, before halt gets a vote: `send_with_halt` polls halt
    // first and would otherwise discard an item that took real drive time to produce.
    // Nothing here can block; `Disconnected` falls through too so diagnosis stays put.
    let item = match pipe.try_send(item) {
        Ok(()) => return Ok(()),
        Err(e) => e.into_inner(),
    };
    match pipe.send_with_halt(item, halt, deadline) {
        Ok(()) => Ok(()),
        Err(item) => {
            if halt.is_cancelled() {
                return Err(SendStall::Halted);
            }
            if pipe.consumer_failed() {
                return Err(SendStall::ConsumerGone);
            }
            // Not halted: item came back for disconnect/deadline/fatal-apply. One probe
            // separates them; `Ok` stays a success even if apply failed (finish() surfaces
            // the real error). Uses `is_disconnected()`: libfreemkv doesn't re-export the type.
            match pipe.try_send(item) {
                Ok(()) => Ok(()),
                Err(e) if e.is_disconnected() => Err(SendStall::ConsumerGone),
                Err(_) => Err(SendStall::Stalled),
            }
        }
    }
}

// The error that fails a pass whose producer stopped with `producer`, given the consumer's
// teardown result: the producer's, unless the consumer's `apply` had already failed.
pub(crate) fn pass_failure<R>(
    producer: Error,
    consumer: Result<R>,
    consumer_failed: bool,
) -> Error {
    match consumer {
        Err(cause) if consumer_failed => cause,
        _ => producer,
    }
}

// A failed image write/seek: earlier records may cover bytes that never reach disk (a
// latched writeback error surfaces on a LATER write), so the mapfile stops persisting.
pub(crate) fn image_write_failed(map: &mapfile::Mapfile, e: std::io::Error) -> Error {
    map.disown_handle().disown();
    Error::from(e)
}

// Halt-aware teardown, join-side sibling of `send_bounded`: a wedged-but-alive consumer gets a
// grace spin, then is abandoned, instead of blocking `finish` forever.
pub(crate) fn finish_bounded<I: Send + 'static, R: Send + 'static>(
    pipe: libfreemkv::io::pipeline::Pipeline<I, R>,
    halt: &libfreemkv::halt::Halt,
) -> Result<R> {
    // `Some(halt)` even with no Stop bit wired (a never-cancelled default) still arms the
    // join's `JOIN_TIMEOUT_SECS` window: 600 s with no consumer progress, not a total (T7).
    pipe.finish_with_halt(Some(halt))
}

// `finish_bounded` for a sink that owns the `Mapfile`: on failed teardown it DISOWNS it, so an
// abandoned-but-running consumer can't clobber a resumed pass's confirmed progress.
pub(crate) fn finish_bounded_disowning<I: Send + 'static, R: Send + 'static>(
    pipe: libfreemkv::io::pipeline::Pipeline<I, R>,
    halt: &libfreemkv::halt::Halt,
    disown: &mapfile::MapfileDisown,
) -> Result<R> {
    let result = finish_bounded(pipe, halt);
    if let Err(ref e) = result {
        disown.disown();
        tracing::warn!(
            target: "freemkv::disc",
            phase = "finish.mapfile_disowned",
            error = %e,
            "pipeline teardown failed; revoking the consumer's mapfile so an \
             abandoned writer cannot overwrite a later pass's record"
        );
    }
    result
}

fn sweep_internal(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
    resume: bool,
    halt: &EngineHalt<'_>,
) -> Result<CopyResult> {
    let sweep_opts = SweepOptions {
        decrypt: opts.decrypt,
        resume,
        batch_sectors: None,
        skip_on_error: opts.multipass,
        progress: opts.progress,
        halt: opts.halt.clone(),
        keys: opts.keys.clone(),
    };
    sweep_in(disc, reader, path, &sweep_opts, None, halt)
}

fn patch_internal(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
    halt: &EngineHalt<'_>,
) -> Result<CopyResult> {
    let patch_opts = PatchOptions {
        keys: opts.keys.clone(),
        ..PatchOptions::for_patch_pass(opts.decrypt, opts.progress, opts.halt.clone())
    };
    let pr = patch::patch_in(disc, reader, path, &patch_opts, halt)?;
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

/// Pass 1 of a multipass rip: walk the disc forward, write every readable
/// sector into `path`, and record the result in the sidecar mapfile. With
/// `skip_on_error: true`, a bad sector zero-fills + marks `NonTrimmed` and
/// the sweep keeps going (jumping ahead through dense damage); without it,
/// the first read failure aborts.
///
/// One of the two flat verbs the library exposes for rip orchestration;
/// multipass + retry decisions are the caller's job — see [`PatchOptions`].
/// A resume over a scoped (MKV-staging) mapfile widens it to the whole disc.
pub fn sweep(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
) -> Result<CopyResult> {
    sweep_in(
        disc,
        reader,
        path,
        opts,
        None,
        &EngineHalt::legacy(opts.halt.clone()),
    )
}

/// [`sweep()`] under the op token `op`, as [`copy_with`].
pub fn sweep_with(
    op: &libfreemkv::Halt,
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
) -> EngineOutcome<CopyResult> {
    let halt = EngineHalt::new(op, opts.halt.clone());
    let r = sweep_in(disc, reader, path, opts, None, &halt);
    EngineOutcome::from_result(r, &halt, |r| r.halted)
}

/// [`sweep()`] over only `scope` (`(lba, sectors)`, e.g. from
/// [`libfreemkv::Disc::mkv_staging_ranges`]): a staged image for an MKV rip. The
/// mapfile records the scope, so the file is never taken for a whole-disc image.
///
/// No bus-map gate: every sector in such a scope is either BEF=0 (AACS BD Pre-recorded
/// 0.953 §3.7: "the BEF shall be set to 0b for the sectors that do not correspond to
/// Clip AV stream files") or a title extent the drive de-busses.
pub fn sweep_scoped(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
    scope: &[(u32, u32)],
) -> Result<CopyResult> {
    let halt = EngineHalt::legacy(opts.halt.clone());
    sweep_in(
        disc,
        reader,
        path,
        opts,
        Some(sector_scope_to_bytes(scope)),
        &halt,
    )
}

/// Refuse the image at `path` as a whole-disc image (`iso://` copy, `dir://` extract)
/// when its sidecar mapfile records a scope: it was staged for an MKV rip, and nothing
/// outside that scope was read. A missing mapfile is fine; an unreadable one is an error.
pub fn ensure_whole_image(path: &std::path::Path) -> Result<()> {
    match mapfile::Mapfile::load(&mapfile::mapfile_path_for(path)) {
        Ok(map) if map.scope().is_some() => Err(Error::ImageScoped {
            path: path.display().to_string(),
        }),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::from(e)),
    }
}

/// Refuse muxing `titles` (indices into `disc.titles`, scanned from the image at `path`)
/// when the image was staged for an MKV rip and its scope does not hold every sector of
/// their extents, or holds some never read (a stopped staging): those would mux zeros. Checked by extents, so
/// a staging mux that re-maps its titles against the staged image still passes.
pub fn ensure_titles_staged(
    path: &std::path::Path,
    disc: &libfreemkv::Disc,
    titles: &[usize],
) -> Result<()> {
    let map = match mapfile::Mapfile::load(&mapfile::mapfile_path_for(path)) {
        Ok(map) => map,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::from(e)),
    };
    let Some(scope) = map.scope() else {
        return Ok(());
    };
    // In scope is not enough: a staging sweep stopped part-way leaves NonTried there too.
    let unread = map.ranges_with(&[mapfile::SectorStatus::NonTried]);
    let count = disc.titles.len();
    if let Some(&index) = titles.iter().find(|&&i| i >= count) {
        return Err(Error::DiscTitleRange { index, count });
    }
    let extents = titles.iter().flat_map(|&i| &disc.titles[i].extents);
    for e in extents {
        let want = (e.start_lba as u64 * 2048, e.sector_count as u64 * 2048);
        let have: u64 = mapfile::intersect(&[want], scope).iter().map(|r| r.1).sum();
        if have != want.1 || !mapfile::intersect(&[want], &unread).is_empty() {
            return Err(Error::ImageScoped {
                path: path.display().to_string(),
            });
        }
    }
    Ok(())
}

/// `(lba, sectors)` ranges as the mapfile's `(pos, size)` byte ranges.
pub(crate) fn sector_scope_to_bytes(scope: &[(u32, u32)]) -> Vec<(u64, u64)> {
    scope
        .iter()
        .map(|&(l, n)| (l as u64 * 2048, n as u64 * 2048))
        .collect()
}

// Minimum interval between sweep progress reports (patch's PROGRESS_TICK_MS).
const SWEEP_TICK: std::time::Duration = std::time::Duration::from_millis(250);

// A sweep's progress bar: `work_*` run 0..=100% over the pass's domain (its scope, or the
// whole disc), counting what the mapfile already held as done, so a resume starts there.
struct SweepBar {
    // Mapfile stats at pass start: stand in for a consumer snapshot until one lands.
    base: mapfile::MapStats,
    done_before: u64,
    domain: u64,
    total_bytes: u64,
}

impl SweepBar {
    // One tick after `done` bytes of this pass's regions, `good` of them read clean.
    fn tick(
        &self,
        disc: &libfreemkv::Disc,
        snap: Option<&sweep::ProgressSnapshot>,
        main_title_bad: u64,
        located: &libfreemkv::progress::LocatedProgress,
        done: u64,
        good: u64,
    ) -> libfreemkv::progress::PassProgress {
        let b = &self.base;
        // The snapshot lags the producer, so good never regresses below its own count.
        // Without one, done-but-not-good bytes are this pass's damage, not lost from view.
        let (good, unreadable, pending, retryable) = match snap {
            Some(s) => (
                s.stats.bytes_good.max(b.bytes_good.saturating_add(good)),
                s.stats.bytes_unreadable,
                s.stats.bytes_pending,
                s.stats.bytes_retryable,
            ),
            None => (
                b.bytes_good.saturating_add(good),
                b.bytes_unreadable,
                b.bytes_pending.saturating_sub(done),
                b.bytes_retryable.saturating_add(done.saturating_sub(good)),
            ),
        };
        let main_title = disc.titles.first();
        libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: self.done_before.saturating_add(done).min(self.domain),
            work_total: self.domain,
            bytes_good_total: good,
            bytes_unreadable_total: unreadable,
            bytes_pending_total: pending,
            bytes_retryable_total: retryable,
            bytes_total_disc: self.total_bytes,
            disc_duration_secs: main_title.map(|t| t.duration_secs),
            bytes_bad_in_main_title: main_title_bad,
            main_title_duration_secs: main_title.map(|t| t.duration_secs),
            main_title_size_bytes: main_title.map(|t| t.size_bytes),
            located: located.clone(),
        }
    }
}

// A sweep under `halt`, which already holds `opts.halt`.
pub(crate) fn sweep_in(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
    scope: Option<Vec<(u64, u64)>>,
    halt: &EngineHalt<'_>,
) -> Result<CopyResult> {
    halt.linked(|lib| sweep_linked(disc, reader, path, opts, scope, halt, lib))
}

// `sweep_in` with `lib`, the libfreemkv token that follows `halt`.
fn sweep_linked(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &SweepOptions,
    scope: Option<Vec<(u64, u64)>>,
    halt: &EngineHalt<'_>,
    lib: &libfreemkv::Halt,
) -> Result<CopyResult> {
    use libfreemkv::io::{DEFAULT_PIPELINE_DEPTH, Pipeline};
    use sweep::{ProgressSnapshot, SweepSink, WorkItem, try_recv_progress};

    // Pre-flight decrypt gate, also enforced in `copy` but re-checked here so a
    // direct `sweep` caller can't bypass it: a decrypting sweep of an encrypted
    // disc with no usable key would write ciphertext at exit 0. No-op for `--raw`.
    crate::resolve::ensure_decryptable_with(disc, !opts.decrypt, opts.keys.as_ref())?;
    // AACS BD Pre-recorded 0.953 §3.7 Note: "PC Host shall decrypt bus-encrypted Clip AV
    // stream file". One the drive's bus map could not locate would land here still encrypted.
    if scope.is_none() {
        libfreemkv::sector::bus_removal::ensure_image_debussable(reader)?;
    }

    // A zero-capacity disc would size the read domain at 0 and write a 0-byte
    // ISO reported as complete; `image_read_sectors` turns it into an
    // `Error::EmptyImage` before the output is created.
    let total_bytes = disc.image_read_sectors()? as u64 * 2048;
    // Decrypt-aware read (`--raw`: pass-through); AACS reads widen onto each content
    // file's unit grid inside it. Bad sectors = physical read failure, not decrypt.
    let mut reader = whole_disc::whole_disc_decrypting_reader(
        disc,
        reader,
        opts.decrypt,
        halt.is_wired().then_some(lib),
        opts.keys.as_ref(),
    )?;
    let reader = &mut reader;

    // Mapfile: load if resuming, else wipe + recreate.
    let mapfile_path = disc.mapfile_for(path);
    // covers_disc reconciliation: a resume against a mismatched mapfile size is
    // unsafe (copy()'s dispatch forces a fresh sweep here too) — under-cover abandons
    // the tail, over-cover reads past capacity. Same downgrade for direct sweep() calls.
    let mut resume = opts.resume;
    if resume && mapfile_path.exists() {
        match mapfile::Mapfile::load(&mapfile_path) {
            Ok(existing) => {
                // Identity first, crucially BEFORE `stamp_identity` below: that stamps
                // the current job's identity onto the mapfile, so checking after never fires.
                if let Err(e) = mapfile::check_mapfile_identity(&existing, disc, opts.keys.as_ref())
                {
                    // Another disc's map over a missing/empty image guards no data (a consumer
                    // deleted the ISO after muxing it): drop it and start fresh.
                    if !no_image(path)? {
                        return Err(Error::from(e));
                    }
                    tracing::info!(
                        "sweep: another disc's mapfile has no image; forcing fresh sweep"
                    );
                    resume = false;
                } else if existing.raw().is_some_and(|raw| raw == opts.decrypt) {
                    // Raw and decrypted sectors must never share one image.
                    tracing::warn!("sweep: image is in the other raw/decrypt mode; overwriting it");
                    resume = false;
                } else if existing.total_size() != total_bytes {
                    tracing::info!(
                        "sweep: mapfile total_size {} != disc {}; forcing fresh sweep",
                        existing.total_size(),
                        total_bytes,
                    );
                    resume = false;
                } else {
                    // Inconsistent-resume guard: mapfile claims progress but the ISO is
                    // missing/short (deleted, truncated, or a stat error misread as 0).
                    // Producer only re-reads NonTried, so downgrade to a fresh sweep.
                    let image = image_state(path, existing.total_size())?;
                    let iso_len = image.len;
                    let claims_progress = existing.stats().bytes_pending != existing.total_size();
                    if image.is_short() && claims_progress {
                        tracing::info!(
                            "sweep: mapfile claims prior progress (pending {} of {}) but the ISO is {} of {} bytes; forcing fresh sweep",
                            existing.stats().bytes_pending,
                            existing.total_size(),
                            iso_len,
                            existing.total_size(),
                        );
                        resume = false;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Mapfile exists but is corrupt/unparseable. resume=true would hand
                // garbage to open_or_create and mis-track progress; downgrade so the
                // `!resume` path below drops it and the rip restarts clean.
                tracing::info!(
                    "sweep: mapfile at {} is corrupt/unparseable; forcing fresh sweep",
                    mapfile_path.display(),
                );
                resume = false;
            }
            // EIO/EACCES/ESTALE say nothing about the contents: a fresh sweep would
            // delete a good mapfile and truncate the ISO, so surface them.
            Err(e) => return Err(Error::from(e)),
        }
    }
    if !resume {
        // A fresh sweep MUST start from an empty mapfile: if the stale file survives,
        // open_or_create loads it and the new disc inherits old Finished ranges →
        // silently zero-filled ISO. ENOENT is fine; any other error aborts.
        stale_mapfile_removed(std::fs::remove_file(&mapfile_path))?;
    }
    let mut map = mapfile::Mapfile::open_or_create(&mapfile_path, total_bytes, MAPFILE_CREATOR)
        .map_err(Error::from)?;

    // The disc's identity for a later resume (KU §4.1): its hash and VID fingerprint only,
    // never a key byte or a raw VID (J6).
    mapfile::stamp_identity(&mut map, disc, opts.keys.as_ref());
    map.set_raw(!opts.decrypt);

    // ISO file: resume + Finished ranges opens existing; otherwise creates fresh,
    // pre-sized to total_bytes. `is_regular` MUST come from the open handle, not a
    // pre-create `metadata(path)` stat error, which unwrap_or(false) would truncate.
    let existing_len = match iso_len_from_metadata(std::fs::metadata(path))? {
        IsoLen::Missing => None,
        IsoLen::Len(n) => Some(n),
    };
    let (file, is_regular) = if resume && existing_len.is_some_and(|len| len > 0) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(Error::from)?;
        let reg = output_is_regular(f.metadata());
        (f, reg)
    } else {
        let f = std::fs::File::create(path).map_err(Error::from)?;
        let reg = output_is_regular(f.metadata());
        if reg {
            f.set_len(total_bytes).map_err(Error::from)?;
        }
        (f, reg)
    };

    // Wrap the raw `File` in our bounded-cache `WritebackFile` (drains dirty
    // pages continuously instead of bursting; see `libfreemkv::io`). It moves
    // into the consumer thread.
    let file = libfreemkv::io::WritebackFile::new(file).map_err(Error::from)?;
    let batch: u16 = sweep_batch_sectors(opts.batch_sectors, opts.skip_on_error, disc.format);

    // A scoped sweep reads only its scope (plus any earlier staging's); a whole-disc
    // sweep over a scoped mapfile fills the rest (the gate above passed). A scoped pass
    // resuming a whole-disc map with progress reads its scope but leaves the map whole.
    let resumes_whole =
        resume && map.scope().is_none() && map.stats().bytes_pending != map.total_size();
    let pass_scope = match scope {
        Some(mut s) if resumes_whole => Some(mapfile::merge_byte_ranges(&mut s)),
        Some(mut s) => {
            s.extend_from_slice(map.scope().unwrap_or(&[]));
            map.set_scope(s);
            map.scope().map(<[_]>::to_vec)
        }
        None => {
            map.clear_scope();
            None
        }
    };

    // Pre-compute NonTried regions before handing the mapfile to the consumer
    // thread. Producer processes them in order; consumer mutates the mapfile
    // per work-item. Regions left NonTrimmed/Unreadable are the patch pass's job.
    let mut regions: Vec<(u64, u64)> = map.ranges_with(&[mapfile::SectorStatus::NonTried]);
    if let Some(scope) = &pass_scope {
        regions = mapfile::intersect(&regions, scope);
    }
    let domain = match &pass_scope {
        Some(scope) => mapfile::intersect(&[(0, total_bytes)], scope)
            .iter()
            .map(|r| r.1)
            .sum(),
        None => total_bytes,
    };
    let todo: u64 = regions.iter().map(|&(p, n)| snap_to_sectors(p, n).1).sum();
    let bar = SweepBar {
        base: map.stats(),
        done_before: domain.saturating_sub(todo),
        domain,
        total_bytes,
    };
    // The drilldown starts from the damage already recorded, not empty.
    let prior_damage = map.ranges_with(&mapfile::damage_sector_statuses());

    // Spawn the consumer (owns WritebackFile + Mapfile; producer keeps reader/halt).
    // `map_disown` is taken BEFORE `map` moves into the sink: it's the only way left
    // to stop an abandoned consumer writing a stale mapfile over a resumed pass.
    let map_disown = map.disown_handle();
    let (sink, prog_rx) = SweepSink::new(file, map, is_regular);
    let pipe: Pipeline<WorkItem, sweep::ConsumerSummary> =
        Pipeline::spawn_named("freemkv-sweep-consumer", DEFAULT_PIPELINE_DEPTH, sink)?;

    // Halt token for `send_bounded` below: the op's; with none wired, a never-cancelled
    // token keeps SEND_DEADLINE as the only bound.
    let send_halt = lib.clone();
    // One hand-off; `Err` ends the read loop (see `SendStall::pass_error`).
    let send =
        |item: WorkItem| send_bounded(&pipe, item, &send_halt).map_err(SendStall::pass_error);

    let mut buf = vec![0u8; batch as usize * 2048];
    // POSITION: how far the producer's cursor has advanced, good bytes and
    // zero-filled damage alike. Drives `work_done` and the pending remainder.
    let mut bytes_done = 0u64;
    // RECOVERY: bytes that came off the platter and were sent to the consumer
    // as `Good`. Never advanced by a skip or a gap fill, so it can be shown to
    // a user as "recovered" without lying. See the progress tick below.
    let mut bytes_good_done = 0u64;
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
    let mut cached_snapshot: Option<ProgressSnapshot> = None;
    // Derived from `cached_snapshot.bad_ranges` + the main title only, changing
    // exactly when a new snapshot lands — not once per batch. The old per-iteration
    // recompute ran `bytes_bad_in_title` (O(ranges x extents)) up to 1.6M times/rip.
    let (mut cached_main_title_bad, mut cached_located) = match disc.titles.first() {
        Some(t) => (
            bytes_bad_in_title(t, &prior_damage),
            locate_ranges(&prior_damage, t),
        ),
        None => (0, libfreemkv::progress::LocatedProgress::default()),
    };
    let mut last_tick: Option<std::time::Instant> = None;
    // A tick the throttle skipped: reported once the pass completes, so its bar ends at 100%.
    let mut tick_owed = false;
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

    // Request the drive's max read speed up front — removes riplock. BD/UHD get
    // speed from drive unlock/init, but DVD skips that path, so without this SET CD
    // SPEED a DVD sweeps riplocked. The damage branch below also re-asserts it later.
    reader.set_speed(0xFFFF);

    'outer: for (region_pos, region_size) in regions {
        // Snap to whole sectors before the range becomes a cursor: mapfile ranges
        // are BYTE ranges (ddrescue `-b 512` interop), and an unaligned offset
        // truncates to the wrong LBA and records shifted payload as Finished.
        let (region_pos, region_size) = snap_to_sectors(region_pos, region_size);
        let region_end = region_pos + region_size;
        let mut pos = region_pos;
        tracing::trace!(
            target: "freemkv::disc",
            phase = "region_enter",
            region_pos,
            region_size,
            region_end,
            "entering NonTried region"
        );

        while pos < region_end {
            if halt.is_cancelled() {
                halt_requested = true;
                break 'outer;
            }

            // Inner block ends snap back onto the file's unit grid, so a bad unit fails one block.
            let mut block_bytes = (region_end - pos).min(batch as u64 * 2048);
            if pos + block_bytes < region_end {
                let end = reader.unit_block_end(pos / 2048, (pos + block_bytes) / 2048);
                block_bytes = end * 2048 - pos;
            }
            let block_lba = (pos / 2048) as u32;
            let block_count = (block_bytes / 2048) as u16;
            let recovery = !opts.skip_on_error;

            // `require_full_read`, not a bare `Ok(_)`: `buf` is reused each iteration,
            // so a short transfer would put the PREVIOUS block's tail into `Good` and
            // write it as recovered data. Routed into the Err arms instead.
            let read_result = require_full_read(
                reader.read_sectors(
                    block_lba,
                    block_count,
                    &mut buf[..block_bytes as usize],
                    recovery,
                ),
                block_bytes as usize,
                block_lba,
            );

            match read_result {
                Ok(_) => {
                    read_ok_count += 1;
                    // `ReadCtx` owns the damage zone (exit after `damage_window_max` clean
                    // reads, jump multiplier reset with it); the drive speed follows it.
                    let was_in_zone = read_ctx.in_damage_zone;
                    read_ctx.on_success();
                    if was_in_zone && !read_ctx.in_damage_zone {
                        reader.set_speed(0xFFFF);
                        tracing::debug!(
                            target: "freemkv::disc",
                            phase = "damage_exit",
                            lba = block_lba,
                            "Exited damage zone; restoring max read speed"
                        );
                    }
                    // bridge_degradation_count already reset inside on_success() above.

                    // Plaintext: the whole-disc reader decrypted in place during the read.

                    // Fresh owned Vec into the channel; producer's `buf` is reused.
                    let send_buf = buf[..block_bytes as usize].to_vec();
                    if let Err(e) = send(WorkItem::Good { pos, buf: send_buf }) {
                        (halt_requested, producer_err) = (e.is_none(), e);
                        break 'outer;
                    }
                    bytes_good_done = bytes_good_done.saturating_add(block_bytes);
                    bytes_done = bytes_done.saturating_add(block_bytes);
                    pos += block_bytes;
                }
                // A Stop that landed mid-read (the drive's LD3 `Halted`): the pass ends
                // halted and the block stays NonTried, never zero-filled as damage.
                Err(Error::Halted) => {
                    halt_requested = true;
                    break 'outer;
                }
                // Not skipping, or not disc damage (e.g. a decrypt refusal): abort
                // with the real cause rather than zero-filling it as NonTrimmed.
                Err(err) if !opts.skip_on_error || !is_damage_candidate(&err) => {
                    producer_err = Some(classify_pass_abort(err, block_lba));
                    break 'outer;
                }
                Err(err) => {
                    read_err_count += 1;
                    let was_in_zone = read_ctx.in_damage_zone;
                    let action = read_error::handle_read_error(&err, &mut read_ctx);

                    match action {
                        read_error::ReadAction::Retry { pause_secs } => {
                            if sleep_secs_or_halt(pause_secs, halt) {
                                halt_requested = true;
                                break 'outer;
                            }
                        }
                        read_error::ReadAction::SkipBlock { pause_secs } => {
                            let fill = WorkItem::SkipFill {
                                pos,
                                len: block_bytes,
                            };
                            if let Err(e) = send(fill) {
                                (halt_requested, producer_err) = (e.is_none(), e);
                                break 'outer;
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);
                            if sleep_secs_or_halt(pause_secs, halt) {
                                halt_requested = true;
                                break 'outer;
                            }
                            pos += block_bytes;
                        }
                        read_error::ReadAction::JumpAhead {
                            sectors,
                            pause_secs,
                        } => {
                            let fill = WorkItem::SkipFill {
                                pos,
                                len: block_bytes,
                            };
                            if let Err(e) = send(fill) {
                                (halt_requested, producer_err) = (e.is_none(), e);
                                break 'outer;
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);

                            // Every Pass-1 jump follows the error that entered the zone.
                            if !was_in_zone {
                                reader.set_speed(0x0000);
                                tracing::debug!(
                                    target: "freemkv::disc",
                                    phase = "damage_enter",
                                    lba = block_lba,
                                    "Entered damage zone; dropping to minimum read speed"
                                );
                            }

                            // Saturating throughout: read_error computes sector count
                            // with saturating_mul as "defence in depth"; honor the same
                            // guarantee here so a pathological jump distance can't wrap.
                            let jump_pos = pos
                                .saturating_add(block_bytes)
                                .saturating_add(sectors.saturating_mul(2048))
                                .min(region_end);
                            let gap_start = pos + block_bytes;
                            let gap_bytes = jump_pos.saturating_sub(gap_start);
                            // `>= 0` here is an equivalent mutant: a zero-length GapFill is a
                            // no-op end to end.
                            if gap_bytes > 0 {
                                let fill = WorkItem::GapFill {
                                    pos: gap_start,
                                    len: gap_bytes,
                                };
                                if let Err(e) = send(fill) {
                                    (halt_requested, producer_err) = (e.is_none(), e);
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
                            if sleep_secs_or_halt(pause_secs, halt) {
                                halt_requested = true;
                                break 'outer;
                            }
                        }
                        read_error::ReadAction::AbortPass => {
                            producer_err = Some(classify_pass_abort(err, block_lba));
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

            // Heartbeat: this gate, its counters and the LBA below are DIAGNOSTIC-ONLY, and
            // carry eight mutants no test here can kill.
            let time_due = last_log_time.elapsed() >= std::time::Duration::from_secs(5);
            if iter_count - last_log_iter >= 100 || time_due {
                last_log_iter = iter_count;
                last_log_time = std::time::Instant::now();
                // Promoted trace -> debug ("no silent hangs"): the heartbeat must be
                // visible at the standard debug level, not only the trace firehose.
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
                // Throttled stats refresh — best-effort try_send so a busy consumer
                // doesn't stall the producer; the cached snapshot stays good enough.
                let _ = pipe.try_send(WorkItem::StatsRequest);
            }

            if let Some(reporter) = opts.progress {
                tick_owed = last_tick.is_some_and(|t| t.elapsed() < SWEEP_TICK);
                if !tick_owed {
                    last_tick = Some(std::time::Instant::now());
                    let pp = bar.tick(
                        disc,
                        cached_snapshot.as_ref(),
                        cached_main_title_bad,
                        &cached_located,
                        bytes_done,
                        bytes_good_done,
                    );
                    // The report point is a stop check: a front end stops from its events.
                    reporter.event(&libfreemkv::Event::Pass(&pp));
                    if halt.is_cancelled() {
                        halt_requested = true;
                        break 'outer;
                    }
                }
            }
        }
    }

    if tick_owed
        && !halt_requested
        && producer_err.is_none()
        && let Some(reporter) = opts.progress
    {
        let pp = bar.tick(
            disc,
            cached_snapshot.as_ref(),
            cached_main_title_bad,
            &cached_located,
            bytes_done,
            bytes_good_done,
        );
        reporter.event(&libfreemkv::Event::Pass(&pp));
    }

    // Producer is done; let the consumer drain and run close() (writeback, fsync,
    // mapfile.flush). Bounded by the SAME halt the sends above use — a plain
    // `Pipeline::finish` would re-block on the stalled consumer, so Stop never returns.
    let consumer_failed = pipe.consumer_failed();
    let summary = finish_bounded_disowning(pipe, &send_halt, &map_disown);

    // The producer's error wins, unless the consumer's apply failed first: then its write
    // error is the cause (see `pass_failure`).
    if let Some(e) = producer_err {
        // Producer error is returned, dropping the consumer's result — but do NOT
        // let a consumer close() failure vanish silently: it's the only signal the
        // mapfile on disk is untrustworthy. Log it, mirroring `patch.finish.dropped`.
        if let Err(close_err) = &summary {
            tracing::warn!(
                target: "freemkv::disc",
                phase = "sweep.finish.dropped",
                read_error = %e,
                close_error = %close_err,
                "sweep: consumer close failed while the pass was already failing — the mapfile on disk may be incomplete"
            );
        }
        return Err(pass_failure(e, summary, consumer_failed));
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

    // End-of-pass diagnostic: one INFO line per sweep letting a post-mortem
    // analyst see disc/drive damage at a glance without grepping the per-error
    // WARN log. Counters come from `ReadCtx`'s accumulated state.
    let pass_sum = read_ctx.pass_summary();
    tracing::info!(
        target: "freemkv::disc",
        phase = "pass1_summary",
        total_reads_ok = pass_sum.total_reads_ok,
        total_errors = pass_sum.total_errors,
        zones_entered = pass_sum.zones_entered,
        jumps_taken = pass_sum.jumps_taken,
        long_pause_escalations = pass_sum.long_pause_escalations,
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
    pub progress: Option<&'a dyn libfreemkv::Events>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The rip's up-front key set (KU §3.2): a decrypting pass reads through its whole-disc
    /// reader and gates on it, with no lookup. `None` holds no key: a decrypting pass over an
    /// AACS disc refuses (E7022) whatever keys the disc banked (KU-X1).
    pub keys: Option<libfreemkv::keys::KeyRing>,
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
    // THE definition of a finished copy: no bytes left to retry, none
    // permanently lost, pass not interrupted. Previously each of five call
    // sites re-derived this and disagreed, reporting a lossy/cancelled rip complete.
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

/// Options for [`sweep()`] (Pass 1 / forward sequential pass).
///
/// Named `Disc::sweep` before 1.6.0, when recovery moved out of libfreemkv
/// and the receiver became a `&Disc` argument.
// KU-E0 (keys-upfront-design §8.2): "`Default` for `SweepOptions`, `PatchOptions`".
#[derive(Default)]
pub struct SweepOptions<'a> {
    pub decrypt: bool,
    pub resume: bool,
    pub batch_sectors: Option<u16>,
    pub skip_on_error: bool,
    pub progress: Option<&'a dyn libfreemkv::Events>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The rip's up-front key set (KU §3.2): a decrypting pass reads through its whole-disc
    /// reader and gates on it, with no lookup. `None` holds no key: a decrypting pass over an
    /// AACS disc refuses (E7022) whatever keys the disc banked (KU-X1).
    pub keys: Option<libfreemkv::keys::KeyRing>,
}

/// Options for [`patch()`] (Pass N retry pass over bad ranges).
// KU-E0 (keys-upfront-design §8.2): "`Default` for `SweepOptions`, `PatchOptions`".
#[derive(Default)]
pub struct PatchOptions<'a> {
    pub decrypt: bool,
    /// Labels the reported [`PassKind`](libfreemkv::progress::PassKind) only
    /// (1 → Scrape, >1 → Trim). It does NOT size any read: the handler chain
    /// owns read sizing and bisection.
    pub block_sectors: Option<u16>,
    /// Diagnostics only — logged as `recovery=` at pass start and read by
    /// nothing. Per-read effort is the handler chain's `ReadParams`.
    pub full_recovery: bool,
    /// Labels the reported [`PassKind`](libfreemkv::progress::PassKind) only.
    /// It does NOT order the walk: `PatchCtx::run` sorts the bad ranges by
    /// (size desc, pos asc), a total order over disjoint runs, so any
    /// pre-ordering is unobservable.
    pub reverse: bool,
    /// Echoed verbatim into [`PatchOutcome::wedged_threshold`] for the caller
    /// to render. Nothing counts wedged reads against it — `wedged_exit` is set
    /// from a handler's transport fault.
    pub wedged_threshold: u64,
    pub progress: Option<&'a dyn libfreemkv::Events>,
    pub halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The rip's up-front key set (KU §3.2): a decrypting pass reads through its whole-disc
    /// reader and gates on it, with no lookup. `None` holds no key: a decrypting pass over an
    /// AACS disc refuses (E7022) whatever keys the disc banked (KU-X1).
    pub keys: Option<libfreemkv::keys::KeyRing>,
}
impl<'a> PatchOptions<'a> {
    /// THE tuning preset for a Pass-N patch pass, shared by both entry points
    /// (`patch_internal` and `multipass_rip`'s patch loop) so they can't drift
    /// apart on a future tuning change.
    ///
    /// `block_sectors: Some(32)` no longer sizes any read — the handler chain
    /// (`section_recover.rs`) owns read sizing/bisection now. It only survives as the pass
    /// LABEL (>1 = Trim, 1 = Scrape). `full_recovery` is diagnostics-only; `wedged_threshold`
    /// is reported, not enforced.
    pub fn for_patch_pass(
        decrypt: bool,
        progress: Option<&'a dyn libfreemkv::Events>,
        halt: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        PatchOptions {
            decrypt,
            block_sectors: Some(32),
            full_recovery: true,
            reverse: true,
            wedged_threshold: 50,
            progress,
            halt,
            keys: None,
        }
    }
}

/// Result returned by [`patch()`].
#[derive(Debug)]
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

// Snap a mapfile byte-range out to whole sectors (start down, end up) — an unaligned offset
// truncates the LBA and records corrupt bytes `Finished`.
pub(super) fn snap_to_sectors(pos: u64, len: u64) -> (u64, u64) {
    use section_recover::SECTOR;
    let start = pos - pos % SECTOR;
    if len == 0 {
        return (start, 0);
    }
    let end_u128 = (pos as u128 + len as u128).div_ceil(SECTOR as u128) * SECTOR as u128;
    let max_end = (u64::MAX / SECTOR) * SECTOR;
    let end = end_u128.min(max_end as u128) as u64;
    (start, end.saturating_sub(start))
}

// Pass 1's wedge-avoidance pause: `secs`, cut short within one `WAIT_SLICE` of a cancel.
// §4.2: "`sleep_secs_or_halt`'s `None` arm … is deleted" — every pause is halt-aware.
// Returns `true` when a cancel cut the pause short.
pub(crate) fn sleep_secs_or_halt(secs: u64, halt: &EngineHalt<'_>) -> bool {
    if secs == 0 {
        return false;
    }
    halt.wait(std::time::Duration::from_secs(secs))
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

#[cfg(test)]
mod ku_tests;
pub(crate) mod mapfile;
mod patch;
mod read_error;
mod section_recover;
mod sweep;
mod whole_disc;

// The mapfile-backed main-title bad-byte reader, used by the multipass
// abort-on-loss gate. `pub` so the engine can re-export it: a front-end reads
// it here rather than the (now-removed) libfreemkv method.
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
#[path = "mod_snap_tests.rs"]
mod snap_tests;

// A stallable consumer, shared by the send- and finish-side bounded-pipeline tests.
#[cfg(test)]
#[path = "mod_stall_fixtures_tests.rs"]
mod stall_fixtures;

// The producer-side handoff guard: what happens to a `send` when the
// consumer is ALIVE but not draining. Pins the fix for a plain
// `Pipeline::send` parking forever on a stalled (not dead) consumer.
#[cfg(test)]
#[path = "mod_send_bounded_tests.rs"]
mod send_bounded_tests;

// The join-side half of the same guarantee `send_bounded_tests` pins: a Stop that gets the
// producer out must not then block forever in `Pipeline::finish` on the same stalled consumer.
#[cfg(test)]
#[path = "mod_finish_bounded_tests.rs"]
mod finish_bounded_tests;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_resume_decision_tests.rs"]
mod resume_decision_tests;

#[cfg(test)]
#[path = "mod_snapshot_tests.rs"]
mod snapshot_tests;

// The shipped Pass-N patch preset, pinned, so it's load-bearing rather than dead literals.
#[cfg(test)]
#[path = "mod_patch_preset_tests.rs"]
mod patch_preset_tests;

// End-to-end sweep contracts over a synthetic reader: resume reconciliation, stops,
// progress and the consumer hand-off.
#[cfg(test)]
#[path = "mod_sweep_contract_tests.rs"]
mod sweep_contract_tests;
