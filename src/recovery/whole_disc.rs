//! The whole-disc (sweep / patch) decrypting reader. AACS content is every content
//! file (`/BDMV/STREAM`, HD DVD `/HVDVD_TS/*.EVO`), not just the kept titles. Each file
//! no title plays is keyed on its own: a key proven on its ciphertext is used; ciphertext
//! no held key opens refuses the rip up front; a file with no readable encrypted probe is
//! keyed only on a provably single-CPS disc, else left unkeyed so an encrypted unit there
//! fails loud mid-pass (never ships as ciphertext). Reads follow each file's own 3-sector
//! unit grid ([`UnitAligned`]).

use libfreemkv::decrypt::{AacsKeyMap, DecryptKeys, Phase};
use libfreemkv::error::{Error, Result};
use libfreemkv::sector::{DecryptingSectorSource, SectorSource};

/// Sectors in one AACS aligned unit (6144 bytes).
const UNIT: u64 = (libfreemkv::aacs::content::ALIGNED_UNIT_LEN / 2048) as u64;

/// The reader sweep and patch read through.
pub(crate) type WholeDiscReader<'r> = UnitAligned<DecryptingSectorSource<&'r mut dyn SectorSource>>;

/// Whole-disc reader: `decrypt` installs the disc's keys; for AACS the key map covers every
/// content file it can prove (see the module doc) and reads follow each file's unit grid.
/// `--raw` / CSS / clear discs pass straight through.
pub(crate) fn whole_disc_decrypting_reader<'r>(
    disc: &libfreemkv::Disc,
    reader: &'r mut dyn SectorSource,
    decrypt: bool,
    halt: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    key_fetch: Option<&libfreemkv::sector::KeyFetch>,
) -> Result<WholeDiscReader<'r>> {
    let mut keys = if decrypt {
        disc.decrypt_keys()
    } else {
        DecryptKeys::None
    };
    let mut content = disc.encrypted_content_ranges();
    let mut spans = Vec::new();
    let mut key_map = None;
    if matches!(keys, DecryptKeys::Aacs { .. }) {
        let halt = halt.cloned().map(libfreemkv::halt::Halt::from_arc);
        let map = disc.resolve_content_key_map(reader, &mut keys, key_fetch, halt.as_ref())?;
        // Read the file map BEFORE wrapping `reader`. freemkv#55: the key map alone is
        // no gate — clear UDF/nav outside it would trip the orphan refusal.
        let files = content_files(reader)?;
        if files.is_empty() && !content.is_empty() {
            tracing::warn!(target: "freemkv::scan", "titles but no AACS content files");
            return Err(Error::DecryptFailed);
        }
        spans = unit_spans(&files, &content);
        let ctx = KeyCtx {
            disc,
            fetch: key_fetch,
            halt: halt.as_ref(),
        };
        key_map = Some(std::sync::Arc::new(key_content_files(
            reader, &ctx, &mut keys, map, &files, &spans,
        )?));
        content.extend(files.iter().flatten());
        content = merge_ranges(content);
    }
    let mut dec = DecryptingSectorSource::new(reader, keys);
    if let Some(map) = key_map {
        dec = dec.with_key_map(map);
    }
    // CSS self-descrambles and `None` decrypts nothing: the gate only matters for AACS.
    if decrypt && !content.is_empty() {
        dec = dec.with_content_ranges(std::sync::Arc::from(content));
    }
    Ok(UnitAligned::new(dec, spans))
}

