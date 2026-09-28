//! KU-E1 image front-door tests (KU §3.2, §4.2, §12): `open_image_with`, the no-rescan image
//! mux (J14), one resolution per rip, and the E7034 rule (J11, J12).

use crate::image::{ImageSource, OpenImageOptions, open_image_with};
use crate::recovery::mapfile::vid_fingerprint;
use crate::remux::MuxPlan;
use crate::test_fixtures::{
    Answer, Calls, Damage, Drive, F1, F2, Fx, K1, K2, VID, bd_image, bd_image_sized, factory,
    fmts_factory, fmts_image, fmts_image_with, resolve,
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

// The staged image's sidecar mapfile: the disc's hash, every sector Finished except
// `unread` (`(lba, sectors)`), which the sweep left NonTrimmed.
fn staged_sidecar(fx: &Fx, iso: &Path, unread: &[(u32, u32)]) {
    let total = fx.img.image.len() as u64;
    let mut map = Mapfile::create(&mapfile_path_for(iso), total, "t").unwrap();
    map.record(0, total, crate::SectorStatus::Finished).unwrap();
    for &(s, n) in unread {
        let (pos, len) = (s as u64 * 2048, n as u64 * 2048);
        map.record(pos, len, crate::SectorStatus::NonTrimmed)
            .unwrap();
    }
    map.set_disc_hash(&fx.disc.aacs.as_ref().unwrap().disc_hash);
    map.flush().unwrap();
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
/// `Unit_Key_RO.inf` sectors were never read (zeroed, marked NonTrimmed in its sidecar)
/// muxes both titles from the drive-scanned disc and its set, byte-equal to the whole
/// image's mux. No rescan: a rescan finds no playlist there. `MuxSource::Iso` opens the
/// file itself, so reads are not counted; the one metadata read is §3.2's key-file check.
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
    staged_sidecar(&fx, &staged, &fx.metadata);
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
        let bytes = |p: &Path| std::fs::read(p).unwrap();
        assert!(
            bytes(&g) == bytes(&w),
            "{t}: byte-equal to the whole image's mux"
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
    crate::remux::remux_iso_sources(&job, f, &crate::NoopSink, &crate::Halt::new()).unwrap();
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

/// KU §2.3 step 13 / Stop T31: a Stop before an image mux's scope top-up stops that resolve
/// too (no request), and the loop reports the halt.
#[test]
fn a_stopped_image_mux_makes_no_top_up_request() {
    struct Stopped;
    impl crate::Sink for Stopped {
        fn should_cancel(&self) -> bool {
            true
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    assert_eq!(calls.len(), 1);
    let plan = MuxPlan::new(vec![0, 1]);
    let out = mux_image_titles(&opened, &plan, &mkv_dest(dir.path()), &Stopped);
    assert_eq!(out, RipOutcome::Halted);
    assert_eq!(calls.len(), 1, "the top-up saw the Stop and asked nothing");
}

/// KU §2.3 step 13 / Stop T31: `remux_iso` resolves under the sink's Stop, so a stopped
/// remux asks no key source and ends `Halted`.
#[test]
fn a_stopped_remux_makes_no_key_request() {
    struct Stopped;
    impl crate::Sink for Stopped {
        fn should_cancel(&self) -> bool {
            true
        }
    }
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let calls = Calls::default();
    let job = crate::RemuxJob {
        iso: ImageSource::Iso(fx.write(dir.path(), "d.iso")),
        title: None,
        streams: crate::StreamChoice::default(),
        target: dir.path().join("remux.mkv"),
        replace: false,
    };
    let f = factory(&[(Answer::Online, &[K1])], &calls);
    let err = crate::remux::remux_iso_sources(&job, f, &Stopped, &crate::Halt::new()).unwrap_err();
    assert!(libfreemkv::is_halt(&err), "{err}");
    assert_eq!(calls.len(), 0, "the Stop reached the resolve");
}

/// D1 (KU §2.1 invariant 4, "never call the key service twice"): the server muxes one title
/// per `mux_image_titles` call on one opened image. The one top-up is remembered: a later
/// call for another title of the same group asks nothing, the keydb included.
#[test]
fn the_top_up_is_asked_once_across_mux_calls() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(
        &[(Answer::Keydb, &[K1]), (Answer::Online, &[K1, K2])],
        &calls,
    );
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    let count = |who: &str| calls.all().iter().filter(|c| c.who == who).count();
    assert_eq!((count("keydb"), count("online")), (1, 0), "the open");
    mux_all(&opened, vec![1], dir.path());
    assert_eq!(
        (count("keydb"), count("online")),
        (2, 1),
        "one top-up for K2"
    );
    mux_all(&opened, vec![2], dir.path());
    mux_all(&opened, vec![1, 2], dir.path());
    assert_eq!(
        (count("keydb"), count("online")),
        (2, 1),
        "never asked again"
    );
}

/// D3: a key refusal before any output (E7022 here; E7026/E7034 alike) is reported as the
/// first planned title's failure (TitleStart, then TitleDone with the error), never as the
/// silence a consumer reads as a user Stop.
#[test]
fn an_up_front_key_refusal_reports_the_first_title_failed() {
    #[derive(Default)]
    struct Events(std::sync::Mutex<Vec<String>>);
    impl crate::Sink for Events {
        fn event(&self, e: &crate::Event<'_>) {
            let s = match e {
                crate::Event::TitleStart { idx, .. } => format!("start:{idx}"),
                crate::Event::TitleDone { idx, result, .. } => format!(
                    "done:{idx}:{:?}",
                    result.as_ref().err().and_then(|e| crate::error_code(e))
                ),
                _ => return,
            };
            self.0.lock().unwrap().push(s);
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let narrow = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    let opened = open_image_with(&src, OpenImageOptions::known(narrow)).unwrap();
    let sink = Events::default();
    let out = mux_image_titles(
        &opened,
        &MuxPlan::new(vec![1, 0]),
        &mkv_dest(dir.path()),
        &sink,
    );
    assert!(
        matches!(
            out,
            RipOutcome::Failed { title_index: 1, .. } | RipOutcome::NoKey
        ),
        "{out:?}"
    );
    let want = [
        "start:1".to_string(),
        format!("done:1:Some({E_NO_DISC_KEY})"),
    ];
    assert_eq!(*sink.0.lock().unwrap(), want);
}

/// D5 (judgement 6: an identity header that cannot be read is refused, never "no identity"):
/// a sidecar mapfile that exists but does not load refuses the open; only a missing one is
/// no identity.
#[test]
fn an_unreadable_sidecar_refuses_the_open() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "d.iso");
    std::fs::write(mapfile_path_for(&iso), "# freemkv-vidfp: zz\n0x0 0x800 +\n").unwrap();
    let calls = Calls::default();
    let opts = OpenImageOptions::resolve(factory(&[(Answer::Keydb, &[K1])], &calls));
    let err = open_image_with(&ImageSource::Iso(iso), opts)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::MapfileInvalid { .. }), "{err:?}");
    assert_eq!(calls.len(), 0, "refused before any request");
}

/// D4 (KU §3.2 "the disc hash is checked when the image's `Unit_Key_RO.inf` is readable";
/// §4.4 decides otherwise): with no sidecar identity an image whose key file cannot be
/// checked is refused, since a same-capacity wrong image would decrypt its Keyed pieces
/// to garbage. With a sidecar, only sectors it marks unread are exempt.
#[test]
fn a_prescanned_disc_refuses_an_image_it_cannot_identify() {
    let fx = bd_image(&[Some(K1)], 1);
    let set = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    let inf = *fx.metadata.last().unwrap();
    let open = |iso: &Path| {
        let opts = OpenImageOptions {
            disc: Some(bd_image(&[Some(K1)], 1).disc),
            ..OpenImageOptions::known(set.clone())
        };
        open_image_with(&ImageSource::Iso(iso.to_path_buf()), opts).map(|_| ())
    };
    let zero_inf = |iso: &Path| {
        let mut b = std::fs::read(iso).unwrap();
        b[inf.0 as usize * 2048..(inf.0 + inf.1) as usize * 2048].fill(0);
        std::fs::write(iso, b).unwrap();
    };
    let dir = tempfile::tempdir().unwrap();
    // No sidecar, key file unreadable: refused.
    let bare = fx.write(dir.path(), "bare.iso");
    zero_inf(&bare);
    let err = open(&bare).unwrap_err();
    assert!(
        matches!(
            err,
            Error::MapfileInvalid {
                kind: "disc-mismatch"
            }
        ),
        "{err:?}"
    );
    // A sidecar that says the key file WAS read: its zero bytes are the image's; refused.
    let finished = fx.write(dir.path(), "finished.iso");
    zero_inf(&finished);
    staged_sidecar(&fx, &finished, &[]);
    assert!(matches!(open(&finished), Err(Error::MapfileInvalid { .. })));
    // A sidecar marking the key file unread, with the disc's identity: rules 1-2 decide.
    let staged = fx.write(dir.path(), "staged.iso");
    zero_inf(&staged);
    staged_sidecar(&fx, &staged, &[inf]);
    open(&staged).expect("a sweep-zeroed key file with sidecar identity passes");
}

/// D6 (J14 holds for `iso://` only): a `dir://` folder muxes through `input()`, which scans
/// the folder, so a pre-scanned disc cannot be honoured there and is refused (E7013, a
/// caller bug, KU §6). A folder opened without one keeps working.
#[test]
fn a_prescanned_disc_is_refused_for_a_folder() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("disc");
    crate::test_fixtures::clear_folder(&folder);
    let src = ImageSource::Dir(folder);
    let none = || factory(&[], &Calls::default());
    let (disc, _) = crate::scan_image(&src).unwrap();
    let opts = OpenImageOptions {
        disc: Some(disc),
        ..OpenImageOptions::resolve(none())
    };
    let err = open_image_with(&src, opts).map(|_| ()).unwrap_err();
    assert_eq!(err.code(), libfreemkv::error::E_DECRYPT_FAILED, "{err}");
    let opened = open_image_with(&src, OpenImageOptions::resolve(none())).unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    mux_all(&opened, vec![0], &out);
}

/// D8: an all-zero `vid` is no VID (KS-29: a VID is read from the media); it never hides the
/// scanned disc's real one, so a Missing piece with the disc's VID in hand stays E7022.
#[test]
fn a_zero_vid_never_hides_the_discs_vid() {
    let c = vid_case(true);
    let mut disc = bd_image(&[Some(K1)], 1).disc;
    disc.aacs.as_mut().unwrap().volume_id = VID;
    let opts = OpenImageOptions {
        disc: Some(disc),
        vid: Some([0; 16]),
        ..OpenImageOptions::resolve(factory(&[(Answer::Online, &[])], &Calls::default()))
    };
    let err = open_image_with(&ImageSource::Iso(c.iso.clone()), opts)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.code(), E_NO_DISC_KEY, "{err}");
}

