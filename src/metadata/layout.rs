//! Movies/TV library layout below a kind's root. Roots, disc images and moving stay with
//! each front end; this is pure path logic shared by the server mover and the desktop.

use super::{MediaKind, MediaMetadata, sanitize_display};
use std::path::{Path, PathBuf};

/// `Title (Year)`, sanitized; the year only when known. Movie and show folders use it.
pub fn title_with_year(meta: &MediaMetadata) -> String {
    let title = sanitize_display(&meta.title);
    match meta.year {
        0 => title,
        year => format!("{title} ({year})"),
    }
}

/// Where `leaf` goes under its kind's root:
/// - TV: `Show (Year)/Season NN/leaf` (season 1 when unknown);
/// - Movie, or kind unknown: `Title (Year)/Title (Year).ext`, keeping `leaf`'s extension
///   when it is plain ASCII letters and digits (`mkv` otherwise);
/// - no title: `leaf` alone.
///
/// Every segment is [`sanitize_display`]ed, so the result never leaves the root.
pub fn relative_path(meta: &MediaMetadata, leaf: &str) -> PathBuf {
    let safe_leaf = sanitize_display(leaf);
    if meta.title.trim().is_empty() {
        return PathBuf::from(safe_leaf);
    }
    let folder = title_with_year(meta);
    let mut path = PathBuf::from(&folder);
    match meta.kind {
        Some(MediaKind::Tv) => {
            path.push(format!("Season {:02}", meta.season.unwrap_or(1)));
            path.push(safe_leaf);
        }
        Some(MediaKind::Movie) | None => {
            let ext = Path::new(leaf)
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
                .unwrap_or("mkv");
            path.push(format!("{folder}.{ext}"));
        }
    }
    path
}
