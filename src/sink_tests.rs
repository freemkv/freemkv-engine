use super::*;

#[test]
fn noop_sink_is_send_sync_and_never_cancels() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NoopSink>();
    assert!(!NoopSink.should_cancel());
}

#[test]
fn noop_sink_defaults_swallow_every_call() {
    // Exercises the default method bodies — no panic, no output.
    let s = NoopSink;
    s.log(Level::Info, "hello");
    let p = Progress {
        pass: std::borrow::Cow::Borrowed("sweep"),
        bytes_done: 10,
        bytes_total: 100,
        ..Default::default()
    };
    s.progress(&p);
    s.event(&Event::Phase { name: "mux" });
    s.completed(&crate::Outcome {
        files: Vec::new(),
        unreadable_bytes: 0,
        lost_ms: 0.0,
        severity: crate::DamageSeverity::Clean,
        elapsed_secs: 0.0,
        avg_bps: 0,
    });
}

#[test]
fn engine_sink_is_object_safe() {
    // The engine takes `&dyn Sink`; prove the trait is object-safe.
    let s = NoopSink;
    let dynref: &dyn Sink = &s;
    assert!(!dynref.should_cancel());
}

// Every phase name the engine emits is listed on `Event::Phase`, derived from the source so
// a new phase cannot go undocumented (a front end maps only the documented names).
#[test]
fn every_emitted_phase_is_documented() {
    let doc = include_str!("sink.rs")
        .split_once("A stage of the operation began")
        .and_then(|(_, rest)| rest.split_once("Phase {"))
        .expect("the Phase doc")
        .0;
    let mut emitted = Vec::new();
    for src in [include_str!("plan.rs"), include_str!("remux.rs")] {
        for piece in src.split("Event::Phase { name: \"").skip(1) {
            emitted.push(piece.split('"').next().unwrap());
        }
    }
    assert!(emitted.len() >= 8, "parsed {emitted:?}");
    for name in emitted {
        assert!(
            doc.contains(&format!("`\"{name}\"`")),
            "{name} missing: {doc}"
        );
    }
}