/// EK14, `Phase::Verify` (KU §5.3; KS-25, KS-26 evidence): an index whose every phase probe
/// faulted at the drive is held as `Verify`. A Known set carrying it makes 0 requests from
/// the image, and each unit is kept decrypted only if it opens clean: here both halves of
/// index 2 open with F2, which only `Verify` decrypts (a resolved phase leaves one half).
#[test]
fn known_set_with_a_verify_phase_decrypts_both_halves() {
    let fx = fmts_image_with(true);
    let (clip, _) = fx.img.files[1];
    let calls = Calls::default();
    let f = fmts_factory(&[(Answer::Online, &[K1, K2])], &[F1, F2], &calls);
    let drive = Drive::new(&fx.img.image);
    drive.set(Damage::Range(clip + 20 * 3, clip + 36 * 3));
    let scope = KeyScope::Titles(vec![1]);
    let set = ResolvedKeySet::resolve(
        &fx.disc,
        &mut drive.clone(),
        scope.clone(),
        &f,
        Default::default(),
    )
    .unwrap()
    .keys;
    let asked = calls.len();
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "uhd.iso");
    let opts = OpenImageOptions {
        disc: Some(fmts_image_with(true).disc),
        scope: Some(scope),
        ..OpenImageOptions::known(set)
    };
    let opened = open_image_with(&ImageSource::Iso(iso.clone()), opts).unwrap();
    let raw = libfreemkv::FileSectorSource::open(&iso).unwrap();
    let mut r = opened.keys.title_reader(&opened.disc, 1, raw).unwrap();
    r.set_unit_base(clip);
    for u in [20u32, 21] {
        let mut buf = vec![0u8; 3 * 2048];
        r.read_sectors(clip + u * 3, 3, &mut buf, true).unwrap();
        assert!(
            buf.chunks(192).all(|p| p[4] == 0x47),
            "index-2 unit {u} decrypted"
        );
    }
    assert_eq!(calls.len(), asked, "0 requests from the image");

    // Control: with the phase probes readable, index 2 resolves to Even and unit 21 (odd)
    // is left as ciphertext, so the decrypt above is Verify's.
    let set = ResolvedKeySet::resolve(
        &fx.disc,
        &mut fx.source(),
        KeyScope::Titles(vec![1]),
        &f,
        Default::default(),
    )
    .unwrap()
    .keys;
    let raw = libfreemkv::FileSectorSource::open(&iso).unwrap();
    let mut r = set.title_reader(&fx.disc, 1, raw).unwrap();
    r.set_unit_base(clip);
    let mut buf = vec![0u8; 3 * 2048];
    r.read_sectors(clip + 21 * 3, 3, &mut buf, true).unwrap();
    assert!(
        !buf.chunks(192).all(|p| p[4] == 0x47),
        "a resolved phase skips unit 21"
    );
}

