//! Trusted local input-admission probe. No confinement, evaluator outcome,
//! runtime qualification, external placement, or Farm acceptance is asserted.

use crate::common::{DaemonHarness, fast_fixture};
use crate::{public_launch, retained_runtime_producer};
use anyhow::{Context as _, Result, ensure};
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use serde_json::{Value, json};
use std::{ffi::OsStr, path::Path, time::Duration};

const TOOL: &str = "tool:fixtures/runtime-input/probe";
const MEMBER: &str = "qualification/subject/bin/program";
const PRODUCER_LAUNCH: &str = "L-3d3a12f19cf34968a9ad0e3ccb971218";
const PROBE_LAUNCH: &str = "L-bb8e4a82a59942c694345935e8554e3c";

fn runtime_input() -> Result<(Vec<u8>, String)> {
    Ok((
        crate::offline_runtime_input::bytes()?,
        crate::offline_runtime_input::MANIFEST_HASH.to_owned(),
    ))
}

fn tool(binary_ref: &str, manifest: &str, runtime_hash: &str) -> Value {
    json!({
        "category":"fixtures/runtime-input","name":"probe","version":"1.0.0",
        "description":"Trusted local retained-input observation, not qualification",
        "executor_id":"@subprocess","execution_protocol":"protocol:ryeos/core/opaque",
        "effects":"live","filesystem_authority":"node_policy","network_authority":"node_policy",
        "external_content":[{"id":"subject","kind":"tree","mode":"pinned","digest":manifest,
            "mount_root":"project","mount":"qualification/subject"}],
        "config":{"command":binary_ref,"args":[MEMBER,runtime_hash],"input_data":"{}","timeout_secs":30},
        "config_schema":{"type":"object","properties":{},"additionalProperties":false}
    })
}

