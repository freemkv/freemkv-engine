use super::*;
use libfreemkv::{
    AudioChannels, AudioStream, Codec, LabelQualifier, SampleRate, Stream, SubtitleStream,
};

// ISO 639-2's complete set of languages whose bibliographic (/B) code differs from its
// terminologic (/T) code: `(639-2/B, 639-2/T, 639-1)`.
const BIB_TERM_ISO1: [(&str, &str, &str); 20] = [
    ("alb", "sqi", "sq"), // Albanian
    ("arm", "hye", "hy"), // Armenian
    ("baq", "eus", "eu"), // Basque
    ("bur", "mya", "my"), // Burmese
    ("chi", "zho", "zh"), // Chinese
    ("cze", "ces", "cs"), // Czech
    ("dut", "nld", "nl"), // Dutch
    ("fre", "fra", "fr"), // French
    ("geo", "kat", "ka"), // Georgian
    ("ger", "deu", "de"), // German
    ("gre", "ell", "el"), // Greek, Modern
    ("ice", "isl", "is"), // Icelandic
    ("mac", "mkd", "mk"), // Macedonian
    ("mao", "mri", "mi"), // Maori
    ("may", "msa", "ms"), // Malay
    ("per", "fas", "fa"), // Persian
    ("rum", "ron", "ro"), // Romanian
    ("slo", "slk", "sk"), // Slovak
    ("tib", "bod", "bo"), // Tibetan
    ("wel", "cym", "cy"), // Welsh
];

/// The 639-2/B codes the table claims to cover. Inputs only — the mapping
/// itself is never restated here, or this would be a second copy of the
/// table that agrees with any edit to it.
const BIB_CODES: [&str; 20] = [
    "alb", "arm", "baq", "bur", "cze", "chi", "dut", "fre", "geo", "ger", "gre", "ice", "mac",
    "mao", "may", "per", "rum", "slo", "tib", "wel",
];

// A forced language the disc lacks must be REPORTED: the two subtitle sides are chosen
// independently, so a hit on the full side says nothing about the forced one.
#[test]
fn a_forced_language_the_disc_lacks_is_reported_on_its_own_side() {
    let t = forced_title(); // deu/eng full, eng/deu forced
    let choice = StreamChoice {
        audio: StreamFilter::All,
        subtitles: SubtitleFilter::split(
            StreamFilter::Langs(vec!["deu".into()]), // present on the full side
            // `fra` exists ONLY as a full subtitle, never as a forced one.
            // If the two sides leaked into each other this would look like
            // a match and report nothing.
            StreamFilter::Langs(vec!["fra".into()]),
        ),
    };
    let miss = choice.unmatched(&t);
    assert_eq!(miss.len(), 1, "exactly the forced side missed: {miss:?}");
    assert_eq!(
        miss[0].class, "subtitle_forced",
        "it must name the FORCED side, or the message tells the user to fix \
             a request that was already satisfied"
    );
    assert_eq!(miss[0].requested, vec!["fra".to_string()]);
    assert!(
        !miss[0].available.iter().any(|l| l == "fra"),
        "the languages offered as alternatives must be the FORCED ones the \
             disc actually has, not the full-subtitle list: {:?}",
        miss[0].available
    );

    // And the full side alone still reports independently.
    let other = StreamChoice {
        audio: StreamFilter::All,
        subtitles: SubtitleFilter::split(
            // `jpn` exists only as FORCED, so the full side must miss it.
            StreamFilter::Langs(vec!["jpn".into()]),
            StreamFilter::Langs(vec!["eng".into()]),
        ),
    };
    let miss2 = other.unmatched(&t);
    assert_eq!(miss2.len(), 1);
    assert_eq!(miss2[0].class, "subtitle");
}

