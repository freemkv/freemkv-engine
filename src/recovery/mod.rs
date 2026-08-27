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

/// A SHORT transfer is a FAILED read, never a partial success.
///
/// [`SectorSource::read_sectors`] answers with the number of BYTES it actually
/// transferred, and every read site in this module ships `buf[..requested]`
/// onward out of a buffer that is REUSED across iterations. If a reader ever
/// answered `Ok(n)` with `n < requested`, the tail of that slice would still
/// hold the PREVIOUS LBA's bytes — and those bytes would be written into the
/// ISO and recorded `Finished`. That is the worst shape of failure this crate
/// has: a rip that reports rc=0 and a clean mapfile while carrying somebody
/// else's sectors inside the movie. (The same class already shipped once, as
/// ~9 MB of ciphertext at rc=0.)
///
/// The live-drive path already makes exactly this call — `Drive::read_one`
/// gates its success arm on `bytes_transferred == count * 2048` and documents
/// "treat a short transfer as a failed read", answering the very
/// `DiscRead { status: None, sense: None }` reproduced here so the two paths
/// classify identically. The engine-side sites matched `Ok(_)` and discarded
/// the count, so they would have believed a short read.
///
/// No `SectorSource` in the tree can trip this today (`FileSectorSource` uses
/// `read_exact`; `Drive` gates as above; `PrefetchedSectorSource` *can* return
/// a short count but the engine never builds one). The trait itself neither
/// permits nor forbids it. This is therefore hardening, not a live bug fix:
/// it makes it impossible for a FUTURE reader implementation to corrupt output
/// silently, and it costs one comparison per read.
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

pub fn copy(
    disc: &libfreemkv::Disc,
    reader: &mut dyn SectorSource,
    path: &std::path::Path,
    opts: &CopyOptions,
) -> Result<CopyResult> {
    // Pre-flight decrypt gate: without it, a decrypting copy of an encrypted disc
    // with no usable key would silently write ciphertext to the ISO and still
    // return Ok at exit 0. `--raw` (opts.decrypt == false) makes this a no-op.
    crate::resolve::ensure_decryptable_strict(disc, !opts.decrypt)?;
    // Mapfile-driven resume dispatch, shared by plain and `--multipass` copies: an
    // interrupted run leaves a crash-safe mapfile, so re-issuing must resume, not
    // re-sweep from 0. Multipass also dispatches to patch on retryable bytes.
    let mf_path = disc.mapfile_for(path);
    if mf_path.exists() {
        let map = mapfile::Mapfile::load(&mf_path).map_err(|e| Error::IoError { source: e })?;
        // BEFORE any resume decision, including "already complete" below: a wrong
        // disc whose predecessor finished would otherwise report the job done
        // having never touched the disc actually in the drive.
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
            // Mapfile capacity != disc capacity: force a full (non-resume) sweep so
            // [0, disc_size) is covered fresh. Under-cover would abandon the readable
            // tail; over-cover would let a resume sweep read LBAs past disc capacity.
            tracing::info!(
                "copy dispatch: → sweep (covers_disc=false, resume=false, map={}, disc={})",
                map.total_size(),
                disc_size,
            );
            return sweep_internal(disc, reader, path, opts, false);
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
    sweep_internal(disc, reader, path, opts, false)
}

/// What goes in the mapfile's `# Rescue Logfile. Created by …` header.
///
/// It must name the crate that actually wrote the file. `env!` expands in the
/// crate being compiled, so the literal `"libfreemkv v"` this used to be
/// concatenated with stamped libfreemkv's NAME onto freemkv-engine's VERSION —
/// a provenance line naming a crate/version pair that has never existed, in the
/// one artifact an operator reads to work out which build produced a rip.
/// Recovery has lived in this crate since 1.6.0.
///
/// Header TEXT only: no parser keys off it (`load` stores the remainder of the
/// line verbatim as `version`), so the on-disk format and its
/// ddrescue-compatibility are untouched.
pub(crate) const MAPFILE_CREATOR: &str = concat!("freemkv-engine v", env!("CARGO_PKG_VERSION"));

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

/// Widen the PHYSICAL read of a region's LAST block out to whole AACS units.
///
/// The third corner of the same invariant, and the one that was missing.
/// [`aacs_aligned_batch`] makes a full batch a whole number of units and
/// [`aacs_aligned_region_start`] anchors the cursor to a unit boundary, but the
/// sweep loop takes `min(region_end - pos, batch * 2048)` — and `region_end` is
/// only SECTOR-snapped (`snap_to_sectors`), never unit-snapped. So the final
/// read of every region is a whole number of sectors that is generally NOT a
/// whole number of units: 6144-byte units straddle its top edge, which is
/// exactly what the two siblings exist to prevent (`decrypt_sectors` anchors
/// units at buffer offset 0, so a partial trailing unit decrypts under the
/// wrong alignment and the verify gate either leaves it encrypted or aborts
/// `DecryptFailed`).
///
/// Widening rather than shrinking: shrinking would leave the region's tail
/// sectors unread forever. `limit` (the disc's total size) caps the widening so
/// a region ending at the disc's end never reads past capacity — a disc whose
/// capacity is not a whole number of units keeps its unavoidable partial tail
/// unit, which is the pre-existing behaviour and not something this can fix.
///
/// Only the READ widens. The caller still accounts, sends, and records exactly
/// `block_bytes`, so the extra bytes cannot shift the cursor or re-status a
/// neighbouring range — the same widen-read-copy-back-the-window shape
/// `patch::recovery_read` already uses for mid-unit recovery reads.
pub(crate) fn aacs_aligned_read_bytes(
    pos: u64,
    block_bytes: u64,
    limit: u64,
    decrypt_is_aacs: bool,
) -> u64 {
    if !decrypt_is_aacs {
        return block_bytes;
    }
    let unit = UNIT_SECTORS as u64 * 2048;
    let rem = block_bytes % unit;
    if rem == 0 {
        return block_bytes;
    }
    let widened = block_bytes.saturating_add(unit - rem);
    // Never past the end of the image, and never NARROWER than what the caller
    // asked for (which `min` alone would do if `limit` were behind `pos`).
    widened.min(limit.saturating_sub(pos)).max(block_bytes)
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

#[cfg(test)]
mod mapfile_creator_tests {
    use super::MAPFILE_CREATOR;

    /// The provenance header must name THIS crate. `concat!` + `env!` puts the
    /// compiling crate's version behind whatever name is written next to it,
    /// so the wrong literal produces a plausible-looking lie
    /// ("libfreemkv v1.6.4") rather than a compile error.
    #[test]
    fn the_mapfile_header_names_the_crate_that_writes_it() {
        assert!(
            MAPFILE_CREATOR.starts_with("freemkv-engine v"),
            "mapfile provenance header is {MAPFILE_CREATOR:?}"
        );
        assert!(
            !MAPFILE_CREATOR.contains("libfreemkv"),
            "recovery has lived in this crate since 1.6.0: {MAPFILE_CREATOR:?}"
        );
        // A version is actually interpolated — not the literal `env!` call, and
        // not an empty tail.
        let version = MAPFILE_CREATOR
            .strip_prefix("freemkv-engine v")
            .expect("prefix asserted above");
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "version tail is {version:?}"
        );
    }
}

#[cfg(test)]
mod aacs_aligned_read_bytes_tests {
    use super::aacs_aligned_read_bytes;

    /// One AACS aligned unit, spelled as the literal the format defines
    /// (3 sectors x 2048) rather than derived from the constant under test.
    const UNIT: u64 = 6144;
    /// A whole disc, far past every `pos` used below, so `limit` never binds.
    const FAR: u64 = 1 << 40;

    /// The case the sweep loop actually produces: the last block of a region.
    ///
    /// A region ends wherever `snap_to_sectors` put it — a SECTOR boundary, not
    /// a unit boundary. `region_end - pos` is then a whole number of sectors
    /// that straddles a unit: 4096 bytes (2 sectors) is two thirds of a unit,
    /// and handing that to the decrypting reader decrypts the trailing unit
    /// under the wrong alignment.
    #[test]
    fn the_last_block_of_a_region_is_widened_to_a_whole_unit() {
        assert_eq!(aacs_aligned_read_bytes(0, 4096, FAR, true), UNIT);
        assert_eq!(aacs_aligned_read_bytes(0, 2048, FAR, true), UNIT);
        // 33 sectors (one aligned batch) + 1 sector → 12 units.
        assert_eq!(aacs_aligned_read_bytes(0, 69_632, FAR, true), 73_728);
    }

    /// A block that is already a whole number of units is left exactly alone —
    /// that is every block but the last one of a region.
    #[test]
    fn a_unit_aligned_block_is_untouched() {
        for bytes in [UNIT, 2 * UNIT, 11 * UNIT, 67_584] {
            assert_eq!(aacs_aligned_read_bytes(12_288, bytes, FAR, true), bytes);
        }
    }

    /// A NON-decrypting sweep (`--raw`, the multipass path) has no unit
    /// geometry to respect and must read exactly what it was asked for.
    #[test]
    fn a_non_aacs_sweep_reads_exactly_the_block() {
        for bytes in [2048u64, 4096, 65_536, 69_632] {
            assert_eq!(aacs_aligned_read_bytes(0, bytes, FAR, false), bytes);
        }
    }

    /// Widening must never read past the end of the image. A disc whose
    /// capacity is not a whole number of units keeps its partial tail unit.
    #[test]
    fn widening_stops_at_the_end_of_the_disc() {
        // 100 sectors = 204800 bytes, which is not a multiple of 6144.
        let capacity = 204_800u64;
        // Final block: the last 2 sectors of the disc.
        assert_eq!(
            aacs_aligned_read_bytes(capacity - 4096, 4096, capacity, true),
            4096,
            "must not read past capacity to complete a unit"
        );
        // A block that CAN be completed inside the disc still is.
        assert_eq!(
            aacs_aligned_read_bytes(0, 4096, capacity, true),
            UNIT,
            "room to widen inside the image"
        );
    }

    /// Never narrower than what was asked for, even if `limit` is behind
    /// `pos` (a mapfile whose extent disagrees with the image length).
    #[test]
    fn never_returns_less_than_the_requested_block() {
        assert_eq!(aacs_aligned_read_bytes(8192, 4096, 0, true), 4096);
        assert_eq!(aacs_aligned_read_bytes(8192, 4096, 8192, true), 4096);
    }
}

/// What the output image's length is, for a resume decision.
///
/// `NotFound` is the ONE error that genuinely means "no file yet" — the
/// fresh-rip case. Every other error is unknown, and destroying data on an
/// unknown is not a decision this code gets to make: an EIO from a flaky
/// USB/NFS staging volume or a momentary permissions problem is not evidence
/// that a populated ISO is empty, and treating it as zero sends the image
/// through `File::create` and permanently zeroes bytes the mapfile still
/// records `Finished`. The producer only builds work from `NonTried` ranges,
/// so those bytes are never re-read: silent, total loss of the recovery.
///
/// Lifted out of `sweep` because the classification was written TWICE in that
/// one function — once for the inconsistent-resume guard and once for the
/// open-vs-create decision — and welded to a live `std::fs::metadata` call in
/// both places, which is why neither copy could be tested. Two copies of one
/// policy is exactly the shape that drifts.
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
        Err(e) => Err(Error::IoError { source: e }),
    }
}

