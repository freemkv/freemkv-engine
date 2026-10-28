use super::finish_with;
use crate::recovery::mapfile::{Mapfile, SectorStatus, mapfile_path_for};
use crate::test_fixtures::{Answer, Calls, Damage, Drive, Fx, K1, mirrored_bd_image, resolve};
use crate::{Held, Job, MultipassOpts, PassHost, Plan, Report, RunWith};
use libfreemkv::keys::{KeyRing, KeyScope};
use libfreemkv::test_util::MetadataMirrorLayout;
use libfreemkv::whole_disc::{DecryptedNames, NamesKept};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type BeforePatch = Box<dyn FnMut(&Drive, &Path) + Send>;

// A drive whose damage the test switches as the first patch pass starts; `stop` is
// cancelled at the first read after that.
struct Host {
    drive: Drive,
    iso: PathBuf,
    before_patch: BeforePatch,
    stop: Option<libfreemkv::Halt>,
    patching: bool,
}

impl PassHost for Host {
    fn reader(&mut self) -> &mut dyn libfreemkv::SectorSource {
        self
    }
    fn before_patch(&mut self, _pass: u32) {
        self.patching = true;
        (self.before_patch)(&self.drive, &self.iso)
    }
}

impl libfreemkv::SectorSource for Host {
    fn capacity_sectors(&self) -> u32 {
        self.drive.capacity_sectors()
    }
    fn read_sectors(
        &mut self,
        lba: u32,
        count: u16,
        buf: &mut [u8],
        r: bool,
    ) -> libfreemkv::Result<usize> {
        if let Some(stop) = self.stop.as_ref().filter(|_| self.patching) {
            stop.cancel();
        }
        self.drive.read_sectors(lba, count, buf, r)
    }
}

