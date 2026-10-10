//! Conservative identities for alternate presentations within one scanned disc.

use libfreemkv::{ContentFormat, DiscTitle};

/// Exact ordered authored playback intervals, never duration/size similarity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PresentationIdentity {
    Clips(ContentFormat, Vec<(String, u32, u32)>),
    DvdCells(Vec<(u32, u32)>),
}

// Stream-file extents are not playback intervals. Only DVD extents carry
// authored cell ranges; other formats require valid ordered clip intervals.
pub(crate) fn identity(title: &DiscTitle) -> Option<PresentationIdentity> {
    if !title.clips.is_empty() {
        return title
            .clips
            .iter()
            .map(|clip| {
                (!clip.clip_id.trim().is_empty() && clip.in_time < clip.out_time).then_some((
                    clip.clip_id.clone(),
                    clip.in_time,
                    clip.out_time,
                ))
            })
            .collect::<Option<Vec<_>>>()
            .map(|clips| PresentationIdentity::Clips(title.content_format, clips));
    }
    if title.content_format != ContentFormat::DvdPs || title.extents.is_empty() {
        return None;
    }
    title
        .extents
        .iter()
        .map(|extent| {
            let last = extent.sector_count.checked_sub(1)?;
            Some((extent.start_lba, extent.start_lba.checked_add(last)?))
        })
        .collect::<Option<Vec<_>>>()
        .map(PresentationIdentity::DvdCells)
}
