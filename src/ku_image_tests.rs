//! KU-E1 image front-door tests (KU §3.2, §4.2, §12): `open_image_with`, the no-rescan image
//! mux (J14), one resolution per rip, and the E7034 rule (J11, J12).

use crate::image::{ImageSource, OpenImageOptions, open_image_with};
use crate::recovery::mapfile::vid_fingerprint;
use crate::remux::MuxPlan;
use crate::test_fixtures::{
    Answer, Calls, Damage, Drive, F1, F2, Fx, K1, K2, VID, bd_image, factory, fmts_factory,
    fmts_image, resolve,
};
use crate::{Mapfile, RipOutcome, mapfile_path_for, mux_image_titles};
use libfreemkv::error::{E_AACS_VID_NEEDS_DISC, E_NO_DISC_KEY};
use libfreemkv::keys::{KeyScope, ResolvedKeySet};
use libfreemkv::{Error, SectorSource};
use std::path::Path;

// Zero the sectors a sweep can leave unread in a staged ISO: every MPLS and
// `/AACS/Unit_Key_RO.inf` (EK13).
fn damage_metadata(fx: &Fx, iso: &Path) {
    let mut bytes = std::fs::read(iso).unwrap();
    for &(s, n) in &fx.metadata {
        bytes[s as usize * 2048..(s + n) as usize * 2048].fill(0);
    }
    std::fs::write(iso, bytes).unwrap();
}

fn mkv_dest(dir: &Path) -> impl Fn(usize) -> String {
    let dir = dir.to_path_buf();
    move |idx| format!("mkv://{}", dir.join(format!("t{idx}.mkv")).display())
}

fn mux_all(opened: &crate::OpenedImage, titles: Vec<usize>, dir: &Path) -> RipOutcome {
    let n = titles.len();
    let out = mux_image_titles(
        opened,
        &MuxPlan::new(titles),
        &mkv_dest(dir),
        &crate::NoopSink,
    );
    assert_eq!(out, RipOutcome::Ok { titles_written: n });
    out
}

fn titles(v: &[usize]) -> Option<KeyScope> {
    Some(KeyScope::Titles(v.to_vec()))
}

/// EK13 (J14, the server's staged-ISO regression): a staged ISO whose MPLS and
/// `Unit_Key_RO.inf` sectors were never read muxes both titles from the drive-scanned
/// disc and its set. The image is never rescanned: a rescan finds no playlist there.
#[test]
fn staged_iso_with_damaged_udf_muxes_from_drive_scanned_title() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let calls = Calls::default();
    let set = resolve(
        &fx,
        KeyScope::Titles(vec![0, 1]),
        &[(Answer::Online, &[K1, K2])],
        &calls,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let whole = ImageSource::Iso(fx.write(dir.path(), "whole.iso"));
    let staged = fx.write(dir.path(), "staged.iso");
    damage_metadata(&fx, &staged);
    assert!(
        crate::scan_image(&ImageSource::Iso(staged.clone()))
            .map_or(true, |(d, _)| d.titles.is_empty()),
        "fixture: a rescan of the staged image finds no title"
    );
    let asked = calls.len();

    let open = |src: &ImageSource| {
        let opts = OpenImageOptions {
            disc: Some(bd_image(&[Some(K1), Some(K2)], 2).disc),
            scope: titles(&[0, 1]),
            ..OpenImageOptions::known(set.clone())
        };
        open_image_with(src, opts).unwrap()
    };
    let got = tempfile::tempdir().unwrap();
    let want = tempfile::tempdir().unwrap();
    let opened = open(&ImageSource::Iso(staged));
    assert!(opened.prescanned && opened.sources.is_none());
    mux_all(&opened, vec![0, 1], got.path());
    mux_all(&open(&whole), vec![0, 1], want.path());
    for t in ["t0.mkv", "t1.mkv"] {
        let (g, w) = (got.path().join(t), want.path().join(t));
        let probe = |p: &Path| crate::verify_mkv(p, &fx.disc.titles[0]).unwrap();
        assert_eq!(probe(&g).tracks.len(), 1, "{t}");
        assert_eq!(probe(&g).last_cue_secs, probe(&w).last_cue_secs, "{t}");
        assert_eq!(
            std::fs::metadata(&g).unwrap().len(),
            std::fs::metadata(&w).unwrap().len()
        );
    }
    assert_eq!(calls.len(), asked, "a Known set makes no request");
}

