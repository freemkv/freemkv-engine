use super::*;
use libfreemkv::disc::ContentSegment;
use libfreemkv::{
    AudioChannels, AudioStream, Clip, Codec, ColorSpace, ContentFormat, Extent, FrameRate,
    HdrFormat, LabelPurpose, Resolution, SampleRate, Stream, VideoStream,
};

const MIN: f64 = 60.0;
// The server's default `min_length_secs`.
const MIN_SECS: f64 = 600.0;
// 45 kHz ticks per minute.
const TICKS: u32 = 45_000 * 60;

fn video(codec: Codec, resolution: Resolution, frame_rate: FrameRate) -> Stream {
    Stream::Video(VideoStream {
        pid: 0x1011,
        codec,
        resolution,
        frame_rate,
        hdr: HdrFormat::Sdr,
        color_space: ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    })
}

fn audio(language: &str) -> Stream {
    Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::Ac3,
        channels: AudioChannels::Stereo,
        language: language.into(),
        sample_rate: SampleRate::S48,
        secondary: false,
        purpose: LabelPurpose::Normal,
        label: String::new(),
    })
}

// A BD playlist `id` playing `(clip, in, out)` items (minutes on the clip's clock).
fn bd(id: u16, minutes: f64, items: &[(&str, u32, u32)], language: &str) -> DiscTitle {
    let mut t = DiscTitle::empty();
    t.playlist_id = id;
    t.playlist = format!("{id:05}.mpls");
    t.duration_secs = minutes * MIN;
    t.clips = (items.iter())
        .map(|&(clip, start, end)| Clip {
            marks_on_pes_clock: true,
            clip_id: clip.into(),
            in_time: start * TICKS,
            out_time: end * TICKS,
            duration_secs: f64::from(end - start) * MIN,
            source_packets: 0,
            feed_span: None,
        })
        .collect();
    t.selection_evidence.segments = ContentSegment::from_clips(&t.clips);
    t.streams = vec![
        video(Codec::H264, Resolution::R1080p, FrameRate::F23_976),
        audio(language),
    ];
    t
}

// A DVD title `id` (TT_SRPT order) over `(start, sectors)` cells; sequential, so it has
// segments too.
fn dvd(id: u16, minutes: f64, cells: &[(u32, u32)], language: &str) -> DiscTitle {
    let mut t = DiscTitle::empty();
    t.content_format = ContentFormat::DvdPs;
    t.playlist_id = id;
    t.playlist = format!("VTS_01_{id}.VOB");
    t.duration_secs = minutes * MIN;
    t.extents = (cells.iter())
        .map(|&(start_lba, sector_count)| Extent {
            start_lba,
            sector_count,
        })
        .collect();
    t.selection_evidence.segments = ContentSegment::from_cell_extents(&t.extents);
    t.streams = vec![
        video(Codec::Mpeg2, Resolution::R480i, FrameRate::F29_97),
        audio(language),
    ];
    t
}

fn model(titles: &[DiscTitle]) -> SelectionModel {
    SelectionModel::from_titles(titles)
}

fn suggest(titles: &[DiscTitle]) -> Suggestion {
    model(titles).suggest_episodes(&StreamFilter::All, None, MIN_SECS)
}

fn resolve(titles: &[DiscTitle]) -> Resolved {
    model(titles).resolve_episodes(&StreamFilter::All, None, MIN_SECS)
}

fn confirm(
    suggested: Vec<usize>,
    clusters: Vec<Vec<usize>>,
    alternates: Vec<Alternate>,
) -> Resolved {
    Resolved::Confirm {
        suggested,
        clusters,
        alternates,
        reason: SelectionReviewReason::MissingEpisodeRoster,
    }
}

// Samurai Champloo Vol. 1: 4 × ~23-min episodes, no play-all, 12 extras. Scan order is by
// length (as main-feature ranking leaves it), so the suggestion follows TT_SRPT order.
#[test]
fn samurai_style_dvd_suggests_its_four_episodes_in_title_order() {
    let mut titles = vec![
        dvd(3, 24.0, &[(30_000, 9_000)], "jpn"),
        dvd(1, 23.7, &[(10_000, 9_000)], "jpn"),
        dvd(4, 23.8, &[(40_000, 9_000)], "jpn"),
        dvd(2, 23.6, &[(20_000, 9_000)], "jpn"),
    ];
    for (n, minutes) in [1.0, 2.0, 0.5, 3.0, 4.5, 1.5, 2.5, 0.3, 11.0, 5.0, 3.5, 30.0]
        .into_iter()
        .enumerate()
    {
        let id = 5 + n as u16;
        titles.push(dvd(
            id,
            minutes,
            &[(100_000 + u32::from(id) * 5_000, 900)],
            "jpn",
        ));
    }
    let samurai = vec![1, 3, 0, 2];
    assert_eq!(
        resolve(&titles),
        confirm(samurai.clone(), vec![samurai], vec![])
    );
    assert!(suggest(&titles).play_alls.is_empty());
}

