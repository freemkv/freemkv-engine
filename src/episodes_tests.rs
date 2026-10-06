use super::*;
use libfreemkv::disc::Extent;

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
fn picks_the_episode_cluster_and_drops_play_all_and_extras() {
    // 6 × ~44-min episodes, a ~264-min "play all" (their sum), a 2-min extra.
    let ep = 44.0 * 60.0;
    let mut titles = vec![title(ep * 6.0, 100)];
    for k in 0..6 {
        titles.push(title(ep + (k as f64), 1000 + k * 100));
    }
    titles.push(title(2.0 * 60.0, 50));
    assert_eq!(episode_titles(&titles), vec![1, 2, 3, 4, 5, 6]);
}

fn spans(dur: f64, spans: &[(u32, u32)]) -> DiscTitle {
    let mut t = title(dur, 0);
    t.extents = spans
        .iter()
        .map(|&(start_lba, sector_count)| Extent {
            start_lba,
            sector_count,
        })
        .collect();
    t
}

#[test]
fn a_play_all_is_the_title_holding_its_episodes_sectors() {
    let ep = 44.0 * 60.0;
    let titles = vec![
        spans(ep * 3.0, &[(1000, 1000), (2000, 1000), (3000, 1000)]),
        spans(ep, &[(1000, 1000)]),
        spans(ep, &[(2000, 1000)]),
        spans(ep, &[(3000, 1000)]),
        spans(120.0, &[(1000, 50)]), // a recap inside it: too short to be a part
        spans(ep, &[(9000, 1000)]),  // same length, elsewhere: proves nothing
    ];
    use TitleRole::*;
    assert_eq!(
        title_roles(&titles),
        vec![
            Some(PlayAll),
            Some(Episode),
            Some(Episode),
            Some(Episode),
            None,
            None
        ]
    );
}

// As on a real DVD: every episode ends on one shared end-card cell that the play-all holds
// once, and an episode can be authored twice with only the audio differing.
#[test]
fn episodes_sharing_an_end_card_or_authored_twice_are_all_episodes() {
    let ep = 48.0 * 60.0;
    let titles = vec![
        spans(ep * 2.0, &[(0, 2000), (2000, 2000), (9000, 5)]),
        spans(ep, &[(0, 2000), (9000, 5)]),
        spans(ep, &[(0, 2000), (9000, 5)]),
        spans(ep, &[(2000, 2000), (9000, 5)]),
    ];
    use TitleRole::*;
    assert_eq!(
        title_roles(&titles),
        vec![Some(PlayAll), Some(Episode), Some(Episode), Some(Episode)]
    );
}

// Decoy playlists repeat the feature's sectors exactly: none is smaller than another.
#[test]
fn identical_playlists_are_not_play_alls() {
    let f = 110.0 * 60.0;
    let titles = vec![
        spans(f, &[(0, 9000)]),
        spans(f, &[(0, 9000)]),
        spans(f, &[(0, 9000)]),
    ];
    assert!(title_roles(&titles).iter().all(Option::is_none));
}

// Two cuts sharing most sectors overlap, so a title holding both plays no episodes.
#[test]
fn overlapping_parts_do_not_make_a_play_all() {
    let t = 50.0 * 60.0;
    let titles = vec![
        spans(t * 2.0, &[(0, 4000)]),
        spans(t, &[(0, 2500)]),
        spans(t, &[(500, 2500)]),
    ];
    assert!(title_roles(&titles).iter().all(Option::is_none));
}

#[test]
fn a_film_disc_proves_no_roles() {
    let titles = vec![
        spans(120.0 * 60.0, &[(0, 9000)]),
        spans(12.0 * 60.0, &[(20_000, 500)]),
    ];
    assert!(title_roles(&titles).iter().all(Option::is_none));
}

#[test]
fn dedups_duplicate_angle_titles() {
    let ep = 44.0 * 60.0;
    let titles = vec![title(ep, 1000), title(ep, 1000), title(ep, 2000)];
    assert_eq!(episode_titles(&titles), vec![0, 2]);
}

