//! Validate a [`Job`] against a scanned [`libfreemkv::Disc`] WITHOUT executing it.
//!
//! This is the "grey out Start and say why" logic the desktop UI needs on
//! every selection change (UI-doc §4.3.2), and the same check the CLI does up
//! front before touching a drive. It has no side effects: no drive open, no
//! file creation, no sector read. It answers, as data, "can this job run, and
//! if not, why."

use crate::job::{Job, Selection};

/// The outcome of [`preflight`]. A front-end greys out Start on `Blocked` and
/// renders each [`Reason`]; `Ready` means the job may proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preflight {
    /// Every checked precondition holds; the job may run.
    Ready,
    /// One or more preconditions fail. Each carries a stable, front-end-
    /// localizable reason key — never a pre-rendered English sentence.
    Blocked(Vec<Reason>),
}

impl Preflight {
    /// True when the job may proceed.
    pub fn is_ready(&self) -> bool {
        matches!(self, Preflight::Ready)
    }

    /// The blocking reasons, empty when [`Ready`](Preflight::Ready).
    pub fn reasons(&self) -> &[Reason] {
        match self {
            Preflight::Ready => &[],
            Preflight::Blocked(rs) => rs,
        }
    }
}

/// One reason a job cannot run, as data. `key` is a stable identifier a
/// front-end maps to a localized message (mirrors the library's error-code
/// discipline — no English decided here). `detail` carries a machine value
/// (an index, a count) the message may interpolate, never prose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    /// Stable reason key. The complete set this crate emits — a front-end that
    /// maps only part of it renders a blocked Start with no explanation:
    ///
    /// ```text
    /// "no-titles"              the scan found no titles at all
    /// "empty-selection"        the selection resolves to no title
    /// "title-out-of-range"     an index past the last title (detail = index)
    /// "language-unmatched"     a language-filtered class no selected title
    ///                          carries (detail = audio|subtitle|subtitle_forced)
    /// "unknown-language"       a requested tag names no language (detail = tag)
    /// "encrypted-no-key"       an encrypted disc, not raw, with no usable key
    /// "selection-review-required" authored episode roster unavailable/invalid
    ///                          (detail = SelectionReviewReason::key())
    /// ```
    pub key: String,
    /// Optional machine detail for the message (e.g. the offending index).
    pub detail: Option<String>,
}

impl Reason {
    fn new(key: &str) -> Self {
        Reason {
            key: key.to_string(),
            detail: None,
        }
    }
    fn with_detail(key: &str, detail: impl ToString) -> Self {
        Reason {
            key: key.to_string(),
            detail: Some(detail.to_string()),
        }
    }
}

/// Validate `job` against the already-scanned `disc`. Pure and side-effect
/// free — safe to call on every UI selection change.
///
/// Checks, cheapest first: the disc has titles; the selection resolves to a non-empty set of
/// in-range indices; every requested language tag names a language; every language-filtered
/// stream class the job asks for is carried by a selected title; and, if the disc is encrypted and the job is not `raw`, a usable key exists.
pub fn preflight(disc: &libfreemkv::Disc, job: &Job) -> Preflight {
    let mut reasons = Vec::new();

    if disc.titles.is_empty() {
        reasons.push(Reason::new("no-titles"));
        // Nothing else is meaningful without titles.
        return Preflight::Blocked(reasons);
    }

    // Resolve the selection to concrete indices and check ranges.
    match &job.selection {
        Selection::Titles(indices) => {
            if indices.is_empty() {
                reasons.push(Reason::new("empty-selection"));
            }
            for &i in indices {
                if i >= disc.titles.len() {
                    reasons.push(Reason::with_detail("title-out-of-range", i));
                }
            }
        }
        // MainMovie / All / Longest / Episodes carry no per-index reason to report; the
        // resolves-to-nothing gate below covers them. It is NOT true that they
        // always resolve — see that gate.
        Selection::MainMovie | Selection::All | Selection::Longest | Selection::Episodes => {}
    }

    // Does the selection resolve to a title? Ask the ONE function that decides
    // it (pure) rather than restate the policy here: `Longest` can resolve to
    // NOTHING (all-NaN durations), which a prior duplicated assumption missed.
    let report = crate::SelectionModel::from_disc(disc).select(&job.selection, &job.streams.audio);
    if let Some(reason) = report.review_reason {
        reasons.push(Reason::with_detail(
            "selection-review-required",
            reason.key(),
        ));
    }
    let resolved = report.indices;
    if reasons.is_empty() && resolved.is_empty() {
        reasons.push(Reason::new("empty-selection"));
    }

    // A tag that names no language fails the rip only after the image opens; refuse it
    // here by name (and skip the class check below, which would blame the disc instead).
    if reasons.is_empty() {
        for tag in job.streams.unknown_language_tags() {
            reasons.push(Reason::with_detail("unknown-language", tag));
        }
    }

    // A language request no selected title can honour. Previously `-a jpn` on a
    // disc with no Japanese audio silently muxed a video-only MKV, exit 0.
    // Judged across the whole selection so multi-title rips aren't over-refused.
    if reasons.is_empty() {
        for class in job
            .streams
            .unmatched_everywhere(resolved.iter().filter_map(|&i| disc.titles.get(i)))
        {
            reasons.push(Reason::with_detail("language-unmatched", class));
        }
    }

    // Decrypt gate: an encrypted disc muxed WITHOUT raw needs a usable key set (KU §3.5) over
    // what the rip decrypts (its titles; the multipass image's whole disc), else the
    // executors' gate (KU-X1), so preflight can't pass a rip the passes refuse.
    let scope = match job.mode {
        crate::RipMode::Multi => libfreemkv::keys::KeyScope::WholeDisc,
        crate::RipMode::Single => libfreemkv::keys::KeyScope::Titles(resolved),
    };
    let keyed = match &job.keys {
        Some(set) => {
            matches!(
                crate::keys::key_status(disc, set),
                libfreemkv::keys::DecryptStatus::Ready
                    | libfreemkv::keys::DecryptStatus::NotEncrypted
            ) && set.covers(&scope)
        }
        None => crate::resolve::ensure_decryptable_with(disc, false, None).is_ok(),
    };
    if disc.encrypted && !job.raw && !keyed {
        reasons.push(Reason::new("encrypted-no-key"));
    }

    if reasons.is_empty() {
        Preflight::Ready
    } else {
        Preflight::Blocked(reasons)
    }
}

#[cfg(test)]
#[path = "preflight_tests.rs"]
mod tests;
