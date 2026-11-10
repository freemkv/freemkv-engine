//! Shared media identification and naming. Disc selection and timing stay in the disc pipeline.

use serde::{Deserialize, Serialize};

pub mod layout;
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

impl MediaMetadata {
    /// Each field from the first source that has it: `operator` > `tmdb` > `labels` > none.
    /// Kind is TV when a label names a season; season and disc read the first label that
    /// names one, only for TV. A Movie (from either source) carries no season, disc or first
    /// episode, so an operator Movie overrides a season-marked label. Title and year never
    /// come from labels. Pass `tmdb: None` when there is no match.
    pub fn resolve(operator: &Self, tmdb: Option<&Self>, labels: &[&str]) -> Self {
        let matched = tmdb.cloned().unwrap_or_default();
        let label = |read: fn(&str) -> Option<u16>| labels.iter().find_map(|label| read(label));
        let kind = operator
            .kind
            .or(matched.kind)
            .or_else(|| label(tmdb::season_from_label).map(|_| MediaKind::Tv));
        let tv = kind == Some(MediaKind::Tv);
        let movie = kind == Some(MediaKind::Movie);
        let title = match operator.title.trim().is_empty() {
            true => matched.title.clone(),
            false => operator.title.clone(),
        };
        Self {
            title,
            year: if operator.year > 0 {
                operator.year
            } else {
                matched.year
            },
            tmdb_id: if operator.tmdb_id > 0 {
                operator.tmdb_id
            } else {
                matched.tmdb_id
            },
            season: (!movie)
                .then(|| operator.season.or(matched.season))
                .flatten()
                .or_else(|| tv.then(|| label(tmdb::season_from_label)).flatten()),
            disc: (!movie)
                .then(|| operator.disc.or(matched.disc))
                .flatten()
                .or_else(|| tv.then(|| label(tmdb::disc_from_label)).flatten()),
            episode_start: (!movie)
                .then(|| operator.episode_start.or(matched.episode_start))
                .flatten(),
            kind,
        }
    }
}

impl From<&tmdb::TmdbResult> for MediaMetadata {
    fn from(result: &tmdb::TmdbResult) -> Self {
        Self {
            title: result.title.clone(),
            year: result.year,
            tmdb_id: result.tmdb_id,
            kind: Some(result.media_type),
            ..Self::default()
        }
    }
}

/// Episodes `first..first+count` cannot all be numbered 1..=65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeRangeError;

impl std::fmt::Display for EpisodeRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("episode numbers out of range")
    }
}

impl std::error::Error for EpisodeRangeError {}

/// A disc's first episode number: the operator's `first`, otherwise disc N (default 1)
/// begins at `(N-1)*count+1`. Every one of the `count` episodes must be numbered 1..=65535.
pub fn episode_numbers(
    count: usize,
    disc: Option<u16>,
    first: Option<u16>,
) -> Result<u16, EpisodeRangeError> {
    let start = match first {
        Some(start) => start,
        None => usize::from(disc.unwrap_or(1).max(1) - 1)
            .checked_mul(count)
            .and_then(|offset| offset.checked_add(1))
            .and_then(|start| u16::try_from(start).ok())
            .ok_or(EpisodeRangeError)?,
    };
    if start == 0
        || count == 0
        || usize::from(start)
            .checked_add(count - 1)
            .is_none_or(|last| last > usize::from(u16::MAX))
    {
        return Err(EpisodeRangeError);
    }
    Ok(start)
}

/// The delivered episode name without extension: `Show S01E01 - Name`, or `Show S01E01`
/// when `name` is empty. Both parts are [`sanitize_display`]ed: TMDB names are untrusted.
pub fn episode_leaf(show: &str, season: u16, episode: u16, name: &str) -> String {
    let show = sanitize_display(show);
    match name.is_empty() {
        true => format!("{show} S{season:02}E{episode:02}"),
        false => format!(
            "{show} S{season:02}E{episode:02} - {}",
            sanitize_display(name)
        ),
    }
}

// Fallback when sanitizing leaves nothing usable.
const SAFE_FALLBACK: &str = "untitled";
// Filesystems cap a name at 255 bytes; callers append suffixes. Output is ASCII.
const MAX_SEGMENT_LEN: usize = 200;

/// One visible path segment: keeps ASCII letters, digits, space and `-_.'`, trimmed, at most
/// 200 bytes, without leading dots; never empty, `.` or `..` (those become `untitled`).
pub fn sanitize_display(name: &str) -> String {
    let filtered: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '\''))
        .collect();
    let stripped = filtered.trim().trim_start_matches('.');
    let stripped = &stripped[..stripped.len().min(MAX_SEGMENT_LEN)];
    if stripped.is_empty() || stripped.chars().all(|c| c == '.') {
        return SAFE_FALLBACK.to_string();
    }
    stripped.to_string()
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

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod resolve_tests;