/// KU §3.2 / §3.1 `ExtractOptions.keys`: a decrypted-folder extract of an AACS image reads
/// through the rip's set (scope `WholeDisc`), with no disc-banked key, via the engine.
#[test]
fn extract_tree_reads_through_the_key_set() {
    let fx = bd_image(&[Some(K1)], 1);
    let set = resolve(
        &fx,
        KeyScope::WholeDisc,
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("tree");
    let r = crate::extract_tree_with(
        &fx.disc,
        &mut fx.source(),
        &dest,
        false,
        Some(&set),
        &crate::NoopSink,
    )
    .unwrap();
    assert!(!r.halted);
    let (s, n) = fx.clip(0);
    let got = std::fs::read(dest.join("BDMV/STREAM/00000.m2ts")).unwrap();
    let want = &fx.img.plain[s as usize * 2048..(s + n) as usize * 2048];
    let mask = |b: &[u8]| {
        b.chunks(192)
            .flat_map(|p| {
                let mut p = p.to_vec();
                p[0] &= 0x3F;
                p
            })
            .collect::<Vec<u8>>()
    };
    assert!(mask(&got) == mask(want), "the stream file is decrypted");
}

/// Up-front refusals keep the whole error: `TitleDone(Err)` carries the typed error, and
/// `RipOutcome::Failed` its data (here a failed top-up whose sidecar turned unreadable:
/// `MapfileInvalid { kind: "vidfp" }`), not only the code.
#[test]
fn an_up_front_refusal_keeps_the_whole_error() {
    #[derive(Default)]
    struct Done(std::sync::Mutex<Vec<String>>);
    impl crate::Sink for Done {
        fn event(&self, e: &crate::Event<'_>) {
            if let crate::Event::TitleDone {
                result: Err(err), ..
            } = e
            {
                let typed = err.get_ref().and_then(|i| i.downcast_ref::<Error>());
                self.0.lock().unwrap().push(format!("{typed:?}"));
            }
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "d.iso");
    // The top-up fails (no source holds K2), and its error path reads the corrupt sidecar.
    let f = factory(&[(Answer::Online, &[K1])], &Calls::default());
    let opened =
        open_image_with(&ImageSource::Iso(iso.clone()), OpenImageOptions::resolve(f)).unwrap();
    std::fs::write(mapfile_path_for(&iso), "# freemkv-vidfp: zz\n0x0 0x800 +\n").unwrap();
    let sink = Done::default();
    let out = mux_image_titles(
        &opened,
        &MuxPlan::new(vec![1]),
        &mkv_dest(dir.path()),
        &sink,
    );
    let RipOutcome::Failed { data, .. } = &out else {
        panic!("{out:?}")
    };
    assert_eq!(data, "vidfp");
    let events = sink.0.lock().unwrap();
    assert_eq!(*events, [r#"Some(MapfileInvalid { kind: "vidfp" })"#]);
}

/// J23 (amends J11 / SG28; KS-16 "Kvu = AES-G(Km, IDv)"): E7034 only when the VID would
/// actually help: a Km is obtainable (a keydb reports "matched, Media Key, no VID") or a
/// configured source consumes the VID (online). Otherwise a plain "no key yet" is E7022.
#[test]
fn vid_needs_disc_only_when_a_vid_would_help() {
    let c = vid_case(true);
    let code = |specs: &[(Answer, &[[u8; 16]])]| {
        let opts = OpenImageOptions::resolve(factory(specs, &Calls::default()));
        let r = open_image_with(&ImageSource::Iso(c.iso.clone()), opts);
        r.map(|_| ()).unwrap_err().code()
    };
    assert_eq!(
        code(&[(Answer::Keydb, &[])]),
        E_NO_DISC_KEY,
        "no Km path, no online source"
    );
    assert_eq!(
        code(&[(Answer::KeydbKmNoVid, &[])]),
        E_AACS_VID_NEEDS_DISC,
        "a Km path"
    );
    let online = [(Answer::Keydb, &[][..]), (Answer::Online, &[][..])];
    assert_eq!(
        code(&online),
        E_AACS_VID_NEEDS_DISC,
        "an online source configured"
    );
}

/// `ResolvedKeySet::none()` holds no AACS key, so it never covers an AACS disc's titles
/// (`covers(scope)` alone has no disc): `Seeded(f, none)` resolves, asking once; `Known(none)`
/// refuses E7022 up front. On a clear disc `none()` still covers.
#[test]
fn a_none_set_never_covers_an_aacs_title() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(&[(Answer::Online, &[K1])], &calls);
    let opened = open_image_with(&src, OpenImageOptions::seeded(f, ResolvedKeySet::none()))
        .expect("Seeded(none) resolves");
    assert_eq!(calls.len(), 1, "the factory was asked once");
    assert!(opened.keys.is_aacs());
    let err = open_image_with(&src, OpenImageOptions::known(ResolvedKeySet::none()))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.code(), E_NO_DISC_KEY, "{err}");
    let folder = dir.path().join("clear");
    crate::test_fixtures::clear_folder(&folder);
    open_image_with(
        &ImageSource::Dir(folder),
        OpenImageOptions::known(ResolvedKeySet::none()),
    )
    .expect("none() covers a clear disc");
}

/// The operator's "why no key": the per-source walk comes back on a refusal too, from both
/// front doors, and matches `OpenedImage.trace` on success.
#[test]
fn the_front_doors_return_the_trace_on_a_refusal() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let calls = Calls::default();
    let f = factory(&[(Answer::Keydb, &[K1]), (Answer::Online, &[K1])], &calls);
    let (r, trace) = crate::keys::resolve_for_rip_traced(
        &fx.disc,
        &mut fx.source(),
        KeyScope::Titles(vec![1]),
        &f,
        None,
        None,
    );
    assert_eq!(r.map(|_| ()).unwrap_err().code(), E_NO_DISC_KEY);
    let who: Vec<&str> = trace.keys.iter().map(|s| s.who.as_str()).collect();
    assert_eq!(who, ["keydb", "online"], "{trace:?}");

    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let opts = OpenImageOptions {
        scope: titles(&[1]),
        ..OpenImageOptions::resolve(f.clone())
    };
    let (r, trace) = crate::open_image_with_traced(&src, opts);
    assert!(r.is_err());
    assert_eq!(trace.keys.len(), 2, "{trace:?}");
    let (r, trace) = crate::open_image_with_traced(&src, OpenImageOptions::resolve(f));
    assert_eq!(r.unwrap().trace, trace);
}

// Online and keydb calls so far.
fn who_counts(calls: &Calls) -> (usize, usize) {
    let all = calls.all();
    let n = |w: &str| all.iter().filter(|c| c.who == w).count();
    (n("keydb"), n("online"))
}

// The code each `TitleDone(Err)` carried, in order.
#[derive(Default)]
struct DoneCodes(std::sync::Mutex<Vec<Option<u16>>>);
impl crate::Sink for DoneCodes {
    fn event(&self, e: &crate::Event<'_>) {
        if let crate::Event::TitleDone { result: Err(e), .. } = e {
            self.0.lock().unwrap().push(crate::error_code(e));
        }
    }
}

/// B1: a first top-up whose request failed (E7028) spends the ask, and a later uncovered
/// title re-raises THAT failure instead of E7022, which would claim every source answered.
#[test]
fn a_failed_top_up_is_remembered_not_reported_as_no_key() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(
        &[(Answer::Keydb, &[K1]), (Answer::Unavailable, &[])],
        &calls,
    );
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    let plan = |t: usize| MuxPlan::new(vec![t]);
    let sink = DoneCodes::default();
    mux_image_titles(&opened, &plan(1), &mkv_dest(dir.path()), &sink);
    let asked = calls.len();
    mux_image_titles(&opened, &plan(2), &mkv_dest(dir.path()), &sink);
    let e7028 = Some(libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE);
    assert_eq!(*sink.0.lock().unwrap(), [e7028, e7028], "not E7022");
    assert_eq!(calls.len(), asked, "the ask was spent: not asked again");
}

