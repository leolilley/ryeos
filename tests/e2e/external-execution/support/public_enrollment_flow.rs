//! Real-daemon, offline fixture enrollment using public authority owners only.
//!
//! Include alongside public_enrollment and retained_runtime_producer in the
//! daemon integration test. Requires current core/standard daemon artifacts and
//! native Linux x86_64 execution. No Codex executable or network/model account
//! is needed. This enrolls a synthetic fixture account, not real Codex access.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

use crate::common::{DaemonHarness, fast_fixture};
use crate::{public_enrollment, retained_runtime_producer};

pub const PROFILE_ID: &str = "credential:public-offline-enrollment";
const LOGIN_LAUNCH_ID: &str = "L-2b3e89703b173ebaf7195f4d672a1010";
const PRODUCER_LAUNCH_ID: &str = "L-2b3e89703b173ebaf7195f4d672a1009";

pub struct EnrolledPublicFixture {
    pub harness: DaemonHarness,
    pub keys: fast_fixture::FastFixture,
    pub producer_project: tempfile::TempDir,
    pub evidence: Value,
}

fn expected_runtime_manifest() -> Result<String> {
    crate::offline_runtime_input::bytes()?;
    Ok(crate::offline_runtime_input::MANIFEST_HASH.to_owned())
}

/// Author the one explicit trusted-hosted opt-in for this disposable fixture.
/// This permits a trusted process group, NOT qualified process-scope isolation.
/// Preserve every other policy value and verify the original node signature.
fn trusted_enrollment_policy(raw: &str, node: &lillux::crypto::SigningKey) -> Result<String> {
    let header = raw
        .lines()
        .next()
        .and_then(|line| lillux::signature::parse_signature_line(line, "#", None))
        .context("preboot isolation policy has no signature")?;
    let body = lillux::signature::strip_signature_lines(raw);
    let fingerprint = lillux::signature::compute_fingerprint(&node.verifying_key());
    ensure!(
        lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            &node.verifying_key(),
            &fingerprint
        ),
        "preboot isolation policy is not signed by this fixture node"
    );
    let mut document: Value = serde_yaml::from_str(&body)?;
    ensure!(
        document.pointer("/policy/mode") == Some(&json!("disabled")),
        "trusted enrollment fixture expects existing disabled isolation topology"
    );
    let flag = document
        .pointer_mut("/policy/trusted_process_group_sessions")
        .context("preboot isolation policy omits explicit trusted session policy")?;
    ensure!(flag.is_boolean(), "trusted session policy is not boolean");
    *flag = json!(true);
    Ok(lillux::signature::sign_content_at(
        &serde_yaml::to_string(&document)?,
        node,
        "#",
        None,
        fast_fixture::FAST_FIXTURE_TIME,
    ))
}

/// Register authored sources and explicitly authorize this fixture's trusted
/// session topology before boot/init sealing. Never called on an installed or
/// running node, and never shared with the independent input-probe fixture.
pub fn install_before_start(
    state: &Path,
    keys: &fast_fixture::FastFixture,
    runtime_manifest: &str,
) -> Result<()> {
    let root = state.join("public-enrollment-fixture");
    ensure!(!root.exists(), "fixture bundle path already exists");
    let isolation_path = state.join(".ai/node/policies/isolation.yaml");
    let policy = trusted_enrollment_policy(&std::fs::read_to_string(&isolation_path)?, &keys.node)?;
    std::fs::write(isolation_path, policy)?;
    let sources = public_enrollment::signed_sources(runtime_manifest, &keys.publisher)?;
    for (relative, bytes) in sources {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().context("fixture source parent")?)?;
        std::fs::write(path, bytes)?;
    }
    let manifest = "name: public-enrollment-fixture\nversion: 1.0.0\ndescription: Offline public credential ceremony fixture\nprovides_kinds: []\nrequires_kinds: [worker, worker_execution]\nuses_kinds: []\n";
    std::fs::write(
        root.join(".ai/manifest.yaml"),
        lillux::signature::sign_content(manifest, &keys.publisher, "#", None),
    )?;
    fast_fixture::register_presigned_fixture_bundle(state, "public-enrollment-fixture", &root, keys)
}

