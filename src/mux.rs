//! ISO/disc → MKV muxing and the multi-title rip loop.
//!
//! This is the orchestration that lived in the CLI's `run()` and autorip's
//! `rip_disc`: resolve which titles to rip, mux each through
//! `libfreemkv::mux_stream`, and decide when a failure is fatal vs skippable.
//! It carries three behaviours that were wrong or duplicated in the consumers
//! (user feedback 2026-07-28):
//!
//! 1. **Fail-fast on a disc-level key failure.** If the disc needs decryption
//!    and no key resolved (and not `raw`), EVERY title will fail — so we refuse
//!    once, up front, instead of printing a "no key" error for all N titles.
//! 2. **Cancel is a full stop.** One halt breaks the whole title loop; it does
//!    NOT cancel each remaining title individually and carry on.
//! 3. **Main-title default** (via [`Selection`]) so an obfuscated disc with 50+
//!    similar-length playlists doesn't rip everything by accident.

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
        Selection::Longest => disc
            .titles
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.duration_secs
                    .partial_cmp(&b.duration_secs)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| vec![i])
            .unwrap_or_default(),
        Selection::Titles(indices) => indices.iter().copied().filter(|&i| i < n).collect(),
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
    Ok { titles_written: usize },
    /// A disc-level key failure surfaced — the whole disc can't be decrypted,
    /// so the loop stopped (fail-fast) rather than iterate every title.
    NoKey,
    /// A title the user wanted (the feature, or an explicit `-t`) failed hard.
    Failed { title_index: usize },
    /// The rip was cancelled — a full stop, not a per-title cancel.
    Halted,
}

/// Drive the multi-title rip loop. `mux_one(idx) -> io::Result<()>` muxes a
/// single title; injecting it keeps the loop's control flow (fail-fast, skip,
/// halt-break) unit-testable without a real ISO. Production passes
/// [`mux_title`] (or the consumer's own single-title mux).
///
/// Self-contained — no `Disc` needed. The three behaviours (user feedback
/// 2026-07-28):
/// 1. **Fail-fast on a disc-level key failure**: the FIRST title that fails
///    with a whole-disc key error (`is_disc_level_no_key`) stops the whole rip
///    — every remaining title would fail identically. No 54× error spew.
/// 2. **Cancel is a full stop**: `should_cancel()` between titles, OR a halt
///    error from inside a title's mux, returns [`RipOutcome::Halted`] and does
///    NOT continue to the next title.
/// 3. Skippable stubs (empty/uncrackable non-feature titles) are skipped in a
///    multi-title, non-explicit rip; fatal on the feature / explicit `-t` /
///    single-title.
///
/// `explicit_selection` is `true` when the user named specific titles (so a
/// stub there is what they asked for → fatal). For an all-titles rip pass
/// `false`.
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
        let result = match mux_one(idx) {
            Ok(()) => TitleResult::Ok,
            Err(e) => classify_title_error(&e),
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
            TitleAction::StopFatal => return RipOutcome::Failed { title_index: idx },
        }
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
    use std::sync::mpsc;

    let halt = libfreemkv::Halt::new();
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
        // Watcher: drains progress → sink, and mirrors should_cancel → halt.
        // `move` captures the `!Sync` Receiver into this thread; `sink` is a
        // borrowed `&dyn Sink` (copied ref, tied to the scope); `done` is a
        // shared Arc the main thread sets when the mux returns.
        let watcher_halt = halt.clone();
        let watcher_done = done.clone();
        s.spawn(move || {
            // The engine's ONE speed/ETA derivation for the mux stage. Owned by
            // this single watcher thread, so a plain `mut` — no lock needed.
            let mut speed = crate::speed::SpeedEstimator::new();
            loop {
                // Coalesce all progress ticks that queued up since the last
                // wake to the LATEST, then sample ONCE. Sampling per-message
                // would measure `dt` as the microseconds between tight-loop
                // iterations against a byte-delta representing ~100 ms of real
                // work — yielding absurd multi-GB/s speeds. One sample per drain
                // makes `dt` the real ~100 ms wake interval.
                let mut latest = None;
                while let Ok(m) = rx.try_recv() {
                    latest = Some(m);
                }
                if let Some((done_b, total_b)) = latest {
                    let (speed_bps, eta_secs) = speed.sample(done_b, total_b);
                    let p = crate::sink::Progress {
                        pass: "mux".to_string(),
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

        sink.log(
            Level::Info,
            &format!(
                "mux: {source_url} -> {dest} (~{})",
                human_bytes(total_bytes_hint)
            ),
        );
        let events: Arc<dyn libfreemkv::MuxEvents> = Arc::new(ChannelEvents { tx });
        let outcome = libfreemkv::mux_stream(
            libfreemkv::MuxInput::Url {
                url: source_url,
                opts: input_opts,
            },
            dest,
            mux_opts,
            &halt,
            events,
        );
        done.store(true, Ordering::Relaxed);
        outcome
    })
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

    #[test]
    fn selection_explicit_drops_out_of_range() {
        let d = disc(2, false, false);
        assert_eq!(
            resolve_selection(&d, &Selection::Titles(vec![0, 9])),
            vec![0]
        );
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

    #[test]
    fn stub_on_the_feature_is_fatal() {
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            if idx == 0 { Err(stub_err()) } else { Ok(()) }
        });
        assert_eq!(outcome, RipOutcome::Failed { title_index: 0 });
    }

    #[test]
    fn explicit_single_title_stub_is_fatal() {
        // `-t 2` explicitly (explicit=true): a stub there is what the user asked
        // for → fatal.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::Titles(vec![1]));
        let outcome = run_titles(&indices, true, &NoopSink, |_| Err(stub_err()));
        assert_eq!(outcome, RipOutcome::Failed { title_index: 1 });
    }

    #[test]
    fn hard_failure_on_non_feature_is_still_fatal() {
        // A non-stub hard error is never skippable, even on a bonus title.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let outcome = run_titles(&indices, false, &NoopSink, |idx| {
            if idx == 1 { Err(hard_err()) } else { Ok(()) }
        });
        assert_eq!(outcome, RipOutcome::Failed { title_index: 1 });
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
}
