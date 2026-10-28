use super::{At, Settled, finish_image_names, finish_with, names_undo_path};
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
    let mut probe = RunWith {
        passes: Some(passes()),
        ..RunWith::default()
    };
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
        scope,
        ..probe
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

// The passes alone, as a run ends before its rename: names as read, a full mapfile.
fn recovered(fx: &Fx, keys: &KeyRing, iso: &Path, damage: Damage) {
    let mut job = Job::new("iso:///src.iso", format!("iso://{}", iso.display()));
    job.keys = Some(keys.clone());
    let drive = Drive::new(&fx.img.image);
    drive.set(damage);
    crate::multipass_rip(
        &fx.disc,
        &mut drive.clone(),
        iso,
        &job,
        &passes(),
        &crate::NoopSink,
    )
    .unwrap();
    assert!(lists(iso, "Unit_Key_RO.inf"));
}

const UNEXPECTED: Option<DecryptedNames> = Some(DecryptedNames::Kept {
    cause: NamesKept::Unexpected,
});

fn write_at(iso: &Path, lba: u32, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(iso).unwrap();
    f.seek(SeekFrom::Start(lba as u64 * 2048)).unwrap();
    f.write_all(bytes).unwrap();
}

// The image and mapfile bytes, to show a failed rename changed neither.
fn state(iso: &Path) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(iso).unwrap(),
        std::fs::read(mapfile_path_for(iso)).unwrap(),
    )
}

// A rename that cannot open the image changes nothing and leaves no undo.
#[test]
fn an_image_that_cannot_be_opened_is_left_unchanged() {
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let before = state(&iso);
    let names = finish_with(&iso, true, &mut |at| match at {
        At::Open => Err(std::io::Error::other("sharing violation")),
        _ => Ok(true),
    });
    assert_eq!(names, UNEXPECTED);
    assert!(state(&iso) == before);
    assert!(!names_undo_path(&iso).exists());
}

// A write that fails part way restores every sector already written.
#[test]
fn a_failed_write_restores_the_originals() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let before = state(&iso);
    let mirror = layout.mirror_of(layout.aacs_dir);
    let names = finish_with(&iso, true, &mut |at| match at {
        At::Write(lba) if lba == mirror => Err(std::io::Error::other("EIO")),
        _ => Ok(true),
    });
    assert_eq!(names, UNEXPECTED);
    assert!(state(&iso) == before);
    assert!(!names_undo_path(&iso).exists());
}

// A write that does not take (the mirror's) is caught by reading back and rolled back.
#[test]
fn a_lost_mirror_write_is_detected_and_rolled_back() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let before = state(&iso);
    let mirror = layout.mirror_of(layout.aacs_dir);
    let names = finish_with(&iso, true, &mut |at| Ok(at != At::Write(mirror)));
    assert_eq!(names, UNEXPECTED);
    assert!(state(&iso) == before);
    assert!(sector(&iso, layout.aacs_dir) == sector(&iso, mirror));
    assert!(lists(&iso, "Unit_Key_RO.inf"));
}

// A crash part way through the writes leaves the undo; the next rename rolls it back first.
#[test]
fn a_crash_mid_write_is_rolled_back_by_the_next_rename() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finish_with(&iso, true, &mut |at| {
            assert_ne!(at, At::Write(mirror), "crash");
            Ok(true)
        })
    }));
    assert!(crashed.is_err());
    assert!(names_undo_path(&iso).exists());
    assert!(
        sector(&iso, main) != sector(&iso, mirror),
        "torn by the crash"
    );

    assert_eq!(finish_image_names(&iso, true), RENAMED);
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
    assert!(!names_undo_path(&iso).exists());
}

// A crash after the commit, before the undo is dropped: the rebuilt mirror is already marked
// read, so the next run's passes leave it, and the leftover undo is dropped.
#[test]
fn a_committed_rename_survives_a_crash_and_the_next_passes() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::Range(mirror, mirror + 1));
    crash_at(&iso, At::Committed);
    assert_eq!(status(&iso, mirror), SectorStatus::Finished);

    let names = rip(&fx, &keys, &iso, Damage::None, |_, _| {});
    assert_eq!(names, RENAMED);
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
    assert_eq!(status(&iso, mirror), SectorStatus::Finished);
    assert!(!names_undo_path(&iso).exists());
}

