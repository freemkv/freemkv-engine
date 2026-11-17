use super::*;
use libfreemkv::DiscTitle;
use libfreemkv::disc::ContentSegment;

#[test]
fn nested_presentations_bound_shared_coverage_work() {
    const N: usize = 256;
    let titles: Vec<_> = (0..N)
        .map(|index| {
            let mut title = DiscTitle::empty();
            title.duration_secs = 1_000.0;
            title.selection_evidence.segments = vec![ContentSegment {
                unit: "one".into(),
                start: 0,
                end: 10_000 + index as u64,
            }];
            title
        })
        .collect();
    let model = SelectionModel::from_titles(&titles);
    let mut work = Work::default();
    let suggestion = model.suggest_episodes_with_work(&StreamFilter::All, None, 600.0, &mut work);
    assert!(suggestion.play_alls.is_empty());
    assert_eq!(work.bitmap_words, 0);
    assert!(
        work.shared_calls <= 4 * N * N,
        "{} shared computations exceed the {} bound for {N} nested presentations",
        work.shared_calls,
        4 * N * N
    );
}

fn finite_titles(masks: &[u8]) -> Vec<DiscTitle> {
    masks
        .iter()
        .map(|&mask| {
            let mut title = DiscTitle::empty();
            title.duration_secs = 1_000.0;
            title.selection_evidence.segments = (0..4)
                .filter(|&atom| mask & (1 << atom) != 0)
                .map(|atom| ContentSegment {
                    unit: format!("atom{atom}"),
                    start: 0,
                    end: 1,
                })
                .collect();
            title
        })
        .collect()
}

fn finite_inner(outer: usize, live: &[usize], masks: &[u8]) -> Vec<usize> {
    live.iter()
        .copied()
        .filter(|&other| {
            masks[other] != 0
                && masks[other] != masks[outer]
                && masks[other] & masks[outer] == masks[other]
        })
        .collect()
}

fn finite_pair(inner: &[usize], masks: &[u8]) -> bool {
    inner
        .iter()
        .enumerate()
        .any(|(at, &a)| inner[at + 1..].iter().any(|&b| masks[a] & masks[b] == 0))
}

// The old ANY-pair semantics expressed as finite sets, with the unchanged boundary,
// representative, ordering and clustering helpers. No Coverage or bitmap queries.
fn finite_suggestion(model: &SelectionModel, masks: &[u8]) -> Suggestion {
    let scores = model.scores(&StreamFilter::All, None);
    let mut live = model.representatives((0..masks.len()).collect(), &scores);
    let mut alternates: Vec<_> = (0..masks.len())
        .filter(|index| !live.contains(index))
        .map(|index| Alternate {
            index,
            of: *live
                .iter()
                .find(|&&kept| model.titles[kept].presentation == model.titles[index].presentation)
                .unwrap(),
        })
        .collect();
    model.sort_play_order(&mut live);
    let mut play_alls = Vec::new();
    loop {
        let found: Vec<_> = live
            .iter()
            .copied()
            .filter(|&outer| {
                let inner = finite_inner(outer, &live, masks);
                finite_pair(&inner, masks) || boundary_partition(outer, &inner, &model.titles)
            })
            .collect();
        if found.is_empty() {
            break;
        }
        live.retain(|index| !found.contains(index));
        play_alls.extend(found);
    }
    alternates.retain(|alternate| {
        if play_alls.contains(&alternate.of) {
            play_alls.push(alternate.index);
            false
        } else {
            true
        }
    });
    model.sort_play_order(&mut play_alls);

    // Compute connected overlap groups directly from set cardinalities.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in live {
        let mut members = vec![index];
        let mut at = 0;
        while at < groups.len() {
            let overlaps = groups[at].iter().any(|&other| {
                masks[index] != 0
                    && masks[other] != 0
                    && (masks[index] & masks[other]).count_ones() * 2
                        >= masks[index].count_ones().min(masks[other].count_ones())
            });
            if overlaps {
                members.extend(groups.remove(at));
            } else {
                at += 1;
            }
        }
        groups.push(members);
    }
    let mut kept = Vec::new();
    for members in groups {
        // Generated titles have equal stream scores: canonical scan index wins.
        let best = *members.iter().min().unwrap();
        alternates.extend(
            members
                .into_iter()
                .filter(|&index| index != best)
                .map(|index| Alternate { index, of: best }),
        );
        kept.push(best);
    }
    let folded = alternates.clone();
    for alternate in &mut alternates {
        if let Some(other) = folded.iter().find(|other| other.index == alternate.of) {
            alternate.of = other.of;
        }
    }
    alternates.sort_by(|a, b| model.play_order(a.index, b.index));
    let mut clusters = model.clusters(&kept);
    clusters.sort_by(|a, b| model.play_order(a[0], b[0]));
    Suggestion {
        suggested: match clusters.as_slice() {
            [one] => one.clone(),
            _ => Vec::new(),
        },
        clusters,
        alternates,
        play_alls,
    }
}

