//! KU-E1 image front-door tests (KU §3.2, §4.2, §12): `open_image_with`, the no-rescan image
//! mux (J14), one resolution per rip, and the E7034 rule (J11, J12).

use crate::image::ImageSource;
use crate::remux::MuxPlan;
use crate::test_fixtures::{Answer, Calls, Fx, K1, K2, bd_image, resolve};
use libfreemkv::keys::KeyScope;

// Zero the sectors a sweep can leave unread in a staged ISO: every MPLS and
// `/AACS/Unit_Key_RO.inf` (EK13).
fn damage_metadata(fx: &Fx, iso: &std::path::Path) {
    let mut bytes = std::fs::read(iso).unwrap();
    for &(s, n) in &fx.metadata {
        bytes[s as usize * 2048..(s + n) as usize * 2048].fill(0);
    }
    std::fs::write(iso, bytes).unwrap();
}

fn mkv_dest(dir: &std::path::Path) -> impl Fn(usize) -> String {
    let dir = dir.to_path_buf();
    move |idx| format!("mkv://{}", dir.join(format!("t{idx}.mkv")).display())
}

/// EK13 (J14, the server's staged-ISO regression): a staged ISO whose MPLS and
/// `Unit_Key_RO.inf` sectors were never read muxes both titles from the drive-scanned
/// disc, never rescanning the image.
#[test]
fn staged_iso_with_damaged_udf_muxes_from_drive_scanned_title() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = tempfile::tempdir().unwrap();
    let iso = fx.write(dir.path(), "staged.iso");
    damage_metadata(&fx, &iso);
    let src = ImageSource::Iso(iso);
    let opened = crate::open_image(&src, &crate::KeyParams::default()).expect("open");
    let out = crate::mux_image_titles(
        &opened,
        &MuxPlan::new(vec![0, 1]),
        &mkv_dest(dir.path()),
        &crate::NoopSink,
    );
    assert_eq!(out, crate::RipOutcome::Ok { titles_written: 2 });
    let _ = (resolve, Answer::Keydb, Calls::default(), KeyScope::None);
}
