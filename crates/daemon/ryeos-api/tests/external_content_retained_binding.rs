//! Exact binding reuse keeps source testimony and destination admission separate.
mod test_state;

use std::sync::Arc;

use base64::Engine as _;
use ryeos_api::handlers::external_content_import;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity, WildcardPolicy};
use ryeos_app::node_policy::sections::external_content::{
    ExternalContentImportLimits, ExternalContentImportPolicyRecord, ManagedExternalContentPolicy,
};
use ryeos_app::operator_external_content::{ImportRequest, RetainedBindingImportRequest};
use ryeos_app::state::AppState;
use ryeos_state::objects::{ExternalContentBinding, ExternalContentConsumerAuthority};
use serde_json::json;

fn fixture(
    large_tier: bool,
) -> (
    tempfile::TempDir,
    Arc<AppState>,
    HandlerContext,
    String,
    String,
) {
    let (tmp, mut state) = test_state::build_test_state();
    let closure = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
        .unwrap()
        .clone();
    state.node_policy = Arc::new(
        ryeos_app::node_policy::NodePolicySnapshot::from_test_records(vec![
            Arc::new(closure),
            Arc::new(ExternalContentImportPolicyRecord {
                schema: 1,
                roots: Default::default(),
                limits: ExternalContentImportLimits {
                    max_depth: 4,
                    max_entries: 16,
                    max_file_bytes: 1024,
                    max_total_bytes: 4096,
                    store_budget_bytes: 8192,
                    minimum_free_bytes: 1,
                },
                managed_activation: ManagedExternalContentPolicy {
                    enabled: false,
                    limits: None,
                },
            }),
        ]),
    );
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let scopes = vec!["ryeos.execute.service.external-content/import".to_owned()];
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "test operator",
        state.identity.fingerprint(),
        "2026-09-07T00:00:00Z",
        state.identity.signing_key(),
        WildcardPolicy::Reject,
    )
    .unwrap();
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let blob = cas.store_blob(b"exact input").unwrap();
    let kind = if large_tier {
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
    } else {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
    };
    let schema = if large_tier {
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA
    } else {
        ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA
    };
    let manifest = cas
        .store_object(&json!({
            "kind":kind, "schema":schema,
            "entries":[{"path":"content", "kind":"file", "mode":420, "blob_hash":blob, "size":11}],
            "entry_count":1, "total_bytes":11,
        }))
        .unwrap();
    let grant = ryeos_app::identity::load_verified_authorized_key(
        operator.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
    let binding = ExternalContentBinding::active(
        manifest.clone(),
        kind.to_owned(),
        ExternalContentConsumerAuthority::installed_bundle(
            "tool:test/source".to_owned(),
            "a".repeat(64),
        )
        .unwrap(),
        state.identity.fingerprint().to_owned(),
        operator.fingerprint().to_owned(),
        grant.source_file_hash,
    )
    .unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
        .state_store
        .with_state_db(|db| {
            db.ensure_current_external_content_binding_epoch(&guard)?;
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &binding_hash,
                None,
                &signer,
                &guard,
            )
        })
        .unwrap();
    drop(guard);
    (tmp, Arc::new(state), context, binding_hash, manifest)
}

fn request(hash: &str, maximum_bytes: u64) -> ImportRequest {
    ImportRequest::RetainedBinding(RetainedBindingImportRequest {
        binding_hash: hash.to_owned(),
        maximum_bytes,
    })
}

