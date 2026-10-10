//! Conservative identities for alternate presentations within one scanned disc.

use libfreemkv::{ContentFormat, DiscTitle};

#[derive(PartialEq, Eq, Hash)]
pub(crate) enum Identity<'a> {
    Clips(ContentFormat, Vec<(&'a str, u32, u32)>),
    DvdCells(Vec<(u32, u32)>),
}

// Stream-file extents are not playback intervals. Only DVD extents carry
// authored cell ranges; other formats require valid ordered clip intervals.
pub(crate) fn identity(title: &DiscTitle) -> Option<Identity<'_>> {
    if !title.clips.is_empty() {
        return title
            .clips
            .iter()
            .map(|clip| {
                (!clip.clip_id.trim().is_empty() && clip.in_time < clip.out_time).then_some((
                    clip.clip_id.as_str(),
                    clip.in_time,
                    clip.out_time,
                ))
            })
            .collect::<Option<Vec<_>>>()
            .map(|clips| Identity::Clips(title.content_format, clips));
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
        .map(Identity::DvdCells)
}
