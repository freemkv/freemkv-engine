//! Test fixtures for the keys-up-front (KU) engine tests: a scannable AACS-encrypted BD
//! image (one playlist per stream file) and counting fake key sources.
//!
//! The MPLS/CLPI builders follow libfreemkv's crate-private test builders. Each clip is
//! BD LPCM audio in real TS/PES, so a title muxes to a verifiable MKV.

use libfreemkv::aacs::mkb::AacsVersion;
use libfreemkv::aacs::types::UnitKey;
use libfreemkv::keysource::ResolveCtx;
use libfreemkv::test_util::{BdFile, EncryptedBdImage, MemSource, encrypted_bd_image};
use libfreemkv::{Disc, KeySource, KeySourceFactory, SectorSource};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const K1: [u8; 16] = *b"\xA1KU-E1 key one!!";
pub(crate) const K2: [u8; 16] = *b"\xB2KU-E1 key two!!";
pub(crate) const VID: [u8; 16] = *b"\x5AKU-E1 volumeID!";

/// Units per clip: at least `MIN_SAMPLE_UNITS` (8) encrypted units, so a
/// sample-dependent source is asked with this piece alone (KU §2.3 step 9.2).
pub(crate) const CLIP_UNITS: u32 = 10;

/// The clips' one elementary stream: BD LPCM stereo 48 kHz 16-bit on this PID.
pub(crate) const AUDIO_PID: u16 = 0x1100;
/// Each title's running time; its clip's audio spans it.
pub(crate) const TITLE_SECS: u32 = 120;

// A one-PlayItem MPLS on `clip` whose STN lists the LPCM stream (KS-10 aside, one CPS
// unit per clip here). In/out times in 45 kHz ticks from 1 s.
fn one_item_mpls(clip: &[u8; 5]) -> Vec<u8> {
    let mut item = clip.to_vec();
    item.extend_from_slice(b"M2TS");
    item.extend_from_slice(&[0u8; 3]);
    item.extend_from_slice(&45_000u32.to_be_bytes());
    item.extend_from_slice(&(45_000u32 * (1 + TITLE_SECS)).to_be_bytes());
    item.extend_from_slice(&[0u8; 12]);
    let mut stn = vec![0u8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    stn.extend_from_slice(&[3, 0x01]);
    stn.extend_from_slice(&AUDIO_PID.to_be_bytes());
    stn.extend_from_slice(&[5, 0x80, 0x31]);
    stn.extend_from_slice(b"eng");
    item.extend_from_slice(&stn);
    let mut pl = vec![0u8; 6];
    pl.extend_from_slice(&1u16.to_be_bytes());
    pl.extend_from_slice(&[0u8; 2]);
    pl.extend_from_slice(&(item.len() as u16).to_be_bytes());
    pl.extend_from_slice(&item);
    let pl_len = (pl.len() - 4) as u32;
    pl[0..4].copy_from_slice(&pl_len.to_be_bytes());
    let mut buf = b"MPLS0200".to_vec();
    buf.extend_from_slice(&40u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 28]);
    buf.extend_from_slice(&pl);
    buf
}

// Source packet `k` of a clip: a TP_extra_header (CPI 11₂ when `encrypted`, KS-5) and one
// TS packet holding one whole LPCM PES (private_stream_1), PTS spread over the title.
fn lpcm_source_packet(k: u32, n: u32, encrypted: bool) -> [u8; 192] {
    const AUDIO: usize = 160;
    let mut p = [0u8; 192];
    p[..4].copy_from_slice(&(k * 100).to_be_bytes());
    p[0] = if encrypted { p[0] | 0xC0 } else { p[0] & 0x3F };
    let ts = &mut p[4..];
    ts[..4].copy_from_slice(&[0x47, 0x40 | (AUDIO_PID >> 8) as u8, AUDIO_PID as u8, 0x30]);
    ts[3] |= (k & 0x0F) as u8;
    let stuffing = 188 - 4 - 1 - (14 + 4 + AUDIO);
    ts[4] = stuffing as u8;
    ts[5] = 0x00;
    ts[6..5 + stuffing].fill(0xFF);
    let pts = 90_000u64 + u64::from(k) * 90_000 * u64::from(TITLE_SECS) / u64::from(n);
    let pes = &mut ts[5 + stuffing..];
    pes[..4].copy_from_slice(&[0, 0, 1, 0xBD]);
    pes[4..6].copy_from_slice(&((8 + 4 + AUDIO) as u16).to_be_bytes());
    pes[6..9].copy_from_slice(&[0x81, 0x80, 5]);
    pes[9] = 0x21 | ((pts >> 29) & 0x0E) as u8;
    pes[10..12].copy_from_slice(&((((pts >> 14) & 0xFFFE) | 1) as u16).to_be_bytes());
    pes[12..14].copy_from_slice(&((((pts << 1) & 0xFFFE) | 1) as u16).to_be_bytes());
    pes[14..16].copy_from_slice(&(AUDIO as u16).to_be_bytes());
    pes[16] = 0x31;
    pes[17] = 0x40;
    for (i, b) in pes[18..18 + AUDIO].iter_mut().enumerate() {
        *b = (k as usize * 7 + i) as u8;
    }
    p
}

