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
        self.calls
            .0
            .lock()
            .unwrap()
            .push(Call { who: self.who, vid });
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
    let specs: Vec<(Answer, Vec<[u8; 16]>)> = specs.iter().map(|(a, k)| (*a, k.to_vec())).collect();
    let calls = calls.clone();
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
