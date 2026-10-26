//! Shared media identification and naming. Disc selection and timing stay in the disc pipeline.

use serde::{Deserialize, Serialize};

pub mod tmdb;

/// Serialises as "movie"/"tv". Strict here; persisted records read it through
/// `lenient`, where unknown, empty or missing is Movie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Tv,
    #[default]
    Movie,
}

/// `deserialize_with` for persisted media types: unknown or null is Movie.
pub fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<MediaKind, D::Error> {
    let raw: Option<String> = Deserialize::deserialize(d)?;
    Ok(MediaKind::parse(raw.as_deref().unwrap_or("")))
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Tv => "tv",
            MediaKind::Movie => "movie",
        }
    }

    /// Lenient parse: trims and ignores case; unknown or empty is Movie.
    pub fn parse(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("tv") {
            MediaKind::Tv
        } else {
            MediaKind::Movie
        }
    }

    pub fn is_tv(self) -> bool {
        self == MediaKind::Tv
    }
}

impl From<&str> for MediaKind {
    fn from(raw: &str) -> Self {
        MediaKind::parse(raw)
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for MediaKind {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMetadata {
    pub title: String,
    pub year: u16,
    pub tmdb_id: u64,
    pub season: Option<u16>,
    pub disc: Option<u16>,
    #[serde(default)]
    pub episode_start: Option<u16>,
    pub kind: Option<MediaKind>,
}

// A short, URL-FREE description of a ureq failure: these summaries reach
// syslog/autorip.jsonl/unauthenticated endpoints, so each variant maps to a fixed label instead
// of ever formatting the error.
pub fn transport_error_kind(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        ureq::Error::Io(io) => match io.raw_os_error() {
            // An OS-generated error (has an errno): its Display is the
            // syscall's own message, derived purely from the errno and never
            // the URL — surface it instead of the useless generic label.
            Some(_) => format!("io: {io}"),
            // No errno — a ureq/std-synthesized io error whose payload we do
            // NOT trust to be URL-free. Fall back to the ErrorKind's fixed
            // description, which is a constant string and never the URL.
            None => format!("io: {}", io.kind()),
        },
        ureq::Error::Timeout(_) => "timeout".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::TooManyRedirects => "too many redirects".to_string(),
        ureq::Error::Tls(_) => "tls error".to_string(),
        ureq::Error::BodyExceedsLimit(_) => "body exceeds limit".to_string(),
        _ => "transport error".to_string(),
    }
}