// A CLPI with what `clpi::parse` needs: magic and the source packet count at 56.
fn minimal_clpi(source_packets: u32) -> Vec<u8> {
    let mut d = vec![0u8; 60];
    d[0..4].copy_from_slice(b"HDMV");
    d[4..8].copy_from_slice(b"0200");
    d[56..60].copy_from_slice(&source_packets.to_be_bytes());
    d
}

/// A scanned fixture: the image a drive would serve, the disc its scan found, and the
/// sectors holding its playlists and `Unit_Key_RO.inf`.
pub(crate) struct Fx {
    pub img: EncryptedBdImage,
    pub disc: Disc,
    /// `(start, sectors)` of every MPLS and of `/AACS/Unit_Key_RO.inf`.
    pub metadata: Vec<(u32, u32)>,
}

impl Fx {
    pub(crate) fn source(&self) -> MemSource {
        MemSource::new(self.img.image.clone())
    }

    /// Write the image to `dir/name` and return its path.
    pub(crate) fn write(&self, dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, &self.img.image).unwrap();
        p
    }

    /// `(start, sectors)` of stream file `i` (clip `i`).
    pub(crate) fn clip(&self, i: usize) -> (u32, u32) {
        self.img.files[self.img.files.len() - self.disc.titles.len() + i]
    }
}

/// A BD image with one title per entry of `clips` (each `CLIP_UNITS` units encrypted with
/// its key, `None` = clear), declaring `declared` CPS units. Scanned like a real image.
pub(crate) fn bd_image(clips: &[Option<[u8; 16]>], declared: usize) -> Fx {
    let uk_ro = libfreemkv::test_util::unit_key_ro(
        AacsVersion::V10,
        &vec![[0xEE; 16]; declared],
        &vec![1u16; clips.len()],
    );
    let n = clips.len();
    let mut files = vec![BdFile::new("BDMV/index.bdmv", 1, None)];
    for i in 0..n {
        files.push(BdFile::new(format!("BDMV/PLAYLIST/{i:05}.mpls"), 1, None));
    }
    for i in 0..n {
        files.push(BdFile::new(format!("BDMV/CLIPINF/{i:05}.clpi"), 1, None));
    }
    for (i, key) in clips.iter().enumerate() {
        files.push(BdFile::new(
            format!("BDMV/STREAM/{i:05}.m2ts"),
            CLIP_UNITS * 3,
            *key,
        ));
    }
    let mut img = encrypted_bd_image(&files, &uk_ro);
    let n_packets = CLIP_UNITS * 32;
    for (i, key) in clips.iter().enumerate() {
        let (start, _) = img.files[1 + 2 * n + i];
        for u in 0..CLIP_UNITS {
            let mut unit: Vec<u8> = (0..32)
                .flat_map(|j| lpcm_source_packet(u * 32 + j, n_packets, key.is_some()))
                .collect();
            let at = (start + u * 3) as usize * 2048;
            img.plain[at..at + unit.len()].copy_from_slice(&unit);
            if let Some(k) = key {
                assert!(libfreemkv::aacs::content::encrypt_unit(&mut unit, k));
            }
            img.image[at..at + unit.len()].copy_from_slice(&unit);
        }
    }
    for i in 0..n {
        let clip = format!("{i:05}");
        let clip: [u8; 5] = clip.as_bytes().try_into().unwrap();
        for (f, bytes) in [
            (1 + i, one_item_mpls(&clip)),
            (1 + n + i, minimal_clpi(CLIP_UNITS * 32)),
        ] {
            let at = img.files[f].0 as usize * 2048;
            img.image[at..at + bytes.len()].copy_from_slice(&bytes);
        }
    }
    let mut src = MemSource::new(img.image.clone());
    let cap = src.capacity_sectors();
    let disc = Disc::scan_image(&mut src, cap, &libfreemkv::ScanOptions::default()).unwrap();
    assert_eq!(
        disc.titles.len(),
        n,
        "the fixture scans to one title per clip"
    );
    let mut metadata: Vec<(u32, u32)> = img.files[1..=n].to_vec();
    metadata.push(uk_ro_extent(&img));
    Fx {
        img,
        disc,
        metadata,
    }
}

