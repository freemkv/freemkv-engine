//! Disc images (`iso://` files and `dir://` extracted trees): the one open path every
//! front-end and the server use. The image is opened once and its keys resolved once, up
//! front, into the rip's in-memory key set (keys-upfront design, KU §3.2, §4.2); no mux
//! of it ever asks a key source.

use crate::keys::{KeyParams, key_source_factory, log_status};
use crate::recovery::mapfile::{DiscIdentity, Mapfile, check_identity, vid_fingerprint};
use libfreemkv::keys::{KeyRing, KeyScope};
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
    Known(KeyRing),
    /// Use the set, and ask the sources only for what it does not cover (e.g. the forensic
    /// anchor it left Pending), with its in-memory VID.
    Seeded(KeySourceFactory, KeyRing),
}

impl KeyInput {
    // The set the caller holds, if any.
    fn held(&self) -> Option<&KeyRing> {
        match self {
            KeyInput::Known(set) | KeyInput::Seeded(_, set) => Some(set),
            KeyInput::Resolve(_) => None,
        }
    }
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
    pub fn known(set: KeyRing) -> Self {
        Self::with(KeyInput::Known(set))
    }

    /// Use `set`, asking `sources` only for what it lacks.
    pub fn seeded(sources: KeySourceFactory, set: KeyRing) -> Self {
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
    pub keys: KeyRing,
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
    top_up: std::sync::Mutex<TopUp>,
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

/// The one title of a container or stream source (`m2ts://`, `mkv://`, `mp4://`, …) as the
/// library reads it, for `info` and an app's open: its tracks and duration. `keys` opens a
/// loose clip; `halt` stops the open.
pub fn stream_info(
    url: &str,
    keys: Option<libfreemkv::keys::KeyRing>,
    halt: &libfreemkv::Halt,
) -> crate::Result<libfreemkv::DiscTitle> {
    let opts = libfreemkv::InputOptions {
        keys,
        ..Default::default()
    };
    let ctx = libfreemkv::Ctx::new(halt.clone());
    let stream = libfreemkv::input(url, &opts, &ctx)?;
    Ok(stream.info().clone())
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
    open_image_with_traced(src, opts).0
}

/// [`open_image_with`], also returning the resolve's per-source walk (e.g. "keydb, matched
/// disc, online") on success AND on a refusal: the operator's answer to "why no key". It
/// holds source labels, node outcomes and counts only, never a key, VID or MKB byte.
pub fn open_image_with_traced(
    src: &ImageSource,
    opts: OpenImageOptions,
) -> (
    crate::Result<OpenedImage>,
    libfreemkv::aacs::trace::ResolutionTrace,
) {
    let trace = std::sync::Mutex::new(Default::default());
    let r = open_image_inner(src, opts, &trace);
    let trace = trace.into_inner().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = &r {
        tracing::info!(target: "freemkv::keys", error = %e, walk = ?trace.keys, "image open refused");
    }
    (r, trace)
}

fn open_image_inner(
    src: &ImageSource,
    opts: OpenImageOptions,
    walk: &std::sync::Mutex<libfreemkv::aacs::trace::ResolutionTrace>,
) -> crate::Result<OpenedImage> {
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
            let held = keys.held();
            let in_hand = in_hand_vid_fingerprint(&disc, vid, held);
            let proven = held
                .map(|s| s.proven_key_fingerprints())
                .unwrap_or_default();
            let map = sidecar.as_ref();
            check_prescanned(&disc, reader.as_mut(), map, in_hand, &proven)?;
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
    let vid_in_hand = in_hand_vid_fingerprint(&disc, vid, keys.held());
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
        KeyInput::Seeded(f, seed)
            if seed.is_for(&disc.media_id()) && covers(&seed, &disc, &scope) =>
        {
            (seed, Some(f), Default::default())
        }
        KeyInput::Seeded(f, seed) => {
            let r = resolve(
                &disc,
                reader.as_mut(),
                &scope,
                &f,
                Some(&seed),
                vid,
                halt,
                walk,
            );
            let r = r.map_err(|e| vid_needs_disc(e, vid_in_hand, sidecar.as_ref()))?;
            (r.keys, Some(f), r.trace)
        }
        KeyInput::Resolve(f) => {
            let r = resolve(&disc, reader.as_mut(), &scope, &f, None, vid, halt, walk);
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
        top_up: std::sync::Mutex::new(TopUp::new(set.clone(), &scope)),
        disc,
        reader,
        keys: set,
        sources,
        prescanned,
        trace,
    })
}

// An opened image's key state after its top-ups: the held set and the titles it was
// resolved over, whether the one asking top-up made a request, and that top-up's failure.
struct TopUp {
    keys: KeyRing,
    titles: Vec<usize>,
    asked: bool,
    failure: Option<Remembered>,
}

impl TopUp {
    fn new(keys: KeyRing, scope: &KeyScope) -> Self {
        let titles = match scope {
            KeyScope::Titles(v) => v.clone(),
            _ => Vec::new(),
        };
        TopUp {
            keys,
            titles,
            asked: false,
            failure: None,
        }
    }
}

// A top-up's refusal kept to re-raise for a later Missing instead of E7022 (review B1,
// minor 2, KU-E1b): its code and the data of its `E<code>: <data>` form.
struct Remembered {
    code: u16,
    data: String,
    // An OS error's kind and errno: its `E5000: <errno|kind>` text alone rebuilds `Other`.
    io: Option<(std::io::ErrorKind, Option<i32>)>,
}

impl Remembered {
    // Any refusal of a top-up that made a request, but Missing (a later call's own verdict)
    // and a Stop (it belongs to the call that was stopped).
    fn of(e: &Error) -> Option<Self> {
        let missing = matches!(e, Error::NoDiscKey { .. } | Error::WholeDiscKeyMissing);
        if missing || matches!(e, Error::Halted) {
            return None;
        }
        let text = e.to_string();
        let data = crate::parse_error_code(&text)
            .map_or("", |(_, d)| d)
            .to_string();
        let io = match e {
            Error::IoError { source, .. } => Some((source.kind(), source.raw_os_error())),
            _ => None,
        };
        Some(Remembered {
            code: e.code(),
            data,
            io,
        })
    }

