use super::*;
use libfreemkv::disc::ContentSegment;

// Synthetic authored layouts, not real discs: each named segment is (unit, start, length).
fn segment(name: &str) -> (&'static str, u32, u32) {
    match name {
        "logo" => ("00000", 0, 10),
        "A" | "feature" => ("00001", 0, 1000),
        "C" => ("00001", 1000, 1000),
        "B-en" | "trailer" => ("00002", 0, 200),
        "B-de" => ("00003", 0, 200),
        "B-mul" => ("00004", 0, 200),
        "X" => ("00005", 0, 300),
        "E1" => ("00006", 0, 700),
        "E2" => ("00006", 700, 700),
        "E3" => ("00006", 1400, 700),
        "copy" => ("00007", 0, 2200),
        other => panic!("unknown fixture segment {other}"),
    }
}

#[derive(Debug, Clone, Copy)]
enum Shape {
    Dvd,
    Bd,
}

type Layout = &'static [(&'static [&'static str], &'static [&'static str])];

// One title per (segments, audio languages); DVD as disc sectors, BD as clip ticks.
fn titles(shape: Shape, layout: Layout) -> Vec<DiscTitle> {
    layout
        .iter()
        .enumerate()
        .map(|(index, (segments, languages))| {
            let mut title = DiscTitle::empty();
            title.playlist_id = index as u16;
            for &name in *segments {
                let (unit, start, len) = segment(name);
                match shape {
                    Shape::Bd => title.clips.push(libfreemkv::Clip {
                        marks_on_pes_clock: true,
                        clip_id: unit.into(),
                        in_time: 90 + start,
                        out_time: 90 + start + len,
                        duration_secs: f64::from(len),
                        source_packets: 0,
                        feed_span: None,
                    }),
                    Shape::Dvd => title.extents.push(libfreemkv::Extent {
                        start_lba: unit.parse::<u32>().unwrap() * 100_000 + start,
                        sector_count: len,
                    }),
                }
            }
            // Each backend's own mapping, as the scanner applies it.
            title.selection_evidence.segments = match shape {
                Shape::Bd => ContentSegment::from_clips(&title.clips),
                Shape::Dvd => ContentSegment::from_cell_extents(&title.extents),
            };
            for (n, language) in languages.iter().enumerate() {
                title
                    .streams
                    .push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid: 0x80 + n as u16,
                        codec: libfreemkv::Codec::Ac3,
                        channels: libfreemkv::AudioChannels::Stereo,
                        language: (*language).into(),
                        sample_rate: libfreemkv::SampleRate::S48,
                        secondary: false,
                        purpose: libfreemkv::LabelPurpose::Normal,
                        label: String::new(),
                    }));
            }
            title
        })
        .collect()
}

const BRANCH: Layout = &[
    (&["A", "B-en", "C"], &["eng"]),
    (&["A", "B-de", "C"], &["deu"]),
];
const PLAY_ALL_AND_EPISODE: Layout = &[(&["E1", "E2", "E3"], &["eng"]), (&["E1"], &["deu"])];
const LOGO_TRAILER: Layout = &[
    (&["logo", "feature"], &["eng"]),
    (&["logo", "trailer"], &["deu"]),
];
const SCRAMBLED_DECOY: Layout = &[
    (&["A", "B-en", "C"], &["eng"]),
    (&["C", "A", "B-en"], &["deu"]),
];
const SEPARATE_COPY: Layout = &[(&["A", "B-en", "C"], &["eng"]), (&["copy"], &["deu"])];
const COVERAGE: Layout = &[
    (&["A", "B-en", "C"], &["eng"]),
    (&["A", "B-de", "C"], &["deu"]),
    (&["A", "B-mul", "C"], &["deu", "eng"]),
];
const EXTENDED_CUT: Layout = &[(&["A", "C"], &["eng"]), (&["A", "X", "C"], &["deu"])];
const ALL: [Layout; 7] = [
    BRANCH,
    PLAY_ALL_AND_EPISODE,
    LOGO_TRAILER,
    SCRAMBLED_DECOY,
    SEPARATE_COPY,
    COVERAGE,
    EXTENDED_CUT,
];

fn langs(tags: &[&str]) -> StreamFilter {
    StreamFilter::Langs(tags.iter().map(|tag| (*tag).into()).collect())
}

fn movie(
    titles: &[DiscTitle],
    presentation: Option<&str>,
    audio: &StreamFilter,
) -> SelectionReport {
    let preferences = SelectionPreferences {
        presentation_language: presentation.map(Into::into),
    };
    SelectionModel::from_titles(titles).select_with_preferences(
        &Selection::MainMovie,
        audio,
        &preferences,
    )
}

fn picks(
    shape: Shape,
    layout: Layout,
    presentation: Option<&str>,
    audio: &StreamFilter,
) -> Vec<usize> {
    let report = movie(&titles(shape, layout), presentation, audio);
    assert_eq!(report.review_reason, None, "{shape:?} {layout:?}");
    report.indices
}

