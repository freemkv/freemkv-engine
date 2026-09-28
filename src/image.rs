//! Disc images (`iso://` files and `dir://` extracted trees): the one open path every
//! front-end and the server use. The image is opened once and its keys resolved once, up
//! front, into the rip's in-memory key set (keys-upfront design, KU §3.2, §4.2); no mux
//! of it ever asks a key source.

use crate::keys::{KeyParams, key_source_factory, log_status};
use crate::recovery::mapfile::{DiscIdentity, Mapfile, check_identity, vid_fingerprint};
use libfreemkv::keys::{KeyScope, ResolveKeysOptions, ResolvedKeySet};
use libfreemkv::{Error, KeySourceFactory};
use std::path::{Path, PathBuf};

/// An image-level source: an ISO file or an extracted disc folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageSource {
    Iso(PathBuf),
    Dir(PathBuf),
}

impl ImageSource {
    /// A folder is [`ImageSource::Dir`], anything else [`ImageSource::Iso`].
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if path.is_dir() {
            Self::Dir(path)
        } else {
            Self::Iso(path)
        }
    }

    /// Parse an `iso://` or `dir://` URL; `None` for any other scheme.
    pub fn from_url(url: &str) -> Option<Self> {
        match libfreemkv::parse_url(url) {
            libfreemkv::StreamUrl::Iso { path } => Some(Self::Iso(path)),
            libfreemkv::StreamUrl::Dir { path } => Some(Self::Dir(path)),
            _ => None,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Iso(p) | Self::Dir(p) => p,
        }
    }

    /// The `iso://` / `dir://` URL the mux input opens.
    pub fn url(&self) -> String {
        match self {
            Self::Iso(p) => format!("iso://{}", p.display()),
            Self::Dir(p) => format!("dir://{}", p.display()),
        }
    }
}

/// Where an image's keys come from (KU §3.2, §12.1).
pub enum KeyInput {
    /// Resolve here, once, from these sources (CLI/GUI: [`key_source_factory`]).
    Resolve(KeySourceFactory),
    /// A set the caller already holds (the drive's up-front set, FMTS forensic keys
    /// included): used as-is, with no key-service call. Anything it does not cover refuses
    /// (E7022, or E7026 for Pending forensic keys).
    Known(ResolvedKeySet),
    /// Use the set, and ask the sources only for what it does not cover (e.g. the forensic
    /// anchor it left Pending), with its in-memory VID.
    Seeded(KeySourceFactory, ResolvedKeySet),
}

/// Options for [`open_image_with`].
pub struct OpenImageOptions {
    pub keys: KeyInput,
    /// An already-scanned disc (the drive scan) for an `iso://` image: it is NOT scanned
    /// (J14); a raw reader is opened and checked for capacity and disc hash. A `dir://`
    /// folder is always scanned, so a disc given with one is refused (E7013, caller bug).
    pub disc: Option<libfreemkv::Disc>,
    /// What the rip decrypts; `None` = `Titles([main])`.
    pub scope: Option<KeyScope>,
    /// An in-memory VID from a drive scan (KU §4.2), checked against the sidecar `vidfp`.
    pub vid: Option<[u8; 16]>,
    /// Stops the resolve and its retry waits.
    pub halt: Option<libfreemkv::Halt>,
}

impl OpenImageOptions {
    /// Resolve here from `sources`; every other option at its default.
    pub fn resolve(sources: KeySourceFactory) -> Self {
        Self::with(KeyInput::Resolve(sources))
    }

    /// Use `set` as-is, with no key-service call.
    pub fn known(set: ResolvedKeySet) -> Self {
        Self::with(KeyInput::Known(set))
    }

    /// Use `set`, asking `sources` only for what it lacks.
    pub fn seeded(sources: KeySourceFactory, set: ResolvedKeySet) -> Self {
        Self::with(KeyInput::Seeded(sources, set))
    }

    fn with(keys: KeyInput) -> Self {
        Self {
            keys,
            disc: None,
            scope: None,
            vid: None,
            halt: None,
        }
    }
}

