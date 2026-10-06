use super::*;

#[test]
fn defaults_are_the_common_case() {
    let j = Job::new("disc:///dev/sr0", "/out");
    assert_eq!(j.mode, RipMode::Single);
    assert_eq!(j.selection, Selection::MainMovie);
    assert!(!j.raw);
}

#[test]
fn builders_compose() {
    let j = Job::new("iso://x.iso", "null://")
        .with_mode(RipMode::Multi)
        .with_selection(Selection::All);
    assert_eq!(j.mode, RipMode::Multi);
    assert_eq!(j.selection, Selection::All);
    assert_eq!(j.source, "iso://x.iso");
    assert_eq!(j.dest, "null://");
}

#[test]
fn explicit_title_selection_round_trips() {
    let j = Job::new("d", "o").with_selection(Selection::Titles(vec![0, 2, 5]));
    assert_eq!(j.selection, Selection::Titles(vec![0, 2, 5]));
}

// `is_all` is the "skip the stream filter entirely" shortcut, so it must
// mean BOTH classes keep everything — answering true for a half-filtered
// choice would ship a track the user asked to exclude.
#[test]
fn is_all_requires_both_classes_to_keep_everything() {
    let all = StreamChoice {
        audio: StreamFilter::All,
        subtitles: StreamFilter::All.into(),
    };
    assert!(all.is_all());
    assert!(
        StreamChoice::default().is_all(),
        "the default choice keeps everything"
    );
    assert!(
        !StreamChoice {
            audio: StreamFilter::None,
            subtitles: StreamFilter::All.into(),
        }
        .is_all(),
        "audio is filtered — this is not a no-op"
    );
    assert!(
        !StreamChoice {
            audio: StreamFilter::All,
            subtitles: StreamFilter::None.into(),
        }
        .is_all(),
        "subtitles are filtered — this is not a no-op"
    );
    assert!(
        !StreamChoice {
            audio: StreamFilter::Langs(vec!["eng".into()]),
            subtitles: StreamFilter::Langs(vec!["eng".into()]).into(),
        }
        .is_all()
    );
}

// The default must keep EVERYTHING, spelled out per side rather than only
// through `is_all()` — checking one through the other alone would let a
// default that quietly dropped forced subtitles agree with a broken `is_all`.
#[test]
fn default_keeps_every_side() {
    let d = StreamChoice::default();
    assert_eq!(d.audio, StreamFilter::All);
    assert_eq!(d.subtitles.normal, StreamFilter::All, "full subtitles");
    assert_eq!(d.subtitles.forced, StreamFilter::All, "forced subtitles");
    assert!(d.is_all());
    assert_eq!(
        Job::new("disc:///dev/sr0", "/out").streams,
        d,
        "a fresh Job is the archival default"
    );
}

// The bug the forced side creates: `is_all()` looking only at the normal
// side would answer TRUE for "every full subtitle, no forced ones", so
// callers skip resolving and ship exactly the forced subtitles excluded.
#[test]
fn is_all_is_false_when_only_the_forced_side_is_narrowed() {
    let no_forced = StreamChoice {
        audio: StreamFilter::All,
        subtitles: SubtitleFilter::split(StreamFilter::All, StreamFilter::None),
    };
    assert!(
        !no_forced.is_all(),
        "forced subtitles are excluded — this is not a no-op"
    );

    let forced_english = StreamChoice {
        audio: StreamFilter::All,
        subtitles: SubtitleFilter::split(StreamFilter::All, StreamFilter::Langs(vec!["en".into()])),
    };
    assert!(
        !forced_english.is_all(),
        "the forced side is language-filtered — this is not a no-op"
    );

    // The mirror: narrowing only the NORMAL side must stay false too, so a
    // fix that merely swapped which side is inspected cannot pass.
    let no_full = StreamChoice {
        audio: StreamFilter::All,
        subtitles: SubtitleFilter::split(StreamFilter::None, StreamFilter::All),
    };
    assert!(!no_full.is_all(), "full subtitles are excluded");
}