/// EK14 (KU §3.2 `Known`, §4.2): a set the caller holds is used as-is, with no key-service
/// call, across open and mux.
#[test]
fn known_set_makes_no_key_request() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let calls = Calls::default();
    let set = resolve(
        &fx,
        KeyScope::Titles(vec![0, 1]),
        &[(Answer::Online, &[K1, K2])],
        &calls,
    )
    .unwrap();
    let asked = calls.len();
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let opts = OpenImageOptions {
        scope: titles(&[0, 1]),
        ..OpenImageOptions::known(set.clone())
    };
    let opened = open_image_with(&src, opts).unwrap();
    assert!(!opened.prescanned, "no disc given: the image was scanned");
    mux_all(&opened, vec![0, 1], dir.path());
    assert_eq!(calls.len(), asked, "0 requests");

    // A Known set that does not cover the scope refuses (E7022) and still asks nothing.
    let narrow = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &calls,
    )
    .unwrap();
    let opts = OpenImageOptions {
        scope: titles(&[0, 1]),
        ..OpenImageOptions::known(narrow)
    };
    let err = open_image_with(&src, opts).map(|_| ()).unwrap_err();
    assert_eq!(err.code(), E_NO_DISC_KEY, "{err}");
}

/// EK14, FMTS (KS-25, KS-26: evidence, no public spec): a set carrying the forensic keys
/// is used with 0 requests; a set whose forensic keys are Pending, `Seeded`, makes exactly
/// the one anchor request from the image, with the seed's in-memory VID.
#[test]
fn known_set_makes_no_key_request_fmts() {
    let fx = fmts_image();
    let (clip, _) = fx.img.files[1];
    let calls = Calls::default();
    let f = fmts_factory(&[(Answer::Online, &[K1, K2])], &[F1, F2], &calls);
    let scope = KeyScope::Titles(vec![0, 1, 2]);
    let whole =
        |d: &mut Drive| ResolvedKeySet::resolve(&fx.disc, d, scope.clone(), &f, Default::default());
    let drive = Drive::new(&fx.img.image);
    let set = whole(&mut drive.clone()).unwrap().keys;
    assert!(!set.forensic_pending());
    let asked = calls.len();
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "uhd.iso");
    let src = ImageSource::Iso(iso.clone());
    let disc = || Some(fmts_image().disc);
    let opts = OpenImageOptions {
        disc: disc(),
        scope: Some(scope.clone()),
        ..OpenImageOptions::known(set)
    };
    let opened = open_image_with(&src, opts).unwrap();
    let raw = libfreemkv::FileSectorSource::open(&iso).unwrap();
    let mut r = opened.keys.title_reader(&opened.disc, 1, raw).unwrap();
    r.set_unit_base(clip);
    let mut buf = vec![0u8; 3 * 2048];
    r.read_sectors(clip, 3, &mut buf, true).unwrap();
    assert_eq!(buf[4], 0x47, "index-1 unit 0 decrypts with the held F1");
    assert_eq!(calls.len(), asked, "0 requests with the forensic set held");

    // Every index-1 anchor unreadable at the drive: Pending, then one anchor request.
    drive.set(Damage::Range(clip, clip + 16 * 3));
    let pending = whole(&mut drive.clone()).unwrap().keys;
    assert!(pending.forensic_pending());
    let calls = Calls::default();
    let f = fmts_factory(&[(Answer::Online, &[K1, K2])], &[F1, F2], &calls);
    let opts = OpenImageOptions {
        disc: disc(),
        scope: Some(scope),
        ..OpenImageOptions::seeded(f, pending)
    };
    let opened = open_image_with(&src, opts).unwrap();
    assert!(!opened.keys.forensic_pending());
    let all = calls.all();
    assert_eq!(all.len(), 1, "exactly the one anchor request: {all:?}");
    assert_eq!((calls.forensic(), all[0].who), (1, "online"));
    assert_eq!(
        all[0].vid,
        Some(VID),
        "with the seed's in-memory VID (coord 3)"
    );
}

