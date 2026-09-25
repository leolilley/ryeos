//! Source authoring for a genuine public ordinary-command admission fixture.
//!
//! Include beside `signed_bundle` and `retained_runtime_producer` in the daemon
//! integration test. Register the standard bundle and install base sources in
//! `DaemonHarness::start_fast_with`,
//! then run/capture the retained producer through the public services before
//! authoring the consumer. This helper creates no admission token, product
//! witness, CAS object, launch claim, or credential-vault entry. It does not
//! claim a successful launch; only the composed public test can establish it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{
    ChildProjectPolicy, ExecutionEnvironmentPolicy, ExecutionPolicy, ExecutionResponse,
    PinnedRealization, PinnedSource, ProjectExecutionPolicy,
};
use ryeos_state::external_content::products::ProductCaptureEvidence;
use ryeos_state::external_execution::transport::ExternalControllerTransportContract;
use serde_json::{Value, json};

use crate::common::fast_fixture::{self, FastFixture};
use crate::{retained_runtime_producer, signed_bundle};

pub const TOOL_REF: &str = "tool:fixtures/ordinary-direct/run";
pub const RUNTIME_REF: &str = "tool:fixtures/ordinary-direct/runtime";
pub const PROTOCOL_REF: &str = "protocol:ryeos/core/opaque";
pub const BINDING_ID: &str = "ordinary-direct";
pub const RECORDED_GRAPH_REF: &str = "graph:fixtures/ordinary-direct/recorded";

#[derive(Clone, Copy)]
enum ConsumerEffects {
    Live,
    Recorded,
}

impl ConsumerEffects {
    fn declaration(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Recorded => "recorded",
        }
    }
}

fn recorded_graph_definition() -> Value {
    json!({
        "version":"2.0.0", "category":"fixtures/ordinary-direct",
        "description":"Record one ordinary externally placed Tool action",
        "requires":{"capabilities":{"declared":[
            "ryeos.execute.tool.fixtures/ordinary-direct/run"
        ]}},
        "config":{
            "start":"run", "max_steps":2, "on_error":"fail",
            "config_schema":{"type":"object","properties":{},"additionalProperties":false},
            "nodes":{
                "run":{
                    "node_type":"action", "effects":"recorded", "cache_result":false,
                    "action":{"item_id":TOOL_REF,"ref_bindings":{},"params":{}},
                    "assign":{"ordinary_result":"${result}"},
                    "next":{"type":"unconditional","to":"done"}
                },
                "done":{"node_type":"return","output":"${state.ordinary_result}"}
            }
        }
    })
}

fn ordinary_execution_policy() -> Value {
    json!({
        "category":"execution", "version":"2.1.0", "schema_version":"2.1.0",
        "items":{"tool":{"fixtures/ordinary-direct/run":{"timeout":30}}}
    })
}

/// Supplied by the test's real local HTTPS fixture and credential provisioning
/// owner. A hash here is only a signed selector, never a fabricated vault value.
pub struct EndpointInputs<'a> {
    pub transport: &'a ExternalControllerTransportContract,
    pub tls_roots_der_base64: &'a [String],
    pub credential_generation: &'a str,
    pub credential_sha256: &'a str,
    pub provider_state_root: &'a Path,
}

fn write_signed_new(path: &Path, value: &Value, signer: &SigningKey) -> Result<()> {
    ensure!(
        !path.exists(),
        "fixture refuses to replace {}",
        path.display()
    );
    std::fs::create_dir_all(path.parent().context("fixture source parent")?)?;
    let body = serde_yaml::to_string(value)?;
    let document = lillux::signature::sign_content_at(
        &body,
        signer,
        "#",
        None,
        fast_fixture::FAST_FIXTURE_TIME,
    );
    std::fs::write(path, document)?;
    Ok(())
}

