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
            Some(libfreemkv::test_util::aacs_state().build())
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

fn audio_title(lang: &str, start: u32) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.content_format = libfreemkv::ContentFormat::DvdPs;
    t.duration_secs = 3600.0;
    t.extents = vec![libfreemkv::disc::Extent {
        start_lba: start,
        sector_count: 1000,
    }];
    t.streams
        .push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid: 0x1100,
            codec: libfreemkv::Codec::TrueHd,
            channels: libfreemkv::AudioChannels::Stereo,
            language: lang.into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        }));
    t
}

#[test]
fn main_movie_language_prefers_an_equivalent_language_presentation() {
    let mut d = disc(0, false, false);
    d.titles = vec![audio_title("eng", 1000), audio_title("deu", 1000)];
    let mut job = Job::new("disc://", "mkv://out");
    job.streams.audio = StreamFilter::Langs(vec!["deu".into()]);
    assert_eq!(resolve_job_selection(&d, &job), vec![1]);
}

#[test]
fn all_audio_does_not_change_main_movie_program_choice() {
    let mut d = disc(0, false, false);
    d.titles = vec![audio_title("eng", 1000), audio_title("deu", 1000)];
    let job = Job::new("disc://", "mkv://out");
    assert_eq!(resolve_job_selection(&d, &job), vec![0]);
}

#[test]
fn language_preference_does_not_switch_to_a_distinct_program() {
    let mut d = disc(0, false, false);
    d.titles = vec![audio_title("eng", 1000), audio_title("deu", 9000)];
    let mut job = Job::new("disc://", "mkv://out");
    job.streams.audio = StreamFilter::Langs(vec!["deu".into()]);
    assert_eq!(resolve_job_selection(&d, &job), vec![0]);
}

fn clipped_audio(lang: &str, start: u32, end: u32) -> libfreemkv::DiscTitle {
    let mut t = audio_title(lang, 1000);
    t.content_format = libfreemkv::ContentFormat::BdTs;
    t.duration_secs = f64::from(end - start);
    t.clips = vec![libfreemkv::Clip {
        clip_id: "00001".into(),
        in_time: start * 45_000,
        out_time: end * 45_000,
        duration_secs: t.duration_secs,
        source_packets: 0,
        feed_span: None,
    }];
    t
}

#[test]
fn regression_authored_intervals_preserve_distinct_episodes_and_main_content() {
    let mut d = disc(0, false, false);
    d.titles = vec![
        clipped_audio("eng", 0, 1200),
        clipped_audio("deu", 1200, 2520),
    ];
    assert_eq!(known_episode_titles(&d.titles), vec![0, 1]);
    assert_eq!(
        resolve_selection_with_audio(
            &d,
            &Selection::MainMovie,
            &StreamFilter::Langs(vec!["de".into()])
        ),
        vec![0]
    );
    d.titles[1].clips[0].in_time = 0;
    assert_eq!(
        resolve_selection_with_audio(
            &d,
            &Selection::MainMovie,
            &StreamFilter::Langs(vec!["de".into()])
        ),
        vec![0]
    );
}

#[test]
fn regression_missing_or_invalid_identity_never_equates_presentations() {
    for shape in 0..5 {
        let mut a = clipped_audio("eng", 0, 1200);
        match shape {
            0 => {
                a.clips.clear();
                a.extents.clear();
            }
            1 => a.clips.clear(),
            2 => a.clips[0].clip_id.clear(),
            3 => a.clips[0].out_time = a.clips[0].in_time,
            _ => {
                a.clips.clear();
                a.content_format = libfreemkv::ContentFormat::DvdPs;
                a.extents[0].sector_count = 0;
            }
        }
        let mut b = a.clone();
        b.streams = audio_title("deu", 1000).streams;
        let mut d = disc(0, false, false);
        d.titles = vec![a, b];
        assert_eq!(known_episode_titles(&d.titles), vec![0, 1], "shape {shape}");
        assert_eq!(
            resolve_selection_with_audio(
                &d,
                &Selection::MainMovie,
                &StreamFilter::Langs(vec!["de".into()])
            ),
            vec![0],
            "shape {shape}"
        );
    }
}