/// EK12 (KU §3.2, §7.7 item 2): the engine image front door resolves once and hands the set
/// to every title; nothing asks after the open. `input_options` carries `keys`.
#[test]
fn open_image_resolves_once_and_hands_the_set_to_every_title() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K1)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let opts = OpenImageOptions {
        scope: titles(&[0, 1, 2]),
        ..OpenImageOptions::resolve(f.clone())
    };
    let opened = open_image_with(&src, opts).unwrap();
    assert_eq!(calls.len(), 2, "one request per key group");
    mux_all(&opened, vec![0, 1, 2], dir.path());
    assert_eq!(calls.len(), 2, "0 after open");

    // Opened for the main title only: ONE scope top-up before the first output byte.
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    assert_eq!(calls.len(), 1);
    mux_all(&opened, vec![0, 1, 2], dir.path());
    assert_eq!(calls.len(), 2, "the top-up asked only for K2's group");

    let input = opened.input_options(1, libfreemkv::StreamSelection::default());
    assert!(input.keys.is_some(), "the set, not banked keys or a fetch");
    assert!(input.unit_keys.is_empty() && input.key_fetch.is_none());
    let src_code = include_str!("image.rs");
    assert!(!src_code.contains(&["fn build_key", "_fetch"].concat()));

    // remux_iso: one open, one resolution round for its title, then the mux.
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let job = crate::RemuxJob {
        iso: src,
        title: Some(1),
        streams: crate::StreamChoice::default(),
        target: dir.path().join("remux.mkv"),
        replace: false,
    };
    crate::remux::remux_iso_with(&job, f, &crate::NoopSink).unwrap();
    assert_eq!(calls.len(), 1, "one request, for title 1's group");
    assert!(job.target.exists());
}

// A VID-derivable disc (KS-16: the key needs the VID) with a sidecar mapfile.
struct VidCase {
    dir: tempfile::TempDir,
    iso: std::path::PathBuf,
}

fn vid_case(sidecar_vidfp: bool) -> VidCase {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "capture.iso");
    let mut map = Mapfile::create(&mapfile_path_for(&iso), fx.img.image.len() as u64, "t").unwrap();
    map.record(0, fx.img.image.len() as u64, crate::SectorStatus::Finished)
        .unwrap();
    map.set_disc_hash(&fx.disc.aacs.as_ref().unwrap().disc_hash);
    if sidecar_vidfp {
        map.set_vid_fingerprint(vid_fingerprint(&VID));
    }
    map.flush().unwrap();
    VidCase { dir, iso }
}

fn needs_vid(calls: &Calls) -> libfreemkv::KeySourceFactory {
    factory(&[(Answer::OnlineNeedsVid, &[K1])], calls)
}

fn open_needing_vid(
    c: &VidCase,
    calls: &Calls,
    disc_vid: Option<[u8; 16]>,
    vid: Option<[u8; 16]>,
) -> crate::Result<crate::OpenedImage> {
    let disc = disc_vid.map(|v| {
        let mut d = bd_image(&[Some(K1)], 1).disc;
        d.aacs.as_mut().unwrap().volume_id = v;
        d
    });
    let opts = OpenImageOptions {
        disc,
        vid,
        ..OpenImageOptions::resolve(needs_vid(calls))
    };
    open_image_with(&ImageSource::Iso(c.iso.clone()), opts)
}