#[test]
fn portable_content_uses_the_admitted_project_generation_for_preview_and_launch() {
    use ryeos_app::external_content_admission::{
        admit_portable_content_dependency_in_publication, preview_portable_content_dependency,
    };
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace, SubjectResolutionAuthority};
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };

    let (_tmp, state, _context, source_binding_hash, manifest_hash) = fixture(false);
    let resolution = ResolutionOutput {
        root: ResolvedAncestor {
            requested_id: "test/environment".into(),
            resolved_ref: "config:test/environment".into(),
            source_path: "/fixture/.ai/config/test/environment.yaml".into(),
            source_space: ItemSpace::Project,
            source_root: ItemSourceRoot::Project,
            trust_class: TrustClass::TrustedProject,
            signer_fingerprint: Some("a".repeat(64)),
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            source_content_digest: "b".repeat(64),
            raw_content_digest: "c".repeat(64),
        },
        ancestors: Vec::new(),
        references_edges: Vec::new(),
        referenced_items: Vec::new(),
        step_outputs: Default::default(),
        effective_trust_class: TrustClass::TrustedProject,
        composed: KindComposedView::identity(json!({
            "external_content": [{
                "id":"content", "kind":"tree", "mode":"pinned",
                "digest":manifest_hash, "mount_root":"project", "mount":"content"
            }]
        })),
    };
    let snapshot_hash = "d".repeat(64);
    let consumer = ExternalContentConsumerAuthority::pinned_project(
        resolution.root.resolved_ref.clone(),
        "a".repeat(64),
        snapshot_hash.clone(),
        ryeos_engine::external_content::pre_external_realization_consumer_digest(&resolution)
            .unwrap(),
        None,
    )
    .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let cas = authority.cas_store().unwrap();
    let source =
        ExternalContentBinding::from_value(&cas.get_object(&source_binding_hash).unwrap().unwrap())
            .unwrap();
    let binding = ExternalContentBinding::active(
        manifest_hash,
        source.manifest_kind,
        consumer,
        source.target_node_fingerprint,
        source.authorized_by,
        source.authorizer_grant_digest,
    )
    .unwrap();
    let binding_hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state
        .state_store
        .with_state_db(|db| {
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &binding_hash,
                None,
                &signer,
                &guard,
            )
        })
        .unwrap();
    drop(guard);
    let policy = ryeos_engine::runtime_registry::LaunchContentExternalPolicy {
        allowed_mount_roots: vec![ryeos_state::objects::ExternalContentMountRoot::Project],
        max_declarations: 1,
        large_content_max_total_bytes: None,
    };
    // COW uses its current admitted generation, not its original base or a
    // mutable path. Both preview and launch must select the same exact binding.
    for subject in [
        SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.clone(),
        },
        SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: "e".repeat(64),
            current_operational_generation: snapshot_hash,
        },
    ] {
        let preview =
            preview_portable_content_dependency(&state, &resolution, &policy, &subject).unwrap();
        assert!(preview.ready_for_admission);
        assert_eq!(
            preview.declarations[0].binding_digest.as_deref(),
            Some(binding_hash.as_str())
        );
        let mut admitted = resolution.clone();
        let mut publication = None;
        admit_portable_content_dependency_in_publication(
            &state,
            &mut admitted,
            &policy,
            &subject,
            None,
            &mut publication,
        )
        .unwrap();
        assert!(publication.is_some());
    }
    for subject in [
        SubjectResolutionAuthority::Projectless,
        SubjectResolutionAuthority::LiveFs,
    ] {
        assert!(
            preview_portable_content_dependency(&state, &resolution, &policy, &subject).is_err()
        );
        assert!(
            admit_portable_content_dependency_in_publication(
                &state,
                &mut resolution.clone(),
                &policy,
                &subject,
                None,
                &mut None,
            )
            .is_err()
        );
    }
    let other = SubjectResolutionAuthority::PinnedGeneration {
        snapshot_hash: "f".repeat(64),
    };
    assert!(
        !preview_portable_content_dependency(&state, &resolution, &policy, &other)
            .unwrap()
            .ready_for_admission
    );
    let error = admit_portable_content_dependency_in_publication(
        &state,
        &mut resolution.clone(),
        &policy,
        &other,
        None,
        &mut None,
    )
    .err()
    .expect("another snapshot must not borrow the admitted consumer binding");
    assert!(error.downcast_ref::<ryeos_app::external_content_admission::ExternalContentBindingUnavailable>().is_some());
}

