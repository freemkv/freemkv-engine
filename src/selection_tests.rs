use super::*;
use crate::test_fixtures::clip_title as title;
use libfreemkv::disc::ContentSegment;

// Rescan a fixture title after editing its clips.
fn rescan(title: &mut DiscTitle) {
    title.selection_evidence.segments = ContentSegment::from_clips(&title.clips);
}

// Synthetic evidence contract fixture, not a claim about a real menu schema.
fn roster(titles: &mut [DiscTitle], members: &[usize]) {
    let title_count = titles.len();
    let mut identities = HashMap::new();
    let mut next = 0;
    let mut orders = HashMap::new();
    for &index in members {
        let identity = crate::presentation::identity(&titles[index]);
        let ordinal = identity
            .as_ref()
            .and_then(|id| identities.get(id))
            .copied()
            .unwrap_or_else(|| {
                let ordinal = next;
                next += 1;
                if let Some(id) = identity {
                    identities.insert(id, ordinal);
                }
                ordinal
            });
        orders.insert(index, ordinal);
    }
    for (index, title) in titles.iter_mut().enumerate() {
        title.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "fixture:verified-episode-menu".into(),
            title_count,
            ordinal: orders.get(&index).copied(),
        };
    }
}

fn select(titles: &[DiscTitle]) -> Result<Vec<usize>, SelectionReview> {
    SelectionModel::from_titles(titles).select(
        &Selection::Episodes,
        &StreamFilter::Langs(vec!["de".into()]),
        None,
    )
}

#[test]
fn authored_partition_order_survives_scan_order_and_audio_alternates() {
    let mut titles = vec![
        title(1, "b", 4000.0, "eng"),
        title(2, "a", 20.0, "eng"),
        title(3, "b", 4000.0, "deu"),
    ];
    roster(&mut titles, &[0, 1, 2]);
    for (title, ordinal) in titles.iter_mut().zip([1, 0, 1]) {
        if let EpisodeEvidence::Authored { ordinal: order, .. } =
            &mut title.selection_evidence.episodes
        {
            *order = Some(ordinal);
        }
    }
    assert_eq!(select(&titles), Ok(vec![1, 2]));
    for (index, ordinal) in [(0, Some(0)), (0, Some(2)), (1, Some(4))] {
        let mut invalid = titles.clone();
        if let EpisodeEvidence::Authored { ordinal: order, .. } =
            &mut invalid[index].selection_evidence.episodes
        {
            *order = ordinal;
        }
        assert!(select(&invalid).is_err());
    }
}

#[test]
fn reported_planet_earth_order_and_vpc_intervals_never_use_size_order_or_fuzzy_identity() {
    // Synthetic model fixture from reported IDs/intervals, not a disc/parser test.
    // The third program uses a synthetic interval; only its exact alias relation
    // was reported. Producer role evidence explicitly excludes VPC presentations.
    let clip = |id, name: &str, start, end, language| {
        let mut t = title(id, name, f64::from(end - start) / 45_000.0, language);
        t.clips[0].in_time = start;
        t.clips[0].out_time = end;
        rescan(&mut t);
        t
    };
    let mut titles = vec![
        title(163, "play-all", 9500.0, "eng"),
        clip(147, "00058", 524_280, 143_930_667, "eng"),
        clip(166, "00058", 524_280, 143_808_671, "deu"),
        clip(149, "synthetic-third", 0, 100_000_000, "eng"),
        clip(168, "synthetic-third", 0, 100_000_000, "deu"),
        clip(148, "00059", 524_280, 139_848_464, "eng"),
        clip(167, "00059", 524_280, 139_681_423, "deu"),
    ];
    roster(&mut titles, &[1, 5, 3]);
    let report = select(&titles).unwrap();
    assert_eq!(report, vec![1, 5, 3]);
    assert_eq!(
        report
            .iter()
            .map(|&i| titles[i].playlist_id)
            .collect::<Vec<_>>(),
        vec![147, 148, 149]
    );
    for (vpc, order) in [(2, 0), (6, 1)] {
        let mut conflicting = titles.clone();
        if let EpisodeEvidence::Authored { ordinal, .. } =
            &mut conflicting[vpc].selection_evidence.episodes
        {
            *ordinal = Some(order);
        }
        assert_eq!(
            select(&conflicting).map_err(|review| review.reason),
            Err(SelectionReviewReason::InvalidEpisodeRoster)
        );
    }
}

