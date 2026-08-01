//! Translate the [`Job`](crate::Job)'s language-based stream policy into the
//! library's PID primitive (`libfreemkv::StreamSelection`) for one scanned
//! title. Pure — no I/O. Language identity is resolved with `isolang` so
//! `-a English`, `-a en`, and `-a eng` all match a stream tagged `eng`.

use crate::job::{StreamChoice, StreamFilter};
use isolang::Language;
use libfreemkv::{PidFilter, StreamSelection};

impl StreamChoice {
    /// Translate this choice into the library's PID [`StreamSelection`] for one
    /// scanned title. See [`resolve_stream_selection`].
    pub fn resolve(
        &self,
        title: &libfreemkv::DiscTitle,
    ) -> Result<StreamSelection, StreamSelError> {
        resolve_stream_selection(title, &self.audio, &self.subtitles)
    }

    /// Report the language-filtered classes that matched NO stream on `title`
    /// while the title HAS streams of that class — i.e. the user asked for a
    /// language that isn't present, so an unguarded rip would ship a file
    /// missing that whole track class with no hint why. See [`UnmatchedClass`].
    ///
    /// Empty result = nothing to warn about: every class was `all`/`none`, or
    /// its language filter matched at least one stream, or the title simply has
    /// no streams of that class (nothing could match).
    pub fn unmatched(&self, title: &libfreemkv::DiscTitle) -> Vec<UnmatchedClass> {
        let mut out = Vec::new();
        check_class(title, &self.audio, StreamClass::Audio, &mut out);
        check_class(title, &self.subtitles, StreamClass::Subtitle, &mut out);
        out
    }
}

/// A language-filtered class (audio or subtitle) where the requested languages
/// matched no stream on a title that DOES carry that class. The front-end turns
/// this into a hard error (a single-title rip) or a warn-and-skip (a batch), so
/// a typo — or a language simply absent from one title — never silently ships a
/// track-less file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmatchedClass {
    /// Stable class key the front-end localizes: `"audio"` or `"subtitle"`.
    pub class: &'static str,
    /// The language tags the user requested, verbatim.
    pub requested: Vec<String>,
    /// The languages actually present on the title for this class (sorted,
    /// deduped) — what to show so the user can correct the request.
    pub available: Vec<String>,
}

fn check_class(
    title: &libfreemkv::DiscTitle,
    sel: &StreamFilter,
    class: StreamClass,
    out: &mut Vec<UnmatchedClass>,
) {
    // Only an explicit language filter can "miss"; all/none are always honored.
    let StreamFilter::Langs(tags) = sel else {
        return;
    };
    // Keep EVERY stream of the class, including ones with an empty language tag.
    // The distinction matters: "this title has no audio at all" is not a miss,
    // but "this title has audio that carries no language tag" is — a language
    // filter cannot match an untagged track, so the resolved selection keeps
    // nothing. Filtering the empties out here made the two cases identical, so
    // an untagged title returned early, reported no miss, and shipped a
    // video-only file at exit 0 — the exact silent-loss this check exists to
    // prevent. DVDs authored with zero language bytes hit it (libfreemkv emits
    // `language: ""` for them).
    let present: Vec<String> = class_languages(title, class);
    if present.is_empty() {
        // The title has no streams of this class at all — nothing to match, and
        // not a "wrong file": there was never a track to keep.
        return;
    }
    let wanted: Vec<Language> = tags.iter().filter_map(|t| normalize_lang(t)).collect();
    let any_match = present
        .iter()
        .any(|l| normalize_lang(l).is_some_and(|pl| wanted.contains(&pl)));
    if !any_match {
        // Report an untagged track as "und" (ISO 639-2 undetermined) rather than
        // as a blank, so the message reads "available audio: und" instead of
        // trailing off — the user needs to see that tracks exist but carry no
        // language, which is why their filter matched nothing.
        let mut available: Vec<String> = present
            .into_iter()
            .map(|l| if l.is_empty() { "und".to_string() } else { l })
            .collect();
        available.sort();
        available.dedup();
        out.push(UnmatchedClass {
            class: class.key(),
            requested: tags.clone(),
            available,
        });
    }
}

