//! Shared signed source authoring for synthetic external-candidate fixtures.
//!
//! These helpers author fixture definitions and their exact source closure only.
//! Runtime production/qualification, node configuration, credentials, dispatch,
//! placement, and recovery remain with their existing owners and test callers.
//! Repository source reads are rooted explicitly by the caller.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::crypto::EncodePublicKey as _;
use ryeos_state::external_execution::admission::{
    ExternalCandidateExecutionRoute, ExternalCandidateProcFilesystem, ExternalCandidateRequirement,
    ExternalCandidateRuntimeRecipe,
};

/// Exact signed fixture source, before runtime-product selection. The profile
/// is compiled from the bytes written below; its admitted source binding is
/// independently previewed from this signed root by the test caller.
pub struct CandidateAuthoringBundle {
    pub root: PathBuf,
    pub profile: ryeos_state::objects::AdmittedStructuredSessionProfile,
    pub provider_runtime_manifest_hash: String,
}

/// A signed draft permits source preview but cannot qualify the relationship's
/// five required claims. The test caller replaces it with the exact D0/D1-free
/// use tuple only after independently previewing the signed Worker source.
pub fn sign_candidate_qualification_inputs(
    bundle_root: &Path,
    identity: &ryeos_app::identity::NodeIdentity,
    external_runtime_manifest_hash: &str,
    qualification_runtime_manifest_hash: &str,
    parameters: &serde_json::Value,
    final_policy: bool,
) {
    let parameters = lillux::canonical_json(parameters).unwrap();
    let claims = if final_policy {
        "    - bounded_candidate_capture\n    - candidate_only_execution\n    - controller_credential_exclusion\n    - native_writer_exclusion\n    - no_local_execution_fallback\n"
    } else {
        "    - fixture_draft_never_admit\n"
    };
    let ai = bundle_root.join(".ai");
    let policy = format!(
        "category: fixtures\nversion: \"1.0.0\"\ndescription: Exact synthetic external candidate runtime qualification\nproduct_qualification_policy:\n  schema: ryeos.product_qualification_policy.v2\n  verifier_ref: tool:fixtures/verify-external-runtime\n  subject_declaration_id: subject\n  allowed_claims:\n{claims}  minimum_verifier_process_settlement: trusted_process_group_absent\n  verifier_parameters: {parameters}\n"
    );
    let path = ai.join("config/fixtures/qualification.yaml");
    std::fs::write(
        path,
        lillux::signature::sign_content(&policy, identity.signing_key(), "#", None),
    )
    .unwrap();
    let tool = format!(
        r#"category: fixtures
name: verify-external-runtime
version: "1.0.0"
description: Execute the exact synthetic runtime's independent qualification probe
executor_id: "@subprocess"
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: captured_execution
network_authority: isolated
external_content:
  - id: subject
    kind: tree
    mode: pinned
    digest: {external_runtime_manifest_hash}
    metadata_hint: exact-synthetic-external-candidate-runtime
    mount_root: execution_runtime
    mount: qualification/subject
  - id: verifier-runtime
    kind: tree
    mode: pinned
    digest: {qualification_runtime_manifest_hash}
    metadata_hint: exact-independent-qualification-runtime
    mount_root: execution_runtime
    mount: qualification/verifier
env_config:
  interpreter:
    type: realization_member
    realization_id: verifier-runtime
    relative_path: lib64/ld-linux-x86-64.so.2
config:
  command: "${{interpreter}}"
  args:
    - --library-path
    - /ryeos/realizations/qualification/verifier/usr/lib
    - /ryeos/realizations/qualification/verifier/bin/candidate
    - --qualification-probe
    - {external_runtime_manifest_hash}
  env: {{}}
  timeout_secs: 30
config_schema:
  type: object
  const: {parameters}
"#,
    );
    let path = ai.join("tools/fixtures/verify-external-runtime.yaml");
    std::fs::write(
        path,
        lillux::signature::sign_content(&tool, identity.signing_key(), "#", None),
    )
    .unwrap();
}

