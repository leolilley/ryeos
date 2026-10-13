//! Admission fixtures use real signed sources and the public sealing path.
//! They require the populated core/standard test bundles. No evaluator runs.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, Result};
use ryeos_app::{
    launch_metadata::{ResumeContext, RuntimeLaunchMetadata},
    thread_lifecycle::*,
};
use ryeos_engine::{contracts::*, effective_program::*, resolution::TrustClass};
use ryeos_state::objects::*;
use serde_json::{Value, json};

/// Fields drop in declaration order: the application, then the owner that
/// closes its Store, then the directory.
struct Fixture {
    state: ryeos_app::state::AppState,
    _store_owner: ryeos_app::test_support::TestStateStore,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let (store_owner, mut state) = ryeos_app::state::test_support::build(root.path())?;
        let bundle = root.path().join("evaluator-bundle");
        let key = lillux::crypto::SigningKey::from_bytes(&[71; 32]);
        let write = |path: &str, body: &str| -> Result<()> {
            let path = bundle.join(path);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, lillux::signature::sign_content(body, &key, "#", None))?;
            Ok(())
        };
        write(
            ".ai/manifest.yaml",
            "name: evaluator-fixture\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        )?;
        write(
            ".ai/graphs/evaluator-fixture/check.yaml",
            r#"version: '1.0.0'
category: evaluator-fixture
description: Finite evaluator admission fixture
requires: {capabilities: {declared: []}}
config:
  start: done
  max_steps: 1
  on_error: fail
  nodes:
    done:
      node_type: return
      output: {accepted: true}
"#,
        )?;
        write(
            ".ai/tools/evaluator-fixture/check.yaml",
            r#"version: '1.0.0'
category: evaluator-fixture
description: Flat evaluator admission fixture
executor_id: '@subprocess'
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: node_policy
network_authority: node_policy
config: {command: /bin/true, args: [], timeout_secs: 1}
config_schema: {type: object, properties: {}, additionalProperties: false}
"#,
        )?;
        let core = ryeos_engine::test_support::core_bundle_root();
        let standard = ryeos_engine::test_support::standard_bundle_root();
        let roots = vec![core.clone(), standard.clone(), bundle];
        let mut trust = ryeos_engine::test_support::live_trust_store();
        trust.extend_from(&ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: lillux::signature::compute_fingerprint(&key.verifying_key()),
                verifying_key: key.verifying_key(),
                label: Some("evaluator fixture".into()),
            },
        ]));
        let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
            &[
                core.join(".ai/node/engine/kinds"),
                standard.join(".ai/node/engine/kinds"),
            ],
            &trust,
        )?;
        let (parsers, _) =
            ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
        let handlers = ryeos_engine::test_support::load_live_handler_registry();
        let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
        let tagged = roots
            .iter()
            .map(|root| (root.clone(), TrustClass::TrustedBundle))
            .collect::<Vec<_>>();
        let protocols = ryeos_engine::protocols::ProtocolRegistry::load_base(&tagged, &trust)?;
        let runtimes = ryeos_engine::runtime_registry::RuntimeRegistry::build_from_bundles(
            &tagged, &trust, &kinds,
        )?;
        // Bind the signed runtime handlers through the same validation and
        // bounded runner as production engine initialization. Loading runtime
        // descriptors alone does not install their launch preparers.
        let runner = ryeos_engine::launch_preparers::LaunchPreparerRunner::from_isolation_runtime(
            state.isolation.clone(),
            &roots,
        )?;
        ryeos_engine::boot_validation::validate_runtime_launch_handlers(
            &runtimes, &handlers, &runner,
        )
        .map_err(|issues| {
            anyhow::anyhow!("runtime launch-preparer validation failed: {issues:?}")
        })?;
        let launch_preparers =
            ryeos_engine::launch_preparers::LaunchPreparerRegistry::from_runtimes(
                &runtimes, &handlers, runner,
            )?;
        let validators =
            ryeos_engine::effective_validators::EffectiveValidatorRegistry::from_kinds(
                &kinds,
                &handlers,
                state.isolation.clone(),
                &roots,
            )?;
        let registered = ["core", "standard", "evaluator-fixture"]
            .into_iter()
            .zip(&roots)
            .map(
                |(name, root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                    name: name.into(),
                    canonical_root: root.clone(),
                },
            )
            .collect();
        state.engine = Arc::new(
            ryeos_engine::engine::Engine::new(
                kinds,
                ryeos_engine::parsers::ParserDispatcher::new(parsers, handlers),
                roots,
            )
            .with_trust_store(trust.clone())
            .with_node_trust_store(trust)
            .with_composers(composers)
            .with_protocols(protocols)
            .with_runtimes(runtimes)
            .with_launch_preparers(launch_preparers)
            .with_effective_validators(validators)
            .with_registered_bundle_roots(registered),
        );
        Ok(Self {
            state,
            _store_owner: store_owner,
            _root: root,
        })
    }

    fn capsule(&self, kind: &str) -> Result<(AdmittedLaunchCapsule, ThreadSnapshot)> {
        let engine = &self.state.engine;
        let context = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: self.state.identity.principal_id(),
                scopes: vec!["*".into()],
            }),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: "site:test".into(),
            origin_site_id: "site:test".into(),
            execution_hints: ExecutionHints::default(),
            scheduled_fire: None,
            validate_only: false,
        };
        let binding = AdmittedProjectBinding::explicit_projectless(engine, &context)?;
        let item_ref = format!("{kind}:evaluator-fixture/check");
        let admission = preflight_root_execution(ResolveRootExecutionParams {
            engine,
            plan_context: context.clone(),
            project_binding: binding,
            node_history_policy:
                &ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy(),
            item_ref: &item_ref,
            ref_bindings: BTreeMap::new(),
            product_selections: vec![],
            launch_mode: "wait",
            parameters: json!({}),
            usage_subject: None,
            usage_subject_asserted_by: None,
            creates_chain_root: true,
        })?
        .root_admission;
        let mut resolution = admission.resolution_output().clone();
        let roots = engine.resolution_roots(None);
        // Capture hooks from the signed kind and actual base/operator/project
        // source discovery, just as production does before final validation.
        // An empty authored list alone is not an effective hook plan.
        let hook_snapshots =
            ryeos_app::effective_program_preparation::prepare_preselection_effective_program(
                engine,
                &admission.verified_subject().resolved,
                &mut resolution,
                &roots,
                &engine.trust_store,
                None,
            )?;
        if kind == "graph" {
            let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(
                resolution
                    .composed
                    .derived
                    .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
                    .context("fixture Graph lost its captured hook plan")?,
            )?;
            assert!(plan.iter_layers().all(|(_, layer)| layer.hooks.is_empty()));
            assert!(
                !plan.sources.is_empty(),
                "empty hooks still retain signed base provenance"
            );
        }
        let semantic_projection = take_recovered_effective_program_derived(&mut resolution);
        let validation = engine.effective_validators.validate(kind, &resolution)?;
        let candidate =
            relock_recovered_effective_program(resolution, validation, semantic_projection)?;
        let config_proofs = hook_snapshots
            .as_ref()
            .map(|snapshots| std::slice::from_ref(&snapshots.dependency_proof))
            .unwrap_or_default();
        let proof = prove_finalization_authority(
            &candidate,
            engine.kinds.get(kind).unwrap().external_content_contract(),
            config_proofs,
            &engine.launch_config_roots(&roots),
            None,
            None,
            None,
        )?;
        let finalized = finalize_effective_program(candidate, proof)?;
        let lifecycle = ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE;
        let authority = self.state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        // The shared CAS guard is a fork-sensitive descriptor: each branch takes
        // it only after any launch preparation that may fork has finished.
        let (guard, request, runtime_ref, artifact, closure, driver) = if kind == "graph" {
            use ryeos_app::thread_lifecycle::managed_runtime_identity as managed;
            let selected = managed::resolve_current_managed_runtime_selection(engine, None, kind)?;
            let executable = managed::resolve_current_managed_executor_identity(engine, &selected)?;
            let request = admission.execution_request(
                RootExecutionRoute::ManagedRuntimeForKind(&selected.runtime.canonical_ref),
                "wait".into(),
                json!({}),
            )?;
            let snapshot =
                engine.effective_request_snapshot(None, &context.subject_resolution_authority)?;
            let prepared = ryeos_executor::execution::launch_preparation::prepare_runtime_launch(
                ryeos_executor::execution::launch_preparation::PrepareRuntimeLaunchRequest {
                    engine,
                    runtime: &selected.runtime,
                    primary: finalized.resolution(),
                    ref_bindings: &request.ref_bindings,
                    product_selections: &request.product_selections,
                    roots: &roots,
                    parsers: &snapshot.parser_dispatcher,
                    trust_store: &snapshot.trust_store,
                    principal: &context.requested_by,
                    subject_resolution_authority: &context.subject_resolution_authority,
                    resolution_cache: None,
                    ref_binding_resolution_timings: None,
                },
            )?;
            let artifact = AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref: selected.runtime.canonical_ref.to_string(),
                runtime_content_hash: selected.runtime.raw_content_digest.clone(),
                runtime_signer_fingerprint: selected.runtime.signer_fingerprint.clone(),
                protocol_ref: selected.protocol.canonical_ref.clone(),
                protocol_content_hash: selected.protocol.raw_content_digest.clone(),
                protocol_signer_fingerprint: selected.protocol.signer_fingerprint.clone(),
                executor_ref: executable.executor_ref,
                executor_content_hash: executable.content_hash.clone(),
                executor_bundle_manifest_hash: executable.bundle_manifest_hash,
                executor_bundle_signer_fingerprint: executable.bundle_signer_fingerprint,
            };
            let closure = AdmittedExecutionClosure::ManagedRuntime {
                prepared_runtime_launch: serde_json::to_value(prepared)?,
                runtime_descriptor_document: std::fs::read_to_string(
                    &selected.runtime.descriptor_path,
                )?,
                protocol_descriptor_document: std::fs::read_to_string(
                    &selected.protocol.descriptor_path,
                )?,
                executor_blob_hash: executable.content_hash,
            };
            (
                authority.acquire_shared_guard()?,
                request,
                selected.runtime.canonical_ref.to_string(),
                artifact,
                closure,
                ExecutionLaunchDriver::ManagedRuntime,
            )
        } else {
            let request = admission.execution_request(
                RootExecutionRoute::RootExecutorChain,
                "wait".into(),
                json!({}),
            )?;
            let mut prepared = prepare_item_plan(
                engine,
                &request,
                &self.state.isolation,
                lifecycle,
                None,
                None,
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            )?;
            prepared.bind_finalized_execution_endpoint(
                &self.state,
                engine,
                finalized.resolution(),
            )?;
            let protocol = resolve_direct_terminator_protocol(engine, &request)?;
            let guard = authority.acquire_shared_guard()?;
            let closure = prepared.admit_execution_closure(
                &cas,
                &self.state.isolation,
                protocol,
                &engine.node_trust_store,
                None,
                Some(admission.project_authority()),
            )?;
            let artifact = prepared.admitted_artifact_identity(&request, protocol)?;
            (
                guard,
                request,
                prepared.runtime_ref()?.to_owned(),
                artifact,
                closure,
                ExecutionLaunchDriver::DirectItemExecutor,
            )
        };
        let sealed = SealedRootExecutionRequest::capture_finalized(
            &request,
            runtime_ref.clone(),
            &finalized,
            None,
        )?;
        let resume = ResumeContext {
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            ref_bindings: request.ref_bindings.clone(),
            product_selections: request.product_selections.clone(),
            launch_mode: request.launch_mode.clone(),
            parameters: request.parameters.clone(),
            project_context: context.project_context.clone(),
            project_authority: admission.project_authority().clone(),
            lifecycle_authority: lifecycle,
            stable_project_identity: None,
            local_overlay_root: None,
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: context.current_site_id.clone(),
            origin_site_id: context.origin_site_id.clone(),
            requested_by: context.requested_by.clone(),
            execution_hints: context.execution_hints.clone(),
            scheduled_fire: None,
            effective_caps: vec![],
            parent_delegation_caps: None,
            executor_ref: Some(request.executor_ref.clone()),
            runtime_ref: Some(runtime_ref.clone()),
        };
        let mut metadata = RuntimeLaunchMetadata {
            sealed_root_request: Some(sealed),
            resume_context: Some(resume),
            admitted_project_authority: Some(admission.project_authority().clone()),
            launch_driver: Some(driver),
            admitted_artifact_identity: Some(artifact),
            admitted_execution_closure: Some(closure),
            admitted_launch_capsule_schema: Some(ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION),
            ..Default::default()
        };
        // Root real test-node substrate testimony, then bind the actual admitted
        // source/closure. This is an admission object, never process observation.
        drop(guard);
        let substrate = ryeos_app::execution_identity_probe::boot_node_execution_identity(
            &self.state.state_store,
            &self.state.identity,
            &ryeos_app::build_info::get(),
        )?;
        let guard = authority.acquire_shared_guard()?;
        let launch = metadata
            .admitted_launch_authority()?
            .context("missing launch authority")?;
        let contract_digest = match metadata.admitted_artifact_identity.as_ref().unwrap() {
            AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_content_hash,
                ..
            } => runtime_content_hash.clone(),
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                runtime_identity, ..
            } => runtime_identity.runtime_content_hash.clone(),
        };
        let realization = AdmittedExecutionRealization {
            schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
            substrate_identity_hash: substrate.identity_hash.clone(),
            substrate_attestation_hash: substrate.attestation_hash.clone(),
            launch_authority_digest: launch.digest()?,
            effective_definition_digest: finalized.effective_definition_digest().as_str().into(),
            artifact_identity_digest: launch.artifact_identity_digest()?,
            execution_closure_digest: launch.execution_closure_digest()?,
            contract_ref: runtime_ref,
            contract_digest,
            components: vec![],
            properties: BTreeMap::new(),
        };
        metadata.execution_realization_hash = Some(cas.store_object(&realization.to_value()?)?);
        authority.ensure_guard(&guard)?;
        let capsule = metadata
            .admitted_launch_capsule()?
            .context("missing capsule")?;
        let capsule_hash = cas.store_object(&serde_json::to_value(&capsule)?)?;
        assert_eq!(capsule_hash, capsule.content_hash()?);
        let mut terminal = ThreadSnapshotBuilder::new(
            "T-evaluator",
            "T-evaluator",
            &request.kind,
            &request.item_ref,
            &request.executor_ref,
        )
        .status(ThreadStatus::Completed)
        .build();
        terminal.admitted_launch_capsule_hash = Some(capsule_hash);
        terminal.project_authority = capsule.project_authority.clone();
        terminal.managed_runtime_terminal =
            (kind == "graph").then_some(ManagedRuntimeTerminalSupplement {
                outputs: Value::Null,
                warnings: vec![],
            });
        Ok((capsule, terminal))
    }
}