/// The image a mapfile describes, measured against the length it should be.
///
/// ONE definition, used by `copy`, `sweep` AND `patch`. It used to be written
/// out at each call site — `copy` compared for equality, `sweep` for "shorter
/// than", and `patch` did not check at all. That third gap was the dangerous
/// one: a truncated staging image could be patched and reported as a whole
/// good disc, because the pass only walks the mapfile's bad ranges and takes
/// `bytes_good` from its stats, so every Finished range beyond the truncation
/// is a hole nothing ever re-reads.
pub(crate) struct ImageState {
    /// Length on disk. A missing file reports 0 — absent is as inconsistent
    /// with a mapfile claiming progress as empty is, and both self-heal the
    /// same way.
    pub(crate) len: u64,
    /// Length the mapfile says the image should be.
    pub(crate) want: u64,
}

impl ImageState {
    /// Exactly the length it should be. A LONGER file is not intact either:
    /// it is not the image this mapfile describes.
    pub(crate) fn is_intact(&self) -> bool {
        self.len == self.want
    }

    /// Short of what the mapfile describes — the case where trusting the
    /// mapfile invents recovered data that was never read.
    pub(crate) fn is_short(&self) -> bool {
        self.len < self.want
    }
}

/// Measure `path` against the length a mapfile expects of it.
///
/// A stat failure other than "not found" is an error, never silently 0: a
/// transient stat failure once threw away a good resume and re-ripped hours
/// of work.
pub(crate) fn image_state(path: &std::path::Path, want: u64) -> Result<ImageState> {
    let len = match iso_len_from_metadata(std::fs::metadata(path))? {
        IsoLen::Missing => 0,
        IsoLen::Len(n) => n,
    };
    Ok(ImageState { len, want })
}

/// Whether the output is a REGULAR FILE, and therefore whether a `sync_all`
/// failure on it is a real error and whether it should be pre-sized.
///
/// `/dev/null` and pipes answer their metadata successfully and report
/// not-a-file, so they map to `false` correctly and always did. The default
/// only fires when the `metadata` call ITSELF fails — a transient NFS ESTALE
/// on the staging volume — and there the two copies of this decision had
/// drifted to OPPOSITE answers: `patch` defaulted to `true` and argued in its
/// comment that surfacing the error is the right side for a data-integrity
/// guard, while `sweep` defaulted to `false`, which silently skipped the
/// pre-size AND made `SweepSink::close` swallow a genuine `sync_all` failure
/// on the just-written image — the exact two failures the comment above
/// sweep's call site says must not happen. Unified on `patch`'s answer.
pub(crate) fn output_is_regular(m: std::io::Result<std::fs::Metadata>) -> bool {
    m.map(|md| md.file_type().is_file()).unwrap_or(true)
}

/// Whether a fresh sweep may proceed after trying to delete a stale mapfile.
///
/// A fresh sweep MUST start from an empty mapfile: if the stale file survives,
/// `open_or_create` loads it and the NEW disc inherits the OLD disc's
/// `Finished` ranges, so the producer skips them and the ISO is silently
/// zero-filled there. `NotFound` means it was already gone, which is fine;
/// anything else must abort rather than inherit.
///
/// (This paragraph used to sit at the top of `output_is_regular`'s doc block
/// with no blank `///` between them, so rustdoc rendered it on that function
/// and this one had no documentation at all.)
pub(crate) fn stale_mapfile_removed(r: std::io::Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::IoError { source: e }),
    }
}

/// The batch size a sweep reads in, before AACS unit alignment.
///
/// `None` means "the mode's default", and the two defaults are a real
/// read-shape decision, not a constant: a skip-on-error sweep reads exactly
/// one ECC block at a time so one skipped batch loses exactly one ECC block,
/// while a clean sweep prefers the larger optical batch for throughput. A zero
/// request is clamped to 1 — a zero batch makes `block_bytes` zero every
/// iteration, so `pos` never advances and the producer spins forever emitting
/// zero-length reads. Composes with [`aacs_aligned_batch`].
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

/// Deadline for ONE producer→consumer handoff on a recovery pipeline.
///
/// Reuses [`libfreemkv::io::pipeline::JOIN_TIMEOUT_SECS`] (600 s) — the budget
/// `Pipeline::finish_with_halt` already gives the same consumer to drain at
/// join — rather than inventing a second number. A producer that gave up
/// sooner than the joiner is willing to wait would abort passes the joiner
/// still considers healthy.
///
/// It is deliberately enormous relative to a real write: one item is at most a
/// batch of sectors (64 KiB on sweep, one recovered span on patch), so even a
/// pathologically slow NFS mount doing a few KiB/s clears it in seconds. The
/// deadline is only the no-halt backstop; responsiveness to Stop comes from the
/// halt poll inside [`libfreemkv::Pipeline::send_with_halt`], which ticks every
/// `libfreemkv::halt::POLL_INTERVAL` (250 ms) regardless of this value.
const SEND_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(libfreemkv::io::pipeline::JOIN_TIMEOUT_SECS);

/// Why a send failed, for producers that must tell "the operator pressed
/// Stop" apart from "the consumer died" apart from "the consumer is alive but
/// has not drained a slot in [`SEND_DEADLINE`]".
///
/// [`libfreemkv::Pipeline::send_with_halt`] collapses all three into `Err(item)` — it hands
/// the item back and leaves the diagnosis to the caller. Mapping them all to
/// `PipelineConsumerGone` would report a Stop, and a hung mount, as a dead
/// consumer thread.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SendStall {
    /// The halt token fired while the producer was waiting for a slot. Not an
    /// error: the caller reports the pass as halted and drops the item.
    Halted,
    /// The consumer thread is gone (panicked / receiver dropped).
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
}

/// Halt-aware, deadline-bounded replacement for `pipe.send(item)` in the
/// recovery producers (sweep's read loop, patch's recovered-span sink).
///
/// The defect this exists for: a plain `Pipeline::send` on a bounded channel
/// blocks forever when the consumer is ALIVE but STALLED — e.g. wedged inside
/// `WritebackFile::write_all` on a hung NFS mount. The producer polls its halt
/// token only at the top of its loop, so a Stop issued while it is parked in
/// `send` cannot land at all. `SendError` only ever reports the consumer being
/// DEAD, which is a different failure.
///
/// Shape: try once without blocking, and only if there is no room does the
/// halt get a vote. `Pipeline::send_with_halt` alone would drop a handoff that
/// could not have blocked, which discards bytes the drive already recovered
/// (see `a_raised_halt_still_delivers_when_the_channel_has_room`).
///
/// HONEST LIMIT: this gets the PRODUCER out — the pass stops reading the drive
/// and the run reports `halted` — but the consumer thread is still inside its
/// write, so the subsequent `Pipeline::finish` join still blocks until the
/// mount answers. This buys responsiveness and a correct diagnosis; it does not
/// cure a D-state hang.
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

