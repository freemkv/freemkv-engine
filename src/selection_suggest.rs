//! Suggested episode sets for discs without a proven roster. A suggestion is never proof:
//! the operator confirms it (docs/playlist-selection.md).

use super::{SelectionModel, SelectionTitle, VideoSignature};
use crate::StreamFilter;
use std::cmp::Ordering;

/// A candidate folded into a kept title: an exact alias or a partial-overlap variant
/// (recap, commentary playlist). Listed unticked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alternate {
    pub index: usize,
    pub of: usize,
}

/// An episode-set suggestion. Every list is in play order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestion {
    /// The one qualifying cluster; empty when none or several qualify.
    pub suggested: Vec<usize>,
    /// Every cluster of 2+ like titles, ordered by their first title.
    pub clusters: Vec<Vec<usize>>,
    pub alternates: Vec<Alternate>,
    /// Candidates that play 2+ other, pairwise disjoint candidates.
    pub play_alls: Vec<usize>,
}

// A new cluster starts where the next title runs more than 15% longer than the previous one.
const CLUSTER_GAP: f64 = 1.15;

// A title's content as merged `[start, end)` spans per unit; never empty.
struct Coverage<'a> {
    spans: Vec<(&'a str, u64, u64)>,
    weight: u64,
}

impl<'a> Coverage<'a> {
    // DVD: sector extents (one address space). Others: authored segments. `None`: undescribed.
    fn of(title: &'a SelectionTitle) -> Option<Self> {
        let mut spans: Vec<(&str, u64, u64)> = match title.dvd {
            true => (title.extents.iter())
                .map(|&(start, end)| ("", start, end))
                .collect(),
            false => (title.evidence.segments.iter())
                .map(|s| (s.unit.as_str(), s.start, s.end))
                .collect(),
        };
        spans.retain(|&(_, start, end)| start < end);
        spans.sort_unstable();
        let mut merged: Vec<(&str, u64, u64)> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if last.0 == span.0 && span.1 <= last.2 => last.2 = last.2.max(span.2),
                _ => merged.push(span),
            }
        }
        let weight = merged.iter().map(|&(_, start, end)| end - start).sum();
        (!merged.is_empty()).then_some(Self {
            spans: merged,
            weight,
        })
    }

    fn shared(&self, other: &Self) -> u64 {
        let mut sum = 0;
        for &(unit, start, end) in &self.spans {
            for &(theirs, other_start, other_end) in &other.spans {
                if unit == theirs {
                    sum += end.min(other_end).saturating_sub(start.max(other_start));
                }
            }
        }
        sum
    }

    // All of `other` is inside `self`, and `self` has more.
    fn strictly_contains(&self, other: &Self) -> bool {
        self.weight > other.weight && self.shared(other) == other.weight
    }
}

impl SelectionModel {
    /// A suggested episode set: candidates of at least `min_secs` without exact aliases,
    /// play-alls and partial-overlap variants (kept: the `audio`/`presentation` match, else
    /// the earlier), clustered by video and primary audio language and by 15% duration gaps.
    pub fn suggest_episodes(
        &self,
        audio: &StreamFilter,
        presentation: Option<&str>,
        min_secs: f64,
    ) -> Suggestion {
        let scores = self.scores(audio, presentation);
        let candidates: Vec<usize> = (self.titles.iter())
            .filter(|title| title.duration_secs >= min_secs)
            .map(|title| title.index)
            .collect();
        let mut live = self.representatives(candidates.clone(), &scores);
        let mut alternates: Vec<Alternate> = (candidates.iter())
            .filter(|index| !live.contains(index))
            .filter_map(|&index| {
                let identity = self.titles[index].presentation.as_ref();
                let of = live
                    .iter()
                    .find(|&&kept| self.titles[kept].presentation.as_ref() == identity)?;
                Some(Alternate { index, of: *of })
            })
            .collect();
        self.sort_play_order(&mut live);
        let coverage: Vec<Option<Coverage>> = self.titles.iter().map(Coverage::of).collect();

        // Remove play-alls until none remain, so a play-all of play-alls goes too.
        let mut play_alls = Vec::new();
        loop {
            let found: Vec<usize> = (live.iter().copied())
                .filter(|&index| play_all(index, &live, &coverage))
                .collect();
            if found.is_empty() {
                break;
            }
            live.retain(|index| !found.contains(index));
            play_alls.extend(found);
        }
        self.sort_play_order(&mut play_alls);

        // Fold variants sharing at least half of the shorter one's content.
        let mut group: Vec<usize> = (0..live.len()).collect();
        for a in 0..live.len() {
            for b in a + 1..live.len() {
                let (Some(x), Some(y)) = (&coverage[live[a]], &coverage[live[b]]) else {
                    continue;
                };
                if x.shared(y).saturating_mul(2) >= x.weight.min(y.weight) {
                    let (ra, rb) = (root(&mut group, a), root(&mut group, b));
                    group[ra.max(rb)] = ra.min(rb);
                }
            }
        }
        let mut kept = Vec::new();
        for slot in 0..live.len() {
            if root(&mut group, slot) != slot {
                continue;
            }
            let members: Vec<usize> = (0..live.len())
                .filter(|&other| root(&mut group, other) == slot)
                .map(|other| live[other])
                .collect();
            // `live` is in play order, so the first best score is the earlier title.
            let best = members.iter().copied().fold(members[0], |best, index| {
                match scores[index] > scores[best] {
                    true => index,
                    false => best,
                }
            });
            alternates.extend(
                (members.iter())
                    .filter(|&&index| index != best)
                    .map(|&index| Alternate { index, of: best }),
            );
            kept.push(best);
        }
        // An alias of a folded variant names the kept title.
        let folded: Vec<Alternate> = alternates.clone();
        for alternate in &mut alternates {
            if let Some(kept) = folded.iter().find(|other| other.index == alternate.of) {
                alternate.of = kept.of;
            }
        }
        alternates.sort_by(|a, b| self.play_order(a.index, b.index));

        let mut clusters = self.clusters(&kept);
        clusters.sort_by(|a, b| self.play_order(a[0], b[0]));
        Suggestion {
            suggested: match clusters.as_slice() {
                [one] => one.clone(),
                _ => Vec::new(),
            },
            clusters,
            alternates,
            play_alls,
        }
    }

