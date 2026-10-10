//! Episode selection requires authored roster evidence; otherwise review is required.

use libfreemkv::DiscTitle;

/// Executable episode indices, empty when evidence requires review.
/// Use `SelectionModel::select` for review candidates and the typed reason.
pub fn episode_titles(titles: &[DiscTitle]) -> Vec<usize> {
    crate::SelectionModel::from_titles(titles)
        .select(&crate::Selection::Episodes, &crate::StreamFilter::All)
        .indices
}

/// Legacy structural-containment hint, not menu reachability or an episode roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleRole {
    /// Plays two or more other titles back to back: their sectors lie wholly inside its own.
    PlayAll,
    /// One of the titles a play-all plays.
    Episode,
}

// Shortest title a play-all's part may be (10 min): a logo or recap inside it is not an episode.
const PART_MIN_SECS: f64 = 600.0;

/// Structural hints in title order; never authorizes automatic episode selection.
///
/// A play-all is read off the sectors, never the runtimes: every cell (extent) of other titles
/// at least [`PART_MIN_SECS`] long and strictly smaller than it is one of its own cells, the
/// same sector range exactly, and those titles begin
/// at two or more different sectors. Every title it holds that way is an episode, including the
/// same episode authored again with other audio (it begins at the same sector). Identical decoy
/// playlists hold the same sectors as each other, so none is smaller and none is a play-all.
pub fn title_roles(titles: &[DiscTitle]) -> Vec<Option<TitleRole>> {
    let mut roles = vec![None; titles.len()];
    for (l, long) in titles.iter().enumerate() {
        let size = sectors(long);
        let held: Vec<usize> = (0..titles.len())
            .filter(|&s| {
                let short = &titles[s];
                s != l
                    && short.duration_secs >= PART_MIN_SECS
                    && sectors(short) < size
                    && !short.extents.is_empty()
                    && short.extents.iter().all(|e| long.extents.contains(e))
            })
            .collect();
        let mut starts: Vec<u32> = held
            .iter()
            .filter_map(|&s| titles[s].extents.first().map(|e| e.start_lba))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        if starts.len() >= 2 {
            roles[l] = Some(TitleRole::PlayAll);
            for s in held {
                roles[s].get_or_insert(TitleRole::Episode);
            }
        }
    }
    roles
}

fn sectors(t: &DiscTitle) -> u64 {
    t.extents.iter().map(|e| u64::from(e.sector_count)).sum()
}

#[cfg(test)]
#[path = "episodes_tests.rs"]
mod tests;