/// Halt-aware teardown for the recovery pipelines — the join-side sibling of
/// [`send_bounded`], and the other half of the same guarantee.
///
/// [`send_bounded`] gets the PRODUCER out from under a consumer that is alive
/// but not draining. It does not get the CALLER out: the very next statement
/// after the producer unparks is the teardown, and a plain
/// `Pipeline::finish` is a blocking `join()` on that same stalled consumer.
/// So a Stop that `send_bounded` finally made land still could not return —
/// it moved the wedge from `send` to `finish` rather than removing it.
/// `Pipeline::finish_with_halt` is the bound libfreemkv ships for exactly
/// this, and both recovery sites used the non-halt-aware sibling.
///
/// Semantics come straight from `finish_with_halt`, and both matter here:
///
/// * Clean drain — identical to `finish`: the consumer's `close()` output is
///   returned unchanged. `finish_with_halt` polls `is_finished()` BEFORE it
///   looks at the halt, so even a run that ends halted still joins normally
///   and returns its summary as long as the consumer is healthy. The halt
///   path is reached only by a consumer that is genuinely not coming back.
/// * Wedged consumer plus a raised halt — a `FINISH_GRACE_SECS` (5 s) spin
///   first, so a consumer that is merely slow still joins cleanly; only past
///   that is the thread abandoned and `Error::Halted` returned.
///
/// MAPFILE DURABILITY, since the consumer owns it and abandoning skips
/// `close()` (which is where `map.flush()` lives): abandoning is still the
/// right trade.
///
/// * The abandoned consumer is detached, not killed, so it is still holding
///   the pass's `Mapfile` when its wedged write finally returns — and it
///   would then write it, from a snapshot that is now stale, over whatever a
///   resumed pass has since persisted. A sink that owns a `Mapfile` must
///   therefore tear down through [`finish_bounded_disowning`], NOT through
///   this function, which revokes that mapfile on a failed teardown. What is
///   given up by revoking it is the abandoned thread's last
///   `FLUSH_INTERVAL` of records, and only in the pessimistic direction.
/// * If the process exits before that, the loss is bounded by the mapfile's
///   one-second `FLUSH_INTERVAL`, and it is loss in the SAFE direction. Both
///   sinks write the file BEFORE they `record()`, so a lost tail of records
///   can only make the mapfile more pessimistic than the ISO — ranges get
///   re-read on resume. It can never mark Finished a range the ISO did not
///   receive, which is the only direction that would corrupt a resume.
/// * The dangerous shape `ABANDONED` exists to stop — a leaked consumer
///   finalising an output the caller already reported failed — does not
///   arise here: neither recovery `close()` does anything structural, only
///   `sync_all` + `map.flush()`, both idempotent.
/// * And the status quo is WORSE for durability, not better. A blocked
///   `finish` never returns, so the consumer thread never unwinds, never
///   drops the `Mapfile`, and never flushes at all. A hang preserves nothing.
pub(crate) fn finish_bounded<I: Send + 'static, R: Send + 'static>(
    pipe: libfreemkv::io::pipeline::Pipeline<I, R>,
    halt: &libfreemkv::halt::Halt,
) -> Result<R> {
    // `Some(halt)` even with no Stop bit wired (a never-cancelled default) still
    // arms `JOIN_TIMEOUT_SECS`, the same 600s budget `SEND_DEADLINE` gives one
    // handoff — the producer/joiner symmetry `SEND_DEADLINE`'s doc argues for.
    pipe.finish_with_halt(Some(halt))
}