/// Every 639-2/B code a disc may carry must reach the language the ISO
/// standard says it names — checked against the standard's own /B ↔ 639-1
/// pairing, never against `bib_to_terminologic`.
#[test]
fn every_bibliographic_code_resolves_to_its_terminologic_language() {
    for (bib, term, iso1) in BIB_TERM_ISO1 {
        let via_bib = normalize_lang(bib).unwrap_or_else(|| panic!("{bib}: no language resolved"));

        // The oracle: the 639-1 code the STANDARD pairs with this /B code,
        // resolved by isolang without consulting anything in this crate.
        let oracle = Language::from_639_1(iso1)
            .unwrap_or_else(|| panic!("{iso1}: not a 639-1 code isolang knows"));
        assert_eq!(
            via_bib, oracle,
            "-a {bib} must select {iso1} ({oracle:?}); it selected {via_bib:?}"
        );

        // And the /T form the standard pairs with it, stated independently
        // of the production table, is the same language and is what the
        // resolved identity reports itself as.
        assert_eq!(
            via_bib.to_639_3(),
            term,
            "{bib}'s terminologic form is {term} per ISO 639-2"
        );
    }
}

// The production table must cover the standard's set exactly: every /B
// code in it, no invented rows. A missing row silently matches nothing;
// an extra row is a code the standard doesn't define as bibliographic.
#[test]
fn the_bibliographic_table_covers_exactly_the_standard_set() {
    for (bib, term, _) in BIB_TERM_ISO1 {
        assert_eq!(
            bib_to_terminologic(bib),
            Some(term),
            "the /B → /T table disagrees with ISO 639-2 for {bib}"
        );
    }
    // Codes that are NOT /B-vs-/T divergences must not be in the table:
    // `eng`/`spa`/`jpn`/`nor` are their own /T forms, `qqq` is not a
    // language, and a stray row would shadow a real 639-3 code.
    for not_bib in ["eng", "spa", "jpn", "nor", "deu", "fra", "zho", "qqq"] {
        assert_eq!(
            bib_to_terminologic(not_bib),
            None,
            "{not_bib} is not an ISO 639-2 bibliographic-only code"
        );
    }
}

