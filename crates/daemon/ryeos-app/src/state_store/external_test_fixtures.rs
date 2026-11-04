//! External placement test fixtures.
use super::*;

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct TestWorkspaceBinding {
    pub workspace_id: String,
    pub thread_id: String,
    pub launch_owner: Option<String>,
    pub backend_id: Option<String>,
    pub backend_version: Option<String>,
    pub pinned_root_identities: Option<String>,
    pub mount_identity: Option<String>,
    pub workspace_output_partition_identity: Option<String>,
    pub base_output_capture_hash: Option<String>,
}

/// Project root recorded by external placement fixtures that have no real
/// retained workspace.
#[cfg(any(test, feature = "test-support"))]
pub(crate) const EXTERNAL_PLACEMENT_TEST_PROJECT_ROOT: &str = "/external-placement-test";

#[cfg(any(test, feature = "test-support"))]
pub(super) fn external_placement_test_workspace_binding(
    reservation: &runtime_db::external_execution::ExternalAllocationReservation,
) -> Result<TestWorkspaceBinding> {
    Ok(TestWorkspaceBinding {
        workspace_id: reservation.owner.dedicated_session()?.workspace_id.clone(),
        thread_id: reservation.placement_thread_id.clone(),
        launch_owner: None,
        backend_id: Some("fixture".into()),
        backend_version: Some("fixture".into()),
        pinned_root_identities: Some("fixture".into()),
        mount_identity: Some("view:external-composed-controller".into()),
        workspace_output_partition_identity: None,
        base_output_capture_hash: None,
    })
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn external_fixture_launch_metadata(
    project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    prepared_runtime_launch: serde_json::Value,
    turn_start_payload: serde_json::Value,
) -> crate::launch_metadata::RuntimeLaunchMetadata {
    use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};

    let snapshot_hash = project_authority
        .operational_snapshot_projection()
        .expect("external fixture project authority is pinned")
        .to_owned();
    let project_context = ProjectContext::SnapshotHash {
        hash: snapshot_hash.clone(),
    };
    let parameters = serde_json::json!({
        "goal": {
            "session_start_payload": {},
            "turn_start_payload": turn_start_payload
        }
    });
    let resolved_ref_bindings: BTreeMap<String, serde_json::Value> = prepared_runtime_launch
        .get("binding_records")
        .and_then(serde_json::Value::as_object)
        .expect("external fixture prepared launch has binding records")
        .iter()
        .map(|(name, record)| (name.clone(), record.clone()))
        .collect();
    let ref_bindings: BTreeMap<String, String> = resolved_ref_bindings
        .iter()
        .map(|(name, record)| {
            (
                name.clone(),
                record["canonical_ref"]
                    .as_str()
                    .expect("external fixture binding has an exact ref")
                    .to_owned(),
            )
        })
        .collect();
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture_with_project_identity(
        project_context.clone(),
        project_authority.clone(),
    )
    .with_storage_test_parameters(parameters.clone())
    .with_storage_test_ref_bindings(ref_bindings.clone(), resolved_ref_bindings);
    let resume = crate::launch_metadata::ResumeContext {
        kind: "graph_run".into(),
        item_ref: "graph:test/storage-fixture".into(),
        ref_bindings,
        product_selections: Vec::new(),
        launch_mode: "detached".into(),
        parameters,
        project_context,
        project_authority: project_authority.clone(),
        lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
        stable_project_identity: None,
        local_overlay_root: None,
        original_snapshot_hash: Some(snapshot_hash),
        original_pushed_head_ref: None,
        state_root: None,
        current_site_id: "site:test".into(),
        origin_site_id: "site:test".into(),
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: "session:test".into(),
            scopes: Vec::new(),
        }),
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        effective_caps: Vec::new(),
        parent_delegation_caps: None,
        executor_ref: Some("native:storage-fixture".into()),
        runtime_ref: Some("runtime:storage-fixture".into()),
    };
    let runtime_key = lillux::crypto::SigningKey::from_bytes(&[21; 32]);
    let runtime_document =
        lillux::signature::sign_content("runtime fixture\n", &runtime_key, "#", None);
    let runtime_header = lillux::signature::parse_signature_line(
        runtime_document.lines().next().expect("runtime signature"),
        "#",
        None,
    )
    .expect("runtime signature header");
    let protocol_key = lillux::crypto::SigningKey::from_bytes(&[22; 32]);
    let protocol_document =
        lillux::signature::sign_content("protocol fixture\n", &protocol_key, "#", None);
    let protocol_header = lillux::signature::parse_signature_line(
        protocol_document
            .lines()
            .next()
            .expect("protocol signature"),
        "#",
        None,
    )
    .expect("protocol signature header");
    let executor_hash = "7".repeat(64);
    let metadata = crate::launch_metadata::RuntimeLaunchMetadata {
        launch_driver: Some(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime),
        resume_context: Some(resume),
        sealed_root_request: Some(sealed),
        admitted_project_authority: Some(project_authority),
        admitted_artifact_identity: Some(
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref: "runtime:storage-fixture".into(),
                runtime_content_hash: runtime_header.content_hash,
                runtime_signer_fingerprint: runtime_header.signer_fingerprint,
                protocol_ref: "protocol:test/fixture".into(),
                protocol_content_hash: protocol_header.content_hash,
                protocol_signer_fingerprint: protocol_header.signer_fingerprint,
                executor_ref: "native:storage-fixture".into(),
                executor_content_hash: executor_hash.clone(),
                executor_bundle_manifest_hash: "8".repeat(64),
                executor_bundle_signer_fingerprint: "9".repeat(64),
            },
        ),
        admitted_launch_capsule_schema: Some(
            ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
        ),
        execution_realization_hash: Some("a".repeat(64)),
        admitted_execution_closure: Some(
            ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
                prepared_runtime_launch,
                runtime_descriptor_document: runtime_document,
                protocol_descriptor_document: protocol_document,
                executor_blob_hash: executor_hash,
            },
        ),
        ..Default::default()
    };
    metadata
        .validate()
        .expect("valid external fixture launch metadata");
    metadata
}