// The one extent of `/AACS/Unit_Key_RO.inf` in the image.
fn uk_ro_extent(img: &EncryptedBdImage) -> (u32, u32) {
    let mut src = MemSource::new(img.image.clone());
    let fs = libfreemkv::read_filesystem(&mut src).unwrap();
    fs.file_extents(&mut src, "/AACS/Unit_Key_RO.inf").unwrap()[0]
}

/// One request a fake source answered.
#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub who: &'static str,
    pub vid: Option<[u8; 16]>,
    /// A forensic (FMTS index-key) request (KU §5.1).
    pub forensic: bool,
}

/// Every request the fakes of one factory answered, shared across its builds.
#[derive(Clone, Default)]
pub(crate) struct Calls(pub Arc<Mutex<Vec<Call>>>);

impl Calls {
    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    pub(crate) fn all(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
    pub(crate) fn forensic(&self) -> usize {
        self.all().iter().filter(|c| c.forensic).count()
    }
}

/// How a fake answers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// A keydb: every held key, asked once per resolve (sample-independent).
    Keydb,
    /// An online service: the held key that opens the samples, asked per piece.
    Online,
    /// An online service that can derive the key only with the disc's VID (KS-16).
    OnlineNeedsVid,
}

struct Fake {
    who: &'static str,
    answer: Answer,
    keys: Vec<[u8; 16]>,
    fmts: Vec<[u8; 16]>,
    calls: Calls,
}

// Whether `key` opens `unit`: every decrypted source packet has TS sync (KS-2).
fn opens(unit: &[u8], key: &[u8; 16]) -> bool {
    let mut u = unit.to_vec();
    libfreemkv::test_util::decrypt_unit(&mut u, key);
    u.chunks(192).all(|p| p[4] == 0x47)
}

impl KeySource for Fake {
    fn get_unit_keys(&self, ctx: &dyn ResolveCtx) -> libfreemkv::Result<Vec<UnitKey>> {
        let vid = ctx.vid().map(|v| v.0);
        self.calls.0.lock().unwrap().push(Call {
            who: self.who,
            vid,
            forensic: false,
        });
        let keys: Vec<[u8; 16]> = match self.answer {
            Answer::Keydb => self.keys.clone(),
            Answer::OnlineNeedsVid if vid.is_none() => Vec::new(),
            Answer::Online | Answer::OnlineNeedsVid => {
                let samples = ctx.samples(usize::MAX).unwrap_or_default();
                let first = samples.first().cloned().unwrap_or_default();
                self.keys
                    .iter()
                    .filter(|k| first.len() == 6144 && opens(&first, k))
                    .copied()
                    .collect()
            }
        };
        Ok(keys
            .into_iter()
            .enumerate()
            .map(|(i, k)| UnitKey::new(i as u32, k))
            .collect())
    }
    fn get_fmts_indexes(&self, ctx: &dyn ResolveCtx) -> libfreemkv::Result<Vec<UnitKey>> {
        let call = Call {
            who: self.who,
            vid: ctx.vid().map(|v| v.0),
            forensic: true,
        };
        self.calls.0.lock().unwrap().push(call);
        Ok(self
            .fmts
            .iter()
            .enumerate()
            .map(|(i, k)| UnitKey::new(i as u32, *k))
            .collect())
    }
    fn label(&self) -> &'static str {
        self.who
    }
    fn answer_depends_on_samples(&self) -> bool {
        self.answer != Answer::Keydb
    }
}

/// A factory building one fake per `(answer, keys)`, recording into `calls`. The
/// returned `Arc` also counts the factory's own holders (LK7: nothing may keep it).
pub(crate) fn factory(specs: &[(Answer, &[[u8; 16]])], calls: &Calls) -> KeySourceFactory {
    fmts_factory(specs, &[], calls)
}

