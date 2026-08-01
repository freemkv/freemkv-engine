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
        // MainMovie / All / Longest carry no per-index reason to report; the
        // resolves-to-nothing gate below covers them. It is NOT true that they
        // always resolve — see that gate.
        Selection::MainMovie | Selection::All | Selection::Longest => {}
    }

    // Does the selection actually resolve to a title? Ask the ONE function that
    // decides it rather than restating the answer here.
    //
    // This block used to be an assumption in a comment — "MainMovie / All /
    // Longest always resolve to at least one title on a disc with titles" — and
    // that is a second copy of a policy `mux::resolve_selection` owns. The
    // copies diverged the moment the owner was hardened: `Longest` drops
    // non-finite durations before folding (a NaN reaching the accumulator first
    // is never displaced by `t > NaN` and would otherwise win outright), so a
    // disc whose every playlist has an unparseable runtime resolves to NOTHING.
    // Preflight said `Ready`, the loop was handed an empty index list, and
    // `run_titles` returned `Ok { titles_written: 0 }`: success, exit 0, no
    // file. The gate is deliberately last and conditional on no earlier reason,
    // so an explicit out-of-range selection still blocks with the reason that
    // names the index instead of a vaguer duplicate.
    //
    // `resolve_selection` is pure — no drive, no file, no read — so calling it
    // keeps preflight's no-side-effects contract.
    if reasons.is_empty() && crate::mux::resolve_selection(disc, &job.selection).is_empty() {
        reasons.push(Reason::new("empty-selection"));
    }

    // Multipass implies raw. A multipass rip is a whole-disc image recovery:
    // it sweeps, patches and resumes against a mapfile that records progress
    // in DISC geometry, and every pass re-reads sectors the previous pass
    // could not get. Decryption has no place in that loop, and the two flags
    // were independent all the way down — the CLI passes `raw` and
    // `multipass` as separate booleans, and all three recovery call sites set
    // `decrypt: !job.raw` regardless of mode, so nothing anywhere refused the
    // combination.
    //
    // Refused rather than silently forced: a caller that asked for decryption
    // and got a raw image would be handed an undecrypted ISO it believes is
    // playable, which is the same wrong-artifact-at-exit-0 shape this crate
    // has been bitten by before. Blocking says so before a sector is read.
    if matches!(job.mode, crate::job::RipMode::Multi) && !job.raw {
        reasons.push(Reason::new("multipass-requires-raw"));
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
    use crate::job::RipMode;

    /// Multipass implies raw, and the engine must be the place that knows it.
    ///
    /// `decrypt` and `multipass` were independent fields with no relationship
    /// encoded anywhere: not in the CLI, which passes them as separate
    /// booleans, and not in the engine, whose three recovery call sites all
    /// set `decrypt: !job.raw` whatever the mode. Every front-end — including
    /// a GUI that does not exist yet — could therefore construct a rip the
    /// product does not support, and nothing would say so.
    #[test]
    fn a_decrypting_multipass_job_is_blocked() {
        let disc = disc_with(2, false, false);

        let mut job = Job::new("disc:///dev/sg0", "iso:///tmp/out.iso");
        job.mode = RipMode::Multi;
        job.raw = false;
        let pf = preflight(&disc, &job);
        assert!(
            pf.reasons()
                .iter()
                .any(|r| r.key == "multipass-requires-raw"),
            "a decrypting multipass job must be refused before a sector is \
             read; got {:?}",
            pf
        );

        // The supported combination still passes this gate.
        job.raw = true;
        let pf = preflight(&disc, &job);
        assert!(
            !pf.reasons()
                .iter()
                .any(|r| r.key == "multipass-requires-raw"),
            "a raw multipass job is the supported shape and must not be blocked"
        );

        // And single-pass decrypting rips are untouched.
        job.mode = RipMode::Single;
        job.raw = false;
        let pf = preflight(&disc, &job);
        assert!(
            !pf.reasons()
                .iter()
                .any(|r| r.key == "multipass-requires-raw"),
            "single-pass decrypt is the ordinary case and must not be blocked"
        );
    }

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

    /// `is_ready` is the accessor a front-end greys out Start on.
    ///
    /// Every test in this module that touches it asserts the BLOCKED
    /// direction, so a constant `false` was indistinguishable — and a
    /// permanently greyed-out Start with no reason shown is a UI nobody can
    /// use. Assert both directions against the same accessor, and against
    /// `reasons()`, which has to agree with it.
    #[test]
    fn is_ready_agrees_with_the_variant_in_both_directions() {
        let ready = preflight(
            &disc_with(2, false, false),
            &Job::new("iso://x.iso", "/out"),
        );
        assert!(ready.is_ready(), "a runnable job must report ready");
        assert!(ready.reasons().is_empty(), "ready carries no reasons");

        let blocked = preflight(
            &disc_with(0, false, false),
            &Job::new("iso://x.iso", "/out"),
        );
        assert!(!blocked.is_ready());
        assert!(!blocked.reasons().is_empty());
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

    /// `Ready` has to mean the rip will actually rip something.
    ///
    /// The selection→indices policy lives in `mux::resolve_selection`, and
    /// preflight carried a SECOND copy of the "does it resolve to anything?"
    /// question — an assumption, written as a comment, that MainMovie / All /
    /// Longest always yield at least one title on a disc that has titles. That
    /// stopped being true when `resolve_selection` was hardened: `Longest` now
    /// drops non-finite durations BEFORE folding (a NaN reaching the
    /// accumulator first was never displaced and won outright), so a disc whose
    /// every playlist has an unparseable runtime resolves to NO title.
    ///
    /// The two copies then disagreed on exactly that input: preflight said
    /// `Ready`, `resolve_selection` said `[]`, and `run_titles([])` returns
    /// `Ok { titles_written: 0 }` — a rip that reports success, exits 0, and
    /// writes nothing. Preflight now asks the owner instead of assuming.
    #[test]
    fn ready_implies_the_selection_resolves_to_at_least_one_title() {
        let mut d = disc_with(3, false, false);
        for t in d.titles.iter_mut() {
            t.duration_secs = f64::NAN;
        }
        let j = Job {
            selection: Selection::Longest,
            ..Job::new("iso://x.iso", "/out")
        };

        // The property, stated against the owner of the policy: preflight may
        // not report Ready for a selection that resolves to nothing.
        let pf = preflight(&d, &j);
        let resolved = crate::mux::resolve_selection(&d, &j.selection);
        assert!(
            resolved.is_empty(),
            "fixture invalid: Longest must resolve to nothing when no duration \
             is measurable, got {resolved:?}"
        );
        assert!(
            !pf.is_ready(),
            "preflight reported Ready for a selection that resolves to no \
             title — the rip would write nothing and exit 0"
        );
        assert!(
            pf.reasons().iter().any(|r| r.key == "empty-selection"),
            "expected empty-selection, got {:?}",
            pf.reasons()
        );

        // And a measurable disc is untouched: one title resolves, Ready holds.
        d.titles[1].duration_secs = 120.0;
        let pf = preflight(&d, &j);
        assert!(
            pf.is_ready(),
            "a disc with one measurable runtime must still be rippable: {pf:?}"
        );
    }

    /// An explicit selection that is entirely out of range must keep reporting
    /// the reason that names the offending index — the new resolves-to-nothing
    /// gate must not displace or duplicate it.
    #[test]
    fn an_all_out_of_range_explicit_selection_still_names_the_index() {
        let d = disc_with(2, false, false);
        let j = Job {
            selection: Selection::Titles(vec![7]),
            ..Job::new("iso://x.iso", "/out")
        };
        let pf = preflight(&d, &j);
        assert!(!pf.is_ready());
        let keys: Vec<&str> = pf.reasons().iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["title-out-of-range"],
            "the specific reason must survive, unduplicated"
        );
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
