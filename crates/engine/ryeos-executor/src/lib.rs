pub mod augmentations;
pub mod dispatch;
pub mod dispatch_error;
pub mod dispatch_role;
pub mod execution;
pub mod executor;
pub(crate) mod resolved_config_cache;
pub mod structured_error;

#[cfg(feature = "test-support")]
pub mod test_support {
    use std::collections::BTreeMap;

    use anyhow::{Context as _, Result};

    pub struct AdmittedWorkerSessionFixture {
        pub capsule_hash: String,
        pub prepared_runtime_launch: serde_json::Value,
    }

    /// Exercise the ordinary stopped-managed-runtime selection and workspace
    /// freeze owner for a completed external candidate. This surface is only
    /// for composed downstream tests; it does not manufacture candidate or
    /// lifecycle evidence.
    pub fn freeze_completed_external_candidate(
        state: &ryeos_app::state::AppState,
        provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<ryeos_state::objects::WorkspaceGenerationPair> {
        crate::execution::prepare_stopped_managed_runtime_terminal_project_result(
            state,
            provenance,
            thread_id,
            launch_owner,
        )?
        .context("external candidate freeze produced no retained generation")
    }

    /// Exercise the ordinary terminal workspace close owner after C has been
    /// bound to the freeze journal.
    pub fn finalize_completed_external_candidate(
        state: &ryeos_app::state::AppState,
        provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
        thread_id: &str,
        candidate_snapshot_hash: &str,
    ) -> Result<()> {
        let root_operation = ryeos_app::hosted_operation::begin_hosted_root_operation(
            &state.state_store,
            thread_id,
        )?;
        crate::execution::runner::close_terminal_workspace(
            state,
            provenance.workspace_lifeline().as_ref(),
            thread_id,
            provenance
                .project_authority()
                .terminal_publication()
                .context("external candidate has no terminal publication authority")?,
            Some(candidate_snapshot_hash),
        )?;
        ryeos_app::dedicated_session_service::append_candidate_capture_fact_under_lease(
            state,
            thread_id,
            candidate_snapshot_hash,
            &root_operation,
        )?;
        anyhow::ensure!(
            state
                .state_store
                .bind_dedicated_session_candidate(thread_id, candidate_snapshot_hash)?,
            "external candidate lost its exact freezing-state CAS"
        );
        let session = state
            .state_store
            .dedicated_session(thread_id)?
            .context("external candidate lost its session after freeze")?;
        if session.candidate_disposition
            == ryeos_app::runtime_db::DedicatedCandidateDisposition::RetainedForReview
        {
            // Mirror the ordinary launch finalizer's admitted disposition.
            // Freezing alone does not authorize evaluator adoption.
            state
                .state_store
                .settle_dedicated_candidate_retained_for_review(
                    thread_id,
                    candidate_snapshot_hash,
                )?;
        }
        Ok(())
    }

    /// Admit one signed worker dependency through the ordinary
    /// worker-execution launch preparer and persistent-session admission path.
    /// The selected products are pre-qualified fixture inputs; this helper
    /// cannot manufacture a capsule, source closure, realization, execution
    /// closure, or protocol profile.
    pub fn admit_worker_session_with_selected_products(
        state: &mut ryeos_app::state::AppState,
        worker_execution_ref: &str,
        project_root: &std::path::Path,
        project_snapshot_hash: &str,
        selected_products: ryeos_state::external_content::products::composition::ResolvedExternalProductSelections,
        external_candidate_requirement: ryeos_state::external_execution::admission::ExternalCandidateRequirement,
        qualification_use: ryeos_state::external_execution::admission::ExternalCandidateQualificationUse,
    ) -> Result<String> {
        let tls_roots = vec!["Zml4dHVyZSBjb250cm9sbGVyIFRMUyByb290".to_owned()];
        let controller =
            ryeos_state::external_execution::transport::ExternalControllerTransportContract {
                schema: 2,
                https_origin: "https://controller.example:7443".to_owned(),
                route_contract:
                    ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_ROUTE_CONTRACT
                        .to_owned(),
                tls_root_bundle_digest:
                    ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                        &tls_roots,
                    )?,
                connect_timeout_ms: 5_000,
                request_timeout_ms: 10_000,
                maximum_response_bytes: 1024 * 1024,
                network_inputs: ryeos_state::external_execution::transport::ExternalNetworkInputPolicy {
                    resolver: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/resolv.conf".into(),
                        max_bytes: 64 * 1024,
                    },
                    hosts: ryeos_state::external_execution::transport::ExternalNetworkInputSelection {
                        source: "/etc/hosts".into(),
                        max_bytes: 64 * 1024,
                    },
                },
            };
        admit_worker_session_with_selected_products_and_placement(
            state,
            worker_execution_ref,
            project_root,
            project_snapshot_hash,
            None,
            None,
            selected_products,
            external_candidate_requirement,
            qualification_use,
            controller,
            tls_roots,
            serde_json::json!({"region":"composed-test", "plan":"bounded-fixture"}),
            "fixture-secret",
        )
        .map(|fixture| fixture.capsule_hash)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit_worker_session_with_selected_products_and_placement(
        state: &mut ryeos_app::state::AppState,
        worker_execution_ref: &str,
        project_root: &std::path::Path,
        project_snapshot_hash: &str,
        environment: Option<(&str, &str)>,
        materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
        selected_products: ryeos_state::external_content::products::composition::ResolvedExternalProductSelections,
        external_candidate_requirement: ryeos_state::external_execution::admission::ExternalCandidateRequirement,
        qualification_use: ryeos_state::external_execution::admission::ExternalCandidateQualificationUse,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_roots: Vec<String>,
        placement_settings: serde_json::Value,
        placement_credential: &str,
    ) -> Result<AdmittedWorkerSessionFixture> {
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
            SubjectResolutionAuthority,
        };
        use ryeos_state::external_content::products::composition::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY;