// Each arm has to be load-bearing. `normalize_lang` tries `from_639_3`
// BEFORE the table, so an entry `isolang` already resolves on its own is
// unreachable — a table with dead rows is one nobody can tell is correct.
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
    sub_flagged(pid, lang, false)
}
fn forced_sub(pid: u16, lang: &str) -> Stream {
    sub_flagged(pid, lang, true)
}
fn sub_flagged(pid: u16, lang: &str, forced: bool) -> Stream {
    Stream::Subtitle(SubtitleStream {
        pid,
        codec: Codec::Pgs,
        language: lang.into(),
        forced,
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

// The refusal reports an untagged track as "und"; asking for "und" keeps it.
#[test]
fn an_untagged_track_is_kept_by_a_request_for_und() {
    let mut t = title();
    t.streams.push(audio(0x1104, ""));
    let und = StreamFilter::Langs(vec!["und".into()]);
    let sel = resolve_stream_selection(&t, &und, &StreamFilter::None).unwrap();
    assert_eq!(sel.audio, PidFilter::Only(vec![0x1104]));
    assert!(choice(und, StreamFilter::None).unmatched(&t).is_empty());
}

#[test]
fn all_maps_to_pidfilter_all() {
    let sel = resolve_stream_selection(&title(), &StreamFilter::All, &StreamFilter::All).unwrap();
    assert_eq!(sel.audio, PidFilter::All);
    assert_eq!(sel.subtitle, PidFilter::All);
}

#[test]
fn none_maps_to_only_empty() {
    let sel = resolve_stream_selection(&title(), &StreamFilter::None, &StreamFilter::None).unwrap();
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
    StreamChoice {
        audio,
        subtitles: subtitles.into(),
    }
}

#[test]
fn unmatched_flags_a_language_absent_from_a_present_class() {
    // Audio jpn is absent (title has eng/spa/fra) → flagged, with the real
    // languages listed. Subtitles All → never flagged.
    let u = choice(StreamFilter::Langs(vec!["jpn".into()]), StreamFilter::All).unmatched(&title());
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

// A class whose streams exist but carry NO language tag is a MISS, not an absent class
// (untagged audio must not look like no audio at all).
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

// Guards the one-line delegation from `StreamChoice::resolve` to the free function every
// other test calls directly, so it can't silently become `Ok(Default::default())`.
#[test]
fn resolve_delegates_to_resolve_stream_selection() {
    let t = title();
    let choice = StreamChoice {
        audio: StreamFilter::Langs(vec!["fre".into()]),
        subtitles: StreamFilter::None.into(),
    };
    let via_method = choice.resolve(&t).unwrap();
    let via_function =
        resolve_stream_selection_forced(&t, &choice.audio, &choice.subtitles).unwrap();
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

/// A disc with the same language on BOTH sides of the forced flag, plus one
/// language that exists only non-forced (fra) and one only forced (jpn) —
/// the four cases the split has to tell apart.
fn forced_title() -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.streams = vec![
        audio(0x1100, "eng"),
        audio(0x1101, "deu"),
        audio(0x1102, "spa"),
        sub(0x1200, "eng"),
        sub(0x1201, "deu"),
        sub(0x1202, "fra"), // fra exists ONLY as a full subtitle
        forced_sub(0x1210, "eng"),
        forced_sub(0x1211, "deu"),
        forced_sub(0x1212, "jpn"), // jpn exists ONLY as a forced subtitle
    ];
    t
}

// The request this split exists for, verbatim: "German & Spanish audio, only German
// subtitles, and forced only if in English."
#[test]
fn german_spanish_audio_german_subs_forced_english() {
    let sel = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::Langs(vec!["German".into(), "Spanish".into()]),
        &SubtitleFilter::split(
            StreamFilter::Langs(vec!["de".into()]),
            StreamFilter::Langs(vec!["en".into()]),
        ),
    )
    .unwrap();
    assert_eq!(
        sel.audio,
        PidFilter::Only(vec![0x1101, 0x1102]),
        "both listed audio languages must be kept"
    );
    assert_eq!(
        sel.subtitle,
        PidFilter::Only(vec![0x1201, 0x1210]),
        "German full subtitle UNION English forced subtitle"
    );
}

/// jpn is on the disc only as a forced subtitle. The normal side must not
/// reach it (that side selects full subtitles, and there is no full Japanese
/// one), while the forced side must.
#[test]
fn language_present_only_as_forced() {
    let normal_side = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::Langs(vec!["jpn".into()]), StreamFilter::None),
    )
    .unwrap();
    assert_eq!(
        normal_side.subtitle,
        PidFilter::Only(vec![]),
        "a forced-only language must not be reachable from the normal side"
    );

    let forced_side = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::None, StreamFilter::Langs(vec!["jpn".into()])),
    )
    .unwrap();
    assert_eq!(forced_side.subtitle, PidFilter::Only(vec![0x1212]));
}

/// The mirror image: fra is on the disc only as a full subtitle, so the
/// forced side must not reach it. Without this, a `forced` side that
/// silently ignored the flag would look correct in the test above.
#[test]
fn language_present_only_as_non_forced() {
    let forced_side = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::None, StreamFilter::Langs(vec!["fra".into()])),
    )
    .unwrap();
    assert_eq!(
        forced_side.subtitle,
        PidFilter::Only(vec![]),
        "a non-forced-only language must not be reachable from the forced side"
    );

    let normal_side = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::Langs(vec!["fra".into()]), StreamFilter::None),
    )
    .unwrap();
    assert_eq!(normal_side.subtitle, PidFilter::Only(vec![0x1202]));
}

