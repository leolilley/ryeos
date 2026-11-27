//! The on-disk trace sink never records signature-verification spans below
//! `warn`, whatever the filter says, and its total size stays small.

use tracing_subscriber::filter::{FilterExt as _, filter_fn};
use tracing_subscriber::{Layer, prelude::*};

use ryeos_tracing::subscriber::{
    SharedFileWriter, TRACE_ROTATE_BYTES, TRUST_TARGET, keeps_in_file_sink,
};

#[test]
fn trust_spans_stay_out_of_the_file_even_when_the_filter_asks_for_them() {
    ryeos_tracing::test::prime_callsites();
    let tmp = tempfile::tempdir().unwrap();
    let trace_path = tmp.path().join("trace-events.ndjson");
    let writer = SharedFileWriter::open(&trace_path, TRACE_ROTATE_BYTES).unwrap();

    // A filter that asks for everything, as `RUST_LOG=trace` would.
    let everything = tracing_subscriber::EnvFilter::new("trace");
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(writer)
        .with_ansi(false)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_filter(everything.and(filter_fn(keeps_in_file_sink)));
    let guard = tracing_subscriber::registry()
        .with(file_layer)
        .set_default();

    tracing::debug_span!(target: TRUST_TARGET, "engine:trust_verify").in_scope(|| {
        tracing::debug!(target: TRUST_TARGET, "checked a signature");
    });
    tracing::warn!(target: TRUST_TARGET, "signature refused");
    tracing::info_span!("other_span").in_scope(|| {
        tracing::info!("kept");
    });
    drop(guard);

    let contents = std::fs::read_to_string(&trace_path).unwrap();
    assert!(!contents.contains("engine:trust_verify"), "{contents}");
    assert!(!contents.contains("checked a signature"), "{contents}");
    assert!(contents.contains("signature refused"), "{contents}");
    assert!(contents.contains("other_span"), "{contents}");
}

#[test]
fn the_sink_with_its_one_rotated_copy_stays_under_64_mib() {
    const { assert!(2 * TRACE_ROTATE_BYTES <= 64 * 1024 * 1024) };
}