        ryeos_app::external_placement::test_support::install_test_placement_vault(state);
        let operator =
            ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
        let scopes = vec!["*".to_owned()];
        let site = state.threads.site_id().to_owned();
        let context = crate::executor::ExecutionContext {
            principal_fingerprint: operator.principal_id(),
            caller_scopes: scopes.clone(),
            engine: state.engine.clone(),
            plan_ctx: PlanContext {
                requested_by: EffectivePrincipal::Local(Principal {
                    fingerprint: operator.principal_id(),
                    scopes,
                }),
                project_context: ProjectContext::LocalPath {
                    path: project_root.to_path_buf(),
                },
                subject_resolution_authority: SubjectResolutionAuthority::PinnedGeneration {
                    snapshot_hash: project_snapshot_hash.to_owned(),
                },
                current_site_id: site.clone(),
                origin_site_id: site,
                execution_hints: ExecutionHints::default(),
                scheduled_fire: None,
                validate_only: false,
            },
            requested_call: None,
        };
        let verified = crate::executor::resolve_and_verify(
            &state.engine,
            &context.plan_ctx,
            worker_execution_ref,
            Some("worker_execution"),
        )?;
        let applicability =
            crate::dispatch::launch_contract_applicability(worker_execution_ref, &context)?;
        let ref_bindings = environment
            .map(|(reference, _)| {
                BTreeMap::from([("environment".to_owned(), reference.to_owned())])
            })
            .unwrap_or_default();
        let mut prepared = if let Some(materialization) = materialization {
            let generation_identity = state.engine.registered_bundle_generation_fingerprint();
            crate::dispatch::prepare_launch_contract_with_materialization(
                &applicability,
                &verified.resolved,
                &ref_bindings,
                project_root,
                &context,
                crate::execution::launch_preparation::PreparedResolutionCacheContext {
                    cache: &state.resolution_cache,
                    materialization,
                    generation_identity: &generation_identity,
                    plan_context_identity: "external-candidate-signed-fixture",
                },
            )?
        } else {
            crate::dispatch::prepare_launch_contract(
                &applicability,
                &verified.resolved,
                &ref_bindings,
                project_root,
                &context,
            )?
        }
        .context("signed worker execution did not produce a managed launch contract")?;
        let dependency = prepared
            .execution_dependencies
            .get_mut("session_worker")
            .context("worker execution omitted its session_worker dependency")?;
        anyhow::ensure!(
            !dependency
                .resolution
                .composed
                .derived
                .contains_key(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY),
            "worker dependency already contains product-selection authority"
        );
        // Rebind the pre-qualified fixture testimony to the exact signed D0
        // consumer resolved above. This is test-only assembly of the same
        // selector that the product-admission service normally returns; the
        // ordinary admission path below still validates every relationship,
        // consumer, proof, slot, and effective-definition coordinate.
        let slots: Vec<
            ryeos_state::external_content::products::composition::ExternalProductSlotDeclaration,
        > = serde_json::from_value(
            dependency
                .resolution
                .composed
                .composed
                .get("external_product_slots")
                .cloned()
                .context("fixture worker has no signed external product slots")?,
        )?;
        let roots = state
            .engine
            .resolution_roots(Some(project_root.to_path_buf()));
        // Product selection is over the exact pre-selection consumer D0,
        // including its admitted source closure. Derive that projection on a
        // copy: the ordinary admission owner must still be the component that
        // inserts reserved source authority into the launch resolution.
        let mut binding_resolution = dependency.resolution.clone();
        let captured_source = ryeos_app::source_closure_admission::admit_source_closure(
            state,
            &state.engine,
            "worker",
            &mut binding_resolution,
            &roots,
            None,
            None,
        )?
        .context("signed worker source admission produced no source authority")?;
        if let Some(publication) = captured_source.into_publication() {
            publication.publish()?;
        }
        let pre_selection_consumer =
            ryeos_app::external_content_admission::derive_consumer_authority_for_test(
                &binding_resolution,
                &context.plan_ctx.subject_resolution_authority,
            )?;
        let selection_consumer_source = match &pre_selection_consumer {
            ryeos_state::objects::ExternalContentConsumerAuthority::InstalledBundle {
                consumer_ref,
                publisher_fingerprint,
            } => ryeos_state::external_content::products::composition::ResolvedProductConsumerSource::InstalledBundle {
                consumer_ref: consumer_ref.clone(),
                publisher_fingerprint: publisher_fingerprint.clone(),
            },
            ryeos_state::objects::ExternalContentConsumerAuthority::PinnedProject {
                consumer_ref,
                publisher_fingerprint,
                project_snapshot_hash,
                source_closure,
                ..
            } => ryeos_state::external_content::products::composition::ResolvedProductConsumerSource::PinnedProject {
                consumer_ref: consumer_ref.clone(),
                publisher_fingerprint: publisher_fingerprint.clone(),
                project_snapshot_hash: project_snapshot_hash.clone(),
                source_closure: source_closure.clone(),
            },
        };
        let pre_selection_digest =
            ryeos_engine::external_content::pre_product_selection_consumer_digest(
                &binding_resolution,
            )?;
        let mut selection_map = selected_products.into_inner();
        anyhow::ensure!(
            selection_map.len() == slots.len(),
            "fixture selections do not exactly cover signed product slots"
        );
        for slot in slots {
            let selection = selection_map
                .get_mut(&slot.id)
                .with_context(|| format!("fixture selection omits signed slot `{}`", slot.id))?;
            let relationship_resolution = state.engine.effective_resolution_output(
                ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(
                        &slot.relationship_ref,
                    )?,
                    expected_kind: Some("config".to_owned()),
                    project_root: roots
                        .authoritative_project_root()?
                        .map(std::path::Path::to_path_buf),
                    subject_resolution_authority: context
                        .plan_ctx
                        .subject_resolution_authority
                        .clone(),
                },
            )?;
            let relationships = ryeos_state::external_content::products::composition::ProductRelationships::from_value(
                relationship_resolution
                    .composed
                    .composed
                    .get("product_relationships")
                    .cloned()
                    .context("fixture relationship Config has no product_relationships")?,
            )?;
            let relationship = relationships
                .relationships
                .into_iter()
                .find(|relationship| relationship.name == slot.relationship)
                .with_context(|| {
                    format!(
                        "signed relationship `{}` is absent from `{}`",
                        slot.relationship, slot.relationship_ref
                    )
                })?;
            selection.relationship_ref = slot.relationship_ref.clone();
            selection.relationship_name = slot.relationship.clone();
            selection.relationship_raw_content_digest =
                relationship_resolution.root.raw_content_digest.clone();
            selection.relationship = relationship;
            selection.consumer_source = selection_consumer_source.clone();
            selection.pre_selection_effective_definition_digest = pre_selection_digest.clone();
            selection.declaration_id = slot.id.clone();
            selection.declaration.id = slot.id.clone();
            selection.declaration.kind = slot.kind;
            selection.declaration.mount_root = slot.mount_root;
            selection.declaration.mount = slot.mount;
        }
        let selected_products = ryeos_state::external_content::products::composition::ResolvedExternalProductSelections::new(selection_map)?;
        let program = external_candidate_requirement
            .resolve_for_use(Some(&selected_products), &qualification_use)?;
        ryeos_app::external_placement::test_support::install_precontact_binding_with_settings(
            state,
            &program,
            controller,
            tls_roots,
            placement_settings,
            placement_credential.as_bytes(),
        )?;
        dependency.resolution.composed.derived.insert(
            EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
            serde_json::to_value(&selected_products)?,
        );
        binding_resolution.composed.derived.insert(
            EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
            serde_json::to_value(&selected_products)?,
        );
        // The external-content binding is an operator-owned prerequisite, not
        // session admission output. Bind the already retained qualified
        // runtime manifest to the exact source-admitted consumer.
        let consumer = ryeos_app::external_content_admission::derive_consumer_authority_for_test(
            &binding_resolution,
            &context.plan_ctx.subject_resolution_authority,
        )?;
        let runtime_manifest_hash = selected_products
            .get(&external_candidate_requirement.runtime_product_declaration_id)
            .context("fixture selected products omit the admitted external runtime")?
            .manifest_hash
            .clone();
        let operator_grant = ryeos_app::identity::load_verified_authorized_key(
            operator.fingerprint(),
            &state.config.authorized_keys_dir,
            &state.identity,
        )?
        .context("fixture operator has no node-signed grant")?;
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_exclusive_guard(true)?;
        let cas = authority.cas_store()?;
        let manifest = cas
            .get_object(&runtime_manifest_hash)?
            .context("fixture runtime manifest is absent from retained CAS")?;
        let manifest_kind = manifest
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .context("fixture runtime manifest is untyped")?;
        let binding = ryeos_state::objects::ExternalContentBinding::active(
            runtime_manifest_hash,
            manifest_kind.to_owned(),
            consumer,
            state.identity.fingerprint().to_owned(),
            operator.fingerprint().to_owned(),
            operator_grant.source_file_hash.clone(),
        )?;
        let binding_hash = cas.store_object(&binding.to_value()?)?;
        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        state.state_store.with_state_db(|db| {
            db.ensure_current_external_content_binding_epoch(&guard)?;
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &binding_hash,
                None,
                &signer,
                &guard,
            )
        })?;
        drop(guard);
        drop(authority);
        if let Some((_, environment_manifest_hash)) = environment {
            bind_prepared_worker_environment(
                state,
                &prepared,
                &context.plan_ctx.subject_resolution_authority,
                environment_manifest_hash,
            )?;
        }
        let filesystem = prepared.filesystem_authority_ceiling;
        let network = prepared.network_authority_ceiling;
        let resources = prepared.resource_authority_ceiling;
        let publications = crate::execution::persistent_session::admit_or_verify_prepared_sessions(
            state,
            &state.engine,
            &mut prepared,
            &context.plan_ctx.subject_resolution_authority,
            false,
            None,
            &roots,
            filesystem,
            network,
            resources,
        )?;
        publications.publish()?;
        let capsule_hash = prepared
            .admitted_sessions
            .get("session_worker")
            .cloned()
            .context("ordinary session admission produced no capsule")?;
        Ok(AdmittedWorkerSessionFixture {
            capsule_hash,
            prepared_runtime_launch: serde_json::to_value(prepared)?,
        })
    }

    /// Author the fixture's operator-owned environment binding from the exact
    /// content dependency returned by ordinary launch preparation. This does
    /// not select products or admit a session; public-dispatch fixtures can
    /// share the same prerequisite without bypassing those production owners.
    pub fn bind_prepared_worker_environment(
        state: &ryeos_app::state::AppState,
        prepared: &crate::execution::launch_preparation::PreparedRuntimeLaunch,
        subject: &ryeos_engine::contracts::SubjectResolutionAuthority,
        environment_manifest_hash: &str,
    ) -> Result<()> {
        let dependency = prepared
            .content_dependencies
            .get("environment")
            .context("fixture environment omitted its exact content dependency")?;
        let environment_consumer =
            ryeos_app::external_content_admission::derive_consumer_authority_for_test(
                &dependency.resolution.restore(),
                subject,
            )?;
        let operator =
            ryeos_app::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
        let operator_grant = ryeos_app::identity::load_verified_authorized_key(
            operator.fingerprint(),
            &state.config.authorized_keys_dir,
            &state.identity,
        )?
        .context("fixture operator has no node-signed grant")?;
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_exclusive_guard(true)?;
        let cas = authority.cas_store()?;
        let environment_manifest = cas
            .get_object(environment_manifest_hash)?
            .context("fixture environment manifest is absent from retained CAS")?;
        let environment_manifest_kind = environment_manifest
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .context("fixture environment manifest is untyped")?;
        let environment_binding = ryeos_state::objects::ExternalContentBinding::active(
            environment_manifest_hash.to_owned(),
            environment_manifest_kind.to_owned(),
            environment_consumer,
            state.identity.fingerprint().to_owned(),
            operator.fingerprint().to_owned(),
            operator_grant.source_file_hash,
        )?;
        let environment_binding_hash = cas.store_object(&environment_binding.to_value()?)?;
        let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
        state.state_store.with_state_db(|db| {
            db.ensure_current_external_content_binding_epoch(&guard)?;
            db.advance_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &environment_binding.binding_subject_id,
                &environment_binding_hash,
                None,
                &signer,
                &guard,
            )
        })?;
        Ok(())
    }
}