/// Either side may be empty, and an empty side removes nothing from the
/// other — "German subtitles, no forced ones" and "no subtitles except
/// forced German" are both expressible, and both-empty keeps nothing.
#[test]
fn an_empty_side_selects_nothing_and_takes_nothing_from_the_other() {
    let subs = |f: SubtitleFilter| {
        resolve_stream_selection_forced(&forced_title(), &StreamFilter::None, &f)
            .unwrap()
            .subtitle
    };
    assert_eq!(
        subs(SubtitleFilter::split(
            StreamFilter::Langs(vec!["deu".into()]),
            StreamFilter::None
        )),
        PidFilter::Only(vec![0x1201]),
        "empty forced side must drop the forced German subtitle only"
    );
    assert_eq!(
        subs(SubtitleFilter::split(
            StreamFilter::None,
            StreamFilter::Langs(vec!["deu".into()])
        )),
        PidFilter::Only(vec![0x1211]),
        "empty normal side must drop the full German subtitle only"
    );
    assert_eq!(
        subs(SubtitleFilter::split(
            StreamFilter::None,
            StreamFilter::None
        )),
        PidFilter::Only(vec![]),
        "both sides empty keeps nothing"
    );
}

// A caller with only one subtitle list keeps today's behavior exactly: the
// `From` conversion applies it to both sides, so forcedness is ignored.
#[test]
fn a_plain_filter_converts_to_a_forced_agnostic_split() {
    let t = forced_title();
    for f in [
        StreamFilter::All,
        StreamFilter::None,
        StreamFilter::Langs(vec!["deu".into()]),
    ] {
        let legacy = resolve_stream_selection(&t, &StreamFilter::All, &f).unwrap();
        let split =
            resolve_stream_selection_forced(&t, &StreamFilter::All, &f.clone().into()).unwrap();
        assert_eq!(legacy.subtitle, split.subtitle, "{f:?} must not change");
    }
    let all = resolve_stream_selection(&t, &StreamFilter::All, &StreamFilter::All).unwrap();
    assert_eq!(all.subtitle, PidFilter::All);
    let none = resolve_stream_selection(&t, &StreamFilter::All, &StreamFilter::None).unwrap();
    assert_eq!(none.subtitle, PidFilter::Only(vec![]));
    let deu = resolve_stream_selection(
        &t,
        &StreamFilter::All,
        &StreamFilter::Langs(vec!["deu".into()]),
    )
    .unwrap();
    assert_eq!(
        deu.subtitle,
        PidFilter::Only(vec![0x1201, 0x1211]),
        "one list means forcedness is ignored: both German subtitles"
    );
}

/// `All` on one side only is still an honest selection: every non-forced
/// subtitle, no forced ones (the "I never want forced subs" case).
#[test]
fn all_on_one_side_enumerates_that_side_only() {
    let sel = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::All, StreamFilter::None),
    )
    .unwrap();
    assert_eq!(sel.subtitle, PidFilter::Only(vec![0x1200, 0x1201, 0x1202]));
}

/// A typo on the forced side is an error even when the disc has no forced
/// subtitle in that language — tags are validated before streams are read.
#[test]
fn unknown_language_on_the_forced_side_errors() {
    let err = resolve_stream_selection_forced(
        &forced_title(),
        &StreamFilter::All,
        &SubtitleFilter::split(
            StreamFilter::None,
            StreamFilter::Langs(vec!["Klingonish".into()]),
        ),
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

// An unknown tag surfaces as E9083 carrying the tag, as an InvalidInput io::Error.
#[test]
fn an_unknown_language_converts_to_its_code() {
    let e = StreamSelError::UnknownLanguage {
        tag: "Klingonish".into(),
    };
    let io: std::io::Error = libfreemkv::Error::from(e).into();
    assert_eq!(io.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        crate::error_code(&io),
        Some(libfreemkv::error::E_STREAM_LANGUAGE_UNKNOWN)
    );
    assert_eq!(io.to_string(), "E9083: Klingonish");
}