// Shared cells/clips around per-language parts; an extended cut is accepted (documented).
#[test]
fn a_branch_variant_in_the_requested_language_is_the_main_movie() {
    for shape in [Shape::Dvd, Shape::Bd] {
        for (language, expected) in [("de", 1), ("ger", 1), ("en", 0), ("", 0), ("fr", 0)] {
            let pick = picks(shape, BRANCH, Some(language), &StreamFilter::All);
            assert_eq!(pick, vec![expected], "{shape:?} {language:?}");
        }
        assert_eq!(
            picks(shape, BRANCH, None, &langs(&["deu"])),
            vec![1],
            "{shape:?}"
        );
        assert_eq!(
            picks(shape, BRANCH, None, &StreamFilter::All),
            vec![0],
            "{shape:?}"
        );
        assert_eq!(
            picks(shape, EXTENDED_CUT, Some("de"), &StreamFilter::All),
            vec![1]
        );
    }
}

// Logo-sharing extras, episodes of a play-all, scrambled decoys and separate copies.
#[test]
fn content_that_is_not_a_majority_ordered_variant_keeps_title_zero() {
    for shape in [Shape::Dvd, Shape::Bd] {
        for layout in [
            PLAY_ALL_AND_EPISODE,
            LOGO_TRAILER,
            SCRAMBLED_DECOY,
            SEPARATE_COPY,
        ] {
            let wanted = [(Some("de"), StreamFilter::All), (None, langs(&["deu"]))];
            for (presentation, audio) in wanted {
                let pick = picks(shape, layout, presentation, &audio);
                assert_eq!(pick, vec![0], "{shape:?} {layout:?} {presentation:?}");
            }
        }
    }
}

#[test]
fn audio_coverage_prefers_the_variant_with_more_requested_languages() {
    for shape in [Shape::Dvd, Shape::Bd] {
        assert_eq!(
            picks(shape, COVERAGE, None, &langs(&["deu", "eng"])),
            vec![2]
        );
        assert_eq!(picks(shape, COVERAGE, None, &langs(&["deu"])), vec![1]);
        assert_eq!(
            picks(shape, COVERAGE, Some("de"), &langs(&["eng"])),
            vec![1]
        );
        assert_eq!(
            picks(shape, COVERAGE, None, &langs(&["fra", "deu", "eng"])),
            vec![2]
        );
    }
}

// MainMovie never holds for review, whatever the layout, shape or language request.
#[test]
fn main_movie_never_reviews_and_decides_the_same_for_dvd_and_bd_shapes() {
    let requests = [
        (None, StreamFilter::All),
        (None, StreamFilter::None),
        (None, langs(&["deu"])),
        (None, langs(&["deu", "eng"])),
        (None, langs(&["invalid-language"])),
        (Some("de"), StreamFilter::All),
        (Some("xx-nope"), langs(&["eng"])),
    ];
    for layout in ALL {
        for (presentation, audio) in &requests {
            let decide = |shape| {
                let report = movie(&titles(shape, layout), *presentation, audio);
                assert_eq!(
                    report.review_reason, None,
                    "{layout:?} {presentation:?} {audio:?}"
                );
                report.indices
            };
            assert_eq!(
                decide(Shape::Dvd),
                decide(Shape::Bd),
                "{layout:?} {presentation:?}"
            );
        }
    }
}

const CANDIDATE_HAS_HUGE_EXTRAS: Layout =
    &[(&["A", "C"], &["eng"]), (&["A", "C", "copy"], &["deu"])];
const SHARED_IS_EXACTLY_HALF: Layout = &[(&["A", "C"], &["eng"]), (&["A", "X"], &["deu"])];
const TITLE_ZERO_UNDESCRIBED: Layout = &[(&[], &["eng"]), (&["A", "C"], &["deu"])];
const DUPLICATED_SEGMENT: Layout = &[(&["A", "A", "C"], &["eng"]), (&["A", "C"], &["deu"])];
const DUPLICATED_SEGMENT_REVERSED: Layout =
    &[(&["A", "C"], &["eng"]), (&["A", "A", "C"], &["deu"])];

// Sharing most of title 0 is not enough: the shared content must also be most of the candidate.
#[test]
fn a_candidate_dominated_by_its_own_extras_keeps_title_zero() {
    for shape in [Shape::Dvd, Shape::Bd] {
        let pick = picks(
            shape,
            CANDIDATE_HAS_HUGE_EXTRAS,
            Some("de"),
            &StreamFilter::All,
        );
        assert_eq!(pick, vec![0], "{shape:?}");
    }
}

// "More than half" is strict: exactly half shared is not a variant.
#[test]
fn sharing_exactly_half_keeps_title_zero() {
    for shape in [Shape::Dvd, Shape::Bd] {
        let pick = picks(
            shape,
            SHARED_IS_EXACTLY_HALF,
            Some("de"),
            &StreamFilter::All,
        );
        assert_eq!(pick, vec![0], "{shape:?}");
    }
}

#[test]
fn a_title_zero_without_segments_keeps_title_zero() {
    for shape in [Shape::Dvd, Shape::Bd] {
        let pick = picks(
            shape,
            TITLE_ZERO_UNDESCRIBED,
            Some("de"),
            &StreamFilter::All,
        );
        assert_eq!(pick, vec![0], "{shape:?}");
    }
}

// A repeated segment makes the shared orders differ, so the titles are not variants.
#[test]
fn a_duplicated_segment_is_not_a_variant() {
    for layout in [DUPLICATED_SEGMENT, DUPLICATED_SEGMENT_REVERSED] {
        let pick = picks(Shape::Bd, layout, Some("de"), &StreamFilter::All);
        assert_eq!(pick, vec![0], "{layout:?}");
    }
}