/// An opened image with the rip's key set, ready to mux titles from.
pub struct OpenedImage {
    pub source: ImageSource,
    /// The disc: the image's scan, or the caller's pre-scanned disc (`prescanned`).
    pub disc: libfreemkv::Disc,
    /// A raw reader over the image (still open; e.g. for a whole-image copy).
    pub reader: Box<dyn libfreemkv::SectorSource>,
    /// The rip's up-front key set: memory only, never written (KU §2.1).
    pub keys: ResolvedKeySet,
    /// The sources, kept only for [`crate::mux_image_titles`]' one scope top-up before its
    /// first output byte; `None` for [`KeyInput::Known`].
    pub sources: Option<KeySourceFactory>,
    /// The disc came from the caller; the image was not scanned.
    pub prescanned: bool,
    /// Per-source walk of the key resolution, for a front-end to render.
    pub trace: libfreemkv::aacs::trace::ResolutionTrace,
    /// Label of the key source that keyed the first piece, if any.
    pub won: Option<String>,
    // The set after `keys_for`'s top-up, and whether `sources` were spent (asked at most once).
    top_up: std::sync::Mutex<(ResolvedKeySet, bool)>,
}

impl OpenedImage {
    /// The `input()` options for title `idx`: index, selection and the rip's set. For a
    /// `dir://` or URL mux; an `iso://` title muxes through [`crate::mux_image_titles`],
    /// which never rescans the image.
    pub fn input_options(
        &self,
        idx: usize,
        selection: libfreemkv::StreamSelection,
    ) -> libfreemkv::InputOptions {
        libfreemkv::InputOptions {
            title_index: Some(idx),
            selection,
            keys: Some(self.keys.clone()),
            ..Default::default()
        }
    }
}

/// Keyless scan of an image. Every front-end scans with the default
/// [`libfreemkv::ScanOptions`]: an image has no drive to hand credentials to.
pub fn scan_image(
    src: &ImageSource,
) -> crate::Result<(libfreemkv::Disc, Box<dyn libfreemkv::SectorSource>)> {
    let opts = libfreemkv::ScanOptions::default();
    match src {
        ImageSource::Iso(p) => libfreemkv::scan_iso(p, opts),
        ImageSource::Dir(p) => libfreemkv::scan_dir(p, opts),
    }
}

/// Open an image and resolve its keys from `keys` once (KU §3.2):
/// `open_image_with(src, OpenImageOptions::resolve(key_source_factory(keys)))`.
pub fn open_image(src: &ImageSource, keys: &KeyParams) -> crate::Result<OpenedImage> {
    open_image_with(src, OpenImageOptions::resolve(key_source_factory(keys)))
}

/// The one image-open API (KU §3.2): the disc (scanned, or the caller's), and the rip's
/// key set from `opts.keys`, before any output. The sidecar mapfile, if any, is read for
/// the §4.4 identity and the E7034 rule, and never written.
///
/// E7034 (J11): a piece in scope is Missing, no VID is in hand (`vid`, the disc's, the
/// seed's) and the sidecar has a `vidfp`, so only the disc can supply the key's VID.
pub fn open_image_with(src: &ImageSource, opts: OpenImageOptions) -> crate::Result<OpenedImage> {
    let OpenImageOptions {
        keys,
        disc,
        scope,
        vid,
        halt,
    } = opts;
    let sidecar = load_sidecar(src)?;
    let prescanned = disc.is_some();
    let (disc, mut reader) = match disc {
        Some(_) if matches!(src, ImageSource::Dir(_)) => {
            // J14 holds for `iso://` only: a folder muxes through `input()`, which scans it.
            tracing::error!(
                target: "freemkv::keys",
                "a pre-scanned disc needs an iso:// image; a dir:// folder is always scanned"
            );
            return Err(Error::DecryptFailed);
        }
        Some(disc) => {
            let mut reader = raw_reader(src)?;
            check_prescanned(&disc, reader.as_mut(), sidecar.as_ref())?;
            (disc, reader)
        }
        None => scan_image(src)?,
    };
    let scope = scope.unwrap_or_else(|| {
        KeyScope::Titles(crate::resolve_selection(
            &disc,
            &crate::Selection::MainMovie,
        ))
    });
    let seed = match &keys {
        KeyInput::Known(set) | KeyInput::Seeded(_, set) => Some(set),
        KeyInput::Resolve(_) => None,
    };
    let vid_in_hand = in_hand_vid_fingerprint(&disc, vid, seed);
    if let Some(map) = &sidecar {
        let identity = DiscIdentity {
            vidfp: vid_in_hand,
            proven: Vec::new(),
            ..DiscIdentity::of(&disc, None)
        };
        identity_ok(map, &identity)?;
    }
    let scope_log = format!("{scope:?}");
    let halt = halt.as_ref();
    let (set, sources, trace) = match keys {
        KeyInput::Known(set) => (known(&disc, set, &scope)?, None, Default::default()),
        KeyInput::Seeded(f, seed) if seed.is_for(&disc) && covers(&seed, &scope) => {
            (seed, Some(f), Default::default())
        }
        KeyInput::Seeded(f, seed) => {
            let r = resolve(&disc, reader.as_mut(), &scope, &f, Some(&seed), vid, halt);
            let r = r.map_err(|e| vid_needs_disc(e, vid_in_hand, sidecar.as_ref()))?;
            (r.keys, Some(f), r.trace)
        }
        KeyInput::Resolve(f) => {
            let r = resolve(&disc, reader.as_mut(), &scope, &f, None, vid, halt);
            let r = r.map_err(|e| vid_needs_disc(e, vid_in_hand, sidecar.as_ref()))?;
            (r.keys, Some(f), r.trace)
        }
    };
    if let Some(map) = &sidecar {
        let mut identity = DiscIdentity::of(&disc, Some(&set));
        identity.vidfp = identity.vidfp.or(vid_in_hand);
        identity_ok(map, &identity)?;
    }
    log_status(&set, &scope_log);
    Ok(OpenedImage {
        source: src.clone(),
        won: set.status().origin.map(str::to_string),
        top_up: std::sync::Mutex::new((set.clone(), false)),
        disc,
        reader,
        keys: set,
        sources,
        prescanned,
        trace,
    })
}