#[tokio::test]
async fn exact_binding_reuse_preserves_manifest_and_independent_durable_stage() {
    for large_tier in [false, true] {
        let (_tmp, state, context, binding_hash, manifest_hash) = fixture(large_tier);
        let first = external_content_import::handle(
            request(&binding_hash, 11),
            context.clone(),
            Arc::clone(&state),
        )
        .await
        .unwrap();
        let second = external_content_import::handle(
            request(&binding_hash, 11),
            context.clone(),
            Arc::clone(&state),
        )
        .await
        .unwrap();
        assert_eq!(first["manifest_hash"], manifest_hash);
        assert_eq!(first["total_bytes"], 11);
        assert_eq!(first["request_digest"], second["request_digest"]);
        assert_ne!(first["staging_id"], second["staging_id"]);
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let binding =
            ExternalContentBinding::from_value(&cas.get_object(&binding_hash).unwrap().unwrap())
                .unwrap();
        ryeos_app::operator_external_content::release(
            Arc::clone(&state),
            context.clone(),
            ryeos_app::operator_external_content::ReleaseRequest {
                binding_subject_id: binding.binding_subject_id,
            },
        )
        .await
        .unwrap();
        assert!(
            external_content_import::handle(
                request(&binding_hash, 11),
                context,
                Arc::clone(&state)
            )
            .await
            .is_err()
        );
        // Receipt is reloaded from durable recovery, not a retained in-memory
        // stage handle. Source release cannot turn its admitted bytes into a
        // destination grant, nor invalidate the separate completed import.
        let guard = authority.acquire_shared_guard().unwrap();
        let stage = authority
            .require_recovery()
            .unwrap()
            .open_durable_cas_upload_admitted(
                &guard,
                first["staging_id"].as_str().unwrap(),
                &binding.authorized_by,
            )
            .unwrap();
        stage.ensure_protects_object(&manifest_hash).unwrap();
        assert!(stage.admitted_target_hash().is_none());
        stage
            .ensure_publication_contract(
                &ryeos_state::DurableCasPublicationKey::external_content_import(
                    first["request_digest"].as_str().unwrap(),
                )
                .unwrap(),
                None,
            )
            .unwrap();
    }
}

#[tokio::test]
async fn binding_reuse_refuses_nonowner_remote_stale_grant_and_byte_overclaims() {
    let (_tmp, state, context, hash, _) = fixture(false);
    for limit in [0, 10, 4097] {
        assert!(
            external_content_import::handle(
                request(&hash, limit),
                context.clone(),
                Arc::clone(&state)
            )
            .await
            .is_err()
        );
    }
    let mut foreign = context.clone();
    foreign.fingerprint = format!("fp:{}", "b".repeat(64));
    assert!(
        external_content_import::handle(request(&hash, 11), foreign, Arc::clone(&state))
            .await
            .is_err()
    );
    let mut remote = context.clone();
    remote.authorized_key_class = Some(AuthorizedKeyPrincipalClass::RemoteOperator);
    remote.authenticated_origin_site_id = Some("site:other".to_owned());
    assert!(
        external_content_import::handle(request(&hash, 11), remote, Arc::clone(&state))
            .await
            .is_err()
    );
    assert!(
        external_content_import::handle(
            request(&"d".repeat(64), 11),
            context.clone(),
            Arc::clone(&state)
        )
        .await
        .is_err()
    );
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    ryeos_app::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &context.scopes,
        "changed grant",
        state.identity.fingerprint(),
        "2026-09-07T00:00:01Z",
        state.identity.signing_key(),
        WildcardPolicy::Reject,
    )
    .unwrap();
    let error = external_content_import::handle(request(&hash, 11), context, state)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("grant changed"));
}

#[tokio::test]
async fn binding_reuse_rejects_lying_payloads_in_both_manifest_tiers() {
    for large_tier in [false, true] {
        let (_tmp, state, context, hash, manifest_hash) = fixture(large_tier);
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let previous =
            ExternalContentBinding::from_value(&cas.get_object(&hash).unwrap().unwrap()).unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let mut manifest = cas.get_object(&manifest_hash).unwrap().unwrap();
        manifest["entries"][0]["size"] = json!(12);
        manifest["total_bytes"] = json!(12);
        let manifest = cas.store_object(&manifest).unwrap();
        let binding = ExternalContentBinding::active(
            manifest,
            previous.manifest_kind,
            previous.consumer,
            previous.target_node_fingerprint,
            previous.authorized_by,
            previous.authorizer_grant_digest,
        )
        .unwrap();
        let hash = cas.store_object(&binding.to_value().unwrap()).unwrap();
        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        state
            .state_store
            .with_state_db(|db| {
                db.advance_generic_head_ref(
                    ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                    &binding.binding_subject_id,
                    &hash,
                    None,
                    &signer,
                    &guard,
                )
            })
            .unwrap();
        drop(guard);
        let error = external_content_import::handle(request(&hash, 12), context, state)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("size"));
    }
}
