//! `ryeos external-execution status` and the doctor's `external_capacity` and
//! `credential_profiles` rows against a real daemon.
//!
//! A clean running node reports both rows ok through the live call. A stopped
//! node reports them `not_checked`. A dedicated session admitted on a profile
//! but never given a Worker is what a daemon that died mid-start leaves: the
//! next start marks it `outcome_unknown` and keeps the profile lock, and both
//! the status verb and the doctor name the session that holds it.

use ryeosd::test_support as common;

use std::process::Output;

use common::DaemonHarness;
use ryeos_app::state_store::{
    DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession,
};
use serde_json::Value;

fn ryeos(h: &DaemonHarness, args: &[&str]) -> Output {
    std::process::Command::new(common::ryeos_binary())
        .args(args)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", common::ryeosd_binary())
        .env("HOME", h.user_space.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn ryeos")
}

fn doctor_rows(h: &DaemonHarness) -> (Value, Value) {
    let output = ryeos(
        h,
        &[
            "node",
            "doctor",
            "--no-bundles",
            "--json",
            "--app-root",
            h.state_path.to_str().unwrap(),
        ],
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor printed no JSON ({error}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let row = |name: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("doctor has no {name} row: {report}"))
    };
    (row("external_capacity"), row("credential_profiles"))
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_doctor_name_a_held_profile_lock() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .expect("start fast daemon");

    // Running and clean: the live call answers and nothing is held.
    let (capacity, profiles) = doctor_rows(&h);
    assert_eq!(capacity["status"], "ok", "{capacity}");
    assert_eq!(profiles["status"], "ok", "{profiles}");
    assert_eq!(profiles["detail"]["held"], serde_json::json!([]));

    // Stopped: the rows are not checked.
    h.stop_gracefully().await.expect("stop daemon");
    let (capacity, profiles) = doctor_rows(&h);
    for row in [&capacity, &profiles] {
        assert_eq!(row["status"], "n/a", "{row}");
        assert_eq!(row["detail"]["state"], "not_checked", "{row}");
    }

    // A session admitted on the profile, whose Worker never attached.
    {
        let store = common::open_stopped_node_store(&h.state_path).expect("open stopped store");
        let operator = "fp:status-e2e";
        store
            .create_credential_profile(NewCredentialProfile {
                profile_id: "personal",
                owner_principal: operator,
                home_id: "status-e2e-home",
            })
            .unwrap();
        store
            .acquire_credential_profile("personal", operator, "worker-status-e2e")
            .unwrap();
        store
            .admit_dedicated_session(NewDedicatedSession {
                placement_thread_id: "T-status-e2e",
                chain_root_id: "T-status-e2e",
                owner_principal: operator,
                admitted_capsule_hash: &"a".repeat(64),
                workspace_id: "W-status-e2e",
                candidate_required: false,
                candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
                credential_profile_id: "personal",
                credential_generation: 1,
                credential_lock_owner: "worker-status-e2e",
            })
            .unwrap();
    }
    h.respawn_with(|_| {}).await.expect("restart daemon");

    let output = ryeos(&h, &["external-execution", "status", "--json"]);
    assert!(
        output.status.success(),
        "status exit={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "status printed no JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    let view = printed.get("result").unwrap_or(&printed);
    let held = view["credential_profiles"].as_array().expect("held profiles");
    assert_eq!(held.len(), 1, "{view}");
    assert_eq!(held[0]["profile_id"], "personal");
    assert_eq!(held[0]["owner"]["kind"], "dedicated_session");
    assert_eq!(held[0]["owner"]["chain_root_id"], "T-status-e2e");
    assert_eq!(held[0]["owner"]["session_state"], "outcome_unknown");
    assert_eq!(held[0]["attention"], "operator");
    assert!(
        held[0]["fix"]
            .as_str()
            .unwrap()
            .contains("ryeos worker session terminate T-status-e2e"),
        "{}",
        held[0]["fix"]
    );

    let (capacity, profiles) = doctor_rows(&h);
    assert_eq!(capacity["status"], "ok", "{capacity}");
    assert_eq!(profiles["status"], "fail", "{profiles}");
    assert_eq!(profiles["detail"]["counts"]["operator"], 1);
    assert_eq!(
        profiles["detail"]["held"][0]["owner"]["chain_root_id"],
        "T-status-e2e"
    );
}
