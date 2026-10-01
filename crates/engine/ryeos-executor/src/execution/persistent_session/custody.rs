//! Pre-contact retention of an exact trusted pooled-session closure.
//!
//! This value is deliberately not a launch permit or a durable cleanup record.
//! Pooled scratch currently has no journal-backed workspace owner. Until that
//! join, financial prebinding, driver evidence and uncertain-start ownership
//! exist, the parent's executable prerequisite guard continues to refuse launch.

use std::ffi::OsStr;

use super::*;
use crate::execution::external_content::BoundExternalRealizations;
use crate::execution::source_closure::BoundSourceClosure;
use ryeos_app::runtime_db::process_resource_custody::{
    MAX_PROCESS_CUSTODY_MATERIALIZATIONS, ProcessMaterializationCache,
    ProcessMaterializationCustody,
};
use ryeos_app::temp_dir_guard::TempDirGuard;

/// Owns the ORIGINAL workspace guard and complete bindings before any contact.
/// No Clone, serde decoder, raw-record constructor, or process release method.
/// It remains unwired while the parent's executable prerequisites are closed.
pub(super) struct PreparedTrustedSessionCustody {
    session_capsule_hash: String,
    execution_realization_hash: String,
    source_binding_hash: Option<String>,
    materializations: BTreeSet<ProcessMaterializationCustody>,
    /// Scratch retention coordinate, explicitly not a durable workspace_id.
    scratch_name: String,
    workspace_identity: lillux::secure_fs::PinnedDirectoryIdentity,
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
        let capsule = load_capsule(state, capsule_hash)?;
        validate_capsule_current_trust(&state.engine, &capsule)?;
        let session = retained_session_protocol(&state.engine, &capsule)?;
        ensure!(
            session.process_mode == PersistentSessionProcessMode::PooledRequests
                && session.cleanup_authority
                    == PersistentSessionCleanupAuthority::TrustedProcessGroup
                && capsule.external_candidate.is_none(),
            "trusted pooled custody requires a local trusted pooled capsule"
        );
        let node = state
            .node_policy
            .require::<ryeos_app::node_policy::sections::execution::NodeExecutionAdmissionPolicy>(
        )?;
        let cleanup_agreement =
            admit_session_resource_cleanup_contract(node.resource_authority.as_ref(), &session)?
                .context("trusted pooled custody requires exact node/product cleanup agreement")?;
        // Agreement is retained, not used as permission to bypass prerequisites.
        let exact = retained_exact_program(&capsule)?;
        validate_exact_evidence_attachments(&exact)?;
        ensure!(
            exact
                .resolution_output
                .effective_definition_digest()?
                .as_str()
                == exact.effective_definition_digest,
            "trusted pooled exact program digest does not reproduce"
        );
        let resolution = exact.resolution_output.restore();
        let (protocol_ref, protocol_digest) = capsule_protocol_identity(&capsule)?;
        crate::execution::execution_realization::verify_persistent_session(
            state,
            &capsule,
            &resolution,
            &exact.effective_definition_digest,
            protocol_ref,
            protocol_digest,
        )?;

        ensure!(
            workspace.owns_effective_path(workspace_path),
            "trusted pooled workspace differs from its original guard"
        );
        let original_workspace = workspace.owned_scratch_root()?;
        original_workspace.ensure_path_binding()?;
        let workspace_identity = original_workspace.identity()?;
        let scratch_name = workspace_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("trusted pooled scratch has no UTF-8 leaf coordinate")?
            .to_owned();
        let state_authority = state.state_store.pinned_state_authority()?;
        let runtime = state_authority.runtime_directory();
        runtime.ensure_path_binding()?;
        let cache = required_child(runtime, "cache")?;
        let executions = required_child(&cache, "executions")?;
        require_same_directory(
            original_workspace,
            &required_child(&executions, &scratch_name)?,
        )?;

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
        runtime.ensure_path_binding()?;
        original_workspace.ensure_path_binding()?;
        Ok(Self {
            session_capsule_hash: capsule_hash.to_owned(),
            execution_realization_hash: capsule.execution_realization_hash,
            source_binding_hash: capsule.source_binding_hash,
            materializations,
            scratch_name,
            workspace_identity,
            workspace,
            external,
            source,
            cleanup_agreement,
        })
    }
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
