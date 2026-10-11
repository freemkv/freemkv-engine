use super::*;
use libfreemkv::disc::{ContentSegment, Extent};

fn title(dur_secs: f64, start_lba: u32) -> DiscTitle {
    let mut t = DiscTitle::empty();
    t.duration_secs = dur_secs;
    t.size_bytes = (dur_secs as u64) * 1_000_000;
    t.extents = vec![Extent {
        start_lba,
        sector_count: 1000,
    }];
    t
}

#[test]
fn runtime_cluster_and_extras_require_review() {
    // 6 × ~44-min episodes, a ~264-min "play all" (their sum), a 2-min extra.
    let ep = 44.0 * 60.0;
    let mut titles = vec![title(ep * 6.0, 100)];
    for k in 0..6 {
        titles.push(title(ep + (k as f64), 1000 + k * 100));
    }
    titles.push(title(2.0 * 60.0, 50));
    assert_unknown(&titles);
}

fn spans(dur: f64, spans: &[(u32, u32)]) -> DiscTitle {
    let mut t = title(dur, 0);
    t.content_format = libfreemkv::ContentFormat::DvdPs;
    t.extents = spans
        .iter()
        .map(|&(start_lba, sector_count)| Extent {
            start_lba,
            sector_count,
        })
        .collect();
    t.selection_evidence.segments = ContentSegment::from_cell_extents(&t.extents);
    t
}

// As on a real DVD: every episode ends on one shared end-card cell that the play-all holds
// once, and an episode can be authored twice with only the audio differing.
// Decoy playlists repeat the feature's sectors exactly: none is smaller than another.
// Two cuts sharing most sectors overlap, so a title holding both plays no episodes.
#[test]
fn dedups_duplicate_angle_titles() {
    let ep = 44.0 * 60.0;
    let titles = vec![
        spans(ep, &[(1000, 1000)]),
        spans(ep, &[(1000, 1000)]),
        spans(ep, &[(2000, 1000)]),
    ];
    assert_eq!(known_episode_titles(&titles), vec![0, 2]);
}

// A shared opening does not establish the identity of the whole episode. DVD titles, so
// each has a cell identity: equal first cell, different later cells (or lengths).
#[test]
fn episodes_sharing_an_opening_clip_are_all_kept() {
    let ep = 24.0 * 60.0;
    let titles = vec![
        spans(ep, &[(500, 1000), (1000, 1000)]),
        spans(ep, &[(500, 1000), (2000, 1000)]),
        spans(ep + 30.0, &[(500, 1000), (3000, 1000)]),
    ];
    let ids: Vec<_> = titles.iter().map(crate::presentation::identity).collect();
    assert!(
        ids.iter().all(Option::is_some),
        "fixture: DVD cell identities"
    );
    assert_eq!(known_episode_titles(&titles), vec![0, 1, 2]);
    let same_start = vec![
        spans(ep, &[(1000, 1000)]),
        spans(ep + 60.0, &[(1000, 1060)]),
    ];
    assert_eq!(known_episode_titles(&same_start), vec![0, 1]);
}

#[test]
fn alternate_playlists_for_one_program_are_selected_once() {
    let ep = 53.0 * 60.0;
    let mut titles = Vec::new();
    for (n, start) in [("islands", 1000), ("mountains", 2000), ("jungles", 3000)] {
        for extent_start in [start, start + 10_000] {
            let mut t = title(ep, extent_start);
            t.clips = vec![libfreemkv::Clip {
                clip_id: n.into(),
                in_time: 0,
                out_time: 53 * 60 * 45_000,
                duration_secs: ep,
                source_packets: 0,
                feed_span: None,
            }];
            t.selection_evidence.segments = ContentSegment::from_clips(&t.clips);
            titles.push(t);
        }
    }
    assert_eq!(known_episode_titles(&titles), vec![0, 2, 4]);
}

#[test]
fn dvd_variants_with_the_same_cells_are_selected_once_even_when_duration_differs() {
    let mut english = spans(48.0 * 60.0, &[(10_000, 80_000), (100_000, 20_000)]);
    let mut german = spans(46.0 * 60.0, &[(10_000, 80_000), (100_000, 20_000)]);
    english.playlist = "VTS_01_7.VOB".into();
    german.playlist = "VTS_01_4.VOB".into();
    assert_eq!(known_episode_titles(&[english, german]), vec![0]);
}

#[test]
fn dvd_titles_with_different_cells_remain_distinct_despite_similar_duration() {
    let a = spans(48.0 * 60.0, &[(10_000, 80_000), (100_000, 20_000)]);
    let b = spans(47.0 * 60.0, &[(200_000, 80_000), (300_000, 20_000)]);
    assert_eq!(known_episode_titles(&[a, b]), vec![0, 1]);
}

