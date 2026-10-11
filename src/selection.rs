//! Owned selection snapshot shared by scan/UI, planners, and execution.

use crate::{PresentationIdentity, Selection, StreamFilter};
use libfreemkv::DiscTitle;
use libfreemkv::disc::{EpisodeEvidence, TitleSelectionEvidence};
use std::collections::{HashMap, HashSet};

/// Lightweight title data; indices refer to the original canonical scan order.
#[derive(Debug, Clone, PartialEq)]
struct SelectionTitle {
    index: usize,
    playlist_id: u16,
    duration_secs: f64,
    presentation: Option<PresentationIdentity>,
    audio_languages: Vec<String>,
    evidence: TitleSelectionEvidence,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SelectionModel {
    titles: Vec<SelectionTitle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionReviewReason {
    MissingEpisodeRoster,
    IncompleteEpisodeRoster,
    ConflictingEpisodeRoster,
    InvalidEpisodeRoster,
}

impl SelectionReviewReason {
    pub fn key(self) -> &'static str {
        match self {
            Self::MissingEpisodeRoster => "missing-episode-roster",
            Self::IncompleteEpisodeRoster => "incomplete-episode-roster",
            Self::ConflictingEpisodeRoster => "conflicting-episode-roster",
            Self::InvalidEpisodeRoster => "invalid-episode-roster",
        }
    }
}

/// A selection the evidence does not prove. `candidates` may size a key scope or a damage
/// measurement; they never authorize a rip. As an error it is E9086
/// (`SelectionReviewRequired`), carrying [`SelectionReviewReason::key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionReview {
    pub reason: SelectionReviewReason,
    pub candidates: Vec<usize>,
}

impl From<SelectionReview> for libfreemkv::Error {
    fn from(review: SelectionReview) -> Self {
        libfreemkv::Error::SelectionReviewRequired {
            reason: review.reason.key(),
        }
    }
}

impl SelectionModel {
    pub fn from_titles(titles: &[DiscTitle]) -> Self {
        Self {
            titles: titles
                .iter()
                .enumerate()
                .map(|(index, title)| SelectionTitle {
                    index,
                    playlist_id: title.playlist_id,
                    duration_secs: title.duration_secs,
                    presentation: crate::presentation::identity(title),
                    audio_languages: title
                        .streams
                        .iter()
                        .filter_map(|stream| match stream {
                            libfreemkv::Stream::Audio(audio) => Some(audio.language.clone()),
                            _ => None,
                        })
                        .collect(),
                    evidence: title.selection_evidence.clone(),
                })
                .collect(),
        }
    }

    /// The executable titles `selection` names, or the review it needs. `audio` (or an
    /// explicit `presentation` language, which outranks it) picks among equivalent cuts.
    /// Only `Episodes` can need review; an empty disc's `MainMovie` is no titles.
    pub fn select(
        &self,
        selection: &Selection,
        audio: &StreamFilter,
        presentation: Option<&str>,
    ) -> Result<Vec<usize>, SelectionReview> {
        let scores = self.scores(audio, presentation);
        Ok(match selection {
            Selection::Episodes => match self.episode_roster() {
                Ok(members) => self.representatives(members, &scores),
                Err(reason) => {
                    return Err(SelectionReview {
                        reason,
                        candidates: self.representatives((0..self.titles.len()).collect(), &scores),
                    });
                }
            },
            Selection::MainMovie => self.main_by(&scores).into_iter().collect(),
            Selection::All => (0..self.titles.len()).collect(),
            Selection::Titles(indices) => {
                let mut seen = HashSet::new();
                indices
                    .iter()
                    .copied()
                    .filter(|&index| index < self.titles.len() && seen.insert(index))
                    .collect()
            }
            Selection::Longest => self
                .titles
                .iter()
                .filter(|title| title.duration_secs.is_finite())
                .fold(None::<&SelectionTitle>, |best, title| match best {
                    Some(previous) if title.duration_secs <= previous.duration_secs => best,
                    _ => Some(title),
                })
                .map(|title| vec![title.index])
                .unwrap_or_default(),
        })
    }