// A caller with only ONE subtitle list must keep today's meaning exactly:
// the list applies to full and forced subtitles alike, so `-s eng` never
// starts dropping the English forced subtitle it used to keep.
#[test]
fn a_plain_subtitle_list_applies_to_both_sides() {
    for f in [
        StreamFilter::All,
        StreamFilter::None,
        StreamFilter::Langs(vec!["deu".into()]),
    ] {
        let via_builder = Job::new("d", "o")
            .with_subtitles(f.clone())
            .streams
            .subtitles;
        assert_eq!(via_builder.normal, f, "normal side");
        assert_eq!(via_builder.forced, f, "forced side");
        assert_eq!(
            via_builder,
            SubtitleFilter::from(f.clone()),
            "the builder must go through the same conversion"
        );
    }
}

/// `with_forced_subtitles` narrows ONLY the forced side — the whole point of
/// the split is that the two are set independently.
#[test]
fn forced_builder_leaves_the_normal_side_alone() {
    let j = Job::new("d", "o")
        .with_subtitles(StreamFilter::Langs(vec!["deu".into()]))
        .with_forced_subtitles(StreamFilter::Langs(vec!["eng".into()]));
    assert_eq!(
        j.streams.subtitles.normal,
        StreamFilter::Langs(vec!["deu".into()])
    );
    assert_eq!(
        j.streams.subtitles.forced,
        StreamFilter::Langs(vec!["eng".into()])
    );
    assert!(!j.streams.is_all());
}

fn audio_stream(pid: u16, lang: &str) -> libfreemkv::Stream {
    libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid,
        codec: libfreemkv::Codec::TrueHd,
        channels: libfreemkv::AudioChannels::Stereo,
        language: lang.into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: String::new(),
    })
}

fn sub_stream(pid: u16, lang: &str, forced: bool) -> libfreemkv::Stream {
    libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
        pid,
        codec: libfreemkv::Codec::Pgs,
        language: lang.into(),
        forced,
        qualifier: libfreemkv::LabelQualifier::None,
        codec_data: None,
    })
}

/// deu and eng each exist on BOTH sides of the forced flag, so a resolve
/// that ignored the flag would keep two PIDs where one is correct.
fn split_title() -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.streams = vec![
        audio_stream(0x1100, "deu"),
        sub_stream(0x1200, "deu", false),
        sub_stream(0x1201, "eng", false),
        sub_stream(0x1210, "deu", true),
        sub_stream(0x1211, "eng", true),
    ];
    t
}

// End-to-end through the type a front-end actually holds: a `StreamChoice`
// split must reach the forced-aware resolver. If `resolve` collapsed the
// choice back to one list, this would get both German or both English PIDs.
#[test]
fn resolve_routes_the_two_subtitle_sides_independently() {
    let choice = StreamChoice {
        audio: StreamFilter::None,
        subtitles: SubtitleFilter::split(
            StreamFilter::Langs(vec!["de".into()]),
            StreamFilter::Langs(vec!["en".into()]),
        ),
    };
    let sel = choice.resolve(&split_title()).unwrap();
    assert_eq!(
        sel.subtitle,
        libfreemkv::PidFilter::Only(vec![0x1200, 0x1211]),
        "German FULL subtitle union English FORCED subtitle"
    );

    // Swap the sides: the answer must swap too, so neither side can be the
    // one that silently drives both.
    let swapped = StreamChoice {
        audio: StreamFilter::None,
        subtitles: SubtitleFilter::split(
            StreamFilter::Langs(vec!["en".into()]),
            StreamFilter::Langs(vec!["de".into()]),
        ),
    };
    assert_eq!(
        swapped.resolve(&split_title()).unwrap().subtitle,
        libfreemkv::PidFilter::Only(vec![0x1201, 0x1210])
    );

    // And a single list still ignores forcedness: both German PIDs.
    let plain = StreamChoice {
        audio: StreamFilter::None,
        subtitles: StreamFilter::Langs(vec!["de".into()]).into(),
    };
    assert_eq!(
        plain.resolve(&split_title()).unwrap().subtitle,
        libfreemkv::PidFilter::Only(vec![0x1200, 0x1210])
    );
}