/// [`factory`] whose sources also answer the forensic index keys `fmts` (KU §5.2).
pub(crate) fn fmts_factory(
    specs: &[(Answer, &[[u8; 16]])],
    fmts: &[[u8; 16]],
    calls: &Calls,
) -> KeySourceFactory {
    let specs: Vec<(Answer, Vec<[u8; 16]>)> = specs.iter().map(|(a, k)| (*a, k.to_vec())).collect();
    let (fmts, calls) = (fmts.to_vec(), calls.clone());
    Arc::new(move || {
        specs
            .iter()
            .map(|(answer, keys)| {
                Box::new(Fake {
                    who: if *answer == Answer::Keydb {
                        "keydb"
                    } else {
                        "online"
                    },
                    answer: *answer,
                    keys: keys.clone(),
                    fmts: fmts.clone(),
                    calls: calls.clone(),
                }) as Box<dyn KeySource>
            })
            .collect()
    })
}

/// Resolve `scope` over `fx` from `specs`, the way a rip's up-front resolve does.
pub(crate) fn resolve(
    fx: &Fx,
    scope: libfreemkv::keys::KeyScope,
    specs: &[(Answer, &[[u8; 16]])],
    calls: &Calls,
) -> libfreemkv::Result<libfreemkv::keys::ResolvedKeySet> {
    let f = factory(specs, calls);
    libfreemkv::keys::ResolvedKeySet::resolve(
        &fx.disc,
        &mut fx.source(),
        scope,
        &f,
        libfreemkv::keys::ResolveKeysOptions::default(),
    )
    .map(|r| r.keys)
}

/// What a fake drive refuses (media damage), switchable between phases of a test.
#[derive(Clone, Copy)]
pub(crate) enum Damage {
    None,
    /// Every read touching `[s, e)`.
    Range(u32, u32),
    /// Every read touching `[s, e)` except one wholly inside `[ok_s, ok_e)`.
    RangeExcept(u32, u32, u32, u32),
    /// Every read of 9+ sectors touching `[s, e)`: the on-arrival side reads (up to 32
    /// units), never a pass's own read of one widened unit (KU §2.4).
    LongReads(u32, u32),
}

/// A random-access fake drive over an image, with switchable [`Damage`].
#[derive(Clone)]
pub(crate) struct Drive {
    inner: MemSource,
    damage: Arc<Mutex<Damage>>,
}

impl Drive {
    pub(crate) fn new(image: &[u8]) -> Self {
        Drive {
            inner: MemSource::new(image.to_vec()),
            damage: Arc::new(Mutex::new(Damage::None)),
        }
    }
    pub(crate) fn set(&self, d: Damage) {
        *self.damage.lock().unwrap() = d;
    }
}

impl SectorSource for Drive {
    fn capacity_sectors(&self) -> u32 {
        self.inner.capacity_sectors()
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        r: bool,
    ) -> libfreemkv::Result<usize> {
        let end = lba + count as u32;
        let touches = |s: u32, e: u32| s < end && lba < e;
        let dead = match *self.damage.lock().unwrap() {
            Damage::None => false,
            Damage::Range(s, e) => touches(s, e),
            Damage::RangeExcept(s, e, os, oe) => touches(s, e) && !(os <= lba && end <= oe),
            Damage::LongReads(s, e) => count >= 9 && touches(s, e),
        };
        if dead {
            return Err(libfreemkv::Error::DiscRead {
                sector: lba as u64,
                status: None,
                sense: None,
            });
        }
        self.inner.read_sectors(lba, count, buf, r)
    }
}

pub(crate) const F1: [u8; 16] = *b"\xE5KU-E1 forensic1";
pub(crate) const F2: [u8; 16] = *b"\xF6KU-E1 forensic2";
const ALT: [u8; 16] = *b"\x07KU-E1 alternate";

