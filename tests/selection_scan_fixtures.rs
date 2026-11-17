//! Selection regressions from native scans of three Blue Planet II UHD ISOs on
//! 2026-11-16, before and after the issue #88 fix (engine c711ef6).
//!
//! The JSON preserves all 51 SelectionModel title records in canonical scan order:
//! exact segments, sector ranges, duration, playlist ID, audio/video signature and
//! authored roster. Both captures had identical metadata and executable picks.
//! It contains no media. These are selection snapshots, not parser tests; stream
//! fields absent from SelectionModel use neutral test values below.
//!
//! Capture SHA-256 (complete diagnostic output, including native/UI outcomes):
//! before: b8ba221dbbebd04efa029a923aacaf866860782c1bd5a93917b2d76c38b61af5
//! after:  e0079280b946db7769fe4bb0a1c023895af4408a5eeb0ef436d1a9b8ae2645d0

use freemkv_engine::{Resolved, Selection, SelectionModel, StreamFilter};
use libfreemkv::disc::{ContentSegment, EpisodeEvidence};
use libfreemkv::{
    AudioChannels, AudioStream, Codec, ColorSpace, DiscTitle, Extent, FrameRate, HdrFormat,
    LabelPurpose, Resolution, SampleRate, Stream, VideoStream,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanFixture {
    label: String,
    source: String,
    expected: Vec<usize>,
    suggested: Vec<usize>,
    clusters: Vec<Vec<usize>>,
    play_alls: Vec<usize>,
    titles: Vec<ScanTitle>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanTitle {
    index: usize,
    playlist_id: u16,
    duration_secs: f64,
    segments: Vec<(String, u64, u64)>,
    audio_tags: Vec<String>,
    primary_audio: String,
    // Half-open sector ranges, as captured by SelectionModel.
    extents: Vec<(u32, u32)>,
    video: [String; 3],
    roster: String,
    title_count: usize,
    ordinal: Option<usize>,
}

impl ScanTitle {
    fn to_title(&self) -> DiscTitle {
        let mut title = DiscTitle::empty();
        title.playlist_id = self.playlist_id;
        title.playlist = format!("{:05}.mpls", self.playlist_id);
        title.duration_secs = self.duration_secs;
        title.selection_evidence.segments = self
            .segments
            .iter()
            .map(|(unit, start, end)| ContentSegment {
                unit: unit.clone(),
                start: *start,
                end: *end,
            })
            .collect();
        title.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: self.roster.clone(),
            title_count: self.title_count,
            ordinal: self.ordinal,
        };
        title.extents = self
            .extents
            .iter()
            .map(|&(start, end)| Extent {
                start_lba: start,
                sector_count: end - start,
            })
            .collect();
        // Every title in these captures has this primary video signature and one
        // English audio tag. Assert it rather than silently coercing new fixtures.
        assert_eq!(self.video, ["Hevc", "R2160p", "F23_976"]);
        assert_eq!(self.audio_tags, ["eng"]);
        assert_eq!(self.primary_audio, "eng");
        title.streams = vec![
            Stream::Video(VideoStream {
                pid: 0x1011,
                codec: Codec::Hevc,
                resolution: Resolution::R2160p,
                frame_rate: FrameRate::F23_976,
                hdr: HdrFormat::Sdr,
                color_space: ColorSpace::Bt709,
                display_aspect: None,
                secondary: false,
                label: String::new(),
                measured_cicp: None,
            }),
            Stream::Audio(AudioStream {
                pid: 0x1100,
                codec: Codec::Ac3,
                channels: AudioChannels::Stereo,
                language: self.primary_audio.clone(),
                sample_rate: SampleRate::S48,
                secondary: false,
                purpose: LabelPurpose::Normal,
                label: String::new(),
            }),
        ];
        title
    }
}

fn check_disc(label: &str, source: &str, expected: &[usize], play_all: usize) {
    let fixtures: Vec<ScanFixture> =
        serde_json::from_str(include_str!("fixtures/blue_planet_ii.json")).unwrap();
    let fixture = fixtures.iter().find(|disc| disc.label == label).unwrap();
    assert_eq!(fixture.source, source);
    assert_eq!(fixture.expected, expected);
    assert_eq!(fixture.play_alls, [play_all]);
    assert_eq!(fixture.titles.len(), 17);
    let titles: Vec<_> = fixture
        .titles
        .iter()
        .enumerate()
        .map(|(index, title)| {
            assert_eq!(title.index, index);
            assert_eq!(title.title_count, 17);
            title.to_title()
        })
        .collect();
    let model = SelectionModel::from_titles(&titles);
    for language in [None, Some("en"), Some("de")] {
        let audio = StreamFilter::All;
        assert_eq!(
            model.select(&Selection::Episodes, &audio, language),
            Ok(expected.to_vec()),
            "{label} {language:?}"
        );
        assert_eq!(
            model.main_or_episodes(&audio, language),
            Ok(expected.to_vec()),
            "{label} {language:?}"
        );
        assert_eq!(
            model.resolve_episodes(&audio, language, 600.0),
            Resolved::Run {
                titles: expected.to_vec(),
                episodes: true,
            },
            "{label} {language:?}"
        );
        // Authored picks and heuristic suggestions are independent: D3's native
        // captures resolve its two authored episodes with no suggested cluster.
        let suggestion = model.suggest_episodes(&audio, language, 600.0);
        assert_eq!(
            suggestion.suggested, fixture.suggested,
            "{label} {language:?}"
        );
        assert_eq!(
            suggestion.clusters, fixture.clusters,
            "{label} {language:?}"
        );
        assert_eq!(suggestion.play_alls, [play_all], "{label} {language:?}");
        assert_eq!(
            model.select(&Selection::MainMovie, &audio, language),
            Ok(vec![0]),
            "{label} {language:?}"
        );
    }
}

#[test]
fn blue_planet_ii_disc_one_keeps_its_three_authored_episodes() {
    check_disc(
        "BLUE_PLANET_II_D1",
        "Blue Planet II (2017)_2.iso",
        &[3, 1, 5],
        0,
    );
}

#[test]
fn blue_planet_ii_disc_two_keeps_its_two_authored_episodes() {
    check_disc(
        "BLUE_PLANET_II_D2",
        "Blue Planet II (2017).iso",
        &[2, 0],
        16,
    );
}

#[test]
fn blue_planet_ii_disc_three_keeps_its_two_authored_episodes() {
    check_disc(
        "BLUE_PLANET_II_D3",
        "Blue Planet II (2017)_3.iso",
        &[2, 0],
        16,
    );
}