/// One isolated acceptance. Returns the still-running disposable harness so a
/// subsequent candidate test can reuse the SAME active profile and owner.
/// All runtime mutations occur once through authenticated public services.
pub async fn start_enrolled_fixture() -> Result<EnrolledPublicFixture> {
    ensure!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "offline fixture ELF has an exact Linux x86_64 target"
    );
    let expected_manifest = expected_runtime_manifest()?;
    let (mut harness, keys) = DaemonHarness::start_fast_with(
        |state, _, keys| {
            fast_fixture::register_standard_bundle(state, keys)?;
            install_before_start(state, keys, &expected_manifest)
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let mut producer_project = tempfile::tempdir()?;
    producer_project.disable_cleanup(true);
    eprintln!(
        "public enrollment fixture: node={}, producer={}, producer_launch={PRODUCER_LAUNCH_ID}, login_launch={LOGIN_LAUNCH_ID}",
        harness.state_path.display(),
        producer_project.path().display()
    );
    let evidence = enroll(&harness, &keys, producer_project.path(), &expected_manifest)
        .await.with_context(|| format!(
            "public enrollment stopped; retain node={}, producer={}, login_launch={LOGIN_LAUNCH_ID}; never relaunch uncertain work",
            harness.state_path.display(), producer_project.path().display()))?;
    Ok(EnrolledPublicFixture {
        harness,
        keys,
        producer_project,
        evidence,
    })
}

async fn enroll(
    harness: &DaemonHarness,
    keys: &fast_fixture::FastFixture,
    project: &Path,
    expected_manifest: &str,
) -> Result<Value> {
    let runtime = crate::offline_runtime_input::bytes()?;
    ensure!(
        !runtime.is_empty() && runtime.len() <= 32768,
        "runtime violates retained fixture bound"
    );
    retained_runtime_producer::write_retained_runtime_producer_sources(
        project,
        &keys.publisher,
        crate::RETAINED_RUNTIME_CAPTURE_RECIPE,
    )?;
    // Do not call the single-Codex-member helper: this exact runtime is bin/program.
    let bin = project.join("products/external-runtime/bin");
    std::fs::create_dir_all(&bin)?;
    let bin = lillux::PinnedDirectory::open(&bin)?.context("fixture runtime bin")?;
    ensure!(
        bin.atomic_create_regular_from_reader(
            std::ffi::OsStr::new("program"),
            &mut std::io::Cursor::new(&runtime),
            32768,
            0o755,
        )?
        .is_some(),
        "fixture runtime already exists"
    );
    let (producer_accepted, producer_detail) = crate::public_launch::completed_launch(
        harness,
        json!({
            "launch_id":PRODUCER_LAUNCH_ID,
            "item_ref":retained_runtime_producer::PRODUCER_REF,
            "project_path":project,"ref_bindings":{},"parameters":{},
            "execution_policy":ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted)
                .exclude_operator_vault(),
        }),
        PRODUCER_LAUNCH_ID,
    )
    .await?;
    let producer_root = producer_accepted["thread_id"]
        .as_str()
        .context("accepted producer root")?;
    ensure!(
        producer_detail.pointer("/thread/thread_id") == Some(&json!(producer_root))
            && producer_detail.pointer("/thread/chain_root_id") == Some(&json!(producer_root))
            && producer_detail.pointer("/thread/status") == Some(&json!("completed")),
        "retained producer terminal coordinates contradict accepted root"
    );
    let producer = crate::RetainedRuntimeProducer {
        chain_root_id: producer_root.to_owned(),
        thread_id: producer_root.to_owned(),
    };
    eprintln!(
        "public enrollment producer root={}, thread={}",
        producer.chain_root_id, producer.thread_id
    );
    let captured = tokio::time::timeout(
        Duration::from_secs(60),
        crate::capture_retained_runtime(harness, &producer),
    )
    .await
    .context("capture observation timeout; retain producer")??;
    let evidence: ryeos_state::external_content::products::ProductCaptureEvidence =
        serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    crate::offline_runtime_input::verify_capture(
        &evidence,
        &producer.thread_id,
        &producer.chain_root_id,
        &format!("fp:{}", keys.user_fp()),
    )?;
    ensure!(
        expected_manifest == crate::offline_runtime_input::MANIFEST_HASH,
        "enrollment consumer selected a different authored fixture pin"
    );
    let imported = crate::production_service(
        harness,
        "service:external-content/import",
        json!({
            "source":"retained_product", "witness_hash":captured["witness_hash"],
            "witness_source":{"kind":"local_capture"}, "maximum_bytes":32768,
        }),
    )
    .await?;
    ensure!(
        imported["manifest_hash"] == expected_manifest,
        "import changed captured manifest"
    );
    let bound = crate::production_service(
        harness,
        "service:external-content/bind",
        json!({
            "staging_id":imported["staging_id"], "request_digest":imported["request_digest"],
            "manifest_hash":expected_manifest, "consumer_ref":public_enrollment::WORKER_REF,
            "consumer_kind":"installed_bundle",
        }),
    )
    .await?;
    ensure!(
        bound["manifest_hash"] == expected_manifest
            && bound["consumer_ref"] == public_enrollment::WORKER_REF,
        "runtime binding selected another enrollment worker"
    );
    let created = tokio::time::timeout(
        Duration::from_secs(30),
        public_enrollment::create_profile(harness, PROFILE_ID),
    )
    .await
    .context("profile creation observation timeout; do not retry")??;
    let accepted = tokio::time::timeout(
        Duration::from_secs(60),
        public_enrollment::launch_login(harness, PROFILE_ID, LOGIN_LAUNCH_ID),
    )
    .await
    .context("login acceptance uncertain; retain launch ID, do not relaunch")??;
    let root = accepted["thread_id"]
        .as_str()
        .context("accepted login root")?;
    eprintln!("public enrollment accepted login root={root}, launch={LOGIN_LAUNCH_ID}");
    let session = wait_ready(harness, root).await?;
    let capsule = session["admitted_capsule_hash"]
        .as_str()
        .context("session capsule")?;
    ensure!(
        lillux::valid_hash(capsule),
        "invalid admitted session capsule"
    );
    let mut observations = Vec::new();
    for (sequence, key, route) in [
        (
            1_u64,
            "public-enrollment-start-v1",
            "credential.login.start",
        ),
        (2, "public-enrollment-account-v1", "credential.account.read"),
    ] {
        let command = tokio::time::timeout(
            Duration::from_secs(30),
            public_enrollment::command(harness, root, key, route),
        )
        .await
        .with_context(|| {
            format!("command observation timeout root={root}, key={key}; do not retry")
        })??;
        ensure!(
            command["chain_root_id"] == root
                && command["placement_thread_id"] == root
                && command["command_sequence"] == sequence
                && command["state"] == "completed",
            "enrollment command has contradictory settled coordinates: {command}"
        );
        let observed = crate::production_service(
            harness,
            "service:worker-executions/command-observation",
            json!({"chain_root_id":root,"placement_thread_id":root,"command_sequence":sequence}),
        )
        .await?;
        verify_observation(&command, &observed, root, capsule, sequence, key, route)?;
        observations.push(observed);
    }
    let terminated = crate::production_service(
        harness,
        "service:worker-executions/terminate",
        json!({"chain_root_id":root,"reason":"cancelled"}),
    )
    .await?;
    ensure!(
        terminated["chain_root_id"] == root
            && terminated["state"] == "terminal"
            && terminated["reason"] == "cancelled",
        "login termination did not settle exact root"
    );
    let terminal = crate::production_service(
        harness,
        "service:worker-executions/status",
        json!({"chain_root_id":root}),
    )
    .await?;
    ensure!(
        terminal["placement_thread_id"] == root
            && terminal["state"] == "terminal"
            && terminal["terminal_reason"] == "cancelled"
            && terminal["admitted_capsule_hash"] == capsule,
        "login terminal projection does not preserve its exact session"
    );
    let confirmed = tokio::time::timeout(
        Duration::from_secs(30),
        public_enrollment::confirm_observed_profile(harness, PROFILE_ID),
    )
    .await
    .context("confirmation observation timeout; do not repeat mutation")??;
    let profile = crate::production_service(
        harness,
        "service:credential-profiles/get",
        json!({"profile_id":PROFILE_ID}),
    )
    .await?;
    ensure!(
        profile["owner_principal"] == format!("fp:{}", keys.user_fp())
            && profile["state"] == "active"
            && profile["credential_generation"] == confirmed["credential_generation"]
            && profile.get("lock_owner") == Some(&Value::Null),
        "enrolled profile owner/generation/settlement mismatch"
    );
    Ok(
        json!({"scope":"public_daemon_offline_enrollment", "producer_root":producer.chain_root_id,
        "producer_accepted":producer_accepted,"producer_terminal":producer_detail,
        "producer_thread":producer.thread_id,"capture":captured,"import":imported,"binding":bound,
        "profile_created":created,"accepted":accepted,"session":session,
        "command_observations":observations,"termination":terminated,"terminal":terminal,
        "confirmation":confirmed,"profile":profile,"paid_model_contact":false,
        "render_contact":false,"real_codex_account":false}),
    )
}