/// Run only inside the existing pre-init callback. `signed_bundle` registers
/// exact executable sidecars/manifest; the loader must still admit the node
/// binding, protocol and backend normally when the daemon starts.
pub fn install_before_start(
    state_path: &Path,
    fixture: &FastFixture,
    artifacts: &signed_bundle::SyntheticExternalArtifacts<'_>,
    endpoint: EndpointInputs<'_>,
) -> Result<PathBuf> {
    endpoint.transport.validate()?;
    ensure!(
        ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
            endpoint.tls_roots_der_base64,
        )? == endpoint.transport.tls_root_bundle_digest,
        "fixture controller roots differ from transport identity"
    );
    for hash in [endpoint.credential_generation, endpoint.credential_sha256] {
        ensure!(
            lillux::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase()),
            "fixture credential coordinate is not a canonical hash"
        );
    }
    ensure!(
        endpoint.provider_state_root.is_absolute(),
        "provider fixture root must be absolute"
    );
    let identity = |path: &Path| -> Result<(String, u64)> {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        ensure!(!bytes.is_empty(), "fixture artifact is empty");
        Ok((lillux::sha256_hex(&bytes), bytes.len() as u64))
    };
    let (adapter_hash, adapter_bytes) = identity(artifacts.adapter)?;
    let (supervisor_hash, supervisor_bytes) = identity(artifacts.supervisor)?;
    let (launcher_hash, launcher_bytes) = identity(artifacts.launcher)?;
    let (bundle, _, _) =
        signed_bundle::install_signed_test_bundle(state_path, true, &fixture.publisher, artifacts);
    fast_fixture::register_presigned_fixture_bundle(
        state_path,
        "synthetic-external",
        &bundle,
        fixture,
    )?;
    let settings = json!({
        "schema":1, "state_root":endpoint.provider_state_root,
        "expected_credential_sha256":endpoint.credential_sha256,
        "maximum_copy_entries":4096, "maximum_copy_depth":32,
        "startup_timeout_ms":30_000,
    });
    let settings_digest = lillux::sha256_hex(lillux::canonical_json(&settings)?.as_bytes());
    write_signed_new(
        &state_path.join(format!(".ai/node/external_execution/{BINDING_ID}.yaml")),
        &json!({
            "kind":"node", "schema":9,
            "protocol":ryeos_state::external_execution::admission::PROTOCOL,
            "workload":{"kind":"direct_command"}, "backend":"synthetic-local",
            "account":"ordinary-fixture", "capacity_group":"ordinary-fixture",
            "credential_generation":endpoint.credential_generation,
            // Exact schema identity declared by the shared signed bundle.
            "settings_schema_digest":"9".repeat(64),
            "settings":settings, "settings_digest":settings_digest,
            "backend_artifact_hash":adapter_hash, "backend_artifact_bytes":adapter_bytes,
            "supervisor_artifact_hash":supervisor_hash, "supervisor_artifact_bytes":supervisor_bytes,
            "launcher_artifact_hash":launcher_hash, "launcher_artifact_bytes":launcher_bytes,
            "network_policy":"supervisor_pinned_owner_only_candidate_denied_v1",
            "storage_policy":"ephemeral_private_candidate_v1",
            "cleanup_proof":"provider_terminal_occurrence_v1",
            "controller_transport":endpoint.transport,
            "controller_tls_root_certificates_der_base64":endpoint.tls_roots_der_base64,
            "max_active":1, "timeout_seconds":30, "contact_timeout_seconds":30,
            "observation_timeout_seconds":60, "cleanup_timeout_seconds":120,
            "max_workspace_bytes":67_108_864, "max_export_bytes":0,
            "max_transfer_bytes":134_217_728,
        }),
        &fixture.node,
    )?;
    Ok(bundle)
}

/// Author the existing bounded retained-project producer. The caller must run
/// its graph and `service:external-content/capture-product`; no filesystem hash
/// or newly manufactured manifest may replace that public capture result.
pub fn write_producer(project: &Path, fixture: &FastFixture, runtime: &[u8]) -> Result<()> {
    ensure!(
        !runtime.is_empty() && runtime.len() <= 32768,
        "runtime exceeds fixture bound"
    );
    let recipe = json!({
        "category":"fixtures", "version":"1.0.0",
        "description":"Retained prebuilt ordinary fixture runtime; not compiler production",
        "build_products":{"schema":"ryeos.build_products.v1","output_roots":[],
            "products":[{"name":retained_runtime_producer::PRODUCT_NAME,
                "source":{"kind":"retained_project"},"path":"products/external-runtime",
                "shape":"tree","storage":"content","required":true,
                "bounds":{"maximum_entries":2,"maximum_depth":2,
                    "maximum_file_bytes":32768,"maximum_total_bytes":32768}}]},
        "product_relationships":{"schema":"ryeos.product_relationships.v1","relationships":[]}
    });
    retained_runtime_producer::write_retained_runtime_producer(
        project,
        &fixture.publisher,
        &serde_yaml::to_string(&recipe)?,
        runtime,
    )
}

/// The evidence argument comes from the public capture response. It is checked
/// for fixture coordinates but is not itself promoted to launch authority:
/// ordinary external-content admission must resolve the signed fixed pin and
/// its stored content through the node CAS, independently of this helper.
pub fn write_consumer(
    project: &Path,
    fixture: &FastFixture,
    captured: &ProductCaptureEvidence,
) -> Result<()> {
    write_consumer_with_effects(project, fixture, captured, ConsumerEffects::Live)
}

