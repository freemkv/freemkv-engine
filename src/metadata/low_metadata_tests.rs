use super::*;
use std::cell::Cell;
use std::io::{Read, Write};
use std::sync::Mutex;

thread_local! {
    static RUNTIME_PROBES: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn count_runtime_probe() {
    RUNTIME_PROBES.with(|count| count.set(count.get() + 1));
}

fn episodes(minutes: &[u16]) -> Vec<Episode> {
    minutes
        .iter()
        .enumerate()
        .map(|(i, &runtime_min)| Episode {
            number: (i + 1) as u16,
            name: String::new(),
            runtime_min,
        })
        .collect()
}

#[test]
fn low_align_tied_minimum_keeps_fallback() {
    assert_eq!(align_disc_offset(&[2400.0], &episodes(&[50, 40, 40]), 1), 1);
    assert_eq!(align_disc_offset(&[2400.0], &episodes(&[40, 50, 40]), 2), 2);
}

#[test]
fn low_align_oversized_count_is_checked_before_arithmetic() {
    for count in [65_536, 65_537] {
        let actual = std::panic::catch_unwind(|| {
            align_disc_offset(&vec![2400.0; count], &episodes(&[50, 40]), 1)
        });
        assert_eq!(
            actual.expect("oversized count must not panic"),
            1,
            "count {count}"
        );
    }
    assert_eq!(
        align_disc_offset(&vec![2400.0; 65_535], &episodes(&[50, 40]), 7),
        7
    );
}

#[test]
fn low_align_runtime_probes_are_bounded_after_indexing() {
    let mut eps = episodes(&vec![45; 512]);
    eps.last_mut().unwrap().runtime_min = 99;
    RUNTIME_PROBES.with(|count| count.set(0));
    assert_eq!(
        align_disc_offset(&[2700.0, 2700.0, 2700.0, 5940.0], &eps, 1),
        509
    );
    let probes = RUNTIME_PROBES.with(Cell::get);
    assert!(
        probes >= eps.len(),
        "runtime probe instrumentation is inactive: {probes}"
    );
    assert!(probes <= eps.len() + 4 * 509, "runtime probes: {probes}");
}

#[test]
fn low_align_preserves_unique_later_better_and_signal_guards() {
    assert_eq!(align_disc_offset(&[2400.0], &episodes(&[50, 40, 60]), 1), 2);
    assert_eq!(
        align_disc_offset(&[2400.0], &episodes(&[45, 45, 40, 60]), 4),
        3
    );
    assert_eq!(align_disc_offset(&[2400.0], &episodes(&[41, 40]), 1), 2);
    assert_eq!(align_disc_offset(&[2430.0], &episodes(&[41, 40]), 1), 1);
    assert_eq!(align_disc_offset(&[0.0], &episodes(&[50, 40]), 7), 7);
    assert_eq!(align_disc_offset(&[2400.0], &episodes(&[0, 0]), 7), 7);
    assert_eq!(align_disc_offset(&[], &episodes(&[40]), 7), 7);
    assert_eq!(align_disc_offset(&[2400.0], &[], 7), 7);
}

#[test]
fn low_align_preserves_first_duplicates_in_unsorted_episodes() {
    let mut eps = episodes(&[50, 40, 60]);
    eps.swap(0, 2);
    eps.push(Episode {
        number: 2,
        name: String::new(),
        runtime_min: 60,
    });
    assert_eq!(align_disc_offset(&[2400.0], &eps, 1), 2);
    let mut unknown = episodes(&[50, 0, 40]);
    unknown.push(Episode {
        number: 2,
        name: String::new(),
        runtime_min: 40,
    });
    assert_eq!(align_disc_offset(&[2400.0], &unknown, 1), 3);
}

#[test]
fn low_lookup_malformed_results_is_response_failure() {
    for response in [
        serde_json::json!({}),
        serde_json::json!({"results": null}),
        serde_json::json!({"results": {}}),
    ] {
        assert!(
            matches!(
                lookup_using("HEAT_UE", |_| Some(response.clone())),
                Err(LookupFailed(None))
            ),
            "response: {response}"
        );
    }
    assert!(matches!(
        lookup_using("HEAT_UE", |_| Some(serde_json::json!({"results": []}))),
        Ok(None)
    ));
}

#[test]
fn low_lookup_malformed_results_retains_guess_and_later_confident_match() {
    let guess = || serde_json::json!({"results": [{"media_type": "movie", "title": "Heat", "release_date": "", "id": 1}]});
    let found = lookup_using("HEAT_UE", |query| {
        if query == "Heat Ue" {
            Some(guess())
        } else {
            Some(serde_json::json!({}))
        }
    });
    assert!(matches!(&found, Err(LookupFailed(Some(candidate))) if candidate.tmdb_id == 1));
    assert_eq!(best_effort(found).unwrap().tmdb_id, 1);
    let found = lookup_using("HEAT_UE", |query| {
        if query == "Heat Ue" {
            Some(serde_json::json!({"results": null}))
        } else {
            Some(
                serde_json::json!({"results": [{"media_type": "movie", "title": "Heat", "release_date": "1995-01-01", "id": 2}]}),
            )
        }
    });
    assert_eq!(found.unwrap().unwrap().tmdb_id, 2);
}

#[derive(Default)]
struct RecordingSink(Mutex<Vec<String>>);
impl crate::Sink for RecordingSink {
    fn log(&self, level: crate::Level, message: &str) {
        assert!(matches!(level, crate::Level::Warn));
        self.0.lock().unwrap().push(message.to_owned());
    }
}

fn local_fetch(
    response: Vec<u8>,
    path: &str,
    context: &str,
    sink: &RecordingSink,
) -> Option<serde_json::Value> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let count = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert_eq!(request.matches("GET ").count(), 1);
        stream.write_all(&response).unwrap();
    });
    let got = fetch_url(
        &format!("http://{addr}{path}?api_key=SECRETKEY123"),
        context,
        sink,
    );
    server.join().unwrap();
    got
}