fn install(
    state: &Path,
    keys: &fast_fixture::FastFixture,
    probe: &[u8],
    manifest: &str,
    runtime_hash: &str,
) -> Result<()> {
    // Inspect, never widen, the preboot fixture's signed trusted/disabled lane.
    let isolation = std::fs::read_to_string(state.join(".ai/node/policies/isolation.yaml"))?;
    let policy: Value =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&isolation))?;
    ensure!(
        policy.pointer("/policy/mode") == Some(&json!("disabled")),
        "input probe requires existing disabled trusted fixture lane; no policy override"
    );
    let root = state.join("public-runtime-input-fixture");
    ensure!(!root.exists(), "input probe bundle already exists");
    let binary = fast_fixture::install_signed_bundle_binary(
        &root,
        "runtime-input-probe",
        probe,
        &keys.publisher,
    )?;
    ensure!(
        binary.ends_with("/runtime-input-probe"),
        "installed probe coordinate differs"
    );
    let path = root.join(".ai/tools/fixtures/runtime-input/probe.yaml");
    std::fs::create_dir_all(path.parent().context("probe source parent")?)?;
    std::fs::write(
        path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&tool("bin:runtime-input-probe", manifest, runtime_hash))?,
            &keys.publisher,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    let manifest = "name: public-runtime-input-fixture\nversion: 1.0.0\ndescription: Trusted input admission fixture, not qualification\nprovides_kinds: []\nrequires_kinds: [tool]\nuses_kinds: []\n";
    std::fs::write(
        root.join(".ai/manifest.yaml"),
        lillux::signature::sign_content_at(
            manifest,
            &keys.publisher,
            "#",
            None,
            fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    fast_fixture::register_presigned_fixture_bundle(
        state,
        "public-runtime-input-fixture",
        &root,
        keys,
    )
}

fn verify_capsule(state: &Path, detail: &Value, probe_hash: &str, signer: &str) -> Result<String> {
    let hash = detail
        .pointer("/thread/admitted_launch_capsule_hash")
        .and_then(Value::as_str)
        .context("probe retained capsule coordinate")?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        lillux::CasStore::new(state.join(".ai/state/objects"))
            .get_object(hash)?
            .context("probe retained capsule")?,
    )?;
    capsule.validate()?;
    ensure!(
        matches!(
            &capsule.project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        ),
        "probe acquired project authority"
    );
    // Projectless is logical authority, not absence of physical storage. The
    // bound subject is materialized into daemon-private scratch used as cwd;
    // successful relative member observation below demonstrates that route.
    // Do not label scratch as a caller project or assert that no workspace exists.
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity,
        ..
    } = &capsule.artifact_identity
    else {
        anyhow::bail!("probe capsule is not direct execution");
    };
    ensure!(
        matches!(executable_identity, ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        content_hash, executor_manifest_signer_fingerprint, ..
    } if content_hash == probe_hash && executor_manifest_signer_fingerprint == signer),
        "probe lost exact signed bundle executable identity"
    );
    ensure!(
        detail.pointer("/thread/item_ref") == Some(&json!(TOOL))
            && detail.pointer("/thread/project_authority")
                == Some(&serde_json::to_value(&capsule.project_authority)?),
        "probe returned thread differs from retained capsule authority"
    );
    Ok(hash.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires current daemon/standard artifacts and explicit RYEOS_TEST_RUNTIME_INPUT_PROBE; trusted input observation only"]
async fn public_projectless_bundle_probe_observes_bound_runtime_input() -> Result<()> {
    let path = std::path::PathBuf::from(
        std::env::var_os("RYEOS_TEST_RUNTIME_INPUT_PROBE")
            .context("explicit RYEOS_TEST_RUNTIME_INPUT_PROBE is required")?,
    );
    ensure!(path.is_absolute(), "probe input must be absolute");
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)?;
    let probe = pinned.read_stable_bounded(&pinned.observation()?, 16 * 1024 * 1024)?;
    ensure!(!probe.is_empty(), "probe input is empty");
    let probe_hash = lillux::sha256_hex(&probe);
    let (runtime, expected_manifest) = runtime_input()?;
    let runtime_hash = lillux::sha256_hex(&runtime);
    let (mut harness, keys) = DaemonHarness::start_fast_with(
        |state, _, keys| {
            fast_fixture::register_standard_bundle(state, keys)?;
            install(state, keys, &probe, &expected_manifest, &runtime_hash)
        },
        |_| {},
    )
    .await?;
    harness.retain_evidence_on_drop(true);
    let mut project = tempfile::tempdir()?;
    project.disable_cleanup(true);
    eprintln!(
        "trusted input prerequisite retained node={} producer_project={}",
        harness.state_path.display(),
        project.path().display()
    );
    retained_runtime_producer::write_retained_runtime_producer_sources(
        project.path(),
        &keys.publisher,
        crate::RETAINED_RUNTIME_CAPTURE_RECIPE,
    )?;
    let root = lillux::PinnedDirectory::open(project.path())?.context("producer project")?;
    let products = root.create_child(OsStr::new("products"), 0o755)?;
    let runtime_dir = products.create_child(OsStr::new("external-runtime"), 0o755)?;
    let bin = runtime_dir.create_child(OsStr::new("bin"), 0o755)?;
    bin.atomic_create_pinned_regular(OsStr::new("program"), &runtime, 0o755)?
        .context("runtime member already present")?;
    let (producer_accepted, producer_detail) = public_launch::completed_launch(&harness, json!({
        "item_ref":retained_runtime_producer::PRODUCER_REF,"launch_id":PRODUCER_LAUNCH,
        "project_path":project.path(),"ref_bindings":{},"parameters":{},
        "execution_policy":ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted).exclude_operator_vault(),
    }), PRODUCER_LAUNCH).await?;
    let root = producer_accepted["thread_id"]
        .as_str()
        .context("accepted producer root")?;
    let producer = crate::RetainedRuntimeProducer {
        thread_id: root.to_owned(),
        chain_root_id: root.to_owned(),
    };
    let captured = tokio::time::timeout(
        Duration::from_secs(60),
        crate::capture_retained_runtime(&harness, &producer),
    )
    .await
    .context("capture observation expired; retain original producer, do not relaunch")??;
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    crate::offline_runtime_input::verify_capture(
        &evidence,
        root,
        root,
        &format!("fp:{}", keys.user_fp()),
    )?;
    ensure!(
        producer_detail.pointer("/thread/result_project_snapshot_hash")
            == Some(&json!(evidence.result_project_snapshot_hash)),
        "public capture result snapshot differs: expected {}, observed {}",
        producer_detail["thread"]["result_project_snapshot_hash"],
        evidence.result_project_snapshot_hash
    );
    let imported = crate::production_service(
        &harness,
        "service:external-content/import",
        json!({
            "source":"retained_product","witness_hash":captured["witness_hash"],
            "witness_source":{"kind":"local_capture"},"maximum_bytes":32768,
        }),
    )
    .await?;
    ensure!(
        imported["manifest_hash"] == expected_manifest
            && imported["total_bytes"] == runtime.len()
            && imported["entry_count"] == 2,
        "public import changed captured input"
    );
    let binding = crate::production_service(&harness, "service:external-content/bind", json!({
        "staging_id":imported["staging_id"],"request_digest":imported["request_digest"],
        "manifest_hash":expected_manifest,"consumer_ref":TOOL,"consumer_kind":"installed_bundle",
    })).await?;
    ensure!(
        binding["manifest_hash"] == expected_manifest
            && binding["consumer_ref"] == TOOL
            && binding["publisher_fingerprint"] == keys.publisher_fp(),
        "binding changed installed consumer identity"
    );
    let (probe_accepted, detail) = public_launch::completed_launch(&harness, json!({
        "item_ref":TOOL,"launch_id":PROBE_LAUNCH,"ref_bindings":{},"parameters":{},
        "execution_policy":ExecutionPolicy::projectless(ExecutionResponse::Accepted).exclude_operator_vault(),
    }), PROBE_LAUNCH).await?;
    let capsule = verify_capsule(
        &harness.state_path,
        &detail,
        &probe_hash,
        &keys.publisher_fp(),
    )?;
    let result = detail
        .pointer("/result/result")
        .context("probe typed terminal result")?;
    ensure!(
        result
            == &json!({"schema":"test.runtime_input_observation.v1","member":MEMBER,
        "sha256":runtime_hash,"bytes":runtime.len()}),
        "probe returned unexpected input observation: {result}"
    );
    ensure!(
        detail["artifacts"] == json!([]),
        "input probe unexpectedly published artifacts"
    );
    let diagnostic = serde_json::to_string(&json!({
        "scope":"trusted-input-admission-not-qualification","node":harness.state_path,"project":project.path(),
        "producer_accepted":producer_accepted,"probe_accepted":probe_accepted,
        "witness_hash":captured["witness_hash"],"manifest_hash":expected_manifest,
        "binding":binding,"probe_capsule_hash":capsule,"probe_artifacts":detail["artifacts"],"observation":result,
    }))?;
    ensure!(
        diagnostic.len() <= 16384,
        "input evidence exceeds diagnostic bound"
    );
    eprintln!("exact public input prerequisite: {diagnostic}");
    harness.kill_daemon().await?;
    Ok(())
}

#[test]
fn trusted_probe_descriptor_keeps_observer_separate_from_subject() {
    let (runtime, manifest) = runtime_input().unwrap();
    let hash = lillux::sha256_hex(&runtime);
    let tool = tool("bin:fixture/probe", &manifest, &hash);
    assert_eq!(tool["filesystem_authority"], "node_policy");
    assert_eq!(tool["network_authority"], "node_policy");
    assert_eq!(tool["external_content"][0]["mount_root"], "project");
    assert_eq!(tool["config"]["args"], json!([MEMBER, hash]));
    assert!(tool.get("execution_endpoint").is_none());
    assert!(tool.get("env_config").is_none());
}