    // Clusters of 2+ titles with one signature and no 15% duration gap, each in play order.
    fn clusters(&self, kept: &[usize]) -> Vec<Vec<usize>> {
        type Signature = (Option<VideoSignature>, Option<String>);
        let mut by_signature: Vec<(Signature, Vec<usize>)> = Vec::new();
        for &index in kept {
            let title = &self.titles[index];
            let signature = (
                title.video,
                title.primary_audio.as_deref().map(language_key),
            );
            match by_signature
                .iter_mut()
                .find(|(other, _)| *other == signature)
            {
                Some((_, members)) => members.push(index),
                None => by_signature.push((signature, vec![index])),
            }
        }
        let mut clusters = Vec::new();
        for (_, mut members) in by_signature {
            members.sort_by(|&a, &b| {
                (self.titles[a].duration_secs)
                    .total_cmp(&self.titles[b].duration_secs)
                    .then_with(|| self.play_order(a, b))
            });
            let mut cluster: Vec<usize> = Vec::new();
            for index in members {
                if let Some(&previous) = cluster.last()
                    && self.titles[index].duration_secs
                        > self.titles[previous].duration_secs * CLUSTER_GAP
                {
                    clusters.push(std::mem::take(&mut cluster));
                }
                cluster.push(index);
            }
            clusters.push(cluster);
        }
        clusters.retain(|cluster| cluster.len() >= 2);
        for cluster in &mut clusters {
            self.sort_play_order(cluster);
        }
        clusters
    }

    fn sort_play_order(&self, indices: &mut [usize]) {
        indices.sort_by(|&a, &b| self.play_order(a, b));
    }

    // DVD: title set, then TT_SRPT order (`playlist_id`). Others: first segment's unit and
    // in-time, then first extent sector, then playlist number. Undescribed sorts last.
    fn play_order(&self, a: usize, b: usize) -> Ordering {
        let key = |index: usize| {
            let title = &self.titles[index];
            let segment = match title.dvd {
                true => None,
                false => title.evidence.segments.first(),
            };
            let extent = match title.dvd {
                true => None,
                false => title.extents.first().map(|&(start, _)| start),
            };
            (
                segment.is_none(),
                segment.map(|s| (s.unit.as_str(), s.start)),
                extent.is_none(),
                extent,
                title.playlist_id,
                index,
            )
        };
        key(a).cmp(&key(b))
    }
}

// Two or more other live candidates lie strictly inside `index`, pairwise disjoint.
fn play_all(index: usize, live: &[usize], coverage: &[Option<Coverage>]) -> bool {
    let Some(outer) = &coverage[index] else {
        return false;
    };
    let inner: Vec<&Coverage> = (live.iter())
        .filter(|&&other| other != index)
        .filter_map(|&other| coverage[other].as_ref())
        .filter(|inner| outer.strictly_contains(inner))
        .collect();
    inner.len() >= 2
        && (inner.iter().enumerate()).all(|(n, a)| inner[n + 1..].iter().all(|b| a.shared(b) == 0))
}

fn root(group: &mut [usize], mut slot: usize) -> usize {
    while group[slot] != slot {
        group[slot] = group[group[slot]];
        slot = group[slot];
    }
    slot
}

// One key per language however tagged ("en", "eng", "English"); unknown tags compare raw.
fn language_key(tag: &str) -> String {
    match crate::streams::stream_lang(tag) {
        Some(language) => language.to_639_3().to_string(),
        None => tag.trim().to_ascii_lowercase(),
    }
}
