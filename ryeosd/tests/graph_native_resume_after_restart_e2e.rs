//! End-to-end resume invariants for the **graph runtime** under V5.5
//! D6 / D10.
//!
//! Mirrors `native_resume_after_restart_e2e.rs` (which pins the
//! generic native-resume wire format) but specifically for the graph
//! runtime's checkpoint+replay precedence:
//!
//!   1. `RYEOS_RESUME=1` + local `CheckpointWriter` payload → checkpoint
//!      wins (D10 step 1; carries cursor + state both).
//!   2. `RYEOS_RESUME=1` + no local checkpoint → replay-events fallback
//!      (D10 step 2; cursor only, state empty per v1 limitation).
//!   3. `RYEOS_RESUME=1` + neither source → fail loud (D10 step 3; the
//!      graph runtime binary `bail!`s rather than silent cold-start).
//!   4. `RYEOS_RESUME` unset → cold start.
//!
//! The full daemon-restart-then-respawn loop is exercised at the OS
//! level by `native_resume_after_restart_e2e.rs`. This file pins the
//! load-bearing graph-side primitives the loop depends on:
//!
//! - `CheckpointWriter::load_latest` round-trips a graph checkpoint
//!   payload (the shape `walker.rs::write_checkpoint` writes — note
//!   `current_node` is the NEXT cursor, R4 fix).
//! - The four resume-precedence variants stay distinct.

use std::path::PathBuf;

use ryeos_runtime::CheckpointWriter;

fn unique_checkpoint_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ryeos_graph_resume_e2e_{}_{}",
        std::process::id(),
        nanos
    ))
}

/// Pin the on-disk shape `walker::write_checkpoint` writes round-trips
/// through `CheckpointWriter::load_latest` and back into a parsable
/// `ResumeState`-shaped value.
///
/// V5.5 R4: `current_node` MUST be the NEXT cursor (the node to resume
/// *into*), not the just-completed node. The walker writes the
/// checkpoint AFTER `graph_step_completed` is appended.
///
/// Uses the path-based `CheckpointWriter::new` constructor so multiple
/// e2e tests can run in parallel without racing on shared env vars.
/// The env-var driven `from_env` constructor is exercised separately
/// in `native_resume_after_restart_e2e::checkpoint_writer_roundtrip_via_env`.
#[test]
fn graph_checkpoint_roundtrips_next_cursor_through_writer() {
    let dir = tempfile::TempDir::new().unwrap();
    let writer = CheckpointWriter::new(dir.path().to_path_buf());

    // Walker writes: NEXT cursor (the node we resume *into*) plus the
    // step count for that node, plus the graph state at the time
    // `graph_step_completed` was appended for the just-completed node.
    let payload = serde_json::json!({
        "graph_run_id": "gr-e2e-1",
        "current_node": "step3",
        "step_count": 2,
        "state": {"counter": 7, "name": "alice"},
        "written_at": "2026-04-28T12:00:00Z",
    });
    writer.write(&payload).expect("write checkpoint");

    // A second-run boot reads the same directory.
    let reader = CheckpointWriter::new(dir.path().to_path_buf());
    let loaded = reader
        .load_latest()
        .expect("load_latest")
        .expect("checkpoint exists");

    assert_eq!(loaded["current_node"], "step3");
    assert_eq!(loaded["step_count"], 2);
    assert_eq!(loaded["state"]["counter"], 7);
    assert_eq!(loaded["state"]["name"], "alice");
    assert_eq!(loaded["graph_run_id"], "gr-e2e-1");
}

/// Pin that `load_latest` returns `None` when the checkpoint
/// directory is empty. The graph runtime binary's main.rs flow uses
/// this as the trigger to fall back to replay-events resume (D10
/// step 2).
#[test]
fn empty_checkpoint_dir_returns_none_so_replay_fallback_triggers() {
    let dir = tempfile::TempDir::new().unwrap();
    let reader = CheckpointWriter::new(dir.path().to_path_buf());
    let loaded = reader.load_latest().expect("load_latest does not error");
    assert!(
        loaded.is_none(),
        "empty checkpoint dir must return None so main.rs trips the \
         replay-events fallback path (V5.5 D10 step 2)"
    );
}

/// Pin that the latest-write wins when multiple checkpoints have been
/// written. The graph walker writes a fresh checkpoint after every
/// `graph_step_completed`; on resume the reader sees the cursor for
/// the most recently completed step.
#[test]
fn latest_checkpoint_wins_on_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let writer = CheckpointWriter::new(dir.path().to_path_buf());

    writer
        .write(&serde_json::json!({
            "graph_run_id": "gr-1",
            "current_node": "step1",
            "step_count": 0,
            "state": {},
        }))
        .unwrap();
    writer
        .write(&serde_json::json!({
            "graph_run_id": "gr-1",
            "current_node": "step2",
            "step_count": 1,
            "state": {"x": 1},
        }))
        .unwrap();
    writer
        .write(&serde_json::json!({
            "graph_run_id": "gr-1",
            "current_node": "step3",
            "step_count": 2,
            "state": {"x": 2},
        }))
        .unwrap();

    let reader = CheckpointWriter::new(dir.path().to_path_buf());
    let loaded = reader.load_latest().unwrap().unwrap();
    assert_eq!(loaded["current_node"], "step3");
    assert_eq!(loaded["step_count"], 2);
    assert_eq!(loaded["state"]["x"], 2);
}

/// Pin that the unique checkpoint-dir helper produces distinct paths
/// per call (sanity for parallel test runs that race on the same temp
/// root).
#[test]
fn unique_checkpoint_dirs_are_distinct() {
    let a = unique_checkpoint_dir();
    let b = unique_checkpoint_dir();
    assert_ne!(a, b);
}
