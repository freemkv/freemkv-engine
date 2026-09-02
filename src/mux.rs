//! ISO/disc → MKV muxing and the multi-title rip loop.
//!
//! Resolves which titles to rip, muxes each through `libfreemkv::mux_stream`,
//! and decides when a failure is fatal vs skippable. Three load-bearing
//! behaviours: fail-fast on a disc-level key failure (every title would fail
//! identically), cancel is a full stop (not a per-title cancel), and a
//! main-title default (via [`Selection`]) so an obfuscated disc doesn't rip
//! everything by accident.
//!
//! See docs/mux.md — module history and the 2026-07-28 rationale.

use crate::job::Selection;
use crate::sink::{Level, Sink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Human-readable byte count for a diagnostic line — GB/MB/KB, so a rip size
/// reads "~51.6 GB" instead of "~55460235264 bytes".
fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K {
        format!("{:.1} GB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.0} MB", f / (K * K))
    } else if f >= K {
        format!("{:.0} KB", f / K)
    } else {
        format!("{b} B")
    }
}

/// Resolve a [`Selection`] to concrete 0-based title indices against a scanned
/// disc. Out-of-range explicit indices are dropped here (preflight surfaces
/// them as blocking reasons before we get here). `MainMovie` is title 0 (the
/// canonical main feature — first in every freemkv title list). `Longest` is
/// the max-duration title (may differ from the canonical feature on odd
/// authoring).
pub fn resolve_selection(disc: &libfreemkv::Disc, sel: &Selection) -> Vec<usize> {
    let n = disc.titles.len();
    match sel {
        Selection::MainMovie => {
            if n == 0 {
                vec![]
            } else {
                vec![0]
            }
        }
        Selection::All => (0..n).collect(),
        // FIRST of the equal maxima, not the last: `Iterator::max_by` keeps the
        // LAST tied element, but playlist obfuscation authors decoys with the
        // SAME runtime as the feature, which is conventionally the lowest index.
        Selection::Longest => disc
            .titles
            .iter()
            .enumerate()
            // Drop non-finite durations BEFORE folding: rejecting only inside
            // the comparison isn't enough, since a NaN in the accumulator is
            // never displaced (`NaN > d` and `t > NaN` are both false).
            .filter(|(_, t)| t.duration_secs.is_finite())
            // `<=` is safe only because the filter above already removed every
            // non-finite duration; without it the two forms would differ.
            .fold(None::<(usize, f64)>, |best, (i, t)| match best {
                Some((_, d)) if t.duration_secs <= d => best,
                _ => Some((i, t.duration_secs)),
            })
            .map(|(i, _)| vec![i])
            .unwrap_or_default(),
        Selection::Titles(indices) => {
            // Range-filter AND de-duplicate. A repeated index (a UI adding the
            // same title twice, or `-t 1 -t 1`) would mux it twice, inflating
            // `titles_written` past what's on disk and mis-flipping `multi_title`.
            let mut seen = std::collections::HashSet::new();
            indices
                .iter()
                .copied()
                .filter(|&i| i < n)
                .filter(|&i| seen.insert(i))
                .collect()
        }
    }
}

/// The result of muxing one title, from the loop's point of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleResult {
    /// Muxed successfully.
    Ok,
    /// A DISC-LEVEL key failure — the whole disc can't be decrypted, so every
    /// remaining title would fail identically. The loop stops immediately
    /// (fail-fast) instead of iterating and re-printing the same error.
    DiscLevelNoKey,
    /// Failed, but skippably — an uncrackable/empty per-title stub (a menu
    /// loop, an FBI-warning nav title). Skipped on a non-feature title in a
    /// multi-title rip; fatal if it's the feature or an explicit selection.
    SkippableStub,
    /// A hard failure for this title (not a stub).
    Failed,
    /// The rip was cancelled (halt) during this title.
    Halted,
}

/// Classify the `io::Error` a single-title mux returned into a [`TitleResult`].
/// Uses libfreemkv's typed classifiers — never string-matches E-codes. Order
/// matters: halt first (a user stop wins), then disc-level no-key (fail-fast),
/// then the per-title skippable stub, else a hard failure.
pub fn classify_title_error(e: &std::io::Error) -> TitleResult {
    if libfreemkv::is_halt(e) {
        TitleResult::Halted
    } else if libfreemkv::is_disc_level_no_key(e) {
        TitleResult::DiscLevelNoKey
    } else if libfreemkv::is_skippable_title_stub(e) {
        TitleResult::SkippableStub
    } else {
        TitleResult::Failed
    }
}