// Episodes that open on the same intro clip are different content: only a matching
// extent list and duration is a duplicate.
#[test]
fn episodes_sharing_an_opening_clip_are_all_kept() {
    let ep = 24.0 * 60.0;
    let episode = |body: u32, dur: f64| {
        let mut t = title(dur, 500);
        t.extents.push(Extent {
            start_lba: body,
            sector_count: 1000,
        });
        t
    };
    let titles = vec![
        episode(1000, ep),
        episode(2000, ep),
        episode(3000, ep + 30.0),
    ];
    assert_eq!(episode_titles(&titles), vec![0, 1, 2]);
    let same_start = vec![title(ep, 1000), title(ep + 60.0, 1000)];
    assert_eq!(episode_titles(&same_start), vec![0, 1]);
}

#[test]
fn single_qualifying_title_passes_through() {
    let titles = vec![title(90.0 * 60.0, 1000), title(60.0, 50)];
    assert_eq!(episode_titles(&titles), vec![0]);
}

// A play-all spanning the given episode titles' extents.
fn play_all(eps: &[&DiscTitle]) -> DiscTitle {
    let mut t = title(eps.iter().map(|e| e.duration_secs).sum(), 0);
    t.extents = eps.iter().flat_map(|e| e.extents.clone()).collect();
    t
}

// Play-alls never displace the episodes, even when as many or more of them exist: they
// play the episodes' extents, or run for the episodes' summed length.
#[test]
fn play_all_titles_never_displace_the_episode_length() {
    let ep = 44.0 * 60.0;
    let (a, b) = (title(ep, 1000), title(ep * 1.02, 2000));
    assert_eq!(episode_titles(&[a.clone(), play_all(&[&a, &a])]), vec![0]);
    let (p2, p3) = (play_all(&[&a, &a]), play_all(&[&a, &a, &a]));
    assert_eq!(episode_titles(&[a.clone(), p2.clone(), p3]), vec![0]);
    let titles = [
        a.clone(),
        b.clone(),
        play_all(&[&a, &b]),
        play_all(&[&b, &a]),
    ];
    assert_eq!(episode_titles(&titles), vec![0, 1]);
}

// Equal-size groups of extras and episodes: the episodes win (reviewer cases).
#[test]
fn as_many_extras_as_episodes_keep_the_episodes() {
    let m = 60.0;
    let t = |ds: &[f64]| -> Vec<DiscTitle> {
        ds.iter()
            .enumerate()
            .map(|(k, d)| title(d * m, 1000 + k as u32 * 5000))
            .collect()
    };
    let three = t(&[44.0, 44.5, 45.0, 15.0, 15.0, 15.0]);
    assert_eq!(episode_titles(&three), vec![0, 1, 2]);
    let four = t(&[44.0, 44.0, 44.5, 44.5, 20.0, 20.5, 20.0, 20.5, 177.0]);
    assert_eq!(episode_titles(&four), vec![0, 1, 2, 3]);
}

// Bench case: two episodes and two play-alls of their summed length, no shared extents.
#[test]
fn densest_cluster_beats_upper_median() {
    let m = 60.0;
    let t = |ds: &[f64]| -> Vec<DiscTitle> {
        ds.iter()
            .enumerate()
            .map(|(k, d)| title(d * m, 1000 + k as u32 * 100))
            .collect()
    };
    assert_eq!(episode_titles(&t(&[44.0, 44.5, 88.0, 88.5])), vec![0, 1]);
    assert_eq!(
        episode_titles(&t(&[44.0, 44.5, 88.0, 88.5, 120.0])),
        vec![0, 1]
    );
}

// A title with no extents has no content identity: equal durations are not duplicates.
#[test]
fn titles_without_extents_are_never_deduped() {
    let ep = 44.0 * 60.0;
    let mut a = title(ep, 0);
    let mut b = title(ep, 0);
    a.extents.clear();
    b.extents.clear();
    assert_eq!(episode_titles(&[a, b, title(ep, 0)]), vec![0, 1, 2]);
}

#[test]
fn none_qualify_yields_empty() {
    let titles = vec![title(120.0, 10), title(90.0, 20), title(f64::NAN, 30)];
    assert!(episode_titles(&titles).is_empty());
}