/// One exact recipe source for both the installed consumer and its real
/// retained-input producer. This authors declarations, never capture evidence.
pub fn external_runtime_recipe_source(
    external_runtime_manifest_hash: &str,
    large_runtime: bool,
) -> anyhow::Result<String> {
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::{
        PRODUCT_DECLARATIONS_SCHEMA, ProductDeclaration, ProductDeclarations, ProductSource,
    };

    // Ordinary declarations must respect the canonical content tier's 32 MiB
    // per-file ceiling. The former relationship-only fixture used 64 MiB,
    // which could not become an admitted ordinary ProductDeclaration. This
    // intentionally corrects that variant's relationship identity; the large
    // variant retains its existing 512 MiB / 1 GiB limits explicitly.
    let (storage, maximum_file_bytes, maximum_total_bytes) = if large_runtime {
        ("large_content", 536_870_912, 1_073_741_824)
    } else {
        (
            "content",
            ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES,
            134_217_728,
        )
    };
    let relationship = format!(
        r#"category: fixtures
version: "1.0.0"
description: Exact synthetic external-candidate runtime relationship
product_relationships:
  schema: ryeos.product_relationships.v1
  relationships:
    - name: auxiliary_to_verifier
      producer:
        canonical_ref: graph:fixtures/build_runtime
        recipe_binding: product_recipe
        product_name: auxiliary
        parameters: {{}}
      consumer:
        canonical_ref: worker:test/external-candidate
        declaration_id: auxiliary
      required_product:
        shape: tree
        storage: {storage}
        bounds:
          maximum_entries: 16
          maximum_depth: 4
          maximum_file_bytes: {maximum_file_bytes}
          maximum_total_bytes: {maximum_total_bytes}
      qualification:
        policy_ref: config:fixtures/qualification
        required_claims:
          - bounded_candidate_capture
          - candidate_only_execution
          - controller_credential_exclusion
          - native_writer_exclusion
          - no_local_execution_fallback
"#
    );
    let source: serde_json::Value = serde_yaml::from_str(&relationship)?;
    let relationships = ProductRelationships::from_value(source["product_relationships"].clone())?;
    let allowance = relationships.select("auxiliary_to_verifier")?;
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![ProductDeclaration {
            name: allowance.producer.product_name.clone(),
            source: ProductSource::RetainedProject {},
            path: "products/external-runtime".into(),
            shape: allowance.required_product.shape,
            storage: allowance.required_product.storage,
            required: true,
            bounds: allowance.required_product.bounds.clone(),
            expected_manifest_hash: Some(external_runtime_manifest_hash.into()),
        }],
    };
    let source = format!(
        "{relationship}\n{}",
        serde_yaml::to_string(&serde_json::json!({"build_products": declarations}))?
    );
    // Validate the exact emitted source with the production contract owners,
    // not merely the intermediate fixture structs.
    let parsed: serde_json::Value = serde_yaml::from_str(&source)?;
    let declarations = ProductDeclarations::from_value(parsed["build_products"].clone())?;
    let relationships = ProductRelationships::from_value(parsed["product_relationships"].clone())?;
    relationships.validate_against(&declarations, "product_recipe")?;
    Ok(source)
}

#[cfg(test)]
mod runtime_recipe_tests {
    use super::external_runtime_recipe_source;
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::{
        ProductDeclarations, ProductShape, ProductSource, ProductStorage,
    };

    fn assert_exact_recipe(
        large: bool,
        storage: ProductStorage,
        file_limit: u64,
        total_limit: u64,
    ) {
        let manifest_hash = "a".repeat(64);
        let source = external_runtime_recipe_source(&manifest_hash, large).unwrap();
        assert_eq!(
            source,
            external_runtime_recipe_source(&manifest_hash, large).unwrap()
        );
        let parsed: serde_json::Value = serde_yaml::from_str(&source).unwrap();
        let declarations =
            ProductDeclarations::from_value(parsed["build_products"].clone()).unwrap();
        let relationships =
            ProductRelationships::from_value(parsed["product_relationships"].clone()).unwrap();
        relationships
            .validate_against(&declarations, "product_recipe")
            .unwrap();
        assert!(declarations.output_roots.is_empty());
        assert_eq!(declarations.products.len(), 1);
        let runtime = declarations.select("auxiliary").unwrap();
        assert!(matches!(runtime.source, ProductSource::RetainedProject {}));
        assert_eq!(runtime.path, "products/external-runtime");
        assert_eq!(runtime.shape, ProductShape::Tree);
        assert_eq!(runtime.storage, storage);
        assert!(runtime.required);
        assert_eq!(
            runtime.expected_manifest_hash.as_deref(),
            Some(manifest_hash.as_str())
        );
        assert_eq!(runtime.bounds.maximum_entries, 16);
        assert_eq!(runtime.bounds.maximum_depth, 4);
        assert_eq!(runtime.bounds.maximum_file_bytes, file_limit);
        assert_eq!(runtime.bounds.maximum_total_bytes, total_limit);
        assert_eq!(relationships.relationships.len(), 1);
        let relationship = relationships.select("auxiliary_to_verifier").unwrap();
        assert_eq!(runtime.bounds, relationship.required_product.bounds);
        assert_eq!(runtime.storage, relationship.required_product.storage);
        assert_eq!(
            relationship.producer.canonical_ref,
            "graph:fixtures/build_runtime"
        );
        assert_eq!(relationship.producer.product_name, "auxiliary");
        assert_eq!(relationship.producer.recipe_binding, "product_recipe");
        assert_eq!(relationship.producer.parameters, serde_json::json!({}));
        assert_eq!(
            relationship.consumer.canonical_ref,
            "worker:test/external-candidate"
        );
        assert_eq!(relationship.consumer.declaration_id, "auxiliary");
        assert_eq!(
            relationship.qualification.policy_ref.as_deref(),
            Some("config:fixtures/qualification")
        );
        assert_eq!(
            relationship.qualification.required_claims,
            [
                "bounded_candidate_capture",
                "candidate_only_execution",
                "controller_credential_exclusion",
                "native_writer_exclusion",
                "no_local_execution_fallback",
            ]
        );
        assert_ne!(
            source,
            external_runtime_recipe_source(&"b".repeat(64), large).unwrap()
        );
    }