/// What the loop should DO about one title's [`TitleResult`]. The single
/// source of the multi-title loop policy — [`run_titles`] uses it, and a
/// front-end with its own loop + error rendering (the CLI) calls it directly so
/// the policy is never duplicated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleAction {
    /// The title muxed (or should be treated as done); move on.
    Continue,
    /// A skippable stub on a non-feature title in a multi-title, non-explicit
    /// rip — skip it with a notice and keep going.
    Skip,
    /// Cancelled — stop the WHOLE rip (full stop, not per-title).
    StopHalt,
    /// Disc-level key failure — stop; every remaining title fails identically.
    StopNoKey,
    /// A hard failure on a title the user wanted — stop and surface it.
    StopFatal,
}

/// Decide what to do about one title's result. `is_feature` = title index 0;
/// `multi_title` = more than one title selected; `explicit_selection` = the
/// user named specific titles. This is THE loop policy, shared by [`run_titles`]
/// and any front-end that drives its own loop.
pub fn decide_title(
    result: &TitleResult,
    is_feature: bool,
    multi_title: bool,
    explicit_selection: bool,
) -> TitleAction {
    match result {
        TitleResult::Ok => TitleAction::Continue,
        TitleResult::Halted => TitleAction::StopHalt,
        TitleResult::DiscLevelNoKey => TitleAction::StopNoKey,
        TitleResult::SkippableStub if !is_feature && multi_title && !explicit_selection => {
            TitleAction::Skip
        }
        TitleResult::SkippableStub | TitleResult::Failed => TitleAction::StopFatal,
    }
}

/// The terminal outcome of a whole multi-title rip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RipOutcome {
    /// Every selected title that mattered succeeded (skippable stubs may have
    /// been skipped). Carries the count actually written.
    ///
    /// `titles_written: 0` is NOT a rip: no file was produced, either because
    /// the selection was empty or because every title in it was a skippable
    /// stub. It is still `Ok` — nothing failed — but a front-end must branch on
    /// the count before it reports success to the user. `run_titles` logs an
    /// Error line naming which of the two happened.
    Ok { titles_written: usize },
    /// A disc-level key failure surfaced — the whole disc can't be decrypted,
    /// so the loop stopped (fail-fast) rather than iterate every title.
    NoKey,
    /// A title the user wanted (the feature, or an explicit `-t`) failed hard.
    ///
    /// Carries both `code` and `kind` because libfreemkv reports a failure's
    /// cause two different ways depending on its origin — see docs/mux.md for
    /// why one field alone would blind a front-end to half the failures.
    Failed {
        title_index: usize,
        /// libfreemkv's numeric code, when the error carried one.
        code: Option<u16>,
        /// The `io::ErrorKind`, which is where a passthrough OS error
        /// (`StorageFull`, `PermissionDenied`) keeps its meaning.
        kind: std::io::ErrorKind,
    },
    /// The rip was cancelled — a full stop, not a per-title cancel.
    Halted,
}