fn result(capsule: &AdmittedLaunchCapsule, graph: bool) -> Result<Value> {
    let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(capsule)?;
    let payload = json!({"schema_version":1,"accepted":true,"candidate_snapshot_hash":"c".repeat(64),
        "base_snapshot_hash":"b".repeat(64),"evidence":{"checked":"finite fixture"}});
    Ok(if graph {
        json!({"success":true,"graph_id":"evaluator-fixture/check",
            "definition_ref":sealed.item_ref(),"effective_definition_digest":sealed.effective_definition_digest().as_str(),
            "graph_run_id":"fixture-run","status":"completed","steps":1,"state":{"internal":1},
            "result":payload,"errors_suppressed":null,"errors":null,"error":null,"cost":null})
    } else {
        payload
    })
}

/// Re-enter the same shared extractor with a capsule loaded from its actual
/// test CAS coordinate. The production callers own terminal-chain/session
/// authentication; this fixture does not manufacture either of those proofs.
fn retained_extract(
    fixture: &Fixture,
    capsule_hash: &str,
    terminal: &ThreadSnapshot,
) -> Result<super::CandidateEvaluationResult> {
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let value = authority
        .cas_store()?
        .get_object(capsule_hash)?
        .context("capsule absent")?;
    let capsule: AdmittedLaunchCapsule = serde_json::from_value(value)?;
    anyhow::ensure!(
        capsule.content_hash()? == capsule_hash
            && terminal.admitted_launch_capsule_hash.as_deref() == Some(capsule_hash),
        "capsule coordinate changed"
    );
    let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule)?;
    let result = super::extract_candidate_evaluation_result(
        &sealed,
        &capsule,
        terminal,
        &"c".repeat(64),
        &"b".repeat(64),
    )
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    authority.ensure_guard(&guard)?;
    Ok(result)
}