impl OpenedImage {
    /// The key set for muxing `titles` (KU §3.2): the held set when it covers them, else a
    /// resolve over their scope seeded with it, before the first output byte and stopped by
    /// `halt`. `sources` are asked at most once per opened image; later top-ups use only the
    /// keys already held (0 requests), and the result is remembered. Never re-scans the image.
    pub(crate) fn keys_for(
        &self,
        titles: &[usize],
        halt: Option<&libfreemkv::Halt>,
    ) -> crate::Result<ResolvedKeySet> {
        let scope = KeyScope::Titles(titles.to_vec());
        let mut held = self.top_up.lock().unwrap_or_else(|e| e.into_inner());
        if covers(&held.0, &scope) {
            return Ok(held.0.clone());
        }
        let Some(sources) = &self.sources else {
            return known(&self.disc, held.0.clone(), &scope);
        };
        // KU §2.1 invariant 4: the key service is never asked twice for this image.
        let no_sources: KeySourceFactory = std::sync::Arc::new(Vec::new);
        let sources = if held.1 { &no_sources } else { sources };
        held.1 = true;
        let mut reader = raw_reader(&self.source)?;
        let vid_in_hand = in_hand_vid_fingerprint(&self.disc, None, Some(&held.0));
        let seed = Some(&held.0);
        let r = resolve(
            &self.disc,
            reader.as_mut(),
            &scope,
            sources,
            seed,
            None,
            halt,
        );
        let sidecar = load_sidecar(&self.source)?;
        let keys = r
            .map_err(|e| vid_needs_disc(e, vid_in_hand, sidecar.as_ref()))?
            .keys;
        log_status(&keys, &format!("{scope:?}"));
        held.0 = keys.clone();
        Ok(keys)
    }
}

// A raw reader over the image, for a caller-scanned disc (no scan).
fn raw_reader(src: &ImageSource) -> crate::Result<Box<dyn libfreemkv::SectorSource>> {
    Ok(match src {
        ImageSource::Iso(p) => Box::new(libfreemkv::FileSectorSource::open(p)?),
        ImageSource::Dir(p) => Box::new(libfreemkv::DirImage::open(p)?),
    })
}