fn fixture() -> (Fx, MetadataMirrorLayout, KeyRing) {
    let (fx, layout) = mirrored_bd_image(&[Some(K1)], 1);
    let keys = resolve(
        &fx,
        KeyScope::WholeDisc,
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();
    (fx, layout, keys)
}

fn passes() -> MultipassOpts {
    MultipassOpts {
        max_passes: 2,
        is_iso_output: true,
        ..Default::default()
    }
}

// One recovery of `fx` into `iso` through `run_with`: `damage` from the start,
// `before_patch` between passes; `tweak` adjusts the plan and run, `scope` stages it.
fn try_rip(
    fx: &Fx,
    keys: &KeyRing,
    iso: &Path,
    damage: Damage,
    before_patch: impl FnMut(&Drive, &Path) + Send + 'static,
    tweak: impl FnOnce(&mut Plan, &mut RunWith<'_>),
    scope: Option<&[(u32, u32)]>,
) -> crate::Result<Option<DecryptedNames>> {
    let drive = Drive::new(&fx.img.image);
    drive.set(damage);
    let mut plan = Plan {
        source: "iso:///src.iso".into(),
        dest: format!("iso://{}", iso.display()),
        ..Plan::default()
    };
    let mut probe = RunWith::default();
    tweak(&mut plan, &mut probe);
    let mut host = Host {
        drive,
        iso: iso.to_path_buf(),
        before_patch: Box::new(before_patch),
        stop: probe.halt.clone(),
        patching: false,
    };
    let with = RunWith {
        keys: if plan.raw { None } else { Some(keys.clone()) },
        held: Some(Held::Host {
            disc: &fx.disc,
            host: &mut host,
        }),
        passes: Some(passes()),
        scope,
        halt: probe.halt,
        ..RunWith::default()
    };
    match crate::run_with(&plan, with, &crate::NoopSink)? {
        Report::Image { names, .. } => Ok(names),
        other => panic!("an image report, got {other:?}"),
    }
}

fn rip_with(
    fx: &Fx,
    keys: &KeyRing,
    iso: &Path,
    damage: Damage,
    tweak: impl FnOnce(&mut Plan, &mut RunWith<'_>),
    scope: Option<&[(u32, u32)]>,
) -> Option<DecryptedNames> {
    try_rip(fx, keys, iso, damage, |_, _| {}, tweak, scope).expect("the recovery runs")
}

fn rip(
    fx: &Fx,
    keys: &KeyRing,
    iso: &Path,
    damage: Damage,
    before_patch: impl FnMut(&Drive, &Path) + Send + 'static,
) -> Option<DecryptedNames> {
    try_rip(fx, keys, iso, damage, before_patch, |_, _| {}, None).expect("the recovery runs")
}

fn sector(iso: &Path, lba: u32) -> Vec<u8> {
    let bytes = std::fs::read(iso).unwrap();
    bytes[lba as usize * 2048..(lba as usize + 1) * 2048].to_vec()
}

fn source_sector(fx: &Fx, lba: u32) -> &[u8] {
    &fx.img.image[lba as usize * 2048..(lba as usize + 1) * 2048]
}

fn lists(iso: &Path, name: &str) -> bool {
    let mut src = libfreemkv::FileSectorSource::open(iso).unwrap();
    let fs = libfreemkv::read_filesystem(&mut src).unwrap();
    fs.file_extents(&mut src, &format!("/AACS/{name}")).is_ok()
}

fn renamed(iso: &Path) -> bool {
    lists(iso, "Unit_Key_RO.bak") && !lists(iso, "Unit_Key_RO.inf")
}

fn status(iso: &Path, lba: u32) -> SectorStatus {
    let map = Mapfile::load(&mapfile_path_for(iso)).unwrap();
    let at = lba as u64 * 2048;
    map.entries()
        .iter()
        .find(|e| e.pos <= at && at < e.pos + e.size)
        .unwrap()
        .status
}

const RENAMED: Option<DecryptedNames> = Some(DecryptedNames::Renamed {
    mirror_sectors_restored: 0,
});

// F1: the sweep cannot read the mirror's /AACS sector, the patch pass can. Renaming per pass
// wrote main with `.inf` and the mirror with `.bak`; one rename at the end keeps them equal.
#[test]
fn a_mirror_recovered_by_a_later_pass_matches_main() {
    let (fx, layout, keys) = fixture();
    let mirror = layout.mirror_of(layout.aacs_dir);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let names = rip(
        &fx,
        &keys,
        &iso,
        Damage::Range(mirror, mirror + 1),
        |d, _| d.set(Damage::None),
    );
    assert!(sector(&iso, layout.aacs_dir) == sector(&iso, mirror));
    assert!(renamed(&iso));
    assert_eq!(names, RENAMED);
}

// A mirror sector no pass reads is rebuilt from main, and the mapfile marks it read.
#[test]
fn a_mirror_lost_in_every_pass_is_restored_from_main() {
    let (fx, layout, keys) = fixture();
    let mirror = layout.mirror_of(layout.aacs_dir);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let names = rip(
        &fx,
        &keys,
        &iso,
        Damage::Range(mirror, mirror + 1),
        |_, _| {},
    );
    assert_eq!(
        names,
        Some(DecryptedNames::Renamed {
            mirror_sectors_restored: 1
        })
    );
    assert!(sector(&iso, layout.aacs_dir) == sector(&iso, mirror));
    assert!(renamed(&iso));
    assert_eq!(status(&iso, mirror), SectorStatus::Finished);
}

// An unreadable main /AACS directory keeps the names and writes nothing; a resume that
// reads it renames.
#[test]
fn a_damaged_main_directory_keeps_the_names_until_a_resume_reads_it() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let names = rip(&fx, &keys, &iso, Damage::Range(main, main + 1), |_, _| {});
    assert_eq!(
        names,
        Some(DecryptedNames::Kept {
            cause: NamesKept::Damaged
        })
    );
    assert!(
        sector(&iso, mirror) == source_sector(&fx, mirror),
        "mirror untouched"
    );
    assert!(
        sector(&iso, main).iter().all(|&b| b == 0),
        "main never written"
    );
    assert_ne!(status(&iso, main), SectorStatus::Finished);
    assert_ne!(status(&iso, mirror), SectorStatus::NonTried);

    let names = rip(&fx, &keys, &iso, Damage::None, |_, _| {});
    assert_eq!(names, RENAMED);
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
}

// A stopped or wedged recovery is left exactly as its passes wrote it.
#[test]
fn a_stopped_recovery_is_not_renamed() {
    stopped_at_the_patch_pass(false);
}

#[test]
fn a_wedged_recovery_is_not_renamed() {
    stopped_at_the_patch_pass(true);
}

fn stopped_at_the_patch_pass(wedge: bool) {
    let (fx, layout, keys) = fixture();
    let mirror = layout.mirror_of(layout.aacs_dir);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let halt = libfreemkv::Halt::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let snap = Arc::clone(&seen);
    let ran = try_rip(
        &fx,
        &keys,
        &iso,
        Damage::Range(mirror, mirror + 1),
        move |d, iso| {
            *snap.lock().unwrap() = std::fs::read(iso).unwrap();
            if wedge {
                d.set(Damage::Transport(mirror, mirror + 1));
            }
        },
        |_, w| w.halt = (!wedge).then_some(halt),
        None,
    );
    assert_eq!(ran.unwrap(), None, "wedge={wedge}");
    assert!(
        std::fs::read(&iso).unwrap() == *seen.lock().unwrap(),
        "wedge={wedge}"
    );
    assert!(lists(&iso, "Unit_Key_RO.inf"), "wedge={wedge}");
}

// A raw copy and a scoped staging image keep the disc's own names.
#[test]
fn raw_and_scoped_copies_are_not_renamed() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.iso");
    let names = rip_with(&fx, &keys, &raw, Damage::None, |p, _| p.raw = true, None);
    assert_eq!(names, None);
    assert!(std::fs::read(&raw).unwrap() == fx.img.image);

    let staged = dir.path().join("staged.iso");
    let all = [(0, fx.disc.image_read_sectors().unwrap())];
    let names = rip_with(&fx, &keys, &staged, Damage::None, |_, _| {}, Some(&all));
    assert_eq!(names, None);
    assert!(sector(&staged, layout.aacs_dir) == source_sector(&fx, layout.aacs_dir));
    assert!(
        Mapfile::load(&mapfile_path_for(&staged))
            .unwrap()
            .scope()
            .is_some()
    );
}

// The passes alone, as a crash before the rename leaves them: names as read, a full mapfile.
fn recovered(fx: &Fx, keys: &KeyRing, iso: &Path) {
    let mut job = Job::new("iso:///src.iso", format!("iso://{}", iso.display()));
    job.keys = Some(keys.clone());
    let r = crate::multipass_rip(
        &fx.disc,
        &mut Drive::new(&fx.img.image),
        iso,
        &job,
        &passes(),
        &crate::NoopSink,
    )
    .unwrap();
    assert!(r.complete && lists(iso, "Unit_Key_RO.inf"), "{r:?}");
}

// A write that fails after the journal leaves the touched sectors unread; a resume reads
// the originals again and renames.
#[test]
fn a_failed_write_is_journaled_for_the_next_resume() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso);
    let names = finish_with(&iso, true, &mut |_, _, _| {
        Err(std::io::Error::other("crash"))
    });
    assert_eq!(
        names,
        Some(DecryptedNames::Kept {
            cause: NamesKept::Unexpected
        })
    );
    assert_eq!(status(&iso, main), SectorStatus::NonTried);
    assert_eq!(status(&iso, mirror), SectorStatus::NonTried);

    let names = rip(&fx, &keys, &iso, Damage::None, |_, _| {});
    assert_eq!(names, RENAMED);
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
}

// A write that does not take is reported, not claimed.
#[test]
fn a_lost_write_is_reported_unexpected() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso);
    let names = finish_with(&iso, true, &mut |file, lba, bytes| {
        use std::io::{Seek, SeekFrom, Write};
        if lba == layout.aacs_dir {
            return Ok(());
        }
        file.seek(SeekFrom::Start(lba as u64 * 2048))?;
        file.write_all(bytes)
    });
    assert_eq!(
        names,
        Some(DecryptedNames::Kept {
            cause: NamesKept::Unexpected
        })
    );
}