#[test]
fn authenticated_graph_and_non_graph_capsules_select_the_same_contract_on_retained_read()
-> Result<()> {
    let fixture = Fixture::new()?;
    for (kind, profile) in [("graph", "graph_run"), ("tool", "tool_run")] {
        let (capsule, mut terminal) = fixture.capsule(kind)?;
        let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule)?;
        assert_eq!(sealed.thread_profile(), profile);
        assert_eq!(terminal.kind_name, profile);
        assert_eq!(sealed.admitted_program_subject()?.kind, kind);
        terminal.result = Some(result(&capsule, kind == "graph")?);
        let first = super::extract_candidate_evaluation_result(
            &sealed,
            &capsule,
            &terminal,
            &"c".repeat(64),
            &"b".repeat(64),
        )
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let retained = retained_extract(&fixture, &capsule.content_hash()?, &terminal)?;
        assert_eq!(first.accepted, retained.accepted);
        assert_eq!(first.evidence, retained.evidence);
        assert!(retained.accepted);
        let testimony = json!({"result":{
            "result_digest":canonical_value_digest(terminal.result.as_ref().unwrap())?,
            "claim_evidence_digest":canonical_value_digest(&first.evidence)?,
        }});
        super::validate_retained_candidate_evaluator_result(
            terminal.result.as_ref().unwrap(),
            &retained,
            &testimony,
        )
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;

        for field in [
            "kind_name",
            "item_ref",
            "executor_ref",
            "admitted_launch_capsule_hash",
        ] {
            let mut changed = serde_json::to_value(&terminal)?;
            changed[field] = match field {
                "kind_name" => json!(kind), // Source kind is not a thread profile.
                "item_ref" => json!("graph:evaluator-fixture/other"),
                "executor_ref" => json!("native:other"),
                _ => json!("e".repeat(64)),
            };
            let changed: ThreadSnapshot = serde_json::from_value(changed)?;
            assert!(
                retained_extract(&fixture, &capsule.content_hash()?, &changed).is_err(),
                "{kind}: {field}"
            );
        }
    }
    Ok(())
}