async fn wait_ready(harness: &DaemonHarness, root: &str) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let (status, response) = harness
                .post_execute(
                    "service:worker-executions/status",
                    ".",
                    json!({"chain_root_id":root}),
                )
                .await?;
            if status.is_success() {
                let session = response.get("result").context("status result")?;
                ensure!(
                    session["chain_root_id"] == root && session["placement_thread_id"] == root,
                    "login status changed authoritative root/placement"
                );
                match session["state"].as_str() {
                    Some("idle") => return Ok(session.clone()),
                    Some("admitted" | "binding") => {}
                    _ => anyhow::bail!("login reached unexpected state: {session}"),
                }
            } else {
                ensure!(
                    status == reqwest::StatusCode::NOT_FOUND,
                    "login status read failed: {status}: {response}"
                );
            }
            // Poll only this accepted root; never retry a launch or command.
            let thread = crate::production_service(
                harness,
                "service:threads/get",
                json!({"thread_id":root}),
            )
            .await?;
            let status = thread
                .pointer("/thread/status")
                .and_then(Value::as_str)
                .context("accepted login exact thread status")?;
            ensure!(
                !ryeos_state::objects::ThreadStatus::from_str_lossy(status)
                    .context("unknown accepted login thread status")?
                    .is_terminal(),
                "login root became terminal before readiness: {thread}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .with_context(|| format!("accepted login root {root} did not become ready; do not relaunch"))?
}