#[test]
fn observed_seventeen_title_authored_roster_selects_three_exact_programs() {
    // Observed scan metadata, not new producer proof. Unreported non-member
    // clip details and audio are placeholders; their exclusion is authoritative.
    let ids = [
        163, 147, 166, 149, 168, 148, 167, 162, 99, 150, 153, 154, 155, 156, 97, 98, 161,
    ];
    let mut titles: Vec<_> = ids
        .iter()
        .map(|&id| title(id, &format!("non-member-{id}"), 3200.0, "deu"))
        .collect();
    for (index, clip, end) in [
        (1, "00058", 143_930_667),
        (2, "00058", 143_808_671),
        (3, "00060", 141_473_838),
        (4, "00060", 141_473_838),
        (5, "00059", 139_848_464),
        (6, "00059", 139_681_423),
    ] {
        let t = &mut titles[index];
        t.clips[0].clip_id = clip.into();
        t.clips[0].in_time = 524_280;
        t.clips[0].out_time = end;
        t.duration_secs = f64::from(end - 524_280) / 45_000.0;
        t.clips[0].duration_secs = t.duration_secs;
        rescan(t);
    }
    titles[0].clips = [2, 6, 4]
        .into_iter()
        .flat_map(|i| titles[i].clips.clone())
        .collect();
    rescan(&mut titles[0]);
    titles[0].duration_secs = titles[0].clips.iter().map(|c| c.duration_secs).sum();
    roster(&mut titles, &[2, 6, 4, 3]);

    for reversed in [false, true] {
        for shift in 0..titles.len() {
            let mut scanned = titles.clone();
            if reversed {
                scanned.reverse();
            }
            scanned.rotate_left(shift);
            let report = SelectionModel::from_titles(&scanned)
                .select(&Selection::Episodes, &StreamFilter::All, None)
                .unwrap();
            assert_eq!(report.len(), 3, "{reversed}/{shift}: {report:?}");
            let selected: Vec<_> = report.iter().map(|&i| &scanned[i]).collect();
            assert_eq!(selected[0].playlist_id, 166);
            assert_eq!(selected[1].playlist_id, 167);
            assert!(matches!(selected[2].playlist_id, 149 | 168));
            for (ordinal, selected) in selected.iter().enumerate() {
                assert!(matches!(
                    selected.selection_evidence.episodes,
                    EpisodeEvidence::Authored { ordinal: Some(order), title_count: 17, .. }
                        if order == ordinal
                ));
            }
        }
    }
}

#[test]
fn authored_order_is_required_and_unknown_identity_cannot_share_a_slot() {
    let original = vec![
        title(1, "a", 100.0, "eng"),
        title(2, "b", 200.0, "deu"),
        title(3, "extra", 300.0, "eng"),
    ];
    for case in 0..3 {
        let mut titles = original.clone();
        roster(&mut titles, &[0, 1]);
        for (index, title) in titles.iter_mut().enumerate() {
            if let EpisodeEvidence::Authored { ordinal, .. } =
                &mut title.selection_evidence.episodes
            {
                match case {
                    0 if index == 1 => *ordinal = Some(usize::MAX),
                    1 if index < 2 => {
                        *ordinal = Some(0);
                        title.clips.clear();
                        rescan(title);
                    }
                    2 if index < 2 => *ordinal = Some(0),
                    _ => {}
                }
            }
        }
        let review = select(&titles).unwrap_err();
        assert!(!review.candidates.is_empty(), "case {case}");
    }
}

#[test]
fn authored_six_alternates_keep_three_uneven_short_episodes_and_reject_distinct_fake() {
    let mut titles = Vec::new();
    for (program, secs) in [("a", 90.0), ("b", 2100.0), ("c", 3900.0)] {
        for language in ["eng", "deu"] {
            titles.push(title(titles.len() as u16, program, secs, language));
        }
    }
    titles.push(title(6, "fake", 2100.0, "deu"));
    roster(&mut titles, &[0, 1, 2, 3, 4, 5]);
    assert_eq!(select(&titles), Ok(vec![1, 3, 5]));
}