/// Author this variant in a fresh fixture, before public snapshot/import/bind.
/// The graph and both Tool definitions are signed normally; this helper grants
/// no effect authorization and does not edit an already admitted snapshot.
pub fn write_recorded_consumer(
    project: &Path,
    fixture: &FastFixture,
    captured: &ProductCaptureEvidence,
) -> Result<()> {
    write_consumer_with_effects(project, fixture, captured, ConsumerEffects::Recorded)?;
    write_signed_new(
        &project.join(".ai/graphs/fixtures/ordinary-direct/recorded.yaml"),
        &recorded_graph_definition(),
        &fixture.publisher,
    )
}

fn write_consumer_with_effects(
    project: &Path,
    fixture: &FastFixture,
    captured: &ProductCaptureEvidence,
    effects: ConsumerEffects,
) -> Result<()> {
    captured.validate()?;
    ensure!(
        captured.producer.canonical_ref == retained_runtime_producer::PRODUCER_REF
            && captured.recipe_ref == retained_runtime_producer::RECIPE_REF
            && captured.recipe_binding == retained_runtime_producer::RECIPE_BINDING
            && captured.declaration.name == retained_runtime_producer::PRODUCT_NAME
            && captured.declaration.path == "products/external-runtime"
            && captured.manifest_kind == ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
        "consumer requires the exact public retained-runtime capture"
    );
    // The execution-policy resolver takes precedence over config.timeout_secs.
    // Scope this override to the consumer, before its public snapshot capture;
    // neither producer defaults nor the protected endpoint ceiling change.
    write_signed_new(
        &project.join(".ai/config/execution/execution.yaml"),
        &ordinary_execution_policy(),
        &fixture.publisher,
    )?;
    // Source policy belongs to the signed executor chain, not its root caller.
    // The resolver projects this policy onto the root Tool's source entry.
    write_signed_new(
        &project.join(".ai/tools/fixtures/ordinary-direct/runtime.yaml"),
        &json!({
            "category":"fixtures/ordinary-direct","name":"runtime","version":"1.0.0",
            "description":"Ordinary exact realization interpreter fixture",
            "executor_id":"@subprocess", "execution_protocol":PROTOCOL_REF,
            "effects":effects.declaration(), "filesystem_authority":"captured_execution", "network_authority":"isolated",
            "source_scope":{"location":"item_namespace",
                "load_roots":["item_directory"],"materialization":"read_only"},
            "env_config":{"interpreter":{"type":"realization_member",
                "realization_id":"runtime","relative_path":"bin/codex"}},
            "config":{"command":"${interpreter}","args":["${source.entry}"],
                "input_data":"{\"id\":1,\"method\":\"fixture/session/run\"}\n","timeout_secs":30}
        }),
        &fixture.publisher,
    )?;
    write_signed_new(
        &project.join(".ai/tools/fixtures/ordinary-direct/run.yaml"),
        &json!({
            "category":"fixtures/ordinary-direct","name":"run","version":"1.0.0",
            "description":"Public direct placement admission fixture",
            "executor_id":RUNTIME_REF, "execution_protocol":PROTOCOL_REF,
            "effects":effects.declaration(), "filesystem_authority":"captured_execution", "network_authority":"isolated",
            "supported_target":{"os":"linux","arch":std::env::consts::ARCH,"resources":[]},
            "execution_endpoint":{"kind":"external","binding_id":BINDING_ID,
                "stdout_max_bytes":4096,"stderr_max_bytes":4096},
            "external_content":[{"id":"runtime","kind":"tree","mode":"pinned",
                "digest":captured.manifest_hash,"mount_root":"project","mount":"vendor/runtime"}],
            "config_schema":{"type":"object","properties":{},"additionalProperties":false}
        }),
        &fixture.publisher,
    )
}

/// Reuse the exact public snapshot to which the operator bound the captured
/// product. Admission still authenticates it; never recapture a new generation.
pub fn request(project: &Path, snapshot_hash: &str, response: ExecutionResponse) -> Result<Value> {
    let mut policy = ExecutionPolicy::local_pinned_capture(response);
    policy.environment = ExecutionEnvironmentPolicy::None;
    policy.project = ProjectExecutionPolicy::Pinned {
        source: PinnedSource::Snapshot {
            hash: snapshot_hash.to_owned(),
        },
        realization: PinnedRealization::ReadOnly,
        child_policy: ChildProjectPolicy::Inherit,
    };
    policy.validate()?;
    Ok(
        json!({"item_ref":TOOL_REF,"project_path":project,"ref_bindings":{},
        "parameters":{},"execution_policy":policy}),
    )
}