    /// The main title, which never needs review: title 0 unless another cut of it carries
    /// more of the wanted languages (as [`select`](Self::select)). `None`: no titles.
    pub fn main_title(&self, audio: &StreamFilter, presentation: Option<&str>) -> Option<usize> {
        self.main_by(&self.scores(audio, presentation))
    }

    fn main_by(&self, scores: &[usize]) -> Option<usize> {
        let main = self.titles.first()?;
        let mut best = 0;
        if let Some(identity) = &main.presentation {
            for (index, title) in self.titles.iter().enumerate().skip(1) {
                if scores[index] > scores[best]
                    && title
                        .presentation
                        .as_ref()
                        .is_some_and(|other| other.branch_variant(identity))
                {
                    best = index;
                }
            }
        }
        Some(best)
    }

    // Each title's coverage of the wanted languages: the presentation language, else `audio`.
    fn scores(&self, audio: &StreamFilter, presentation: Option<&str>) -> Vec<usize> {
        let presentation_audio =
            presentation.map(|language| StreamFilter::Langs(vec![language.to_string()]));
        let preference =
            crate::streams::AudioPreference::new(presentation_audio.as_ref().unwrap_or(audio));
        self.titles
            .iter()
            .map(|title| preference.score_languages(&title.audio_languages))
            .collect()
    }

    fn representatives(&self, indices: Vec<usize>, scores: &[usize]) -> Vec<usize> {
        let mut groups = HashMap::new();
        let mut chosen: Vec<usize> = Vec::new();
        for index in indices {
            if let Some(identity) = &self.titles[index].presentation {
                if let Some(&slot) = groups.get(identity) {
                    if scores[index] > scores[chosen[slot]] {
                        chosen[slot] = index;
                    }
                    continue;
                }
                groups.insert(identity, chosen.len());
            }
            chosen.push(index);
        }
        chosen
    }

    fn episode_roster(&self) -> Result<Vec<usize>, SelectionReviewReason> {
        let mut source = None;
        let mut members = Vec::new();
        let mut observed = 0;
        let mut ids = HashSet::new();
        let mut by_order = HashMap::new();
        let mut by_identity = HashMap::new();
        for title in &self.titles {
            let EpisodeEvidence::Authored {
                roster,
                title_count,
                ordinal,
            } = &title.evidence.episodes
            else {
                continue;
            };
            if roster.trim().is_empty() || !ids.insert(title.playlist_id) {
                return Err(SelectionReviewReason::InvalidEpisodeRoster);
            }
            if *title_count != self.titles.len() {
                return Err(SelectionReviewReason::IncompleteEpisodeRoster);
            }
            if source.is_some_and(|previous| previous != roster) {
                return Err(SelectionReviewReason::ConflictingEpisodeRoster);
            }
            source = Some(roster);
            observed += 1;
            if let Some(ordinal) = *ordinal {
                if let Some(previous) = by_order.insert(ordinal, title.index) {
                    let identity = title.presentation.as_ref();
                    if identity.is_none() || identity != self.titles[previous].presentation.as_ref()
                    {
                        return Err(SelectionReviewReason::InvalidEpisodeRoster);
                    }
                }
                if let Some(identity) = &title.presentation
                    && by_identity
                        .insert(identity, ordinal)
                        .is_some_and(|old| old != ordinal)
                {
                    return Err(SelectionReviewReason::InvalidEpisodeRoster);
                }
                members.push((ordinal, title.index));
            }
        }
        if source.is_none() {
            return Err(SelectionReviewReason::MissingEpisodeRoster);
        }
        if observed != self.titles.len() {
            return Err(SelectionReviewReason::IncompleteEpisodeRoster);
        }
        if !(0..by_order.len()).all(|ordinal| by_order.contains_key(&ordinal)) {
            return Err(SelectionReviewReason::InvalidEpisodeRoster);
        }
        members.sort_by_key(|&(ordinal, _)| ordinal);
        Ok(members.into_iter().map(|(_, index)| index).collect())
    }
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "selection_variant_tests.rs"]
mod variant_tests;
