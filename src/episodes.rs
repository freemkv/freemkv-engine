//! TV episode-title selection ([`crate::Selection::Episodes`]).
//!
//! A TV disc lists every title: the episodes, usually a "play all" title whose
//! runtime is the SUM of the episodes, plus extras/menus and sometimes duplicate
//! angles. The episodes are the *episode cluster*: the group of similar-length
//! titles, so every episode is kept and the play-all/extras are dropped.

use libfreemkv::DiscTitle;
use std::collections::HashSet;

// Shortest title that counts as an episode (10 min); anything shorter is a menu or extra.
const EPISODE_MIN_SECS: f64 = 600.0;

/// The episode titles of a TV disc, in disc order: drops the "play all"
/// sum-title, extras/menus (far from the episode-length cluster), and
/// duplicate-content titles.
pub fn episode_titles(titles: &[DiscTitle]) -> Vec<usize> {
    let durations: Vec<f64> = titles.iter().map(|t| t.duration_secs).collect();
    dedup_by_content(titles, episode_cluster(&durations, EPISODE_MIN_SECS))
}

// Indices whose duration sits in the modal episode-length cluster.
fn episode_cluster(durations: &[f64], min_len: f64) -> Vec<usize> {
    let cands: Vec<usize> = durations
        .iter()
        .enumerate()
        .filter(|(_, d)| d.is_finite() && **d >= min_len)
        .map(|(i, _)| i)
        .collect();
    if cands.len() <= 1 {
        return cands;
    }
    let mut lens: Vec<f64> = cands.iter().map(|&i| durations[i]).collect();
    lens.sort_by(f64::total_cmp);
    let median = lens[lens.len() / 2];
    let tol = (median * 0.25).max(300.0);
    cands
        .into_iter()
        .filter(|&i| (durations[i] - median).abs() <= tol)
        .collect()
}

// Drop titles whose content duplicates an already-kept one: DVD angles or redundant
// playlists of the same programme (same first-extent start LBA and duration).
fn dedup_by_content(titles: &[DiscTitle], indices: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    indices
        .into_iter()
        .filter(|&i| {
            let t = &titles[i];
            let key = (
                t.extents.first().map(|e| e.start_lba).unwrap_or(0),
                t.duration_secs.round() as i64,
            );
            seen.insert(key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::disc::Extent;

    fn title(dur_secs: f64, start_lba: u32) -> DiscTitle {
        let mut t = DiscTitle::empty();
        t.duration_secs = dur_secs;
        t.size_bytes = (dur_secs as u64) * 1_000_000;
        t.extents = vec![Extent {
            start_lba,
            sector_count: 1000,
        }];
        t
    }

    #[test]
    fn picks_the_episode_cluster_and_drops_play_all_and_extras() {
        // 6 × ~44-min episodes, a ~264-min "play all" (their sum), a 2-min extra.
        let ep = 44.0 * 60.0;
        let mut titles = vec![title(ep * 6.0, 100)];
        for k in 0..6 {
            titles.push(title(ep + (k as f64), 1000 + k * 100));
        }
        titles.push(title(2.0 * 60.0, 50));
        assert_eq!(episode_titles(&titles), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn dedups_duplicate_angle_titles() {
        let ep = 44.0 * 60.0;
        let titles = vec![title(ep, 1000), title(ep, 1000), title(ep, 2000)];
        assert_eq!(episode_titles(&titles), vec![0, 2]);
    }

    #[test]
    fn single_qualifying_title_passes_through() {
        let titles = vec![title(90.0 * 60.0, 1000), title(60.0, 50)];
        assert_eq!(episode_titles(&titles), vec![0]);
    }

    #[test]
    fn none_qualify_yields_empty() {
        let titles = vec![title(120.0, 10), title(90.0, 20), title(f64::NAN, 30)];
        assert!(episode_titles(&titles).is_empty());
    }
}
