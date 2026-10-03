//! Pre-contact retention of an exact trusted pooled-session closure.
//!
//! This value is deliberately not a launch permit or a durable cleanup record.
//! The capture derives the current custody record from original descriptor
//! owners, but does not insert it into a journal or admit an application launch.
//! Until that join, financial prebinding and driver evidence exist, the parent's
//! executable prerequisite guard continues to refuse launch.

use std::ffi::OsStr;

use super::*;
use crate::execution::external_content::BoundExternalRealizations;
use crate::execution::source_closure::BoundSourceClosure;
use ryeos_app::runtime_db::process_resource_custody::{
    MAX_PROCESS_CUSTODY_MATERIALIZATIONS, ProcessCustodyPhysicalState, ProcessMaterializationCache,
    ProcessMaterializationCustody, ProcessResourceRuntimeCustody, ProcessWorkspaceCustody,
};
use ryeos_app::temp_dir_guard::TempDirGuard;

/// Owns the ORIGINAL workspace guard and complete bindings before any contact.
/// No Clone, serde decoder, raw-record constructor, or process release method.
/// It remains unwired while the parent's executable prerequisites are closed.
pub(super) struct PreparedTrustedSessionCustody {
    /// Retention data only; decoding an equivalent record admits nothing.
    runtime_custody: ProcessResourceRuntimeCustody,
    verified_inputs: ryeos_app::persistent_session_admission::VerifiedTrustedPooledSessionInputs,
    state_store: Arc<ryeos_app::state_store::StateStore>,
    state_authority: Arc<ryeos_state::PinnedStateAuthority>,
    workspace: Arc<TempDirGuard>,
    external: Option<Arc<BoundExternalRealizations>>,
    source: Option<Arc<BoundSourceClosure>>,
    cleanup_agreement: AdmittedTrustedResourceCleanupContract,
}