#[test]
fn authenticated_graph_capsule_refuses_runtime_capsule_and_terminal_substitution() -> Result<()> {
    let fixture = Fixture::new()?;
    let (capsule, mut terminal) = fixture.capsule("graph")?;
    terminal.result = Some(result(&capsule, true)?);
    let hash = capsule.content_hash()?;
    retained_extract(&fixture, &hash, &terminal)?;
    for supplement in [
        None,
        Some(ManagedRuntimeTerminalSupplement {
            outputs: json!({"extra":true}),
            warnings: vec![],
        }),
    ] {
        let mut changed = terminal.clone();
        changed.managed_runtime_terminal = supplement;
        assert!(retained_extract(&fixture, &hash, &changed).is_err());
    }
    let (other, _) = fixture.capsule("tool")?;
    assert!(retained_extract(&fixture, &other.content_hash()?, &terminal).is_err());
    let mut changed_terminal = terminal.clone();
    changed_terminal.admitted_launch_capsule_hash = Some(other.content_hash()?);
    assert!(retained_extract(&fixture, &other.content_hash()?, &changed_terminal).is_err());

    for pointer in [
        "/runtime_ref",
        "/executor_ref",
        "/artifact_identity/runtime_ref",
        "/artifact_identity/executor_ref",
        "/sealed_invocation/kind",
    ] {
        let mut changed = serde_json::to_value(&capsule)?;
        let target = changed
            .pointer_mut(pointer)
            .context("fixture missing capsule field")?;
        *target = if pointer.ends_with("kind") {
            json!("tool_run")
        } else if pointer.ends_with("executor_ref") {
            json!("native:other")
        } else {
            json!("runtime:other")
        };
        let changed: AdmittedLaunchCapsule = serde_json::from_value(changed)?;
        // Capsule digest comparison precedes decoding in both real callers.
        assert_ne!(
            canonical_value_digest(&serde_json::to_value(&changed)?)?,
            hash
        );
        if !pointer.ends_with("kind") {
            assert!(
                SealedRootExecutionRequest::decode_from_admitted_capsule(&changed).is_err(),
                "{pointer}"
            );
        }
    }
    Ok(())
}