// Every AACS content file's extents, one entry per file (contiguous extents joined):
// `/BDMV/STREAM` (m2ts before SSIF, which re-lists them) or else HD DVD `/HVDVD_TS/*.EVO`.
// An unreadable UDF or unmappable file fails loud: it would otherwise ship as ciphertext.
fn content_files(reader: &mut dyn SectorSource) -> Result<Vec<Vec<(u32, u32)>>> {
    let fs = libfreemkv::read_filesystem(reader)?;
    let (top, evo_only) = if fs.find_dir("/BDMV/STREAM").is_some() {
        ("/BDMV/STREAM", false)
    } else if fs.find_dir("/HVDVD_TS").is_some() {
        ("/HVDVD_TS", true)
    } else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    let mut stack: Vec<_> = fs
        .find_dir(top)
        .map(|d| (top.to_string(), d))
        .into_iter()
        .collect();
    while let Some((dir, entry)) = stack.pop() {
        for e in &entry.entries {
            let path = format!("{dir}/{}", e.name);
            if e.is_dir {
                stack.push((path, e));
            } else if !evo_only || e.name.to_ascii_uppercase().ends_with(".EVO") {
                paths.push(path);
            }
        }
    }
    paths.sort_by_key(|p| (p.to_ascii_uppercase().contains("/SSIF/"), p.clone()));
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let mut extents: Vec<(u32, u32)> = Vec::new();
        for (lba, n) in fs.file_extents(reader, &path)? {
            match extents.last_mut() {
                Some(last) if n > 0 && last.0 as u64 + last.1 as u64 == lba as u64 => last.1 += n,
                _ if n > 0 => extents.push((lba, n)),
                _ => {}
            }
        }
        if !extents.is_empty() {
            files.push(extents);
        }
    }
    Ok(files)
}

/// A content extent `(start, count)` and the LBA its unit grid is anchored at: the
/// owning file's first sector, carried across extents by file offset.
pub(crate) type UnitSpan = (u32, u32, u64);

// The unit grid of every content extent: per file by file offset (never a merged run:
// adjacent files each start their own grid), then title extents no file covers (anchored
// at the extent start). Sorted, disjoint; on overlap the earlier-listed source wins.
fn unit_spans(files: &[Vec<(u32, u32)>], title_extents: &[(u32, u32)]) -> Vec<UnitSpan> {
    let mut raw: Vec<(UnitSpan, usize)> = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let mut off = 0u64;
        for &(lba, n) in file {
            raw.push(((lba, n, (lba as u64).saturating_sub(off % UNIT)), i));
            off += n as u64;
        }
    }
    for &(lba, n) in title_extents {
        raw.push(((lba, n, lba as u64), files.len()));
    }
    raw.retain(|&((_, n, _), _)| n > 0);
    raw.sort_by_key(|&((lba, _, _), i)| (lba, i));
    let mut spans: Vec<UnitSpan> = Vec::with_capacity(raw.len());
    let mut covered = 0u64;
    for ((lba, n, anchor), _) in raw {
        let end = lba as u64 + n as u64;
        if end <= covered {
            continue;
        }
        let start = (lba as u64).max(covered);
        spans.push((start as u32, (end - start) as u32, anchor));
        covered = end;
    }
    spans
}

// The span holding `lba`, if any.
fn span_at(spans: &[UnitSpan], lba: u64) -> Option<UnitSpan> {
    let i = spans
        .partition_point(|&(s, _, _)| s as u64 <= lba)
        .checked_sub(1)?;
    let span = spans[i];
    (lba < span.0 as u64 + span.1 as u64).then_some(span)
}

// Shared inputs for keying content files.
struct KeyCtx<'a> {
    disc: &'a libfreemkv::Disc,
    fetch: Option<&'a libfreemkv::sector::KeyFetch>,
    halt: Option<&'a libfreemkv::halt::Halt>,
}