/// Both equivalent graph launches must use this same snapshot and unchanged
/// action. The caller supplies a distinct ingress launch_id for each confirmed
/// new graph root; launch IDs are not injected into the recorded Tool params.
pub fn recorded_request(
    project: &Path,
    snapshot_hash: &str,
    response: ExecutionResponse,
) -> Result<Value> {
    let mut value = request(project, snapshot_hash, response)?;
    value["item_ref"] = json!(RECORDED_GRAPH_REF);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_recorded_companion_keeps_one_exact_uncached_action() {
        assert_eq!(ConsumerEffects::Live.declaration(), "live");
        assert_eq!(ConsumerEffects::Recorded.declaration(), "recorded");
        let graph = recorded_graph_definition();
        let nodes = graph["config"]["nodes"].as_object().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(graph["config"]["start"], "run");
        assert_eq!(nodes["run"]["effects"], "recorded");
        assert_eq!(nodes["run"]["cache_result"], false);
        assert_eq!(
            nodes["run"]["action"],
            json!({"item_id":TOOL_REF,"ref_bindings":{},"params":{}})
        );
        assert_eq!(
            nodes["run"]["next"],
            json!({"type":"unconditional","to":"done"})
        );
        assert_eq!(
            nodes["done"],
            json!({"node_type":"return","output":"${state.ordinary_result}"})
        );
        assert_eq!(
            graph["requires"]["capabilities"]["declared"],
            json!(["ryeos.execute.tool.fixtures/ordinary-direct/run"])
        );
    }

    #[test]
    fn ordinary_recorded_graph_request_changes_only_subject_not_pinned_authority() {
        let snapshot = "b".repeat(64);
        let direct = request(
            Path::new("/fixture"),
            &snapshot,
            ExecutionResponse::Accepted,
        )
        .unwrap();
        let mut graph = recorded_request(
            Path::new("/fixture"),
            &snapshot,
            ExecutionResponse::Accepted,
        )
        .unwrap();
        assert_eq!(graph["item_ref"], RECORDED_GRAPH_REF);
        assert!(graph.get("launch_id").is_none());
        graph["item_ref"] = json!(TOOL_REF);
        assert_eq!(graph, direct);
    }

    #[test]
    fn ordinary_timeout_policy_overrides_only_exact_consumer() {
        use ryeos_engine::canonical_ref::CanonicalRef;
        use ryeos_engine::execution_policy::ExecutionPolicyResolver;

        let policy = ordinary_execution_policy();
        assert!(policy.get("defaults").is_none());
        let resolve = |document: &Value, item: &str| {
            ExecutionPolicyResolver::resolve_from_value_for_item(
                document,
                &CanonicalRef::parse(item).unwrap(),
                None,
                None,
            )
            .unwrap()
        };
        assert_eq!(resolve(&policy, TOOL_REF).timeout.unwrap().value, 30);
        assert!(resolve(&policy, RUNTIME_REF).timeout.is_none());
        assert!(
            resolve(&policy, retained_runtime_producer::PRODUCER_REF)
                .timeout
                .is_none()
        );
        // Exercise actual resolver precedence against the inherited Core value.
        let mut inherited = policy;
        inherited["defaults"] = json!({"timeout":86400});
        assert_eq!(resolve(&inherited, TOOL_REF).timeout.unwrap().value, 30);
        assert_eq!(
            resolve(&inherited, RUNTIME_REF).timeout.unwrap().value,
            86400
        );
    }

    #[test]
    fn ordinary_request_reuses_bound_snapshot_readonly_without_environment() {
        let snapshot_hash = "a".repeat(64);
        let request = request(
            Path::new("/fixture"),
            &snapshot_hash,
            ExecutionResponse::Wait,
        )
        .unwrap();
        let policy: ExecutionPolicy =
            serde_json::from_value(request["execution_policy"].clone()).unwrap();
        assert_eq!(policy.environment, ExecutionEnvironmentPolicy::None);
        assert_eq!(
            policy.project,
            ProjectExecutionPolicy::Pinned {
                source: PinnedSource::Snapshot {
                    hash: snapshot_hash
                },
                realization: PinnedRealization::ReadOnly,
                child_policy: ChildProjectPolicy::Inherit,
            }
        );
    }
}