/// [`finish_bounded`] for a pipeline whose sink owns the `Mapfile`: on any
/// failed teardown it DISOWNS that mapfile.
///
/// Both recovery pipelines must use this, not the bare `finish_bounded`.
///
/// The hole it closes: abandoning is detaching, not killing. The leaked
/// consumer is still holding the pass's `Mapfile`, and when its wedged write
/// finally returns it goes on to `record()` (whose `FLUSH_INTERVAL` check has
/// long since elapsed, so it writes immediately) and then drops the sink,
/// whose `Mapfile::drop` flushes again. Both rewrite the WHOLE file from a
/// snapshot taken before the abandonment. Meanwhile the caller has had the
/// `Err` back and — for a Stop, the single most likely next action, and for a
/// wedge the action this crate's own docs tell the operator to take — may
/// already have resumed the rip against that same path with a second,
/// independent `Mapfile`. Nothing serialises two `Mapfile`s on one path (they
/// even share the `<path>.tmp` staging name), so the abandoned thread's write
/// can land last and silently revert sectors the resumed pass confirmed
/// `Finished` — minutes of recovery off damaged media, thrown away with no
/// error anywhere.
///
/// Raised on ANY `Err`, not just the leak errors, because on every OTHER
/// error path it is a provable no-op: those all come back through
/// `handle.join()`, which only observes a thread that has already terminated,
/// so the sink — and the `Mapfile` inside it — was dropped and flushed before
/// this function returned. That keeps the rule free of any dependence on
/// which `Error` variant libfreemkv uses for a leak, including the
/// close-already-in-flight leak, which has no distinct variant at all.
///
/// What it costs: an abandoned consumer no longer lands its last
/// `FLUSH_INTERVAL` (1 s) of records. That is loss in the SAFE direction —
/// both sinks write the image BEFORE they `record()`, so a mapfile missing a
/// tail of records is merely pessimistic and those ranges get re-read on
/// resume. It can never mark `Finished` a range the image did not receive.
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

    // Pre-flight decrypt gate, also enforced in `copy` but re-checked here so a
    // direct `sweep` caller can't bypass it: a decrypting sweep of an encrypted
    // disc with no usable key would write ciphertext at exit 0. No-op for `--raw`.
    crate::resolve::ensure_decryptable_strict(disc, !opts.decrypt)?;

    let total_bytes = disc.capacity_sectors as u64 * 2048;
    // Decrypt-aware read: `opts.decrypt` decrypts each unit in place (plaintext ISO);
    // otherwise pure pass-through (keys = `None`). Bad sectors are found by physical
    // read success, not decrypt structure — proven at mux time, not capture time.
    let mut keys = if opts.decrypt {
        disc.decrypt_keys()
    } else {
        libfreemkv::decrypt::DecryptKeys::None
    };
    let decrypt_is_aacs = matches!(keys, libfreemkv::decrypt::DecryptKeys::Aacs { .. });
    // AACS sweep: resolve a whole-disc key map up front (fail-loud on missing
    // CPS-unit key) and decrypt via the map — clear sectors are in no range and
    // pass through, so no separate gate is needed. CSS keeps its own content-gate.
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
    // covers_disc reconciliation: a resume against a mismatched mapfile size is
    // unsafe (copy()'s dispatch forces a fresh sweep here too) — under-cover abandons
    // the tail, over-cover reads past capacity. Same downgrade for direct sweep() calls.
    let mut resume = opts.resume;
    if resume && mapfile_path.exists() {
        match mapfile::Mapfile::load(&mapfile_path) {
            Ok(existing) => {
                // Identity first, crucially BEFORE the unconditional set_vid/
                // set_unit_keys overwrite below: that stamps the current job's
                // identity onto the mapfile, so checking after never fires.
                mapfile::check_mapfile_identity(&existing, disc)?;
                if existing.total_size() != total_bytes {
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
            Err(_) => {
                // Mapfile exists but is corrupt/unparseable. resume=true would hand
                // garbage to open_or_create and mis-track progress; downgrade so the
                // `!resume` path below drops it and the rip restarts clean.
                tracing::info!(
                    "sweep: mapfile at {} is corrupt/unparseable; forcing fresh sweep",
                    mapfile_path.display(),
                );
                resume = false;
            }
        }
    }
    if !resume {
        // A fresh sweep MUST start from an empty mapfile: if the stale file survives,
        // open_or_create loads it and the new disc inherits old Finished ranges →
        // silently zero-filled ISO. ENOENT is fine; any other error aborts.
        stale_mapfile_removed(std::fs::remove_file(&mapfile_path))?;
    }
    let mut map = mapfile::Mapfile::open_or_create(&mapfile_path, total_bytes, MAPFILE_CREATOR)
        .map_err(|e| Error::IoError { source: e })?;

    // Persist decryption state into the mapfile header (ddrescue-safe comment
    // lines, no ISO payload touched) so it survives to deferred-mux/resume.
    // KEYS XOR VID: a keyed disc writes unit keys; unresolved writes only VID.
    if !opts.unit_keys.is_empty() {
        map.set_unit_keys(&opts.unit_keys);
    } else if let Some(vid) = opts.vid {
        map.set_vid(vid);
    }

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
            .map_err(|e| Error::IoError { source: e })?;
        let reg = output_is_regular(f.metadata());
        (f, reg)
    } else {
        let f = std::fs::File::create(path).map_err(|e| Error::IoError { source: e })?;
        let reg = output_is_regular(f.metadata());
        if reg {
            f.set_len(total_bytes)
                .map_err(|e| Error::IoError { source: e })?;
        }
        (f, reg)
    };

    // Wrap the raw `File` in our bounded-cache `WritebackFile` (drains dirty
    // pages continuously instead of bursting; see `libfreemkv::io`). It moves
    // into the consumer thread.
    let file =
        libfreemkv::io::WritebackFile::new(file).map_err(|e| Error::IoError { source: e })?;
    let mut batch: u16 = sweep_batch_sectors(opts.batch_sectors, opts.skip_on_error, disc.format);

    // AACS unit alignment for a decrypting sweep: units are 3 sectors and must
    // start/span whole units or decrypt under the wrong CBC alignment. ecc_sectors()=32
    // isn't a multiple of 3, so round batch up (32→33); region starts align down below.
    batch = aacs_aligned_batch(batch, decrypt_is_aacs);

    // Pre-compute NonTried regions before handing the mapfile to the consumer
    // thread. Producer processes them in order; consumer mutates the mapfile
    // per work-item. Regions left NonTrimmed/Unreadable are the patch pass's job.
    let regions: Vec<(u64, u64)> = map.ranges_with(&[mapfile::SectorStatus::NonTried]);

    // Spawn the consumer (owns WritebackFile + Mapfile; producer keeps reader/halt).
    // `map_disown` is taken BEFORE `map` moves into the sink: it's the only way left
    // to stop an abandoned consumer writing a stale mapfile over a resumed pass.
    let map_disown = map.disown_handle();
    let (sink, prog_rx) = SweepSink::new(file, map, is_regular);
    let pipe: Pipeline<WorkItem, sweep::ConsumerSummary> =
        Pipeline::spawn_named("freemkv-sweep-consumer", DEFAULT_PIPELINE_DEPTH, sink)?;

    // Halt token for `send_bounded` below: `opts.halt` is adopted as-is; with none
    // wired, a never-cancelled token keeps SEND_DEADLINE as the only bound. Every
    // in-crate caller wires a real Stop bit; only an external caller could omit one.
    let send_halt = opts
        .halt
        .clone()
        .map(libfreemkv::halt::Halt::from_arc)
        .unwrap_or_default();

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
    let mut in_damage_zone = false;
    const DAMAGE_ZONE_EXIT_THRESHOLD: u64 = 16;
    let mut cached_snapshot: Option<ProgressSnapshot> = None;
    // Derived from `cached_snapshot.bad_ranges` + the main title only, changing
    // exactly when a new snapshot lands — not once per batch. The old per-iteration
    // recompute ran `bytes_bad_in_title` (O(ranges x extents)) up to 1.6M times/rip.
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
        // AACS unit alignment: anchor the cursor DOWN to the nearest 6144-byte unit
        // boundary so the decrypting reader never starts mid-unit. Re-reading the few
        // already-covered head sectors is idempotent; only resume regions are shifted.
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
            // The region's last block can end mid-unit on a decrypting AACS sweep
            // (region_end is only sector-snapped). Widen the physical read (never
            // accounting) to whole units; fits `buf` since `batch` is unit-aligned.
            let read_bytes =
                aacs_aligned_read_bytes(pos, block_bytes, total_bytes, decrypt_is_aacs);
            let block_lba = (pos / 2048) as u32;
            let block_count = (read_bytes / 2048) as u16;
            let recovery = !opts.skip_on_error;

            // `require_full_read`, not a bare `Ok(_)`: `buf` is reused each iteration,
            // so a short transfer would put the PREVIOUS block's tail into `Good` and
            // write it as recovered data. Routed into the Err arms instead.
            let read_result = require_full_read(
                reader.read_sectors(
                    block_lba,
                    block_count,
                    &mut buf[..read_bytes as usize],
                    recovery,
                ),
                read_bytes as usize,
                block_lba,
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

                    // Plaintext: the wrapped DecryptingSectorSource applied AACS/CSS
                    // in-place during read_sectors above; consumer sees decrypted bytes.

                    // Move the batch into the channel via fresh
                    // owned Vec. The producer's `buf` is reused
                    // for the next read.
                    let send_buf = buf[..block_bytes as usize].to_vec();
                    match send_bounded(&pipe, WorkItem::Good { pos, buf: send_buf }, &send_halt) {
                        Ok(()) => {}
                        // A Stop that lands while this send is parked is a
                        // halt, not a failure: same outcome as the loop-top
                        // check that used to be the only place it could land.
                        Err(SendStall::Halted) => {
                            halt_requested = true;
                            break 'outer;
                        }
                        Err(stall) => {
                            producer_err = Some(stall.into_error());
                            break 'outer;
                        }
                    }
                    bytes_good_done = bytes_good_done.saturating_add(block_bytes);
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
                        read_error::ReadAction::SkipBlock { pause_secs } => {
                            match send_bounded(
                                &pipe,
                                WorkItem::SkipFill {
                                    pos,
                                    len: block_bytes,
                                },
                                &send_halt,
                            ) {
                                Ok(()) => {}
                                Err(SendStall::Halted) => {
                                    halt_requested = true;
                                    break 'outer;
                                }
                                Err(stall) => {
                                    producer_err = Some(stall.into_error());
                                    break 'outer;
                                }
                            }
                            bytes_done = bytes_done.saturating_add(block_bytes);
                            sleep_secs_or_halt(pause_secs, opts.halt.as_ref());
                            pos += block_bytes;
                        }
                        read_error::ReadAction::JumpAhead {
                            sectors,
                            pause_secs,
                        } => {
                            match send_bounded(
                                &pipe,
                                WorkItem::SkipFill {
                                    pos,
                                    len: block_bytes,
                                },
                                &send_halt,
                            ) {
                                Ok(()) => {}
                                Err(SendStall::Halted) => {
                                    halt_requested = true;
                                    break 'outer;
                                }
                                Err(stall) => {
                                    producer_err = Some(stall.into_error());
                                    break 'outer;
                                }
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

                            // Saturating throughout: read_error computes sector count
                            // with saturating_mul as "defence in depth"; honor the same
                            // guarantee here so a pathological jump distance can't wrap.
                            let jump_pos = pos
                                .saturating_add(block_bytes)
                                .saturating_add(sectors.saturating_mul(2048))
                                .min(region_end);
                            let gap_start = pos + block_bytes;
                            let gap_bytes = jump_pos.saturating_sub(gap_start);
                            if gap_bytes > 0 {
                                match send_bounded(
                                    &pipe,
                                    WorkItem::GapFill {
                                        pos: gap_start,
                                        len: gap_bytes,
                                    },
                                    &send_halt,
                                ) {
                                    Ok(()) => {}
                                    Err(SendStall::Halted) => {
                                        halt_requested = true;
                                        break 'outer;
                                    }
                                    Err(stall) => {
                                        producer_err = Some(stall.into_error());
                                        break 'outer;
                                    }
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
                // Use the latest consumer snapshot if present, else synthesise from the
                // two producer counters. `bytes_good` is recovery-only, NOT `bytes_done`
                // (a position advancing over skipped/zero-fills too, once conflated).
                let main_title = disc.titles.first();
                let main_title_bad = cached_main_title_bad;
                // Consumer snapshot is truth for unreadable/pending, but bytes_good lags
                // the producer when consumer is behind — take the max so display never
                // regresses. Floor is `bytes_good_done`, not `bytes_done` (damage paths).
                let (bytes_good, bytes_unreadable, bytes_pending, bytes_retryable) =
                    match &cached_snapshot {
                        Some(snap) => (
                            snap.stats.bytes_good.max(bytes_good_done),
                            snap.stats.bytes_unreadable,
                            snap.stats.bytes_pending,
                            snap.stats.bytes_retryable,
                        ),
                        // No snapshot yet: derive the partition from the two producer
                        // counters. Done-but-not-good bytes are this pass's damage
                        // (skip/fail/zero-fill); they must land in a bucket, not vanish.
                        None => (
                            bytes_good_done,
                            0u64,
                            total_bytes.saturating_sub(bytes_done),
                            bytes_done.saturating_sub(bytes_good_done),
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

    // Producer is done; let the consumer drain and run close() (writeback, fsync,
    // mapfile.flush). Bounded by the SAME halt the sends above use — a plain
    // `Pipeline::finish` would re-block on the stalled consumer, so Stop never returns.
    let summary = finish_bounded_disowning(pipe, &send_halt, &map_disown);

    // Producer-side error wins over consumer-side (the read failure
    // is what motivated quitting; the consumer's flush error, if
    // any, is downstream).
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
    /// On-decrypt-miss key fetch (see [`libfreemkv::sector::KeyFetch`]).
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

/// Options for [`sweep()`] (Pass 1 / forward sequential pass).
///
/// Named `Disc::sweep` before 1.6.0, when recovery moved out of libfreemkv
/// and the receiver became a `&Disc` argument.
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

/// Options for [`patch()`] (Pass N retry pass over bad ranges).
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
    /// NOTE on `block_sectors: Some(32)`: it no longer sizes any read. The
    /// adaptive 32→1→32 batching this comment used to describe was replaced by
    /// the handler chain (`section_recover.rs`), which owns read sizing and
    /// bisection. What survives is the pass LABEL — >1 reports a Trim pass, 1
    /// reports a Scrape pass. Likewise `full_recovery` is now diagnostics-only
    /// and `wedged_threshold` is reported in the outcome, not enforced. See
    /// `patch_preset_tests` for what each value actually does.
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

/// Result returned by [`patch()`].
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
/// `(pos + len).div_ceil(SECTOR) * SECTOR` overflows u64 for a range ending in
/// the last sector of the address space, which `Mapfile::load` accepts (it
/// checks `checked_add`, and that does not wrap). The old fix saturated the
/// END of that computation but not the LENGTH derived from it, so a range
/// this close to `u64::MAX` came back with a length that was not a multiple
/// of `SECTOR` (2047 instead of 0) — silently breaking the one invariant
/// every recovery handler relies on (`count = len / SECTOR` truncates to 0,
/// producing a zero-sector read that is reported as a successful recovery of
/// bytes nobody read).
///
/// Do the rounding-up arithmetic in u128 so `pos + len` can never overflow,
/// then cap the result at the largest sector-aligned value a u64 can hold.
/// There is no way to represent the true end of a range whose last whole
/// sector runs past `u64::MAX` (its top edge is `2^64`, not representable),
/// so that case is capped down to the last representable sector boundary —
/// which here coincides with `start` itself, correctly yielding a length of
/// 0 rather than a fabricated sub-sector remainder. Every caller
/// (`Linear`/`Jump`/`CachePrime` via `SubRanges::from_section`, and the sweep
/// loop's `while pos < region_end`) already treats a 0-length range as
/// "nothing to do here", so this stays safe: never a phantom read past the
/// end, never a non-whole-sector length.
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
        // Length must ALWAYS be a whole number of sectors (every handler's `count
        // = len / SECTOR` depends on it). The true final sector runs past u64::MAX
        // and can't be represented, so 0 is the only whole-sector-respecting answer.
        assert_eq!(
            len % 2048,
            0,
            "returned length must be a whole number of sectors"
        );
        assert_eq!(len, 0, "the final sector here cannot be represented in u64");
    }

    /// The u128 overflow-proofing above must not change ordinary, non-extreme
    /// snapping. Pinned independently of the top-of-address-space edge test:
    /// mutate the fix and this test should stay green while the edge test
    /// above goes red, proving the two paths are decoupled.
    #[test]
    fn a_normal_range_is_unaffected_by_the_overflow_fix() {
        assert_eq!(snap_to_sectors(512, 1024), (0, 2048));
        assert_eq!(snap_to_sectors(1_000_000, 5_000), (999_424, 6144));
    }
}

/// The producer-side handoff guard: what happens to a `send` when the consumer
/// is ALIVE but not draining.
///
/// The defect these pin: both recovery producers used a plain
/// `Pipeline::send`, which only ever fails when the consumer is DEAD. A
/// consumer STALLED inside `WritebackFile::write_all` on a hung mount is alive,
/// so the producer parked in `send` forever — it polls its halt token only at
/// the top of its loop, so a Stop pressed at that moment could never land.
#[cfg(test)]
mod send_bounded_tests {
    use super::*;
    use libfreemkv::halt::Halt;
    use libfreemkv::io::pipeline::{Flow, Pipeline, Sink, WRITE_THROUGH_DEPTH};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Cancel is flipped this long after the producer parks in `send`. Long
    /// enough that the send is definitely parked, short enough to keep the
    /// test quick.
    const HALT_AFTER: Duration = Duration::from_millis(500);
    /// The margin this test relies on, stated explicitly. `send_with_halt`
    /// observes the halt within one `libfreemkv::halt::POLL_INTERVAL`
    /// (250 ms), so the expected return is ~750 ms after the send parks; 3 s
    /// is 4x that, leaving 2.25 s of slack for a loaded box. The regression it
    /// separates from is not "slower" but UNBOUNDED — the stalled sink is
    /// released only after the watchdog below fires — so there is no window in
    /// which the two behaviours could be confused.
    const MAX_RETURN: Duration = Duration::from_secs(3);
    /// Bound on the whole experiment so a regression FAILS instead of hanging
    /// the suite.
    const WATCHDOG: Duration = Duration::from_secs(10);

    /// A gate a test thread can hold shut and then open.
    #[derive(Clone)]
    struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        fn shut() -> Self {
            Self(Arc::new((Mutex::new(false), Condvar::new())))
        }
        fn wait(&self) {
            let (lock, cv) = &*self.0;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }
        fn open(&self) {
            let (lock, cv) = &*self.0;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
    }

    /// The hung-mount consumer: healthy, alive, draining nothing because its
    /// first `apply` is stuck in a write that never returns. Stands in for
    /// `SweepSink`/`PatchSink` blocked inside `WritebackFile::write_all`.
    struct StalledSink {
        entered: Arc<AtomicUsize>,
        gate: Gate,
    }

    impl Sink<u32> for StalledSink {
        type Output = ();
        fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.gate.wait();
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }

    /// A consumer that DIES on its first item — the case a plain `send` can
    /// already detect, and which must stay distinguishable from the stalled
    /// one. It has to panic: `Flow::Stop` and an `apply` error both leave the
    /// consumer thread alive and draining, so neither disconnects the channel.
    struct DyingSink;

    impl Sink<u32> for DyingSink {
        type Output = ();
        fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
            panic!("test fixture: consumer thread dies here");
        }
        fn close(self) -> std::result::Result<(), Error> {
            Ok(())
        }
    }

    /// Park a producer against a stalled consumer, then flip the halt: the
    /// producer must come back promptly, and must report HALTED — not
    /// `PipelineConsumerGone`, which would be a lie about a consumer that is
    /// alive.
    #[test]
    fn a_stop_lands_on_a_producer_parked_on_a_stalled_consumer() {
        let entered = Arc::new(AtomicUsize::new(0));
        let gate = Gate::shut();
        let pipe = Pipeline::<u32, ()>::spawn(
            WRITE_THROUGH_DEPTH,
            StalledSink {
                entered: Arc::clone(&entered),
                gate: gate.clone(),
            },
        )
        .expect("spawn consumer");
        let halt = Halt::new();

        // Item 1 is taken by the consumer, which then wedges inside `apply`.
        assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
        let waited = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                waited.elapsed() < WATCHDOG,
                "consumer never picked up the first item"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Item 2 fills the depth-1 channel. Now the pipeline is saturated and
        // the consumer is not coming back.
        assert_eq!(send_bounded(&pipe, 2, &halt), Ok(()));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(HALT_AFTER);
                halt.cancel();
            });
            scope.spawn(|| {
                let t0 = Instant::now();
                // Item 3 has nowhere to go: this is the send that used to be
                // unkillable.
                let result = send_bounded(&pipe, 3, &halt);
                let _ = tx.send((result, t0.elapsed()));
            });

            let observed = rx.recv_timeout(WATCHDOG);
            // Release the consumer whatever happened, so both scoped threads
            // can join and the failure below is a FAILURE, not a hang.
            gate.open();

            let (result, elapsed) = observed.unwrap_or_else(|_| {
                panic!(
                    "producer did not return within {WATCHDOG:?} of a halt \
                     raised {HALT_AFTER:?} in: it is parked in send() on a \
                     consumer that is alive but stalled, which is exactly the \
                     wedge Stop has to be able to break"
                )
            });
            assert_eq!(
                result,
                Err(SendStall::Halted),
                "a stalled consumer plus a halt is a HALT; reporting \
                 ConsumerGone would blame a thread that is alive"
            );
            assert!(
                elapsed < MAX_RETURN,
                "producer took {elapsed:?} to observe a halt raised at \
                 {HALT_AFTER:?}; budget is {MAX_RETURN:?}"
            );
        });

        drop(pipe);
    }

    /// The other side of the discrimination: a consumer that is really gone
    /// must still report `ConsumerGone`, not `Halted`, with the halt clear.
    #[test]
    fn a_departed_consumer_is_reported_gone_not_halted() {
        let pipe = Pipeline::<u32, ()>::spawn(WRITE_THROUGH_DEPTH, DyingSink).expect("spawn");
        let halt = Halt::new();
        // First send may land before the consumer exits; keep sending until
        // the channel disconnects (bounded, so a regression fails).
        let t0 = Instant::now();
        loop {
            match send_bounded(&pipe, 7, &halt) {
                Ok(()) => {
                    assert!(
                        t0.elapsed() < WATCHDOG,
                        "consumer never departed: sends kept succeeding"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(stall) => {
                    assert_eq!(stall, SendStall::ConsumerGone);
                    break;
                }
            }
        }
        assert!(!halt.is_cancelled(), "no halt was ever raised");
    }

    /// The deadline branch: consumer alive, stalled, and NO halt. The producer
    /// must still come back — as `Stalled`, which maps to a timeout error, not
    /// to `PipelineConsumerGone`.
    #[test]
    fn a_stalled_consumer_with_no_halt_times_out_rather_than_blocking() {
        let entered = Arc::new(AtomicUsize::new(0));
        let gate = Gate::shut();
        let pipe = Pipeline::<u32, ()>::spawn(
            WRITE_THROUGH_DEPTH,
            StalledSink {
                entered: Arc::clone(&entered),
                gate: gate.clone(),
            },
        )
        .expect("spawn consumer");
        let halt = Halt::new();
        assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
        let waited = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(waited.elapsed() < WATCHDOG, "consumer never started");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(send_bounded(&pipe, 2, &halt), Ok(()));

        // A short deadline stands in for SEND_DEADLINE so this costs 600 ms
        // rather than 600 s. `send_with_halt` parks in POLL_INTERVAL slices,
        // so anything under one slice would not exercise the loop.
        let deadline = Duration::from_millis(600);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let t0 = Instant::now();
                let result = send_bounded_within(&pipe, 3, &halt, deadline);
                let _ = tx.send((result, t0.elapsed()));
            });
            // Same watchdog discipline as the halt test: a producer that never
            // returns must FAIL the suite, not hang it.
            let observed = rx.recv_timeout(WATCHDOG);
            gate.open();

            let (result, elapsed) = observed.unwrap_or_else(|_| {
                panic!(
                    "producer did not return within {WATCHDOG:?} against a \
                     deadline of {deadline:?}: an unbounded send on a stalled \
                     consumer never comes back at all"
                )
            });
            assert_eq!(
                result,
                Err(SendStall::Stalled),
                "an alive-but-not-draining consumer is a stall, not a death"
            );
            assert!(
                matches!(SendStall::Stalled.into_error(), Error::PipelineJoinTimeout),
                "the stall must surface as a timeout, not as PipelineConsumerGone"
            );
            assert!(
                elapsed < MAX_RETURN,
                "deadline of {deadline:?} took {elapsed:?} to fire"
            );
        });
        drop(pipe);
    }

    /// The other half of the halt contract, and the regression that routing
    /// every recovery send through `send_with_halt` introduced.
    ///
    /// `send_with_halt` checks the halt BEFORE it touches the channel, so once
    /// Stop is pressed the very next item is handed straight back — even when
    /// the channel has a free slot and the delivery would not have blocked for
    /// a microsecond. The plain `send` it replaced delivered that item. What
    /// gets thrown away is not a queue entry: on patch it is a span the drive
    /// may have spent minutes clawing off a damaged disc, and on sweep a 64 KiB
    /// batch already read. Pressing Stop asks to stop DOING more work; it does
    /// not ask to discard work already done.
    ///
    /// The halt's job is to decide what happens when there is NOWHERE to put
    /// the item — that is the parked-producer case the sibling test above pins.
    /// It is not a licence to drop a free-slot handoff.
    #[test]
    fn a_raised_halt_still_delivers_when_the_channel_has_room() {
        /// Stalls inside `apply` on the FIRST item only. That reproduces the
        /// live shape — a consumer busy in a slow write — while leaving the
        /// depth-1 channel EMPTY, i.e. with room for exactly one more item.
        struct StallOnceSink {
            entered: Arc<AtomicUsize>,
            gate: Gate,
            seen: Arc<Mutex<Vec<u32>>>,
        }

        impl Sink<u32> for StallOnceSink {
            type Output = ();
            fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
                if self.entered.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.gate.wait();
                }
                self.seen.lock().unwrap().push(item);
                Ok(Flow::Continue)
            }
            fn close(self) -> std::result::Result<(), Error> {
                Ok(())
            }
        }

        let entered = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = Gate::shut();
        let pipe = Pipeline::<u32, ()>::spawn(
            WRITE_THROUGH_DEPTH,
            StallOnceSink {
                entered: Arc::clone(&entered),
                gate: gate.clone(),
                seen: Arc::clone(&seen),
            },
        )
        .expect("spawn consumer");
        let halt = Halt::new();

        // Item 1 is taken off the channel and the consumer wedges on it. The
        // channel is now EMPTY: one free slot, no producer can block on it.
        assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
        let waited = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                waited.elapsed() < WATCHDOG,
                "consumer never picked up the first item"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // Stop is pressed. Item 2 is the already-recovered span in the
        // producer's hand at that instant.
        halt.cancel();
        let result = send_bounded(&pipe, 2, &halt);

        // Release the consumer before asserting, so a failure is a FAILURE and
        // not a hang in `finish`.
        gate.open();

        assert_eq!(
            result,
            Ok(()),
            "the channel had a free slot, so this handoff could not block: a \
             raised halt must not discard bytes the drive already recovered"
        );
        pipe.finish().expect("clean close");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![1, 2],
            "item 2 was accepted, so the consumer must have written it"
        );
    }

    /// Guard the healthy path: with a consumer that drains, `send_bounded` is
    /// an ordinary send — every item lands, in order, and nothing is dropped.
    #[test]
    fn a_draining_consumer_still_receives_every_item() {
        struct CountingSink(Arc<AtomicUsize>);
        impl Sink<u32> for CountingSink {
            type Output = usize;
            fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
                self.0.fetch_add(item as usize, Ordering::SeqCst);
                Ok(Flow::Continue)
            }
            fn close(self) -> std::result::Result<usize, Error> {
                Ok(self.0.load(Ordering::SeqCst))
            }
        }
        let seen = Arc::new(AtomicUsize::new(0));
        let pipe =
            Pipeline::<u32, usize>::spawn(WRITE_THROUGH_DEPTH, CountingSink(Arc::clone(&seen)))
                .expect("spawn");
        let halt = Halt::new();
        for i in 1..=1000u32 {
            assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
        }
        assert_eq!(pipe.finish().expect("clean close"), 500_500);
    }
}

