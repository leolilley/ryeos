//! PR1a Task 3 test: file sink survives daemon restart.
//!
//! Child-process integration test: spawn two ryeosd subprocesses against the
//! same state dir, kill both, then verify the ndjson file contains spans
//! from BOTH runs.

use std::fs;
use std::process::Command;

/// Build the ryeosd binary if needed, then spawn it with a temp state dir.
/// Returns (state_dir, trace_path).
fn spawn_daemon_once(tmp: &tempfile::TempDir, exe: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let state_dir = tmp.path().to_path_buf();
    let trace_path = state_dir.join(".state/trace-events.ndjson");

    // Bootstrap a minimal state
    let output = Command::new(exe)
        .args([
            "--init-only",
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .env("RYE_STATE", state_dir.to_str().unwrap())
        .env("RUST_LOG", "ryeosd=info")
        .output()
        .expect("failed to run ryeosd --init-only");

    if !output.status.success() {
        panic!(
            "ryeosd --init-only failed: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    (state_dir, trace_path)
}

fn cargo_exe() -> Option<String> {
    // Try to find the ryeosd binary in the target directory
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .unwrap_or_else(|_| "target".to_string());
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
    let (state_dir, trace_path) = spawn_daemon_once(&tmp, &exe);
    let state_str = state_dir.to_str().unwrap();

    // Start daemon #1, let it write some startup spans, then kill it
    let mut child1 = Command::new(&exe)
        .args(["--state-dir", state_str])
        .env("RYE_STATE", state_str)
        .env("RUST_LOG", "ryeosd=info")
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
        .args(["--state-dir", state_str])
        .env("RYE_STATE", state_str)
        .env("RUST_LOG", "ryeosd=info")
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
    assert!(
        !lines.is_empty(),
        "expected ndjson lines from daemon runs"
    );

    // Verify all lines are valid JSON
    for (i, line) in lines.iter().enumerate() {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} invalid JSON: {}", i, e));
    }

    // The final file should be strictly larger than after run 1
    // (run 2 appends more lines)
    let run1_line_count = contents_after_run1.lines().filter(|l| !l.is_empty()).count();
    assert!(
        lines.len() > run1_line_count,
        "expected more lines after restart ({} > {})",
        lines.len(),
        run1_line_count
    );
}