    #[test]
    fn ordinary_runtime_recipe_has_exact_valid_declaration_and_relationship() {
        assert_exact_recipe(false, ProductStorage::Content, 33_554_432, 134_217_728);
    }

    #[test]
    fn large_runtime_recipe_has_exact_valid_declaration_and_relationship() {
        assert_exact_recipe(
            true,
            ProductStorage::LargeContent,
            536_870_912,
            1_073_741_824,
        );
    }

    #[test]
    fn runtime_recipe_refuses_malformed_expected_manifest_hash() {
        for large in [false, true] {
            for invalid in [String::new(), "not-a-manifest-hash".into(), "a".repeat(63)] {
                assert!(external_runtime_recipe_source(&invalid, large).is_err());
            }
        }
    }
}

pub fn write_candidate_authoring_bundle(
    repository_source_root: &Path,
    root: &Path,
    identity: &ryeos_app::identity::NodeIdentity,
    external_runtime_manifest_hash: &str,
    qualification_runtime_manifest_hash: &str,
    real_codex: bool,
    large_runtime: bool,
    scripted_model_origin: Option<&str>,
    command_tools_manifest_hash: Option<&str>,
) -> CandidateAuthoringBundle {
    let bundle = root.join("external-candidate-authoring");
    let ai = bundle.join(".ai");
    let tool_path = ai.join("tools/external-candidate-authoring/integrate.yaml");
    std::fs::create_dir_all(tool_path.parent().unwrap()).unwrap();
    let manifest = r#"name: external-candidate-authoring
version: "1.0.0"
description: Signed authoring authority for the external-candidate E2E
provides_kinds: []
requires_kinds: [config, tool, worker, worker_execution]
uses_kinds: []
runtime_authority:
  item_authoring:
    - kind: knowledge
      namespace: test/external-candidate/integration
"#;
    let tool = r#"version: "1.0.0"
category: external-candidate-authoring
name: integrate
description: Author one accepted integration result into its sealed private candidate workspace
executor_id: "@subprocess"
execution_protocol: protocol:ryeos/core/tool_callback
effects: live
workspace_access: shared_exclusive
filesystem_authority: node_policy
network_authority: node_policy
requires:
  capabilities:
    manifest:
      runtime_authority:
        item_authoring:
          - kind: knowledge
            namespace: test/external-candidate/integration
config:
  command: bin:core/ryeos-core-tools
  args: [author-item, --stdin-json]
  input_data: "${params_json}"
  timeout_secs: 30
config_schema:
  type: object
  properties:
    item_ref:
      type: string
      enum: ["knowledge:test/external-candidate/integration"]
    content: {type: string}
    mode:
      type: string
      enum: ["create"]
    format_ext:
      type: string
      enum: [".md"]
  required: [item_ref, content, mode, format_ext]
  additionalProperties: false
"#;
    std::fs::write(
        ai.join("manifest.yaml"),
        lillux::signature::sign_content(manifest, identity.signing_key(), "#", None),
    )
    .unwrap();
    std::fs::write(
        tool_path,
        lillux::signature::sign_content(tool, identity.signing_key(), "#", None),
    )
    .unwrap();
    let relationship_path = ai.join("config/fixtures/build_recipe.yaml");
    std::fs::create_dir_all(relationship_path.parent().unwrap()).unwrap();
    let recipe = external_runtime_recipe_source(external_runtime_manifest_hash, large_runtime)
        .expect("exact external runtime recipe");
    std::fs::write(
        relationship_path,
        lillux::signature::sign_content(&recipe, identity.signing_key(), "#", None),
    )
    .unwrap();

    std::fs::create_dir_all(ai.join("tools/fixtures")).unwrap();
    sign_candidate_qualification_inputs(
        &bundle,
        identity,
        external_runtime_manifest_hash,
        qualification_runtime_manifest_hash,
        &serde_json::json!({"qualification_draft":"exact_source_preview_only"}),
        false,
    );

    let source = ai.join("workers/test/lib/external-candidate");
    let schema = source.join("schema");
    std::fs::create_dir_all(&schema).unwrap();
    let empty_request = br#"{"additionalProperties":false,"type":"object"}"#;
    let turn_request = br#"{"additionalProperties":false,"properties":{"threadId":{"type":"string","minLength":1,"maxLength":256}},"type":"object"}"#;
    let initialize_response = br#"{"additionalProperties":false,"properties":{"ready":{"const":true}},"required":["ready"],"type":"object"}"#;
    let session_response = br#"{"additionalProperties":false,"properties":{"thread":{"additionalProperties":false,"properties":{"id":{"type":"string"}},"required":["id"],"type":"object"}},"required":["thread"],"type":"object"}"#;
    let turn_response = br#"{"additionalProperties":false,"properties":{"turn":{"additionalProperties":false,"properties":{"id":{"type":"string"}},"required":["id"],"type":"object"}},"required":["turn"],"type":"object"}"#;
    let mut profile = serde_json::json!({
        "schema_version": 10,
        "external_candidate": joined_external_candidate_requirement(),
        "transport": "stdio_jsonrpc",
        "http_sse": null,
        "configuration_authority": "immutable_argv",
        "workload_realization_id": "provider-runtime",
        "workload_executable": "bin/provider",
        "workload_args": [],
        "workload_home_env": "FIXTURE_HOME",
        "required_process_environment": [],
        "workload_client": null,
        "baseline_config": "baseline.json",
        "baseline_destination": "fixture.json",
        "auxiliary_configs": [],
        "runtime_configs": [],
        "portable_state": {
            "schema":1,
            "restore_contract":"ryeos.worker_session.restore.v1",
            "max_depth":8,
            "max_entries":8,
            "max_file_bytes":1048576,
            "max_total_bytes":2097152,
            "selectors":[
                {"pattern":"environments.toml","class":"forbidden_or_unknown","max_matches":1},
                {"pattern":"sessions/{session_id}.json","class":"portable_session_state","max_matches":1}
            ]
        },
        "credential_subject": null,
        "initialization": [{
            "method":"initialize",
            "effect_class":"pure_read",
            "params":{},
            "response_schema":"schema/initialize-response.json",
            "notification":null
        }],
        "recovery": {
            "resume_route":"session.resume",
            "inspect_route":"session.read",
            "route_sets":["session"]
        },
        "route_sets": {"session":["session.read","session.resume","session.start","turn.start"]},
        "routes": [
            {
                "id":"session.start",
                "method":"session/start",
                "effect_class":"external_effect",
                "request_schema":"schema/empty-request.json",
                "response_schema":"schema/session-response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "forbidden_fields":[],
                "response_predicates":[],
                "observations":[{"when":[],"value":{"op":"object","fields":{
                    "kind":{"op":"literal","value":"remote_thread"},
                    "id":{"op":"pointer","pointer":"/response/result/thread/id","max_string_bytes":256}
                }}}],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{"action":"bind_new","request_field":null,"response_pointer":"/result/thread/id"},
                "post_success_routes":[]
            },
            {
                "id":"turn.start",
                "method":"turn/start",
                "effect_class":"external_effect",
                "request_schema":"schema/turn-request.json",
                "response_schema":"schema/turn-response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "forbidden_fields":[],
                "response_predicates":[],
                "observations":[
                    {"when":[],"value":{"op":"object","fields":{
                        "kind":{"op":"literal","value":"state"},
                        "expected":{"op":"literal","value":"idle"},
                        "next":{"op":"literal","value":"turn_running"},
                        "turn_id":{"op":"pointer","pointer":"/response/result/turn/id","max_string_bytes":256}
                    }}},
                    {"when":[],"value":{"op":"object","fields":{
                        "kind":{"op":"literal","value":"state"},
                        "expected":{"op":"literal","value":"turn_running"},
                        "next":{"op":"literal","value":"idle"},
                        "completed_turn_id":{"op":"pointer","pointer":"/response/result/turn/id","max_string_bytes":256}
                    }}}
                ],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{"action":"require","request_field":"threadId","response_pointer":null},
                "post_success_routes":[]
            },
            {
                "id":"session.resume",
                "method":"session/resume",
                "audience":"runtime",
                "effect_class":"session_mutation",
                "request_schema":"schema/turn-request.json",
                "response_schema":"schema/session-response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "forbidden_fields":[],
                "response_predicates":[],
                "observations":[{"when":[],"value":{"op":"object","fields":{
                    "kind":{"op":"literal","value":"remote_thread_recovered"},
                    "id":{"op":"pointer","pointer":"/response/result/thread/id","max_string_bytes":256}
                }}}],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{"action":"bind_expected","request_field":"threadId","response_pointer":"/result/thread/id"},
                "post_success_routes":[]
            },
            {
                "id":"session.read",
                "method":"session/read",
                "audience":"runtime",
                "effect_class":"pure_read",
                "request_schema":"schema/turn-request.json",
                "response_schema":"schema/session-read-response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "forbidden_fields":[],
                "response_predicates":[],
                "observations":[{"when":[{"pointer":"/response/result/thread/status/type","equals":"idle"}],"value":{"op":"object","fields":{
                    "kind":{"op":"literal","value":"remote_thread_recovery_status"},
                    "id":{"op":"pointer","pointer":"/response/result/thread/id","max_string_bytes":256},
                    "outcome":{"op":"literal","value":"safe_idle"}
                }}}],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{"action":"require","request_field":"threadId","response_pointer":null},
                "post_success_routes":[]
            }
        ],
        "notifications":[],
        "ignored_notifications":{},
        "server_requests":[]
    });
    let mut profile_sources = BTreeMap::from([
        ("baseline.json".to_owned(), b"{}\n".to_vec()),
        (
            "schema/empty-request.json".to_owned(),
            empty_request.to_vec(),
        ),
        ("schema/turn-request.json".to_owned(), turn_request.to_vec()),
        (
            "schema/initialize-response.json".to_owned(),
            initialize_response.to_vec(),
        ),
        (
            "schema/session-response.json".to_owned(),
            session_response.to_vec(),
        ),
        (
            "schema/session-read-response.json".to_owned(),
            br#"{"additionalProperties":false,"properties":{"thread":{"additionalProperties":false,"properties":{"id":{"type":"string"},"status":{"additionalProperties":false,"properties":{"type":{"const":"idle"}},"required":["type"],"type":"object"}},"required":["id","status"],"type":"object"}},"required":["thread"],"type":"object"}"#.to_vec(),
        ),
        (
            "schema/turn-response.json".to_owned(),
            turn_response.to_vec(),
        ),
    ]);
    if let Some(origin) = scripted_model_origin {
        configure_real_codex_scripted_turn_profile(
            repository_source_root,
            &mut profile,
            &mut profile_sources,
            origin,
        );
    } else if real_codex {
        configure_real_codex_handshake_profile(
            repository_source_root,
            &mut profile,
            &mut profile_sources,
        );
    }
    let profile = lillux::canonical_json(&profile).unwrap();
    profile_sources.insert("profile.json".into(), profile.as_bytes().to_vec());
    let compiled_profile =
        ryeos_engine::structured_session_profile::compile(profile.as_bytes(), &profile_sources)
            .unwrap();
    let source_manifest = ryeos_state::objects::SourceClosureManifest::new(
        vec![ryeos_state::objects::LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        profile_sources
            .iter()
            .map(|(path, bytes)| ryeos_state::objects::SourceClosureFile {
                root: "source".to_owned(),
                path: path.clone(),
                blob_hash: lillux::sha256_hex(bytes),
                size: bytes.len() as u64,
                mode: ryeos_state::objects::SourceFileMode::ReadOnly,
            })
            .collect(),
    )
    .unwrap();
    let source_manifest_hash = source_manifest.digest().unwrap();
    for (path, bytes) in &profile_sources {
        std::fs::write(source.join(path), bytes).unwrap();
    }

    let worker_path = ai.join("workers/test/external-candidate.yaml");
    std::fs::create_dir_all(worker_path.parent().unwrap()).unwrap();
    let worker = format!(
        r#"category: test
version: "1.0.0"
executor_id: "@subprocess"
description: Joined external candidate structured-session fixture
execution_protocol: protocol:ryeos/core/trusted_structured_session
filesystem_authority: node_policy
session_resources:
  real_uid_process_limit: 4096
supported_target:
  os: linux
  arch: x86_64
  resources: []
source:
  root: lib/external-candidate
  entry: profile.json
  digest: "{}"
external_content:
  - id: provider-runtime
    kind: tree
    mode: pinned
    digest: "{external_runtime_manifest_hash}"
    metadata_hint: joined-external-provider-runtime
    mount_root: execution_runtime
    mount: provider-runtime
external_product_slots:
  - id: auxiliary
    relationship_ref: config:fixtures/build_recipe
    relationship: auxiliary_to_verifier
    kind: tree
    mount_root: execution_runtime
    mount: auxiliary
config:
  command: "bin:core/ryeos-structured-session-bridge"
  args: ["${{source.entry}}"]
  env:
    PATH: ""
    LANG: C
    LC_ALL: C
  timeout_secs: 0
"#,
        source_manifest_hash,
    );
    std::fs::write(
        worker_path,
        lillux::signature::sign_content(&worker, identity.signing_key(), "#", None),
    )
    .unwrap();

    if let Some(command_tools_manifest_hash) = command_tools_manifest_hash {
        let environment_path = ai.join("config/test/external-candidate-environment.yaml");
        std::fs::create_dir_all(environment_path.parent().unwrap()).unwrap();
        let environment = format!(
            r#"category: test
schema: ryeos.worker_environment.v6
external_product_slots: []
worker_ref: worker:test/external-candidate
external_content:
  - id: authoring-tools
    kind: tree
    mode: pinned
    digest: {command_tools_manifest_hash}
    metadata_hint: exact-pinned-codex-authoring-tools-fixture
    mount_root: execution_runtime
    mount: authoring-tools
configuration:
  executable_search:
    - realization_id: authoring-tools
      relative_directory: bin
  process_environment: {{}}
credential_requirement:
  workload_family: codex
  required_state: active
  subject_projection_contract: codex.account.v1
portable_state_contract: ryeos.worker_session.restore.v1
workload_client: null
"#,
        );
        std::fs::write(
            environment_path,
            lillux::signature::sign_content(&environment, identity.signing_key(), "#", None),
        )
        .unwrap();
    }

    let execution_path = ai.join("worker-executions/test/external-candidate.yaml");
    std::fs::create_dir_all(execution_path.parent().unwrap()).unwrap();
    let execution = r#"version: "1.0.0"
category: test
description: Run the joined external candidate fixture through one ordinary session
config:
  worker_ref: worker:test/external-candidate
  environment_binding: null
  required_credential_state: active
  route_set: session
  allowed_effect_classes: [credential_read, external_effect, pure_read, session_mutation]
  credential_home_env: RYEOS_WORKLOAD_HOME
  workspace_env: RYEOS_WORKSPACE
  require_pinned_cow: true
  required_terminal_publication: retain_result
  max_lifetime_seconds: 300
  recover_upstream_session: true
  mode:
    kind: bounded_turn
    session_start_route: session.start
    turn_start_route: turn.start
    max_uncontacted_attempts: 1
  candidate_disposition: retained_for_review
  workload_client_delegation_caps: []
limits:
  duration_seconds: 360
requires:
  capabilities:
    declared:
      - ryeos.runtime.dedicated_session.start
      - ryeos.runtime.dedicated_session.command
      - ryeos.runtime.dedicated_session.terminate
"#;
    let execution = if real_codex && scripted_model_origin.is_none() {
        execution
            .replace("    kind: bounded_turn\n    session_start_route: session.start\n    turn_start_route: turn.start\n    max_uncontacted_attempts: 1", "    kind: session")
            .replace("candidate_disposition: retained_for_review", "candidate_disposition: owner_decision")
    } else {
        execution.to_owned()
    };
    let execution = if command_tools_manifest_hash.is_some() {
        execution
            .replace(
                "  worker_ref: worker:test/external-candidate",
                "  worker_ref: null",
            )
            .replace(
                "  environment_binding: null",
                "  environment_binding: environment",
            )
    } else {
        execution
    };
    std::fs::write(
        execution_path,
        lillux::signature::sign_content(&execution, identity.signing_key(), "#", None),
    )
    .unwrap();
    CandidateAuthoringBundle {
        root: bundle,
        profile: compiled_profile,
        provider_runtime_manifest_hash: external_runtime_manifest_hash.to_owned(),
    }
}

fn configure_real_codex_handshake_profile(
    repository_source_root: &Path,
    profile: &mut serde_json::Value,
    sources: &mut BTreeMap<String, Vec<u8>>,
) {
    profile["external_candidate"] = serde_json::to_value(real_codex_requirement()).unwrap();
    profile["workload_executable"] = "bin/codex".into();
    profile["workload_args"] = serde_json::json!([
        "--strict-config",
        "-c",
        "check_for_update_on_startup=false",
        "app-server"
    ]);
    profile["workload_home_env"] = "CODEX_HOME".into();
    profile["baseline_destination"] = "config.toml".into();
    // This credential-free inspection profile has no upstream session to
    // resume. Do not retain the synthetic session/turn recovery contract after
    // replacing its complete route inventory with environment inspection.
    profile["recovery"] = serde_json::Value::Null;
    profile["initialization"] = serde_json::json!([
        {"method":"initialize", "effect_class":"pure_read", "params":{
            "clientInfo":{"name":"ryeos-handshake-qualification","version":"1"},
            "capabilities":{"experimentalApi":true}
        }, "response_schema":"schema/initialize-response.json", "notification":null},
        {"method":"initialized", "effect_class":"pure_read", "params":{}, "response_schema":null, "notification":"initialized"}
    ]);
    profile["route_sets"] = serde_json::json!({"session":["environment.inspect"]});
    profile["routes"] = serde_json::json!([{
        "id":"environment.inspect", "method":"environment/info", "effect_class":"external_effect",
        "request_schema":"schema/environment-request.json", "response_schema":"schema/environment-response.json",
        "fixed_params":{}, "workspace_fields":[], "forbidden_non_null_fields":[], "forbidden_fields":[],
        "response_predicates":[], "observations":[], "result_retention":"ephemeral", "ceremony":null,
        "post_success_routes":[]
    }]);
    // The pinned Codex app-server can report recoverable startup/configuration
    // warnings before answering a route. Preserve the same schema-checked,
    // durable rules as its signed hosted profile; unknown methods still refuse.
    let hosted_profile: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            repository_source_root
                .join("bundles/codex/.ai/workers/codex/lib/hosted/structured-session.profile.json"),
        )
        .unwrap(),
    )
    .unwrap();
    profile["notifications"] = serde_json::Value::Array(
        hosted_profile["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|rule| matches!(rule["method"].as_str(), Some("configWarning" | "warning")))
            .cloned()
            .collect(),
    );
    profile["ignored_notifications"] = serde_json::json!({
        "remoteControl/status/changed":
            hosted_profile["ignored_notifications"]["remoteControl/status/changed"]
    });
    sources.insert(
        "baseline.json".into(),
        b"check_for_update_on_startup = false\n".to_vec(),
    );
    sources.insert(
        "schema/initialize-response.json".into(),
        std::fs::read(
            repository_source_root
                .join("bundles/codex/.ai/workers/codex/lib/hosted/schema/InitializeResponse.json"),
        )
        .unwrap(),
    );
    sources.insert("schema/environment-request.json".into(), br#"{"type":"object","additionalProperties":false,"required":["environmentId"],"properties":{"environmentId":{"enum":["ryeos-external-candidate","local"]}}}"#.to_vec());
    sources.insert("schema/environment-response.json".into(), br#"{"type":"object","required":["shell","cwd"],"properties":{"shell":{"type":"object","required":["name","path"],"properties":{"name":{"type":"string"},"path":{"type":"string"}}},"cwd":{"type":["string","null"]}}}"#.to_vec());
    sources.insert(
        "schema/ConfigWarningNotification.json".into(),
        std::fs::read(repository_source_root.join(
            "bundles/codex/.ai/workers/codex/lib/hosted/schema/ConfigWarningNotification.json",
        ))
        .unwrap(),
    );
    sources.insert(
        "schema/WarningNotification.json".into(),
        std::fs::read(
            repository_source_root
                .join("bundles/codex/.ai/workers/codex/lib/hosted/schema/WarningNotification.json"),
        )
        .unwrap(),
    );
    sources.insert(
        "schema/RemoteControlStatusChangedNotification.json".into(),
        std::fs::read(repository_source_root.join("bundles/codex/.ai/workers/codex/lib/hosted/schema/RemoteControlStatusChangedNotification.json")).unwrap(),
    );
}

pub fn authored_codex_hosted_sources(repository_source_root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, sources: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(
                !kind.is_symlink(),
                "authored Codex source must not follow links"
            );
            if kind.is_dir() {
                collect(root, &entry.path(), sources);
            } else {
                assert!(
                    kind.is_file(),
                    "authored Codex source must be a regular file"
                );
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                assert!(
                    sources
                        .insert(relative, std::fs::read(entry.path()).unwrap())
                        .is_none()
                );
            }
        }
    }

    let root = repository_source_root.join("bundles/codex/.ai/workers/codex/lib/hosted");
    let mut sources = BTreeMap::new();
    collect(&root, &root, &mut sources);
    sources
}