// One m2ts, cut into episodes by in/out marks; the play-all plays it whole. Playlist numbers
// are out of episode order, so the order comes from the in-time.
#[test]
fn bd_episodes_cut_from_one_clip_drop_the_play_all_and_order_by_in_time() {
    let titles = vec![
        bd(800, 176.0, &[("00010", 0, 176)], "eng"),
        bd(803, 44.0, &[("00010", 0, 44)], "eng"),
        bd(801, 44.0, &[("00010", 88, 132)], "eng"),
        bd(804, 44.0, &[("00010", 44, 88)], "eng"),
        bd(802, 44.0, &[("00010", 132, 176)], "eng"),
        bd(805, 12.0, &[("00020", 0, 12)], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![0]);
    assert_eq!(suggestion.suggested, vec![1, 3, 2, 4]);
    assert_eq!(suggestion.clusters, vec![vec![1, 3, 2, 4]]);
    assert!(suggestion.alternates.is_empty());
}

// Separate clips: BD order is the first clip, before the playlist number.
#[test]
fn bd_order_is_first_clip_then_extent_then_playlist() {
    let mut titles = vec![
        bd(1, 44.0, &[("00003", 0, 44)], "eng"),
        bd(2, 44.0, &[("00001", 0, 44)], "eng"),
        bd(3, 44.0, &[("00002", 0, 44)], "eng"),
    ];
    assert_eq!(suggest(&titles).suggested, vec![1, 2, 0]);
    // Undescribed titles: first extent sector, then playlist number.
    for (t, (lba, id)) in titles.iter_mut().zip([(500, 9), (900, 7), (500, 8)]) {
        t.selection_evidence.segments.clear();
        t.extents = vec![Extent {
            start_lba: lba,
            sector_count: 100,
        }];
        t.playlist_id = id;
    }
    assert_eq!(suggest(&titles).suggested, vec![2, 0, 1]);
}

// DVD angle titles repeat an episode's cells. Exact aliases collapse whether or not the title
// has segments (non-sequential titles have only extents, so they fold instead).
#[test]
fn dvd_angle_duplicates_are_suggested_once() {
    let titles = vec![
        dvd(1, 44.0, &[(1_000, 5_000)], "eng"),
        dvd(2, 44.0, &[(1_000, 5_000)], "eng"),
        dvd(3, 45.0, &[(6_000, 5_000)], "eng"),
        dvd(4, 45.0, &[(6_000, 5_000)], "eng"),
        dvd(5, 43.0, &[(11_000, 5_000)], "eng"),
    ];
    let alternates = vec![Alternate { index: 1, of: 0 }, Alternate { index: 3, of: 2 }];
    let expected = confirm(vec![0, 2, 4], vec![vec![0, 2, 4]], alternates);
    assert_eq!(resolve(&titles), expected);
    let mut undescribed = titles.clone();
    for t in &mut undescribed {
        t.selection_evidence.segments.clear();
    }
    assert_eq!(resolve(&undescribed), expected);
}

#[test]
fn two_clusters_suggest_nothing_and_return_both() {
    let minutes = [44.0, 15.0, 44.5, 15.2, 45.0, 14.8];
    let titles: Vec<_> = (minutes.iter().enumerate())
        .map(|(n, &m)| bd(n as u16, m, &[(&format!("{n:05}"), 0, m as u32)], "eng"))
        .collect();
    assert_eq!(
        resolve(&titles),
        confirm(vec![], vec![vec![0, 2, 4], vec![1, 3, 5]], vec![])
    );
}

// Runtime alone proves nothing: the 88s are distinct content, so two clusters remain. When
// they do play the 44s, they are play-alls and the 44s are the suggestion.
#[test]
fn doubled_runtimes_are_play_alls_only_when_their_content_says_so() {
    let distinct: Vec<_> = ([44.0, 44.5, 88.0, 88.5, 120.0].iter().enumerate())
        .map(|(n, &m)| bd(n as u16, m, &[(&format!("{n:05}"), 0, m as u32)], "eng"))
        .collect();
    let suggestion = suggest(&distinct);
    assert!(suggestion.suggested.is_empty());
    assert_eq!(suggestion.clusters, vec![vec![0, 1], vec![2, 3]]);
    assert!(suggestion.play_alls.is_empty());

    let a = ("00001", 0, 44);
    let b = ("00002", 0, 45);
    let playing = vec![
        bd(1, 44.0, &[a], "eng"),
        bd(2, 44.5, &[b], "eng"),
        bd(3, 88.0, &[a, b], "eng"),
        bd(4, 88.5, &[b, a], "eng"),
        bd(5, 120.0, &[("00005", 0, 120)], "eng"),
    ];
    let suggestion = suggest(&playing);
    assert_eq!(suggestion.play_alls, vec![2, 3]);
    assert_eq!(suggestion.suggested, vec![0, 1]);
}

// A bonus as long as the episodes joins their set unless its signature differs; either way
// the operator confirms.
#[test]
fn two_episodes_and_a_same_length_bonus() {
    let mut titles = vec![
        bd(1, 44.0, &[("00001", 0, 44)], "eng"),
        bd(2, 44.5, &[("00002", 0, 44)], "eng"),
        bd(3, 44.2, &[("00003", 0, 44)], "eng"),
    ];
    assert_eq!(
        resolve(&titles),
        confirm(vec![0, 1, 2], vec![vec![0, 1, 2]], vec![])
    );
    // Per-episode extras (a commentary track) do not split the set...
    titles[1].streams.push(audio("eng"));
    assert_eq!(suggest(&titles).suggested, vec![0, 1, 2]);
    // ...but an SD bonus or one in another primary language does.
    let mut sd = titles.clone();
    sd[2].streams[0] = video(Codec::Mpeg2, Resolution::R480i, FrameRate::F29_97);
    assert_eq!(suggest(&sd).suggested, vec![0, 1]);
    let mut dub = titles.clone();
    dub[2].streams[1] = audio("jpn");
    assert_eq!(suggest(&dub).suggested, vec![0, 1]);
    // Language tags compare by identity, not spelling.
    let mut spelled = titles.clone();
    spelled[2].streams[1] = audio("en");
    assert_eq!(suggest(&spelled).suggested, vec![0, 1, 2]);
}

// A recap playlist and a commentary/dub cut fold into their episode; the kept one is the
// presentation language, else the earlier.
#[test]
fn recap_and_commentary_variants_fold_into_alternates() {
    let mut titles = vec![
        bd(1, 44.0, &[("00011", 0, 44)], "eng"),
        bd(2, 46.0, &[("00030", 0, 2), ("00011", 0, 44)], "eng"),
        bd(3, 44.5, &[("00012", 0, 40), ("00013", 0, 4)], "eng"),
        bd(4, 44.5, &[("00012", 0, 40), ("00014", 0, 4)], "eng"),
    ];
    titles[3].streams.push(audio("deu"));
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.suggested, vec![0, 2]);
    // Alternates in play order: title 3 starts on clip 00012, the recap on 00030.
    assert_eq!(
        suggestion.alternates,
        vec![Alternate { index: 3, of: 2 }, Alternate { index: 1, of: 0 }]
    );
    assert!(suggestion.play_alls.is_empty());
    let german = model(&titles).suggest_episodes(&StreamFilter::All, Some("de"), MIN_SECS);
    assert_eq!(german.suggested, vec![0, 3]);
    assert_eq!(
        german.alternates,
        vec![Alternate { index: 2, of: 3 }, Alternate { index: 1, of: 0 }]
    );
}