impl StateStore {
    /// Install the composed placement as a chain root admitted through the
    /// exact root launch metadata, so the production placement owner finds
    /// the same admitted launch capsule an ordinary launch would record.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_admitted_external_placement_test_fixture(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        root_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        self.install_external_placement_test_fixture_with_workspace_inner(
            reservation,
            Some(binding),
            std::path::Path::new(EXTERNAL_PLACEMENT_TEST_PROJECT_ROOT),
            "fp:operator",
            true,
            false,
            Some(root_launch_metadata),
            |_| external_placement_test_workspace_binding(reservation),
        )
    }

    #[cfg(test)]
    pub(crate) fn install_external_placement_test_fixture(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
    ) -> Result<()> {
        self.install_external_placement_test_fixture_with_workspace(
            reservation,
            binding,
            std::path::Path::new(EXTERNAL_PLACEMENT_TEST_PROJECT_ROOT),
            |_| external_placement_test_workspace_binding(reservation),
        )
    }

    #[cfg(test)]
    pub(crate) fn install_external_placement_test_fixture_with_workspace(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        project_root: &std::path::Path,
        prepare_workspace: impl FnOnce(&str) -> Result<TestWorkspaceBinding>,
    ) -> Result<()> {
        self.install_external_placement_test_fixture_with_workspace_inner(
            reservation,
            Some(binding),
            project_root,
            "fp:operator",
            false,
            false,
            None,
            prepare_workspace,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_external_placement_test_fixture_with_workspace_owner(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        project_root: &std::path::Path,
        owner_principal: &str,
        root_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        prepare_workspace: impl FnOnce(&str) -> Result<TestWorkspaceBinding>,
    ) -> Result<()> {
        self.install_external_placement_test_fixture_with_workspace_inner(
            reservation,
            Some(binding),
            project_root,
            owner_principal,
            true,
            false,
            Some(root_launch_metadata),
            prepare_workspace,
        )
    }

    /// Install only the ordinary thread/session/workspace prerequisites for a
    /// real external-placement start. Unlike the composed fixture above this
    /// does not reserve allocation authority or create a provider occurrence.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_external_session_test_fixture_with_workspace_owner(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        project_root: &std::path::Path,
        owner_principal: &str,
        retained_for_review: bool,
        root_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        prepare_workspace: impl FnOnce(&str) -> Result<TestWorkspaceBinding>,
    ) -> Result<()> {
        self.install_external_placement_test_fixture_with_workspace_inner(
            reservation,
            None,
            project_root,
            owner_principal,
            true,
            retained_for_review,
            Some(root_launch_metadata),
            prepare_workspace,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn install_external_placement_test_fixture_with_workspace_inner(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        binding: Option<
            &crate::node_config::sections::external_execution::RetainedExternalExecutionBinding,
        >,
        project_root: &std::path::Path,
        owner_principal: &str,
        placement_is_chain_root: bool,
        retained_for_review: bool,
        root_launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        prepare_workspace: impl FnOnce(&str) -> Result<TestWorkspaceBinding>,
    ) -> Result<()> {
        let source_root_id = if placement_is_chain_root {
            reservation.placement_thread_id.clone()
        } else {
            "T-test-root".to_owned()
        };
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:external-placement-test".to_owned(),
            Some(project_root.to_path_buf()),
            reservation.base_snapshot_hash.clone(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )?
        .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)?;
        let captured_history_policy = ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: "worker_execution:test/external-candidate".to_owned(),
            item_content_hash: "a".repeat(64),
            item_signer_fingerprint: Some("b".repeat(64)),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: "c".repeat(64),
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        };
        let placement_executor_ref = root_launch_metadata
            .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
            .transpose()?
            .flatten()
            .map(|capsule| capsule.executor_ref)
            .unwrap_or_else(|| "runtime:test/external-candidate".to_owned());
        if !placement_is_chain_root {
            self.create_thread_for_test(&NewThreadRecord {
                thread_id: source_root_id.clone(),
                chain_root_id: source_root_id.clone(),
                kind: "worker_execution".to_owned(),
                item_ref: "worker_execution:test/external-candidate".to_owned(),
                executor_ref: placement_executor_ref.clone(),
                launch_mode: "detached".to_owned(),
                current_site_id: "site:composed-test".to_owned(),
                origin_site_id: "site:composed-test".to_owned(),
                upstream_thread_id: None,
                requested_by: Some(owner_principal.to_owned()),
                project_root: Some(project_root.to_path_buf()),
                project_authority: project_authority.clone(),
                base_project_snapshot_hash: Some(reservation.base_snapshot_hash.clone()),
                usage_subject: None,
                usage_subject_asserted_by: None,
                captured_history_policy: Some(captured_history_policy.clone()),
            })?;
        }
        let placement_thread = NewThreadRecord {
            thread_id: reservation.placement_thread_id.clone(),
            chain_root_id: source_root_id.clone(),
            kind: "worker_execution".to_owned(),
            item_ref: "worker_execution:test/external-candidate".to_owned(),
            executor_ref: placement_executor_ref,
            launch_mode: "detached".to_owned(),
            current_site_id: "site:composed-test".to_owned(),
            origin_site_id: "site:composed-test".to_owned(),
            upstream_thread_id: (!placement_is_chain_root).then_some(source_root_id.clone()),
            requested_by: Some(owner_principal.to_owned()),
            project_root: Some(project_root.to_path_buf()),
            project_authority: project_authority.clone(),
            base_project_snapshot_hash: Some(reservation.base_snapshot_hash.clone()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: placement_is_chain_root.then_some(captured_history_policy),
        };
        if let Some(metadata) = root_launch_metadata {
            self.create_root_thread_for_test_with_launch_metadata(&placement_thread, metadata)?;
        } else {
            self.create_thread_for_test(&placement_thread)?;
        }
        self.mark_thread_running(
            &reservation.placement_thread_id,
            Some(&reservation.base_snapshot_hash),
        )?;
        if root_launch_metadata.is_none() {
            self.seed_launch_metadata(
                &reservation.placement_thread_id,
                &external_fixture_launch_metadata(
                    project_authority,
                    serde_json::json!({
                        "binding_records": {},
                        "required_secrets": [],
                        "admitted_sessions": {},
                    }),
                    serde_json::json!({}),
                ),
            )?;
        }
        let launch_claim = self
            .claim_thread_launch_active(
                &reservation.placement_thread_id,
                "claim-external-composed-controller",
                "daemon:external-composed-test",
            )?
            .context("composed external fixture lost its launch claim")?;
        self.reserve_execution_workspace(
            &reservation.owner.dedicated_session()?.workspace_id,
            &reservation.base_snapshot_hash,
            project_root
                .to_str()
                .context("composed workspace project path is not UTF-8")?,
        )?;
        self.transition_execution_workspace(
            &reservation.owner.dedicated_session()?.workspace_id,
            &[runtime_db::WorkspaceState::Reserved],
            runtime_db::WorkspaceState::Constructing,
            None,
        )?;
        self.claim_execution_workspace_construction(
            &reservation.owner.dedicated_session()?.workspace_id,
            &reservation.placement_thread_id,
            &launch_claim.claimed_by,
        )?;
        let workspace = prepare_workspace(&launch_claim.claimed_by)?;
        anyhow::ensure!(
            workspace.workspace_id == reservation.owner.dedicated_session()?.workspace_id
                && workspace.thread_id == reservation.placement_thread_id,
            "composed workspace preparation changed its durable owner"
        );
        let view_identity = workspace
            .mount_identity
            .as_deref()
            .context("composed workspace preparation omitted its view identity")?;
        let workspace_binding = runtime_db::RuntimeWorkspaceBinding {
            workspace_id: reservation.owner.dedicated_session()?.workspace_id.clone(),
            view_identity: view_identity.to_owned(),
            borrower_launch_owner: launch_claim.owner.clone(),
        };
        let g = self.lock()?;
        let profile = format!("P-{}", reservation.placement_thread_id);
        g.runtime_db
            .create_credential_profile(runtime_db::NewCredentialProfile {
                profile_id: &profile,
                owner_principal,
                home_id: "external-placement-test",
            })?;
        g.runtime_db.acquire_credential_profile(
            &profile,
            owner_principal,
            &reservation.owner.dedicated_session()?.worker_instance_id,
        )?;
        g.runtime_db
            .admit_dedicated_session(runtime_db::NewDedicatedSession {
                placement_thread_id: &reservation.placement_thread_id,
                chain_root_id: &source_root_id,
                owner_principal,
                admitted_capsule_hash: &reservation.admitted_capsule_hash,
                workspace_id: &reservation.owner.dedicated_session()?.workspace_id,
                candidate_required: true,
                candidate_disposition: if retained_for_review {
                    runtime_db::DedicatedCandidateDisposition::RetainedForReview
                } else {
                    runtime_db::DedicatedCandidateDisposition::OwnerDecision
                },
                credential_profile_id: &profile,
                credential_generation: 1,
                credential_lock_owner: &reservation.owner.dedicated_session()?.worker_instance_id,
            })?;
        anyhow::ensure!(
            workspace.launch_owner.as_deref().is_none()
                || workspace.launch_owner.as_deref() == Some(launch_claim.claimed_by.as_str()),
            "composed workspace preparation changed its launch owner"
        );
        g.runtime_db.bind_workspace(runtime_db::WorkspaceBinding {
            workspace_id: &workspace.workspace_id,
            thread_id: &workspace.thread_id,
            launch_owner: Some(&launch_claim.claimed_by),
            backend_id: workspace.backend_id.as_deref(),
            backend_version: workspace.backend_version.as_deref(),
            pinned_root_identities: workspace.pinned_root_identities.as_deref(),
            mount_identity: workspace.mount_identity.as_deref(),
            workspace_output_partition_identity: workspace
                .workspace_output_partition_identity
                .as_deref(),
            base_output_capture_hash: workspace.base_output_capture_hash.as_deref(),
        })?;
        if let Some(binding) = binding {
            g.runtime_db
                .reserve_external_allocation(reservation, binding)?;
        }
        drop(g);
        self.bind_thread_workspace(&reservation.placement_thread_id, &workspace_binding)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn external_candidate_test_authority(&self) -> &ryeos_state::PinnedStateAuthority {
        &self.state_authority
    }

    #[cfg(test)]
    pub(crate) fn claim_external_candidate_import_test_fixture(
        &self,
        placement: &str,
        seal_sequence: u64,
        seal_digest: &str,
    ) -> Result<()> {
        match self.lock()?.runtime_db.claim_external_candidate_import(
            placement,
            seal_sequence,
            seal_digest,
        )? {
            runtime_db::external_execution::ExternalCandidateImportClaim::Reconcile(_) => Ok(()),
            runtime_db::external_execution::ExternalCandidateImportClaim::AlreadyApplied(_) => {
                bail!("external candidate import test fixture was already applied")
            }
        }
    }
}