#[test]
fn generated_finite_sets_match_pair_and_full_suggestion_oracles() {
    // Every nonempty family from the seven nonempty subsets of three atoms.
    // This includes {x,y}, {x}, {y} inside {x,y,z}, and overlapping alternate cuts.
    for family in 1..128 {
        let mut masks: Vec<u8> = (1..8)
            .filter(|mask| family & (1 << (mask - 1)) != 0)
            .collect();
        masks.push(0); // Undescribed titles have no disjointness edges.
        let mut titles = finite_titles(&masks);
        if let Some(index) = masks.iter().position(|mask| mask.count_ones() >= 2) {
            // Equal coverage, one distinct ordered presentation and one exact alias.
            let mut reversed = titles[index].clone();
            reversed.selection_evidence.segments.reverse();
            titles.push(reversed);
            titles.push(titles[index].clone());
            masks.extend([masks[index]; 2]);
        }
        let model = SelectionModel::from_titles(&titles);
        let scores = model.scores(&StreamFilter::All, None);
        let mut initial = model.representatives((0..titles.len()).collect(), &scores);
        model.sort_play_order(&mut initial);
        let coverage: Vec<_> = model.titles.iter().map(Coverage::of).collect();
        let mut work = Work::default();
        let mut pairs = DisjointPairs::new(&initial, &coverage, &mut work);
        // All eligibility masks exercise removed titles and repeated scratch-mask reuse.
        for eligibility in 0..1 << initial.len() {
            let live: Vec<_> = initial
                .iter()
                .enumerate()
                .filter(|&(slot, _)| eligibility & (1 << slot) != 0)
                .map(|(_, &index)| index)
                .collect();
            for &outer in &live {
                let inner = finite_inner(outer, &live, &masks);
                let expected_pair = finite_pair(&inner, &masks);
                assert_eq!(
                    pairs.has_pair(&inner, &mut work),
                    expected_pair,
                    "family={family} live={live:?} outer={outer}"
                );
                assert_eq!(
                    play_all(
                        outer,
                        &live,
                        &coverage,
                        &model.titles,
                        &mut pairs,
                        &mut work
                    ),
                    expected_pair || boundary_partition(outer, &inner, &model.titles),
                    "family={family} live={live:?} outer={outer}"
                );
            }
        }
        assert_eq!(
            model.suggest_episodes(&StreamFilter::All, None, 600.0),
            finite_suggestion(&model, &masks),
            "family={family}"
        );
    }
}

#[test]
fn disjoint_pair_crosses_word_boundary_and_excludes_removed_titles() {
    let mut titles = finite_titles(&[1, 1, 1]);
    titles[0].duration_secs = 0.0; // Filtered before the index is allocated.
    for marker in 0..64 {
        let mut title = finite_titles(&[3]).remove(0);
        title.selection_evidence.segments.push(ContentSegment {
            unit: format!("marker{marker:02}"),
            start: 0,
            end: 1,
        });
        titles.push(title);
    }
    titles.extend(finite_titles(&[2]));
    let model = SelectionModel::from_titles(&titles);
    let scores = model.scores(&StreamFilter::All, None);
    let mut initial = model.representatives((1..titles.len()).collect(), &scores);
    model.sort_play_order(&mut initial);
    let coverage: Vec<_> = model.titles.iter().map(Coverage::of).collect();
    let mut work = Work::default();
    let mut pairs = DisjointPairs::new(&initial, &coverage, &mut work);
    assert_eq!(pairs.rows.len(), 66); // Neither the short title nor the alias gets a row.
    assert_eq!(pairs.mask.len(), 2);
    assert_eq!(pairs.slots[&1], 0);
    assert_eq!(pairs.slots[&67], 65);
    assert!(pairs.has_pair(&[1, 67], &mut work));
    assert!(work.bitmap_words > 0);
    for inner in [&[1][..], &[67], &[], &[1, 3], &[3, 67]] {
        assert!(!pairs.has_pair(inner, &mut work), "{inner:?}");
    }
    assert!(play_all(
        3,
        &initial,
        &coverage,
        &model.titles,
        &mut pairs,
        &mut work
    ));
    initial.retain(|&index| index != 67);
    assert!(!play_all(
        3,
        &initial,
        &coverage,
        &model.titles,
        &mut pairs,
        &mut work
    ));
    let suggestion = model.suggest_episodes(&StreamFilter::All, None, 600.0);
    assert_eq!(suggestion.play_alls, (3..67).collect::<Vec<_>>());
    assert_eq!(suggestion.suggested, vec![1, 67]);
    assert_eq!(suggestion.alternates, vec![Alternate { index: 2, of: 1 }]);
}

fn one_segment(start: u64, end: u64) -> DiscTitle {
    let mut title = DiscTitle::empty();
    title.duration_secs = 1_000.0;
    title.selection_evidence.segments = vec![ContentSegment {
        unit: "one".into(),
        start,
        end,
    }];
    title
}

#[test]
fn an_empty_span_is_not_coverage() {
    let model = SelectionModel::from_titles(&[one_segment(5, 5)]);
    assert!(Coverage::of(&model.titles[0]).is_none());
}

#[test]
fn equal_coverage_does_not_strictly_contain() {
    let model =
        SelectionModel::from_titles(&[one_segment(0, 10), one_segment(0, 10), one_segment(0, 4)]);
    let mut work = Work::default();
    let (a, b, c) = (
        Coverage::of(&model.titles[0]).unwrap(),
        Coverage::of(&model.titles[1]).unwrap(),
        Coverage::of(&model.titles[2]).unwrap(),
    );
    assert!(!a.strictly_contains(&b, &mut work));
    assert!(a.strictly_contains(&c, &mut work));
    assert!(!c.strictly_contains(&a, &mut work));
    assert_eq!(work.shared_calls, 1, "only the heavier side is measured");
}