/// B1: a top-up that made no request (a Stop before it, or an image that would not open)
/// has not spent the ask: the next call asks and succeeds.
#[test]
fn a_top_up_that_made_no_request_is_not_spent() {
    struct Stopped;
    impl crate::Sink for Stopped {
        fn should_cancel(&self) -> bool {
            true
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "d.iso");
    let calls = Calls::default();
    let f = factory(
        &[(Answer::Keydb, &[K1]), (Answer::Online, &[K1, K2])],
        &calls,
    );
    let opened =
        open_image_with(&ImageSource::Iso(iso.clone()), OpenImageOptions::resolve(f)).unwrap();
    let plan = MuxPlan::new(vec![1]);
    let out = mux_image_titles(&opened, &plan, &mkv_dest(dir.path()), &Stopped);
    assert_eq!(out, RipOutcome::Halted);
    let away = dir.path().join("away.iso");
    std::fs::rename(&iso, &away).unwrap();
    let out = mux_image_titles(&opened, &plan, &mkv_dest(dir.path()), &crate::NoopSink);
    assert!(matches!(out, RipOutcome::Failed { .. }), "{out:?}");
    std::fs::rename(&away, &iso).unwrap();
    assert_eq!(who_counts(&calls), (1, 0), "no top-up request so far");
    mux_all(&opened, vec![1], dir.path());
    assert_eq!(who_counts(&calls), (2, 1), "the one top-up");
}

/// Minor 2: a top-up resolves the union of the held scope and the new titles, so muxing
/// [0], [1], [0] resolves once.
#[test]
fn a_top_up_keeps_the_held_scope() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let f = factory(&[(Answer::Online, &[K1, K2])], &Calls::default());
    let opened = open_image_with(&src, OpenImageOptions::resolve(f)).unwrap();
    let held = opened.keys_for(&[1], None).unwrap();
    assert!(held.covers(&KeyScope::Titles(vec![0, 1])), "{held:?}");
}

