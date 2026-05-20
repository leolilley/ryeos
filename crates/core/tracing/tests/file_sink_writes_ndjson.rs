//! PR1a Task 3 test: file sink writes valid ndjson lines.

use std::fs;

use tracing_subscriber::{Layer, prelude::*};

#[test]
fn file_sink_writes_ndjson() {
    ryeos_tracing::test::prime_callsites();

    let tmp = tempfile::tempdir().unwrap();
    let trace_path = tmp.path().join("trace-events.ndjson");

    // Use a fresh subscriber (test binary has no global subscriber yet)
    // We directly test the SharedFileWriter + json layer.
    let writer = std::sync::Arc::new(std::sync::Mutex::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_path)
            .unwrap(),
    ));

    let file_writer = ryeos_tracing::subscriber::SharedFileWriter::new(writer);

    let filter = tracing_subscriber::EnvFilter::new("info");
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_filter(filter);

    let _guard = tracing_subscriber::registry().with(file_layer).set_default();

    // Emit some events
    tracing::info!(message = "test_event_1", key = "value1");
    tracing::info_span!("test_span", span_field = "span_value").in_scope(|| {
        tracing::info!(message = "inside_span");
    });

    // Drop the guard to flush
    drop(_guard);

    // Read and verify
    let contents = fs::read_to_string(&trace_path).unwrap();
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();

    // Should have at least the two events plus span NEW/CLOSE
    assert!(lines.len() >= 2, "expected at least 2 ndjson lines, got {}", lines.len());

    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} is not valid JSON: {}\ncontent: {}", i, e, line));
        // Every ndjson line from tracing-subscriber must have these fields
        assert!(
            parsed.get("timestamp").is_some(),
            "line {} missing 'timestamp': {}",
            i,
            line
        );
        assert!(
            parsed.get("target").is_some(),
            "line {} missing 'target': {}",
            i,
            line
        );
    }

    // Check that our specific events appear
    let all_text = contents;
    assert!(all_text.contains("test_event_1"), "missing test_event_1 in trace output");
    assert!(all_text.contains("inside_span"), "missing inside_span in trace output");
}
