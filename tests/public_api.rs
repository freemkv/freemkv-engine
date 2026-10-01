//! Every type a consumer can OBTAIN must also be NAMEABLE.
//!
//! A `pub` type inside a private module that is not re-exported can be bound by
//! inference but can never be written in a signature, a struct field, or a
//! turbofish. `RipMode` was the sharp case: `RipMode::Multi` was unreachable
//! through the public API before its re-export.
//!
//! These are compile-time assertions — if an export is dropped this file stops
//! compiling, which a doc test or a runtime check could not catch.

/// The multipass mode selector, and the builder that takes it.
#[test]
fn rip_mode_is_nameable_and_settable() {
    let mode: freemkv_engine::RipMode = freemkv_engine::RipMode::Multi;
    let job = freemkv_engine::Job::new("iso://in.iso", "mkv://out.mkv").with_mode(mode);
    assert!(matches!(job.mode, freemkv_engine::RipMode::Multi));

    // The field is settable directly too — the other half of the API.
    let mut j2 = freemkv_engine::Job::new("iso://in.iso", "mkv://out.mkv");
    j2.mode = freemkv_engine::RipMode::Single;
    assert!(matches!(j2.mode, freemkv_engine::RipMode::Single));
}

/// Types returned by exported functions must be nameable, or a front-end
/// cannot store the value it was just handed.
#[test]
fn returned_types_are_nameable() {
    fn _holds_key_status(_: freemkv_engine::KeyStatus) {}
    fn _holds_multipass_result(_: freemkv_engine::MultipassResult) {}
    fn _holds_pass_plan(_: freemkv_engine::PassPlan) {}
    fn _holds_rip_file(_: freemkv_engine::RipFile) {}
    fn _holds_unmatched(_: Vec<freemkv_engine::UnmatchedClass>) {}
    // `Preflight::Blocked` carries these; a UI must destructure it.
    fn _holds_reasons(_: &[freemkv_engine::Reason]) {}
    fn _holds_opened(_: freemkv_engine::OpenedImage) {}
    fn _holds_remux(_: freemkv_engine::RemuxReport) {}
    fn _holds_event(_: freemkv_engine::Event<'_>) {}
}

/// `resolve_stream_selection` is referenced by `StreamFilter`'s own doc as
/// `[crate::resolve_stream_selection]` and by the CHANGELOG, so the intra-doc
/// link was pointing at something no consumer could call.
#[test]
fn documented_free_functions_are_callable() {
    let _: fn(
        &libfreemkv::DiscTitle,
        &freemkv_engine::StreamFilter,
        &freemkv_engine::StreamFilter,
    ) -> Result<libfreemkv::StreamSelection, freemkv_engine::StreamSelError> =
        freemkv_engine::resolve_stream_selection;
}

// Forced subtitles are selectable independently of normal ones. `SubtitleFilter`
// lives in a private module, so without its re-export it would be bindable by
// inference but unwritable in a signature — the failure this file exists to catch.
#[test]
fn forced_subtitle_selection_is_nameable_and_constructible() {
    use freemkv_engine::SubtitleFilter;

    // Both sides named explicitly: the user's own example.
    let split: SubtitleFilter = SubtitleFilter::split(
        freemkv_engine::StreamFilter::Langs(vec!["de".into()]),
        freemkv_engine::StreamFilter::Langs(vec!["en".into()]),
    );
    fn _takes_split(_: &SubtitleFilter) {}
    _takes_split(&split);

    // And a plain filter must still convert, so existing callers are unbroken.
    let from_plain: SubtitleFilter = freemkv_engine::StreamFilter::All.into();
    _takes_split(&from_plain);

    // The forced-aware resolver is reachable by name.
    let _f: fn(
        &libfreemkv::DiscTitle,
        &freemkv_engine::StreamFilter,
        &SubtitleFilter,
    ) -> Result<freemkv_engine::StreamSelection, freemkv_engine::StreamSelError> =
        freemkv_engine::resolve_stream_selection_forced;
}

// KU-E0, keys-upfront-design §8.2: "`Default` for `SweepOptions`, `PatchOptions`".
// Per spec; do not change without a spec citation proving otherwise. It is the
// `..Default` prep ST-E1 and the library server (§12.1) build their literals on.
#[test]
fn sweep_and_patch_options_are_default() {
    fn _is_default<T: Default>() {}
    _is_default::<freemkv_engine::SweepOptions<'static>>();
    _is_default::<freemkv_engine::PatchOptions<'static>>();

    // The §12.1 raw-sweep shape, minus `keys` (a KU-E1 field): a default names
    // nothing to decrypt, resume, or persist. Keys are memory only (§2.1).
    let s = freemkv_engine::SweepOptions {
        decrypt: false,
        ..Default::default()
    };
    assert!(!s.decrypt && !s.resume && !s.skip_on_error);
    assert!(s.batch_sectors.is_none());
    assert!(s.progress.is_none() && s.halt.is_none());
    assert!(s.keys.is_none());

    let p = freemkv_engine::PatchOptions {
        decrypt: false,
        ..Default::default()
    };
    assert!(!p.decrypt && !p.full_recovery && !p.reverse);
    assert!(p.block_sectors.is_none() && p.wedged_threshold == 0);
    assert!(p.progress.is_none() && p.halt.is_none() && p.keys.is_none());
}

/// KU-E1 (KU §3.2, §12.2): the engine's key front door, nameable where the server and
/// both shells call it.
#[test]
fn key_front_door_is_nameable() {
    use libfreemkv::keys::{KeyRing, KeyScope};
    type ResolveForRip = fn(
        &libfreemkv::Disc,
        &mut dyn libfreemkv::SectorSource,
        KeyScope,
        &libfreemkv::KeySourceFactory,
        Option<&KeyRing>,
        Option<&libfreemkv::Halt>,
    ) -> Result<KeyRing, libfreemkv::Error>;
    let _: ResolveForRip = freemkv_engine::keys::resolve_for_rip;
    let _: fn(&libfreemkv::Disc, &[usize], freemkv_engine::keys::RipOutput) -> KeyScope =
        freemkv_engine::keys::rip_scope;
    let _: fn(&libfreemkv::Disc, &KeyRing) -> libfreemkv::keys::DecryptStatus =
        freemkv_engine::keys::key_status;
    let _: fn(&freemkv_engine::KeyParams) -> libfreemkv::KeySourceFactory =
        freemkv_engine::key_source_factory;
    let _: fn(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error> = freemkv_engine::open_scan;
}

/// KU-E1 (KU §3.2, §12.1): the one image-open API the server calls, and what it returns.
#[test]
fn image_front_door_is_nameable() {
    use freemkv_engine::{ImageSource, KeyInput, OpenImageOptions, OpenedImage};
    use libfreemkv::keys::{KeyRing, KeyScope};
    let _: fn(&ImageSource, OpenImageOptions) -> Result<OpenedImage, libfreemkv::Error> =
        freemkv_engine::open_image_with;
    let _: fn(&ImageSource, &freemkv_engine::KeyParams) -> Result<OpenedImage, libfreemkv::Error> =
        freemkv_engine::open_image;
    let f: libfreemkv::KeySourceFactory = std::sync::Arc::new(Vec::new);
    let set = KeyRing::none();
    for keys in [
        KeyInput::Resolve(f.clone()),
        KeyInput::Known(set.clone()),
        KeyInput::Seeded(f, set),
    ] {
        let opts = OpenImageOptions {
            keys,
            disc: None,
            scope: Some(KeyScope::Titles(vec![0])),
            vid: None,
            halt: Some(libfreemkv::Halt::new()),
        };
        drop(opts);
    }
    fn _fields(o: &OpenedImage) -> (&KeyRing, Option<&libfreemkv::KeySourceFactory>, bool) {
        (&o.keys, o.sources.as_ref(), o.prescanned)
    }
}

/// KU §4.1: a consumer computes and verifies a mapfile `vidfp` with the engine's one
/// fingerprint, `SHA-256("freemkv-vid-fp-v1" ‖ VID)` (a fingerprint, never the VID). The
/// vector is computed independently (Python `hashlib`).
#[test]
fn vid_fingerprint_is_public_and_pinned() {
    let fp: [u8; 32] = freemkv_engine::vid_fingerprint(&[0x5A; 16]);
    let hex: String = fp.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "298468a3589bccbcca31adc371cab3c0d1c7914c04a7036575476442ee849700"
    );
}