/// Minor 3: when the only gap is forensic keys left Pending at the drive, a top-up over a
/// scope the set already covers cannot fill it: the held set comes back, nothing is asked.
#[test]
fn a_pending_only_gap_is_not_re_resolved() {
    let fx = fmts_image();
    let (clip, _) = fx.img.files[1];
    let drive = Drive::new(&fx.img.image);
    drive.set(Damage::Range(clip, clip + 16 * 3));
    let calls = Calls::default();
    let f = fmts_factory(&[(Answer::Keydb, &[K1, K2])], &[F1, F2], &calls);
    let scope = KeyScope::Titles(vec![0, 1]);
    let pending =
        ResolvedKeySet::resolve(&fx.disc, &mut drive.clone(), scope, &f, Default::default())
            .unwrap()
            .keys;
    assert!(pending.forensic_pending());
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "uhd.iso");
    let asked = calls.len();
    let opened =
        crate::OpenedImage::for_test(&iso, fmts_image().disc, pending, Some(f), vec![0, 1]);
    let held = opened.keys_for(&[1], None).unwrap();
    assert!(held.forensic_pending());
    assert_eq!(calls.len(), asked, "keydb not re-asked");
}

// A staged copy of `fx` whose key file is zeroed and whose sidecar (written by `side`) is
// then adjusted as each test needs; `open` opens it with the drive disc and `set`.
fn staged_with(fx: &Fx, dir: &Path, name: &str) -> std::path::PathBuf {
    let iso = fx.write(dir, name);
    let inf = *fx.metadata.last().unwrap();
    let mut b = std::fs::read(&iso).unwrap();
    b[inf.0 as usize * 2048..(inf.0 + inf.1) as usize * 2048].fill(0);
    std::fs::write(&iso, b).unwrap();
    iso
}