// Extend the title `map` over every content file no kept title plays, ONE FILE AT A TIME
// (files of different CPS units sit back to back): its own resolved key, kept only when it
// opens the file's ciphertext. Ciphertext no held key opens refuses the rip now.
fn key_content_files(
    reader: &mut dyn SectorSource,
    ctx: &KeyCtx,
    keys: &mut DecryptKeys,
    map: AacsKeyMap,
    files: &[Vec<(u32, u32)>],
    spans: &[UnitSpan],
) -> Result<AacsKeyMap> {
    let mut keyed: Vec<(u32, u32)> = map.ranges().iter().map(|&(s, e, _, _)| (s, e)).collect();
    let single = single_cps_key_slot(ctx.disc, keys, &map);
    let mut ranges = map.ranges().to_vec();
    for file in files {
        let orphans = subtract_ranges(file, &keyed);
        if orphans.is_empty() {
            continue;
        }
        let mut title = libfreemkv::DiscTitle::empty();
        title.content_format = ctx.disc.content_format;
        title.extents = orphans
            .iter()
            .map(|&(s, e)| libfreemkv::Extent {
                start_lba: s,
                sector_count: e - s,
            })
            .collect();
        let fmap = libfreemkv::resolve_mux_key_map(
            reader,
            &title,
            keys,
            ctx.fetch,
            ctx.disc.content_format,
            ctx.halt,
        )?;
        for &(s, e) in &orphans {
            let anchor = span_at(spans, s as u64).map_or(s as u64, |sp| sp.2);
            if key_proven(reader, &fmap, keys, ctx.disc.content_format, (s, e), anchor)? {
                ranges.extend(fmap.ranges().iter().filter_map(|&(rs, re, i, p)| {
                    (rs < e && re > s).then_some((rs.max(s), re.min(e), i, p))
                }));
            } else if let Some(slot) = single {
                ranges.push((s, e, slot, Phase::All));
            } else {
                // No encrypted sample to prove against: left unkeyed, so an encrypted
                // unit here still fails loud (orphan refusal) — never passes as clear.
                tracing::warn!(target: "freemkv::scan", lba = s, "content file unprovable");
            }
        }
        // Settled either way: a file re-listing these sectors (SSIF) is not re-keyed.
        keyed.extend(orphans);
        keyed.sort_unstable();
    }
    Ok(AacsKeyMap::from_ranges_phased(merge_key_ranges(ranges)))
}

// Sorted, disjoint key ranges (`AacsKeyMap::entry_for` checks one neighbour), merged as
// libfreemkv's per-disc map is: same key + phase overlap unions; a conflicting overlap drops.
fn merge_key_ranges(mut ranges: Vec<(u32, u32, usize, Phase)>) -> Vec<(u32, u32, usize, Phase)> {
    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(u32, u32, usize, Phase)> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.0 < last.1 => {
                if r.2 == last.2 && r.3 == last.3 {
                    last.1 = last.1.max(r.1);
                }
            }
            _ => merged.push(r),
        }
    }
    merged
}

// Does `map`'s key for `[start, end)` open a real encrypted unit on the file's grid
// (`anchor`)? Ok(false): no encrypted unit sampled; Err(DecryptFailed): none opened.
fn key_proven(
    reader: &mut dyn SectorSource,
    map: &AacsKeyMap,
    keys: &DecryptKeys,
    format: libfreemkv::ContentFormat,
    (start, end): (u32, u32),
    anchor: u64,
) -> Result<bool> {
    use libfreemkv::aacs::content::{aacs_unit_encrypted, decrypt_unit, is_clean};
    const PROBES: u64 = 8;
    let DecryptKeys::Aacs { unit_keys, .. } = keys else {
        return Ok(false);
    };
    let head = start as u64 + (UNIT - (start as u64).saturating_sub(anchor) % UNIT) % UNIT;
    let units = (end as u64).saturating_sub(head) / UNIT;
    let mut seen = false;
    let mut buf = vec![0u8; libfreemkv::aacs::content::ALIGNED_UNIT_LEN];
    for p in 1..=PROBES {
        let unit = units * p / (PROBES + 1);
        let Ok(lba) = u32::try_from(head + unit * UNIT) else {
            continue;
        };
        if unit >= units
            || !matches!(reader.read_sectors(lba, UNIT as u16, &mut buf, false),
                Ok(n) if n == buf.len())
            || !aacs_unit_encrypted(&buf, format)
        {
            continue;
        }
        seen = true;
        match map.entry_for(lba) {
            Some((idx, Phase::All, _)) => {
                if let Some((_, key)) = unit_keys.get(idx) {
                    decrypt_unit(&mut buf, key);
                    if is_clean(&buf, format) {
                        return Ok(true);
                    }
                }
            }
            // Forensic segment: libfreemkv verifies every such unit as it decrypts.
            Some(_) => return Ok(true),
            None => {}
        }
    }
    if seen {
        tracing::warn!(target: "freemkv::scan", lba = start, "no held key opens content file");
        return Err(Error::DecryptFailed);
    }
    Ok(false)
}