/// EK15 (J11, J12; KS-16 "Kvu = AES-G(Km, IDv)", KS-29 the VID "from the media"): a resume
/// that needs the VID asks once without it, then needs the disc (E7034), touching nothing.
#[test]
fn resume_needing_the_vid_asks_once_then_needs_the_disc() {
    let c = vid_case(true);
    let map_path = mapfile_path_for(&c.iso);
    let before = (
        std::fs::read(&c.iso).unwrap(),
        std::fs::read(&map_path).unwrap(),
    );
    // (a) No VID in hand: one request, then E7034; image and mapfile byte-identical.
    let calls = Calls::default();
    let err = open_needing_vid(&c, &calls, None, None)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.code(), E_AACS_VID_NEEDS_DISC, "{err}");
    assert!(matches!(err, Error::AacsVidNeedsDisc));
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (
            std::fs::read(&c.iso).unwrap(),
            std::fs::read(&map_path).unwrap()
        ),
        before,
        "nothing written"
    );
    // (b) The drive's scanned disc carries the VID: one request, Ok, and the mux succeeds.
    let calls = Calls::default();
    let opened = open_needing_vid(&c, &calls, Some(VID), None).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls.all()[0].vid, Some(VID));
    mux_all(&opened, vec![0], c.dir.path());
    // (c) A VID whose fingerprint differs from the sidecar's: mismatch, 0 requests.
    let calls = Calls::default();
    let err = open_needing_vid(&c, &calls, None, Some([0x33; 16]))
        .map(|_| ())
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::MapfileInvalid {
                kind: "disc-mismatch"
            }
        ),
        "{err:?}"
    );
    assert_eq!(calls.len(), 0);
    // (d) No sidecar vidfp: E7022, not E7034.
    let d = vid_case(false);
    let err = open_needing_vid(&d, &Calls::default(), None, None)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.code(), E_NO_DISC_KEY, "{err}");
    // (e) No raw VID in any file the rip wrote (FK10's scan).
    let hex: String = VID.iter().map(|b| format!("{b:02x}")).collect();
    for e in std::fs::read_dir(c.dir.path()).unwrap() {
        let bytes = std::fs::read(e.unwrap().path()).unwrap();
        assert!(
            !bytes.windows(16).any(|w| w == VID),
            "raw VID bytes on disk"
        );
        assert!(
            !String::from_utf8_lossy(&bytes).contains(&hex),
            "VID hex on disk"
        );
    }
}

/// SG28 — per spec; do not change without a spec citation — KS-16, KS-29: "Kvu =
/// AES-G(Km, IDv)"; the drive "reads Volume ID … from the media". E7034 only when a piece
/// is Missing, no VID is in hand and the sidecar has a `vidfp`; with the VID, E7022.
#[test]
fn vid_needed_only_when_no_vid_in_hand_and_disc_has_one() {
    use libfreemkv::spec::keys::{KS_16_KVU, KS_29_VID_FROM_MEDIA};
    assert!(KS_16_KVU.text.contains("Kvu = AES-G(Km, IDv)"));
    assert!(KS_29_VID_FROM_MEDIA.text.contains("from the media"));
    let c = vid_case(true);
    let never = |calls: &Calls| factory(&[(Answer::Online, &[])], calls);
    let open = |disc_vid: Option<[u8; 16]>, vid: Option<[u8; 16]>, c: &VidCase| {
        let disc = disc_vid.map(|v| {
            let mut d = bd_image(&[Some(K1)], 1).disc;
            d.aacs.as_mut().unwrap().volume_id = v;
            d
        });
        let opts = OpenImageOptions {
            disc,
            vid,
            ..OpenImageOptions::resolve(never(&Calls::default()))
        };
        open_image_with(&ImageSource::Iso(c.iso.clone()), opts)
            .map(|_| ())
            .unwrap_err()
            .code()
    };
    assert_eq!(
        open(None, None, &c),
        E_AACS_VID_NEEDS_DISC,
        "no VID in hand"
    );
    assert_eq!(open(None, Some(VID), &c), E_NO_DISC_KEY, "the caller's VID");
    assert_eq!(
        open(Some(VID), None, &c),
        E_NO_DISC_KEY,
        "the scanned disc's VID"
    );
    assert_eq!(
        open(None, None, &vid_case(false)),
        E_NO_DISC_KEY,
        "no vidfp"
    );
}