// A copy that starts the image over (here raw over decrypted) drops a leftover undo instead
// of restoring it into the new image later.
#[test]
fn a_fresh_copy_drops_a_leftover_undo() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let len = std::fs::metadata(&iso).unwrap().len();
    let stale = super::Record {
        lba: layout.aacs_dir,
        fill: false,
        status: SectorStatus::Finished,
        planned: 0,
        bytes: Box::new([0xAA; 2048]),
    };
    super::write_undo(&names_undo_path(&iso), len, &[stale]).unwrap();
    let names = rip_with(&fx, &keys, &iso, Damage::None, |p, _| p.raw = true, None);
    assert_eq!(names, None);
    assert!(!names_undo_path(&iso).exists());
    assert!(std::fs::read(&iso).unwrap() == fx.img.image);
}

// A mapfile without the raw/decrypted header can't vouch for the image: kept, not renamed.
#[test]
fn a_mapfile_without_the_decrypted_header_is_kept_unexpected() {
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let mf = mapfile_path_for(&iso);
    let text = std::fs::read_to_string(&mf).unwrap();
    let legacy: String = text
        .lines()
        .filter(|l| !l.starts_with("# freemkv-raw:"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&mf, legacy).unwrap();
    let before = state(&iso);
    assert_eq!(finish_image_names(&iso, true), UNEXPECTED);
    assert_eq!(finish_image_names(&iso, false), None);
    assert!(state(&iso) == before);
}

// A one-pass resume whose patch wedges is left as its pass wrote it, as a recovery's is.
#[test]
fn a_one_pass_copy_that_wedges_is_not_renamed() {
    let (fx, layout, keys) = fixture();
    let (clip, _) = fx.clip(0);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    // A recovery stopped at its patch pass: the clip still retryable, the names as read.
    let stopped = try_rip(
        &fx,
        &keys,
        &iso,
        Damage::Range(clip, clip + 1),
        |_, _| {},
        |_, w| w.halt = Some(libfreemkv::Halt::new()),
        None,
    );
    assert_eq!(stopped.unwrap(), None);
    assert_eq!(status(&iso, layout.aacs_dir), SectorStatus::Finished);
    let one_pass = |p: &mut Plan, w: &mut RunWith<'_>| {
        p.multipass = true;
        w.passes = None;
    };
    let names = rip_with(
        &fx,
        &keys,
        &iso,
        Damage::Transport(clip, clip + 1),
        one_pass,
        None,
    );
    assert_eq!(names, None);
    assert!(lists(&iso, "Unit_Key_RO.inf"), "never renamed");
}

// A stalled consumer abandoned at a real teardown (`finish_bounded_disowning`, as sweep and
// patch call it) blocks the rename for any later run in the process until it lets go.
#[test]
fn a_live_abandoned_consumer_blocks_the_rename() {
    use crate::recovery::stall_fixtures::Gate;
    use libfreemkv::io::pipeline::{Flow, Pipeline, Sink, WRITE_THROUGH_DEPTH};
    struct Stalled {
        _map: Mapfile,
        gate: Gate,
        entered: Arc<Mutex<bool>>,
    }
    impl Sink<u32> for Stalled {
        type Output = ();
        fn apply(&mut self, _: u32) -> Result<Flow, libfreemkv::Error> {
            *self.entered.lock().unwrap() = true;
            self.gate.wait();
            Ok(Flow::Continue)
        }
        fn close(self) -> Result<(), libfreemkv::Error> {
            Ok(())
        }
    }
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let mf = mapfile_path_for(&iso);
    // The consumer names the mapfile through a symlinked folder; the rename by its real path.
    #[cfg(unix)]
    let named = {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        link.join(mf.file_name().unwrap())
    };
    #[cfg(not(unix))]
    let named = mf.clone();
    let map = Mapfile::load(&named).unwrap();
    let disown = map.disown_handle();
    let (gate, entered) = (Gate::shut(), Arc::new(Mutex::new(false)));
    let sink = Stalled {
        _map: map,
        gate: gate.clone(),
        entered: Arc::clone(&entered),
    };
    let pipe = Pipeline::<u32, ()>::spawn(WRITE_THROUGH_DEPTH, sink).unwrap();
    let halt = libfreemkv::Halt::new();
    crate::recovery::send_bounded(&pipe, 1, &halt).unwrap();
    while !*entered.lock().unwrap() {
        std::thread::yield_now();
    }
    halt.cancel();
    assert!(crate::recovery::finish_bounded_disowning(pipe, &halt, &disown).is_err());
    drop(disown);

    let before = state(&iso);
    let names = || {
        let eh = crate::EngineHalt::new(&libfreemkv::Halt::new(), None);
        let copy = crate::CopyResult::new(fx.disc.capacity_bytes, 1, 0, 0, 0, false);
        crate::plan::image_names(&copy, None, &eh, &iso, &fx.disc)
    };
    assert_eq!(names(), UNEXPECTED);
    assert!(state(&iso) == before);
    gate.open();
    let start = std::time::Instant::now();
    while crate::recovery::abandoned_writer_alive(&mf) {
        assert!(start.elapsed().as_secs() < 30, "the consumer never let go");
        std::thread::yield_now();
    }
    assert_eq!(names(), RENAMED);
}