/// An AACS 2.1 FMTS disc (KS-25, KS-26: evidence, no public spec), after libfreemkv's
/// KU-L2 fixture: clip 0 (K1, 10 units), forensic clip 1 (base K2, 60 units) with an
/// index-1 segment over units 0..16 and an index-2 one over 20..36. Our phase is Even
/// (F1 / F2); odd segment units are the alternate variant, except that with `verify` index
/// 2's odd units also open with F2, so only `Phase::Verify` decrypts them (KU §5.3).
/// Titles: [0], [1], [0, 1].
pub(crate) fn fmts_image_with(verify: bool) -> Fx {
    let files = [
        BdFile::new("BDMV/STREAM/00001.m2ts", 30, Some(K1)),
        BdFile::new("BDMV/STREAM/00002.fmts", 180, Some(K2)),
        BdFile::new("AACS/IndividualSegment.tbl", 1, None),
    ];
    let uk_ro = libfreemkv::test_util::unit_key_ro(AacsVersion::V10, &[[0xEE; 16]; 2], &[1; 3]);
    let mut img = encrypted_bd_image(&files, &uk_ro);
    let segs = [(1u16, 0u32, 16u32), (2, 20, 36)];
    let mut tbl = Vec::new();
    tbl.extend_from_slice(&0x0100_0000u32.to_be_bytes());
    tbl.extend_from_slice(&(segs.len() as u16).to_be_bytes());
    tbl.extend_from_slice(&16u16.to_be_bytes());
    for &(index, a, b) in &segs {
        tbl.extend_from_slice(&0x0100_0000u32.to_be_bytes());
        tbl.extend_from_slice(&index.to_be_bytes());
        tbl.extend_from_slice(&1u16.to_be_bytes());
        tbl.extend_from_slice(&(a * 32).to_be_bytes());
        tbl.extend_from_slice(&(b * 32 - 1).to_be_bytes());
    }
    let at = img.files[2].0 as usize * 2048;
    img.image[at..at + tbl.len()].copy_from_slice(&tbl);
    let clip = img.files[1].0;
    for &(index, a, b) in &segs {
        let ours = if index == 1 { F1 } else { F2 };
        for u in a..b {
            let alt = if verify && index == 2 { ours } else { ALT };
            let key = if (u - a) % 2 == 0 { ours } else { alt };
            let at = (clip + u * 3) as usize * 2048;
            let mut unit = img.plain[at..at + 6144].to_vec();
            assert!(libfreemkv::aacs::content::encrypt_unit(&mut unit, &key));
            img.image[at..at + 6144].copy_from_slice(&unit);
        }
    }
    let disc = manual_disc(&img, &uk_ro, &[&[0], &[1], &[0, 1]]);
    Fx {
        img,
        disc,
        metadata: Vec::new(),
    }
}

/// [`fmts_image_with`] without the `Phase::Verify` variant.
pub(crate) fn fmts_image() -> Fx {
    fmts_image_with(false)
}

// A UHD FMTS disc over `img` whose title `t` plays files `titles[t]` (not scanned: the
// fixture has no playlists).
fn manual_disc(img: &EncryptedBdImage, uk_ro: &[u8], titles: &[&[usize]]) -> Disc {
    let titles = titles
        .iter()
        .enumerate()
        .map(|(t, files)| {
            let extents: Vec<libfreemkv::disc::Extent> = files
                .iter()
                .map(|&f| libfreemkv::disc::Extent {
                    start_lba: img.files[f].0,
                    sector_count: img.files[f].1,
                })
                .collect();
            libfreemkv::DiscTitle {
                playlist: format!("{t:05}.mpls"),
                size_bytes: extents.iter().map(|e| e.sector_count as u64 * 2048).sum(),
                extents,
                ..libfreemkv::DiscTitle::empty()
            }
        })
        .collect();
    let capacity = (img.image.len() / 2048) as u32;
    let hash = libfreemkv::aacs::inf::disc_hash_hex(&libfreemkv::aacs::inf::disc_hash(uk_ro));
    Disc {
        volume_id: "KU_E1_FMTS".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::Fmts,
        capacity_sectors: capacity,
        capacity_bytes: capacity as u64 * 2048,
        layers: 1,
        titles,
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: Some(
            libfreemkv::test_util::aacs_state()
                .disc_hash(hash)
                .volume_id(VID)
                .uk_ro(uk_ro.to_vec())
                .build(),
        ),
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

/// A clear (decrypted) one-title BD folder under `dir`, the LPCM clip of [`bd_image`].
pub(crate) fn clear_folder(dir: &Path) {
    let file = |rel: &str, bytes: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    };
    let n = CLIP_UNITS * 32;
    let m2ts: Vec<u8> = (0..n)
        .flat_map(|k| lpcm_source_packet(k, n, false))
        .collect();
    file("BDMV/index.bdmv", &[1u8; 64]);
    file("BDMV/PLAYLIST/00000.mpls", &one_item_mpls(b"00000"));
    file("BDMV/CLIPINF/00000.clpi", &minimal_clpi(n));
    file("BDMV/STREAM/00000.m2ts", &m2ts);
}
