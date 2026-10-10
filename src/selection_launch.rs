//! Consume producer-proved direct full-title launches, never infer equivalence.

use super::*;
use crate::streams::normalize_lang;
use libfreemkv::disc::DvdLaunchEvidence;

impl SelectionModel {
    pub(super) fn launch_selection(
        &self,
        audio: &StreamFilter,
        preferences: &SelectionPreferences,
    ) -> Option<SelectionReport> {
        let has_root = self.titles.iter().any(|t| {
            matches!(
                t.evidence.dvd_launch,
                DvdLaunchEvidence::VerifiedRoot { .. }
            )
        });
        let requested = preferences
            .presentation_language
            .as_ref()
            .map(|language| vec![language.clone()])
            .unwrap_or_else(|| match audio {
                StreamFilter::Langs(languages) => languages.clone(),
                _ => Vec::new(),
            });
        if !has_root {
            if !requested.is_empty()
                && self
                    .titles
                    .iter()
                    .any(|t| matches!(t.evidence.dvd_launch, DvdLaunchEvidence::Review(_)))
            {
                return Some(review(
                    (0..self.titles.len()).collect(),
                    SelectionReviewReason::IncompleteLaunchProof,
                ));
            }
            return None;
        }
        Some(match self.launch_candidates() {
            Err(reason) => review((0..self.titles.len()).collect(), reason),
            Ok(routes) => {
                let candidates: Vec<_> = routes
                    .iter()
                    .map(|&(index, _)| index)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                let wanted: Option<Vec<_>> =
                    requested.iter().map(|tag| normalize_lang(tag)).collect();
                match wanted {
                    None => review(
                        candidates,
                        SelectionReviewReason::InvalidPresentationLanguage,
                    ),
                    Some(wanted) if wanted.is_empty() => review(
                        candidates,
                        SelectionReviewReason::MissingPresentationLanguage,
                    ),
                    Some(wanted) => {
                        let matching: HashSet<_> = routes
                            .iter()
                            .filter(|(_, language)| wanted.contains(language))
                            .map(|&(index, _)| index)
                            .collect();
                        if matching.len() == 1 {
                            let indices: Vec<_> = matching.into_iter().collect();
                            SelectionReport {
                                candidates: indices.clone(),
                                indices,
                                basis: SelectionBasis::AuthoredLaunch,
                                review_reason: None,
                            }
                        } else {
                            review(
                                candidates,
                                if matching.is_empty() {
                                    SelectionReviewReason::UnmatchedPresentationLanguage
                                } else {
                                    SelectionReviewReason::AmbiguousPresentationLanguage
                                },
                            )
                        }
                    }
                }
            }
        })
    }

    fn launch_candidates(&self) -> Result<Vec<(usize, isolang::Language)>, SelectionReviewReason> {
        let mut root = None;
        let mut buttons = HashSet::new();
        let mut ids = HashSet::new();
        let mut candidates = Vec::new();
        let mut destinations = HashMap::new();
        for title in &self.titles {
            let DvdLaunchEvidence::VerifiedRoot {
                vts,
                pgcn,
                title_count,
                routes,
            } = &title.evidence.dvd_launch
            else {
                return Err(SelectionReviewReason::IncompleteLaunchProof);
            };
            if *title_count != self.titles.len()
                || *vts == 0
                || *pgcn == 0
                || root.is_some_and(|previous| previous != (*vts, *pgcn))
                || !ids.insert(title.playlist_id)
            {
                return Err(SelectionReviewReason::InvalidLaunchProof);
            }
            root = Some((*vts, *pgcn));
            for route in routes {
                if destinations
                    .insert((route.target_vts, route.target_title), title.index)
                    .is_some_and(|previous| previous != title.index)
                {
                    return Err(SelectionReviewReason::InvalidLaunchProof);
                }
                let language = normalize_lang(&route.audio_language)
                    .filter(|language| *language != isolang::Language::Und)
                    .ok_or(SelectionReviewReason::InvalidLaunchProof)?;
                let audio: Vec<_> = title
                    .audio_by_pid
                    .iter()
                    .filter(|(pid, _)| *pid == route.audio_pid)
                    .collect();
                if route.target_vts != *vts
                    || route.target_title == 0
                    || route.target_part != 1
                    || route.audio_stream > 7
                    || route.button == 0
                    || route.button > 36
                    || !buttons.insert(route.button)
                    || route.display_masks.is_empty()
                    || route.traces.is_empty()
                    || route.traces.iter().any(Vec::is_empty)
                    || audio.len() != 1
                    || normalize_lang(&audio[0].1) != Some(language)
                {
                    return Err(SelectionReviewReason::InvalidLaunchProof);
                }
                candidates.push((title.index, language));
            }
        }
        if buttons.is_empty() || !(1..=buttons.len()).all(|n| buttons.contains(&(n as u8))) {
            return Err(SelectionReviewReason::IncompleteLaunchProof);
        }
        Ok(candidates)
    }
}

fn review(mut candidates: Vec<usize>, reason: SelectionReviewReason) -> SelectionReport {
    candidates.sort_unstable();
    candidates.dedup();
    SelectionReport {
        indices: Vec::new(),
        candidates,
        basis: SelectionBasis::UncertainReview,
        review_reason: Some(reason),
    }
}
