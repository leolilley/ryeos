//! Public-daemon native runtime prerequisite, not a candidate evaluation or
//! qualification. The caller owns daemon/project retention on error. This
//! helper never retries an execution ingress or constructs admission/CAS state.

use std::{path::Path, time::Duration};

use anyhow::{Context as _, Result, ensure};
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use serde_json::{Value, json};

use crate::common::{DaemonHarness, fast_fixture::FastFixture};
use crate::{
    RetainedRuntimeProducer, capture_retained_runtime, native_evaluation, production_service,
    retained_runtime_producer,
};

/// Choose these once before invoking prepare, retain them on any uncertainty,
/// and never call prepare again to recover a partially observed run.
pub struct LaunchIds<'a> {
    pub producer: &'a str,
    pub snapshot: &'a str,
}

pub struct PreparedNativeEvaluation {
    pub producer_accepted: Value,
    pub snapshot_accepted: Value,
    pub captured: Value,
    pub evidence: ProductCaptureEvidence,
    pub snapshot_hash: String,
    pub imported: Value,
    pub binding: Value,
}

fn request(project: &Path, item: &str, launch_id: &str, parameters: Value, pinned: bool) -> Value {
    let policy = if pinned {
        ExecutionPolicy::local_pinned_capture(ExecutionResponse::Accepted)
    } else {
        ExecutionPolicy::local_live(ExecutionResponse::Accepted)
    }
    .exclude_operator_vault();
    json!({"launch_id":launch_id,"item_ref":item,"ref_bindings":{},
        "project_path":project,"parameters":parameters,"execution_policy":policy})
}

use crate::public_launch::completed_launch;