#[test]
fn authenticated_graph_retains_whole_result_identity_and_exact_authored_evidence() -> Result<()> {
    let fixture = Fixture::new()?;
    let (capsule, mut terminal) = fixture.capsule("graph")?;
    let original = result(&capsule, true)?;
    terminal.result = Some(original.clone());
    let capsule_hash = capsule.content_hash()?;
    let full_digest = canonical_value_digest(&original)?;
    let first = retained_extract(&fixture, &capsule_hash, &terminal)?;
    let testimony = json!({"result":{
        "result_digest":full_digest,
        "claim_evidence_digest":canonical_value_digest(&first.evidence)?,
    }});
    super::validate_retained_candidate_evaluator_result(&original, &first, &testimony)
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_ne!(full_digest, canonical_value_digest(&original["result"])?);
    let mut payload_digest_testimony = testimony.clone();
    payload_digest_testimony["result"]["result_digest"] =
        json!(canonical_value_digest(&original["result"])?);
    assert!(
        super::validate_retained_candidate_evaluator_result(
            &original,
            &first,
            &payload_digest_testimony
        )
        .is_err()
    );
    for pointer in [
        "/result/candidate_snapshot_hash",
        "/result/base_snapshot_hash",
        "/effective_definition_digest",
        "/definition_ref",
    ] {
        let mut changed = terminal.clone();
        *changed
            .result
            .as_mut()
            .unwrap()
            .pointer_mut(pointer)
            .unwrap() = if pointer.ends_with("definition_ref") {
            json!("graph:evaluator-fixture/other")
        } else {
            json!("e".repeat(64))
        };
        assert!(
            retained_extract(&fixture, &capsule_hash, &changed).is_err(),
            "{pointer}"
        );
    }
    let mut outer = terminal.clone();
    outer.result.as_mut().unwrap()["state"]["internal"] = json!(2);
    assert_eq!(outer.result.as_ref().unwrap()["result"], original["result"]);
    assert_eq!(
        retained_extract(&fixture, &capsule_hash, &outer)?.evidence,
        first.evidence
    );
    assert_ne!(
        canonical_value_digest(outer.result.as_ref().unwrap())?,
        full_digest
    );
    assert!(
        super::validate_retained_candidate_evaluator_result(
            outer.result.as_ref().unwrap(),
            &retained_extract(&fixture, &capsule_hash, &outer)?,
            &testimony,
        )
        .is_err()
    );
    let mut payload = terminal.clone();
    payload.result.as_mut().unwrap()["result"]["evidence"]["checked"] = json!("substituted");
    let changed = retained_extract(&fixture, &capsule_hash, &payload)?;
    assert_ne!(changed.evidence, first.evidence);
    assert_ne!(
        canonical_value_digest(&changed.evidence)?,
        canonical_value_digest(&first.evidence)?
    );
    assert_ne!(
        canonical_value_digest(payload.result.as_ref().unwrap())?,
        full_digest
    );
    assert!(
        super::validate_retained_candidate_evaluator_result(
            payload.result.as_ref().unwrap(),
            &changed,
            &testimony
        )
        .is_err()
    );
    // Even a changed whole-result digest cannot preserve the original authored
    // evidence identity. Both retained coordinates are independently checked.
    let mut changed_testimony = testimony.clone();
    changed_testimony["result"]["result_digest"] =
        json!(canonical_value_digest(payload.result.as_ref().unwrap())?);
    assert!(
        super::validate_retained_candidate_evaluator_result(
            payload.result.as_ref().unwrap(),
            &changed,
            &changed_testimony
        )
        .is_err()
    );
    let mut rejected = terminal.clone();
    rejected.result.as_mut().unwrap()["result"]["accepted"] = json!(false);
    let rejected_result = retained_extract(&fixture, &capsule_hash, &rejected)?;
    let rejected_testimony = json!({"result":{
        "result_digest":canonical_value_digest(rejected.result.as_ref().unwrap())?,
        "claim_evidence_digest":canonical_value_digest(&rejected_result.evidence)?,
    }});
    assert!(
        super::validate_retained_candidate_evaluator_result(
            rejected.result.as_ref().unwrap(),
            &rejected_result,
            &rejected_testimony
        )
        .is_err()
    );
    Ok(())
}