impl PreparedTrustedSessionCustody {
    /// Capture only already-redeemed private bindings. The caller must retain
    /// these SAME Arc-backed original owners in its pool slot before validation. The capsule and retained
    /// signed protocol are reopened in the SAME current StateStore and trust
    /// domain; caller-provided digests cannot mint this value.
    pub(super) fn capture(
        state: &AppState,
        capsule_hash: &str,
        workspace_path: &Path,
        workspace: Arc<TempDirGuard>,
        external: Option<Arc<BoundExternalRealizations>>,
        source: Option<Arc<BoundSourceClosure>>,
    ) -> Result<Self> {
        let verified_inputs =
            ryeos_app::persistent_session_admission::VerifiedTrustedPooledSessionInputs::verify(
                state,
                capsule_hash,
            )?;
        let capsule = verified_inputs.capsule().clone();
        let session = retained_session_protocol(&state.engine, &capsule)?;
        let node = state
            .node_policy
            .require::<ryeos_app::node_policy::sections::execution::NodeExecutionAdmissionPolicy>(
        )?;
        let cleanup_agreement =
            admit_session_resource_cleanup_contract(node.resource_authority.as_ref(), &session)?
                .context("trusted pooled custody requires exact node/product cleanup agreement")?;
        ensure!(
            cleanup_agreement.cleanup_contract_digest == verified_inputs.cleanup_contract_digest(),
            "trusted pooled custody differs from the same verified cleanup agreement"
        );
        // Verified inputs remain read-only, never executable admission.
        let resolution = verified_inputs.exact().resolution_output.restore();

        ensure!(
            workspace.owns_effective_path(workspace_path),
            "trusted pooled workspace differs from its original guard"
        );
        let original_workspace = workspace.owned_scratch_root()?;
        original_workspace.ensure_path_binding()?;
        let workspace_identity = original_workspace.identity()?;
        let state_authority = Arc::new(state.state_store.pinned_state_authority()?);
        let runtime = state_authority.runtime_directory();
        let workspace_custody = capture_owned_scratch_custody(runtime, &workspace)?;
        let cache = required_child(runtime, "cache")?;

        let expected_external = resolution
            .composed
            .derived
            .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .map(ryeos_engine::external_realization::RealizedExternalContentSet::from_value)
            .transpose()?
            .unwrap_or_default();
        let mut materializations = BTreeSet::new();
        match external.as_ref() {
            Some(bound) => {
                let (actual, generations) = bound.custody_records();
                ensure!(
                    actual == &expected_external
                        && !actual.is_empty()
                        && actual.iter().len() == generations.len(),
                    "trusted pooled external bindings differ from admitted complete set"
                );
                ensure!(
                    bound.private_workspace_identity() == Some(&workspace_identity),
                    "trusted pooled external delivery differs from original private workspace"
                );
                let expected_root = required_child(runtime, "external-content-cache")?;
                for (entry, (root, generation)) in actual.iter().zip(generations) {
                    require_same_directory(root, &expected_root)?;
                    require_same_directory(
                        generation,
                        &required_child(root, &entry.manifest_hash)?,
                    )?;
                    materializations.insert(ProcessMaterializationCustody {
                        cache: ProcessMaterializationCache::ExternalContent,
                        manifest_hash: entry.manifest_hash.clone(),
                    });
                }
            }
            None => ensure!(
                expected_external.is_empty(),
                "trusted pooled admitted external bindings are missing"
            ),
        }

        let projection = resolution
            .composed
            .derived
            .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
            .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
            .transpose()?;
        match (source.as_ref(), projection.as_ref()) {
            (Some(bound), Some(projection)) => {
                let (records, root) = bound.custody_records();
                records.validate_projection(projection)?;
                ensure!(
                    capsule.source_binding_hash.as_deref() == Some(records.binding_hash()),
                    "trusted pooled source binding differs from admitted capsule"
                );
                ensure!(
                    bound.private_workspace_identity() == Some(&workspace_identity),
                    "trusted pooled source delivery differs from original private workspace"
                );
                require_same_directory(root, &required_child(&cache, "source-closures")?)?;
                require_same_directory(
                    bound.source_directory(),
                    &required_child(root, records.content_manifest_hash())?,
                )?;
                records.verify_tree(bound.source_directory())?;
                materializations.insert(ProcessMaterializationCustody {
                    cache: ProcessMaterializationCache::SourceClosure,
                    manifest_hash: records.content_manifest_hash().to_owned(),
                });
            }
            (None, None) => ensure!(
                capsule.source_binding_hash.is_none(),
                "trusted pooled capsule source identity has no redeemed binding"
            ),
            _ => bail!("trusted pooled source binding/projection presence differs"),
        }
        ensure!(
            materializations.len() <= MAX_PROCESS_CUSTODY_MATERIALIZATIONS,
            "trusted pooled materialization custody exceeds its finite bound"
        );
        let runtime_custody = ProcessResourceRuntimeCustody {
            version: ProcessResourceRuntimeCustody::VERSION,
            session_capsule_hash: capsule_hash.to_owned(),
            execution_realization_hash: capsule.execution_realization_hash,
            source_binding_hash: capsule.source_binding_hash,
            materializations: materializations.into_iter().collect(),
            workspace: workspace_custody,
            physical_state: ProcessCustodyPhysicalState::Retained,
        };
        runtime_custody.validate()?;
        let captured = Self {
            runtime_custody,
            verified_inputs,
            state_store: Arc::clone(&state.state_store),
            state_authority,
            workspace,
            external,
            source,
            cleanup_agreement,
        };
        // Recheck after complete binding validation; no stale runtime/parent/
        // scratch incarnation may be published as the original capture.
        captured.retained_runtime_custody(state)?;
        Ok(captured)
    }

