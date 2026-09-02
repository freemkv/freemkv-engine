//! Exercises forced-subtitle stream selection through the PUBLIC API rather
//! than through crate internals: audio as an independent language set, a
//! forced-subtitle language set independent of the non-forced one, and the
//! legacy single-list path unchanged.
//!
//! See docs/stream-selection-forced.md for the user request that motivated
//! this coverage and why each case matters. The unit tests in `streams.rs`
//! cover the matcher; this file covers that the types are nameable,
//! constructible and wired to the same behaviour from outside the crate.

use freemkv_engine::{
    PidFilter, StreamFilter, SubtitleFilter, resolve_stream_selection,
    resolve_stream_selection_forced,
};
use libfreemkv::{
    AudioChannels, AudioStream, Codec, DiscTitle, LabelQualifier, SampleRate, Stream,
    SubtitleStream,
};

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

fn sub(pid: u16, lang: &str, forced: bool) -> Stream {
    Stream::Subtitle(SubtitleStream {
        pid,
        codec: Codec::Pgs,
        language: lang.into(),
        forced,
        qualifier: LabelQualifier::None,
        codec_data: None,
    })
}

/// A disc shaped like the one the request is about: several audio languages,
/// and German/English subtitles in both forced and non-forced form.
fn multilingual_title() -> DiscTitle {
    let mut t = DiscTitle::empty();
    t.streams = vec![
        audio(0x1100, "deu"),
        audio(0x1101, "spa"),
        audio(0x1102, "eng"),
        sub(0x1200, "deu", false),
        sub(0x1201, "eng", false),
        sub(0x1202, "eng", true),
        sub(0x1203, "deu", true),
    ];
    t
}

fn only(f: &PidFilter) -> Vec<u16> {
    match f {
        PidFilter::Only(v) => {
            let mut v = v.clone();
            v.sort_unstable();
            v
        }
        PidFilter::All => panic!("expected an enumerated selection, got All"),
    }
}

/// The request, end to end.
#[test]
fn german_and_spanish_audio_german_subtitles_and_forced_english_only() {
    let t = multilingual_title();
    let sel = resolve_stream_selection_forced(
        &t,
        &StreamFilter::Langs(vec!["German".into(), "Spanish".into()]),
        &SubtitleFilter::split(
            StreamFilter::Langs(vec!["German".into()]),
            StreamFilter::Langs(vec!["English".into()]),
        ),
    )
    .expect("every language here is real");

    assert_eq!(
        only(&sel.audio),
        vec![0x1100, 0x1101],
        "audio is a SET — German AND Spanish, and NOT the English track"
    );
    assert_eq!(
        only(&sel.subtitle),
        vec![0x1200, 0x1202],
        "non-forced German (0x1200) plus forced English (0x1202) — and neither \
         the non-forced English (0x1201) nor the forced German (0x1203), which \
         is what proves the two sides are independent"
    );
}

/// The independence claim, from the other direction: asking for forced English
/// must not drag in the non-forced English track, and asking for German
/// subtitles must not drag in the forced German one.
#[test]
fn each_side_selects_only_its_own_forcedness() {
    let t = multilingual_title();

    let forced_only = resolve_stream_selection_forced(
        &t,
        &StreamFilter::None,
        &SubtitleFilter::split(
            StreamFilter::None,
            StreamFilter::Langs(vec!["English".into()]),
        ),
    )
    .expect("resolves");
    assert_eq!(
        only(&forced_only.subtitle),
        vec![0x1202],
        "forced English only — the non-forced English track must not appear"
    );

    let normal_only = resolve_stream_selection_forced(
        &t,
        &StreamFilter::None,
        &SubtitleFilter::split(
            StreamFilter::Langs(vec!["German".into()]),
            StreamFilter::None,
        ),
    )
    .expect("resolves");
    assert_eq!(
        only(&normal_only.subtitle),
        vec![0x1200],
        "non-forced German only — the forced German track must not appear"
    );
}

// The legacy path must be untouched: one list still applies to both sides
// (existing callers and the CLI's `-a`/`--subtitles` behave as before). A
// regression here is silent — a saved selection quietly narrows.
#[test]
fn a_single_list_still_means_both_forced_and_not() {
    let t = multilingual_title();
    let legacy = resolve_stream_selection(
        &t,
        &StreamFilter::None,
        &StreamFilter::Langs(vec!["English".into()]),
    )
    .expect("resolves");
    assert_eq!(
        only(&legacy.subtitle),
        vec![0x1201, 0x1202],
        "one list means English subtitles WHETHER OR NOT they are forced"
    );

    // And the same request routed through the split type must agree.
    let via_split = resolve_stream_selection_forced(
        &t,
        &StreamFilter::None,
        &StreamFilter::Langs(vec!["English".into()]).into(),
    )
    .expect("resolves");
    assert_eq!(
        only(&via_split.subtitle),
        only(&legacy.subtitle),
        "the compatibility conversion must not change the answer"
    );
}

/// `All` and `None` are the two settings a user can reach without typing a
/// language, so they must survive the new dimension unchanged.
#[test]
fn all_and_none_are_unchanged_by_the_forced_dimension() {
    let t = multilingual_title();

    let all = resolve_stream_selection(&t, &StreamFilter::All, &StreamFilter::All).expect("ok");
    assert!(
        matches!(all.audio, PidFilter::All) && matches!(all.subtitle, PidFilter::All),
        "All must stay the wildcard, not become an enumerated list"
    );

    let none = resolve_stream_selection(&t, &StreamFilter::None, &StreamFilter::None).expect("ok");
    assert!(only(&none.audio).is_empty() && only(&none.subtitle).is_empty());
}

/// A typo on the FORCED side must be reported even on a disc that has no forced
/// subtitles at all — otherwise the mistake is silently ignored and the user
/// gets no forced track without ever being told why.
#[test]
fn an_unknown_language_on_the_forced_side_is_still_an_error() {
    let mut t = DiscTitle::empty();
    t.streams = vec![sub(0x1200, "deu", false)];

    let err = resolve_stream_selection_forced(
        &t,
        &StreamFilter::None,
        &SubtitleFilter::split(
            StreamFilter::None,
            StreamFilter::Langs(vec!["notalanguage".into()]),
        ),
    )
    .expect_err("an unknown tag must not be silently dropped");
    assert!(
        matches!(err, freemkv_engine::StreamSelError::UnknownLanguage { .. }),
        "expected UnknownLanguage, got {err:?}"
    );
}