/// The join-side half of the same guarantee `send_bounded_tests` pins.
///
/// The defect these pin: `send_bounded` gets the PRODUCER out from under a
/// stalled consumer, but the statement immediately after it unparks is the
/// teardown — and both recovery sites tore down with plain `Pipeline::finish`,
/// a blocking `join()` on that same stalled consumer. So a Stop that
/// `send_bounded` finally made land still never returned to the caller; the
/// wedge had only moved from `send` to `finish`. `finish_with_halt` is the
/// bound libfreemkv ships for it, and nothing in this crate called it.
#[cfg(test)]
mod finish_bounded_tests {
    use super::*;
    use libfreemkv::halt::Halt;
    use libfreemkv::io::pipeline::{Flow, Pipeline, Sink, WRITE_THROUGH_DEPTH};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// THE MARGIN THIS TEST RELIES ON. `finish_with_halt` observes the halt
    /// within one `libfreemkv::halt::POLL_INTERVAL` (250 ms), then spends a
    /// fixed `FINISH_GRACE_SECS` (5 s) spin-polling before it accepts the
    /// abandonment — so a correct teardown returns at ~5.3 s. 15 s is ~3x
    /// that, leaving ~10 s of slack for a loaded box.
    ///
    /// The regression this separates from is not "slower" but UNBOUNDED: the
    /// stalled sink is released only after the watchdog below fires, so
    /// `pipe.finish()` cannot return early by luck and the two behaviours have
    /// no window in which they could be confused.
    const MAX_RETURN: Duration = Duration::from_secs(15);
    /// Bound on the whole experiment so a regression FAILS rather than hanging
    /// the suite — the teardown under test is precisely a call that does not
    /// come back, so an unguarded `join()` here would wedge `cargo test`.
    const WATCHDOG: Duration = Duration::from_secs(45);

