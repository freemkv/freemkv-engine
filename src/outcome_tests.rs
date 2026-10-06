use super::*;

#[test]
fn perfect_outcome_detects_zero_loss() {
    let o = Outcome {
        files: vec![RipFile {
            path: "/out/A1_t00.mkv".into(),
            bytes: 5_800_000_000,
            title_index: 0,
        }],
        unreadable_bytes: 0,
        lost_ms: 0.0,
        severity: DamageSeverity::Clean,
        elapsed_secs: 512.0,
        avg_bps: 11_300_000,
    };
    assert!(o.is_perfect());
    assert_eq!(o.files.len(), 1);
}

#[test]
fn partial_outcome_is_not_perfect() {
    let o = Outcome {
        files: vec![],
        unreadable_bytes: 4096,
        lost_ms: 40.0,
        severity: DamageSeverity::Clean,
        elapsed_secs: 1.0,
        avg_bps: 0,
    };
    assert!(!o.is_perfect());
}

#[test]
fn unresolved_key_status_carries_a_summary_key_not_prose() {
    let k = KeyStatus::unresolved("no-keydb");
    assert!(!k.resolved);
    assert!(k.origin.is_none());
    assert_eq!(k.summary, "no-keydb");
}
