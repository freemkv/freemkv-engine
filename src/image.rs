//! Disc images (`iso://` files and `dir://` extracted trees): the one scan,
//! key-resolution and mid-mux key-fetch path every front-end opens an image
//! through.

use crate::keys::{KeyParams, key_source_factory, key_sources, won_source};
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

/// A scanned image with its AACS keys resolved, ready to mux titles from.
pub struct OpenedImage {
    pub source: ImageSource,
    /// The scanned disc, with any resolved unit keys banked on it.
    pub disc: libfreemkv::Disc,
    /// The sector reader the scan used (still open; e.g. for a whole-image copy).
    pub reader: Box<dyn libfreemkv::SectorSource>,
    /// On-decrypt-miss key fetch over the full [`KeyParams`] chain; `None` for a
    /// disc with no AACS inputs.
    pub key_fetch: Option<libfreemkv::sector::KeyFetch>,
    /// Per-source walk of the key resolution, for a front-end to render.
    pub trace: libfreemkv::aacs::trace::ResolutionTrace,
    /// Label of the key source that won, if any.
    pub won: Option<String>,
}

impl OpenedImage {
    /// The mux input for title `idx`: index, the banked unit keys, the stream
    /// selection and the mid-mux key fetch.
    pub fn input_options(
        &self,
        idx: usize,
        selection: libfreemkv::StreamSelection,
    ) -> libfreemkv::InputOptions {
        libfreemkv::InputOptions {
            title_index: Some(idx),
            unit_keys: self
                .disc
                .aacs
                .as_ref()
                .map(|a| a.unit_keys.clone())
                .unwrap_or_default(),
            key_fetch: self.key_fetch.clone(),
            selection,
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

/// Scan an image, resolve its AACS keys from `keys` (local-first, see
/// [`crate::key_sources`]) and build the mid-mux key fetch over the same chain.
/// A key that does not resolve is not an error here — the mux reports it.
pub fn open_image(src: &ImageSource, keys: &KeyParams) -> crate::Result<OpenedImage> {
    let (mut disc, mut reader) = scan_image(src)?;
    let resolved =
        libfreemkv::resolve_keys_for(reader.as_mut(), &mut disc, key_source_factory(keys));
    Ok(OpenedImage {
        source: src.clone(),
        won: won_source(&resolved.trace),
        disc,
        reader,
        key_fetch: resolved.key_fetch,
        trace: resolved.trace,
    })
}

/// The mid-mux key fetch for an image without scanning it: reads only the AACS
/// inputs and asks the full [`KeyParams`] chain on each decrypt miss. `None`
/// for a non-AACS image or when `keys` yields no source.
pub fn build_key_fetch(
    src: &ImageSource,
    keys: &KeyParams,
) -> Option<libfreemkv::sector::KeyFetch> {
    if key_sources(keys).is_empty() {
        return None;
    }
    let (inf, mkb, version) = match src {
        ImageSource::Iso(p) => libfreemkv::Disc::read_aacs_inputs(p).ok()?,
        ImageSource::Dir(p) => libfreemkv::Disc::read_aacs_inputs_from_dir(p).ok()?,
    };
    if inf.is_empty() {
        return None;
    }
    // An image has no drive handshake, so no Volume ID; the hash is what a keydb keys on.
    let hash = libfreemkv::aacs::inf::disc_hash(&inf);
    let inputs = libfreemkv::DiscInputs {
        disc_hash: libfreemkv::aacs::inf::disc_hash_hex(&hash),
        volume_id: [0u8; 16],
        version,
        mkb,
        unit_key_ro: inf,
        samples: Vec::new(),
        volume_label: None,
    };
    let keys = keys.clone();
    Some(libfreemkv::keysource::key_fetch(
        inputs,
        std::sync::Arc::new(move || key_sources(&keys)),
    ))
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
    fn build_key_fetch_needs_a_source_and_aacs_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let src = ImageSource::Dir(dir.path().to_path_buf());
        let none = KeyParams::default();
        assert!(build_key_fetch(&src, &none).is_none(), "no key source");
        let keydb = KeyParams {
            keydb_path: Some("keydb.cfg".into()),
            ..Default::default()
        };
        assert!(build_key_fetch(&src, &keydb).is_none(), "no AACS inputs");
    }

    // Index, banked keys, selection and fetch: each is invisible when dropped.
    #[test]
    fn input_options_carry_index_keys_selection_and_fetch() {
        struct NoRead;
        impl libfreemkv::SectorSource for NoRead {
            fn read_sectors(
                &mut self,
                _: u32,
                _: u16,
                _: &mut [u8],
                _: bool,
            ) -> libfreemkv::Result<usize> {
                unreachable!()
            }
            fn capacity_sectors(&self) -> u32 {
                0
            }
        }
        let keys = vec![(1u32, [9u8; 16])];
        let disc = libfreemkv::Disc {
            volume_id: String::new(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: Some(
                libfreemkv::test_util::aacs_state()
                    .unit_keys(keys.clone())
                    .build(),
            ),
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        let fetch =
            libfreemkv::keysource::key_fetch(disc.inputs().unwrap(), std::sync::Arc::new(Vec::new));
        let opened = OpenedImage {
            source: ImageSource::Iso("x.iso".into()),
            disc,
            reader: Box::new(NoRead),
            key_fetch: Some(fetch),
            trace: libfreemkv::aacs::trace::ResolutionTrace::new(),
            won: None,
        };
        let sel = libfreemkv::StreamSelection {
            audio: libfreemkv::PidFilter::Only(vec![4352]),
            subtitle: libfreemkv::PidFilter::Only(vec![]),
        };
        let input = opened.input_options(3, sel.clone());
        assert_eq!(input.title_index, Some(3));
        assert_eq!(input.unit_keys, keys);
        assert_eq!(input.selection, sel);
        assert!(input.key_fetch.is_some());
    }

    #[test]
    fn open_image_reports_a_missing_image_as_a_scan_error() {
        let src = ImageSource::Iso("/nonexistent/freemkv/none.iso".into());
        assert!(open_image(&src, &KeyParams::default()).is_err());
    }
}