    /// Consume this actual capture into the SAME original pooled funding slot.
    /// The constructed record is retention/intent data, never native admission.
    pub(super) fn stage_original_funding(
        self,
        state: &AppState,
        original_start: &ryeos_app::persistent_session::PersistentSessionPooledStart,
        allocation_scope: &ryeos_state::objects::AdmittedAccountingScope,
        owner_coordinate: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        // Preserve all original physical and pinned owners before validation or
        // the first RuntimeDb/ledger write. No replacement materialization.
        let mut owners: Vec<Box<dyn Send + Sync>> = vec![
            Box::new(Arc::clone(&self.workspace)),
            Box::new(Arc::clone(&self.state_store)),
            Box::new(Arc::clone(&self.state_authority)),
        ];
        if let Some(external) = &self.external {
            owners.push(Box::new(Arc::clone(external)));
        }
        if let Some(source) = &self.source {
            owners.push(Box::new(Arc::clone(source)));
        }
        if let Err(error) = original_start.retain_before_contact(owners) {
            let retention = original_start
                .retain_before_contact(vec![Box::new(self)])
                .err();
            return Err(error.context(format!(
                "same original custody retained on refusal: {retention:?}"
            )));
        }
        let prepared: Result<ryeos_app::runtime_db::ProcessResourceReservationRecord> = (|| {
            self.retained_runtime_custody(state)?;
            let allocation_limit = self
                .verified_inputs
                .selected()
                .max_concurrent_exclusive_allocations()
                .context("original trusted funding lacks admitted allocation ceiling")?;
            let ProcessWorkspaceCustody::ResourceOwnedScratch { scratch_name, .. } =
                &self.runtime_custody.workspace
            else {
                bail!("original pooled funding lacks its actual owned scratch coordinate");
            };
            ensure!(
                scratch_name == owner_coordinate,
                "original pooled coordinate differs from actual captured scratch owner"
            );
            let original = ryeos_app::runtime_db::ProcessResourceReservationRecord {
                owner_kind: "pooled_session".to_owned(),
                owner_coordinate: owner_coordinate.to_owned(),
                daemon_generation_id: ryeos_app::runtime_db::daemon_generation_id().to_owned(),
                selections: self.verified_inputs.selected().selections().to_vec(),
                allocation_limit,
                runtime_custody: Some(self.runtime_custody.clone()),
                launch_authority:
                    ryeos_app::runtime_db::ProcessResourceLaunchAuthority::TrustedProcessGroup {
                        cleanup_contract_digest: self
                            .cleanup_agreement
                            .cleanup_contract_digest
                            .clone(),
                        host_lifetime: lillux::ProcessHostLifetime::capture_current()
                            .map_err(anyhow::Error::msg)?,
                        phase: ryeos_app::runtime_db::TrustedResourceLaunchPhase::Reserved,
                    },
            };
            Ok(original)
        })(
        );
        let original = match prepared {
            Ok(original) => original,
            Err(error) => {
                let retention = original_start
                    .retain_before_contact(vec![Box::new(self)])
                    .err();
                return Err(error.context(format!(
                    "same original custody retained on validation refusal: {retention:?}"
                )));
            }
        };
        if !original_start.prepare_trusted_original_funding(
            state,
            self.verified_inputs,
            allocation_scope,
            &original,
            self.workspace,
            cancelled,
        )? {
            bail!("original trusted pooled funding was denied; same custody remains retained");
        }
        Ok(())
    }

    /// Borrow retention data from the SAME live capture. This private method
    /// grants no admission, reservation, spawn permit or physical-close proof.
    /// An eventual journal handoff must separately preserve the original guard
    /// before its first possible commit and consume private admitted authority.
    fn retained_runtime_custody(&self, state: &AppState) -> Result<&ProcessResourceRuntimeCustody> {
        self.verified_inputs.require_same_state(state)?;
        ensure!(
            Arc::ptr_eq(&self.state_store, &state.state_store),
            "trusted pooled custody belongs to another StateStore owner"
        );
        let current = state.state_store.pinned_state_authority()?;
        let original_runtime = self.state_authority.runtime_directory();
        require_same_directory(original_runtime, current.runtime_directory())?;
        ensure!(
            capture_owned_scratch_custody(original_runtime, &self.workspace)?
                == self.runtime_custody.workspace,
            "trusted pooled scratch differs from its original custody capture"
        );
        self.runtime_custody.validate()?;
        ensure!(
            self.runtime_custody.physical_state == ProcessCustodyPhysicalState::Retained,
            "trusted pooled capture no longer retains its original scratch"
        );
        Ok(&self.runtime_custody)
    }
}

/// Derive coordinates only from the ORIGINAL guard and protected runtime.
/// The parent and leaf come from its owned removal descriptor, not a pathname.
/// This is intentionally private retention machinery, never launch admission.
fn capture_owned_scratch_custody(
    runtime: &lillux::PinnedDirectory,
    original: &TempDirGuard,
) -> Result<ProcessWorkspaceCustody> {
    runtime.ensure_path_binding()?;
    let root = original.owned_scratch_root()?;
    ensure!(
        original.path().as_deref() == Some(root.path()),
        "trusted pooled scratch guard is disarmed or differs from its owned root"
    );
    let (parent, leaf) = original.owned_scratch_parent()?;
    let scratch_name = leaf
        .to_str()
        .context("trusted pooled scratch has no UTF-8 leaf coordinate")?;
    let cache = required_child(runtime, "cache")?;
    require_same_directory(parent, &required_child(&cache, "executions")?)?;
    require_same_directory(root, &required_child(parent, scratch_name)?)?;
    Ok(ProcessWorkspaceCustody::ResourceOwnedScratch {
        runtime_directory_identity: runtime.identity()?,
        parent_directory_identity: parent.identity()?,
        scratch_name: scratch_name.to_owned(),
        workspace_identity: root.identity()?,
    })
}