// A rename that crashes at `at`, leaving its undo.
fn crash_at(iso: &Path, at: At) {
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finish_with(iso, true, &mut |now| {
            assert_ne!(now, at, "crash");
            Ok(true)
        })
    }));
    assert!(crashed.is_err());
    assert!(names_undo_path(iso).exists());
}

// A blanked mirror sector the mapfile marks finished is rebuilt; a crash before the commit
// rolls back both copies, so the next rename starts from a consistent image.
#[test]
fn a_crash_before_commit_rolls_back_a_blank_finished_mirror() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    write_at(&iso, mirror, &[0; 2048]);
    crash_at(&iso, At::Marked);
    assert_eq!(
        finish_image_names(&iso, true),
        Some(DecryptedNames::Renamed {
            mirror_sectors_restored: 1
        })
    );
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
}

// Move the mapfile aside and put a directory in its place: it loads with an error, not NotFound.
fn break_mapfile(iso: &Path) -> PathBuf {
    let mf = mapfile_path_for(iso);
    let aside = mf.with_extension("aside");
    std::fs::rename(&mf, &aside).unwrap();
    std::fs::create_dir(&mf).unwrap();
    aside
}

fn mend_mapfile(iso: &Path, aside: &Path) {
    let mf = mapfile_path_for(iso);
    std::fs::remove_dir(&mf).unwrap();
    std::fs::rename(aside, &mf).unwrap();
}

// A pending rollback of main sectors needs no mapfile: an unreadable one does not drop it.
#[test]
fn a_pending_undo_rolls_back_with_an_unreadable_mapfile() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let original = sector(&iso, layout.aacs_dir);
    crash_at(&iso, At::Marked);
    let aside = break_mapfile(&iso);
    assert_eq!(super::settle_names_undo(&iso), Settled::Done);
    mend_mapfile(&iso, &aside);
    assert!(sector(&iso, layout.aacs_dir) == original);
    assert!(lists(&iso, "Unit_Key_RO.inf"));
    assert!(!names_undo_path(&iso).exists());
}

// A rebuilt mirror sector's status can only be reset through the mapfile: while it can't be
// read the bytes are back but the undo is kept, and once it can the sector is unfinished again.
#[test]
fn a_fill_rollback_waits_for_a_readable_mapfile() {
    let (fx, layout, keys) = fixture();
    let mirror = layout.mirror_of(layout.aacs_dir);
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::Range(mirror, mirror + 1));
    let was = status(&iso, mirror);
    assert_ne!(was, SectorStatus::Finished);
    crash_at(&iso, At::Marked);
    assert_eq!(status(&iso, mirror), SectorStatus::Finished, "marked");
    let aside = break_mapfile(&iso);
    assert_eq!(super::settle_names_undo(&iso), Settled::StatusOwed);
    assert!(names_undo_path(&iso).exists());
    assert!(lists(&iso, "Unit_Key_RO.inf"), "bytes rolled back");
    mend_mapfile(&iso, &aside);
    assert_eq!(super::settle_names_undo(&iso), Settled::Done);
    assert_eq!(status(&iso, mirror), was);
    assert!(lists(&iso, "Unit_Key_RO.inf"));
}

fn corrupt_undo(iso: &Path) {
    let undo = names_undo_path(iso);
    let mut bytes = std::fs::read(&undo).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&undo, bytes).unwrap();
}

// A corrupt undo is kept, never guessed at: the rename leaves the image alone, and a
// resumed copy still runs.
#[test]
fn a_corrupt_undo_is_kept_and_the_copy_still_runs() {
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    crash_at(&iso, At::Marked);
    corrupt_undo(&iso);
    let before = state(&iso);
    assert_eq!(finish_image_names(&iso, true), UNEXPECTED);
    assert_eq!(finish_image_names(&iso, false), None);
    assert!(state(&iso) == before);
    assert_eq!(rip(&fx, &keys, &iso, Damage::None, |_, _| {}), UNEXPECTED);
    assert!(names_undo_path(&iso).exists());
}