// The key-pool slot of the disc's only CPS unit, when provably single-CPS: Unit_Key_RO.inf
// declares exactly one unit, the disc is not FMTS, and the map uses at most one key.
fn single_cps_key_slot(
    disc: &libfreemkv::Disc,
    keys: &DecryptKeys,
    map: &AacsKeyMap,
) -> Option<usize> {
    use libfreemkv::aacs::mkb::AacsVersion;
    let aacs = disc.aacs.as_ref()?;
    if disc.format == libfreemkv::DiscFormat::Fmts || map.ranges().iter().any(|r| r.3 != Phase::All)
    {
        return None;
    }
    let version = if aacs.version >= 2 {
        AacsVersion::V20
    } else {
        AacsVersion::V10
    };
    let ukf = libfreemkv::aacs::inf::parse_unit_key_ro(&aacs.uk_ro, version)?;
    if ukf.encrypted_keys.len() != 1 {
        return None;
    }
    match (map.key_indices(), keys) {
        ([only], _) => Some(*only),
        ([], DecryptKeys::Aacs { unit_keys, .. }) if unit_keys.len() == 1 => Some(0),
        _ => None,
    }
}

// `content` `(start, count)` ranges minus the sorted, disjoint `[start, end)` `keyed`
// ranges, as `[start, end)` pieces.
fn subtract_ranges(content: &[(u32, u32)], keyed: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for &(start, count) in content {
        let end = start.saturating_add(count);
        let mut pos = start;
        for &(ks, ke) in keyed {
            if ke <= pos || ks >= end {
                continue;
            }
            if ks > pos {
                out.push((pos, ks));
            }
            pos = pos.max(ke);
        }
        if pos < end {
            out.push((pos, end));
        }
    }
    out
}

// Sort + coalesce overlapping/adjacent `(start, count)` ranges (the content gate's
// binary search needs them sorted and disjoint); empty ranges are dropped.
fn merge_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.retain(|&(_, count)| count > 0);
    ranges.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (start, count) in ranges {
        let end = start as u64 + count as u64;
        if let Some(last) = out.last_mut() {
            let last_end = last.0 as u64 + last.1 as u64;
            if start as u64 <= last_end {
                let merged = last_end.max(end) - last.0 as u64;
                last.1 = u32::try_from(merged).unwrap_or(u32::MAX);
                continue;
            }
        }
        out.push((start, count));
    }
    out
}

/// Reads through `inner` on each content file's AACS unit grid: a read touching a span
/// is widened to whole units of that span (sweep batches and patch's single-sector reads
/// are not unit multiples). A unit whose head lies outside its span fails loud.
pub(crate) struct UnitAligned<S> {
    inner: S,
    spans: Vec<UnitSpan>,
    scratch: Vec<u8>,
}

impl<S: SectorSource> UnitAligned<S> {
    fn new(inner: S, spans: Vec<UnitSpan>) -> Self {
        Self {
            inner,
            spans,
            scratch: Vec::new(),
        }
    }

    /// Where a block of sectors `[start, end)` should end so consecutive blocks tile each
    /// file's unit grid: `end` pulled back to its unit's head when that unit straddles it
    /// (else a bad sector there fails both neighbouring blocks). Never at or before `start`.
    pub(crate) fn unit_block_end(&self, start: u64, end: u64) -> u64 {
        match span_at(&self.spans, end) {
            Some((s, _, anchor)) if end > anchor => {
                let head = anchor + (end - anchor) / UNIT * UNIT;
                if head > start && head >= s as u64 {
                    head
                } else {
                    end
                }
            }
            _ => end,
        }
    }
}