pub fn configure_real_codex_scripted_turn_profile(
    repository_source_root: &Path,
    profile: &mut serde_json::Value,
    sources: &mut BTreeMap<String, Vec<u8>>,
    model_origin: &str,
) {
    *sources = authored_codex_hosted_sources(repository_source_root);
    *profile = serde_json::from_slice(sources.get("authoring.profile.json").unwrap()).unwrap();
    profile["external_candidate"] = serde_json::to_value(real_codex_requirement()).unwrap();
    profile["workload_realization_id"] = "provider-runtime".into();
    profile["workload_executable"] = "bin/codex".into();
    profile["workload_args"] = serde_json::json!([
        "--strict-config",
        "-c",
        "check_for_update_on_startup=false",
        "app-server"
    ]);
    profile["required_process_environment"] = serde_json::json!([]);
    profile["workload_client"] = serde_json::Value::Null;
    profile["credential_subject"] = serde_json::Value::Null;
    profile["baseline_config"] = "scripted.config.toml".into();
    let selectors = profile["portable_state"]["selectors"]
        .as_array_mut()
        .unwrap();
    selectors.push(serde_json::json!({
        "pattern":"environments.toml",
        "class":"forbidden_or_unknown",
        "max_matches":1
    }));
    selectors.sort_by(|left, right| {
        left["pattern"]
            .as_str()
            .unwrap()
            .cmp(right["pattern"].as_str().unwrap())
    });
    let baseline = format!(
        "model = \"gpt-5.5\"\nmodel_provider = \"routing-fixture\"\n\
         approval_policy = \"never\"\ndefault_permissions = \"danger-full-access\"\n\
         check_for_update_on_startup = false\nweb_search = \"disabled\"\n\
         allow_login_shell = false\nmcp_servers = {{}}\nnotify = []\n\
         [permissions.danger-full-access]\nfilesystem = {{ \":root\" = \"write\" }}\nnetwork = {{ enabled = true }}\n\
         [agents]\nenabled = false\n\
         [orchestrator.skills]\nenabled = false\n\
         [orchestrator.mcp]\nenabled = false\n\
         [features]\napps = false\nplugins = false\n\
         skill_mcp_dependency_install = false\nremote_plugin = false\n\
         hooks = false\nmulti_agent = false\nmulti_agent_v2 = false\nmemories = false\n\
         network_proxy = false\ncode_mode_host = true\n\
         [features.code_mode]\nenabled = false\n\
         [model_providers.routing-fixture]\nname = \"credential-free deterministic routing fixture\"\n\
         base_url = {model_origin:?}\nwire_api = \"responses\"\n\
         requires_openai_auth = false\nsupports_websockets = false\n\
         request_max_retries = 0\nstream_max_retries = 0\n"
    );
    sources.insert("scripted.config.toml".into(), baseline.into_bytes());
}