fn assert_safe_warning(sink: &RecordingSink, expected: &str) {
    let logs = sink.0.lock().unwrap();
    assert_eq!(logs.len(), 1, "warnings: {logs:?}");
    assert!(logs[0].contains(expected), "warnings: {logs:?}");
    for forbidden in ["SECRETKEY123", "http://", "api_key=", "RAW_BODY_SECRET"] {
        assert!(!logs[0].contains(forbidden), "leaked {forbidden}: {logs:?}");
    }
}

#[test]
fn low_sink_reports_search_and_season_http_failures() {
    for path in ["/3/search/multi", "/3/tv/7/season/3"] {
        let sink = RecordingSink::default();
        assert!(
            local_fetch(
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
                path,
                "season 3",
                &sink
            )
            .is_none()
        );
        assert_safe_warning(&sink, "503");
    }
}

#[test]
fn low_sink_reports_search_and_season_json_and_body_failures() {
    for path in ["/3/search/multi", "/3/tv/7/season/3"] {
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\nRAW_BODY_SECRET"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".to_vec(),
        ] {
            let sink = RecordingSink::default();
            assert!(local_fetch(response, path, "season 3", &sink).is_none());
            assert_safe_warning(&sink, "response");
        }
    }
}

#[test]
fn low_sink_reports_search_and_season_transport_failures() {
    for context in ["search", "season 3"] {
        let sink = RecordingSink::default();
        assert!(fetch_url("api_key=SECRETKEY123", context, &sink).is_none());
        assert_safe_warning(&sink, "transport");
    }
}

#[test]
fn low_sink_keeps_no_key_quiet_and_401_throttled() {
    let sink = RecordingSink::default();
    let client = Client::new("", &sink);
    assert!(client.lookup("Heat").unwrap().is_none());
    assert!(client.search("Heat", 5).is_empty());
    assert!(client.season_episodes(7, 3).is_empty());
    assert!(sink.0.lock().unwrap().is_empty());
    for _ in 0..2 {
        assert!(
            local_fetch(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
                "/3/search/multi",
                "Heat",
                &sink
            )
            .is_none()
        );
    }
    let logs = sink.0.lock().unwrap();
    assert!(
        logs.len() <= 1,
        "401 must not add a generic warning: {logs:?}"
    );
    assert!(
        logs.iter()
            .all(|message| message.contains("API key rejected (HTTP 401)"))
    );
}