#[test]
fn unknown_is_a_review_hold_not_runtime_proof() {
    let titles = vec![title(0, "a", 90.0, "eng"), title(1, "b", 2400.0, "deu")];
    assert_eq!(
        select(&titles),
        Err(SelectionReview {
            reason: SelectionReviewReason::MissingEpisodeRoster,
            candidates: vec![0, 1],
        })
    );
}

#[test]
fn partial_conflicting_invalid_and_sliced_rosters_fail_closed() {
    let original = vec![title(0, "a", 90.0, "eng"), title(1, "b", 2400.0, "deu")];
    for case in 0..5 {
        let mut titles = original.clone();
        roster(&mut titles, &[0, 1]);
        let expected = match case {
            0 => {
                titles[1].selection_evidence.episodes = EpisodeEvidence::Unknown;
                SelectionReviewReason::IncompleteEpisodeRoster
            }
            1 => {
                if let EpisodeEvidence::Authored { roster, .. } =
                    &mut titles[1].selection_evidence.episodes
                {
                    *roster = "different".into();
                }
                SelectionReviewReason::ConflictingEpisodeRoster
            }
            2 => {
                if let EpisodeEvidence::Authored { roster, .. } =
                    &mut titles[0].selection_evidence.episodes
                {
                    roster.clear();
                }
                SelectionReviewReason::InvalidEpisodeRoster
            }
            3 => {
                titles.pop();
                SelectionReviewReason::IncompleteEpisodeRoster
            }
            _ => {
                titles[1].playlist_id = 0;
                SelectionReviewReason::InvalidEpisodeRoster
            }
        };
        assert_eq!(
            select(&titles).map_err(|review| review.reason),
            Err(expected),
            "case {case}"
        );
    }
}

#[test]
fn empty_complete_roster_is_incomplete_not_an_empty_selection() {
    let mut titles = vec![title(0, "a", 90.0, "eng"), title(1, "b", 2400.0, "deu")];
    roster(&mut titles, &[]);
    assert_eq!(
        select(&titles).map_err(|review| review.reason),
        Err(SelectionReviewReason::IncompleteEpisodeRoster)
    );
}

#[test]
fn ordinal_gap_is_an_invalid_roster() {
    let mut titles = vec![title(0, "a", 90.0, "eng"), title(1, "b", 2400.0, "deu")];
    roster(&mut titles, &[0, 1]);
    if let EpisodeEvidence::Authored { ordinal, .. } = &mut titles[1].selection_evidence.episodes {
        *ordinal = Some(2);
    }
    assert_eq!(
        select(&titles).map_err(|review| review.reason),
        Err(SelectionReviewReason::InvalidEpisodeRoster)
    );
}

#[test]
fn exact_intervals_and_unknown_identity_remain_conservative_under_authored_roster() {
    let a = title(0, "intro", 1200.0, "eng");
    let mut b = a.clone();
    b.playlist_id = 1;
    b.clips[0].in_time += 1;
    rescan(&mut b);
    let mut c = a.clone();
    c.playlist_id = 2;
    c.clips.clear();
    rescan(&mut c);
    let mut d = c.clone();
    d.playlist_id = 3;
    let mut titles = vec![a, b, c, d];
    roster(&mut titles, &[0, 1, 2, 3]);
    assert_eq!(select(&titles), Ok(vec![0, 1, 2, 3]));
}

#[test]
fn excluded_equivalent_cannot_supply_audio_to_an_authored_episode() {
    let mut titles = vec![title(0, "a", 100.0, "eng"), title(1, "a", 100.0, "deu")];
    roster(&mut titles, &[0]);
    assert_eq!(select(&titles), Ok(vec![0]));
}

#[test]
fn owned_model_preserves_audio_and_explicit_choices() {
    let titles = vec![title(0, "a", 100.0, "eng"), title(1, "a", 200.0, "deu")];
    let model = SelectionModel::from_titles(&titles);
    drop(titles);
    let audio = StreamFilter::Langs(vec!["de".into()]);
    assert_eq!(
        model.select(&Selection::MainMovie, &audio, None),
        Ok(vec![1])
    );
    assert_eq!(model.main_title(&audio, None), Some(1));
    for (selection, expected) in [
        (Selection::Titles(vec![1, 0, 1, 99]), vec![1, 0]),
        (Selection::All, vec![0, 1]),
        (Selection::Longest, vec![1]),
    ] {
        assert_eq!(model.select(&selection, &audio, None), Ok(expected));
    }
}
