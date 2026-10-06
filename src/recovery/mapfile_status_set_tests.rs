use super::*;

// The two sets differ by exactly `NonTried`, and neither may ever contain
// `Finished`. Both used to be hand-written arrays scattered across five
// call sites in three files — in different orders — this pins the relation.
#[test]
fn damage_set_is_the_bad_set_without_the_unread_remainder() {
    let bad = bad_sector_statuses();
    let damage = damage_sector_statuses();

    for s in damage {
        assert!(bad.contains(&s), "{s:?} is damage but not bad");
    }
    for s in bad {
        assert!(
            damage.contains(&s) || s == SectorStatus::NonTried,
            "{s:?} is bad but not damage, and is not the unread remainder"
        );
    }
    assert!(bad.contains(&SectorStatus::NonTried));
    assert!(!damage.contains(&SectorStatus::NonTried));
    for s in bad {
        assert!(!s.is_finished(), "{s:?} must not count as good");
    }
}

/// `is_finished` is the single arbiter of "confirmed good"; every other
/// status must disagree with it.
#[test]
fn only_finished_is_finished() {
    assert!(SectorStatus::Finished.is_finished());
    for s in bad_sector_statuses() {
        assert!(!s.is_finished());
    }
}