fn verify_observation(
    command: &Value,
    observation: &Value,
    root: &str,
    capsule: &str,
    sequence: u64,
    key: &str,
    route: &str,
) -> Result<()> {
    let request_digest = ryeos_state::objects::canonical_value_digest(&json!({
        "command_kind":"route", "payload":{"route_id":route,"payload":{}}
    }))?;
    let response_digest = ryeos_state::objects::canonical_value_digest(&command["result"])?;
    ensure!(
        observation["chain_root_id"] == root
            && observation["placement_thread_id"] == root
            && observation["command_sequence"] == sequence
            && observation["command_state"] == "completed"
            && observation["admitted_capsule_hash"] == capsule
            && observation["route_id"] == route
            && observation["idempotency_key"] == key
            && observation["request_digest"] == request_digest
            && observation["response_digest"] == response_digest
            && observation.get("operation") == Some(&Value::Null)
            && observation.get("completion_fence").is_none(),
        "enrollment command observation contradicts exact settled command: {observation}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_preboot_opt_in_changes_only_trusted_session_flag_and_remains_node_signed() {
        let profile: Value = serde_yaml::from_str(include_str!(
            "../../../../bundles/.ai/node/init/profiles/full.yaml"
        ))
        .unwrap();
        let original = profile["policies"]["isolation"].clone();
        assert_eq!(original["policy"]["trusted_process_group_sessions"], false);
        let node = fast_fixture::node_signing_key();
        let raw = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&original).unwrap(),
            &node,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        );
        let signed = trusted_enrollment_policy(&raw, &node).unwrap();
        let body = lillux::signature::strip_signature_lines(&signed);
        let mut observed: Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(observed["policy"]["trusted_process_group_sessions"], true);
        observed["policy"]["trusted_process_group_sessions"] = json!(false);
        assert_eq!(observed, original, "fixture widened another policy field");
        let header =
            lillux::signature::parse_signature_line(signed.lines().next().unwrap(), "#", None)
                .unwrap();
        assert!(lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            &node.verifying_key(),
            &lillux::signature::compute_fingerprint(&node.verifying_key())
        ));
        assert!(trusted_enrollment_policy(&raw, &fast_fixture::publisher_signing_key()).is_err());
        assert!(trusted_enrollment_policy(&body, &node).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires current signed core/standard daemon artifacts and native Linux x86_64 execution"]
    async fn public_offline_enrollment_observes_then_confirms_exact_profile() -> Result<()> {
        let fixture = start_enrolled_fixture().await?;
        ensure!(
            fixture.evidence["profile"]["state"] == "active"
                && fixture.evidence["profile"]["profile_id"] == PROFILE_ID,
            "public enrollment did not retain its exact active profile"
        );
        eprintln!("public-offline-enrollment-evidence: {}", fixture.evidence);
        Ok(())
    }

    #[test]
    fn enrollment_authoring_keeps_current_signed_session_and_two_entry_runtime_pin() {
        let manifest = expected_runtime_manifest().unwrap();
        let files = public_enrollment::signed_sources(
            &manifest,
            &lillux::crypto::SigningKey::from_bytes(&[42; 32]),
        )
        .unwrap();
        let worker: Value =
            serde_yaml::from_slice(&files[".ai/workers/fixture/enrollment.yaml"]).unwrap();
        assert_eq!(worker["external_content"][0]["digest"], manifest);
        assert_eq!(
            worker["execution_protocol"],
            "protocol:ryeos/core/trusted_structured_session"
        );
        assert_eq!(worker["filesystem_authority"], "node_policy");
        assert_eq!(worker["source"]["entry"], "profile.json");
        let login: Value =
            serde_yaml::from_slice(&files[".ai/worker-executions/fixture/login.yaml"]).unwrap();
        assert_eq!(login["config"]["mode"]["kind"], "session");
        assert_eq!(login["config"]["required_credential_state"], "any");
        assert_eq!(login["config"]["worker_ref"], public_enrollment::WORKER_REF);
        let profile: Value =
            serde_json::from_slice(&files[".ai/workers/fixture/lib/hosted/profile.json"]).unwrap();
        assert_eq!(profile["workload_executable"], "bin/program");
        assert_eq!(profile["external_candidate"], Value::Null);
        assert_eq!(profile["routes"].as_array().unwrap().len(), 2);
        assert_ne!(LOGIN_LAUNCH_ID, PRODUCER_LAUNCH_ID);
    }

    #[test]
    fn enrollment_observation_rejects_changed_coordinates_and_testimony() {
        let root = "T-2b3e8970-3b17-3eba-f719-5f4d672a1010";
        let capsule = "a".repeat(64);
        let route = "credential.login.start";
        let command = json!({"result":{"login_id":"fixture-login-v1"}});
        let observation = json!({
            "chain_root_id":root,"placement_thread_id":root,"command_sequence":1,
            "command_state":"completed","admitted_capsule_hash":capsule,
            "route_id":route,"idempotency_key":"key",
            "request_digest":ryeos_state::objects::canonical_value_digest(&json!({
                "command_kind":"route","payload":{"route_id":route,"payload":{}}
            })).unwrap(),
            "response_digest":ryeos_state::objects::canonical_value_digest(&command["result"]).unwrap(),
            "operation":null,
        });
        assert!(
            verify_observation(&command, &observation, root, &capsule, 1, "key", route).is_ok()
        );
        for (field, value) in [
            ("command_sequence", json!(2)),
            ("chain_root_id", json!("other")),
            ("admitted_capsule_hash", json!("b".repeat(64))),
            ("response_digest", json!("c".repeat(64))),
            ("operation", json!({"kind":"turn"})),
            ("completion_fence", json!({})),
        ] {
            let mut changed = observation.clone();
            changed[field] = value;
            assert!(
                verify_observation(&command, &changed, root, &capsule, 1, "key", route).is_err()
            );
        }
    }
}