/// `project` must be fresh and retained by the caller through failure diagnosis.
/// Node endpoint/credentials and ordinary signed bundles must already be
/// installed by the existing harness. No node policy or capture budget is
/// rewritten here. The 4.3 MiB evaluator fits the fixture's 8 MiB member bound
/// and the current 32 MiB external-content file ceiling. Project capture streams
/// regular blobs; the 1 MiB snapshot-config bound is not a runtime blob bound.
/// Actual disk admission/capture policy remains authoritative and may refuse.
pub async fn prepare(
    harness: &DaemonHarness,
    fixture: &FastFixture,
    project: &Path,
    inputs: native_evaluation::RuntimeInputs<'_>,
    binding_id: &str,
    rule: &native_evaluation::EvaluationRule,
    launches: LaunchIds<'_>,
) -> Result<PreparedNativeEvaluation> {
    ensure!(
        !launches.producer.is_empty()
            && !launches.snapshot.is_empty()
            && launches.producer != launches.snapshot,
        "require two retained distinct launch IDs"
    );
    eprintln!(
        "native prerequisite project={} producer_launch={} snapshot_launch={}; caller must retain daemon and project on failure",
        project.display(),
        launches.producer,
        launches.snapshot
    );
    native_evaluation::write_producer(project, fixture, inputs)?;
    let (producer_accepted, producer_detail) = completed_launch(
        harness,
        request(
            project,
            retained_runtime_producer::PRODUCER_REF,
            launches.producer,
            json!({}),
            true,
        ),
        launches.producer,
    )
    .await?;
    let thread_id = producer_accepted["thread_id"]
        .as_str()
        .context("accepted producer root")?
        .to_owned();
    // These are exact coordinates returned by public admission, not synthetic
    // producer evidence. The capture service independently validates authority.
    let producer = RetainedRuntimeProducer {
        chain_root_id: thread_id.clone(),
        thread_id,
    };
    let captured = tokio::time::timeout(
        Duration::from_secs(60),
        capture_retained_runtime(harness, &producer),
    )
    .await
    .with_context(|| {
        format!(
            "capture timed out for original producer {}; do not relaunch",
            producer.thread_id
        )
    })??;
    let evidence: ProductCaptureEvidence = serde_json::from_value(captured["evidence"].clone())?;
    evidence.validate()?;
    ensure!(
        evidence.thread_id == producer.thread_id
            && evidence.chain_root_id == producer.chain_root_id,
        "capture changed exact accepted producer coordinates"
    );
    ensure!(
        producer_detail.pointer("/thread/result_project_snapshot_hash")
            == Some(&json!(evidence.result_project_snapshot_hash)),
        "capture changed producer retained result snapshot"
    );
    let witness = captured["witness_hash"]
        .as_str()
        .context("public captured witness hash")?;
    ensure!(
        canonical_hash(witness),
        "captured witness hash is not canonical"
    );
    eprintln!(
        "native prerequisite producer={} witness={witness} manifest={}",
        producer.thread_id, evidence.manifest_hash
    );
    native_evaluation::write_consumer(project, fixture, &evidence, binding_id, rule)?;
    let (snapshot_accepted, snapshot_detail) = completed_launch(harness,
        request(project, "tool:core/snapshot-create", launches.snapshot, json!({
            "project_path":project,"message":"native external evaluator with captured runtime","allow_empty":true
        }), false), launches.snapshot).await?;
    let snapshot = snapshot_detail
        .pointer("/result/result")
        .context("snapshot-create terminal payload")?;
    ensure!(
        snapshot["kind"] == "snapshot_create"
            && snapshot["created"] == true
            && snapshot["project_path"] == json!(project)
            && snapshot["snapshot_hash"] == snapshot["head_snapshot_hash"],
        "snapshot-create returned unexpected capture: {snapshot}"
    );
    let snapshot_hash = snapshot["snapshot_hash"]
        .as_str()
        .context("exact consumer snapshot")?
        .to_owned();
    ensure!(
        canonical_hash(&snapshot_hash),
        "consumer snapshot hash is not canonical"
    );
    eprintln!("native prerequisite consumer snapshot={snapshot_hash}");
    let imported = production_service(harness, "service:external-content/import", json!({
        "source":"retained_product","witness_hash":witness,"witness_source":{"kind":"local_capture"},
        "maximum_bytes":evidence.declaration.bounds.maximum_total_bytes,
    })).await?;
    ensure!(
        imported["manifest_hash"] == evidence.manifest_hash
            && imported["manifest_kind"] == evidence.manifest_kind
            && imported["entry_count"] == evidence.entry_count
            && imported["total_bytes"] == evidence.total_bytes,
        "public import changed captured runtime identity: {imported}"
    );
    ensure!(
        imported["staging_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
            && imported["request_digest"]
                .as_str()
                .is_some_and(canonical_hash),
        "public import staging coordinates absent"
    );
    eprintln!(
        "native prerequisite import staging_id={} request_digest={}",
        imported["staging_id"], imported["request_digest"]
    );
    let binding = production_service(harness, "service:external-content/bind", json!({
        "staging_id":imported["staging_id"],"request_digest":imported["request_digest"],
        "manifest_hash":imported["manifest_hash"],"consumer_ref":native_evaluation::TOOL_REF,
        "consumer_kind":"pinned_project","project_snapshot_hash":snapshot_hash,"project_path":project,
    })).await?;
    ensure!(
        binding["manifest_hash"] == evidence.manifest_hash
            && binding["consumer_ref"] == native_evaluation::TOOL_REF
            && binding["publisher_fingerprint"] == fixture.publisher_fp(),
        "public binding identity differs: {binding}"
    );
    Ok(PreparedNativeEvaluation {
        producer_accepted,
        snapshot_accepted,
        captured,
        evidence,
        snapshot_hash,
        imported,
        binding,
    })
}

fn canonical_hash(value: &str) -> bool {
    lillux::valid_hash(value) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_payload_is_inside_thread_result_record_not_wait_success_envelope() {
        // threads.get serializes its real ThreadResultRecord in `result`.
        // Its inner result is the parsed snapshot Tool stdout object, not a
        // second execute/Wait response. This tests serialization only.
        let payload = json!({"kind":"snapshot_create","snapshot_hash":"a".repeat(64)});
        let record = ryeos_app::state_store::ThreadResultRecord {
            outcome_code: None,
            result: Some(payload.clone()),
            error: None,
            metadata: None,
        };
        let detail = json!({"result":record});
        assert_eq!(detail.pointer("/result/result"), Some(&payload));
        assert!(detail.pointer("/result/result/result").is_none());
    }

    fn explicit_input(variable: &str) -> Result<lillux::PinnedRegularFile> {
        let path = std::path::PathBuf::from(
            std::env::var_os(variable)
                .with_context(|| format!("required explicit input {variable} is absent"))?,
        );
        ensure!(
            path.is_absolute(),
            "{variable} must select an absolute regular-file path"
        );
        lillux::secure_fs::open_pinned_regular_file_no_follow(&path).with_context(|| {
            format!(
                "pin exact {variable} input {} without following its leaf",
                path.display()
            )
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires current signed daemon/standard fixture and four explicit native ELF input paths; public capture/import/bind only"]
    async fn public_native_evaluator_runtime_capture_import_bind_prerequisite() -> Result<()> {
        // Explicit host fixture inputs only: no ldconfig/PATH search, symlink
        // canonicalization, subprocess ELF discovery, or guest execution here.
        let evaluator = explicit_input("RYEOS_TEST_NATIVE_EVALUATOR")?;
        let loader = explicit_input("RYEOS_TEST_NATIVE_LOADER")?;
        let libc = explicit_input("RYEOS_TEST_NATIVE_LIBC")?;
        let libgcc = explicit_input("RYEOS_TEST_NATIVE_LIBGCC")?;
        let (mut harness, fixture) = DaemonHarness::start_fast().await?;
        harness.retain_evidence_on_drop(true);
        let mut project = tempfile::tempdir()?;
        project.disable_cleanup(true);
        eprintln!(
            "native public prerequisite retained node={} project={}",
            harness.state_path.display(),
            project.path().display()
        );
        let prepared = prepare(
            &harness,
            &fixture,
            project.path(),
            native_evaluation::RuntimeInputs {
                evaluator: &evaluator,
                loader: &loader,
                libc: &libc,
                libgcc: &libgcc,
            },
            crate::ordinary_direct::BINDING_ID,
            &native_evaluation::EvaluationRule {
                schema_version: 1,
                sample_path: "candidate-strategy.txt".into(),
                expected_utf8: "known-good\n".into(),
                integration: None,
            },
            LaunchIds {
                producer: "L-0b5277d8f2e64565bac769641533c2a1",
                snapshot: "L-197fea692b694bcfb46c6f2c93d517a0",
            },
        )
        .await?;
        // Bounded exact coordinates, never source bodies, credentials, or an
        // implied native runtime/evaluator execution or isolation testimony.
        let evidence = json!({
            "scope":"native-runtime-public-capture-import-bind-only",
            "node":harness.state_path,"project":project.path(),
            "producer_launch_id":prepared.producer_accepted["launch_id"],
            "producer_thread_id":prepared.producer_accepted["thread_id"],
            "snapshot_launch_id":prepared.snapshot_accepted["launch_id"],
            "snapshot_thread_id":prepared.snapshot_accepted["thread_id"],
            "witness_hash":prepared.captured["witness_hash"],
            "manifest_hash":prepared.evidence.manifest_hash,
            "entry_count":prepared.evidence.entry_count,"total_bytes":prepared.evidence.total_bytes,
            "snapshot_hash":prepared.snapshot_hash,
            "staging_id":prepared.imported["staging_id"],
            "request_digest":prepared.imported["request_digest"],
            "binding_subject_id":prepared.binding["binding_subject_id"],
            "binding_id":prepared.binding["binding_id"],"binding_hash":prepared.binding["binding_hash"],
            "consumer_ref":prepared.binding["consumer_ref"],
        });
        let diagnostic = serde_json::to_string(&evidence)?;
        ensure!(
            diagnostic.len() <= 8192,
            "native prerequisite evidence exceeds diagnostic bound"
        );
        eprintln!("native prerequisite exact public evidence: {diagnostic}");
        harness.kill_daemon().await?;
        // Retain exact files even on success for inspection/reconciliation;
        // the daemon is stopped, not left as a background fixture owner.
        Ok(())
    }

    #[test]
    fn native_prerequisite_requests_keep_ingress_identity_and_accepted_response() {
        let launch_id = "L-4bd4df404846f1d4a89ccab141c58d80";
        let request = request(
            Path::new("/fixture"),
            retained_runtime_producer::PRODUCER_REF,
            launch_id,
            json!({}),
            true,
        );
        assert_eq!(request["launch_id"], launch_id);
        assert_eq!(request["item_ref"], retained_runtime_producer::PRODUCER_REF);
        let policy: ExecutionPolicy =
            serde_json::from_value(request["execution_policy"].clone()).unwrap();
        assert_eq!(policy.response, ExecutionResponse::Accepted);
        assert!(canonical_hash(&"a".repeat(64)));
        assert!(!canonical_hash(&"A".repeat(64)));
    }
}