// KU §3.2: a pre-scanned disc must be this image's: capacity equal, and the disc hash equal
// ("checked when the image's `Unit_Key_RO.inf` is readable"). A key file the sidecar marks
// unread is left to its identity (§4.4); with no sidecar identity, unchecked is refused.
fn check_prescanned(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    sidecar: Option<&Mapfile>,
) -> crate::Result<()> {
    let have = reader.capacity_sectors();
    if have < disc.capacity_sectors {
        return Err(Error::ImageTruncated {
            have: have as u64 * 2048,
            want: disc.capacity_sectors as u64 * 2048,
        });
    }
    let norm = |h: &str| libfreemkv::hex::strip_hex_prefix(h).to_ascii_lowercase();
    let disc_hash = disc.aacs.as_ref().map(|a| norm(&a.disc_hash));
    let image_hash = if sidecar.is_some_and(|m| key_file_unread(m, reader)) {
        None
    } else {
        let inf = libfreemkv::read_filesystem(reader)
            .and_then(|fs| fs.read_file(reader, KEY_FILE))
            .ok();
        let hash = |inf: Vec<u8>| {
            libfreemkv::aacs::inf::disc_hash_hex(&libfreemkv::aacs::inf::disc_hash(&inf))
        };
        inf.map(|inf| norm(&hash(inf)))
    };
    let identified =
        sidecar.is_some_and(|m| m.disc_hash().is_some() || m.vid_fingerprint().is_some());
    let hash_ok = match (&image_hash, &disc_hash) {
        (Some(i), Some(d)) => i == d,
        (None, Some(_)) => identified,
        (_, None) => true,
    };
    if have != disc.capacity_sectors || !hash_ok {
        tracing::warn!(
            target: "freemkv::keys",
            image_sectors = have,
            disc_sectors = disc.capacity_sectors,
            hash_checked = image_hash.is_some(),
            "the image is not the pre-scanned disc's"
        );
        return Err(Error::MapfileInvalid {
            kind: "disc-mismatch",
        });
    }
    Ok(())
}

const KEY_FILE: &str = "/AACS/Unit_Key_RO.inf";

// Whether the sidecar marks any sector of `/AACS/Unit_Key_RO.inf` as not read (a sweep
// zero-fills those). An image whose UDF cannot locate the file counts as unread.
fn key_file_unread(map: &Mapfile, reader: &mut dyn libfreemkv::SectorSource) -> bool {
    use crate::SectorStatus as S;
    let extents =
        libfreemkv::read_filesystem(reader).and_then(|fs| fs.file_extents(reader, KEY_FILE));
    let Ok(extents) = extents else {
        return true;
    };
    let file: Vec<(u64, u64)> = extents
        .iter()
        .map(|&(s, n)| (s as u64 * 2048, n as u64 * 2048))
        .collect();
    let unread = map.ranges_with(&[S::NonTried, S::NonTrimmed, S::NonScraped, S::Unreadable]);
    !crate::recovery::mapfile::intersect(&unread, &file).is_empty()
}

// The image's sidecar mapfile, read-only. Absent is no identity; one that exists but does
// not load is refused (MapfileInvalid), never taken for "no identity" (judgement 6).
fn load_sidecar(src: &ImageSource) -> crate::Result<Option<Mapfile>> {
    let path = crate::mapfile_path_for(src.path());
    match Mapfile::load(&path) {
        Ok(map) => Ok(Some(map)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            tracing::warn!(target: "freemkv::keys", error = %e, "sidecar mapfile unreadable");
            Err(match Error::from(e) {
                invalid @ Error::MapfileInvalid { .. } => invalid,
                _ => Error::MapfileInvalid { kind: "sidecar" },
            })
        }
    }
}

// The fingerprint of the VID in hand: the caller's, the scanned disc's, else the seed's.
fn in_hand_vid_fingerprint(
    disc: &libfreemkv::Disc,
    vid: Option<[u8; 16]>,
    seed: Option<&ResolvedKeySet>,
) -> Option<[u8; 32]> {
    let real = |v: &[u8; 16]| *v != [0u8; 16];
    let disc_vid = disc.aacs.as_ref().map(|a| a.volume_id).filter(real);
    vid.filter(real)
        .or(disc_vid)
        .map(|v| vid_fingerprint(&v))
        .or_else(|| seed.and_then(|s| s.vid_fingerprint()))
}

fn identity_ok(map: &Mapfile, identity: &DiscIdentity) -> crate::Result<()> {
    check_identity(map, identity).map_err(|_| Error::MapfileInvalid {
        kind: "disc-mismatch",
    })
}

// `covers`, and no forensic keys left Pending.
fn covers(set: &ResolvedKeySet, scope: &KeyScope) -> bool {
    set.covers(scope) && !set.forensic_pending()
}

// KU §3.2 `Known`: the set as-is; what it does not cover refuses, with no request.
fn known(
    disc: &libfreemkv::Disc,
    set: ResolvedKeySet,
    scope: &KeyScope,
) -> crate::Result<ResolvedKeySet> {
    if !set.is_for(disc) {
        tracing::error!(target: "freemkv::keys", "a Known key set for another disc (caller bug)");
        return Err(Error::DecryptFailed);
    }
    if set.forensic_pending() {
        return Err(Error::FmtsKeyMissing);
    }
    if !set.covers(scope) {
        return Err(Error::NoDiscKey {
            disc_hash: disc.aacs.as_ref().map_or_else(String::new, |a| {
                libfreemkv::hex::strip_hex_prefix(&a.disc_hash).to_string()
            }),
        });
    }
    Ok(set)
}

