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

use crate::job::{Job, Selection};
use crate::sink::{Level, Sink};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// Uses libfreemkv's typed classifiers — never string-matches E-codes.
pub fn classify_title_error(e: &std::io::Error) -> TitleResult {
    if libfreemkv::is_halt(e) {
        TitleResult::Halted
    } else if libfreemkv::is_skippable_title_stub(e) {
        TitleResult::SkippableStub
    } else {
        TitleResult::Failed
    }
}

/// The terminal outcome of a whole multi-title rip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RipOutcome {
    /// Every selected title that mattered succeeded (skippable stubs may have
    /// been skipped). Carries the count actually written.
    Ok { titles_written: usize },
    /// The disc needs decryption but no key resolved — refused before muxing
    /// any title (fail-fast; nothing was attempted).
    NoKey,
    /// A title the user wanted (the feature, or an explicit `-t`) failed hard.
    Failed { title_index: usize },
    /// The rip was cancelled — a full stop, not a per-title cancel.
    Halted,
}

/// Drive the multi-title rip loop. `mux_one(idx) -> io::Result<()>` muxes a
/// single title; injecting it keeps the loop's control flow (fail-fast, skip,
/// halt-break) unit-testable without a real ISO. Production passes
/// [`mux_title`].
///
/// `should_cancel` is polled between titles for the full-stop behaviour; the
/// per-title `mux_one` is also expected to observe cancellation internally (it
/// returns a halt error), which the loop treats as a full stop too.
pub fn run_titles<F>(
    disc: &libfreemkv::Disc,
    job: &Job,
    indices: &[usize],
    sink: &dyn Sink,
    mut mux_one: F,
) -> RipOutcome
where
    F: FnMut(usize) -> std::io::Result<()>,
{
    // (1) FAIL-FAST: a disc-level key failure means every title fails. Refuse
    // once, up front — do NOT iterate N titles printing a per-title error.
    if disc.encrypted && !job.raw {
        let has_key = disc.aacs.is_some() || disc.css.is_some();
        if !has_key {
            sink.log(
                Level::Error,
                "disc is encrypted and no decryption key resolved — refusing before muxing \
                 (every title would fail); provide a keydb or key source",
            );
            return RipOutcome::NoKey;
        }
    }

    let explicit_selection = matches!(job.selection, Selection::Titles(_));
    let multi_title = indices.len() > 1;
    let mut titles_written = 0usize;

    for &idx in indices {
        // Poll for a full-stop between titles (2).
        if sink.should_cancel() {
            sink.log(Level::Info, "cancelled — stopping the whole rip");
            return RipOutcome::Halted;
        }

        // The main FEATURE is title index 0; a failure there is always fatal.
        let is_feature = idx == 0;

        match mux_one(idx) {
            Ok(()) => titles_written += 1,
            Err(e) => match classify_title_error(&e) {
                // (2) A halt from inside the mux is a FULL STOP — break the
                // whole loop, do not continue to the next title.
                TitleResult::Halted => {
                    sink.log(Level::Info, "cancelled during mux — stopping the whole rip");
                    return RipOutcome::Halted;
                }
                TitleResult::SkippableStub if !is_feature && multi_title && !explicit_selection => {
                    // An incidental extra title that's an uncrackable/empty stub
                    // in an all-titles rip. Skip it, keep going.
                    sink.log(
                        Level::Info,
                        &format!("title {} skipped (empty/uncrackable stub)", idx + 1),
                    );
                }
                // The feature, an explicitly-requested title, a single-title
                // rip, or a non-stub failure → hard error for a title the user
                // wanted.
                TitleResult::SkippableStub | TitleResult::Failed => {
                    return RipOutcome::Failed { title_index: idx };
                }
                TitleResult::Ok => unreachable!("Ok is the Ok(()) arm"),
            },
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
            loop {
                // Drain any progress ticks that arrived.
                while let Ok((done_b, total_b)) = rx.try_recv() {
                    let p = crate::sink::Progress {
                        pass: "mux".to_string(),
                        bytes_done: done_b,
                        bytes_total: total_b,
                        sectors_bad: 0,
                        speed_bps: 0,
                        eta_secs: None,
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
            &format!("mux: {source_url} -> {dest} (~{total_bytes_hint} bytes)"),
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
    use crate::job::{Job, RipMode};
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

    #[test]
    fn fail_fast_no_key_refuses_before_muxing_any_title() {
        // The user's scenario: 54 titles, encrypted, no key. Must NOT iterate.
        let d = disc(54, true, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job::new("iso://x", "/o");
        let calls = AtomicUsize::new(0);
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(outcome, RipOutcome::NoKey);
        assert_eq!(calls.load(Ordering::Relaxed), 0, "no title was muxed");
    }

    #[test]
    fn raw_bypasses_the_no_key_fail_fast() {
        let d = disc(2, true, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job {
            raw: true,
            ..Job::new("iso://x", "/o")
        };
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |_| Ok(()));
        assert_eq!(outcome, RipOutcome::Ok { titles_written: 2 });
    }

    #[test]
    fn halt_from_mux_is_a_full_stop_not_per_title() {
        // First title halts → the loop stops immediately, does NOT visit title 2.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job::new("iso://x", "/o");
        let visited = AtomicUsize::new(0);
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |idx| {
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
        // All-titles rip: title 0 ok, title 1 a stub (skipped), title 2 ok.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job::new("iso://x", "/o").with_mode(RipMode::Single);
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |idx| {
            if idx == 1 { Err(stub_err()) } else { Ok(()) }
        });
        assert_eq!(outcome, RipOutcome::Ok { titles_written: 2 });
    }

    #[test]
    fn stub_on_the_feature_is_fatal() {
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job::new("iso://x", "/o");
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |idx| {
            if idx == 0 { Err(stub_err()) } else { Ok(()) }
        });
        assert_eq!(outcome, RipOutcome::Failed { title_index: 0 });
    }

    #[test]
    fn explicit_single_title_stub_is_fatal() {
        // `-t 2` explicitly: a stub there is what the user asked for → fatal.
        let d = disc(3, false, false);
        let job = Job {
            selection: Selection::Titles(vec![1]),
            ..Job::new("iso://x", "/o")
        };
        let indices = resolve_selection(&d, &job.selection);
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |_| Err(stub_err()));
        assert_eq!(outcome, RipOutcome::Failed { title_index: 1 });
    }

    #[test]
    fn hard_failure_on_non_feature_is_still_fatal() {
        // A non-stub hard error is never skippable, even on a bonus title.
        let d = disc(3, false, false);
        let indices = resolve_selection(&d, &Selection::All);
        let job = Job::new("iso://x", "/o");
        let outcome = run_titles(&d, &job, &indices, &NoopSink, |idx| {
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
        let job = Job::new("iso://x", "/o");
        let sink = CancelAfterFirst {
            seen: AtomicUsize::new(0),
        };
        let outcome = run_titles(&d, &job, &indices, &sink, |_| {
            sink.seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(outcome, RipOutcome::Halted);
    }
}
