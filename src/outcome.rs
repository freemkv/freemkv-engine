//! What a rip produced, and where key resolution landed — both as data.

/// Coarse damage tier for a finished or in-progress rip. Maps the observable
/// signals (bad sector count + lost wallclock playback time) onto a small
/// discrete classification so UIs can render a colored badge and operators can
/// decide whether to rescan / replug / accept. Produced by
/// [`crate::classify_damage`] — the freemkv product judgment, owned by the
/// engine (it moved here from libfreemkv in the engine split, so the strategy
/// and its severity type live together).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DamageSeverity {
    /// No bad sectors at all.
    Clean,
    /// 1–50 bad sectors AND under 1 s lost. Likely unnoticeable.
    Cosmetic,
    /// 51–499 sectors, OR 1 s up to (not including) 30 s lost. Visible
    /// artifacts possible.
    Moderate,
    /// 500 or more sectors, OR 30 s or more lost — or a loss that could not be
    /// quantified at all (`lost_ms` NaN), which fails safe to this tier because
    /// the abort-on-loss gate is refusing the same rip. Significant damage;
    /// consider a rescan or a different drive. Boundaries are owned by
    /// [`crate::classify_damage`], not restated here: they used to read
    /// "51–500"/"500+" (ambiguous at exactly 500), which disagreed with the
    /// code's actual answer for that case.
    Serious,
}

/// One output file a rip produced.
#[derive(Clone, Debug)]
pub struct RipFile {
    /// Absolute path written.
    pub path: std::path::PathBuf,
    /// Size in bytes.
    pub bytes: u64,
    /// Canonical title index this file came from.
    pub title_index: usize,
}

/// The result of a completed rip. Success, partial, and failure are all
/// expressed here rather than only through the `Result` — a partial rip that
/// wrote a usable MKV with some unreadable sectors is `Ok(Outcome { .. })` with
/// a non-`Clean` [`severity`](Outcome::severity), not an `Err`.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Files written (usually one per selected title).
    pub files: Vec<RipFile>,
    /// Total bytes the drive could never read (0 = perfect rip).
    pub unreadable_bytes: u64,
    /// Milliseconds of main-feature video affected by unreadable sectors.
    pub lost_ms: f64,
    /// Severity classification of the loss.
    pub severity: DamageSeverity,
    /// Wall-clock seconds the rip took.
    pub elapsed_secs: f64,
    /// Average throughput over the whole job, bytes/sec.
    pub avg_bps: u64,
}

impl Outcome {
    /// True when nothing was lost — the 100%-readable outcome freemkv exists to
    /// achieve.
    pub fn is_perfect(&self) -> bool {
        self.unreadable_bytes == 0
    }
}

/// Key resolution state, as data rather than log lines — so a UI can render the
/// "keydb: N entries" strip and grey out Start with a real reason instead of
/// scraping a log. Populated by [`crate::resolve_keys`].
#[derive(Clone, Debug)]
pub struct KeyStatus {
    /// Whether usable decryption keys were resolved for the selected content.
    pub resolved: bool,
    /// Where the keys came from, if resolved (keydb / derived / external / CSS / …).
    pub origin: Option<libfreemkv::KeyOrigin>,
    /// Number of entries in the loaded keydb, if one was found.
    pub keydb_entries: Option<usize>,
    /// A stable, front-end-localizable summary key describing the state
    /// (e.g. `"no-keydb"`, `"resolved-keydb"`, `"resolved-external"`). Never a
    /// pre-rendered English sentence — front-ends map it.
    pub summary: String,
}

impl KeyStatus {
    /// The "no keys, cannot decrypt" state, with the standard summary key.
    pub fn unresolved(summary: impl Into<String>) -> Self {
        KeyStatus {
            resolved: false,
            origin: None,
            keydb_entries: None,
            summary: summary.into(),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