fn open_prescanned(iso: &Path, disc_vid: [u8; 16], set: &ResolvedKeySet) -> crate::Result<()> {
    let mut disc = bd_image(&[Some(K1)], 1).disc;
    disc.aacs.as_mut().unwrap().volume_id = disc_vid;
    let opts = OpenImageOptions {
        disc: Some(disc),
        ..OpenImageOptions::known(set.clone())
    };
    open_image_with(&ImageSource::Iso(iso.to_path_buf()), opts).map(|_| ())
}

// A sidecar marking `unread` NonTrimmed, with `identity` lines inserted after its header.
fn sidecar_text(fx: &Fx, iso: &Path, unread: &[(u32, u32)], identity: &str) {
    let total = fx.img.image.len() as u64;
    let path = mapfile_path_for(iso);
    let mut map = Mapfile::create(&path, total, "t").unwrap();
    map.record(0, total, crate::SectorStatus::Finished).unwrap();
    for &(s, n) in unread {
        let (p, l) = (s as u64 * 2048, n as u64 * 2048);
        map.record(p, l, crate::SectorStatus::NonTrimmed).unwrap();
    }
    map.flush().unwrap();
    drop(map);
    let text = std::fs::read_to_string(&path).unwrap();
    let (head, rest) = text.split_once('\n').unwrap();
    std::fs::write(&path, format!("{head}\n{identity}{rest}")).unwrap();
}