/// Drive the multi-title rip loop. `mux_one(idx) -> io::Result<()>` muxes a
/// single title; injecting it keeps the loop's control flow unit-testable
/// without a real ISO. Production passes [`mux_title`] (or the consumer's
/// own single-title mux). Self-contained — no `Disc` needed.
///
/// Fails fast on a disc-level key error, treats cancel as a full stop, and
/// skips skippable stubs only on a non-feature title in a multi-title,
/// non-explicit rip (fatal otherwise). `explicit_selection` is `true` when
/// the user named specific titles. See docs/mux.md for the full rationale.
pub fn run_titles<F>(
    indices: &[usize],
    explicit_selection: bool,
    sink: &dyn Sink,
    mut mux_one: F,
) -> RipOutcome
where
    F: FnMut(usize) -> std::io::Result<()>,
{
    let multi_title = indices.len() > 1;
    let mut titles_written = 0usize;

    for &idx in indices {
        // (2) Poll for a full-stop between titles.
        if sink.should_cancel() {
            sink.log(Level::Info, "cancelled — stopping the whole rip");
            return RipOutcome::Halted;
        }

        let is_feature = idx == 0;
        // Keep the rendered cause alive past the classification: the error
        // itself is consumed by `classify_title_error` and was then dropped,
        // taking the only description of WHY the title failed with it.
        let mut fail_detail = String::new();
        // The code has to be taken here too: `classify_title_error` consumes
        // the error into a coarse verdict, and the typed cause is gone after.
        let mut fail_code = None;
        let mut fail_kind = std::io::ErrorKind::Other;
        let result = match mux_one(idx) {
            Ok(()) => TitleResult::Ok,
            Err(e) => {
                fail_detail = e.to_string();
                fail_code = libfreemkv::error_code(&e);
                fail_kind = e.kind();
                classify_title_error(&e)
            }
        };

        match decide_title(&result, is_feature, multi_title, explicit_selection) {
            TitleAction::Continue => titles_written += 1,
            TitleAction::Skip => {
                sink.log(
                    Level::Info,
                    &format!("title {} skipped (empty/uncrackable stub)", idx + 1),
                );
            }
            TitleAction::StopHalt => {
                sink.log(Level::Info, "cancelled — stopping the whole rip");
                return RipOutcome::Halted;
            }
            TitleAction::StopNoKey => {
                sink.log(
                    Level::Error,
                    "disc has no decryption key — every title would fail; stopping",
                );
                return RipOutcome::NoKey;
            }
            TitleAction::StopFatal => {
                // Unlike every other arm here, this one used to return silently,
                // so a hard failure (disk full, permission denied) reached the
                // front-end as a bare title index with no diagnostic anywhere.
                sink.log(
                    Level::Error,
                    &format!("title {} failed — stopping the rip: {fail_detail}", idx + 1),
                );
                return RipOutcome::Failed {
                    title_index: idx,
                    code: fail_code,
                    kind: fail_kind,
                };
            }
        }
    }

    // A rip that wrote NOTHING must not return silently: an empty `indices` or
    // an all-skippable-stub selection used to surface as exit-0 success with no
    // explanation. `Ok` stays the variant (matched exhaustively elsewhere).
    if titles_written == 0 {
        sink.log(
            Level::Error,
            if indices.is_empty() {
                "no titles were selected — nothing was written"
            } else {
                "every selected title was skipped (empty/uncrackable stub) — \
                 nothing was written"
            },
        );
    }
    RipOutcome::Ok { titles_written }
}

/// Mux a single title from a source URL to `dest`, driving
/// `libfreemkv::mux_stream` and reporting through the engine [`Sink`].
///
/// Bridges the two libfreemkv seams onto the Sink:
/// - `MuxEvents` write-progress → `Sink::progress` (via a channel + a scoped
///   watcher thread, because `mux_stream` takes an `Arc<dyn MuxEvents + 'static>`
///   that cannot borrow the `&dyn Sink` directly).
/// - `Sink::should_cancel()` → the `Halt` token the mux polls (the watcher sets
///   it), so a UI Cancel / Ctrl-C stops the pump exactly as today.
pub fn mux_title(
    source_url: &str,
    dest: &str,
    input_opts: libfreemkv::InputOptions,
    mux_opts: &libfreemkv::MuxOptions,
    total_bytes_hint: u64,
    sink: &dyn Sink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    let input = libfreemkv::MuxInput::Url {
        url: source_url,
        opts: input_opts,
    };
    mux_with_input(input, source_url, dest, mux_opts, total_bytes_hint, sink)
}

/// Mux a single title live off an opened, scanned, key-resolved
/// [`libfreemkv::DiscSession`] (the drive's staged reader), driving
/// `libfreemkv::mux_stream` and reporting through the engine [`Sink`] —
/// the disc:// analogue of [`mux_title`]. Shares the exact same
/// watcher/speed/halt/done scaffolding via [`mux_with_input`], so a live-drive
/// rip gets the same speed/ETA reporting a file/ISO rip does.
pub fn mux_title_session(
    session: &mut libfreemkv::DiscSession,
    title_index: usize,
    dest: &str,
    mux_opts: &libfreemkv::MuxOptions,
    total_bytes_hint: u64,
    sink: &dyn Sink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    let source_label = format!("disc title {}", title_index + 1);
    let input = libfreemkv::MuxInput::Session {
        session,
        title_index,
    };
    mux_with_input(input, &source_label, dest, mux_opts, total_bytes_hint, sink)
}

// Shared scaffolding behind `mux_title` and `mux_title_session`: drives
// `mux_stream` for an already-built `MuxInput`, bridging progress and cancel
// onto the Sink via `with_mux_watcher` (see its doc for the mechanism).
fn mux_with_input(
    input: libfreemkv::MuxInput<'_>,
    source_label: &str,
    dest: &str,
    mux_opts: &libfreemkv::MuxOptions,
    total_bytes_hint: u64,
    sink: &dyn Sink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    with_mux_watcher(sink, |halt, events| {
        sink.log(
            Level::Info,
            &format!(
                "mux: {source_label} -> {dest} (~{})",
                human_bytes(total_bytes_hint)
            ),
        );
        libfreemkv::mux_stream(input, dest, mux_opts, halt, events)
    })
}