    // The refusal again, by code: the key service, E7034, the keydb (E8xxx) and the other
    // key refusals; any other kept as an I/O error carrying its `E<code>: <data>` text.
    fn rebuild(&self) -> Error {
        use libfreemkv::error as c;
        if let Some((kind, errno)) = self.io {
            let os = errno.map_or_else(
                || std::io::Error::from(kind),
                std::io::Error::from_raw_os_error,
            );
            return Error::from(os);
        }
        let d = self.data.clone();
        match self.code {
            c::E_KEY_SERVICE_UNAVAILABLE => Error::KeyServiceUnavailable,
            c::E_KEY_SERVICE_UNAUTHORIZED => Error::KeyServiceUnauthorized,
            c::E_KEY_SERVICE_RATE_LIMITED => Error::KeyServiceRateLimited,
            c::E_AACS_VID_NEEDS_DISC => Error::AacsVidNeedsDisc,
            c::E_FMTS_KEY_MISSING => Error::FmtsKeyMissing,
            c::E_DECRYPT_FAILED => Error::DecryptFailed,
            c::E_KEYDB_CONNECT => Error::KeydbConnect { host: d },
            c::E_KEYDB_HTTP => Error::KeydbHttp {
                status: d.parse().unwrap_or(0),
            },
            c::E_KEYDB_INVALID => Error::KeydbInvalid,
            c::E_KEYDB_WRITE => Error::KeydbWrite { path: d },
            c::E_KEYDB_PARSE => Error::KeydbParse,
            c::E_KEYDB_LOAD => Error::KeydbLoad { path: d },
            c::E_KEYDB_UNSUPPORTED_SCHEME => Error::KeydbUnsupportedScheme { scheme: d },
            c::E_KEYDB_TOO_MANY_REDIRECTS => Error::KeydbTooManyRedirects,
            code => Error::IoError {
                source: std::io::Error::other(format!("E{code}: {d}")),
            },
        }
    }
}

impl OpenedImage {
    /// The key set for muxing `titles` (KU §3.2): the held set when it covers them (a gap
    /// that is only forensic keys left Pending cannot be filled here: the held set), else a
    /// resolve over the held titles plus `titles`, seeded with the held set, before the first
    /// output byte and stopped by `halt`. At most one top-up per opened image asks the
    /// sources, counted once it made a request; later ones use only held keys (0 requests)
    /// and re-raise its source failure or E7034 rather than E7022 (never its Stop). The
    /// lock is held across the top-up's reads: one call at a time. Never re-scans the image.
    pub(crate) fn keys_for(
        &self,
        titles: &[usize],
        halt: Option<&libfreemkv::Halt>,
    ) -> crate::Result<KeyRing> {
        let mut held = self.top_up.lock().unwrap_or_else(|e| e.into_inner());
        if keys_scope(&held.keys, &self.disc, &KeyScope::Titles(titles.to_vec())) {
            return Ok(held.keys.clone());
        }
        let Some(sources) = &self.sources else {
            return known(
                &self.disc,
                held.keys.clone(),
                &KeyScope::Titles(titles.to_vec()),
            );
        };
        let mut union: Vec<usize> = held.titles.iter().chain(titles).copied().collect();
        union.sort_unstable();
        union.dedup();
        let scope = KeyScope::Titles(union.clone());
        // KU §2.1 invariant 4: at most one top-up per opened image asks the key sources.
        let ask = !held.asked;
        let no_sources: KeySourceFactory = std::sync::Arc::new(Vec::new);
        let factory = if ask { sources } else { &no_sources };
        let mut reader = raw_reader(&self.source)?;
        let vid_in_hand = in_hand_vid_fingerprint(&self.disc, None, Some(&held.keys));
        let walk = std::sync::Mutex::new(Default::default());
        let seed = Some(&held.keys);
        let r = resolve(
            &self.disc,
            reader.as_mut(),
            &scope,
            factory,
            seed,
            None,
            halt,
            &walk,
        );
        let walk = walk.into_inner().unwrap_or_else(|e| e.into_inner());
        let requested = ask && !walk.keys.is_empty();
        held.asked |= requested;
        match r {
            Ok(k) => {
                log_status(&k.keys, &format!("{scope:?}"));
                held.keys = k.keys.clone();
                held.titles = union;
                Ok(k.keys)
            }
            Err((e, help)) => {
                tracing::info!(target: "freemkv::keys", error = %e, walk = ?walk.keys, "top-up refused");
                let missing = matches!(e, Error::NoDiscKey { .. } | Error::WholeDiscKeyMissing);
                if let (false, true, Some(kept)) = (ask, missing, &held.failure) {
                    return Err(kept.rebuild());
                }
                // Only a Missing can become E7034 by the sidecar; any other refusal stays itself.
                let e = if missing {
                    // An unreadable sidecar leaves the refusal as it is, never replaces it.
                    let sidecar = load_sidecar(&self.source).ok().flatten();
                    vid_needs_disc((e, help), vid_in_hand, sidecar.as_ref())
                } else {
                    e
                };
                if requested {
                    held.failure = Remembered::of(&e);
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
impl OpenedImage {
    // An opened `iso` over `disc` holding `keys`, as `open_image_with` builds it (tests of
    // states a file image cannot produce, e.g. forensic keys left Pending at the drive).
    pub(crate) fn for_test(
        iso: &Path,
        disc: libfreemkv::Disc,
        keys: KeyRing,
        sources: Option<KeySourceFactory>,
        titles: Vec<usize>,
    ) -> Self {
        OpenedImage {
            source: ImageSource::Iso(iso.to_path_buf()),
            disc,
            reader: raw_reader(&ImageSource::Iso(iso.to_path_buf())).unwrap(),
            top_up: std::sync::Mutex::new(TopUp::new(keys.clone(), &KeyScope::Titles(titles))),
            keys,
            sources,
            prescanned: true,
            trace: Default::default(),
            won: None,
        }
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
// unread is left to the §4.4 identity, if the sidecar identifies the disc; else refused.
fn check_prescanned(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    sidecar: Option<&Mapfile>,
    vid_in_hand: Option<[u8; 32]>,
    proven: &[[u8; 8]],
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
    // One UDF parse serves both reads. An OS error reading the image is that error, never
    // "a different disc"; a key file the image does not hold (or zeroed metadata) is.
    let fs = not_os_error(libfreemkv::read_filesystem(reader))?;
    let unread = key_file_unread(sidecar, fs.as_ref(), reader)?;
    let image_hash = match fs {
        Some(fs) if !unread => {
            let inf = not_os_error(fs.read_file(reader, KEY_FILE))?;
            let hash = |inf: Vec<u8>| {
                libfreemkv::aacs::inf::disc_hash_hex(&libfreemkv::aacs::inf::disc_hash(&inf))
            };
            inf.map(|inf| norm(&hash(inf)))
        }
        _ => None,
    };
    let hash_ok = match (&image_hash, &disc_hash) {
        (Some(i), Some(d)) => i == d,
        (None, Some(_)) => unread && sidecar.is_some_and(|m| identifies(m, vid_in_hand, proven)),
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

// Whether `map` can stand in for the key-file hash (§4.4): a disc hash; a `vidfp` with a VID
// in hand to compare; or legacy key fingerprints one of the set's proven keys matches.
fn identifies(map: &Mapfile, vid_in_hand: Option<[u8; 32]>, proven: &[[u8; 8]]) -> bool {
    let legacy = map.legacy_key_fingerprints();
    map.disc_hash().is_some()
        || (map.vid_fingerprint().is_some() && vid_in_hand.is_some())
        || legacy.iter().any(|f| proven.contains(f))
}

const KEY_FILE: &str = "/AACS/Unit_Key_RO.inf";

// `Ok(None)` for a read that failed on the image's contents; an OS error stays an error.
fn not_os_error<T>(r: crate::Result<T>) -> crate::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e @ Error::IoError { .. }) => Err(e),
        Err(_) => Ok(None),
    }
}

// Whether the sidecar marks any sector of `/AACS/Unit_Key_RO.inf` not read (a sweep zero-
// fills those). If the UDF cannot locate the file, only when the sidecar marks any sector so.
fn key_file_unread(
    map: Option<&Mapfile>,
    fs: Option<&libfreemkv::UdfFs>,
    reader: &mut dyn libfreemkv::SectorSource,
) -> crate::Result<bool> {
    use crate::SectorStatus as S;
    let Some(map) = map else {
        return Ok(false);
    };
    let unread = map.ranges_with(&[S::NonTried, S::NonTrimmed, S::NonScraped, S::Unreadable]);
    let extents = match fs {
        Some(fs) => not_os_error(fs.file_extents(reader, KEY_FILE))?,
        None => None,
    };
    let Some(extents) = extents else {
        return Ok(!unread.is_empty());
    };
    let file: Vec<(u64, u64)> = extents
        .iter()
        .map(|&(s, n)| (s as u64 * 2048, n as u64 * 2048))
        .collect();
    Ok(!crate::recovery::mapfile::intersect(&unread, &file).is_empty())
}

// The image's sidecar mapfile, read-only. Absent is no identity; an unparseable one is
// refused (MapfileInvalid, judgement 6), and one that cannot be read is an I/O error.
fn load_sidecar(src: &ImageSource) -> crate::Result<Option<Mapfile>> {
    let path = crate::mapfile_path_for(src.path());
    match Mapfile::load(&path) {
        Ok(map) => Ok(Some(map)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            tracing::warn!(target: "freemkv::keys", error = %e, "sidecar mapfile unreadable");
            let parse = e.kind() == std::io::ErrorKind::InvalidData;
            Err(match Error::from(e) {
                invalid @ Error::MapfileInvalid { .. } => invalid,
                _ if parse => Error::MapfileInvalid { kind: "sidecar" },
                io => io,
            })
        }
    }
}

// The fingerprint of the VID in hand: the caller's, the scanned disc's, else the seed's.
fn in_hand_vid_fingerprint(
    disc: &libfreemkv::Disc,
    vid: Option<[u8; 16]>,
    seed: Option<&KeyRing>,
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

// Whether `set` keys `scope` of `disc`. A set with no AACS key (`none()`) never keys an
// AACS disc's titles, though `covers(scope)` (no disc) says so; a clear disc needs none.
fn keys_scope(set: &KeyRing, disc: &libfreemkv::Disc, scope: &KeyScope) -> bool {
    let aacs_needed = disc.aacs.is_some() && *scope != KeyScope::None;
    (set.is_aacs() || !aacs_needed) && set.covers(scope)
}

// `keys_scope`, and no forensic keys left Pending.
fn covers(set: &KeyRing, disc: &libfreemkv::Disc, scope: &KeyScope) -> bool {
    keys_scope(set, disc, scope) && !set.forensic_pending()
}

// KU §3.2 `Known`: the set as-is; what it does not cover refuses, with no request.
fn known(disc: &libfreemkv::Disc, set: KeyRing, scope: &KeyScope) -> crate::Result<KeyRing> {
    if !set.is_for(&disc.media_id()) {
        tracing::error!(target: "freemkv::keys", "a Known key set for another disc (caller bug)");
        return Err(Error::DecryptFailed);
    }
    if set.forensic_pending() {
        return Err(Error::FmtsKeyMissing);
    }
    if !keys_scope(&set, disc, scope) {
        return Err(Error::NoDiscKey {
            disc_hash: disc.aacs.as_ref().map_or_else(String::new, |a| {
                libfreemkv::hex::strip_hex_prefix(&a.disc_hash).to_string()
            }),
        });
    }
    Ok(set)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: &KeyScope,
    sources: &KeySourceFactory,
    seed: Option<&KeyRing>,
    vid: Option<[u8; 16]>,
    halt: Option<&libfreemkv::Halt>,
    walk: &std::sync::Mutex<libfreemkv::aacs::trace::ResolutionTrace>,
) -> Result<libfreemkv::keys::KeyResolution, (Error, bool)> {
    let req = crate::keys::Acquire {
        seed,
        vid,
        halt,
        progress: None,
    };
    crate::keys::acquire(disc, reader, scope, sources, req, walk)
}

/// KU §4.2 J11, J23: a Missing piece (E7022/E7032) becomes E7034 when no VID is in hand,
/// the sidecar has a `vidfp`, and `resolve` found that the VID would help (a Km path or a
/// VID-consuming source): only then could the disc's VID derive the key ("Kvu = AES-G(Km,
/// IDv)", KS-16), read only from the disc (KS-29). Any other error is unchanged.
pub(crate) fn vid_needs_disc(
    (e, vid_would_help): (Error, bool),
    vid_in_hand: Option<[u8; 32]>,
    sidecar: Option<&Mapfile>,
) -> Error {
    let missing = matches!(e, Error::NoDiscKey { .. } | Error::WholeDiscKeyMissing);
    let disc_has_vid = sidecar.is_some_and(|m| m.vid_fingerprint().is_some());
    if missing && vid_in_hand.is_none() && disc_has_vid && vid_would_help {
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
/// [`libfreemkv::error_code`], re-exported so a front-end needs only the engine.
pub fn error_code(e: &std::io::Error) -> Option<u16> {
    libfreemkv::error_code(e)
}

/// Split a libfreemkv error's display form, `E<code>` or `E<code>: <data>`,
/// into the code and its (trimmed) data. The code reads as [`error_code`]
/// reads it: one wider than `u16` is no libfreemkv code (`None`).
pub fn parse_error_code(s: &str) -> Option<(u16, &str)> {
    let rest = s.strip_prefix('E')?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let code = rest[..end].parse::<u16>().ok()?;
    let data = rest[end..].strip_prefix(':').map_or("", str::trim);
    Some((code, data))
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
