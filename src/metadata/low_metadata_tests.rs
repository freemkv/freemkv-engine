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
                lookup_using("HEAT_UE", |_| Ok(response.clone())),
                Err(LookupFailed(None))
            ),
            "response: {response}"
        );
    }
    assert!(matches!(
        lookup_using("HEAT_UE", |_| Ok(serde_json::json!({"results": []}))),
        Ok(None)
    ));
}

#[test]
fn low_lookup_malformed_results_retains_guess_and_later_confident_match() {
    let guess = || serde_json::json!({"results": [{"media_type": "movie", "title": "Heat", "release_date": "", "id": 1}]});
    let found = lookup_using("HEAT_UE", |query| {
        if query == "Heat Ue" {
            Ok(guess())
        } else {
            Ok(serde_json::json!({}))
        }
    });
    assert!(matches!(&found, Err(LookupFailed(Some(candidate))) if candidate.tmdb_id == 1));
    assert_eq!(best_effort(found).unwrap().tmdb_id, 1);
    let found = lookup_using("HEAT_UE", |query| {
        if query == "Heat Ue" {
            Ok(serde_json::json!({"results": null}))
        } else {
            Ok(
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

// Serve one canned `response` on loopback and point the TMDB base at it for this thread.
// Returns the server handle; joining it proves exactly one request arrived.
fn serve_once(response: Vec<u8>) -> std::thread::JoinHandle<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    super::TEST_API_BASE.with(|b| *b.borrow_mut() = Some(format!("http://{addr}")));
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        let count = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert_eq!(request.matches("GET ").count(), 1);
        stream.write_all(&response).unwrap();
    })
}

// Run each Client method against its own one-shot server answering `response`.
// All three must come back empty/failed, forwarding their failure to the sink.
fn assert_client_calls_fail(response: &[u8], expected: &str) {
    type Call = fn(&Client) -> bool;
    let calls: [(&str, Call); 3] = [
        ("search", |c| c.search("Heat", 5).is_empty()),
        ("lookup", |c| c.lookup("Heat").is_err()),
        ("season_episodes", |c| c.season_episodes(7, 3).is_empty()),
    ];
    for (name, call) in calls {
        let sink = RecordingSink::default();
        let server = serve_once(response.to_vec());
        assert!(call(&Client::new("SECRETKEY123", &sink)), "{name}");
        server.join().unwrap();
        assert_safe_warning(&sink, expected);
    }
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
    assert_client_calls_fail(
        b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "503",
    );
}

#[test]
fn low_sink_reports_search_and_season_json_and_body_failures() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\nRAW_BODY_SECRET"[..],
        &b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{"[..],
    ] {
        assert_client_calls_fail(response, "response");
    }
}

#[test]
fn low_sink_reports_search_and_season_transport_failures() {
    super::TEST_API_BASE.with(|b| *b.borrow_mut() = Some("api_key".to_string()));
    for context in ["search", "lookup", "season_episodes"] {
        let sink = RecordingSink::default();
        let client = Client::new("SECRETKEY123", &sink);
        match context {
            "search" => assert!(client.search("Heat", 5).is_empty()),
            "lookup" => assert!(client.lookup("Heat").is_err()),
            _ => assert!(client.season_episodes(7, 3).is_empty()),
        }
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
    let client = Client::new("SECRETKEY123", &sink);
    for _ in 0..2 {
        let server = serve_once(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        assert!(client.search("Heat", 5).is_empty());
        server.join().unwrap();
    }
    // The throttle is per thread under test, so the first 401 always warns, the second never.
    let logs = sink.0.lock().unwrap();
    assert_eq!(logs.len(), 1, "one 401 warning, no generic one: {logs:?}");
    assert!(logs[0].contains("API key rejected (HTTP 401)"), "{logs:?}");
}

// Serve `responses` in order (the last repeats) until `done` is set; returns the
// accepted-request counter. Lets a test count how many requests a lookup sent.
fn serve_counting(
    responses: Vec<Vec<u8>>,
) -> (
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    use std::sync::atomic::Ordering::SeqCst;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    super::TEST_API_BASE.with(|b| *b.borrow_mut() = Some(format!("http://{addr}")));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c, d) = (count.clone(), done.clone());
    let handle = std::thread::spawn(move || {
        while !d.load(SeqCst) {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let n = c.fetch_add(1, SeqCst);
            let mut request = [0; 2048];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&responses[n.min(responses.len() - 1)]);
        }
    });
    (count, done, handle)
}

fn status_response(status: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

// Requests `Client::lookup("HEAT_UE")` (two query variants) sends for `responses`.
fn lookup_requests(responses: Vec<Vec<u8>>) -> (usize, Result<Option<TmdbResult>, LookupFailed>) {
    use std::sync::atomic::Ordering::SeqCst;
    let (count, done, server) = serve_counting(responses);
    let sink = RecordingSink::default();
    let found = Client::new("KEY", &sink).lookup("HEAT_UE");
    done.store(true, SeqCst);
    server.join().unwrap();
    (count.load(SeqCst), found)
}

#[test]
fn low_lookup_stops_on_errors_another_query_cannot_fix() {
    for status in [
        "401 Unauthorized",
        "429 Too Many Requests",
        "503 Unavailable",
    ] {
        let (requests, found) = lookup_requests(vec![status_response(status)]);
        assert_eq!(requests, 1, "{status}");
        assert!(matches!(found, Err(LookupFailed(None))), "{status}");
    }
}

#[test]
fn low_lookup_stops_on_transport_failure() {
    let calls = Cell::new(0);
    let found = lookup_using("HEAT_UE", |_| {
        calls.set(calls.get() + 1);
        Err(Failure::Stop)
    });
    assert_eq!(calls.get(), 1);
    assert!(matches!(found, Err(LookupFailed(None))));
    let sink = RecordingSink::default();
    super::TEST_API_BASE.with(|b| *b.borrow_mut() = Some("api_key".to_string()));
    assert!(matches!(
        fetch_multi("Heat", "KEY", &sink),
        Err(Failure::Stop)
    ));
}

#[test]
fn low_lookup_continues_past_a_query_specific_status() {
    let hit = br#"{"results":[{"media_type":"movie","title":"Heat","release_date":"1995-12-15","id":9}]}"#;
    let mut ok = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        hit.len()
    )
    .into_bytes();
    ok.extend_from_slice(hit);
    for status in [
        "422 Unprocessable Entity",
        "404 Not Found",
        "400 Bad Request",
    ] {
        let (requests, found) = lookup_requests(vec![status_response(status), ok.clone()]);
        assert_eq!(requests, 2, "{status}");
        assert_eq!(found.unwrap().unwrap().tmdb_id, 9, "{status}");
    }
}