fn required_child(parent: &lillux::PinnedDirectory, name: &str) -> Result<lillux::PinnedDirectory> {
    parent
        .open_child_directory(OsStr::new(name))?
        .with_context(|| format!("trusted pooled custody directory {name} is missing"))
}

fn require_same_directory(
    original: &lillux::PinnedDirectory,
    expected: &lillux::PinnedDirectory,
) -> Result<()> {
    original.ensure_path_binding()?;
    expected.ensure_path_binding()?;
    ensure!(
        original.identity()? == expected.identity()?,
        "trusted pooled custody belongs to a different directory incarnation"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Descriptor-only controls: no signed capsule, journal, launch or GPU.
    fn scratch_fixture(base: &Path) -> (lillux::PinnedDirectory, PathBuf, Arc<TempDirGuard>) {
        std::fs::create_dir_all(base).unwrap();
        let runtime = lillux::PinnedDirectory::open_or_create(&base.join("runtime")).unwrap();
        let (path, original) = ryeos_app::temp_dir_guard::create_projectless_workspace(
            &runtime.path().join("cache"),
            "original-scratch",
        )
        .unwrap();
        std::fs::write(path.join("sentinel"), b"original custody bytes").unwrap();
        (runtime, path, original)
    }

    #[test]
    fn trusted_custody_coordinates_use_original_removal_descriptors() {
        let temporary = tempfile::tempdir().unwrap();
        let (runtime, path, original) = scratch_fixture(temporary.path());
        let (parent, leaf) = original.owned_scratch_parent().unwrap();
        assert_eq!(
            capture_owned_scratch_custody(&runtime, &original).unwrap(),
            ProcessWorkspaceCustody::ResourceOwnedScratch {
                runtime_directory_identity: runtime.identity().unwrap(),
                parent_directory_identity: parent.identity().unwrap(),
                scratch_name: leaf.to_str().unwrap().to_owned(),
                workspace_identity: original.owned_scratch_root().unwrap().identity().unwrap(),
            }
        );
        let path_only = TempDirGuard::new(path.clone());
        path_only.disarm();
        assert!(capture_owned_scratch_custody(&runtime, &path_only).is_err());
        original.disarm();
        assert!(capture_owned_scratch_custody(&runtime, &original).is_err());
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"original custody bytes"
        );
    }

    #[test]
    fn trusted_custody_refuses_another_runtime_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let (runtime, path, original) = scratch_fixture(&temporary.path().join("first"));
        let (other_runtime, _, _other_guard) = scratch_fixture(&temporary.path().join("second"));
        assert!(capture_owned_scratch_custody(&runtime, &original).is_ok());
        assert!(capture_owned_scratch_custody(&other_runtime, &original).is_err());
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"original custody bytes"
        );
    }

    #[test]
    fn trusted_custody_refuses_runtime_parent_and_root_replacement() {
        for replaced in ["runtime", "parent", "root"] {
            let temporary = tempfile::tempdir().unwrap();
            let (runtime, path, original) = scratch_fixture(temporary.path());
            capture_owned_scratch_custody(&runtime, &original).unwrap();
            // Keep test cleanup outside the guard after deliberately breaking
            // its path binding. There is no journal or removal receipt here.
            original.preserve_for_explicit_cleanup();
            let victim = match replaced {
                "runtime" => runtime.path().to_path_buf(),
                "parent" => original
                    .owned_scratch_parent()
                    .unwrap()
                    .0
                    .path()
                    .to_path_buf(),
                "root" => path.clone(),
                _ => unreachable!(),
            };
            let retained = victim.with_extension("original");
            let suffix = path.strip_prefix(&victim).unwrap().to_path_buf();
            std::fs::rename(&victim, &retained).unwrap();
            std::fs::create_dir(&victim).unwrap();
            assert!(
                capture_owned_scratch_custody(&runtime, &original).is_err(),
                "{replaced}"
            );
            assert_eq!(
                std::fs::read(retained.join(suffix).join("sentinel")).unwrap(),
                b"original custody bytes"
            );
        }
    }
}