// The image is verified and its fills marked before the commit: a commit or delete that
// fails is not a failed rename, and the leftover undo settles without undoing it.
#[test]
fn a_failed_commit_still_reports_the_verified_rename() {
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let names = finish_with(&iso, true, &mut |at| match at {
        At::Committed => Err(std::io::Error::other("fsync")),
        _ => Ok(true),
    });
    assert_eq!(names, RENAMED);
    assert!(names_undo_path(&iso).exists());
    assert_eq!(super::settle_names_undo(&iso), Settled::Done);
    assert!(renamed(&iso));
    assert!(!names_undo_path(&iso).exists());
}

// The rename's last crash left a pending undo with the rebuilt mirror still unfinished.
fn torn_pending(fx: &Fx, keys: &KeyRing, iso: &Path, mirror: u32) {
    recovered(fx, keys, iso, Damage::Range(mirror, mirror + 1));
    crash_at(iso, At::Marked);
    let mut map = Mapfile::load(&mapfile_path_for(iso)).unwrap();
    map.record(mirror as u64 * 2048, 2048, SectorStatus::NonTrimmed)
        .unwrap();
    map.flush().unwrap();
}

// An undo whose bytes could not be rolled back keeps its sectors out of the passes: no pass
// reads the original mirror back under a renamed main. The next settle rolls it back.
#[test]
fn passes_skip_an_undo_they_could_not_roll_back() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    torn_pending(&fx, &keys, &iso, mirror);
    super::FAIL_RESTORE.with(|f| f.set(true));
    let names = rip(&fx, &keys, &iso, Damage::None, |_, _| {});
    super::FAIL_RESTORE.with(|f| f.set(false));
    assert_eq!(names, UNEXPECTED);
    assert!(sector(&iso, main) == sector(&iso, mirror), "never torn");
    assert_eq!(
        finish_image_names(&iso, true),
        Some(DecryptedNames::Renamed {
            mirror_sectors_restored: 1
        })
    );
    assert!(sector(&iso, main) == sector(&iso, mirror));
    assert!(renamed(&iso));
}

// An unreadable undo hides its sectors, so a pass may tear the image: the rename then never
// claims it renamed, even once the undo is gone.
#[test]
fn a_torn_image_behind_a_corrupt_undo_is_never_reported_renamed() {
    let (fx, layout, keys) = fixture();
    let (main, mirror) = (layout.aacs_dir, layout.mirror_of(layout.aacs_dir));
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    torn_pending(&fx, &keys, &iso, mirror);
    corrupt_undo(&iso);
    assert_eq!(rip(&fx, &keys, &iso, Damage::None, |_, _| {}), UNEXPECTED);
    assert!(
        sector(&iso, main) != sector(&iso, mirror),
        "the pass read the mirror back"
    );
    std::fs::remove_file(names_undo_path(&iso)).unwrap();
    assert_eq!(finish_image_names(&iso, true), UNEXPECTED);
}

// An undo that matches neither what it replaced nor what it wrote is another image's: dropped,
// never restored.
#[test]
fn an_undo_for_another_image_is_dropped_unapplied() {
    let (fx, layout, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    recovered(&fx, &keys, &iso, Damage::None);
    let len = std::fs::metadata(&iso).unwrap().len();
    let other = super::Record {
        lba: layout.aacs_dir,
        fill: false,
        status: SectorStatus::Finished,
        planned: super::fnv(&[0xBB; 2048]),
        bytes: Box::new([0xAA; 2048]),
    };
    super::write_undo(&names_undo_path(&iso), len, &[other]).unwrap();
    let before = state(&iso);
    assert_eq!(super::settle_names_undo(&iso), Settled::Done);
    assert!(state(&iso) == before);
    assert!(!names_undo_path(&iso).exists());
}

// A fresh copy that cannot delete a stale undo still runs; the rename then leaves the image.
#[test]
fn a_fresh_copy_runs_past_an_undo_it_cannot_delete() {
    let (fx, _, keys) = fixture();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("d.iso");
    let undo = names_undo_path(&iso);
    std::fs::create_dir(&undo).unwrap();
    std::fs::write(undo.join("x"), b"x").unwrap();
    let names = rip_with(
        &fx,
        &keys,
        &iso,
        Damage::None,
        |_, w| {
            w.passes = Some(MultipassOpts {
                resume_sweep: Some(false),
                ..passes()
            })
        },
        None,
    );
    assert_eq!(names, UNEXPECTED);
    assert!(lists(&iso, "Unit_Key_RO.inf"));
}
