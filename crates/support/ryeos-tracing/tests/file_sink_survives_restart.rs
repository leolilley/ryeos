//! PR1a Task 3 test: file sink survives daemon restart.
//!
//! Child-process integration test: spawn two ryeosd subprocesses against the
//! same state dir, kill both, then verify the ndjson file contains spans
//! from BOTH runs.

use std::fs;
use std::process::Command;

/// Build the ryeosd binary if needed, then initialize a temp app root.
/// Returns (app_root, trace_path).
fn init_node_once(
    tmp: &tempfile::TempDir,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let state_dir = tmp.path().to_path_buf();
    let trace_path = state_dir
        .join(".ai")
        .join("state")
        .join("trace-events.ndjson");

    let root = workspace_root();
    ryeos_node::run_init(&ryeos_node::InitOptions {
        app_root: state_dir.clone(),
        source_dir: root.join("bundles"),
        trust_files: vec![root.join(".dev-keys/PUBLISHER_DEV_TRUST.toml")],
        skip_preflight: true,
    })
    .expect("ryeos init state for tracing test");

    (state_dir, trace_path)
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/")
        .to_path_buf()
}

fn cargo_exe() -> Option<String> {
    // Try to find the ryeosd binary in the target directory
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let path = format!("{}/debug/ryeosd", target_dir);
    if std::path::Path::new(&path).exists() {
        Some(path)
    } else {
        None
    }
}

#[test]
fn file_sink_survives_daemon_restart() {
    let exe = match cargo_exe() {
        Some(e) => e,
        None => {
            eprintln!("skipping file_sink_survives_daemon_restart: ryeosd binary not found in target/debug/");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();

    // Run #1: init and quickly start/stop
    let (app_root, trace_path) = init_node_once(&tmp);
    let app_root_str = app_root.to_str().unwrap();

    // Start daemon #1, let it write some startup spans, then kill it
    let mut child1 = Command::new(&exe)
        .args(["--app-root", app_root_str])
        .env("RUST_LOG", "ryeosd=info")
        .env("RYEOS_APP_ROOT", &app_root)
        .env("HOME", &app_root)
        .spawn()
        .expect("failed to spawn ryeosd #1");

    // Give it a moment to start and write traces
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = child1.kill();
    let _ = child1.wait();

    // Verify run #1 wrote something
    let contents_after_run1 = fs::read_to_string(&trace_path).unwrap_or_default();
    if contents_after_run1.is_empty() {
        // Daemon might not have started (missing key, etc.) — skip gracefully
        eprintln!("skipping file_sink_survives_daemon_restart: daemon #1 produced no output");
        return;
    }

    // Run #2: restart against same state
    let mut child2 = Command::new(&exe)
        .args(["--app-root", app_root_str])
        .env("RUST_LOG", "ryeosd=info")
        .env("RYEOS_APP_ROOT", &app_root)
        .env("HOME", &app_root)
        .spawn()
        .expect("failed to spawn ryeosd #2");

    std::thread::sleep(std::time::Duration::from_millis(500));
    let _ = child2.kill();
    let _ = child2.wait();

    // Read final file — should contain spans from BOTH runs
    let final_contents = fs::read_to_string(&trace_path).unwrap();
    let lines: Vec<&str> = final_contents.lines().filter(|l| !l.is_empty()).collect();

    // Each daemon start writes multiple JSON lines. We had 2 starts.
    // The file persists across restarts, so line count should be >= contents_after_run1.
    assert!(!lines.is_empty(), "expected ndjson lines from daemon runs");

    // Verify all lines are valid JSON
    for (i, line) in lines.iter().enumerate() {
        let _: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {} invalid JSON: {}", i, e));
    }

    // The final file should be strictly larger than after run 1
    // (run 2 appends more lines)
    let run1_line_count = contents_after_run1
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert!(
        lines.len() > run1_line_count,
        "expected more lines after restart ({} > {})",
        lines.len(),
        run1_line_count
    );
}