/// Review minors 5-7 (D4, KU §3.2 / §4.4): the key-file check is skipped only for sectors
/// the sidecar marks unread (a UDF that cannot locate the file counts only if some sector is
/// unread), and only when the sidecar identifies the disc: a disc hash, a `vidfp` with a VID
/// in hand, or legacy key fingerprints a proven key of the set matches (rule 3).
#[test]
fn a_prescanned_image_is_identified_only_by_real_identity() {
    let fx = bd_image(&[Some(K1)], 1);
    let set = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    let inf = *fx.metadata.last().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let hash = libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash)
        .to_ascii_lowercase();
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mismatch = |r: crate::Result<()>| {
        matches!(
            r,
            Err(Error::MapfileInvalid {
                kind: "disc-mismatch"
            })
        )
    };

    // 5. The UDF anchor is gone (the key file cannot be located): refused unless the
    // sidecar marks some sector unread.
    let broken = staged_with(&fx, dir.path(), "noudf.iso");
    let mut b = std::fs::read(&broken).unwrap();
    b[256 * 2048..257 * 2048].fill(0);
    std::fs::write(&broken, b).unwrap();
    sidecar_text(&fx, &broken, &[], &format!("# freemkv-disc: {hash}\n"));
    assert!(
        mismatch(open_prescanned(&broken, [0; 16], &set)),
        "nothing unread"
    );
    sidecar_text(
        &fx,
        &broken,
        &[(256, 1)],
        &format!("# freemkv-disc: {hash}\n"),
    );
    open_prescanned(&broken, [0; 16], &set).expect("the anchor was never read");

    // 6. A vidfp alone identifies only with a VID in hand to compare.
    let vidfp = staged_with(&fx, dir.path(), "vidfp.iso");
    let fp = hex(&vid_fingerprint(&VID));
    sidecar_text(&fx, &vidfp, &[inf], &format!("# freemkv-vidfp: {fp}\n"));
    assert!(
        mismatch(open_prescanned(&vidfp, [0; 16], &set)),
        "no VID in hand"
    );
    open_prescanned(&vidfp, VID, &set).expect("the disc's VID matches the vidfp");

    // 7. A pre-1.8 sidecar identified only by legacy key fingerprints: rule 3.
    // The legacy prefix is assembled so only `parse_legacy_key_lines` spells it (EK9).
    let uk = |k: &[u8; 16]| format!("# {}uk: 1:{}\n", "freemkv-", hex(k));
    let legacy = staged_with(&fx, dir.path(), "legacy.iso");
    sidecar_text(&fx, &legacy, &[inf], &uk(&K1));
    open_prescanned(&legacy, [0; 16], &set).expect("a proven key matches");
    sidecar_text(&fx, &legacy, &[inf], &uk(&K2));
    assert!(
        mismatch(open_prescanned(&legacy, [0; 16], &set)),
        "no proven key matches"
    );
}

/// Review minor 8 (D5): only an unparseable sidecar is `MapfileInvalid`; one that cannot be
/// read at all (EIO, permissions; here a directory in its place) surfaces as an I/O error.
#[test]
fn an_unreadable_sidecar_is_an_io_error() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "d.iso");
    std::fs::create_dir(mapfile_path_for(&iso)).unwrap();
    let opts = OpenImageOptions::resolve(factory(&[(Answer::Keydb, &[K1])], &Calls::default()));
    let err = open_image_with(&ImageSource::Iso(iso), opts)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::IoError { .. }), "{err:?}");
}

