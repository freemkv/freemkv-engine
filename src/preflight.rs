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
    /// Stable reason key. The complete set this crate emits — a front-end that
    /// maps only part of it renders a blocked Start with no explanation:
    ///
    /// * `"no-titles"` — the scan found no titles at all.
    /// * `"empty-selection"` — the selection resolves to no title.
    /// * `"title-out-of-range"` — an explicit index past the last title;
    ///   `detail` carries the offending index.
    /// * `"multipass-requires-raw"` — `RipMode::Multi` without `job.raw`.
    /// * `"language-unmatched"` — a language-filtered stream class the job asks
    ///   for is carried by no selected title; `detail` carries the class key
    ///   (`audio`, `subtitle`, `subtitle_forced`).
    /// * `"encrypted-no-key"` — an encrypted disc, not raw, with no usable key.
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
/// 3. Every language-filtered stream class the job asks for is carried by at
///    least one selected title (so a rip cannot silently ship a file without
///    the audio or subtitle track the user asked for).
/// 4. If the disc is encrypted and the job is NOT `raw`, a usable key exists
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
    let resolved = crate::mux::resolve_selection(disc, &job.selection);
    if reasons.is_empty() && resolved.is_empty() {
        reasons.push(Reason::new("empty-selection"));
    }

    // A language request no selected title can honour. `StreamChoice::
    // unmatched` has computed this all along and nothing called it: the
    // resolved `StreamSelection` simply came back with no PIDs for the class,
    // the mux wrote the file without it, and the run exited 0 — `-a jpn` on a
    // disc with no Japanese audio delivered a video-only MKV and reported
    // success. Detected, and not reported, is the same as not detected.
    //
    // Judged across the whole selection, not per title: a batch where the
    // language is present on SOME selected title can honour the request, and
    // blocking it would refuse ordinary multi-title rips. Only a request that
    // no selected title can satisfy is a rip that will certainly ship without
    // the track. A class no selected title carries at all is not a miss —
    // there was never a track to keep.
    //
    // Conditional on no earlier reason for the same purpose as the gate above:
    // a job blocked for an out-of-range index gets the reason that names the
    // index, not a second, vaguer one derived from titles it never selected.
    if reasons.is_empty() {
        for class in job
            .streams
            .unmatched_everywhere(resolved.iter().filter_map(|&i| disc.titles.get(i)))
        {
            reasons.push(Reason::with_detail("language-unmatched", class));
        }
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

    /// Every reason key this module can emit must be documented where a
    /// front-end will look for it.
    ///
    /// `Reason.key` is a contract with a UI that renders nothing but the key:
    /// a key the guide does not list is a blocked Start button with no
    /// message. The set was presented as four for as long as there have been
    /// five — `multipass-requires-raw` was emitted but listed nowhere, and it
    /// is the key `USING_THE_ENGINE.md`'s own §1 example triggers.
    ///
    /// Derived from the SOURCE, not from a hand-kept list here, so adding a
    /// sixth key without documenting it fails this test rather than quietly
    /// repeating the same omission.
    #[test]
    fn every_emitted_reason_key_is_documented() {
        let src = include_str!("preflight.rs");
        let guide = include_str!("../USING_THE_ENGINE.md");
        // The `Reason.key` rustdoc: inside the struct, i.e. between its
        // declaration and the `impl` that follows.
        let doc_block = src
            .split_once("pub struct Reason")
            .expect("the file has a Reason struct")
            .1
            .split_once("impl Reason")
            .expect("the struct is followed by its impl")
            .0;

        let mut keys: Vec<&str> = Vec::new();
        for marker in ["Reason::new(\"", "Reason::with_detail(\""] {
            for (i, part) in src.split(marker).enumerate() {
                // The first split part is what precedes the first marker.
                if i == 0 {
                    continue;
                }
                keys.push(part.split('"').next().expect("a closed string literal"));
            }
        }
        keys.sort_unstable();
        keys.dedup();
        assert!(
            keys.len() >= 5,
            "fixture check: expected at least the five known keys, found {keys:?}"
        );
        assert!(
            keys.contains(&"multipass-requires-raw"),
            "fixture check: the key this test was written for is gone: {keys:?}"
        );

        for key in keys {
            assert!(
                doc_block.contains(key),
                "reason key {key:?} is emitted but not listed on `Reason.key`"
            );
            assert!(
                guide.contains(key),
                "reason key {key:?} is emitted but not listed in USING_THE_ENGINE.md"
            );
        }
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

    // A title carrying exactly the audio languages named, and one English
    // full subtitle so the subtitle class is never the thing being tested.
    fn title_with_audio(langs: &[&str]) -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.duration_secs = 3600.0;
        for (i, l) in langs.iter().enumerate() {
            t.streams
                .push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                    pid: 0x1100 + i as u16,
                    codec: libfreemkv::Codec::TrueHd,
                    channels: libfreemkv::AudioChannels::Stereo,
                    language: (*l).into(),
                    sample_rate: libfreemkv::SampleRate::S48,
                    secondary: false,
                    purpose: libfreemkv::LabelPurpose::Normal,
                    label: String::new(),
                }));
        }
        t
    }

    fn disc_with_titles(titles: Vec<libfreemkv::DiscTitle>) -> libfreemkv::Disc {
        let mut d = disc_with(0, false, false);
        d.titles = titles;
        d
    }

    fn audio_job(langs: &[&str]) -> Job {
        Job::new("iso://x.iso", "/out").with_audio(crate::job::StreamFilter::Langs(
            langs.iter().map(|s| s.to_string()).collect(),
        ))
    }

    /// `-a jpn` on a disc with no Japanese audio must be REFUSED, not run.
    ///
    /// `StreamChoice::unmatched` computes exactly this and had no caller
    /// anywhere in the crate: the resolved selection simply kept no audio PIDs,
    /// the mux wrote a video-only file, and the run exited 0. The user asked for
    /// a track, got a file without one, and nothing said so.
    #[test]
    fn a_language_no_selected_title_carries_is_refused() {
        let d = disc_with_titles(vec![title_with_audio(&["eng", "deu"])]);
        let pf = preflight(&d, &audio_job(&["jpn"]));
        assert!(
            !pf.is_ready(),
            "a rip that cannot honour its own audio request must not report \
             Ready: {pf:?}"
        );
        let r = pf
            .reasons()
            .iter()
            .find(|r| r.key == "language-unmatched")
            .unwrap_or_else(|| panic!("expected language-unmatched, got {:?}", pf.reasons()));
        assert_eq!(
            r.detail.as_deref(),
            Some("audio"),
            "the reason must name the class the front-end has to explain"
        );

        // A language the disc DOES carry is untouched — including via its
        // bibliographic code, which is how discs label tracks.
        for ok in [vec!["eng"], vec!["ger"], vec!["jpn", "eng"]] {
            let pf = preflight(&d, &audio_job(&ok));
            assert!(
                pf.is_ready(),
                "{ok:?} is satisfiable on this disc and must not block: {pf:?}"
            );
        }
    }

    /// The gate is about the RIP, not about one title: a batch where some
    /// selected title carries the language still runs. Only a request no
    /// selected title can satisfy is refused.
    #[test]
    fn a_language_present_on_one_selected_title_does_not_block_the_batch() {
        let d = disc_with_titles(vec![
            title_with_audio(&["eng"]),
            title_with_audio(&["jpn", "eng"]),
        ]);
        let j = Job {
            selection: Selection::All,
            ..audio_job(&["jpn"])
        };
        assert!(
            preflight(&d, &j).is_ready(),
            "one title carries the language; the rip can honour the request"
        );

        // Selecting only the title that lacks it is refused, though.
        let j = Job {
            selection: Selection::Titles(vec![0]),
            ..audio_job(&["jpn"])
        };
        assert!(
            preflight(&d, &j)
                .reasons()
                .iter()
                .any(|r| r.key == "language-unmatched"),
            "the only selected title has no Japanese audio"
        );
    }

    /// A title with NO audio at all is not a language miss: there was never a
    /// track to keep, and blocking would refuse every silent-title rip.
    #[test]
    fn a_class_the_disc_lacks_entirely_is_not_a_language_miss() {
        let d = disc_with_titles(vec![title_with_audio(&[])]);
        assert!(
            preflight(&d, &audio_job(&["jpn"])).is_ready(),
            "a title with no audio streams cannot 'miss' an audio language"
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