impl<S: SectorSource> SectorSource for UnitAligned<S> {
    fn capacity_sectors(&self) -> u32 {
        self.inner.capacity_sectors()
    }

    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
    ) -> Result<usize> {
        self.read_sectors_fua(lba, count, buf, recovery, false)
    }

    fn read_sectors_fua(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        recovery: bool,
        fua: bool,
    ) -> Result<usize> {
        const SECTOR: usize = 2048;
        let end = lba as u64 + count as u64;
        let mut cur = lba as u64;
        while cur < end {
            let off = (cur - lba as u64) as usize * SECTOR;
            let Some((s, n, anchor)) = span_at(&self.spans, cur) else {
                // Outside every content span: a plain read up to the next span.
                let i = self.spans.partition_point(|&(s, _, _)| s as u64 <= cur);
                let next = self
                    .spans
                    .get(i)
                    .map_or(end, |&(s, _, _)| end.min(s as u64));
                let want = (next - cur) as usize * SECTOR;
                self.inner.set_unit_base(cur as u32);
                let got = self.inner.read_sectors_fua(
                    cur as u32,
                    (next - cur) as u16,
                    &mut buf[off..off + want],
                    recovery,
                    fua,
                )?;
                if got < want {
                    return Ok(off + got);
                }
                cur = next;
                continue;
            };
            let (s, e) = (s as u64, s as u64 + n as u64);
            let a0 = anchor + (cur - anchor) / UNIT * UNIT;
            if a0 < s {
                // The unit straddles a non-contiguous extent boundary: undecryptable here.
                return Err(Error::DecryptFailed);
            }
            // Capped so the widened read still fits one u16-count request.
            let piece_end = end.min(e).min(a0 + (u16::MAX as u64 / UNIT - 1) * UNIT);
            let a1 = e.min(a0 + (piece_end - a0).div_ceil(UNIT) * UNIT);
            let len = (a1 - a0) as usize * SECTOR;
            self.scratch.resize(len, 0);
            self.inner.set_unit_base(a0 as u32);
            let got = self.inner.read_sectors_fua(
                a0 as u32,
                (a1 - a0) as u16,
                &mut self.scratch[..len],
                recovery,
                fua,
            )?;
            let skip = (cur - a0) as usize * SECTOR;
            let want = (piece_end - cur) as usize * SECTOR;
            let have = got.saturating_sub(skip).min(want);
            buf[off..off + have].copy_from_slice(&self.scratch[skip..skip + have]);
            if have < want {
                return Ok(off + have);
            }
            cur = piece_end;
        }
        Ok(count as usize * SECTOR)
    }

    fn set_speed(&mut self, kbs: u16) {
        self.inner.set_speed(kbs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_leaves_only_the_unkeyed_pieces() {
        // content [10,40) and [50,60); keyed [0,12), [20,25), [38,55).
        let got = subtract_ranges(&[(10, 30), (50, 10)], &[(0, 12), (20, 25), (38, 55)]);
        assert_eq!(got, vec![(12, 20), (25, 38), (55, 60)]);
        assert_eq!(subtract_ranges(&[(10, 5)], &[(10, 15)]), Vec::new());
        assert_eq!(subtract_ranges(&[(10, 5)], &[]), vec![(10, 15)]);
    }

    #[test]
    fn merge_sorts_coalesces_overlap_and_adjacency_and_drops_empties() {
        let got = merge_ranges(vec![(50, 10), (10, 5), (12, 10), (22, 3), (40, 0), (30, 1)]);
        assert_eq!(got, vec![(10, 15), (30, 1), (50, 10)]);
        assert_eq!(merge_ranges(vec![(0, 100), (10, 5)]), vec![(0, 100)]);
        let got = merge_ranges(vec![(0, u32::MAX), (u32::MAX - 1, u32::MAX)]);
        assert_eq!(got, vec![(0, u32::MAX)]);
    }

    /// Each file anchors its own grid at its first sector; a >1 GiB file's later
    /// extents keep the grid by FILE OFFSET, not by their own start LBA.
    #[test]
    fn unit_spans_follow_each_file_by_offset_across_extents() {
        // File A: 4 sectors at 100, then 5 at 300 (non-contiguous: by offset 4 the
        // second extent's grid is anchored at 299). File B, adjacent at 104: own grid.
        let files = vec![vec![(100, 4), (300, 5)], vec![(104, 7)]];
        let spans = unit_spans(&files, &[]);
        assert_eq!(spans, vec![(100, 4, 100), (104, 7, 104), (300, 5, 299)]);
        // No files: per title extent.
        assert_eq!(unit_spans(&[], &[(7, 9)]), vec![(7, 9, 7)]);
        // SSIF re-lists m2ts extents (same LBAs): the m2ts, listed first, keeps its grid.
        let spans = unit_spans(&[vec![(10, 7)], vec![(10, 7), (40, 3)]], &[]);
        assert_eq!(spans, vec![(10, 7, 10), (40, 3, 39)]);
    }

    /// Synthetic drive: each sector filled with its LBA's low byte (so any LBA is
    /// readable without an image); records reads, unit bases and FUA flags.
    #[derive(Default)]
    struct Mem {
        reads: Vec<(u32, u16)>,
        bases: Vec<u32>,
        fua: Vec<bool>,
        /// Deliver at most this many bytes per read (a short transfer).
        short: Option<usize>,
    }
    impl SectorSource for Mem {
        fn capacity_sectors(&self) -> u32 {
            u32::MAX
        }
        fn read_sectors(&mut self, lba: u32, count: u16, buf: &mut [u8], r: bool) -> Result<usize> {
            self.read_sectors_fua(lba, count, buf, r, false)
        }
        fn read_sectors_fua(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _: bool,
            fua: bool,
        ) -> Result<usize> {
            self.reads.push((lba, count));
            self.fua.push(fua);
            let n = self.short.unwrap_or(usize::MAX).min(count as usize * 2048);
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = (lba as usize + i / 2048) as u8;
            }
            Ok(n)
        }
        fn set_unit_base(&mut self, lba: u32) {
            self.bases.push(lba);
        }
    }

    fn reader(spans: Vec<UnitSpan>) -> UnitAligned<Mem> {
        UnitAligned::new(Mem::default(), spans)
    }

    /// A read longer than one u16-count request (after widening) is split at a
    /// unit boundary (65532) and still hands back exactly the requested sectors.
    #[test]
    fn unit_aligned_splits_a_max_length_read_on_the_grid() {
        let mut r = reader(vec![(0, 70_000, 0)]);
        let mut buf = vec![0u8; u16::MAX as usize * 2048];
        assert_eq!(
            r.read_sectors(1, u16::MAX, &mut buf, false).unwrap(),
            buf.len()
        );
        assert_eq!(r.inner.reads, vec![(0, 65_532), (65_532, 6)]);
        assert!(
            buf.chunks(2048)
                .enumerate()
                .all(|(i, c)| c[0] == (1 + i) as u8)
        );
    }

    /// A short inner transfer is reported short (never padded with stale bytes).
    #[test]
    fn unit_aligned_reports_a_short_inner_transfer() {
        let mut r = reader(vec![(100, 30, 100)]);
        r.inner.short = Some(2048);
        let mut buf = vec![0u8; 3 * 2048];
        assert_eq!(r.read_sectors(100, 3, &mut buf, false).unwrap(), 2048);
        r.inner.short = Some(0);
        assert_eq!(r.read_sectors(98, 2, &mut buf, false).unwrap(), 0);
    }

    /// FUA reaches the drive on span and plain reads; plain reads reset the unit
    /// base to their own start (no stale base from the previous span read).
    #[test]
    fn unit_aligned_passes_fua_and_bases_plain_reads_at_their_start() {
        let mut r = reader(vec![(100, 30, 100)]);
        let mut buf = vec![0u8; 5 * 2048];
        r.read_sectors_fua(98, 5, &mut buf, true, true).unwrap();
        assert_eq!(r.inner.fua, vec![true, true]);
        assert_eq!(r.inner.bases, vec![98, 100]);
    }

    /// A title extent outside every file still gets its own grid (anchored at
    /// the extent start); one inside a file defers to the file's grid.
    #[test]
    fn unit_spans_fold_title_extents_outside_every_file() {
        let spans = unit_spans(&[vec![(100, 6)]], &[(100, 6), (200, 4)]);
        assert_eq!(spans, vec![(100, 6, 100), (200, 4, 200)]);
    }

    // A single-CPS AACS 1.0 BD: `Unit_Key_RO.inf` declares one CPS unit.
    fn aacs_disc() -> libfreemkv::Disc {
        let mut uk_ro = vec![0u8; 144];
        uk_ro[3] = 48; // key storage at 48
        uk_ro[49] = 1; // one unit key
        libfreemkv::Disc {
            volume_id: String::new(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1000,
            capacity_bytes: 1000 * 2048,
            layers: 1,
            titles: Vec::new(),
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: Some(libfreemkv::disc::AacsState {
                version: 1,
                bus_encryption: false,
                mkb_version: None,
                disc_hash: String::new(),
                key_source: libfreemkv::disc::KeyOrigin::DeviceKey,
                vuk: None,
                unit_keys: vec![(1, [0x5A; 16])],
                volume_id: [0; 16],
                uk_ro,
                mkb: Vec::new(),
            }),
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    /// SSIF re-lists an m2ts's extents: that file is resolved/probed once and the
    /// map stays sorted and disjoint (`AacsKeyMap::entry_for` relies on it).
    #[test]
    fn a_relisted_file_is_keyed_once_into_a_disjoint_map() {
        let disc = aacs_disc();
        let ctx = KeyCtx {
            disc: &disc,
            fetch: None,
            halt: None,
        };
        let mut keys = DecryptKeys::Aacs {
            unit_keys: vec![(1, [0x5A; 16])],
            format: libfreemkv::ContentFormat::BdTs,
        };
        let files = vec![vec![(300, 6)], vec![(300, 6)]];
        let spans = unit_spans(&files, &[]);
        let mut mem = Mem::default();
        let map = key_content_files(
            &mut mem,
            &ctx,
            &mut keys,
            AacsKeyMap::from_ranges(Vec::new()),
            &files,
            &spans,
        )
        .unwrap();
        let r = map.ranges();
        assert!(r.windows(2).all(|w| w[0].1 <= w[1].0), "disjoint: {r:?}");
        assert_eq!(
            r.len(),
            1,
            "one keyed range for the one physical file: {r:?}"
        );
        let probes = mem.reads.iter().filter(|&&(l, _)| l >= 300).count();
        assert!(
            probes <= 8,
            "the re-listed file must not be probed twice: {probes}"
        );
    }

    /// Reads are widened onto the span's grid and the caller gets exactly its sectors.
    #[test]
    fn unit_aligned_widens_reads_onto_each_files_grid() {
        let mut r = reader(vec![(100, 30, 100), (300, 9, 299)]);
        let mut buf = vec![0u8; 5 * 2048];
        assert_eq!(r.read_sectors(98, 5, &mut buf, false).unwrap(), 5 * 2048);
        let got: Vec<u8> = buf.chunks(2048).map(|c| c[0]).collect();
        assert_eq!(got, vec![98, 99, 100, 101, 102]);
        // 98..100 plain, then the span piece widened to the unit [100, 103).
        assert_eq!(r.inner.reads, vec![(98, 2), (100, 3)]);
        r.inner.reads.clear();
        let mut one = vec![0u8; 2048];
        r.read_sectors(305, 1, &mut one, false).unwrap();
        assert_eq!(one[0], (305u32 % 256) as u8);
        // Grid anchored at 299: the unit holding 305 is [305, 308).
        assert_eq!(r.inner.reads, vec![(305, 3)]);
    }

    /// A unit that straddles a non-contiguous extent boundary fails loud.
    #[test]
    fn unit_aligned_refuses_a_unit_split_across_extents() {
        let mut r = reader(vec![(100, 4, 100), (300, 5, 299)]);
        let mut buf = vec![0u8; 2048];
        assert!(matches!(
            r.read_sectors(300, 1, &mut buf, false),
            Err(Error::DecryptFailed)
        ));
    }
}