    /// A gate a test thread can hold shut and then open.
    #[derive(Clone)]
    struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        fn shut() -> Self {
            Self(Arc::new((Mutex::new(false), Condvar::new())))
        }
        fn wait(&self) {
            let (lock, cv) = &*self.0;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }
        fn open(&self) {
            let (lock, cv) = &*self.0;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
    }

    /// The hung-mount consumer: alive, healthy, and stuck in its first `apply`
    /// forever. Stands in for `SweepSink`/`PatchSink` blocked inside
    /// `WritebackFile::write_all` on a mount that never answers.
    struct StalledSink {
        entered: Arc<AtomicUsize>,
        gate: Gate,
        closed: Arc<AtomicUsize>,
    }

    impl Sink<u32> for StalledSink {
        type Output = u32;
        fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.gate.wait();
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<u32, Error> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    /// Park a consumer inside `apply`, raise the halt, then tear the pipeline
    /// down. The teardown must COME BACK — this is the last link in the Stop
    /// chain, and until it returns the operator's Stop has not happened no
    /// matter how promptly the producer unparked.
    #[test]
    fn a_stop_lands_on_a_teardown_joining_a_stalled_consumer() {
        let entered = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let gate = Gate::shut();
        let pipe = Pipeline::<u32, u32>::spawn(
            WRITE_THROUGH_DEPTH,
            StalledSink {
                entered: Arc::clone(&entered),
                gate: gate.clone(),
                closed: Arc::clone(&closed),
            },
        )
        .expect("spawn consumer");
        let halt = Halt::new();

        // Item 1 is taken; the consumer wedges inside `apply` and never
        // returns. This is the state `send_bounded` leaves behind after it
        // hands the producer back on a halt.
        assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
        let waited = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                waited.elapsed() < WATCHDOG,
                "consumer never picked up the first item"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // The operator's Stop. Raised BEFORE the teardown, exactly as the
        // production sites see it: the producer has already observed this bit
        // and broken out of its read loop.
        halt.cancel();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let t0 = Instant::now();
                // The teardown that used to be unkillable.
                let result = finish_bounded(pipe, &halt);
                let _ = tx.send((result.is_err(), t0.elapsed()));
            });

            let observed = rx.recv_timeout(WATCHDOG);
            // Release the stalled consumer whatever happened, so the scoped
            // thread can join and the assertion below is a FAILURE, not a hang.
            gate.open();