#[test]
fn distinct_programs_with_shared_intro_are_not_collapsed() {
    let ep = 53.0 * 60.0;
    let program = |body: &str, extent_start: u32| {
        let mut t = title(ep, extent_start);
        t.clips = vec![
            libfreemkv::Clip {
                clip_id: "shared-intro".into(),
                in_time: 0,
                out_time: 30 * 45_000,
                duration_secs: 30.0,
                source_packets: 0,
                feed_span: None,
            },
            libfreemkv::Clip {
                clip_id: body.into(),
                in_time: 0,
                out_time: 52 * 60 * 45_000,
                duration_secs: 52.0 * 60.0,
                source_packets: 0,
                feed_span: None,
            },
        ];
        t.selection_evidence.segments = ContentSegment::from_clips(&t.clips);
        t
    };
    let titles = vec![
        program("episode-a", 1000),
        program("episode-a", 20_000),
        program("episode-b", 30_000),
    ];
    assert_eq!(known_episode_titles(&titles), vec![0, 2]);
}

#[test]
fn one_long_title_is_not_an_episode_roster() {
    let titles = vec![title(90.0 * 60.0, 1000), title(60.0, 50)];
    assert_unknown(&titles);
}

// A play-all spanning the given episode titles' extents.
fn play_all(eps: &[&DiscTitle]) -> DiscTitle {
    let mut t = title(eps.iter().map(|e| e.duration_secs).sum(), 0);
    t.extents = eps.iter().flat_map(|e| e.extents.clone()).collect();
    t
}

// Structural play-all hints cannot establish menu reachability.
#[test]
fn play_all_structure_alone_does_not_prove_a_roster() {
    let ep = 44.0 * 60.0;
    let (a, b) = (title(ep, 1000), title(ep * 1.02, 2000));
    assert_unknown(&[a.clone(), play_all(&[&a, &a])]);
    let (p2, p3) = (play_all(&[&a, &a]), play_all(&[&a, &a, &a]));
    assert_unknown(&[a.clone(), p2.clone(), p3]);
    let titles = [
        a.clone(),
        b.clone(),
        play_all(&[&a, &b]),
        play_all(&[&b, &a]),
    ];
    assert_unknown(&titles);
}

// Similar runtimes do not distinguish extras from episodes.
#[test]
fn equal_groups_of_extras_and_episodes_require_review() {
    let m = 60.0;
    let t = |ds: &[f64]| -> Vec<DiscTitle> {
        ds.iter()
            .enumerate()
            .map(|(k, d)| title(d * m, 1000 + k as u32 * 5000))
            .collect()
    };
    let three = t(&[44.0, 44.5, 45.0, 15.0, 15.0, 15.0]);
    assert_unknown(&three);
    let four = t(&[44.0, 44.0, 44.5, 44.5, 20.0, 20.5, 20.0, 20.5, 177.0]);
    assert_unknown(&four);
}

// Bench case: two episodes and two play-alls of their summed length, no shared extents.
#[test]
fn density_and_upper_median_do_not_authorize_episodes() {
    let m = 60.0;
    let t = |ds: &[f64]| -> Vec<DiscTitle> {
        ds.iter()
            .enumerate()
            .map(|(k, d)| title(d * m, 1000 + k as u32 * 100))
            .collect()
    };
    assert_unknown(&t(&[44.0, 44.5, 88.0, 88.5]));
    assert_unknown(&t(&[44.0, 44.5, 88.0, 88.5, 120.0]));
}

// A title with no extents has no content identity: equal durations are not duplicates.
#[test]
fn titles_without_extents_are_never_deduped() {
    let ep = 44.0 * 60.0;
    let mut a = title(ep, 0);
    let mut b = title(ep, 0);
    a.extents.clear();
    b.extents.clear();
    assert_eq!(known_episode_titles(&[a, b, title(ep, 0)]), vec![0, 1, 2]);
}

#[test]
fn short_and_unknown_durations_remain_review_candidates() {
    let titles = vec![title(120.0, 10), title(90.0, 20), title(f64::NAN, 30)];
    assert_unknown(&titles);
}

fn known_episode_titles(titles: &[DiscTitle]) -> Vec<usize> {
    let mut titles = titles.to_vec();
    let members = (0..titles.len()).collect::<Vec<_>>();
    crate::test_fixtures::authored_episodes(&mut titles, &members);
    episodes(&titles).unwrap_or_default()
}

fn episodes(titles: &[DiscTitle]) -> Result<Vec<usize>, crate::SelectionReview> {
    crate::SelectionModel::from_titles(titles).select(
        &crate::Selection::Episodes,
        &crate::StreamFilter::All,
        None,
    )
}

fn assert_unknown(titles: &[DiscTitle]) {
    let review = episodes(titles).unwrap_err();
    assert_eq!(review.candidates, (0..titles.len()).collect::<Vec<_>>());
}
