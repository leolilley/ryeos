//! Local private material for one accepted qualification-only Worker.
//!
//! The caller already owns the Q private runtime workspace and
//! its disposable state root and the separately admitted credential home.
//! This module redeems accepted CAS content and publishes the accepted
//! configuration into that credential home under the retained profile fence.

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

/// The plan, privately copied inputs, and exact source/cache/workspace lifelines stay
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

/// Admit an enforced local Worker launch against the guarantees the node
/// reports. The Worker's signed protocol must declare the containment it
/// needs and a captured-execution scratch workspace, and the node must have
/// qualified the retained PID-namespace init that this launch uses. Nothing
/// here falls back to the trusted process-group lane under enforcement.
fn require_contained_worker_launch(
    state: &ryeos_app::state::AppState,
    protocol: &PersistentSessionProtocol,
) -> Result<()> {
    contained_worker_launch_admission(
        protocol,
        &state
            .isolation
            .inspection()
            .process_scope_readiness
            .descendant_containment,
    )
}

fn contained_worker_launch_admission(
    protocol: &PersistentSessionProtocol,
    containment: &ryeos_engine::isolation::DescendantContainmentReadiness,
) -> Result<()> {
    use ryeos_engine::protocols::descriptor::{
        PersistentSessionCleanupAuthority, PersistentSessionWorkspaceAuthority,
    };
    ensure!(
        protocol.cleanup_authority == PersistentSessionCleanupAuthority::DescendantContainment,
        "enforced qualification Worker requires a signed protocol declaring `descendant_containment` cleanup, not {:?}",
        protocol.cleanup_authority
    );
    ensure!(
        protocol.workspace_authority == PersistentSessionWorkspaceAuthority::EphemeralScratch,
        "enforced qualification Worker requires a signed captured-execution scratch workspace, not {:?}",
        protocol.workspace_authority
    );
    // The protocol declares the need (descendant containment); the node's
    // reported guarantee answers it by whichever mechanism it qualified.
    ensure!(
        containment.enforced_guarantee().is_some(),
        "enforced qualification Worker needs descendant containment, but this node reports no qualified mechanism (cgroup scope unavailable; PID-namespace init: {})",
        containment.pid_namespace.detail
    );
    Ok(())
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
    credential_home: &lillux::PinnedDirectory,
    budget: &super::external_content::PrivateMaterializationBudget,
) -> Result<PreparedExternalQualificationLocalMaterials> {
    // Disabled isolation runs the trusted controller lane over private
    // directory copies. Enforced isolation instead runs the Worker in a
    // sandbox over read-only mounts, retained by its PID-namespace init.
    let enforced = state.isolation.is_enforced();
    ensure!(
        workspace_lifeline.owns_effective_path(workspace),
        "qualification Worker workspace differs from its original owner"
    );
    workspace_lifeline
        .owned_scratch_root()?
        .ensure_path_binding()?;
    workspace_lifeline.require_uncreated_private_workspace()?;
    state_root.require_owner_private_directory()?;
    state_root.ensure_path_binding()?;
    credential_home.require_owner_private_directory()?;
    credential_home.ensure_path_binding()?;
    credential_home.require_disjoint_directory_tree(state_root)?;
    credential_home.require_disjoint_directory_tree(workspace_lifeline.owned_scratch_root()?)?;
    let credential_home_lifeline = credential_home.inherited_descriptor_authority()?;
    let state_root_lifeline = state_root.inherited_descriptor_authority()?;
    let state_root_path = state_root.path().to_path_buf();
    let workspace_path = workspace.to_path_buf();
    let runtime_view = (!enforced)
        .then(|| super::persistent_session::create_node_owned_runtime_view(workspace))
        .transpose()?;

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
            if enforced {
                require_contained_worker_launch(state, &session.protocol)?;
            }
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
            let (bound, source) = match runtime_view.as_ref() {
                Some(runtime_view) => (
                    super::external_content::bind_retained_qualification_local_realizations(
                        state,
                        &realizations,
                        runtime_view.path(),
                        budget,
                    )?,
                    super::source_closure::bind_retained_qualification_source(
                        state,
                        &content.worker_source,
                        runtime_view.path(),
                        budget,
                    )?,
                ),
                None => (
                    super::external_content::bind_retained_qualification_isolated_realizations(
                        state,
                        &realizations,
                        &workspace_path,
                    )?,
                    super::source_closure::bind_retained_qualification_source_isolated(
                        state,
                        &content.worker_source,
                        &workspace_path,
                    )?,
                ),
            };
            let bound = bound.context("qualification local Worker has no retained realizations")?;
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
                    credential_home.path(),
                    enforced,
                )?,
            );
            state_root.ensure_path_binding()?;
            credential_home.ensure_path_binding()?;
            plan.bind_persistent_session_workspace(&workspace_path)?;
            let executable_search = (!content.executable_search.is_empty())
                .then(|| {
                    lillux::canonical_json(&serde_json::to_value(&content.executable_search)?)
                        .map_err(anyhow::Error::from)
                })
                .transpose()?;
            let (realization_root, delivery) = match runtime_view.as_ref() {
                Some(runtime_view) => (
                    runtime_view.path(),
                    ExternalRealizationDelivery::PrivateDescriptorRoot,
                ),
                // Mounted at their signed coordinates in the sandbox.
                None => (
                    workspace_path.as_path(),
                    ExternalRealizationDelivery::FixedNamespace,
                ),
            };
            plan.bind_persistent_session_spawn_environment(
                Some(&realizations_env),
                Some(realization_root),
                Some(delivery),
                Some(source.sealed_identity_env()),
                Some(source.execution_entry_path()),
                executable_search.as_deref(),
            )?;
            let mut lifelines: Vec<Box<dyn Send + Sync>> = leases
                .into_iter()
                .map(|lease| Box::new(lease) as Box<dyn Send + Sync>)
                .collect();
            lifelines.push(Box::new(source));
            if let Some(runtime_view) = runtime_view {
                lifelines.push(Box::new(runtime_view));
            }
            lifelines.push(Box::new(state_root_lifeline));
            lifelines.push(Box::new(credential_home_lifeline));
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

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::isolation::{
        DescendantContainmentReadiness, PidNamespaceContainmentQualification,
    };
    use ryeos_engine::protocols::descriptor::{
        PersistentSessionCleanupAuthority as Cleanup,
        PersistentSessionWorkspaceAuthority as Workspace,
    };

    fn protocol(cleanup: Cleanup, workspace: Workspace) -> PersistentSessionProtocol {
        let signed = include_str!(
            "../../../../../bundles/core/.ai/protocols/ryeos/core/contained_structured_session.yaml"
        );
        let descriptor: ryeos_engine::protocols::descriptor::ProtocolDescriptor =
            serde_yaml::from_str(signed).unwrap();
        let mut session =
            ryeos_engine::protocols::validate_persistent_session_protocol(&descriptor)
                .unwrap()
                .clone();
        session.cleanup_authority = cleanup;
        session.workspace_authority = workspace;
        session
    }

    fn readiness(qualified: bool) -> DescendantContainmentReadiness {
        DescendantContainmentReadiness {
            cgroup_scope: false,
            pid_namespace: if qualified {
                PidNamespaceContainmentQualification::qualified_by(
                    lillux::process_control::ProofSource::Measured,
                    "probed",
                )
            } else {
                PidNamespaceContainmentQualification::not_probed("user namespaces refused")
            },
            trusted_process_group: true,
        }
    }

    #[test]
    fn signed_contained_protocol_declares_containment_over_captured_scratch() {
        let session = protocol(Cleanup::DescendantContainment, Workspace::EphemeralScratch);
        // The shipped signed descriptor itself, not a hand-built one.
        let shipped: ryeos_engine::protocols::descriptor::ProtocolDescriptor = serde_yaml::from_str(
            include_str!(
                "../../../../../bundles/core/.ai/protocols/ryeos/core/contained_structured_session.yaml"
            ),
        )
        .unwrap();
        let shipped =
            ryeos_engine::protocols::validate_persistent_session_protocol(&shipped).unwrap();
        assert_eq!(shipped.cleanup_authority, session.cleanup_authority);
        assert_eq!(shipped.workspace_authority, session.workspace_authority);
        contained_worker_launch_admission(&session, &readiness(true)).unwrap();
    }

    #[test]
    fn enforced_worker_is_refused_without_its_declared_need_or_qualified_containment() {
        let trusted = protocol(Cleanup::TrustedProcessGroup, Workspace::EphemeralScratch);
        let error = contained_worker_launch_admission(&trusted, &readiness(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("descendant_containment"), "{error}");
        let runtime = protocol(Cleanup::DescendantContainment, Workspace::RuntimeWorkspace);
        let error = contained_worker_launch_admission(&runtime, &readiness(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("captured-execution scratch"), "{error}");
        // The trusted-lane acceptance on the node is never a substitute.
        let contained = protocol(Cleanup::DescendantContainment, Workspace::EphemeralScratch);
        let error = contained_worker_launch_admission(&contained, &readiness(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("user namespaces refused"), "{error}");
    }

    #[test]
    fn either_qualified_mechanism_answers_the_same_declared_need() {
        let contained = protocol(Cleanup::DescendantContainment, Workspace::EphemeralScratch);
        let readiness = |cgroup_scope, namespace| DescendantContainmentReadiness {
            cgroup_scope,
            pid_namespace: if namespace {
                PidNamespaceContainmentQualification::qualified_by(
                    lillux::process_control::ProofSource::Measured,
                    "probed",
                )
            } else {
                PidNamespaceContainmentQualification::not_probed("user namespaces refused")
            },
            trusted_process_group: false,
        };
        // Admission and launch agree: whatever the connector gate admits.
        contained_worker_launch_admission(&contained, &readiness(true, false)).unwrap();
        contained_worker_launch_admission(&contained, &readiness(false, true)).unwrap();
        contained_worker_launch_admission(&contained, &readiness(true, true)).unwrap();
        let error = contained_worker_launch_admission(&contained, &readiness(false, false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no qualified mechanism"), "{error}");
    }

    #[test]
    fn both_launch_checks_agree_on_each_lane() {
        use ryeos_app::thread_lifecycle::qualification_worker_protocol_lane_admits as plan_lane;
        let contained = protocol(Cleanup::DescendantContainment, Workspace::EphemeralScratch);
        let trusted = protocol(Cleanup::TrustedProcessGroup, Workspace::RuntimeWorkspace);
        let qualified = readiness(true);
        // Enforced: the contained Worker passes the plan check AND the launch
        // check; the trusted triple passes neither.
        assert!(plan_lane(&contained, true));
        contained_worker_launch_admission(&contained, &qualified).unwrap();
        assert!(!plan_lane(&trusted, true));
        assert!(contained_worker_launch_admission(&trusted, &qualified).is_err());
        // Disabled: the existing trusted triple, never the contained one.
        assert!(plan_lane(&trusted, false));
        assert!(!plan_lane(&contained, false));
    }
}
