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
// Titles within this fraction of an episode's length (but at least the floor) are the
// same episode length.
const EPISODE_TOLERANCE_FRAC: f64 = 0.25;
const EPISODE_TOLERANCE_MIN_SECS: f64 = 300.0;
// A play-all's runtime equals its episodes' sum within this fraction (or floor).
const PLAY_ALL_SUM_FRAC: f64 = 0.01;
const PLAY_ALL_SUM_MIN_SECS: f64 = 15.0;

/// The episode titles of a TV disc, in disc order: drops the "play all"
/// sum-title, extras/menus (far from the episode-length cluster), and
/// duplicate-content titles.
pub fn episode_titles(titles: &[DiscTitle]) -> Vec<usize> {
    dedup_by_program(titles, episode_cluster(titles, EPISODE_MIN_SECS))
}

/// A title's role, stated only where the disc's own structure proves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleRole {
    /// Plays two or more other titles back to back: their sectors lie wholly inside its own.
    PlayAll,
    /// One of the titles a play-all plays.
    Episode,
}

// Shortest title a play-all's part may be (10 min): a logo or recap inside it is not an episode.
const PART_MIN_SECS: f64 = 600.0;

/// The proven role of every title, in `titles` order; `None` where the disc proves nothing.
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

fn same_length(center: f64, d: f64) -> bool {
    (d - center).abs() <= (center * EPISODE_TOLERANCE_FRAC).max(EPISODE_TOLERANCE_MIN_SECS)
}

// Indices in the modal episode-length cluster: the largest group of similar lengths. Among
// equal-size groups the longest wins unless it is a play-all of a shorter, disjoint one.
fn episode_cluster(titles: &[DiscTitle], min_len: f64) -> Vec<usize> {
    let dur = |i: usize| titles[i].duration_secs;
    let cands: Vec<usize> = (0..titles.len())
        .filter(|&i| dur(i).is_finite() && dur(i) >= min_len)
        .collect();
    let group = |c: usize| -> Vec<usize> {
        cands
            .iter()
            .copied()
            .filter(|&i| same_length(dur(c), dur(i)))
            .collect()
    };
    let Some(most) = cands.iter().map(|&c| group(c).len()).max() else {
        return cands;
    };
    let mut tied: Vec<usize> = cands
        .iter()
        .copied()
        .filter(|&c| group(c).len() == most)
        .collect();
    tied.sort_by(|&a, &b| dur(b).total_cmp(&dur(a)));
    let pick = tied
        .iter()
        .copied()
        .find(|&l| {
            let long = group(l);
            !tied
                .iter()
                .filter(|&&s| dur(s) < dur(l))
                .map(|&s| group(s))
                .filter(|s| s.iter().all(|i| !long.contains(i)))
                .any(|s| plays_all(titles, &long, &s))
        })
        .or(tied.last().copied());
    pick.map(group).unwrap_or_default()
}

// Every title of `long` plays the `short` group: its extents hold a `short` title's start,
// or its runtime is their sum (a playlist's runtime is its clips', to within seconds).
fn plays_all(titles: &[DiscTitle], long: &[usize], short: &[usize]) -> bool {
    let sum: f64 = short.iter().map(|&i| titles[i].duration_secs).sum();
    long.iter().all(|&l| {
        let t = &titles[l];
        let holds = short.iter().any(|&s| {
            titles[s].extents.first().is_some_and(|f| {
                t.extents.iter().any(|e| {
                    (e.start_lba..e.start_lba.saturating_add(e.sector_count)).contains(&f.start_lba)
                })
            })
        });
        holds
            || (t.duration_secs - sum).abs() <= (sum * PLAY_ALL_SUM_FRAC).max(PLAY_ALL_SUM_MIN_SECS)
    })
}

// Drop alternate playlists for the same authored program. Physical extents are not a
// playlist identity: alternate STN selections may read the same ordered clips through
// different extents. The ordered clip program is the stable identity we want for
// episode fan-out: alternate playlists can carry different in/out points and slightly
// different durations while still referring to the same authored episode. Synthetic/
// minimal titles without clip metadata use physical extents as a conservative fallback.
fn dedup_by_program(titles: &[DiscTitle], indices: Vec<usize>) -> Vec<usize> {
    let mut seen_programs = HashSet::new();
    let mut seen_extents = HashSet::new();
    indices
        .into_iter()
        .filter(|&i| {
            let t = &titles[i];
            if !t.clips.is_empty() {
                let program: Vec<String> = t.clips.iter().map(|c| c.clip_id.clone()).collect();
                seen_programs.insert(program)
            } else {
                let extents: Vec<(u32, u32)> = t
                    .extents
                    .iter()
                    .map(|e| (e.start_lba, e.sector_count))
                    .collect();
                // DVD has no authored clip list in this model. Physical cell
                // extents are the stable fallback identity; language or
                // authoring variants may report different runtimes while
                // referring to the same cells. Duration is corroboration,
                // never part of identity.
                if extents.is_empty() {
                    true
                } else {
                    // A single shared cell is commonly an opening/recap. It
                    // is not enough to establish program identity, so retain
                    // runtime as corroborating identity in that degenerate
                    // shape. Multi-cell DVD programs have stable structure
                    // and deliberately ignore small language-version runtime
                    // differences.
                    let duration = (t.duration_secs * 1000.0).round() as u64;
                    seen_extents.insert((extents, (t.extents.len() == 1).then_some(duration)))
                }
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "episodes_tests.rs"]
mod tests;