/// The raw language tags of every stream of `class` on `title` (order preserved,
/// not yet deduped). Empty tags are RETAINED — see `unmatched_for_class`: an
/// untagged stream still counts as a stream of the class, and dropping it here
/// makes "no tracks" indistinguishable from "no *tagged* tracks".
fn class_languages(title: &libfreemkv::DiscTitle, class: StreamClass) -> Vec<String> {
    match class {
        StreamClass::Audio => title.audio_streams().map(|a| a.language.clone()).collect(),
        StreamClass::Subtitle => title
            .subtitle_streams()
            .map(|s| s.language.clone())
            .collect(),
    }
}

/// What can go wrong translating a language policy. `Sink`-renderable data, not
/// prose — the front-end localizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamSelError {
    /// A requested tag resolves to no known language — a typo (`"Klingonish"`),
    /// not a disc property. Hard error.
    UnknownLanguage { tag: String },
}

/// Translate the audio + subtitle policy for ONE scanned title into a lib
/// `StreamSelection` (PIDs). A `Langs` tag that resolves to no known language
/// is an [`StreamSelError::UnknownLanguage`]. A resolvable tag that simply has
/// no matching stream on THIS title yields no PIDs for it here — the caller
/// (preflight) decides whether that is a disc-wide error or a per-title skip.
pub fn resolve_stream_selection(
    title: &libfreemkv::DiscTitle,
    audio: &StreamFilter,
    subtitles: &StreamFilter,
) -> Result<StreamSelection, StreamSelError> {
    Ok(StreamSelection {
        audio: resolve_class(title, audio, StreamClass::Audio)?,
        subtitle: resolve_class(title, subtitles, StreamClass::Subtitle)?,
    })
}

#[derive(Clone, Copy)]
enum StreamClass {
    Audio,
    Subtitle,
}

impl StreamClass {
    /// Stable, localizable key for this class.
    fn key(self) -> &'static str {
        match self {
            StreamClass::Audio => "audio",
            StreamClass::Subtitle => "subtitle",
        }
    }
}

fn resolve_class(
    title: &libfreemkv::DiscTitle,
    sel: &StreamFilter,
    class: StreamClass,
) -> Result<PidFilter, StreamSelError> {
    match sel {
        StreamFilter::All => Ok(PidFilter::All),
        StreamFilter::None => Ok(PidFilter::Only(vec![])),
        StreamFilter::Langs(tags) => {
            // Resolve every tag first (surfacing typos before touching streams).
            let wanted: Vec<Language> = tags
                .iter()
                .map(|t| {
                    normalize_lang(t)
                        .ok_or_else(|| StreamSelError::UnknownLanguage { tag: t.clone() })
                })
                .collect::<Result<_, _>>()?;

            // Iterate the class's streams via the lib accessors (cleaner than a
            // Stream-enum match); keep those whose language matches any tag.
            let matches = |lang: &str, pid: u16| -> Option<u16> {
                normalize_lang(lang)
                    .filter(|l| wanted.contains(l))
                    .map(|_| pid)
            };
            let pids: Vec<u16> = match class {
                StreamClass::Audio => title
                    .audio_streams()
                    .filter_map(|a| matches(&a.language, a.pid))
                    .collect(),
                StreamClass::Subtitle => title
                    .subtitle_streams()
                    .filter_map(|s| matches(&s.language, s.pid))
                    .collect(),
            };
            Ok(PidFilter::Only(pids))
        }
    }
}