#[test]
fn regression_episodes_choose_audio_before_dedup_in_either_disc_order() {
    for dvd in [false, true] {
        for reversed in [false, true] {
            let mut a = clipped_audio("eng", 0, 2880);
            let mut b = clipped_audio("deu", 0, 2880);
            if dvd {
                for t in [&mut a, &mut b] {
                    t.clips.clear();
                    t.content_format = libfreemkv::ContentFormat::DvdPs;
                    t.extents.push(libfreemkv::disc::Extent {
                        start_lba: 9000,
                        sector_count: 100,
                    });
                }
                b.duration_secs = 2760.0;
            }
            let mut d = disc(0, false, false);
            d.titles = if reversed { vec![b, a] } else { vec![a, b] };
            mark_authored(&mut d.titles);
            let mut job = Job::new("disc://", "mkv://out");
            job.selection = Selection::Episodes;
            job.streams.audio = StreamFilter::Langs(vec!["German".into()]);
            assert_eq!(
                resolve_job_selection(&d, &job),
                vec![usize::from(!reversed)]
            );
            assert_eq!(crate::preflight(&d, &job), crate::Preflight::Ready);
            job.streams.audio = StreamFilter::Langs(vec!["jpn".into()]);
            assert_eq!(resolve_job_selection(&d, &job), vec![0]);
            assert!(matches!(
                crate::preflight(&d, &job),
                crate::Preflight::Blocked(_)
            ));
            for audio in [StreamFilter::All, StreamFilter::None] {
                job.streams.audio = audio;
                assert_eq!(resolve_job_selection(&d, &job), vec![0]);
            }
        }
    }
}

#[test]
fn regression_audio_ranking_prefers_language_coverage_not_duplicate_tracks() {
    let a = clipped_audio("eng", 0, 1200);
    let mut duplicate_tracks = a.clone();
    duplicate_tracks.streams.extend(a.streams.clone());
    let mut both = a.clone();
    both.streams.extend(clipped_audio("deu", 0, 1200).streams);
    let mut d = disc(0, false, false);
    d.titles = vec![duplicate_tracks, both, a];
    mark_authored(&mut d.titles);
    let audio = StreamFilter::Langs(vec!["en".into(), "eng".into(), "de".into()]);
    for selection in [Selection::MainMovie, Selection::Episodes] {
        assert_eq!(
            resolve_selection_with_audio(&d, &selection, &audio),
            vec![1]
        );
    }
    for selection in [
        Selection::Titles(vec![2, 0]),
        Selection::All,
        Selection::Longest,
    ] {
        assert_eq!(
            resolve_selection_with_audio(&d, &selection, &audio),
            resolve_selection(&d, &selection)
        );
    }
}

#[test]
fn regression_dvd_cell_order_and_content_format_are_identity() {
    let mut a = audio_title("eng", 1000);
    a.extents.push(libfreemkv::disc::Extent {
        start_lba: 9000,
        sector_count: 100,
    });
    let mut b = a.clone();
    b.extents.reverse();
    let mut c = a.clone();
    c.content_format = libfreemkv::ContentFormat::MpegPs;
    assert_eq!(known_episode_titles(&[a, b, c]), vec![0, 1, 2]);
}

#[test]
fn regression_six_playlists_preserve_three_episodes_and_distinct_decoys() {
    let mut d = disc(0, false, false);
    for program in ["episode-a", "episode-b", "episode-c"] {
        for lang in ["eng", "deu"] {
            let mut t = clipped_audio(lang, 30, 1230);
            t.clips[0].clip_id = program.into();
            let mut intro = t.clips[0].clone();
            intro.clip_id = "shared-intro".into();
            intro.in_time = 0;
            intro.out_time = 30 * 45_000;
            t.clips.insert(0, intro);
            d.titles.push(t);
        }
    }
    mark_authored(&mut d.titles);
    let audio = StreamFilter::Langs(vec!["de".into()]);
    assert_eq!(known_episode_titles(&d.titles), vec![0, 2, 4]);
    assert_eq!(
        resolve_selection_with_audio(&d, &Selection::Episodes, &audio),
        vec![1, 3, 5]
    );
    let mut reordered = d.titles[1].clone();
    reordered.clips.reverse();
    let mut shifted = d.titles[1].clone();
    shifted.clips[1].in_time += 1;
    d.titles.extend([reordered, shifted]);
    // Newly added titles are not covered by the original roster: hold for review.
    let report = crate::SelectionModel::from_disc(&d).select(&Selection::Episodes, &audio);
    assert!(report.indices.is_empty());
    assert!(report.requires_review());
    assert_eq!(report.candidates, vec![1, 3, 5, 6, 7]);
}

