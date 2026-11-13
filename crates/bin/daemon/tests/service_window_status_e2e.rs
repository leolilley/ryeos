//! The running node reports what remains of its service window before the
//! planned rollover, through `ryeos node status --json` and `ryeos node doctor`.
//!
//! The fixture's signed policy is rewritten to a 600 s window with a 60 s
//! cleanup occupancy and a 60 s drain margin: the service edge is 540 s after
//! the window opens and the rollover begins 60 s before it, at 480 s. The
//! reported seconds must be measured to that rollover point, not to the edge,
//! and must count down.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture;
use lillux::crypto::SigningKey;
use serde_json::Value;

const MAXIMUM_OCCUPANCY_MS: u64 = 600_000;
const CLEANUP_OCCUPANCY_MS: u64 = 60_000;
const DRAIN_MARGIN_MS: u64 = 60_000;

fn set_service_window(state_path: &Path, node: &SigningKey) -> anyhow::Result<()> {
    let path = state_path.join(".ai/node/policies/native_capacity.yaml");
    let raw = std::fs::read_to_string(&path)?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let mut policy: serde_yaml::Value = serde_yaml::from_str(&body)?;
    let incarnation = policy["incarnation"]
        .as_mapping_mut()
        .expect("incarnation envelope");
    incarnation.insert("maximum_occupancy_ms".into(), MAXIMUM_OCCUPANCY_MS.into());
    incarnation.insert("cleanup_occupancy_ms".into(), CLEANUP_OCCUPANCY_MS.into());
    incarnation.insert("rollover_drain_margin_ms".into(), DRAIN_MARGIN_MS.into());
    let body = serde_yaml::to_string(&policy)?;
    let signed =
        lillux::signature::sign_content_at(&body, node, "#", None, fast_fixture::FAST_FIXTURE_TIME);
    std::fs::write(&path, signed)?;
    Ok(())
}

fn ryeos_json(harness: &DaemonHarness, args: &[&str]) -> Value {
    let output = std::process::Command::new(common::ryeos_binary())
        .args(args)
        .arg("--json")
        .arg("--app-root")
        .arg(&harness.state_path)
        .env("RYEOS_APP_ROOT", &harness.state_path)
        .env("RYEOSD_BIN", common::ryeosd_binary())
        .env("HOME", harness.user_space.path())
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ryeos");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "ryeos {args:?} --json printed no JSON ({error}); exit={:?}\nstdout={}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_doctor_report_the_service_window_to_the_rollover() {
    let (mut harness, _fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| set_service_window(state_path, &fixture.node),
        |_| {},
    )
    .await
    .expect("start fast daemon with a 600 s window");

    let until_rollover = (MAXIMUM_OCCUPANCY_MS - CLEANUP_OCCUPANCY_MS - DRAIN_MARGIN_MS) / 1_000;
    let first = ryeos_json(&harness, &["node", "status"]);
    let window = &first["Running"]["service_window"];
    let seconds = window["seconds_until_rollover"]
        .as_u64()
        .unwrap_or_else(|| panic!("running status without a service window: {first:#}"));
    assert!(
        seconds <= until_rollover && seconds + 120 > until_rollover,
        "the window runs to the drain margin ({until_rollover} s), not the edge: {window:#}"
    );
    assert!(
        window["work_steps_until_rollover"].as_u64().unwrap() > 0,
        "{window:#}"
    );
    assert!(
        window["rolls_at"].as_str().unwrap().ends_with('Z'),
        "{window:#}"
    );

    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let later = ryeos_json(&harness, &["node", "status"]);
    let later_seconds = later["Running"]["service_window"]["seconds_until_rollover"]
        .as_u64()
        .expect("service window on the second read");
    assert!(
        later_seconds < seconds,
        "the window counts down: {seconds} then {later_seconds}"
    );

    let doctor = ryeos_json(&harness, &["node", "doctor", "--no-bundles"]);
    let row = doctor["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "service_window")
        .unwrap_or_else(|| panic!("doctor without a service_window row: {doctor:#}"))
        .clone();
    assert_eq!(row["status"], "ok", "{row:#}");
    assert!(
        row["detail"]["seconds_until_rollover"].as_u64().unwrap() <= until_rollover,
        "{row:#}"
    );

    harness.stop_gracefully().await.expect("orderly stop");
    let doctor = ryeos_json(&harness, &["node", "doctor", "--no-bundles"]);
    let row = doctor["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "service_window")
        .unwrap_or_else(|| panic!("doctor without a service_window row: {doctor:#}"))
        .clone();
    assert_eq!(row["status"], "n/a", "{row:#}");
}