// A title holding two overlapping candidates is a longer cut, not a play-all.
#[test]
fn contained_titles_must_be_disjoint_for_a_play_all() {
    let a = ("00001", 0, 40);
    let x = ("00002", 0, 10);
    let c = ("00003", 0, 40);
    let titles = vec![
        bd(1, 90.0, &[a, x, c], "eng"),
        bd(2, 80.0, &[a, c], "eng"),
        bd(3, 40.0, &[a], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert!(suggestion.play_alls.is_empty());
    assert!(suggestion.suggested.is_empty());
}

// Play-alls of play-alls go too; a title the disc cannot describe proves nothing.
#[test]
fn nested_play_alls_and_undescribed_titles() {
    let (e1, e2, e3) = (("00001", 0, 30), ("00002", 0, 30), ("00003", 0, 30));
    let mut titles = vec![
        bd(1, 30.0, &[e1], "eng"),
        bd(2, 30.0, &[e2], "eng"),
        bd(3, 30.0, &[e3], "eng"),
        bd(4, 60.0, &[e1, e2], "eng"),
        bd(5, 90.0, &[e1, e2, e3], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![3, 4]);
    assert_eq!(suggestion.suggested, vec![0, 1, 2]);
    // Random/shuffle playlists carry no segments: never a play-all, never inside one.
    titles[4].selection_evidence.segments.clear();
    titles[2].selection_evidence.segments.clear();
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![3]);
    assert_eq!(suggestion.suggested, vec![0, 1, 2]);
}

// Movies are never split: the main film is unchanged, and an episode-intended pick only
// proposes what clusters (here nothing).
#[test]
fn a_movie_with_featurettes_suggests_nothing() {
    let titles = vec![
        bd(800, 120.0, &[("00001", 0, 120)], "eng"),
        bd(801, 3.0, &[("00002", 0, 3)], "eng"),
        bd(802, 7.0, &[("00003", 0, 7)], "eng"),
        bd(803, 12.0, &[("00004", 0, 12)], "eng"),
        bd(804, 25.0, &[("00005", 0, 25)], "eng"),
    ];
    let model = model(&titles);
    assert_eq!(
        model.main_or_episodes(&StreamFilter::All, None),
        Ok(vec![0])
    );
    assert_eq!(resolve(&titles), confirm(vec![], vec![], vec![]));
}

#[test]
fn a_single_title_runs_as_itself() {
    let titles = vec![bd(1, 44.0, &[("00001", 0, 44)], "eng")];
    let run = Resolved::Run {
        titles: vec![0],
        episodes: false,
    };
    assert_eq!(resolve(&titles), run);
    assert_eq!(suggest(&titles), Suggestion::default());
    assert_eq!(
        resolve(&[]),
        Resolved::Run {
            titles: vec![],
            episodes: false
        }
    );
}

#[test]
fn a_proven_roster_runs_and_one_that_collapses_runs_the_main_title() {
    let mut titles = vec![
        bd(0, 44.0, &[("00001", 0, 44)], "eng"),
        bd(1, 44.0, &[("00002", 0, 44)], "eng"),
        bd(2, 5.0, &[("00003", 0, 5)], "eng"),
    ];
    crate::test_fixtures::authored_episodes(&mut titles, &[0, 1]);
    assert_eq!(
        resolve(&titles),
        Resolved::Run {
            titles: vec![0, 1],
            episodes: true
        }
    );
    crate::test_fixtures::authored_episodes(&mut titles, &[1]);
    assert_eq!(
        resolve(&titles),
        Resolved::Run {
            titles: vec![0],
            episodes: false
        }
    );
}

#[test]
fn an_incomplete_roster_asks_with_its_reason() {
    let mut titles = vec![
        bd(0, 44.0, &[("00001", 0, 44)], "eng"),
        bd(1, 44.0, &[("00002", 0, 44)], "eng"),
    ];
    crate::test_fixtures::authored_episodes(&mut titles, &[0, 1]);
    titles.push(bd(2, 5.0, &[("00003", 0, 5)], "eng"));
    assert!(matches!(
        resolve(&titles),
        Resolved::Confirm {
            reason: SelectionReviewReason::IncompleteEpisodeRoster,
            ..
        }
    ));
}

#[test]
fn explicit_picks_keep_operator_order_and_collapse_aliases_only_for_episodes() {
    let titles = vec![
        bd(0, 44.0, &[("00001", 0, 44)], "eng"),
        bd(1, 44.0, &[("00001", 0, 44)], "deu"),
        bd(2, 44.0, &[("00002", 0, 44)], "eng"),
    ];
    let model = model(&titles);
    assert_eq!(
        model.resolve_explicit(&[2, 1, 0, 2, 9], true),
        Resolved::Run {
            titles: vec![2, 1],
            episodes: true
        }
    );
    assert_eq!(
        model.resolve_explicit(&[2, 1, 0], false),
        Resolved::Run {
            titles: vec![2, 1, 0],
            episodes: false
        }
    );
}

// Building the model still resolves no language tags (see the variant tests).
#[test]
fn capturing_suggestion_data_resolves_no_tags() {
    use crate::streams::LANG_LOOKUPS;
    LANG_LOOKUPS.with(|n| n.set(0));
    let _ = model(&[bd(0, 44.0, &[("00001", 0, 44)], "eng")]);
    assert_eq!(LANG_LOOKUPS.with(|n| n.get()), 0);
}

fn secondary(mut stream: Stream) -> Stream {
    match &mut stream {
        Stream::Video(video) => video.secondary = true,
        Stream::Audio(audio) => audio.secondary = true,
        _ => {}
    }
    stream
}

// Another cut of a play-all (here a dub) is a play-all too, not an alternate of one.
#[test]
fn an_alias_of_a_play_all_is_a_play_all() {
    let (e1, e2) = (("00001", 0, 30), ("00002", 0, 30));
    let titles = vec![
        bd(1, 30.0, &[e1], "eng"),
        bd(2, 30.0, &[e2], "eng"),
        bd(3, 60.0, &[e1, e2], "eng"),
        bd(4, 60.0, &[e1, e2], "deu"),
    ];
    for presentation in [None, Some("de")] {
        let suggestion =
            model(&titles).suggest_episodes(&StreamFilter::All, presentation, MIN_SECS);
        assert_eq!(suggestion.play_alls, vec![2, 3], "{presentation:?}");
        assert!(suggestion.alternates.is_empty(), "{presentation:?}");
        assert_eq!(suggestion.suggested, vec![0, 1], "{presentation:?}");
    }
}

#[test]
fn an_alias_of_a_folded_variant_names_the_kept_title() {
    let recap = [("00030", 0, 2), ("00011", 0, 44)];
    let titles = vec![
        bd(1, 44.0, &[("00011", 0, 44)], "eng"),
        bd(2, 46.0, &recap, "eng"),
        bd(3, 46.0, &recap, "deu"),
        bd(4, 44.0, &[("00012", 0, 44)], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.suggested, vec![0, 3]);
    assert_eq!(
        suggestion.alternates,
        vec![Alternate { index: 1, of: 0 }, Alternate { index: 2, of: 0 }]
    );
}

// Variants fold when they share at least half of the shorter one's content.
#[test]
fn variants_fold_from_exactly_half_shared() {
    let half = vec![
        bd(1, 40.0, &[("00001", 0, 40)], "eng"),
        bd(2, 40.0, &[("00001", 0, 20), ("00002", 0, 20)], "eng"),
    ];
    let suggestion = suggest(&half);
    assert_eq!(suggestion.alternates, vec![Alternate { index: 1, of: 0 }]);
    assert!(suggestion.clusters.is_empty());
    let under = vec![
        bd(1, 40.0, &[("00001", 0, 40)], "eng"),
        bd(2, 40.0, &[("00001", 0, 19), ("00002", 0, 21)], "eng"),
    ];
    let suggestion = suggest(&under);
    assert!(suggestion.alternates.is_empty());
    assert_eq!(suggestion.suggested, vec![0, 1]);
}

// Exactly 15% longer stays in the set; only more than 15% starts a new one.
#[test]
fn the_cluster_gap_is_more_than_fifteen_percent() {
    let pair = |longer: f64| {
        vec![
            bd(1, 40.0, &[("00001", 0, 40)], "eng"),
            bd(2, longer, &[("00002", 0, 46)], "eng"),
        ]
    };
    assert_eq!(suggest(&pair(46.0)).suggested, vec![0, 1]);
    assert!(suggest(&pair(46.1)).clusters.is_empty());
    for (short, long) in [(20.0, 23.0), (100.0, 115.0)] {
        let titles = vec![
            bd(1, short, &[("00001", 0, 1)], "eng"),
            bd(2, long, &[("00002", 0, 1)], "eng"),
        ];
        let got = model(&titles).suggest_episodes(&StreamFilter::All, None, 0.0);
        assert_eq!(got.suggested, vec![0, 1], "{short} {long}");
    }
}

// Described titles come first, then the first non-empty extent, then none.
#[test]
fn described_titles_order_before_undescribed_ones() {
    let mut titles = vec![
        bd(1, 44.0, &[("00009", 0, 44)], "eng"),
        bd(2, 44.0, &[("00005", 0, 44)], "eng"),
        bd(3, 44.0, &[("00007", 0, 44)], "eng"),
        bd(4, 44.0, &[("00008", 0, 44)], "eng"),
    ];
    let extent = |start_lba, sector_count| Extent {
        start_lba,
        sector_count,
    };
    for (n, extents) in [
        (0, vec![extent(0, 0), extent(900, 100)]),
        (2, vec![extent(500, 100)]),
        (3, vec![]),
    ] {
        titles[n].selection_evidence.segments.clear();
        titles[n].extents = extents;
    }
    assert_eq!(suggest(&titles).suggested, vec![1, 2, 0, 3]);
}

// DVD order is the title number, whatever the sectors say.
#[test]
fn dvd_order_is_the_title_number_not_the_sector() {
    let titles = vec![
        dvd(1, 44.0, &[(30_000, 5_000)], "eng"),
        dvd(2, 44.0, &[(10_000, 5_000)], "eng"),
        dvd(3, 44.0, &[(20_000, 5_000)], "eng"),
    ];
    assert_eq!(suggest(&titles).suggested, vec![0, 1, 2]);
}

// A DVD play-all is found by its extents even without segments; a BD one that repeats a
// clip still plays each episode once.
#[test]
fn play_alls_by_dvd_extents_and_repeated_clips() {
    let mut titles = vec![
        dvd(1, 44.0, &[(1_000, 5_000)], "eng"),
        dvd(2, 44.0, &[(6_000, 5_000)], "eng"),
        dvd(3, 44.0, &[(11_000, 5_000)], "eng"),
        dvd(
            4,
            132.0,
            &[(1_000, 5_000), (6_000, 5_000), (11_000, 5_000)],
            "eng",
        ),
    ];
    for t in &mut titles {
        t.selection_evidence.segments.clear();
    }
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![3]);
    assert_eq!(suggestion.suggested, vec![0, 1, 2]);

    let (e1, e2) = (("00001", 0, 30), ("00002", 0, 30));
    let titles = vec![
        bd(1, 30.0, &[e1], "eng"),
        bd(2, 30.0, &[e2], "eng"),
        bd(3, 90.0, &[e1, e1, e2], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![2]);
    assert_eq!(suggestion.suggested, vec![0, 1]);
}

// The signature is the first primary video and audio: secondary (PiP, commentary) streams
// listed first do not split the set.
#[test]
fn secondary_streams_do_not_sign_a_title() {
    let mut titles = vec![
        bd(1, 44.0, &[("00001", 0, 44)], "eng"),
        bd(2, 44.0, &[("00002", 0, 44)], "eng"),
        bd(3, 44.0, &[("00003", 0, 44)], "eng"),
    ];
    let pip = video(Codec::Mpeg2, Resolution::R480i, FrameRate::F29_97);
    titles[1].streams.insert(0, secondary(pip));
    titles[2].streams.insert(1, secondary(audio("jpn")));
    assert_eq!(suggest(&titles).suggested, vec![0, 1, 2]);
}

// Unrecognised language tags compare trimmed and case-insensitively.
#[test]
fn unknown_language_tags_compare_by_spelling() {
    let mut titles = vec![
        bd(1, 44.0, &[("00001", 0, 44)], "zz-custom"),
        bd(2, 44.0, &[("00002", 0, 44)], " ZZ-Custom "),
        bd(3, 44.0, &[("00003", 0, 44)], "qq-other"),
    ];
    assert_eq!(suggest(&titles).suggested, vec![0, 1]);
    titles[2].streams[1] = audio("ZZ-CUSTOM");
    assert_eq!(suggest(&titles).suggested, vec![0, 1, 2]);
}

#[test]
fn min_secs_is_inclusive() {
    let titles = vec![
        bd(1, 10.0, &[("00001", 0, 10)], "eng"),
        bd(2, 10.0, &[("00002", 0, 10)], "eng"),
    ];
    assert_eq!(suggest(&titles).suggested, vec![0, 1]);
    let got = model(&titles).suggest_episodes(&StreamFilter::All, None, MIN_SECS + 0.001);
    assert_eq!(got, Suggestion::default());
}

// resolve_episodes suggests with the caller's language and minimum length.
#[test]
fn resolve_episodes_passes_its_language_and_minimum_through() {
    let mut titles = vec![
        bd(1, 44.0, &[("00011", 0, 44)], "eng"),
        bd(2, 46.0, &[("00030", 0, 2), ("00011", 0, 44)], "eng"),
        bd(3, 44.5, &[("00012", 0, 40), ("00013", 0, 4)], "eng"),
        bd(4, 44.5, &[("00012", 0, 40), ("00014", 0, 4)], "eng"),
        bd(5, 5.0, &[("00040", 0, 5)], "eng"),
        bd(6, 5.0, &[("00041", 0, 5)], "eng"),
    ];
    titles[3].streams.push(audio("deu"));
    let got = model(&titles).resolve_episodes(&StreamFilter::All, Some("de"), MIN_SECS);
    let alternates = vec![Alternate { index: 2, of: 3 }, Alternate { index: 1, of: 0 }];
    assert_eq!(got, confirm(vec![0, 3], vec![vec![0, 3]], alternates));
    let german = StreamFilter::Langs(vec!["deu".into()]);
    let got = model(&titles).resolve_episodes(&german, None, MIN_SECS);
    assert!(matches!(got, Resolved::Confirm { suggested, .. } if suggested == vec![0, 3]));
}

// A roster whose episodes are all one programme (dub playlists) runs that programme once.
#[test]
fn a_roster_of_aliases_runs_the_main_title() {
    let mut titles = vec![
        bd(0, 44.0, &[("00001", 0, 44)], "eng"),
        bd(1, 44.0, &[("00001", 0, 44)], "deu"),
        bd(2, 5.0, &[("00003", 0, 5)], "eng"),
    ];
    crate::test_fixtures::authored_episodes(&mut titles, &[0, 1]);
    let model = model(&titles);
    for (presentation, main) in [(None, 0), (Some("de"), 1)] {
        assert_eq!(
            model.resolve_episodes(&StreamFilter::All, presentation, MIN_SECS),
            Resolved::Run {
                titles: vec![main],
                episodes: false
            },
            "{presentation:?}"
        );
    }
}

#[test]
fn explicit_picks_never_collapse_undescribed_titles() {
    let mut titles = vec![
        bd(0, 44.0, &[("00001", 0, 44)], "eng"),
        bd(1, 44.0, &[("00001", 0, 44)], "eng"),
    ];
    for t in &mut titles {
        t.selection_evidence.segments.clear();
    }
    let model = model(&titles);
    assert_eq!(
        model.resolve_explicit(&[1, 0], true),
        Resolved::Run {
            titles: vec![1, 0],
            episodes: true
        }
    );
    assert_eq!(
        model.resolve_explicit(&[], true),
        Resolved::Run {
            titles: vec![],
            episodes: true
        }
    );
}

// Synthetic shared-terminal-cell topology: canonical scan order prefers English;
// authored title numbers put the German cut first.
fn shared_terminal_titles() -> Vec<DiscTitle> {
    let a = (1_000, 1_000);
    let b = (2_000, 1_000);
    let c = (3_000, 600);
    let en = (4_000, 400);
    let de = (5_000, 400);
    let tail = (6_000, 100);
    vec![
        dvd(2, 144.0, &[a, b, c, en, tail], "eng"),
        dvd(1, 140.0, &[a, b, c, de, tail], "deu"),
        dvd(7, 48.0, &[a, tail], "eng"),
        dvd(4, 48.0, &[a, tail], "deu"),
        dvd(8, 47.0, &[b, tail], "eng"),
        dvd(5, 47.0, &[b, tail], "deu"),
        dvd(9, 47.0, &[c, en, tail], "eng"),
        dvd(6, 43.0, &[c, de, tail], "deu"),
    ]
}

#[test]
fn shared_terminal_cell_preserves_three_episode_presentations() {
    let titles = shared_terminal_titles();
    let snapshot = model(&titles);
    for (language, expected) in [
        (None, vec![2, 4, 6]),
        (Some("en"), vec![2, 4, 6]),
        (Some("de"), vec![3, 5, 7]),
    ] {
        let suggestion = snapshot.suggest_episodes(&StreamFilter::All, language, MIN_SECS);
        assert_eq!(suggestion.play_alls, vec![1, 0]);
        assert_eq!(suggestion.suggested, expected);
        assert!(
            matches!(snapshot.resolve_episodes(&StreamFilter::All, language, MIN_SECS), Resolved::Confirm { suggested, .. } if suggested == expected)
        );
    }
    assert_eq!(snapshot, model(&titles));
    assert!(
        titles
            .iter()
            .all(|title| title.extents.last().unwrap().start_lba == 6_000)
    );
}

#[test]
fn shared_intro_and_outro_are_compared_without_trimming_titles() {
    let intro = ("00001", 0, 2);
    let a = ("00002", 0, 42);
    let b = ("00003", 0, 42);
    let outro = ("00004", 0, 1);
    let titles = vec![
        bd(1, 87.0, &[intro, a, b, outro], "eng"),
        bd(2, 45.0, &[intro, a, outro], "eng"),
        bd(3, 45.0, &[intro, b, outro], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![0]);
    assert_eq!(suggestion.suggested, vec![1, 2]);
    assert_eq!(titles[1].selection_evidence.segments.len(), 3);
    let mut undescribed = titles.clone();
    undescribed[1].selection_evidence.segments.clear();
    assert!(suggest(&undescribed).play_alls.is_empty());
}

#[test]
fn shared_boundary_requires_the_complete_ordered_program_partition() {
    let a = ("00001", 0, 44);
    let b = ("00002", 0, 44);
    let tail = ("00003", 0, 1);
    let extra = ("00004", 0, 5);
    let titles = vec![
        bd(1, 94.0, &[a, extra, b, tail], "eng"),
        bd(2, 45.0, &[a, tail], "eng"),
        bd(3, 45.0, &[b, tail], "eng"),
    ];
    assert!(suggest(&titles).play_alls.is_empty());
    let mut mismatched = titles;
    mismatched[0].selection_evidence.segments.pop();
    assert!(suggest(&mismatched).play_alls.is_empty());
}

#[test]
fn alternate_constituent_does_not_veto_a_composite() {
    let a = ("00001", 0, 40);
    let b = ("00002", 0, 40);
    let x = ("00003", 0, 4);
    let titles = vec![
        bd(1, 84.0, &[a, x, b], "eng"),
        bd(2, 40.0, &[a], "eng"),
        bd(3, 44.0, &[a, x], "eng"),
        bd(4, 40.0, &[b], "eng"),
    ];
    let suggestion = suggest(&titles);
    assert_eq!(suggestion.play_alls, vec![0]);
    assert_eq!(suggestion.suggested, vec![1, 3]);
}

#[test]
fn shared_boundary_rejects_incomplete_repeated_reordered_and_ambiguous_bodies() {
    let a = ("00001", 0, 20);
    let b = ("00002", 0, 20);
    let c = ("00003", 0, 20);
    let d = ("00004", 0, 20);
    let tail = ("00005", 0, 1);
    let extra = ("00006", 0, 1);
    let episodes = vec![vec![a, b, tail], vec![c, d, tail]];
    for (case, wrapper, bodies) in [
        (
            "extra content",
            vec![a, b, extra, c, d, tail],
            episodes.clone(),
        ),
        (
            "missing body segment",
            vec![a, b, c, tail],
            episodes.clone(),
        ),
        (
            "repeated body",
            vec![a, b, a, b, c, d, tail],
            episodes.clone(),
        ),
        (
            "reversed within body",
            vec![b, a, c, d, tail],
            episodes.clone(),
        ),
        (
            "interleaved bodies",
            vec![a, c, b, d, tail],
            episodes.clone(),
        ),
        (
            "unused contained body",
            vec![a, b, c, d, tail],
            vec![vec![a, b, tail], vec![c, d, tail], vec![a, c, tail]],
        ),
        (
            "ambiguous body prefix",
            vec![a, b, c, tail],
            vec![vec![a, tail], vec![a, b, tail], vec![c, tail]],
        ),
        (
            "boundary-only title",
            vec![a, b, c, d, tail],
            [episodes.clone(), vec![vec![tail]]].concat(),
        ),
        (
            "shared internal segment is not a boundary",
            vec![a, b, c, d, tail],
            vec![vec![a, tail, b], vec![c, tail, d]],
        ),
    ] {
        let mut titles = vec![bd(1, 81.0, &wrapper, "eng")];
        titles.extend(
            bodies
                .iter()
                .enumerate()
                .map(|(i, body)| bd(i as u16 + 2, 41.0, body, "eng")),
        );
        let snapshot = model(&titles);
        assert!(
            !snapshot
                .suggest_episodes(&StreamFilter::All, None, MIN_SECS)
                .play_alls
                .contains(&0),
            "{case}"
        );
        assert!(
            matches!(
                snapshot.resolve_episodes(&StreamFilter::All, None, MIN_SECS),
                Resolved::Confirm {
                    reason: SelectionReviewReason::MissingEpisodeRoster,
                    ..
                }
            ),
            "{case}"
        );
    }
}

#[test]
fn shared_boundary_rejects_locally_ambiguous_complete_partition() {
    let a = ("00001", 0, 20);
    let b = ("00002", 0, 20);
    let c = ("00003", 0, 20);
    let tail = ("00004", 0, 1);
    let titles = vec![
        bd(1, 81.0, &[a, b, a, c, tail], "eng"),
        bd(2, 41.0, &[a, b, tail], "eng"),
        bd(3, 21.0, &[a, tail], "eng"),
        bd(4, 21.0, &[c, tail], "eng"),
    ];
    // Greedily choosing AB yields a complete AB+A+C partition, but the opening
    // also matches A: conservatively reject this locally ambiguous choice.
    assert!(!suggest(&titles).play_alls.contains(&0));
}

#[test]
fn shared_boundary_rejects_reordered_opening_with_matching_coverage() {
    let intro_one = ("00001", 0, 1);
    let intro_two = ("00002", 0, 1);
    let a = ("00003", 0, 40);
    let b = ("00004", 0, 40);
    let tail = ("00005", 0, 1);
    let titles = vec![
        bd(1, 83.0, &[intro_two, intro_one, a, b, tail], "eng"),
        bd(2, 43.0, &[intro_one, intro_two, a, tail], "eng"),
        bd(3, 43.0, &[intro_one, intro_two, b, tail], "eng"),
    ];
    assert!(!suggest(&titles).play_alls.contains(&0));
}

#[test]
fn shared_boundary_requires_exact_unit_start_and_end_even_when_coverage_contains_it() {
    let a = ("00001", 0, 40);
    let b = ("00002", 0, 40);
    let tail = ("00003", 1, 2);
    let titles = vec![
        bd(1, 81.0, &[a, b, tail], "eng"),
        bd(2, 41.0, &[a, tail], "eng"),
        bd(3, 41.0, &[b, tail], "eng"),
    ];
    assert_eq!(suggest(&titles).play_alls, vec![0]);
    for which in [0, 1] {
        for field in ["unit", "start", "end"] {
            let mut mismatch = titles.clone();
            let boundary = mismatch[which]
                .selection_evidence
                .segments
                .last_mut()
                .unwrap();
            match field {
                "unit" => boundary.unit = "different".into(),
                // Expand the wrapper, or shrink a constituent, by just one tick:
                // containment still holds, but exact boundary equality does not.
                "start" if which == 0 => boundary.start -= 1,
                "start" => boundary.start += 1,
                "end" if which == 0 => boundary.end += 1,
                "end" => boundary.end -= 1,
                _ => unreachable!(),
            }
            assert!(suggest(&mismatch).play_alls.is_empty(), "{which} {field}");
        }
    }
}

#[test]
fn shared_boundaries_accept_prefix_suffix_and_multiple_segments() {
    let a = ("00010", 0, 40);
    let b = ("00020", 0, 40);
    let intro = [("00001", 0, 1), ("00002", 0, 1)];
    let outro = [("00030", 0, 1), ("00031", 0, 1)];
    for (prefix, suffix) in [
        (&intro[..], &[][..]),
        (&[][..], &outro[..]),
        (&intro[..], &outro[..]),
    ] {
        // Body order in the wrapper need not equal playlist or scan order; each
        // complete body must occur once and retain its own internal order.
        for body_order in [[a, b], [b, a]] {
            let wrap = [prefix, &body_order, suffix].concat();
            let first = [prefix, &[a], suffix].concat();
            let second = [prefix, &[b], suffix].concat();
            let titles = vec![
                bd(1, 84.0, &wrap, "eng"),
                bd(2, 44.0, &first, "eng"),
                bd(3, 44.0, &second, "eng"),
            ];
            let suggestion = suggest(&titles);
            assert_eq!(suggestion.play_alls, vec![0]);
            assert_eq!(suggestion.suggested, vec![1, 2]);
            assert_eq!(
                resolve(&titles),
                confirm(vec![1, 2], vec![vec![1, 2]], vec![])
            );
            assert_eq!(titles[1].clips.len(), prefix.len() + 1 + suffix.len());
        }
    }
}

#[test]
fn shared_boundary_language_selection_survives_scan_permutations() {
    let original = shared_terminal_titles();
    for order in [
        [0, 1, 2, 3, 4, 5, 6, 7],
        [7, 6, 5, 4, 3, 2, 1, 0],
        [3, 0, 7, 4, 1, 6, 5, 2],
        [2, 3, 4, 5, 6, 7, 0, 1],
    ] {
        let titles: Vec<_> = order.iter().map(|&i| original[i].clone()).collect();
        let snapshot = model(&titles);
        for (language, ids) in [("en", vec![7, 8, 9]), ("de", vec![4, 5, 6])] {
            let presentation =
                snapshot.suggest_episodes(&StreamFilter::All, Some(language), MIN_SECS);
            assert_eq!(
                presentation
                    .suggested
                    .iter()
                    .map(|&i| titles[i].playlist_id)
                    .collect::<Vec<_>>(),
                ids,
                "{order:?} {language}"
            );
            assert_eq!(
                presentation
                    .play_alls
                    .iter()
                    .map(|&i| titles[i].playlist_id)
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
            let audio = StreamFilter::Langs(vec![language.into()]);
            assert_eq!(
                snapshot.suggest_episodes(&audio, None, MIN_SECS),
                presentation
            );
            assert!(matches!(
                snapshot.resolve_episodes(&audio, None, MIN_SECS),
                Resolved::Confirm { suggested, .. } if suggested == presentation.suggested
            ));
        }
    }
}

#[test]
fn shared_boundary_aliases_keep_one_episode_copy_and_mark_both_wrappers() {
    let a = ("00001", 0, 40);
    let b = ("00002", 0, 40);
    let tail = ("00003", 0, 1);
    let titles = vec![
        bd(1, 81.0, &[a, b, tail], "eng"),
        bd(2, 81.0, &[a, b, tail], "deu"),
        bd(3, 41.0, &[a, tail], "eng"),
        bd(4, 41.0, &[a, tail], "deu"),
        bd(5, 41.0, &[b, tail], "eng"),
        bd(6, 41.0, &[b, tail], "deu"),
    ];
    let snapshot = model(&titles);
    for (language, expected, alternates) in [
        (
            None,
            vec![2, 4],
            vec![Alternate { index: 3, of: 2 }, Alternate { index: 5, of: 4 }],
        ),
        (
            Some("de"),
            vec![3, 5],
            vec![Alternate { index: 2, of: 3 }, Alternate { index: 4, of: 5 }],
        ),
    ] {
        let suggestion = snapshot.suggest_episodes(&StreamFilter::All, language, MIN_SECS);
        assert_eq!(suggestion.play_alls, vec![0, 1]);
        assert_eq!(suggestion.suggested, expected);
        assert_eq!(suggestion.alternates, alternates);
    }
}

#[test]
fn shared_boundary_suggestions_preserve_main_movie_and_explicit_picks() {
    let titles = shared_terminal_titles();
    let snapshot = model(&titles);
    for (language, expected_main) in [(None, 0), (Some("en"), 0), (Some("de"), 1)] {
        let audio = StreamFilter::All;
        assert_eq!(
            snapshot.select(&Selection::MainMovie, &audio, language),
            Ok(vec![expected_main])
        );
        assert_eq!(
            snapshot.main_or_episodes(&audio, language),
            Ok(vec![expected_main])
        );
        // The operator may still deliberately choose a wrapper and an episode.
        assert_eq!(
            snapshot.select(&Selection::Titles(vec![6, 0, 1]), &audio, language),
            Ok(vec![6, 0, 1])
        );
    }
    for episodes in [false, true] {
        assert_eq!(
            snapshot.resolve_explicit(&[6, 0, 1], episodes),
            Resolved::Run {
                titles: vec![6, 0, 1],
                episodes
            }
        );
    }
    assert_eq!(snapshot, model(&titles));
}