/// Normalize a language tag (a name, 639-1, 639-2/T, 639-2/B, or 639-3 code) to
/// a language identity, case-insensitively. `None` if unrecognized.
fn normalize_lang(tag: &str) -> Option<Language> {
    let t = tag.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // 639-1 two-letter, then 639-3/639-2-T three-letter.
    Language::from_639_1(&lower)
        .or_else(|| Language::from_639_3(&lower))
        // 639-2/B bibliographic (fre, ger, dut, …) → 639-3/T, then resolve.
        .or_else(|| bib_to_terminologic(&lower).and_then(Language::from_639_3))
        // Full English name, case-insensitive.
        .or_else(|| Language::from_name_lowercase(&lower))
}

/// The ISO 639-2/B (bibliographic) codes that differ from 639-2/T (=639-3),
/// mapped to their /T form so `isolang::from_639_3` can resolve them.
fn bib_to_terminologic(code: &str) -> Option<&'static str> {
    Some(match code {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "cze" => "ces",
        "chi" => "zho",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, LabelQualifier, SampleRate, Stream, SubtitleStream,
    };

    /// The 639-2/B codes the table claims to cover. Inputs only — the mapping
    /// itself is never restated here, or this would be a second copy of the
    /// table that agrees with any edit to it.
    const BIB_CODES: [&str; 20] = [
        "alb", "arm", "baq", "bur", "cze", "chi", "dut", "fre", "geo", "ger", "gre", "ice", "mac",
        "mao", "may", "per", "rum", "slo", "tib", "wel",
    ];

    /// Discs label tracks with bibliographic codes (`ger`, `fre`, `chi`) as
    /// often as terminologic ones, so `-a ger` has to reach a German track.
    /// Only `fre` was covered: every other arm could be deleted and the suite
    /// stayed green, which is a track the user asked for silently not matching.
    ///
    /// `isolang` is the oracle rather than a restated mapping — the property is
    /// that a bibliographic tag and the /T form the table sends it to name the
    /// SAME language.
    #[test]
    fn every_bibliographic_code_resolves_to_its_terminologic_language() {
        for bib in BIB_CODES {
            let via_bib =
                normalize_lang(bib).unwrap_or_else(|| panic!("{bib}: no language resolved"));
            let term = bib_to_terminologic(bib)
                .unwrap_or_else(|| panic!("{bib}: not in the /B → /T table"));
            let via_term = Language::from_639_3(term)
                .unwrap_or_else(|| panic!("{bib} → {term}: not a 639-3 code"));
            assert_eq!(
                via_bib, via_term,
                "{bib} resolved to {via_bib:?} but its /T form {term} is {via_term:?}"
            );
        }
    }

    /// Each arm has to be load-bearing. `normalize_lang` tries `from_639_3`
    /// BEFORE the table, so an entry `isolang` already resolves on its own is
    /// unreachable — and a table with dead rows in it is a table nobody can
    /// tell is still correct.
    #[test]
    fn no_bibliographic_arm_is_dead() {
        for bib in BIB_CODES {
            assert!(
                Language::from_639_1(bib).is_none() && Language::from_639_3(bib).is_none(),
                "{bib} resolves without the table — its arm is unreachable"
            );
        }
    }

    /// The negative side: an unknown three-letter tag must stay unresolved
    /// rather than fall through to some other language.
    #[test]
    fn unknown_three_letter_tags_do_not_resolve() {
        for tag in ["zzz", "qqq", "xyz"] {
            assert!(normalize_lang(tag).is_none(), "{tag} unexpectedly resolved");
        }
    }

    fn audio(pid: u16, lang: &str) -> Stream {
        Stream::Audio(AudioStream {
            pid,
            codec: Codec::TrueHd,
            channels: AudioChannels::Stereo,
            language: lang.into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }
    fn sub(pid: u16, lang: &str) -> Stream {
        Stream::Subtitle(SubtitleStream {
            pid,
            codec: Codec::Pgs,
            language: lang.into(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        })
    }

    // video(0x1011) + audio eng/spa/fra/eng-commentary + sub eng/fra.
    fn title() -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.streams = vec![
            audio(0x1100, "eng"),
            audio(0x1101, "spa"),
            audio(0x1102, "fra"),
            audio(0x1103, "eng"), // e.g. a commentary track, same language
            sub(0x1200, "eng"),
            sub(0x1201, "fra"),
        ];
        t
    }

    #[test]
    fn all_maps_to_pidfilter_all() {
        let sel =
            resolve_stream_selection(&title(), &StreamFilter::All, &StreamFilter::All).unwrap();
        assert_eq!(sel.audio, PidFilter::All);
        assert_eq!(sel.subtitle, PidFilter::All);
    }

    #[test]
    fn none_maps_to_only_empty() {
        let sel =
            resolve_stream_selection(&title(), &StreamFilter::None, &StreamFilter::None).unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![]));
        assert_eq!(sel.subtitle, PidFilter::Only(vec![]));
    }

    #[test]
    fn full_name_code1_code3_all_match_same_language() {
        for tag in ["English", "english", "en", "eng", "ENG"] {
            let sel = resolve_stream_selection(
                &title(),
                &StreamFilter::Langs(vec![tag.into()]),
                &StreamFilter::All,
            )
            .unwrap();
            // Both eng audio streams (0x1100 and the 0x1103 commentary) match.
            assert_eq!(
                sel.audio,
                PidFilter::Only(vec![0x1100, 0x1103]),
                "tag {tag} should match both eng audio streams"
            );
        }
    }

    #[test]
    fn bibliographic_variant_unifies_with_terminologic() {
        // Request "fre" (639-2/B); stream tagged "fra" (639-2/T) must match.
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["fre".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1102]));
    }

    #[test]
    fn multiple_langs_select_union_in_stream_order() {
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["spa".into(), "fra".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1101, 0x1102]));
    }

    #[test]
    fn subtitle_langs_are_independent_of_audio() {
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["eng".into()]),
            &StreamFilter::Langs(vec!["fra".into()]),
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1100, 0x1103]));
        assert_eq!(sel.subtitle, PidFilter::Only(vec![0x1201]));
    }

    #[test]
    fn unknown_language_tag_errors() {
        let err = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["Klingonish".into()]),
            &StreamFilter::All,
        )
        .unwrap_err();
        assert_eq!(
            err,
            StreamSelError::UnknownLanguage {
                tag: "Klingonish".into()
            }
        );
    }

    #[test]
    fn lang_missing_on_title_yields_no_pids_for_that_class() {
        // Japanese not on this title: resolves fine (known language), selects
        // nothing here. Preflight decides if that's a disc-wide error.
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["jpn".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![]));
    }

    fn choice(audio: StreamFilter, subtitles: StreamFilter) -> StreamChoice {
        StreamChoice { audio, subtitles }
    }

    #[test]
    fn unmatched_flags_a_language_absent_from_a_present_class() {
        // Audio jpn is absent (title has eng/spa/fra) → flagged, with the real
        // languages listed. Subtitles All → never flagged.
        let u =
            choice(StreamFilter::Langs(vec!["jpn".into()]), StreamFilter::All).unmatched(&title());
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].class, "audio");
        assert_eq!(u[0].requested, vec!["jpn".to_string()]);
        assert_eq!(
            u[0].available,
            vec!["eng".to_string(), "fra".into(), "spa".into()]
        );
    }

    #[test]
    fn unmatched_is_empty_when_a_requested_language_is_present() {
        // eng present → no audio flag; fra present as a subtitle → no sub flag.
        let u = choice(
            StreamFilter::Langs(vec!["eng".into()]),
            StreamFilter::Langs(vec!["fra".into()]),
        )
        .unmatched(&title());
        assert!(u.is_empty(), "got {u:?}");
    }

    #[test]
    fn unmatched_ignores_all_and_none() {
        // all/none are always honored — a user asking for none WANTS none.
        assert!(
            choice(StreamFilter::All, StreamFilter::None)
                .unmatched(&title())
                .is_empty()
        );
        assert!(
            choice(StreamFilter::None, StreamFilter::All)
                .unmatched(&title())
                .is_empty()
        );
    }

    #[test]
    fn unmatched_skips_a_class_the_title_lacks_entirely() {
        // A title with audio but NO subtitles: -s eng can't "miss" — there was
        // never a subtitle track to keep, so it is not flagged.
        let mut t = libfreemkv::DiscTitle::empty();
        t.streams = vec![audio(0x1100, "eng")];
        let u = choice(StreamFilter::All, StreamFilter::Langs(vec!["eng".into()])).unmatched(&t);
        assert!(u.is_empty(), "got {u:?}");
    }

    /// A class whose streams exist but carry NO language tag is a MISS, not an
    /// absent class.
    ///
    /// Regression: `class_languages` used to filter empty tags out, so a title
    /// with untagged audio looked identical to a title with no audio — the
    /// unmatched check returned early, nothing was reported, and `resolve`
    /// produced `Only([])`, i.e. keep nothing. `-a eng` on such a disc therefore
    /// wrote a video-only file and exited 0, which is precisely the silent
    /// track-loss the fail-loud work was meant to end. DVDs authored with zero
    /// language bytes reach this (libfreemkv emits `language: ""`).
    #[test]
    fn unmatched_flags_a_class_whose_streams_are_all_untagged() {
        let mut t = libfreemkv::DiscTitle::empty();
        // Two real audio tracks, neither carrying a language tag.
        t.streams = vec![audio(0x1100, ""), audio(0x1101, "")];
        let u = choice(StreamFilter::Langs(vec!["eng".into()]), StreamFilter::All).unmatched(&t);
        assert_eq!(
            u.len(),
            1,
            "untagged audio must be reported as unmatched, got {u:?}"
        );
        assert_eq!(u[0].class, "audio");
        assert_eq!(
            u[0].available,
            vec!["und".to_string()],
            "an untagged track must surface as 'und', not as a blank"
        );
    }

    /// `StreamChoice::resolve` is the method every front-end calls; the free
    /// function underneath it is what every test above calls. That left the
    /// one-line delegation replaceable with `Ok(Default::default())` — an
    /// empty selection, which for a `PidFilter::Only(vec![])` audio choice is
    /// a plausible-looking answer that silently drops every track.
    #[test]
    fn resolve_delegates_to_resolve_stream_selection() {
        let t = title();
        let choice = StreamChoice {
            audio: StreamFilter::Langs(vec!["fre".into()]),
            subtitles: StreamFilter::None,
        };
        let via_method = choice.resolve(&t).unwrap();
        let via_function = resolve_stream_selection(&t, &choice.audio, &choice.subtitles).unwrap();
        assert_eq!(via_method.audio, via_function.audio);
        assert_eq!(via_method.subtitle, via_function.subtitle);
        // Spelled out too, so the assertion above cannot pass by both sides
        // being the same wrong (default) value.
        assert_eq!(via_method.audio, PidFilter::Only(vec![0x1102]));
        assert_eq!(via_method.subtitle, PidFilter::Only(vec![]));
        assert_ne!(
            via_method.audio,
            StreamSelection::default().audio,
            "a real resolve must not coincide with the Default"
        );
    }

    #[test]
    fn unmatched_flags_both_classes_independently() {
        let u = choice(
            StreamFilter::Langs(vec!["jpn".into()]),
            StreamFilter::Langs(vec!["kor".into()]),
        )
        .unmatched(&title());
        assert_eq!(u.len(), 2);
        assert!(u.iter().any(|c| c.class == "audio"));
        assert!(u.iter().any(|c| c.class == "subtitle"));
    }
}
