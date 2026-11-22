//! Every place the executor tells a tool or runtime where to call back must
//! hand it the callback-only socket, never the control socket whose
//! unauthenticated lifecycle and health reads would leak node paths into a
//! sandbox. The sites are large async launch paths that cannot be driven in
//! isolation, so this pins them at the source: each named file keeps exactly
//! the expected number of callback-socket handoffs, all through
//! `Config::runtime_uds_path`, and none through the control `uds_path`.

fn squeezed(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[test]
fn executor_hands_tools_only_the_runtime_socket() {
    // compose_context_positions has the envelope callback, the
    // RYEOSD_SOCKET_PATH value and the isolation pin (through one local);
    // runner has the isolation pin and the RYEOSD_SOCKET_PATH value.
    for (file, expected) in [
        ("src/augmentations/compose_context_positions.rs", 3),
        ("src/dispatch.rs", 1),
        ("src/execution/launch.rs", 1),
        ("src/execution/runner.rs", 2),
    ] {
        let source = squeezed(file);
        assert!(
            !source.contains("config.uds_path"),
            "{file} reads the control socket path"
        );
        assert_eq!(
            source.matches("config.runtime_uds_path()").count(),
            expected,
            "{file}: callback socket handoffs"
        );
    }
}