#[test]
fn regression_identity_ignores_runtime_and_size_when_authored_intervals_match() {
    for dvd in [false, true] {
        let mut a = if dvd {
            audio_title("eng", 1000)
        } else {
            clipped_audio("eng", 0, 1200)
        };
        a.duration_secs = 1200.0;
        let mut b = a.clone();
        b.duration_secs = 1320.0;
        b.size_bytes += 10_000;
        assert_eq!(known_episode_titles(&[a, b]), vec![0]);
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

// Ties go to the FIRST title, not the last (`max_by` would pick the LAST of equal maxima,
// i.e. a decoy).
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

// A non-finite duration must never win, INCLUDING as the first title (`t > NaN` is false
// for every `t`, so a leading NaN is never displaced).
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

// The range filter is `i < n`, and `n` itself is out of range — pins the boundary that `<`
// vs `<=` off-by-one bugs hide behind.
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

// Episodes: a failed one is dropped and the rest delivered; when every one fails the
// rip failed (never an `Ok` that wrote nothing), with the last failure's detail.
#[test]
fn episodes_that_all_fail_are_a_failed_rip() {
    let hard = |_: usize| {
        Err(TitleError {
            result: TitleResult::Failed,
            error: std::io::Error::other("disk full"),
        })
    };
    let out = run_episodes(&[1, 2], &NoopSink, hard);
    assert!(
        matches!(out, RipOutcome::Failed { title_index: 2, .. }),
        "{out:?}"
    );
    let some = run_episodes(&[1, 2], &NoopSink, |i| match i {
        1 => hard(i),
        _ => Ok(()),
    });
    assert_eq!(some, RipOutcome::Ok { titles_written: 1 });
}

// A lone selected title is NOT a multi-title rip: `multi_title = indices.len() > 1` guards
// against silently swallowing a single-title stub as a "successful" empty rip.
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
            data: error_data(&stub_err()),
        },
        "the only title the rip was going to write came back a stub — that \
             is a failed rip, not a rip that skipped a bonus feature"
    );
}

/// `open_scan` opens a real drive, so the only testable part of it
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

// The GUI raw disc→ISO copy must scan like the CLI `--raw`: on past an unreadable key file.
#[test]
fn raw_copy_reaches_the_scan_options() {
    assert!(
        scan_options(true).raw_copy,
        "a raw copy must scan with raw_copy"
    );
    assert!(
        !scan_options(false).raw_copy,
        "a decrypting rip keeps the fatal E7031"
    );

    // `session.scan` needs a live drive, so pin the source instead: the value
    // handed to it must be this function's answer, not a hardcoded default.
    let src = include_str!("mux.rs").replace("\r\n", "\n");
    let body = |name: &str| {
        let start = src.find(name).expect("definition present");
        let end = start
            + src[start..]
                .find("\n}\n")
                .expect("the function body still ends the definition");
        src[start..end].to_string()
    };
    assert!(
        body("pub fn open_scan_with(").contains("scan_options_with(raw_copy, halt)")
            && scan_options_with(true, &libfreemkv::Halt::new()).raw_copy,
        "open_scan must hand its own raw_copy parameter to the scan, \
             not a hardcoded default"
    );
}

