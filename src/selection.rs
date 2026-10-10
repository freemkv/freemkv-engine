//! Owned selection snapshot shared by scan/UI, planners, and execution.

use crate::{PresentationIdentity, Selection, StreamFilter};
use libfreemkv::DiscTitle;
use libfreemkv::disc::{EpisodeEvidence, MovieSelectionBasis, TitleSelectionEvidence};
use std::collections::{HashMap, HashSet};

#[path = "selection_launch.rs"]
mod launch;

/// Content-presentation preference, independent of which streams are retained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionPreferences {
    pub presentation_language: Option<String>,
}

/// Lightweight title data; indices refer to the original canonical scan order.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionTitle {
    pub index: usize,
    pub playlist_id: u16,
    pub playlist: String,
    pub duration_secs: f64,
    pub presentation: Option<PresentationIdentity>,
    pub audio_languages: Vec<String>,
    pub audio_by_pid: Vec<(u16, String)>,
    pub evidence: TitleSelectionEvidence,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SelectionModel {
    titles: Vec<SelectionTitle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionBasis {
    Explicit,
    All,
    Longest,
    Movie(MovieSelectionBasis),
    AuthoredLaunch,
    AuthoredEpisodes { roster: String },
    UncertainReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionReviewReason {
    MissingEpisodeRoster,
    IncompleteEpisodeRoster,
    ConflictingEpisodeRoster,
    InvalidEpisodeRoster,
    IncompleteLaunchProof,
    InvalidLaunchProof,
    MissingPresentationLanguage,
    InvalidPresentationLanguage,
    UnmatchedPresentationLanguage,
    AmbiguousPresentationLanguage,
}

impl SelectionReviewReason {
    pub fn key(self) -> &'static str {
        match self {
            Self::MissingEpisodeRoster => "missing-episode-roster",
            Self::IncompleteEpisodeRoster => "incomplete-episode-roster",
            Self::ConflictingEpisodeRoster => "conflicting-episode-roster",
            Self::InvalidEpisodeRoster => "invalid-episode-roster",
            Self::IncompleteLaunchProof => "incomplete-launch-proof",
            Self::InvalidLaunchProof => "invalid-launch-proof",
            Self::MissingPresentationLanguage => "missing-presentation-language",
            Self::InvalidPresentationLanguage => "invalid-presentation-language",
            Self::UnmatchedPresentationLanguage => "unmatched-presentation-language",
            Self::AmbiguousPresentationLanguage => "ambiguous-presentation-language",
        }
    }
}

/// `indices` alone are executable. Review candidates never authorize a rip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionReport {
    pub indices: Vec<usize>,
    pub candidates: Vec<usize>,
    pub basis: SelectionBasis,
    pub review_reason: Option<SelectionReviewReason>,
}

impl SelectionReport {
    pub fn requires_review(&self) -> bool {
        self.review_reason.is_some()
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
                    playlist: title.playlist.clone(),
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
                    audio_by_pid: title
                        .audio_streams()
                        .map(|audio| (audio.pid, audio.language.clone()))
                        .collect(),
                    evidence: title.selection_evidence.clone(),
                })
                .collect(),
        }
    }

    pub fn from_disc(disc: &libfreemkv::Disc) -> Self {
        Self::from_titles(&disc.titles)
    }

    pub fn titles(&self) -> &[SelectionTitle] {
        &self.titles
    }

    pub fn select(&self, selection: &Selection, audio: &StreamFilter) -> SelectionReport {
        self.select_with_preferences(selection, audio, &SelectionPreferences::default())
    }

    pub fn select_with_preferences(
        &self,
        selection: &Selection,
        audio: &StreamFilter,
        preferences: &SelectionPreferences,
    ) -> SelectionReport {
        if matches!(selection, Selection::MainMovie)
            && let Some(report) = self.launch_selection(audio, preferences)
        {
            return report;
        }
        let presentation_audio = preferences
            .presentation_language
            .as_ref()
            .map(|language| StreamFilter::Langs(vec![language.clone()]));
        let audio = presentation_audio.as_ref().unwrap_or(audio);
        let preference = crate::streams::AudioPreference::new(audio);
        let scores: Vec<_> = self
            .titles
            .iter()
            .map(|title| preference.score_languages(&title.audio_languages))
            .collect();
        let (indices, basis) = match selection {
            Selection::Episodes => match self.episode_roster() {
                Ok((roster, members)) => (
                    self.representatives(members, &scores),
                    SelectionBasis::AuthoredEpisodes { roster },
                ),
                Err(reason) => {
                    return SelectionReport {
                        indices: Vec::new(),
                        candidates: self.representatives((0..self.titles.len()).collect(), &scores),
                        basis: SelectionBasis::UncertainReview,
                        review_reason: Some(reason),
                    };
                }
            },
            Selection::MainMovie => {
                let Some(main) = self.titles.first() else {
                    return SelectionReport {
                        indices: Vec::new(),
                        candidates: Vec::new(),
                        basis: SelectionBasis::Movie(MovieSelectionBasis::CanonicalFallback),
                        review_reason: None,
                    };
                };
                let mut best = 0;
                if let Some(identity) = &main.presentation {
                    for (index, title) in self.titles.iter().enumerate().skip(1) {
                        if title.presentation.as_ref() == Some(identity)
                            && scores[index] > scores[best]
                        {
                            best = index;
                        }
                    }
                }
                (vec![best], SelectionBasis::Movie(main.evidence.movie_basis))
            }
            Selection::All => ((0..self.titles.len()).collect(), SelectionBasis::All),
            Selection::Titles(indices) => {
                let mut seen = HashSet::new();
                (
                    indices
                        .iter()
                        .copied()
                        .filter(|&index| index < self.titles.len() && seen.insert(index))
                        .collect(),
                    SelectionBasis::Explicit,
                )
            }
            Selection::Longest => {
                let best = self
                    .titles
                    .iter()
                    .filter(|title| title.duration_secs.is_finite())
                    .fold(None::<&SelectionTitle>, |best, title| match best {
                        Some(previous) if title.duration_secs <= previous.duration_secs => best,
                        _ => Some(title),
                    });
                (
                    best.map(|title| vec![title.index]).unwrap_or_default(),
                    SelectionBasis::Longest,
                )
            }
        };
        SelectionReport {
            candidates: indices.clone(),
            indices,
            basis,
            review_reason: None,
        }
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

    fn episode_roster(&self) -> Result<(String, Vec<usize>), SelectionReviewReason> {
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
                member,
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
            if *member {
                let ordinal = ordinal.ok_or(SelectionReviewReason::IncompleteEpisodeRoster)?;
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
            } else if ordinal.is_some() {
                return Err(SelectionReviewReason::InvalidEpisodeRoster);
            }
        }
        let source = source.ok_or(SelectionReviewReason::MissingEpisodeRoster)?;
        if observed != self.titles.len() {
            return Err(SelectionReviewReason::IncompleteEpisodeRoster);
        }
        if !(0..by_order.len()).all(|ordinal| by_order.contains_key(&ordinal)) {
            return Err(SelectionReviewReason::InvalidEpisodeRoster);
        }
        members.sort_by_key(|&(ordinal, _)| ordinal);
        Ok((
            source.clone(),
            members.into_iter().map(|(_, index)| index).collect(),
        ))
    }
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
