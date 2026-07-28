//! Validate a [`Job`] against a scanned [`Disc`] WITHOUT executing it.
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
    /// Stable reason key, e.g. `"no-titles"`, `"title-out-of-range"`,
    /// `"encrypted-no-key"`, `"empty-selection"`.
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
/// Checks, cheapest first:
/// 1. The disc has at least one title.
/// 2. The selection resolves to a non-empty set of in-range title indices.
/// 3. If the disc is encrypted and the job is NOT `raw`, a usable key exists
///    (so a decrypting rip cannot silently write ciphertext — the same class
///    of guard `Disc::ensure_decryptable` enforces at execution time, surfaced
///    here earlier as data).
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
        // MainMovie / All / Longest always resolve to at least one title on a
        // disc with titles (checked above), so they never block here.
        Selection::MainMovie | Selection::All | Selection::Longest => {}
    }

    // Decrypt gate: an encrypted disc muxed WITHOUT raw needs a usable key.
    // `disc.encrypted` is the library's authoritative "needs decryption" flag.
    // Delegate the "do we actually have a key?" judgment to `resolve_keys` — the
    // ONE place that decides it — so preflight and the key-status report can
    // never disagree (a bare `aacs.is_some()` here would pass a VID-only
    // placeholder scan that `resolve_keys` correctly reports as unresolved).
    // Raw passes (the user wants ciphertext); unencrypted passes.
    if disc.encrypted && !job.raw && !crate::resolve::resolve_keys(disc).resolved {
        reasons.push(Reason::new("encrypted-no-key"));
    }

    if reasons.is_empty() {
        Preflight::Ready
    } else {
        Preflight::Blocked(reasons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Job;

    // A resolved AacsState carrying real key material (non-empty unit_keys) —
    // what the preflight decrypt gate (via resolve_keys) accepts as "keyed". All
    // fields spelled out because the lib's plain-data types don't derive Default.
    fn resolved_aacs() -> libfreemkv::AacsState {
        libfreemkv::AacsState {
            version: 1,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: String::new(),
            key_source: libfreemkv::KeyOrigin::KeyDb,
            vuk: None,
            unit_keys: vec![(0, [0u8; 16])],
            read_data_key: None,
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        }
    }

    // A minimal scanned Disc with `n` titles, encrypted flag, and key presence.
    // Disc has all-pub fields, so an external crate can build a test fixture.
    fn disc_with(n: usize, encrypted: bool, has_aacs_key: bool) -> libfreemkv::Disc {
        let titles = (0..n).map(|_| libfreemkv::DiscTitle::empty()).collect();
        libfreemkv::Disc {
            volume_id: "TEST".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1000,
            capacity_bytes: 1000 * 2048,
            layers: 1,
            titles,
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: if has_aacs_key {
                Some(resolved_aacs())
            } else {
                None
            },
            css: None,
            encrypted,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    #[test]
    fn ready_on_clean_unencrypted_disc() {
        let d = disc_with(3, false, false);
        let j = Job::new("iso://x.iso", "/out");
        assert_eq!(preflight(&d, &j), Preflight::Ready);
    }

    #[test]
    fn blocks_when_no_titles() {
        let d = disc_with(0, false, false);
        let j = Job::new("iso://x.iso", "/out");
        let pf = preflight(&d, &j);
        assert!(!pf.is_ready());
        assert_eq!(pf.reasons()[0].key, "no-titles");
    }

    #[test]
    fn blocks_encrypted_without_key_unless_raw() {
        let d = disc_with(2, true, false);
        let j = Job::new("iso://x.iso", "/out");
        let pf = preflight(&d, &j);
        assert!(pf.reasons().iter().any(|r| r.key == "encrypted-no-key"));

        // Raw passes the decrypt gate (user wants ciphertext).
        let raw_job = Job {
            raw: true,
            ..Job::new("iso://x.iso", "/out")
        };
        assert_eq!(preflight(&d, &raw_job), Preflight::Ready);
    }

    #[test]
    fn encrypted_with_key_is_ready() {
        let d = disc_with(2, true, true);
        let j = Job::new("iso://x.iso", "/out");
        assert_eq!(preflight(&d, &j), Preflight::Ready);
    }

    #[test]
    fn blocks_encrypted_placeholder_aacs_without_key_material() {
        // A VID-only scan leaves `aacs = Some(..)` with EMPTY unit_keys and no
        // VUK. preflight must NOT treat that as keyed (the bug: gating on
        // `aacs.is_some()` passed it) — it now delegates to resolve_keys, which
        // reports unresolved, so the decrypt gate blocks.
        let mut d = disc_with(2, true, true);
        if let Some(a) = d.aacs.as_mut() {
            a.unit_keys = Vec::new();
            a.vuk = None;
        }
        let pf = preflight(&d, &Job::new("iso://x.iso", "/out"));
        assert!(pf.reasons().iter().any(|r| r.key == "encrypted-no-key"));
    }

    #[test]
    fn blocks_out_of_range_explicit_title() {
        let d = disc_with(2, false, false);
        let j = Job {
            selection: Selection::Titles(vec![0, 5]),
            ..Job::new("iso://x.iso", "/out")
        };
        let pf = preflight(&d, &j);
        let r = pf
            .reasons()
            .iter()
            .find(|r| r.key == "title-out-of-range")
            .expect("expected out-of-range reason");
        assert_eq!(r.detail.as_deref(), Some("5"));
    }

    #[test]
    fn blocks_empty_explicit_selection() {
        let d = disc_with(2, false, false);
        let j = Job {
            selection: Selection::Titles(vec![]),
            ..Job::new("iso://x.iso", "/out")
        };
        let pf = preflight(&d, &j);
        assert!(pf.reasons().iter().any(|r| r.key == "empty-selection"));
    }
}
