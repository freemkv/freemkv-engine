//! Test fixtures for the keys-up-front (KU) engine tests: a scannable AACS-encrypted BD
//! image (one playlist per stream file) and counting fake key sources.
//!
//! The MPLS/CLPI builders mirror libfreemkv's `dirimage::tests::{one_item_mpls,
//! minimal_clpi}` (crate-private there). The clips carry TS sync and CPI but no
//! elementary stream, so a mux of them reads every unit, then ends E6008 (`MkvInvalid`).

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

// A one-PlayItem MPLS on `clip`, 2 min long (`parse_playlist` drops < 30 s stubs).
fn one_item_mpls(clip: &[u8; 5]) -> Vec<u8> {
    let mut buf = b"MPLS0200".to_vec();
    buf.extend_from_slice(&40u32.to_be_bytes());
    buf.extend_from_slice(&[0u8; 28]);
    let pl = buf.len();
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    let mut item = clip.to_vec();
    item.extend_from_slice(b"M2TS");
    item.extend_from_slice(&[0u8; 3]);
    item.extend_from_slice(&0u32.to_be_bytes());
    item.extend_from_slice(&(45_000u32 * 120).to_be_bytes());
    item.extend_from_slice(&[0u8; 12]);
    item.extend_from_slice(&16u16.to_be_bytes());
    item.extend_from_slice(&[0u8; 16]);
    buf.extend_from_slice(&(item.len() as u16).to_be_bytes());
    buf.extend_from_slice(&item);
    let pl_len = (buf.len() - pl - 4) as u32;
    buf[pl..pl + 4].copy_from_slice(&pl_len.to_be_bytes());
    let mark_start = buf.len() as u32;
    buf[12..16].copy_from_slice(&mark_start.to_be_bytes());
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf
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