pub(crate) fn resolve(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: &KeyScope,
    sources: &KeySourceFactory,
    seed: Option<&ResolvedKeySet>,
    vid: Option<[u8; 16]>,
    halt: Option<&libfreemkv::Halt>,
) -> crate::Result<libfreemkv::keys::KeyResolution> {
    let opts = ResolveKeysOptions {
        halt,
        seed,
        vid,
        vid_would_help: None,
    };
    ResolvedKeySet::resolve(disc, reader, scope.clone(), sources, opts)
}

/// KU §4.2 J11: a Missing piece (E7022/E7032) becomes E7034 when no VID is in hand and the
/// sidecar has a `vidfp`: the disc's VID could derive the key ("Kvu = AES-G(Km, IDv)",
/// KS-16), and it is read only from the disc (KS-29). Any other error is unchanged.
pub(crate) fn vid_needs_disc(
    e: Error,
    vid_in_hand: Option<[u8; 32]>,
    sidecar: Option<&Mapfile>,
) -> Error {
    let missing = matches!(e, Error::NoDiscKey { .. } | Error::WholeDiscKeyMissing);
    let disc_has_vid = sidecar.is_some_and(|m| m.vid_fingerprint().is_some());
    if missing && vid_in_hand.is_none() && disc_has_vid {
        tracing::info!(
            target: "freemkv::keys",
            code = libfreemkv::error::E_AACS_VID_NEEDS_DISC,
            "keys need the disc's Volume ID: insert the disc (its scan only)"
        );
        return Error::AacsVidNeedsDisc;
    }
    e
}

/// The numeric code of a libfreemkv error that went through `io::Error`, or
/// `None` when it carries none (an OS error, a front-end's own message).
pub fn error_code(e: &std::io::Error) -> Option<u16> {
    parse_error_code(&e.to_string()).map(|(code, _)| code)
}

/// Split a libfreemkv error's display form, `E<code>` or `E<code>: <data>`,
/// into the code and its (trimmed) data. A code wider than `u16` saturates:
/// it still names an error rather than reading as "no code".
pub fn parse_error_code(s: &str) -> Option<(u16, &str)> {
    let rest = s.strip_prefix('E')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let code = rest[..end]
        .parse::<u32>()
        .map_or(u16::MAX, |v| v.min(u16::MAX as u32) as u16);
    let data = rest[end..].strip_prefix(':').map_or("", str::trim);
    Some((code, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_code_splits_code_and_data() {
        assert_eq!(parse_error_code("E6009"), Some((6009, "")));
        assert_eq!(parse_error_code("E7022: abcdef"), Some((7022, "abcdef")));
        assert_eq!(parse_error_code("E5000: 13"), Some((5000, "13")));
        assert_eq!(parse_error_code("E99999"), Some((u16::MAX, "")));
        assert_eq!(parse_error_code("No drive found"), None);
        assert_eq!(parse_error_code("E"), None);
        assert_eq!(parse_error_code("Eabc"), None);
    }

    // Agrees with the library's own reading of every error it renders.
    #[test]
    fn error_code_matches_the_library() {
        for e in [
            libfreemkv::Error::MkvInvalid,
            libfreemkv::Error::Halted,
            libfreemkv::Error::MkvSourceInvalid,
        ] {
            let code = e.code();
            let io: std::io::Error = e.into();
            assert_eq!(error_code(&io), Some(code));
            assert_eq!(error_code(&io), libfreemkv::error_code(&io));
        }
        assert_eq!(error_code(&std::io::Error::other("disk full")), None);
    }

    #[test]
    fn image_source_urls_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let d = ImageSource::from_path(dir.path());
        assert!(matches!(d, ImageSource::Dir(_)));
        assert_eq!(ImageSource::from_url(&d.url()), Some(d.clone()));
        let iso = ImageSource::from_path(dir.path().join("x.iso"));
        assert!(matches!(iso, ImageSource::Iso(_)));
        assert_eq!(ImageSource::from_url(&iso.url()), Some(iso));
        assert_eq!(ImageSource::from_url("mkv:///tmp/a.mkv"), None);
    }

    #[test]
    fn open_image_reports_a_missing_image_as_a_scan_error() {
        let src = ImageSource::Iso("/nonexistent/freemkv/none.iso".into());
        assert!(open_image(&src, &KeyParams::default()).is_err());
    }
}