/// D2 (review minor 9, behaviour): `remux_iso` runs its open, title pick and key top-up
/// under one Stop. A Stop that lands once the open's one request is made reaches everything
/// after it: the remux ends Halted and no further key request is made.
#[test]
fn a_stop_after_the_remux_open_asks_nothing_more() {
    struct StopAfter(Calls, usize);
    impl crate::Sink for StopAfter {
        fn should_cancel(&self) -> bool {
            self.0.len() >= self.1
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let calls = Calls::default();
    let job = crate::RemuxJob {
        iso: ImageSource::Iso(fx.write(dir.path(), "d.iso")),
        title: Some(1),
        streams: crate::StreamChoice::default(),
        target: dir.path().join("remux.mkv"),
        replace: false,
    };
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let err =
        crate::remux::remux_iso_sources(&job, f, &StopAfter(calls.clone(), 1), &crate::Halt::new())
            .unwrap_err();
    assert!(libfreemkv::is_halt(&err), "{err}");
    assert_eq!(calls.len(), 1, "the open's one request, then nothing");
    assert!(!job.target.exists());
}

// The title index that plays clip `clip` (`0000{clip}.mpls`).
fn title_of(fx: &Fx, clip: usize) -> usize {
    let name = format!("{clip:05}.mpls");
    fx.disc
        .titles
        .iter()
        .position(|t| t.playlist == name)
        .unwrap()
}

// `fx` written with a sidecar carrying the disc hash and the VID's fingerprint.
fn with_vidfp_sidecar(fx: &Fx, dir: &Path) -> std::path::PathBuf {
    let iso = fx.write(dir, "capture.iso");
    let total = fx.img.image.len() as u64;
    let mut map = Mapfile::create(&mapfile_path_for(&iso), total, "t").unwrap();
    map.record(0, total, crate::SectorStatus::Finished).unwrap();
    map.set_disc_hash(&fx.disc.aacs.as_ref().unwrap().disc_hash);
    map.set_vid_fingerprint(vid_fingerprint(&VID));
    map.flush().unwrap();
    iso
}

/// Review B-1: with the key service down, an image open over two Missing pieces (the larger
/// title's asked first, the other skipped as the source is dead) refuses with the outage
/// (E7028), never E7022 turned into E7034 ("insert the disc") by the vidfp sidecar.
#[test]
fn an_outage_is_never_e7034() {
    let fx = bd_image_sized(&[(Some(K1), 10), (Some(K2), 20)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = with_vidfp_sidecar(&fx, dir.path());
    let f = factory(&[(Answer::Unavailable, &[])], &Calls::default());
    let opts = OpenImageOptions {
        scope: titles(&[0, 1]),
        ..OpenImageOptions::resolve(f)
    };
    let err = open_image_with(&ImageSource::Iso(iso), opts)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(
        err.code(),
        libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE,
        "{err}"
    );
}

/// Review B-1 and minor 2 at the top-up: the outage is the top-up's refusal and is
/// remembered (a later call re-raises it); a first top-up's E7034 is re-raised too, never
/// a later E7022.
#[test]
fn a_top_up_outage_or_vid_need_is_remembered() {
    let fx = bd_image_sized(&[(Some(K1), 10), (Some(K2), 10), (Some(K2), 20)], 2);
    let (t0, t1, t2) = (title_of(&fx, 0), title_of(&fx, 1), title_of(&fx, 2));
    let dir = tempfile::tempdir().unwrap();
    let iso = with_vidfp_sidecar(&fx, dir.path());
    let src = ImageSource::Iso(iso);
    let open = |second: Answer| {
        let f = factory(
            &[(Answer::Keydb, &[K1]), (second, &[K2])],
            &Calls::default(),
        );
        let opts = OpenImageOptions {
            scope: titles(&[t0]),
            ..OpenImageOptions::resolve(f)
        };
        open_image_with(&src, opts).unwrap()
    };
    let codes = |opened: &crate::OpenedImage, plans: &[Vec<usize>]| {
        let sink = DoneCodes::default();
        for p in plans {
            mux_image_titles(
                opened,
                &MuxPlan::new(p.clone()),
                &mkv_dest(dir.path()),
                &sink,
            );
        }
        sink.0.into_inner().unwrap()
    };
    let e7028 = Some(libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE);
    let got = codes(&open(Answer::Unavailable), &[vec![t1, t2], vec![t1]]);
    assert_eq!(got, [e7028, e7028], "the outage, then remembered");
    let e7034 = Some(E_AACS_VID_NEEDS_DISC);
    let got = codes(&open(Answer::OnlineNeedsVid), &[vec![t1], vec![t1]]);
    assert_eq!(got, [e7034, e7034], "the VID need, then remembered");
}

/// Review minor 2: a first top-up stopped after its request went out spends the ask, but a
/// later call that nobody stopped is not reported as cancelled.
#[test]
fn a_remembered_stop_does_not_cancel_a_later_call() {
    struct StopAfter(Calls, usize);
    impl crate::Sink for StopAfter {
        fn should_cancel(&self) -> bool {
            self.0.len() >= self.1
        }
    }
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let (t0, t1) = (title_of(&fx, 0), title_of(&fx, 1));
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let calls = Calls::default();
    let f = factory(&[(Answer::Keydb, &[K1]), (Answer::Down, &[K2])], &calls);
    let opts = OpenImageOptions {
        scope: titles(&[t0]),
        ..OpenImageOptions::resolve(f)
    };
    let opened = open_image_with(&src, opts).unwrap();
    let plan = MuxPlan::new(vec![t1]);
    let stop = StopAfter(calls.clone(), 3);
    let out = mux_image_titles(&opened, &plan, &mkv_dest(dir.path()), &stop);
    assert_eq!(out, RipOutcome::Halted, "stopped during the retry");
    let asked = calls.len();
    let out = mux_image_titles(&opened, &plan, &mkv_dest(dir.path()), &crate::NoopSink);
    assert_ne!(out, RipOutcome::Halted, "nobody stopped this call");
    assert_eq!(calls.len(), asked, "the ask was spent");
}

/// KU-E1b item 3: a top-up that made a request remembers any refusal but Missing and a
/// Stop, keydb failures included: a keydb that went unreadable after a keyed open is E8002
/// for the top-up and for a later call, never a later E7022 that hides it.
#[test]
fn a_top_up_remembers_a_keydb_failure() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K2)], 2);
    let (t0, t1, t2) = (title_of(&fx, 0), title_of(&fx, 1), title_of(&fx, 2));
    let dir = tempfile::tempdir().unwrap();
    let src = ImageSource::Iso(fx.write(dir.path(), "d.iso"));
    let f = factory(&[(Answer::KeydbThenUnreadable, &[K1])], &Calls::default());
    let opts = OpenImageOptions {
        scope: titles(&[t0]),
        ..OpenImageOptions::resolve(f)
    };
    let opened = open_image_with(&src, opts).unwrap();
    let sink = DoneCodes::default();
    for plan in [vec![t1], vec![t2]] {
        mux_image_titles(&opened, &MuxPlan::new(plan), &mkv_dest(dir.path()), &sink);
    }
    let e8002 = Some(libfreemkv::error::E_KEYDB_INVALID);
    assert_eq!(*sink.0.lock().unwrap(), [e8002, e8002]);
}