/// `open_scan` (KU §3.2): the raw drive bring-up, no key call; signature pinned.
#[test]
fn open_scan_signature() {
    let _: fn(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error> = open_scan;
    type OpenScanWith = fn(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
        &libfreemkv::Halt,
        &libfreemkv::halt::Liveness,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error>;
    let _: OpenScanWith = open_scan_with;
}

// ET9 `open_scan_with_locks_tray_after_scan` — stop design v5 §4.2: "KU's `open_scan`
// (which replaces `open_scan_resolve*`, `raw_copy` preserved) locks the tray after the
// scan"; §5.4: "a Stop during the scan leaves the tray unlocked".
#[test]
fn open_scan_with_locks_tray_after_scan() {
    let steps = std::cell::RefCell::new(Vec::new());
    let stopped = scan_then_lock(
        (),
        |_| {
            steps.borrow_mut().push("scan");
            Err(libfreemkv::Error::Halted)
        },
        |_| steps.borrow_mut().push("lock"),
    );
    assert!(matches!(stopped, Err(libfreemkv::Error::Halted)));
    assert_eq!(
        *steps.borrow(),
        ["scan"],
        "a Stop during the scan locks nothing"
    );
    steps.borrow_mut().clear();
    let ok = scan_then_lock(
        (),
        |_| {
            steps.borrow_mut().push("scan");
            Ok(())
        },
        |_| steps.borrow_mut().push("lock"),
    );
    assert!(ok.is_ok());
    assert_eq!(*steps.borrow(), ["scan", "lock"]);
    // `open_scan` needs a live drive: pin that it goes through the ordered helper.
    let src = include_str!("mux.rs").replace("\r\n", "\n");
    let start = src.find("pub fn open_scan_with(").unwrap();
    let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
    assert!(body.contains("scan_then_lock(") && !body.contains("session.lock_tray()"));
}

// Stop design v5 §4.3, "The open token": "Both are threaded into the scan (the Drive
// bumps the `Progress` per `exec` …)". The scan needs a live drive, so this pins the
// wiring. Per spec; do not change without a spec citation proving otherwise.
#[test]
fn open_scan_with_threads_the_open_token_into_the_scan() {
    let src = include_str!("mux.rs").replace("\r\n", "\n");
    let body = |name: &str| {
        let start = src.find(name).expect("definition present");
        src[start..start + src[start..].find("\n}\n").unwrap()].to_string()
    };
    let with = body("pub fn open_scan_with(");
    for wiring in [
        "DiscSession::open_with(target, build_keyspec(credentials), halt)",
        "session.attach_progress(progress);",
        "let opts = scan_options_with(raw_copy, halt);",
        "session.scan_with(opts)",
        "scan_then_lock(",
    ] {
        assert!(with.contains(wiring), "open_scan_with lacks {wiring}");
    }
    assert!(
        body("pub fn open_scan(").contains("open_scan_with("),
        "open_scan delegates"
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

// The mux must not start work it has ALREADY been told to stop; the sink here answers
// `true` exactly ONCE (the pre-check), `false` after, so only the pre-check can cancel the
// token.
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
    let halted_on_entry = with_mux_watcher(&sink, "mkv:///o.mkv", |ctx| ctx.halt.is_cancelled());
    assert!(
        halted_on_entry,
        "a rip that was cancelled before it began must reach the muxer \
             already halted, not run to completion"
    );
}

// A Stop pressed once the mux is under way must reach the muxer via the watcher's poll, not
// the pre-check (the sink only starts cancelling AFTER the mux begins).
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
    let saw_halt = with_mux_watcher(&sink, "mkv:///o.mkv", |ctx| {
        sink.started.store(true, Ordering::SeqCst);
        wait_for(5, || ctx.halt.is_cancelled())
    });
    assert!(
        saw_halt,
        "the watcher must mirror should_cancel onto the halt token the mux \
             polls; otherwise Stop does nothing until the mux finishes on its own"
    );
}

// Write-progress from the muxer must arrive at the sink as a `mux` tick via the channel +
// watcher drain bridge.
#[test]
fn write_progress_reaches_the_sink_as_a_mux_progress_tick() {
    let sink = RecordingSink::default();
    with_mux_watcher(&sink, "mkv:///o.mkv", |ctx| {
        ctx.events.event(&libfreemkv::Event::BytesWritten {
            bytes: 4096,
            total: 8192,
        });
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

// The output opening reaches the sink with its dest and title (the front ends print the
// pre-mux note there), even when the mux returns before the watcher's next poll.
#[test]
fn output_opened_reaches_the_sink_even_from_a_mux_that_returns_at_once() {
    #[derive(Default)]
    struct Opened(std::sync::Mutex<Vec<(String, usize)>>);
    impl Sink for Opened {
        fn event(&self, e: &crate::sink::Event<'_>) {
            if let crate::sink::Event::OutputOpened { dest, title } = e {
                self.0
                    .lock()
                    .unwrap()
                    .push((dest.to_string(), title.streams.len()));
            }
        }
    }
    let sink = Opened::default();
    with_mux_watcher(&sink, "mpg:///o.mpg", |ctx| {
        ctx.events.event(&libfreemkv::Event::OutputOpened {
            title: &libfreemkv::DiscTitle::empty(),
        });
    });
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![("mpg:///o.mpg".to_string(), 0)]
    );
}

// ET23 `mux_close_flush_progress_reaches_sink` — stop design v5 §4.5: each `BytesDurable`
// event becomes `Sink::progress(pass: "sync")`; no call when nothing moves.
#[test]
fn mux_close_flush_progress_reaches_sink() {
    let sink = RecordingSink::default();
    with_mux_watcher(&sink, "mkv:///o.mkv", |ctx| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(
            sink.ticks.lock().unwrap().is_empty(),
            "a call with no flush progress"
        );
        ctx.events.event(&libfreemkv::Event::BytesDurable {
            bytes: 1 << 20,
            total: 4 << 20,
        });
        ctx.events.event(&libfreemkv::Event::BytesDurable {
            bytes: 2 << 20,
            total: 4 << 20,
        });
        assert!(
            wait_for(5, || sink.ticks.lock().unwrap().len() == 2),
            "the flush progress did not reach the sink"
        );
    });
    let ticks = sink.ticks.lock().unwrap();
    assert!(
        ticks
            .iter()
            .all(|p| p.pass == "sync" && p.bytes_total == 4 << 20)
    );
    assert_eq!(ticks[0].bytes_done, 1 << 20);
    assert_eq!(ticks[1].bytes_done, 2 << 20);
}

// A tick sent while the watcher sits between its drain and its `done` check still
// arrives: its cancel poll (in that window) lets the mux send its last tick and return.
#[test]
fn a_last_tick_sent_as_the_mux_returns_reaches_the_sink() {
    use std::sync::mpsc::{Receiver, Sender, channel};
    struct Gate {
        ticks: std::sync::Mutex<Vec<crate::sink::Progress>>,
        polls: AtomicUsize,
        in_poll: std::sync::Mutex<Option<Sender<()>>>,
        sent: std::sync::Mutex<Option<Receiver<()>>>,
    }
    impl Sink for Gate {
        fn progress(&self, p: &crate::sink::Progress) {
            self.ticks.lock().unwrap().push(p.clone());
        }
        // Poll 0 is the caller's pre-check; poll 1 is the watcher's first.
        fn should_cancel(&self) -> bool {
            if self.polls.fetch_add(1, Ordering::SeqCst) == 1 {
                let _ = self.in_poll.lock().unwrap().take().map(|t| t.send(()));
                if let Some(rx) = self.sent.lock().unwrap().take() {
                    let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            false
        }
    }
    let (in_poll, polled) = channel();
    let (sent, got) = channel();
    let sink = Gate {
        ticks: Default::default(),
        polls: AtomicUsize::new(0),
        in_poll: std::sync::Mutex::new(Some(in_poll)),
        sent: std::sync::Mutex::new(Some(got)),
    };
    with_mux_watcher(&sink, "mkv:///o.mkv", |ctx| {
        let _ = polled.recv_timeout(std::time::Duration::from_secs(5));
        ctx.events.event(&libfreemkv::Event::BytesWritten {
            bytes: 4 << 20,
            total: 4 << 20,
        });
        ctx.events.event(&libfreemkv::Event::BytesDurable {
            bytes: 4 << 20,
            total: 4 << 20,
        });
        let _ = sent.send(());
    });
    let ticks = sink.ticks.lock().unwrap();
    let passes: Vec<&str> = ticks.iter().map(|p| p.pass.as_ref()).collect();
    assert_eq!(
        passes,
        ["mux", "sync"],
        "the last mux and sync ticks were dropped"
    );
}

// A panic inside the mux must still release the watcher, or an unwind skips storing `done`
// and the watcher loops forever — a hang, not a failure. Bounded here for that reason.
#[test]
fn a_panicking_mux_still_releases_the_watcher() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let sink = NoopSink;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_mux_watcher(&sink, "mkv:///o.mkv", |_ctx| panic!("mux blew up"))
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
    // The user's ~51.7 GB rip: GB, one decimal, not 55460235264 B.
    assert_eq!(human_bytes(55_460_235_264), "51.7 GB");
}

/// A value just under a threshold that rounds up to it reads in the next unit.
#[test]
fn human_bytes_never_renders_1024_of_a_unit() {
    assert_eq!(human_bytes(1_048_575), "1 MB");
    assert_eq!(human_bytes(1_073_741_823), "1.0 GB");
    assert_eq!(human_bytes(1_048_000), "1023 KB");
    assert_eq!(human_bytes(1023 * 1024 * 1024), "1023 MB");
}

/// Each threshold is a PRODUCT of 1024s, and the values BETWEEN two
/// thresholds are what prove it: `K * K * K` mis-read as `K + K * K`
/// (or `K * K` as `K + K`) still renders the exact boundary values above
/// correctly, and turns every mid-range size into "0.0 GB" / "0 MB".
#[test]
fn human_bytes_thresholds_are_products_of_1024_not_sums() {
    // 2 MiB — above `K + K * K` (1 049 600) but far below a gigabyte.
    assert_eq!(human_bytes(2 * 1024 * 1024), "2 MB");
    // 4 KiB — above `K + K` (2048) but far below a megabyte.
    assert_eq!(human_bytes(4096), "4 KB");
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

// A rip that writes NO title must say so, whatever emptied it (empty `indices`, or every
// title a skippable stub) — the outcome stays `Ok` but must not be SILENT.
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
    let sink = LogSink::default();
    let outcome = run_titles(&[1, 2], false, &sink, |_| Err(stub_err()));
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

// The cause on `Failed` must actually discriminate: three failures, three distinguishable
// causes, each legible through the field that carries its meaning (typed vs passthrough OS
// error).
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
            data: error_data(&stub_err()),
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
            data: error_data(&stub_err()),
        }
    );
}

// `-t 2 -t 3`: a stub on an explicitly chosen non-feature title of a multi-title rip is
// fatal, not skipped; only a non-explicit rip skips it.
#[test]
fn explicit_multi_title_stub_is_fatal() {
    let d = disc(3, false, false);
    let indices = resolve_selection(&d, &Selection::Titles(vec![1, 2]));
    let outcome = run_titles(&indices, true, &NoopSink, |idx| {
        if idx == 1 { Err(stub_err()) } else { Ok(()) }
    });
    assert!(
        matches!(outcome, RipOutcome::Failed { title_index: 1, .. }),
        "{outcome:?}"
    );
    assert_eq!(
        decide_title(&TitleResult::SkippableStub, false, true, true),
        TitleAction::StopFatal
    );
    assert_eq!(
        decide_title(&TitleResult::SkippableStub, false, true, false),
        TitleAction::Skip
    );
}

// N9: disc-controlled text (a playlist name, an error's data) reaches `Sink::log` with
// its control characters escaped, so it cannot inject lines or terminal escapes.
#[test]
fn disc_text_in_a_log_line_has_no_control_characters() {
    #[derive(Default)]
    struct Lines(std::sync::Mutex<Vec<String>>);
    impl Sink for Lines {
        fn log(&self, _level: Level, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
    }
    let evil = "x\u{1b}[2J\nE0000 forged line\r";
    let sink = Lines::default();
    run_titles(&[0], false, &sink, |_| Err(std::io::Error::other(evil)));
    let path = std::path::Path::new("/r/d.iso");
    let mut lines = sink.0.into_inner().unwrap();
    lines.push(iso_mux_line(path, evil));
    for l in &lines {
        assert!(!l.chars().any(char::is_control), "{l:?}");
    }
    assert!(lines.iter().all(|l| l.contains("forged line")), "{lines:?}");
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
            data: error_data(&hard_err()),
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

// A repeated `-t` index must produce ONE entry, or `titles_written` over-counts against the
// disk and `multi_title` misfires.
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

fn mark_authored(titles: &mut [libfreemkv::DiscTitle]) {
    let members = (0..titles.len()).collect::<Vec<_>>();
    crate::test_fixtures::authored_episodes(titles, &members);
}

fn known_episode_titles(titles: &[libfreemkv::DiscTitle]) -> Vec<usize> {
    let mut titles = titles.to_vec();
    mark_authored(&mut titles);
    crate::episode_titles(&titles)
}