pub fn real_codex_requirement() -> ExternalCandidateRequirement {
    let mut requirement = joined_external_candidate_requirement();
    requirement.runtime_recipe.executable_relative_path = "bin/codex".into();
    requirement.runtime_recipe.argv0 = "codex".into();
    requirement.runtime_recipe.arguments =
        vec!["exec-server".into(), "--listen".into(), "stdio".into()];
    requirement.runtime_recipe.environment.clear();
    requirement
}

pub fn joined_external_candidate_requirement() -> ExternalCandidateRequirement {
    ExternalCandidateRequirement {
        schema: 6,
        required_lifecycle_capabilities: Default::default(),
        protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
        connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        provider_declaration_id: "codex-hosted".into(),
        provider_configuration_destination: "environments.toml".into(),
        runtime_product_declaration_id: "auxiliary".into(),
        runtime_recipe: ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "lib64/ld-linux-x86-64.so.2".into(),
            argv0: "ld-linux-x86-64.so.2".into(),
            arguments: vec![
                "--library-path".into(),
                "/runtime/usr/lib".into(),
                "/runtime/bin/candidate".into(),
            ],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([(
                "RYEOS_SYNTHETIC_JOINED_PROVIDER_REQUIRED".into(),
                "1".into(),
            )]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        },
    }
}