            let (was_err, elapsed) = observed.unwrap_or_else(|_| {
                panic!(
                    "teardown did not return within {WATCHDOG:?} of a halt \
                     raised before it was even called: it is blocked in \
                     join() on a consumer that is alive but stalled. \
                     send_bounded got the producer out of exactly this wedge; \
                     leaving it here means Stop still never returns"
                )
            });
            assert!(
                was_err,
                "a consumer that never came back has no summary to report: \
                 the teardown must say Halted, not invent a result"
            );
            assert!(
                elapsed < MAX_RETURN,
                "teardown took {elapsed:?} to return after a halt; budget is \
                 {MAX_RETURN:?} (5 s grace + one 250 ms poll, ~3x margin)"
            );
        });

        // The abandoned consumer must NOT go on to run `close()`. For the
        // recovery sinks that call is `sync_all` + `map.flush()` against an
        // output the caller has already reported as interrupted.
        assert_eq!(
            closed.load(Ordering::SeqCst),
            0,
            "an abandoned consumer must not finalise the output"
        );
    }

    /// Sets a flag when it is dropped. Declared AFTER the `Mapfile` field in
    /// [`MapSink`] so it fires only once that mapfile has been dropped —
    /// i.e. after its `Drop` flush has had its chance at the file.
    struct SignalOnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for SignalOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// The hung-mount consumer of the recovery pipelines, in miniature: it
    /// owns the pass's `Mapfile`, it blocks inside `apply` exactly where
    /// `PatchSink`/`SweepSink` block (in the write, BEFORE the `record`),
    /// and it records once the write comes back.
    struct MapSink {
        map: mapfile::Mapfile,
        done: SignalOnDrop,
        gate: Gate,
        entered: Arc<AtomicUsize>,
    }

    impl Sink<u32> for MapSink {
        type Output = u32;
        fn apply(&mut self, _item: u32) -> std::result::Result<Flow, Error> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            // The wedged write on the hung mount.
            self.gate.wait();
            // …which eventually returns, and the sink records it — the
            // interval since the last persist is long past, so this alone
            // rewrites the whole mapfile.
            self.map
                .record(0, 2048, mapfile::SectorStatus::Unreadable)
                .map_err(|e| Error::IoError { source: e })?;
            Ok(Flow::Continue)
        }
        fn close(self) -> std::result::Result<u32, Error> {
            Ok(0)
        }
    }

    /// A STOP MUST NOT COST THE NEXT PASS ITS RECORD. The consumer abandoned
    /// on a hung mount is detached, not killed: it still owns the pass's
    /// `Mapfile`, and its stale snapshot must never reach the path again —
    /// because by the time its write returns, the operator has done the
    /// ordinary next thing after a Stop (or the thing this crate's docs tell
    /// them to do after a wedge) and resumed the rip, whose own `Mapfile` is
    /// now the record. A whole-file rewrite from the old snapshot silently
    /// reverts sectors the resumed pass confirmed `Finished` — minutes of
    /// recovery off damaged media, discarded with no error anywhere.
    #[test]
    fn an_abandoned_consumer_cannot_overwrite_a_resumed_passs_mapfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.iso.mapfile");
        let total = 4096u64;

        // The abandoned pass's mapfile, with in-memory state that has not
        // reached disk (record() batches by FLUSH_INTERVAL).
        let mut map = mapfile::Mapfile::create(&path, total, "abandoned-pass")
            .expect("create the pass mapfile");
        map.record(2048, 2048, mapfile::SectorStatus::NonTrimmed)
            .expect("record");
        let disown = map.disown_handle();

        let entered = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Gate::shut();
        let pipe = Pipeline::<u32, u32>::spawn(
            WRITE_THROUGH_DEPTH,
            MapSink {
                map,
                done: SignalOnDrop(Arc::clone(&dropped)),
                gate: gate.clone(),
                entered: Arc::clone(&entered),
            },
        )
        .expect("spawn consumer");
        let halt = Halt::new();

        assert_eq!(send_bounded(&pipe, 1, &halt), Ok(()));
        let waited = Instant::now();
        while entered.load(Ordering::SeqCst) == 0 {
            assert!(
                waited.elapsed() < WATCHDOG,
                "consumer never picked up the first item"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // The operator's Stop, and the teardown that abandons the wedged
        // consumer — the SAME call both recovery passes make.
        halt.cancel();
        assert!(
            finish_bounded_disowning(pipe, &halt, &disown).is_err(),
            "a consumer that never came back has no summary to report"
        );

        // The resume. A second, independent `Mapfile` on the same path, as
        // `sweep`/`patch` would build it, recording real progress.
        {
            let mut resumed = mapfile::Mapfile::create(&path, total, "resumed-pass")
                .expect("create the resumed mapfile");
            resumed
                .record(0, total, mapfile::SectorStatus::Finished)
                .expect("record");
            resumed.flush().expect("flush the resumed mapfile");
        }
        let after_resume = std::fs::read_to_string(&path).expect("read the resumed mapfile");
        assert!(
            after_resume.contains("resumed-pass"),
            "fixture check: the resumed pass's mapfile is what is on disk"
        );

        // The mount comes back and the abandoned thread runs on: its
        // `record` persists, then its sink — and the mapfile in it — drops.
        gate.open();
        let waited = Instant::now();
        while !dropped.load(Ordering::SeqCst) {
            assert!(
                waited.elapsed() < WATCHDOG,
                "the abandoned consumer never released its mapfile"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let now = std::fs::read_to_string(&path).expect("read the mapfile");
        assert_eq!(
            now, after_resume,
            "the abandoned consumer overwrote the resumed pass's mapfile with \
             its own stale snapshot — the resume's recorded progress is gone \
             from disk and those ranges will be read off the damaged disc all \
             over again"
        );
    }

    /// THE CLEAN PATH MUST BE EXACT. A healthy consumer is joined and its
    /// `close()` output returned unchanged — including on a run that ended
    /// halted, which is the common Stop, and which must still report the real
    /// summary rather than an error.
    #[test]
    fn a_healthy_consumer_is_still_joined_and_its_summary_returned() {
        struct CountingSink(u32);
        impl Sink<u32> for CountingSink {
            type Output = u32;
            fn apply(&mut self, item: u32) -> std::result::Result<Flow, Error> {
                self.0 += item;
                Ok(Flow::Continue)
            }
            fn close(self) -> std::result::Result<u32, Error> {
                Ok(self.0)
            }
        }

        // (a) No halt raised: an ordinary end-of-pass teardown.
        let pipe =
            Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, CountingSink(0)).expect("spawn");
        let halt = Halt::new();
        for i in 1..=1000u32 {
            assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
        }
        assert_eq!(
            finish_bounded(pipe, &halt).expect("a draining consumer joins cleanly"),
            500_500,
            "the clean path must return the consumer's summary unchanged"
        );

        // (b) Halt RAISED, consumer healthy — the common Stop. Must not cost the
        // caller its summary: `finish_with_halt` checks `is_finished()` before the
        // halt, so a consumer that's coming back is still joined normally.
        let pipe =
            Pipeline::<u32, u32>::spawn(WRITE_THROUGH_DEPTH, CountingSink(0)).expect("spawn");
        let halt = Halt::new();
        for i in 1..=100u32 {
            assert_eq!(send_bounded(&pipe, i, &halt), Ok(()));
        }
        halt.cancel();
        let t0 = Instant::now();
        assert_eq!(
            finish_bounded(pipe, &halt)
                .expect("a halted-but-healthy run still reports its summary"),
            5_050,
            "pressing Stop must not turn a completed pass's summary into an error"
        );
        assert!(
            t0.elapsed() < MAX_RETURN,
            "a healthy consumer must join promptly even with the halt up, \
             not sit out the full grace period"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image guard is ONE definition used by copy, sweep and patch. These
    /// pin the two questions each of them asks, because they used to ask them
    /// with hand-written comparisons that disagreed: copy tested equality,
    /// sweep tested "shorter than", and patch tested nothing at all.
    #[test]
    fn image_state_answers_intact_and_short_separately() {
        let exact = ImageState {
            len: 4096,
            want: 4096,
        };
        assert!(exact.is_intact());
        assert!(!exact.is_short(), "the right length is not short");

        let short = ImageState {
            len: 2048,
            want: 4096,
        };
        assert!(!short.is_intact());
        assert!(short.is_short(), "this is the case that invents good data");

        // A LONGER file is not the image this mapfile describes either, so it
        // is not intact — but it is not the dangerous case, so not short.
        let long = ImageState {
            len: 8192,
            want: 4096,
        };
        assert!(
            !long.is_intact(),
            "a longer file is not this mapfile's image"
        );
        assert!(!long.is_short());
    }

    /// A missing file reports length 0 rather than erroring: absent is exactly
    /// as inconsistent with a mapfile claiming progress as empty is, and both
    /// self-heal the same way.
    #[test]
    fn image_state_treats_a_missing_file_as_zero_length() {
        let dir = std::env::temp_dir().join(format!("fmkv-imgstate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let absent = dir.join("not-here.iso");
        let _ = std::fs::remove_file(&absent);

        let st = image_state(&absent, 4096).expect("a missing image is not an error");
        assert_eq!(st.len, 0);
        assert!(st.is_short(), "absent must read as short, not as intact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measured against the real file, not a guess.
    #[test]
    fn image_state_measures_the_file_on_disk() {
        let dir = std::env::temp_dir().join(format!("fmkv-imgstate-len-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("short.iso");
        std::fs::write(&f, vec![0u8; 2048]).unwrap();

        assert!(image_state(&f, 4096).unwrap().is_short());
        assert!(image_state(&f, 2048).unwrap().is_intact());
        let _ = std::fs::remove_dir_all(&dir);
    }

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

        // EXACT expected values: properties alone aren't enough, since an impl
        // that always returned 0 would satisfy "aligned"/"moved down" while
        // silently discarding every byte of resume progress.
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
mod resume_decision_tests {
    use super::*;
    use std::io::ErrorKind;

    fn err(kind: ErrorKind) -> std::io::Result<std::fs::Metadata> {
        Err(std::io::Error::from(kind))
    }

    /// NotFound is the only error that means "no file yet".
    ///
    /// Every other error is UNKNOWN, and the difference matters more than any
    /// other line in this file: the two call sites use this to decide whether
    /// to open the existing image or `File::create` over it. Answer "missing"
    /// on a transient EIO from a flaky staging volume and a populated ISO is
    /// truncated to zeros — bytes the mapfile still records `Finished`, which
    /// the producer will therefore never re-read.
    ///
    /// Not reachable through a filesystem fixture: any path that makes
    /// `metadata()` fail with something other than NotFound also makes the
    /// immediately following `File::create` fail the same way, so the original
    /// and a mutant both come back `Err` with the same errno. Hence the
    /// decision is a function taking the `io::Result` directly.
    #[test]
    fn only_not_found_means_the_image_is_missing() {
        assert_eq!(
            iso_len_from_metadata(err(ErrorKind::NotFound)).unwrap(),
            IsoLen::Missing
        );
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
            ErrorKind::TimedOut,
            ErrorKind::InvalidInput,
        ] {
            assert!(
                iso_len_from_metadata(err(kind)).is_err(),
                "{kind:?} is not evidence that the image is absent — it must \
                 abort, not fall through to a truncating create"
            );
        }
    }

    /// And a real length comes through as itself, including zero.
    #[test]
    fn a_readable_image_reports_its_own_length() {
        let dir = std::env::temp_dir().join(format!("fmkv-isolen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.iso");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(
            iso_len_from_metadata(std::fs::metadata(&empty)).unwrap(),
            IsoLen::Len(0),
            "an existing zero-length image is NOT the same as an absent one"
        );

        let full = dir.join("full.iso");
        std::fs::write(&full, vec![0u8; 4096]).unwrap();
        assert_eq!(
            iso_len_from_metadata(std::fs::metadata(&full)).unwrap(),
            IsoLen::Len(4096)
        );
        assert_eq!(
            iso_len_from_metadata(std::fs::metadata(dir.join("nope.iso"))).unwrap(),
            IsoLen::Missing
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stale mapfile that will not delete must abort the fresh sweep.
    #[test]
    fn a_stale_mapfile_that_cannot_be_removed_aborts_the_fresh_sweep() {
        assert!(stale_mapfile_removed(Ok(())).is_ok());
        assert!(
            stale_mapfile_removed(Err(std::io::Error::from(ErrorKind::NotFound))).is_ok(),
            "already gone is the same as removed"
        );
        for kind in [ErrorKind::PermissionDenied, ErrorKind::Other] {
            assert!(
                stale_mapfile_removed(Err(std::io::Error::from(kind))).is_err(),
                "{kind:?}: proceeding would inherit the previous disc's Finished \
                 ranges and silently zero-fill the new ISO there"
            );
        }
    }

    /// The default batch size is a mode decision, not a constant.
    #[test]
    fn the_sweep_batch_defaults_by_mode_and_format() {
        use libfreemkv::DiscFormat::*;

        // skip-on-error (multipass Pass 1): one ECC block, so one skipped
        // batch loses exactly one ECC block.
        assert_eq!(sweep_batch_sectors(None, true, Uhd), 32);
        assert_eq!(sweep_batch_sectors(None, true, BluRay), 32);
        assert_eq!(sweep_batch_sectors(None, true, Dvd), 16);
        assert_eq!(sweep_batch_sectors(None, true, HdDvd), 16);

        // Clean sweep: the larger optical batch, regardless of format.
        assert_eq!(sweep_batch_sectors(None, false, Uhd), 60);
        assert_eq!(sweep_batch_sectors(None, false, Dvd), 60);

        // An explicit request wins in either mode.
        assert_eq!(sweep_batch_sectors(Some(7), true, Uhd), 7);
        assert_eq!(sweep_batch_sectors(Some(7), false, Uhd), 7);

        // Zero is clamped: a zero batch makes block_bytes zero, so `pos` never
        // advances and the producer spins forever.
        assert_eq!(sweep_batch_sectors(Some(0), true, Uhd), 1);
        assert_eq!(sweep_batch_sectors(Some(0), false, Dvd), 1);
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

/// The shipped Pass-N patch preset, pinned.
///
/// `PatchOptions::for_patch_pass` exists so the two routes into a patch pass
/// cannot drift, and five test sites nevertheless hand-wrote its four values as
/// literals — which meant the SHIPPED preset was exercised by nothing: every
/// value could be changed in production and the whole suite stayed green
/// (verified: `block_sectors: Some(1), full_recovery: false, reverse: false,
/// wedged_threshold: 8` passes 285 unit + 56 integration tests). The literals
/// are gone; this is the assertion that makes the preset load-bearing.
///
/// WHAT THESE FOUR VALUES ACTUALLY DO — read this before "fixing" a value here
/// because a doc comment somewhere says it tunes recovery. Three of the four no
/// longer steer the pass at all; the handler chain in `section_recover.rs` owns
/// read sizing, per-read timeouts and the wedge exit:
/// - `block_sectors: Some(32)` — LABEL ONLY. It does not size any read. Its one
///   observable effect is the pass label the front end renders: `Some(1)` reads
///   as a SCRAPE pass, anything larger as a TRIM pass
///   (`patch::pass_kind`). Asserted below through that production function.
/// - `reverse: true` — LABEL ONLY. It decorates the same `PassKind`. It does
///   NOT order the walk: `PatchCtx::run` sorts bad ranges (size desc, pos asc).
///   Asserted below through the label.
/// - `full_recovery: true` — DIAGNOSTIC ONLY. `patch()` logs it as `recovery=`
///   and nothing reads it. Pinned below as a value, not as a behaviour, and
///   deliberately not dressed up as one.
/// - `wedged_threshold: 50` — REPORTED, NOT ENFORCED. Nothing counts wedged
///   reads against it; `PatchOutcome::wedged_exit` comes from a handler's
///   `TransportFault`. The threshold is echoed verbatim into the outcome for
///   the caller to render, which is what the test asserts (through
///   `build_outcome`, not by re-reading the field).
///
/// So this is honestly "the preset, and the labels/echoes it produces" — not
/// recovery tuning. The reason it still earns its place: the preset exists so
/// the two routes into a patch pass cannot drift, and five test sites used to
/// hand-write its four values as literals, which meant the SHIPPED preset was
/// exercised by nothing (verified: `block_sectors: Some(1), full_recovery:
/// false, reverse: false, wedged_threshold: 8` passed the whole suite).
#[cfg(test)]
mod patch_preset_tests {
    use super::*;

    /// The preset's values, pinned — including the two that are inert, so a
    /// future change to them is at least deliberate.
    #[test]
    fn for_patch_pass_carries_the_shipped_tuning() {
        let o = PatchOptions::for_patch_pass(false, None, None, None);
        assert_eq!(o.block_sectors, Some(32));
        assert!(o.full_recovery, "diagnostics-only, but pinned");
        assert!(o.reverse);
        assert_eq!(o.wedged_threshold, 50);
        assert!(!o.decrypt, "decrypt is the caller's, forwarded verbatim");

        // And `decrypt` really is forwarded, not hard-coded.
        assert!(PatchOptions::for_patch_pass(true, None, None, None).decrypt);
    }

    /// The behaviour `block_sectors` + `reverse` still have: the pass label the
    /// operator sees on every progress tick. With the shipped preset that is a
    /// REVERSE TRIM pass; `Some(1)` would relabel the same pass as a scrape.
    #[test]
    fn the_preset_reports_a_reverse_trim_pass() {
        use libfreemkv::progress::PassKind;
        let o = PatchOptions::for_patch_pass(false, None, None, None);

        let kind = patch::pass_kind(patch::initial_batch_of(&o), o.reverse);
        assert!(
            matches!(kind, PassKind::Trim { reverse: true }),
            "the shipped preset must render as a reverse TRIM pass, got {kind:?}"
        );

        // The contrast, so the assertion above is not just "whatever it does":
        // a single-sector batch is a SCRAPE pass, and `reverse` really is the
        // flag that decorates it.
        let mut scrape = PatchOptions::for_patch_pass(false, None, None, None);
        scrape.block_sectors = Some(1);
        scrape.reverse = false;
        assert!(matches!(
            patch::pass_kind(patch::initial_batch_of(&scrape), scrape.reverse),
            PassKind::Scrape { reverse: false }
        ));

        // `Some(0)` must not underflow into the scrape label by accident.
        let mut zero = PatchOptions::for_patch_pass(false, None, None, None);
        zero.block_sectors = Some(0);
        assert_eq!(
            patch::initial_batch_of(&zero),
            1,
            "clamped to a valid batch"
        );
    }

    /// The behaviour `wedged_threshold` has: it is REPORTED, verbatim, in the
    /// outcome the caller renders — and it does not, by itself, make the pass
    /// look wedged. Both halves matter: a caller that printed a wedge warning
    /// off this field alone would be wrong, and a caller that never saw the
    /// number could not explain the wedge exit when it does happen.
    #[test]
    fn the_wedged_threshold_is_reported_not_enforced() {
        let o = PatchOptions::for_patch_pass(false, None, None, None);

        let state = patch::PatchLoopState::new(0, patch::initial_batch_of(&o), 4096);
        let summary = patch::PatchSummary {
            stats: mapfile::MapStats::default(),
        };
        let outcome = patch::build_outcome(
            &state,
            &summary,
            std::path::Path::new("/nonexistent/for-outcome-only"),
            4096,
            0,
            o.wedged_threshold,
        );
        assert_eq!(
            outcome.wedged_threshold, 50,
            "the preset's threshold must reach the caller's outcome verbatim"
        );
        assert!(
            !outcome.wedged_exit,
            "the threshold alone must not mark a pass wedged — `wedged_exit` \
             comes from a handler's TransportFault, nothing counts against 50"
        );
    }
}
