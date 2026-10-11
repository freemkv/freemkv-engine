//! Conservative identities for alternate presentations within one scanned disc.

use libfreemkv::DiscTitle;
use libfreemkv::disc::ContentSegment;
use std::collections::HashSet;

/// Exact ordered authored content segments, never duration/size similarity. Format-neutral:
/// the scanner describes every format's titles as segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PresentationIdentity(Vec<ContentSegment>);

pub(crate) fn identity(title: &DiscTitle) -> Option<PresentationIdentity> {
    let segments = &title.selection_evidence.segments;
    (!segments.is_empty()).then(|| PresentationIdentity(segments.clone()))
}

impl PresentationIdentity {
    // Another cut of the same programme: the content both share appears in the same order
    // in each and is more than half of each. Exact aliases share everything.
    pub(crate) fn branch_variant(&self, other: &Self) -> bool {
        let (mine, theirs) = (shared(&self.0, &other.0), shared(&other.0, &self.0));
        let common = weight(mine.iter().copied());
        mine == theirs
            && common.saturating_mul(2) > weight(self.0.iter())
            && common.saturating_mul(2) > weight(other.0.iter())
    }
}

// `a`'s segments that also occur in `b`, in `a`'s order.
fn shared<'a>(a: &'a [ContentSegment], b: &[ContentSegment]) -> Vec<&'a ContentSegment> {
    let present: HashSet<_> = b.iter().collect();
    a.iter()
        .filter(|segment| present.contains(segment))
        .collect()
}

fn weight<'a>(segments: impl Iterator<Item = &'a ContentSegment>) -> u64 {
    segments.fold(0, |sum, s| {
        sum.saturating_add(s.end.saturating_sub(s.start))
    })
}