// The Sink↔libfreemkv bridge every mux runs inside, lifted out of
// `mux_with_input` so it's testable against a closure without real media.
// See docs/mux.md for why it's split out and what it's guarding against.
fn with_mux_watcher<T>(
    sink: &dyn Sink,
    f: impl FnOnce(&libfreemkv::Halt, Arc<dyn libfreemkv::MuxEvents>) -> T,
) -> T {
    use std::sync::mpsc;

    let halt = libfreemkv::Halt::new();
    // Ask ONCE before starting (same rule as `with_cancel_watcher` in run.rs):
    // a watcher alone makes cancellation a race the work can win on a short
    // title, and only asking before the work begins closes that window.
    if sink.should_cancel() {
        halt.cancel();
    }
    let (tx, rx) = mpsc::channel::<(u64, u64)>();

    // MuxEvents impl that forwards write-progress over the channel. Owned +
    // 'static (holds only a Sender), so it satisfies mux_stream's Arc bound.
    struct ChannelEvents {
        tx: mpsc::Sender<(u64, u64)>,
    }
    impl libfreemkv::MuxEvents for ChannelEvents {
        fn on_write_progress(&self, bytes_written: u64, bytes_total: u64) {
            let _ = self.tx.send((bytes_written, bytes_total));
        }
    }

    let done = Arc::new(AtomicBool::new(false));

    std::thread::scope(|s| {
        // Watcher: drains progress → sink, mirrors should_cancel → halt. `move`
        // captures the `!Sync` Receiver; `sink` is a borrowed ref tied to the
        // scope; `done` is a shared Arc the main thread sets when mux returns.
        let watcher_halt = halt.clone();
        let watcher_done = done.clone();
        s.spawn(move || {
            // The engine's ONE speed/ETA derivation for the mux stage. Owned by
            // this single watcher thread, so a plain `mut` — no lock needed.
            let mut speed = crate::speed::SpeedEstimator::new();
            loop {
                // Coalesce queued progress ticks to the LATEST, sample ONCE:
                // sampling per-message would measure `dt` in microseconds
                // against a ~100ms byte-delta, yielding absurd multi-GB/s speeds.
                let mut latest = None;
                while let Ok(m) = rx.try_recv() {
                    latest = Some(m);
                }
                if let Some((done_b, total_b)) = latest {
                    let (speed_bps, eta_secs) = speed.sample(done_b, total_b);
                    let p = crate::sink::Progress {
                        pass: std::borrow::Cow::Borrowed("mux"),
                        bytes_done: done_b,
                        bytes_total: total_b,
                        sectors_bad: 0,
                        speed_bps,
                        eta_secs,
                    };
                    sink.progress(&p);
                }
                if sink.should_cancel() {
                    watcher_halt.cancel();
                }
                if watcher_done.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        let events: Arc<dyn libfreemkv::MuxEvents> = Arc::new(ChannelEvents { tx });
        // Same guard the recovery paths use: `mux_stream` runs on damaged media
        // and can panic; storing `done` after the call would let an unwind skip
        // it, leaving thread::scope joining a watcher that loops forever.
        let _signal_done = crate::run::SignalDone(&done);
        f(&halt, events)
    })
}

// Lifted out of `open_scan_resolve`'s struct literal so the one field that
// matters (`credentials`, the sole input to the SCSI AACS handshake) is
// unit-testable — dropped, every caller silently authenticates as no-one.
fn build_keyspec(credentials: Option<libfreemkv::DriveCredentials>) -> libfreemkv::KeySpec {
    libfreemkv::KeySpec {
        credentials,
        ..Default::default()
    }
}

/// Open a live optical drive and get it ready to rip: open the session, lock
/// the tray, scan the disc, and resolve its AACS keys. Returns the scanned
/// session (its `disc()` is populated and its drive is still owned, ready to
/// be staged for a `MuxInput::Session` mux) plus the resolution trace.
///
/// The ONE drive-bring-up sequence shared by the CLI's `pipe_disc` and the
/// desktop GUI's disc:// path; `factory` and `credentials` are supplied by
/// the caller so a shell can log key attempts or stay quiet. `disc_to_iso`
/// uses a different, lower-level `Drive` API and isn't covered here.
pub fn open_scan_resolve(
    target: libfreemkv::DeviceTarget,
    credentials: Option<libfreemkv::DriveCredentials>,
    factory: libfreemkv::KeySourceFactory,
) -> Result<
    (
        libfreemkv::DiscSession,
        libfreemkv::aacs::trace::ResolutionTrace,
    ),
    libfreemkv::Error,
> {
    let mut session = libfreemkv::DiscSession::open(target, build_keyspec(credentials))?;
    // Lock the tray so the disc can't eject mid-rip; Drive::drop unlocks it.
    session.lock_tray();
    session.scan(libfreemkv::ScanOptions::default())?;
    let trace = session.resolve_keys(factory)?;
    Ok((session, trace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::NoopSink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn disc(n: usize, encrypted: bool, has_key: bool) -> libfreemkv::Disc {
        let titles = (0..n)
            .map(|i| {
                let mut t = libfreemkv::DiscTitle::empty();
                t.duration_secs = (i as f64 + 1.0) * 60.0; // title i longer than i-1
                t
            })
            .collect();
        libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles,
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: if has_key {
                Some(libfreemkv::AacsState {
                    version: 1,
                    bus_encryption: false,
                    mkb_version: None,
                    disc_hash: String::new(),
                    key_source: libfreemkv::KeyOrigin::KeyDb,
                    vuk: None,
                    unit_keys: vec![(0, [0u8; 16])],
                    read_data_key: None,
                    volume_id: [0u8; 16],
                    uk_ro: Vec::new(),
                    mkb: Vec::new(),
                })
            } else {
                None
            },
            css: None,
            encrypted,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    fn stub_err() -> std::io::Error {
        // E_MKV_INVALID is a skippable stub per is_skippable_title_stub.
        libfreemkv::Error::MkvInvalid.into()
    }
    fn hard_err() -> std::io::Error {
        libfreemkv::Error::IoError {
            source: std::io::Error::other("boom"),
        }
        .into()
    }

    #[test]
    fn selection_main_movie_is_title_zero() {
        let d = disc(5, false, false);
        assert_eq!(resolve_selection(&d, &Selection::MainMovie), vec![0]);
    }

    #[test]
    fn selection_all_is_every_title() {
        let d = disc(3, false, false);
        assert_eq!(resolve_selection(&d, &Selection::All), vec![0, 1, 2]);
    }

    #[test]
    fn selection_longest_picks_max_duration() {
        let d = disc(4, false, false); // durations 60,120,180,240 → index 3
        assert_eq!(resolve_selection(&d, &Selection::Longest), vec![3]);
    }

    // Ties go to the FIRST title, not the last (`max_by` would pick the LAST
    // of equal maxima, i.e. a decoy). See docs/mux.md for why.
    #[test]
    fn selection_longest_breaks_a_tie_towards_the_first_title() {
        let mut d = disc(5, false, false);
        // Three playlists at the same, longest runtime; index 1 is the real one.
        d.titles[1].duration_secs = 7200.0;
        d.titles[3].duration_secs = 7200.0;
        d.titles[4].duration_secs = 7200.0;
        assert_eq!(
            resolve_selection(&d, &Selection::Longest),
            vec![1],
            "the first of the equal-longest playlists is the feature; the later \
             ones are decoys"
        );
    }

    // A non-finite duration must never win, INCLUDING as the first title
    // (`t > NaN` is false for every `t`, so a leading NaN is never
    // displaced). See docs/mux.md.
    #[test]
    fn selection_longest_ignores_a_leading_title_with_no_measurable_duration() {
        let mut d = disc(3, false, false); // 60, 120, 180
        d.titles[0].duration_secs = f64::NAN;
        assert_eq!(
            resolve_selection(&d, &Selection::Longest),
            vec![2],
            "a leading NaN must not win the longest-title selection"
        );

        // Every duration unusable: no title is selectable, so select nothing
        // rather than defaulting to index 0 and ripping an arbitrary playlist.
        for t in d.titles.iter_mut() {
            t.duration_secs = f64::NAN;
        }
        assert!(resolve_selection(&d, &Selection::Longest).is_empty());
    }

    #[test]
    fn selection_longest_ignores_a_title_with_no_measurable_duration() {
        let mut d = disc(3, false, false); // 60, 120, 180
        d.titles[2].duration_secs = f64::NAN;
        assert_eq!(
            resolve_selection(&d, &Selection::Longest),
            vec![1],
            "an unmeasurable title is not the longest one"
        );
    }

    #[test]
    fn selection_longest_on_an_empty_disc_selects_nothing() {
        let d = disc(0, false, false);
        assert_eq!(
            resolve_selection(&d, &Selection::Longest),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn selection_explicit_drops_out_of_range() {
        let d = disc(2, false, false);
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![0, 9])),
            vec![0]
        );
    }

    // The range filter is `i < n`, and `n` itself is out of range — pins the
    // boundary that `<` vs `<=` off-by-one bugs hide behind. See docs/mux.md.
    #[test]
    fn selection_explicit_index_equal_to_the_title_count_is_out_of_range() {
        let d = disc(3, false, false); // valid indices are 0, 1, 2
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![3])),
            Vec::<usize>::new(),
            "index == title count is one past the end, not a fourth title"
        );
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![2, 3])),
            vec![2],
            "the last valid index still survives the same filter"
        );
    }

    // A lone selected title is NOT a multi-title rip: `multi_title =
    // indices.len() > 1` guards against silently swallowing a single-title
    // stub as a "successful" empty rip. See docs/mux.md.
    #[test]
    fn a_single_non_explicit_title_stub_is_fatal_not_skipped() {
        let d = disc(4, false, false); // durations 60..240 → longest is index 3
        let indices = resolve_selection(&d, &Selection::Longest);
        assert_eq!(indices, vec![3], "one title, and not the feature");

        let outcome = run_titles(&indices, false, &NoopSink, |_| Err(stub_err()));
        assert_eq!(
            outcome,
            RipOutcome::Failed {
                title_index: 3,
                code: libfreemkv::error_code(&stub_err()),
                kind: stub_err().kind(),
            },
            "the only title the rip was going to write came back a stub — that \
             is a failed rip, not a rip that skipped a bonus feature"
        );
    }

    /// `open_scan_resolve` opens a real drive, so the only testable part of it
    /// is the spec it opens with.
    #[test]
    fn build_keyspec_forwards_the_caller_credentials() {
        assert!(
            build_keyspec(None).credentials.is_none(),
            "no credentials in, no credentials out"
        );
        let creds = libfreemkv::DriveCredentials {
            host_certs: Vec::new(),
        };
        assert!(
            build_keyspec(Some(creds)).credentials.is_some(),
            "the host certs the shell supplied are the only input to the AACS \
             handshake — dropping them authenticates as no-one"
        );
    }

    /// Wait for `cond` to hold, up to `secs`. Returns whether it held — a
    /// bounded wait, so a bridge that never fires fails the test instead of
    /// hanging the suite forever.
    fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cond()
    }

    /// A sink that records every progress tick it is handed and never cancels.
    #[derive(Default)]
    struct RecordingSink {
        ticks: std::sync::Mutex<Vec<crate::sink::Progress>>,
    }
    impl Sink for RecordingSink {
        fn progress(&self, p: &crate::sink::Progress) {
            self.ticks.lock().unwrap().push(p.clone());
        }
    }

    // The mux must not start work it has ALREADY been told to stop; the sink
    // here answers `true` exactly ONCE (the pre-check), `false` after, so
    // only the pre-check can cancel the token. See docs/mux.md.
    #[test]
    fn an_already_cancelled_sink_halts_the_mux_before_it_starts() {
        struct CancelledOnce {
            asked: AtomicUsize,
        }
        impl Sink for CancelledOnce {
            fn should_cancel(&self) -> bool {
                self.asked.fetch_add(1, Ordering::SeqCst) == 0
            }
        }
        let sink = CancelledOnce {
            asked: AtomicUsize::new(0),
        };
        let halted_on_entry = with_mux_watcher(&sink, |halt, _events| halt.is_cancelled());
        assert!(
            halted_on_entry,
            "a rip that was cancelled before it began must reach the muxer \
             already halted, not run to completion"
        );
    }

    // A Stop pressed once the mux is under way must reach the muxer via the
    // watcher's poll, not the pre-check (the sink only starts cancelling
    // AFTER the mux begins). See docs/mux.md.
    #[test]
    fn a_cancel_during_the_mux_reaches_the_halt_token() {
        struct CancelOnceStarted {
            started: AtomicBool,
        }
        impl Sink for CancelOnceStarted {
            fn should_cancel(&self) -> bool {
                self.started.load(Ordering::SeqCst)
            }
        }
        let sink = CancelOnceStarted {
            started: AtomicBool::new(false),
        };
        let saw_halt = with_mux_watcher(&sink, |halt, _events| {
            sink.started.store(true, Ordering::SeqCst);
            wait_for(5, || halt.is_cancelled())
        });
        assert!(
            saw_halt,
            "the watcher must mirror should_cancel onto the halt token the mux \
             polls; otherwise Stop does nothing until the mux finishes on its own"
        );
    }

    // Write-progress from the muxer must arrive at the sink as a `mux` tick
    // via the channel + watcher drain bridge. See docs/mux.md.
    #[test]
    fn write_progress_reaches_the_sink_as_a_mux_progress_tick() {
        let sink = RecordingSink::default();
        with_mux_watcher(&sink, |_halt, events| {
            events.on_write_progress(4096, 8192);
            assert!(
                wait_for(5, || !sink.ticks.lock().unwrap().is_empty()),
                "no progress tick reached the sink"
            );
        });
        let ticks = sink.ticks.lock().unwrap();
        let p = ticks.first().expect("a tick was recorded");
        assert_eq!(p.pass, "mux", "the mux stage must name itself");
        assert_eq!(p.bytes_done, 4096);
        assert_eq!(p.bytes_total, 8192);
    }

    // A panic inside the mux must still release the watcher, or an unwind
    // skips storing `done` and the watcher loops forever — a hang, not a
    // failure. Bounded here for that reason. See docs/mux.md.
    #[test]
    fn a_panicking_mux_still_releases_the_watcher() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let sink = NoopSink;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_mux_watcher(&sink, |_halt, _events| panic!("mux blew up"))
            }));
            assert!(r.is_err(), "the panic must still propagate to the caller");
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
            "the scope never joined: a panicking mux left the watcher looping, \
             so the rip hangs instead of reporting the failure"
        );
    }

    /// The size hint in the mux log line is a byte count rendered for humans;
    /// each threshold is pinned so a unit never shifts by 1024×.
    #[test]
    fn human_bytes_picks_the_unit_at_each_threshold() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1024 * 1024), "1 MB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
        // The user's ~51.6 GB rip: GB, one decimal, not 55460235264 B.
        assert_eq!(human_bytes(55_460_235_264), "51.7 GB");
    }

    fn disc_no_key_err() -> std::io::Error {
        // E_NO_DISC_KEY — a disc-level key failure (keydb has no entry).
        libfreemkv::Error::NoDiscKey {
            disc_hash: "deadbeef".to_string(),
        }
        .into()
    }

    #[test]
    fn fail_fast_on_disc_level_no_key_stops_after_first_title() {
        // The user's scenario: 54 titles, no key. The FIRST title's mux returns
        // a disc-level no-key error → stop; do NOT iterate the other 53.
        let d = disc(54, true, false);
        let indices = resolve_selection(&d, &Selection::All);
        let calls = AtomicUsize::new(0);
        let outcome = run_titles(&indices, false, &NoopSink, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(disc_no_key_err())
        });
        assert_eq!(outcome, RipOutcome::NoKey);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "must stop after the first no-key title, not iterate all 54"
        );
    }

    #[test]
    fn halt_from_mux_is_a_full_stop_not_per_title() {
        // First title halts → the loop stops immediately, does NOT visit title 2.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let visited = AtomicUsize::new(0);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            visited.fetch_add(1, Ordering::Relaxed);
            if idx == 0 {
                Err(libfreemkv::Error::Halted.into())
            } else {
                Ok(())
            }
        });
        assert_eq!(outcome, RipOutcome::Halted);
        assert_eq!(
            visited.load(Ordering::Relaxed),
            1,
            "halt on title 0 must stop before title 1"
        );
    }

    // A rip that writes NO title must say so, whatever emptied it (empty
    // `indices`, or every title a skippable stub) — the outcome stays `Ok`
    // but must not be SILENT. See docs/mux.md.
    #[test]
    fn a_rip_that_writes_nothing_says_so_rather_than_returning_a_silent_ok() {
        #[derive(Default)]
        struct LogSink {
            errors: std::sync::Mutex<Vec<String>>,
        }
        impl Sink for LogSink {
            fn log(&self, level: Level, msg: &str) {
                if matches!(level, Level::Error) {
                    self.errors.lock().unwrap().push(msg.to_string());
                }
            }
        }

        // (a) No titles selected at all.
        let sink = LogSink::default();
        let outcome = run_titles(&[], false, &sink, |_| Ok(()));
        assert_eq!(outcome, RipOutcome::Ok { titles_written: 0 });
        assert_eq!(
            sink.errors.lock().unwrap().len(),
            1,
            "an empty selection wrote nothing and must not pass for a rip"
        );

        // (b) A non-empty selection whose every title is a skippable stub.
        let d = disc(3, false, false);
        let sink = LogSink::default();
        let outcome = run_titles(&[1, 2], false, &sink, |_| Err(stub_err()));
        let _ = &d;
        assert_eq!(
            outcome,
            RipOutcome::Ok { titles_written: 0 },
            "skipping stubs is not a failure — but it is not a written title \
             either"
        );
        assert_eq!(
            sink.errors.lock().unwrap().len(),
            1,
            "every selected title was skipped: nothing was written, and the \
             user has to be told"
        );
    }

    #[test]
    fn skippable_stub_on_non_feature_multi_title_is_skipped() {
        // All-titles rip (explicit=false): title 0 ok, title 1 a stub (skipped),
        // title 2 ok.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            if idx == 1 { Err(stub_err()) } else { Ok(()) }
        });
        assert_eq!(outcome, RipOutcome::Ok { titles_written: 2 });
    }

    // The cause on `Failed` must actually discriminate: three failures, three
    // distinguishable causes, each legible through the field that carries
    // its meaning (typed vs passthrough OS error). See docs/mux.md.
    #[test]
    fn the_failure_cause_says_which_failure_it_was() {
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);

        let run = |e: fn() -> std::io::Error| {
            run_titles(&indices, false, &NoopSink, move |idx| {
                if idx == 0 { Err(e()) } else { Ok(()) }
            })
        };
        let cause = |o: &RipOutcome| match o {
            RipOutcome::Failed { code, kind, .. } => (*code, *kind),
            other => panic!("expected Failed, got {other:?}"),
        };

        // Typed failure: code present, kind uninformative.
        let stub = cause(&run(stub_err));
        assert_eq!(stub.0, Some(libfreemkv::error::E_MKV_INVALID));

        // Passthrough OS failure: the disk filled mid-write. No E-code exists
        // for it — `From<Error> for io::Error` hands the OS error straight
        // back — so `kind` is the only channel carrying the reason.
        let full = cause(&run(|| {
            libfreemkv::Error::IoError {
                source: std::io::Error::from(std::io::ErrorKind::StorageFull),
            }
            .into()
        }));
        assert_eq!(full.0, None, "a passthrough OS error carries no E-code");
        assert_eq!(
            full.1,
            std::io::ErrorKind::StorageFull,
            "a full disk must stay legible as a full disk"
        );

        assert_ne!(
            stub, full,
            "a cause that cannot tell a malformed title from a full disk \
             carries no information"
        );
    }

    #[test]
    fn stub_on_the_feature_is_fatal() {
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            if idx == 0 { Err(stub_err()) } else { Ok(()) }
        });
        assert_eq!(
            outcome,
            RipOutcome::Failed {
                title_index: 0,
                code: libfreemkv::error_code(&stub_err()),
                kind: stub_err().kind(),
            }
        );
    }

    #[test]
    fn explicit_single_title_stub_is_fatal() {
        // `-t 2` explicitly (explicit=true): a stub there is what the user asked
        // for → fatal.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::Titles(vec![1]));
        let outcome = run_titles(&indices, true, &NoopSink, |_| Err(stub_err()));
        assert_eq!(
            outcome,
            RipOutcome::Failed {
                title_index: 1,
                code: libfreemkv::error_code(&stub_err()),
                kind: stub_err().kind(),
            }
        );
    }

    #[test]
    fn hard_failure_on_non_feature_is_still_fatal() {
        // A non-stub hard error is never skippable, even on a bonus title.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            if idx == 1 { Err(hard_err()) } else { Ok(()) }
        });
        assert_eq!(
            outcome,
            RipOutcome::Failed {
                title_index: 1,
                code: libfreemkv::error_code(&hard_err()),
                kind: hard_err().kind(),
            }
        );
    }

    #[test]
    fn should_cancel_between_titles_stops_the_rip() {
        struct CancelAfterFirst {
            seen: AtomicUsize,
        }
        impl Sink for CancelAfterFirst {
            fn should_cancel(&self) -> bool {
                // Cancel once the first title has been muxed.
                self.seen.load(Ordering::Relaxed) >= 1
            }
        }
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let sink = CancelAfterFirst {
            seen: AtomicUsize::new(0),
        };
        let outcome = run_titles(&indices, false, &sink, |_| {
            sink.seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(outcome, RipOutcome::Halted);
    }

    // A repeated `-t` index must produce ONE entry, or `titles_written`
    // over-counts against the disk and `multi_title` misfires. See
    // docs/mux.md.
    #[test]
    fn duplicate_title_indices_are_deduped_preserving_first_seen_order() {
        let d = disc(4, false, true);
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![1, 1])),
            vec![1],
            "a repeated index must collapse to one"
        );
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![2, 0, 2, 1, 0])),
            vec![2, 0, 1],
            "de-duplication must keep first-seen order"
        );
        // Out-of-range entries are still dropped, and dedupe applies after.
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![9, 3, 9, 3])),
            vec![3]
        );
    }
}
