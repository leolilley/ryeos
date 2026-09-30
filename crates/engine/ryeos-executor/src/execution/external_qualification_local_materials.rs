//! Local immutable material for one accepted qualification-only Worker.
//!
//! The caller already owns the Q runtime workspace, its installed view, and
//! the protected provider state root. This module only redeems accepted CAS
//! content and publishes the accepted configuration into that state root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use ryeos_app::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_engine::protocols::descriptor::PersistentSessionProtocol;
use ryeos_state::objects::{
    ExternalContentRealizationSet, ExternalRealizationDelivery, PersistentSessionLifecycleContract,
    PersistentSessionWireContract,
};

/// Exact signed Worker lifecycle and protocol selected with its checked plan.
/// The protocol carries workspace/network authority, channel environment,
/// readiness identity slot, and runtime environment allowlist. No later
/// caller needs to resolve another Bundle generation to start or speak to Q.
pub(crate) struct PreparedExternalQualificationLocalSession {
    pub(crate) lifecycle: PersistentSessionLifecycleContract,
    pub(crate) wire: PersistentSessionWireContract,
    pub(crate) protocol: PersistentSessionProtocol,
}

/// The plan, immutable mounts, and exact source/cache/workspace lifelines stay
/// in one owner until a future Q process owner transfers them to its held
/// subprocess. This carries no connector, scope, or provider-contact permit.
pub(crate) struct PreparedExternalQualificationLocalMaterials {
    plan: ryeos_app::thread_lifecycle::PreparedItemPlan,
    session: PreparedExternalQualificationLocalSession,
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    lifelines: Vec<Box<dyn Send + Sync>>,
    workspace: PathBuf,
    state_root: PathBuf,
}

impl PreparedExternalQualificationLocalMaterials {
    pub(crate) fn into_spawn_parts(
        self,
    ) -> (
        ryeos_app::thread_lifecycle::PreparedItemPlan,
        PreparedExternalQualificationLocalSession,
        Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
        Vec<Box<dyn Send + Sync>>,
        PathBuf,
        PathBuf,
    ) {
        (
            self.plan,
            self.session,
            self.mounts,
            self.lifelines,
            self.workspace,
            self.state_root,
        )
    }
}

/// Prepare only retained Worker source, product realizations, and signed
/// structured-session configuration. Plan compilation and every CAS binding
/// run under one checked signed Bundle generation. The original workspace
/// guard and pinned state directory must be supplied by the future Q owner;
/// this function neither creates nor reopens a workspace from a path.
pub(crate) fn prepare_external_qualification_local_materials(
    state: &ryeos_app::state::AppState,
    root: &PreparedExternalQualificationRoot,
    workspace: &Path,
    workspace_lifeline: Arc<TempDirGuard>,
    state_root: &lillux::PinnedDirectory,
) -> Result<PreparedExternalQualificationLocalMaterials> {
    ensure!(
        state.isolation.is_enforced(),
        "qualification local Worker requires enforced read-only source and realization mounts"
    );
    ensure!(
        workspace_lifeline.owns_effective_path(workspace),
        "qualification Worker workspace differs from its original owner"
    );
    workspace_lifeline
        .owned_scratch_root()?
        .ensure_path_binding()?;
    ensure!(
        workspace_lifeline.workspace_view_identity()?.is_some(),
        "qualification Worker workspace has no installed original view"
    );
    state_root.require_owner_private_directory()?;
    state_root.ensure_path_binding()?;
    let state_root_lifeline = state_root.inherited_descriptor_authority()?;
    let state_root_path = state_root.path().to_path_buf();
    let workspace_path = workspace.to_path_buf();

    ryeos_app::thread_lifecycle::with_prepared_external_qualification_local_worker_plan(
        state,
        root,
        |mut plan, declaration, signed_protocol, composed| {
            let mut declaration = declaration.clone();
            super::persistent_session::apply_resource_overrides(&mut declaration, composed)?;
            let session = PreparedExternalQualificationLocalSession {
                lifecycle: super::persistent_session::lifecycle_contract(&declaration)?,
                wire: super::persistent_session::wire_contract(signed_protocol)?,
                protocol: ryeos_engine::protocols::validate_persistent_session_protocol(
                    &signed_protocol.descriptor,
                )
                .map_err(anyhow::Error::msg)?
                .clone(),
            };
            let content = root
                .purpose()
                .consumer_content
                .as_ref()
                .context("qualification local Worker has no accepted content")?;
            let realizations = ExternalContentRealizationSet::new(
                content
                    .worker_literals
                    .iter()
                    .chain(content.environment_realizations.iter())
                    .chain(std::iter::once(&content.runtime_realization))
                    .cloned()
                    .collect(),
            )?;
            super::source_closure::validate_retained_qualification_mount_separation(
                state,
                &content.worker_source,
                &realizations,
            )?;
            let bound = super::external_content::bind_retained_qualification_local_realizations(
                state,
                &realizations,
                &workspace_path,
            )?
            .context("qualification local Worker has no retained realizations")?;
            let source = super::source_closure::bind_retained_qualification_source(
                state,
                &content.worker_source,
                &workspace_path,
            )?;
            let (mut mounts, realizations_env, leases) = bound.into_spawn_parts();
            ensure!(
                !leases.is_empty(),
                "qualification local Worker has no retained realization leases"
            );
            mounts.extend_from_slice(source.mounts());
            mounts.extend(
                super::persistent_session::prepare_structured_session_configurations(
                    root.worker_profile(),
                    source.source_directory(),
                    &state_root_path,
                    true,
                )?,
            );
            state_root.ensure_path_binding()?;
            plan.bind_persistent_session_workspace(&workspace_path)?;
            let executable_search = (!content.executable_search.is_empty())
                .then(|| {
                    lillux::canonical_json(&serde_json::to_value(&content.executable_search)?)
                        .map_err(anyhow::Error::from)
                })
                .transpose()?;
            plan.bind_persistent_session_spawn_environment(
                Some(&realizations_env),
                Some(&workspace_path),
                Some(ExternalRealizationDelivery::FixedNamespace),
                Some(source.sealed_identity_env()),
                Some(source.execution_entry_path()),
                executable_search.as_deref(),
            )?;
            let mut lifelines: Vec<Box<dyn Send + Sync>> = leases
                .into_iter()
                .map(|lease| Box::new(lease) as Box<dyn Send + Sync>)
                .collect();
            lifelines.push(Box::new(source));
            lifelines.push(Box::new(state_root_lifeline));
            lifelines.push(Box::new(workspace_lifeline));
            Ok(PreparedExternalQualificationLocalMaterials {
                plan,
                session,
                mounts,
                lifelines,
                workspace: workspace_path,
                state_root: state_root_path,
            })
        },
    )
}