pub fn signed_candidate_operation_sources(
    identity: &ryeos_app::identity::NodeIdentity,
) -> BTreeMap<String, (Vec<u8>, u32)> {
    let metadata = |description: &str, workspace_access: &str| {
        format!(
            "# ryeos-tool:\n#   category: test/external-candidate\n#   version: \"1.0.0\"\n#   description: {description}\n#   executor_id: tool:ryeos/core/runtimes/python/script\n#   execution_protocol: protocol:ryeos/core/opaque\n#   effects: live\n#   workspace_access: {workspace_access}\n#   filesystem_authority: node_policy\n#   network_authority: node_policy\n#   config_schema:\n#     type: object\n#     additionalProperties: true\n"
        )
    };
    let evaluator = format!(
        "{}{}",
        metadata(
            "Independently evaluate one exact candidate generation",
            "immutable_current_generation",
        ),
        r##"import json, sys
from pathlib import Path

request = json.loads(sys.stdin.buffer.read(65537))
project = Path(sys.argv[2])
candidate = request["candidate_snapshot_hash"]
base = request["base_snapshot_hash"]
accepted = candidate != base and (project / "candidate-strategy.txt").read_text() == "composed external candidate C\n"
if request.get("expect_integration"):
    integration = project / ".ai/knowledge/test/external-candidate/integration.md"
    accepted = accepted and integration.is_file() and "# Integration D" in integration.read_text()
print(json.dumps({"schema_version": 1, "candidate_snapshot_hash": candidate,
                  "base_snapshot_hash": base, "accepted": accepted,
                  "evidence": {"operation": "independent-test-evaluator"}},
                 sort_keys=True, separators=(",", ":")))
"##,
    );
    let signing_key = identity.signing_key();
    let fingerprint = identity.fingerprint();
    let public_der = identity.verifying_key().to_public_key_der().unwrap();
    let public_pem = format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        STANDARD.encode(public_der.as_bytes())
    );
    let trust = format!(
        "version = \"1.0.0\"\nfingerprint = \"{fingerprint}\"\nowner = \"external candidate E2E\"\nattestation = \"\"\n\n[public_key]\npem = \"\"\"\n{public_pem}\"\"\"\n"
    );
    BTreeMap::from([
        (
            ".ai/config/keys/trusted/e2e.toml".into(),
            (
                trust.into_bytes(),
                ryeos_state::objects::ProjectFile::REGULAR_MODE,
            ),
        ),
        (
            ".ai/tools/test/external-candidate/evaluate.py".into(),
            (
                lillux::signature::sign_content(&evaluator, signing_key, "#", None).into_bytes(),
                ryeos_state::objects::ProjectFile::REGULAR_MODE,
            ),
        ),
    ])
}
