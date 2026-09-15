//! Execution provenance — single source of truth, type-state encoded.
//!
//! A provenance value is constructed exactly once at the entry point
//! (HTTP route, SSE launch, scheduler tick, callback handler, resume
//! reconciler). It flows through dispatch, runner, native launch, and
//! callback token minting unchanged. Callback children are derived only
//! by cloning this value as a borrowed child.
//!
//! The six variants enumerate the six legal shapes. Invalid shapes
//! (for example, "Root PushedHead without lifeline" or "borrowed child
//! with snapshot hash") do not compile.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_engine::engine::Engine;

use crate::temp_dir_guard::TempDirGuard;

/// Project source dimension. Used only as an accessor return type for
/// capability checks and tracing. The enum is not stored as a field on
/// `ExecutionProvenance`; it is derived from the variant tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectSourceKind {
    Projectless,
    LiveFs,
    PushedHead,
}

#[derive(Clone)]
#[doc(hidden)]
pub struct ProvenanceSeal(());

#[derive(Clone)]
#[doc(hidden)]
pub enum PinnedMaterializationAuthority {
    Verified(ryeos_state::PinnedProjectMaterialization),
    #[cfg(test)]
    Fixture,
}

impl PinnedMaterializationAuthority {
    fn verified(&self) -> Option<&ryeos_state::PinnedProjectMaterialization> {
        match self {
            Self::Verified(materialization) => Some(materialization),
            #[cfg(test)]
            Self::Fixture => None,
        }
    }
}

/// Single source of truth for what engine, workspace, lineage, and role
/// belong to an execution.
///
/// Construct via `root_live_fs` / `root_pushed_head`, derive an ordinary
/// callback child via `clone_for_borrowed_child`, and derive the transient
/// immutable-input shape only through `with_immutable_workspace_input`. The
/// private seal field on each variant prevents construction outside this
/// module while still permitting explicit variant matching by consumers.
#[derive(Clone)]
pub enum ExecutionProvenance {
    /// Execution with no project authority. The effective directory is a
    /// daemon-owned scratch root used only to satisfy subprocess cwd needs; it
    /// must never be interpreted as a live project.
    Projectless {
        request_engine: Arc<Engine>,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        is_child: bool,
        __seal: ProvenanceSeal,
    },

    /// Top-level run with live-filesystem authority.
    RootLiveProject {
        request_engine: Arc<Engine>,
        /// Directory used for resolution and execution.
        project_path: PathBuf,
        /// Canonical live project that supplied the execution.
        original_project_path: PathBuf,
        /// Present only when the live-fs path is a daemon-created ephemeral
        /// workspace (for example `--no-project`). The runner transfers this
        /// lifeline into detached execution ownership.
        workspace_lifeline: Option<Arc<TempDirGuard>>,
        /// Deliberate runtime state-root override (`/execute` `state_root`):
        /// item resolution stays anchored at `project_path` while the
        /// daemon-side runtime-state authority points here. Callback tokens
        /// retain it, but subprocesses never receive the host path as callback
        /// authority. `None` = state lives under the project as usual.
        state_root: Option<PathBuf>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        __seal: ProvenanceSeal,
    },

    /// Top-level run against a pushed CAS snapshot. Owns the snapshot
    /// lineage (pin + foldback) and pins the materialized checkout dir
    /// via `workspace_lifeline`.
    RootPinnedGeneration {
        request_engine: Arc<Engine>,
        /// Operator-side absolute path (HEAD-ref key).
        original_project_path: PathBuf,
        /// Daemon-side temp checkout the execution runs against. Must
        /// equal `workspace_lifeline.path()`.
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
        pinned_materialization: PinnedMaterializationAuthority,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        __seal: ProvenanceSeal,
    },

    /// Callback child of a `RootLiveProject` parent. It inherits the parent's exact
    /// execution workspace and distinct live overlay source, if materialized.
    /// It owns no snapshot lineage.
    ChildLiveProject {
        request_engine: Arc<Engine>,
        project_path: PathBuf,
        original_project_path: PathBuf,
        workspace_lifeline: Option<Arc<TempDirGuard>>,
        /// Inherited runtime state-root override; children of a run whose
        /// state was redirected keep writing state to the same place.
        state_root: Option<PathBuf>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        __seal: ProvenanceSeal,
    },

    /// Callback child of a `RootPinnedGeneration` parent (or another borrowed
    /// pushed child). Inherits engine, paths, and lifeline. It never
    /// carries the snapshot hash: the root owns lineage.
    ChildPinnedGeneration {
        request_engine: Arc<Engine>,
        original_project_path: PathBuf,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        base_snapshot_hash: String,
        pinned_materialization: PinnedMaterializationAuthority,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        __seal: ProvenanceSeal,
    },

    /// Borrowed child whose item/source authority remains the parent's sealed
    /// pinned generation while its process observes a separately captured,
    /// immutable generation of the current shared COW workspace.
    ChildImmutableWorkspaceInput {
        request_engine: Arc<Engine>,
        original_project_path: PathBuf,
        subject_effective_path: PathBuf,
        subject_workspace_lifeline: Arc<TempDirGuard>,
        base_snapshot_hash: String,
        subject_pinned_materialization: PinnedMaterializationAuthority,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        input_snapshot_hash: String,
        input_output_capture_hash: Option<String>,
        input_pinned_materialization: PinnedMaterializationAuthority,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
        candidate_evaluation:
            Option<Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>>,
        __seal: ProvenanceSeal,
    },
}

/// Compile durable project authority into the engine's filesystem authority
/// without requiring callers to manufacture an `ExecutionProvenance` solely
/// for an immediate local launch.
pub fn isolation_project_authority_for_project(
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> ryeos_engine::isolation::IsolationProjectAuthority {
    match project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
            ryeos_engine::isolation::IsolationProjectAuthority::EphemeralScratch
        }
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            live_access:
                ryeos_state::objects::LiveAccessAuthority {
                    access: ryeos_state::objects::LiveProjectAccess::ReadOnly,
                    ..
                },
            ..
        }
        | ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ..
        } => ryeos_engine::isolation::IsolationProjectAuthority::ReadOnly,
        ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            live_access:
                ryeos_state::objects::LiveAccessAuthority {
                    access: ryeos_state::objects::LiveProjectAccess::ReadWrite,
                    ..
                },
            ..
        } => ryeos_engine::isolation::IsolationProjectAuthority::External,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        } => ryeos_engine::isolation::IsolationProjectAuthority::RuntimeWorkspace,
    }
}

/// Translate a resolved live-project confinement into the exact launch
/// authority consumed by the isolation runtime. Descriptor-rooted authority
/// is bound to the current canonical directory identity before launch.
pub fn isolation_live_access_authority_for_project(
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> anyhow::Result<Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>> {
    project_authority
        .live_access()
        .map(|authority| match &authority.confinement {
            ryeos_state::objects::LiveFilesystemConfinement::DescriptorRootedFixedParents {
                denied_control_paths,
                symlink_policy: ryeos_state::objects::LiveSymlinkPolicy::AdmittedExecutionNamespace,
            } => {
                let root = project_authority
                    .open_environment_root()?
                    .ok_or_else(|| anyhow::anyhow!("live project authority lost its root"))?;
                let ai = root
                    .open_child_directory(std::ffi::OsStr::new(ryeos_engine::AI_DIR))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "live project authority root has no real descriptor-relative .ai directory"
                        )
                    })?;
                drop(ai);
                let (root_device_id, root_inode) = root.device_inode()?;
                Ok(
                    ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedFixedParents {
                        root: Arc::new(root),
                        root_device_id,
                        root_inode,
                        denied_control_paths: denied_control_paths
                            .iter()
                            .map(PathBuf::from)
                            .collect(),
                        authorized_write_namespaces: authority.authorized_write_namespaces.clone(),
                    },
                )
            }
            ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost => Ok(
                ryeos_engine::isolation::IsolationLiveAccessAuthority::UnconfinedHost {
                    authorized_write_namespaces: authority.authorized_write_namespaces.clone(),
                },
            ),
        })
        .transpose()
}

impl ExecutionProvenance {
    /// Exact subject-resolution class for this execution transition.
    ///
    /// The operational generation comes from the same sealed project
    /// authority as the workspace. Callers must carry this value into the
    /// sealed admitted project binding; deriving it later from a
    /// `PlanContext::LocalPath` would erase the distinction between live,
    /// immutable, and writable-COW resolution. `PlanContext` remains the broad
    /// planning surface used by non-admission inspection and is not itself an
    /// execution-authority container.
    pub fn subject_resolution_authority(
        &self,
    ) -> ryeos_engine::contracts::SubjectResolutionAuthority {
        match self {
            Self::Projectless { .. } => {
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
            }
            Self::RootLiveProject { .. } | Self::ChildLiveProject { .. } => {
                ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs
            }
            Self::RootPinnedGeneration {
                project_authority, ..
            }
            | Self::ChildPinnedGeneration {
                project_authority, ..
            }
            | Self::ChildImmutableWorkspaceInput {
                project_authority, ..
            } => subject_resolution_authority_for_pinned(project_authority),
        }
    }

    /// Construct Root provenance for a live filesystem execution.
    pub fn root_live_fs(
        project_path: PathBuf,
        request_engine: Arc<Engine>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        project_authority.validate()?;
        if !matches!(
            project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        ) {
            anyhow::bail!("live provenance requires an explicitly resolved live project authority");
        }
        if project_authority.project_root_projection() != Some(project_path.as_path()) {
            anyhow::bail!(
                "live provenance project path does not match the resolved project authority"
            );
        }
        let provenance = Self::RootLiveProject {
            request_engine,
            original_project_path: project_path.clone(),
            project_path,
            workspace_lifeline: None,
            state_root: None,
            project_authority,
            candidate_evaluation: None,
            __seal: ProvenanceSeal(()),
        };
        Ok(provenance)
    }

    pub fn root_projectless(
        effective_path: PathBuf,
        request_engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        if !matches!(
            project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        ) {
            anyhow::bail!("projectless provenance requires projectless authority");
        }
        project_authority.validate()?;
        match workspace_lifeline.path() {
            Some(_) if workspace_lifeline.owns_effective_path(&effective_path) => {}
            Some(root) => anyhow::bail!(
                "projectless workspace lifeline {} does not own {}",
                root.display(),
                effective_path.display()
            ),
            None => anyhow::bail!("projectless workspace lifeline is disarmed"),
        }
        Ok(Self::Projectless {
            request_engine,
            effective_path,
            workspace_lifeline,
            project_authority,
            candidate_evaluation: None,
            is_child: false,
            __seal: ProvenanceSeal(()),
        })
    }

    /// Attach ownership of a daemon-created live-fs workspace.
    ///
    /// # Panics
    ///
    /// Panics if the guard is disarmed, names a different path, or this is a
    /// pushed-head provenance (which already has a mandatory lifeline).
    pub fn with_workspace_lifeline(
        mut self,
        workspace_lifeline: Option<Arc<TempDirGuard>>,
    ) -> Self {
        if let Some(lifeline) = &workspace_lifeline {
            match lifeline.path() {
                Some(_) if lifeline.owns_effective_path(self.effective_path()) => {}
                Some(path) => panic!(
                    "ExecutionProvenance::with_workspace_lifeline: lifeline path {} \
                     does not match effective_path {}",
                    path.display(),
                    self.effective_path().display(),
                ),
                None => {
                    panic!("ExecutionProvenance::with_workspace_lifeline: lifeline is disarmed")
                }
            }
        }
        match &mut self {
            Self::Projectless { .. } => {
                if workspace_lifeline.is_some() {
                    panic!(
                        "ExecutionProvenance::with_workspace_lifeline: projectless provenance already owns its workspace"
                    );
                }
            }
            Self::RootLiveProject {
                workspace_lifeline: slot,
                ..
            }
            | Self::ChildLiveProject {
                workspace_lifeline: slot,
                ..
            } => *slot = workspace_lifeline,
            Self::RootPinnedGeneration { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => {
                panic!(
                    "ExecutionProvenance::with_workspace_lifeline: pushed-head provenance already owns its workspace"
                );
            }
        }
        self
    }

    /// Attach a runtime state-root override to a live-fs provenance.
    ///
    /// Only meaningful on the live-fs variants — a pushed-head execution
    /// already runs against an ephemeral checkout, so the caller must have
    /// rejected the combination before constructing provenance.
    ///
    /// # Panics
    ///
    /// Panics on a pushed-head variant: reaching here means the entry-point
    /// validation was bypassed, which is a programmer error.
    pub fn with_state_root(mut self, override_root: Option<PathBuf>) -> Self {
        match &mut self {
            Self::Projectless { .. } => {
                if override_root.is_some() {
                    panic!(
                        "ExecutionProvenance::with_state_root: projectless execution cannot redirect project state"
                    );
                }
            }
            Self::RootLiveProject { state_root, .. }
            | Self::ChildLiveProject { state_root, .. } => {
                *state_root = override_root;
            }
            Self::RootPinnedGeneration { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => {
                if override_root.is_some() {
                    panic!(
                        "ExecutionProvenance::with_state_root: state_root is a \
                         live-fs control; pushed-head executions already run in \
                         an ephemeral checkout"
                    );
                }
            }
        }
        self
    }

    /// The deliberate runtime state-root override, when one was requested.
    /// `None` = runtime state lives under the (effective) project path.
    pub fn state_root_override(&self) -> Option<&Path> {
        match self {
            Self::Projectless { .. } => None,
            Self::RootLiveProject { state_root, .. }
            | Self::ChildLiveProject { state_root, .. } => state_root.as_deref(),
            Self::RootPinnedGeneration { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => None,
        }
    }

    /// Construct Root provenance for a pushed-head checkout.
    ///
    /// # Panics
    ///
    /// Panics if `workspace_lifeline.path()` is `None` (disarmed) or
    /// does not equal `effective_path`. This is a programmer error and
    /// is surfaced at the construction site.
    pub fn root_pushed_head(
        original_project_path: PathBuf,
        request_engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        pinned_materialization: ryeos_state::PinnedProjectMaterialization,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        pinned_materialization.ensure_root_binding()?;
        let effective_path = pinned_materialization.path().to_path_buf();
        let snapshot_hash = pinned_materialization.snapshot_hash().to_owned();
        match workspace_lifeline.path() {
            Some(_) if workspace_lifeline.owns_effective_path(&effective_path) => {}
            Some(p) => panic!(
                "ExecutionProvenance::root_pushed_head: lifeline path {} \
                 does not match effective_path {} — caller mis-paired \
                 the temp dir guard",
                p.display(),
                effective_path.display(),
            ),
            None => panic!(
                "ExecutionProvenance::root_pushed_head: lifeline is \
                 disarmed — caller passed a TempDirGuard whose dir was \
                 already taken"
            ),
        }

        Self::RootPinnedGeneration {
            request_engine,
            original_project_path,
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            pinned_materialization: PinnedMaterializationAuthority::Verified(
                pinned_materialization,
            ),
            project_authority,
            candidate_evaluation: None,
            __seal: ProvenanceSeal(()),
        }
        .validate_project_authority_binding()
    }

    /// Condition a fresh, non-borrowed pinned root with its exact prepared
    /// workspace-output partition. This changes no generation,
    /// materialization, engine, path, or caller authority.
    pub fn condition_root_workspace_outputs(
        mut self,
        partition: ryeos_state::objects::WorkspaceOutputPartition,
    ) -> anyhow::Result<Self> {
        let authority = match &self {
            Self::RootPinnedGeneration {
                candidate_evaluation: None,
                project_authority,
                ..
            } => project_authority.condition_initial_workspace_outputs(partition)?,
            Self::RootPinnedGeneration {
                candidate_evaluation: Some(_),
                ..
            } => {
                anyhow::bail!(
                    "candidate-evaluation provenance cannot acquire workspace output authority"
                )
            }
            Self::Projectless { .. }
            | Self::RootLiveProject { .. }
            | Self::ChildLiveProject { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => {
                anyhow::bail!(
                    "workspace output authority requires fresh non-borrowed pinned root provenance"
                )
            }
        };
        let Self::RootPinnedGeneration {
            project_authority, ..
        } = &mut self
        else {
            unreachable!("workspace output conditioning admitted only pinned root provenance")
        };
        *project_authority = authority;
        self.validate_project_authority_binding()
    }

    #[cfg(test)]
    pub(crate) fn root_pushed_head_for_test(
        effective_path: PathBuf,
        original_project_path: PathBuf,
        request_engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        match workspace_lifeline.path() {
            Some(_) if workspace_lifeline.owns_effective_path(&effective_path) => {}
            Some(_) => {
                anyhow::bail!("test pinned workspace lifeline does not match its effective path")
            }
            None => anyhow::bail!("test pinned workspace lifeline is disarmed"),
        }
        Self::RootPinnedGeneration {
            request_engine,
            original_project_path,
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            pinned_materialization: PinnedMaterializationAuthority::Fixture,
            project_authority,
            candidate_evaluation: None,
            __seal: ProvenanceSeal(()),
        }
        .validate_project_authority_binding()
    }

    fn validate_project_authority_binding(mut self) -> anyhow::Result<Self> {
        let authority = self.project_authority().clone();
        authority.validate()?;
        let shape_matches = matches!(
            (&self, &authority),
            (
                Self::Projectless { .. },
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
            ) | (
                Self::RootLiveProject { .. } | Self::ChildLiveProject { .. },
                ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
            ) | (
                Self::RootPinnedGeneration { .. }
                    | Self::ChildPinnedGeneration { .. }
                    | Self::ChildImmutableWorkspaceInput { .. },
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration { .. }
            )
        );
        if !shape_matches {
            anyhow::bail!(
                "execution project authority kind does not match provenance kind: authority {authority:?}, provenance {:?}",
                self.project_source()
            );
        }
        let projected_root = authority.project_root_projection();
        let projected_snapshot = authority.operational_snapshot_projection();
        if projected_root != Some(self.original_project_path())
            && !matches!(
                &authority,
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
            )
        {
            anyhow::bail!(
                "execution project authority root does not match provenance root: authority {:?}, provenance {}",
                projected_root,
                self.original_project_path().display()
            );
        }
        if matches!(
            &authority,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        ) && self.effective_path() != self.original_project_path()
        {
            anyhow::bail!(
                "live execution effective path {} differs from canonical project root {}",
                self.effective_path().display(),
                self.original_project_path().display()
            );
        }
        if projected_snapshot != self.pinned_snapshot_hash() {
            anyhow::bail!(
                "execution project authority snapshot does not match provenance snapshot: authority {:?}, provenance {:?}",
                projected_snapshot,
                self.pinned_snapshot_hash()
            );
        }
        if let Self::RootPinnedGeneration {
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            pinned_materialization,
            ..
        }
        | Self::ChildPinnedGeneration {
            effective_path,
            workspace_lifeline,
            base_snapshot_hash: snapshot_hash,
            pinned_materialization,
            ..
        } = &self
        {
            if !workspace_lifeline.owns_effective_path(effective_path) {
                anyhow::bail!("pinned provenance lifeline does not own its effective project path");
            }
            if let Some(materialization) = pinned_materialization.verified() {
                if materialization.snapshot_hash() != snapshot_hash
                    || !materialization.owns_path(effective_path)?
                {
                    anyhow::bail!(
                        "pinned provenance materialization proof contradicts its snapshot or path"
                    );
                }
                materialization.ensure_root_binding()?;
            }
        }
        if let Self::ChildImmutableWorkspaceInput {
            subject_effective_path,
            subject_workspace_lifeline,
            base_snapshot_hash,
            subject_pinned_materialization,
            effective_path,
            workspace_lifeline,
            input_snapshot_hash,
            input_output_capture_hash,
            input_pinned_materialization,
            ..
        } = &self
        {
            if !subject_workspace_lifeline.owns_effective_path(subject_effective_path) {
                anyhow::bail!("immutable-input provenance lost its sealed subject materialization");
            }
            if let Some(materialization) = subject_pinned_materialization.verified() {
                if materialization.snapshot_hash() != base_snapshot_hash
                    || !materialization.owns_path(subject_effective_path)?
                {
                    anyhow::bail!(
                        "immutable-input subject materialization contradicts its sealed generation"
                    );
                }
                materialization.ensure_root_binding()?;
            }
            if !workspace_lifeline.owns_effective_path(effective_path) {
                anyhow::bail!("immutable-input lifeline does not own its execution path");
            }
            if let Some(materialization) = input_pinned_materialization.verified() {
                if materialization.snapshot_hash() != input_snapshot_hash
                    || !materialization.owns_path(effective_path)?
                {
                    anyhow::bail!(
                        "immutable-input materialization contradicts its captured generation"
                    );
                }
                materialization.ensure_root_binding()?;
            }
            ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: input_snapshot_hash.clone(),
                output_capture_hash: input_output_capture_hash.clone(),
            }
            .validate()?;
        }
        match &mut self {
            Self::Projectless {
                project_authority: slot,
                ..
            }
            | Self::RootLiveProject {
                project_authority: slot,
                ..
            }
            | Self::RootPinnedGeneration {
                project_authority: slot,
                ..
            }
            | Self::ChildLiveProject {
                project_authority: slot,
                ..
            }
            | Self::ChildPinnedGeneration {
                project_authority: slot,
                ..
            }
            | Self::ChildImmutableWorkspaceInput {
                project_authority: slot,
                ..
            } => *slot = authority,
        }
        Ok(self)
    }

    pub fn project_authority(&self) -> &ryeos_state::objects::ExecutionProjectAuthority {
        match self {
            Self::Projectless {
                project_authority, ..
            }
            | Self::RootLiveProject {
                project_authority, ..
            }
            | Self::RootPinnedGeneration {
                project_authority, ..
            }
            | Self::ChildLiveProject {
                project_authority, ..
            }
            | Self::ChildPinnedGeneration {
                project_authority, ..
            }
            | Self::ChildImmutableWorkspaceInput {
                project_authority, ..
            } => project_authority,
        }
    }

    /// Select the canonical live project root for a daemon-mediated read.
    ///
    /// A pinned materialization is an execution view, never an ambient route
    /// back to mutable live state.
    pub fn durable_live_read_root(&self) -> anyhow::Result<&Path> {
        let root = self.project_authority().authorized_live_read_root()?;
        if root != self.original_project_path() {
            anyhow::bail!(
                "durable live read root {} does not match provenance project identity {}",
                root.display(),
                self.original_project_path().display()
            );
        }
        Ok(root)
    }

    /// Select the durable project root for a daemon-mediated live mutation.
    ///
    /// `effective_path()` is an execution view and may be an ephemeral
    /// materialization. Durable mutation authority comes only from the sealed
    /// project authority and must remain bound to the original project
    /// identity carried by this provenance.
    pub fn durable_live_write_root(&self, namespace: &str) -> anyhow::Result<&Path> {
        let root = self
            .project_authority()
            .authorized_live_write_root(namespace)?;
        if root != self.original_project_path() {
            anyhow::bail!(
                "durable live write root {} does not match provenance project identity {}",
                root.display(),
                self.original_project_path().display()
            );
        }
        Ok(root)
    }

    /// Resolve the original project root for a daemon-mediated item
    /// publication.
    ///
    /// This is deliberately distinct from live-project filesystem authority.
    /// A pinned execution never receives ambient access to this path. The
    /// callback service must separately prove the sealed caller project-write
    /// ceiling and the item's exact authoring capability before using the
    /// returned root. Pinned authority contributes only the path-and-identity
    /// fence retained at admission.
    pub fn durable_item_publication_root(&self, namespace: &str) -> anyhow::Result<&Path> {
        if namespace != "project" {
            anyhow::bail!(
                "daemon-mediated item publication does not support namespace `{namespace}`"
            );
        }
        let root = match self.project_authority() {
            authority @ ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. } => {
                authority.authorized_live_write_root(namespace)?
            }
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                stable_project_identity,
                display_path,
                environment,
                ..
            } => {
                let root = display_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("pinned item publication has no admitted original project path")
                })?;
                let ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                    project_authority_id,
                    ..
                } = environment
                else {
                    anyhow::bail!(
                        "pinned item publication has no original project authority binding"
                    );
                };
                let expected_authority_id = lillux::sha256_hex(
                    format!(
                        "live-project\0{}\0{}",
                        stable_project_identity,
                        root.display(),
                    )
                    .as_bytes(),
                );
                if project_authority_id != &expected_authority_id {
                    anyhow::bail!(
                        "pinned item publication project authority identity is not canonical"
                    );
                }
                root
            }
            ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
                anyhow::bail!("projectless execution cannot publish project items")
            }
        };
        if root != self.original_project_path() {
            anyhow::bail!(
                "durable item publication root {} does not match provenance project identity {}",
                root.display(),
                self.original_project_path().display()
            );
        }
        Ok(root)
    }

    /// Select the private workspace root for daemon-mediated item authoring
    /// inside an explicitly admitted candidate-integration operation.
    ///
    /// This is additive to normal live publication: ordinary provenance still
    /// uses [`Self::durable_item_publication_root`]. A candidate evaluator has
    /// no authoring root, and borrowed children cannot inherit one because
    /// child authority drops retain-current-HEAD publication.
    pub fn candidate_item_authoring_root(&self) -> anyhow::Result<Option<&Path>> {
        let Some(scope) = self.candidate_evaluation_scope() else {
            return Ok(None);
        };
        if !matches!(
            &scope.authority().purpose,
            crate::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
        ) {
            return Ok(None);
        }
        let matches_scope = matches!(
            self.project_authority(),
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                base_snapshot_hash,
                snapshot_hash,
                realization:
                    ryeos_state::objects::PinnedProjectRealization::Cow {
                        terminal_publication:
                            ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                                expected_hash,
                                ..
                            }
                    },
                environment: ryeos_state::objects::EnvironmentAuthority::None,
                ..
            } if base_snapshot_hash == &scope.authority().base_snapshot_hash
                && snapshot_hash == &scope.authority().candidate_snapshot_hash
                && expected_hash == &scope.authority().base_snapshot_hash
        );
        if !matches_scope || !self.effective_path().is_absolute() {
            anyhow::bail!(
                "candidate item authoring provenance contradicts its retained integration workspace"
            );
        }
        Ok(Some(self.effective_path()))
    }

    pub fn advances_project_head(&self) -> bool {
        matches!(
            self.project_authority(),
            ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                realization: ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication:
                        ryeos_state::objects::PinnedTerminalPublication::AdvanceHead { .. },
                },
                ..
            }
        )
    }

    pub fn environment_authority(&self) -> ryeos_state::objects::EnvironmentAuthority {
        self.project_authority().environment().clone()
    }

    /// Attach the narrow independently-authorized candidate-operation scope.
    /// Ordinary live and pinned execution never calls this; the owner service
    /// constructs it after independently materializing the trusted base and
    /// candidate generations.
    pub fn with_candidate_evaluation_scope(
        mut self,
        scope: Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>,
    ) -> anyhow::Result<Self> {
        let candidate = scope.authority().candidate_snapshot_hash.as_str();
        let authority_matches = match (&scope.authority().purpose, self.project_authority()) {
            (
                crate::thread_lifecycle::CandidateOperationPurpose::Evaluate,
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                    base_snapshot_hash,
                    snapshot_hash,
                    realization:
                        ryeos_state::objects::PinnedProjectRealization::ReadOnly
                        | ryeos_state::objects::PinnedProjectRealization::Cow {
                            terminal_publication:
                                ryeos_state::objects::PinnedTerminalPublication::Discard,
                        },
                    environment: ryeos_state::objects::EnvironmentAuthority::None,
                    ..
                },
            ) => base_snapshot_hash == candidate && snapshot_hash == candidate,
            (
                crate::thread_lifecycle::CandidateOperationPurpose::Integrate { .. },
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                    base_snapshot_hash,
                    snapshot_hash,
                    realization:
                        ryeos_state::objects::PinnedProjectRealization::Cow {
                            terminal_publication:
                                ryeos_state::objects::PinnedTerminalPublication::RetainCurrentHead {
                                    expected_hash,
                                    ..
                                },
                        },
                    environment: ryeos_state::objects::EnvironmentAuthority::None,
                    ..
                },
            ) => {
                base_snapshot_hash == &scope.authority().base_snapshot_hash
                    && snapshot_hash == candidate
                    && expected_hash == &scope.authority().base_snapshot_hash
            }
            _ => false,
        };
        if !authority_matches || !Arc::ptr_eq(self.request_engine(), scope.request_engine()) {
            anyhow::bail!(
                "candidate operation provenance contradicts its candidate/base execution scope"
            );
        }
        match &mut self {
            Self::Projectless {
                candidate_evaluation,
                ..
            }
            | Self::RootLiveProject {
                candidate_evaluation,
                ..
            }
            | Self::RootPinnedGeneration {
                candidate_evaluation,
                ..
            }
            | Self::ChildLiveProject {
                candidate_evaluation,
                ..
            }
            | Self::ChildPinnedGeneration {
                candidate_evaluation,
                ..
            }
            | Self::ChildImmutableWorkspaceInput {
                candidate_evaluation,
                ..
            } => *candidate_evaluation = Some(scope),
        }
        Ok(self)
    }

    pub fn candidate_evaluation_scope(
        &self,
    ) -> Option<&Arc<crate::thread_lifecycle::CandidateEvaluationExecutionScope>> {
        match self {
            Self::Projectless {
                candidate_evaluation,
                ..
            }
            | Self::RootLiveProject {
                candidate_evaluation,
                ..
            }
            | Self::RootPinnedGeneration {
                candidate_evaluation,
                ..
            }
            | Self::ChildLiveProject {
                candidate_evaluation,
                ..
            }
            | Self::ChildPinnedGeneration {
                candidate_evaluation,
                ..
            }
            | Self::ChildImmutableWorkspaceInput {
                candidate_evaluation,
                ..
            } => candidate_evaluation.as_ref(),
        }
    }

    /// Derive borrowed-callback-child provenance from this parent.
    ///
    /// "Borrowed" is intentionally limited to the immutable request engine,
    /// project/workspace authority, and the lifeline that keeps that exact
    /// workspace present. It does not copy the parent's prepared runtime
    /// launch, process environment, executable dependencies, effect grant, or
    /// resource limits. The child is an ordinary RyeOS execution and must
    /// resolve and admit its own effective program under those borrowed
    /// project coordinates. Do not turn provenance into a deferred child
    /// environment carrier; that would create a second program authority.
    pub fn clone_for_borrowed_child(&self) -> Self {
        match self {
            Self::Projectless {
                request_engine,
                effective_path,
                workspace_lifeline,
                project_authority,
                candidate_evaluation,
                ..
            } => Self::Projectless {
                request_engine: request_engine.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                project_authority: child_authority(project_authority),
                candidate_evaluation: candidate_evaluation.clone(),
                is_child: true,
                __seal: ProvenanceSeal(()),
            },
            Self::RootLiveProject {
                request_engine,
                project_path,
                original_project_path,
                workspace_lifeline,
                state_root,
                project_authority,
                candidate_evaluation,
                ..
            }
            | Self::ChildLiveProject {
                request_engine,
                project_path,
                original_project_path,
                workspace_lifeline,
                state_root,
                project_authority,
                candidate_evaluation,
                ..
            } => Self::ChildLiveProject {
                request_engine: request_engine.clone(),
                project_path: project_path.clone(),
                original_project_path: original_project_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                state_root: state_root.clone(),
                project_authority: child_authority(project_authority),
                candidate_evaluation: candidate_evaluation.clone(),
                __seal: ProvenanceSeal(()),
            },
            Self::RootPinnedGeneration {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                snapshot_hash,
                pinned_materialization,
                project_authority,
                candidate_evaluation,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                base_snapshot_hash: snapshot_hash.clone(),
                pinned_materialization: pinned_materialization.clone(),
                project_authority: child_authority(project_authority),
                candidate_evaluation: candidate_evaluation.clone(),
                __seal: ProvenanceSeal(()),
            },
            Self::ChildPinnedGeneration {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                base_snapshot_hash,
                pinned_materialization,
                project_authority,
                candidate_evaluation,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                base_snapshot_hash: base_snapshot_hash.clone(),
                pinned_materialization: pinned_materialization.clone(),
                project_authority: child_authority(project_authority),
                candidate_evaluation: candidate_evaluation.clone(),
                __seal: ProvenanceSeal(()),
            },
            Self::ChildImmutableWorkspaceInput {
                request_engine,
                original_project_path,
                subject_effective_path,
                subject_workspace_lifeline,
                base_snapshot_hash,
                subject_pinned_materialization,
                effective_path,
                workspace_lifeline,
                input_snapshot_hash,
                input_output_capture_hash,
                input_pinned_materialization,
                project_authority,
                candidate_evaluation,
                ..
            } => Self::ChildImmutableWorkspaceInput {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                subject_effective_path: subject_effective_path.clone(),
                subject_workspace_lifeline: subject_workspace_lifeline.clone(),
                base_snapshot_hash: base_snapshot_hash.clone(),
                subject_pinned_materialization: subject_pinned_materialization.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                input_snapshot_hash: input_snapshot_hash.clone(),
                input_output_capture_hash: input_output_capture_hash.clone(),
                input_pinned_materialization: input_pinned_materialization.clone(),
                project_authority: child_authority(project_authority),
                candidate_evaluation: candidate_evaluation.clone(),
                __seal: ProvenanceSeal(()),
            },
        }
    }

    /// Give an already-borrowed pinned child a separately materialized,
    /// immutable view of the current shared workspace. The subject-side path,
    /// materialization and project authority remain untouched so the captured
    /// input cannot become a new item or external-content consumer authority.
    ///
    /// Candidate-freezing state is not an implementation shortcut here: this
    /// transient read generation never changes the root workspace lifecycle.
    pub fn with_immutable_workspace_input(
        self,
        input_generation: ryeos_state::objects::WorkspaceGenerationPair,
        input_materialization: ryeos_state::PinnedProjectMaterialization,
        input_workspace_lifeline: Arc<TempDirGuard>,
    ) -> anyhow::Result<Self> {
        input_generation.validate()?;
        input_materialization.ensure_root_binding()?;
        if input_materialization.snapshot_hash() != input_generation.snapshot_hash.as_str() {
            anyhow::bail!(
                "immutable workspace-input materialization does not match its captured generation"
            );
        }
        let effective_path = input_materialization.path().to_path_buf();
        if !input_workspace_lifeline.owns_effective_path(&effective_path) {
            anyhow::bail!("immutable workspace-input lifeline does not own its materialization");
        }
        let Self::ChildPinnedGeneration {
            request_engine,
            original_project_path,
            effective_path: subject_effective_path,
            workspace_lifeline: subject_workspace_lifeline,
            base_snapshot_hash,
            pinned_materialization: subject_pinned_materialization,
            project_authority,
            candidate_evaluation,
            ..
        } = self
        else {
            anyhow::bail!("immutable workspace input requires borrowed pinned provenance");
        };
        Self::ChildImmutableWorkspaceInput {
            request_engine,
            original_project_path,
            subject_effective_path,
            subject_workspace_lifeline,
            base_snapshot_hash,
            subject_pinned_materialization,
            effective_path,
            workspace_lifeline: input_workspace_lifeline,
            input_snapshot_hash: input_generation.snapshot_hash,
            input_output_capture_hash: input_generation.output_capture_hash,
            input_pinned_materialization: PinnedMaterializationAuthority::Verified(
                input_materialization,
            ),
            project_authority,
            candidate_evaluation,
            __seal: ProvenanceSeal(()),
        }
        .validate_project_authority_binding()
    }

    /// Construct provenance for a fresh child chain root that executes from
    /// its own pinned materialization. This is deliberately a root provenance:
    /// unlike a callback child created with `clone_for_borrowed_child`, the
    /// new chain owns this workspace's claim, process attachment, and terminal
    /// fold/discard lifecycle.
    pub fn root_for_pinned_child_workspace(
        &self,
        request_engine: Arc<Engine>,
        pinned_materialization: ryeos_state::PinnedProjectMaterialization,
        workspace_lifeline: Arc<TempDirGuard>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        pinned_materialization.ensure_root_binding()?;
        let effective_path = pinned_materialization.path().to_path_buf();
        let snapshot_hash = pinned_materialization.snapshot_hash().to_owned();
        match workspace_lifeline.path() {
            Some(_) if workspace_lifeline.owns_effective_path(&effective_path) => {}
            Some(root) => anyhow::bail!(
                "pinned child workspace lifeline {} does not own {}",
                root.display(),
                effective_path.display()
            ),
            None => anyhow::bail!("pinned child workspace lifeline is disarmed"),
        }
        if project_authority.operational_snapshot_projection() != Some(snapshot_hash.as_str()) {
            anyhow::bail!("pinned child authority does not match child snapshot");
        }
        let provenance = Self::RootPinnedGeneration {
            request_engine,
            original_project_path: self.original_project_path().to_path_buf(),
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            pinned_materialization: PinnedMaterializationAuthority::Verified(
                pinned_materialization,
            ),
            project_authority,
            candidate_evaluation: self.candidate_evaluation_scope().cloned(),
            __seal: ProvenanceSeal(()),
        };
        provenance.project_authority().validate()?;
        Ok(provenance)
    }

    /// The engine to use for resolution / verification / execution.
    pub fn request_engine(&self) -> &Arc<Engine> {
        match self {
            Self::Projectless { request_engine, .. }
            | Self::RootLiveProject { request_engine, .. }
            | Self::RootPinnedGeneration { request_engine, .. }
            | Self::ChildLiveProject { request_engine, .. }
            | Self::ChildPinnedGeneration { request_engine, .. } => request_engine,
            Self::ChildImmutableWorkspaceInput { request_engine, .. } => request_engine,
        }
    }

    /// The directory the execution runs against.
    pub fn effective_path(&self) -> &Path {
        match self {
            Self::Projectless { effective_path, .. } => effective_path.as_path(),
            Self::RootLiveProject { project_path, .. }
            | Self::ChildLiveProject { project_path, .. } => project_path.as_path(),
            Self::RootPinnedGeneration { effective_path, .. }
            | Self::ChildPinnedGeneration { effective_path, .. }
            | Self::ChildImmutableWorkspaceInput { effective_path, .. } => effective_path.as_path(),
        }
    }

    /// Filesystem root from which the child program was resolved and admitted.
    /// This differs from `effective_path` only for an immutable shared-workspace
    /// read, whose process sees a later captured generation.
    pub fn subject_effective_path(&self) -> &Path {
        match self {
            Self::ChildImmutableWorkspaceInput {
                subject_effective_path,
                ..
            } => subject_effective_path,
            _ => self.effective_path(),
        }
    }

    /// The caller-side live project root. Ordinary live-FS execution uses the
    /// same path for execution and overlays; a resumed pinned local snapshot
    /// executes from a materialized checkout while retaining this source path.
    pub fn original_project_path(&self) -> &Path {
        match self {
            Self::Projectless { effective_path, .. } => effective_path.as_path(),
            Self::RootLiveProject {
                original_project_path,
                ..
            }
            | Self::ChildLiveProject {
                original_project_path,
                ..
            } => original_project_path.as_path(),
            Self::RootPinnedGeneration {
                original_project_path,
                ..
            }
            | Self::ChildPinnedGeneration {
                original_project_path,
                ..
            }
            | Self::ChildImmutableWorkspaceInput {
                original_project_path,
                ..
            } => original_project_path.as_path(),
        }
    }

    /// Exact immutable generation used by pinned provenance. Live provenance
    /// never carries an optional snapshot that can change its semantics.
    pub fn pinned_snapshot_hash(&self) -> Option<&str> {
        match self {
            Self::Projectless { .. }
            | Self::RootLiveProject { .. }
            | Self::ChildLiveProject { .. } => None,
            Self::RootPinnedGeneration { snapshot_hash, .. } => Some(snapshot_hash),
            Self::ChildPinnedGeneration {
                base_snapshot_hash, ..
            }
            | Self::ChildImmutableWorkspaceInput {
                base_snapshot_hash, ..
            } => Some(base_snapshot_hash),
        }
    }

    pub fn immutable_workspace_input_snapshot_hash(&self) -> Option<&str> {
        match self {
            Self::ChildImmutableWorkspaceInput {
                input_snapshot_hash,
                ..
            } => Some(input_snapshot_hash),
            _ => None,
        }
    }

    /// Exact source/output generation presented to an immutable borrowed
    /// child's process. This is distinct from the unchanged project authority
    /// used to resolve and admit the child program.
    pub fn immutable_workspace_input_generation(
        &self,
    ) -> Option<ryeos_state::objects::WorkspaceGenerationPair> {
        match self {
            Self::ChildImmutableWorkspaceInput {
                input_snapshot_hash,
                input_output_capture_hash,
                ..
            } => Some(ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: input_snapshot_hash.clone(),
                output_capture_hash: input_output_capture_hash.clone(),
            }),
            _ => None,
        }
    }

    /// Project source dimension for capability gating and tracing.
    pub fn project_source(&self) -> ProjectSourceKind {
        match self {
            Self::Projectless { .. } => ProjectSourceKind::Projectless,
            Self::RootLiveProject { .. } | Self::ChildLiveProject { .. } => {
                ProjectSourceKind::LiveFs
            }
            Self::RootPinnedGeneration { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => ProjectSourceKind::PushedHead,
        }
    }

    /// Whether the project path is a daemon-created runtime workspace that is
    /// allowed to live beneath the otherwise protected app-root cache.
    pub fn isolation_project_authority(
        &self,
    ) -> ryeos_engine::isolation::IsolationProjectAuthority {
        if matches!(self, Self::ChildImmutableWorkspaceInput { .. }) {
            return ryeos_engine::isolation::IsolationProjectAuthority::ReadOnly;
        }
        isolation_project_authority_for_project(self.project_authority())
    }

    pub fn isolation_live_access_authority(
        &self,
    ) -> anyhow::Result<Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>> {
        isolation_live_access_authority_for_project(self.project_authority())
    }

    /// Clone the ephemeral workspace lifeline, when this execution owns one.
    /// Callers moving process work into a blocking task keep this Arc in that
    /// task so cancellation of the async request cannot remove the live cwd.
    pub fn workspace_lifeline(&self) -> Option<Arc<TempDirGuard>> {
        match self {
            Self::Projectless {
                workspace_lifeline, ..
            } => Some(workspace_lifeline.clone()),
            Self::RootLiveProject {
                workspace_lifeline, ..
            }
            | Self::ChildLiveProject {
                workspace_lifeline, ..
            } => workspace_lifeline.clone(),
            Self::RootPinnedGeneration {
                workspace_lifeline, ..
            }
            | Self::ChildPinnedGeneration {
                workspace_lifeline, ..
            } => Some(workspace_lifeline.clone()),
            Self::ChildImmutableWorkspaceInput {
                workspace_lifeline, ..
            } => Some(workspace_lifeline.clone()),
        }
    }

    pub fn subject_workspace_lifeline(&self) -> Option<Arc<TempDirGuard>> {
        match self {
            Self::ChildImmutableWorkspaceInput {
                subject_workspace_lifeline,
                ..
            } => Some(subject_workspace_lifeline.clone()),
            _ => self.workspace_lifeline(),
        }
    }

    pub fn pinned_materialization(&self) -> Option<&ryeos_state::PinnedProjectMaterialization> {
        match self {
            Self::RootPinnedGeneration {
                pinned_materialization,
                ..
            }
            | Self::ChildPinnedGeneration {
                pinned_materialization,
                ..
            } => pinned_materialization.verified(),
            Self::ChildImmutableWorkspaceInput {
                subject_pinned_materialization,
                ..
            } => subject_pinned_materialization.verified(),
            Self::Projectless { .. }
            | Self::RootLiveProject { .. }
            | Self::ChildLiveProject { .. } => None,
        }
    }

    /// Exact process input, which differs from definition/subject authority
    /// for immutable children. Launch must not substitute the parent's pinned
    /// materialization merely because it resolves the same signed operation.
    pub fn execution_input_materialization(
        &self,
    ) -> Option<&ryeos_state::PinnedProjectMaterialization> {
        match self {
            Self::ChildImmutableWorkspaceInput {
                input_pinned_materialization,
                ..
            } => input_pinned_materialization.verified(),
            Self::RootPinnedGeneration {
                pinned_materialization,
                ..
            }
            | Self::ChildPinnedGeneration {
                pinned_materialization,
                ..
            } => pinned_materialization.verified(),
            Self::Projectless { .. }
            | Self::RootLiveProject { .. }
            | Self::ChildLiveProject { .. } => None,
        }
    }

    pub fn immutable_workspace_input_materialization(
        &self,
    ) -> Option<&ryeos_state::PinnedProjectMaterialization> {
        match self {
            Self::ChildImmutableWorkspaceInput {
                input_pinned_materialization,
                ..
            } => input_pinned_materialization.verified(),
            _ => None,
        }
    }

    /// True iff this execution must skip pin + foldback because a root
    /// parent owns the snapshot lifecycle.
    ///
    /// Written as an exhaustive match (not `matches!`) so adding another
    /// variant is a compile error here. The
    /// runner's lifecycle gates depend on this predicate; a silent
    /// default would skip or duplicate pin/foldback for the new role.
    pub fn is_borrowed_child(&self) -> bool {
        match self {
            Self::Projectless { is_child, .. } => *is_child,
            Self::RootLiveProject { .. } | Self::RootPinnedGeneration { .. } => false,
            Self::ChildLiveProject { .. }
            | Self::ChildPinnedGeneration { .. }
            | Self::ChildImmutableWorkspaceInput { .. } => true,
        }
    }
}

fn subject_resolution_authority_for_pinned(
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> ryeos_engine::contracts::SubjectResolutionAuthority {
    match project_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ..
        } => ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot_hash.clone(),
        },
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash,
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::Cow { .. },
            ..
        } => ryeos_engine::contracts::SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: base_snapshot_hash.clone(),
            current_operational_generation: snapshot_hash.clone(),
        },
        ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. }
        | ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. } => {
            unreachable!("pinned provenance was already validated against pinned authority")
        }
    }
}

fn child_authority(
    authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> ryeos_state::objects::ExecutionProjectAuthority {
    authority
        .clone()
        .for_child()
        .expect("validated parent project authority must derive a valid child authority")
}

impl std::fmt::Debug for ExecutionProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionProvenance")
            .field(
                "role",
                &if self.is_borrowed_child() {
                    "BorrowedCallbackChild"
                } else {
                    "Root"
                },
            )
            .field("project_source", &self.project_source())
            .field("original_project_path", &self.original_project_path())
            .field("effective_path", &self.effective_path())
            .field(
                "has_lifeline",
                &match self {
                    Self::Projectless { .. } => true,
                    Self::RootLiveProject {
                        workspace_lifeline, ..
                    }
                    | Self::ChildLiveProject {
                        workspace_lifeline, ..
                    } => workspace_lifeline.is_some(),
                    Self::RootPinnedGeneration { .. }
                    | Self::ChildPinnedGeneration { .. }
                    | Self::ChildImmutableWorkspaceInput { .. } => true,
                },
            )
            .field(
                "snapshot_hash",
                &match self {
                    Self::Projectless { .. }
                    | Self::RootLiveProject { .. }
                    | Self::ChildLiveProject { .. } => None,
                    Self::RootPinnedGeneration { snapshot_hash, .. } => {
                        Some(snapshot_hash.as_str())
                    }
                    Self::ChildPinnedGeneration {
                        base_snapshot_hash, ..
                    }
                    | Self::ChildImmutableWorkspaceInput {
                        base_snapshot_hash, ..
                    } => Some(base_snapshot_hash.as_str()),
                },
            )
            .field(
                "immutable_workspace_input_snapshot_hash",
                &self.immutable_workspace_input_snapshot_hash(),
            )
            .field(
                "immutable_workspace_input_generation",
                &self.immutable_workspace_input_generation(),
            )
            .field("state_root", &self.state_root_override())
            .field(
                "engine_arc_strong_count",
                &Arc::strong_count(self.request_engine()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_policy::ExecutionPolicyResolution;

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            vec![],
        ))
    }

    fn live(path: &str, request_engine: Arc<Engine>) -> ExecutionProvenance {
        let path = PathBuf::from(path);
        let authority =
            crate::execution_policy::synthetic_test_live_project_authority(path.as_path());
        ExecutionProvenance::root_live_fs(path, request_engine, authority).unwrap()
    }

    fn pinned(
        original_path: &Path,
        snapshot_hash: &str,
    ) -> ryeos_state::objects::ExecutionProjectAuthority {
        ryeos_state::objects::ExecutionProjectAuthority::pinned(
            format!("site:test:{}", original_path.display()),
            Some(original_path.to_path_buf()),
            snapshot_hash.to_string(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap()
    }

    fn pinned_with_item_publication(
        original_path: &Path,
        snapshot_hash: &str,
    ) -> ryeos_state::objects::ExecutionProjectAuthority {
        let stable_project_identity = format!("local:{}", original_path.display());
        let project_authority_id = lillux::sha256_hex(
            format!(
                "live-project\0{}\0{}",
                stable_project_identity,
                original_path.display()
            )
            .as_bytes(),
        );
        ryeos_state::objects::ExecutionProjectAuthority::pinned(
            stable_project_identity,
            Some(original_path.to_path_buf()),
            snapshot_hash.to_string(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id,
                source_identity: format!("dotenv:{}/.env", original_path.display()),
                include_operator_vault: true,
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
            vec![crate::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
        )
        .unwrap()
    }

    #[test]
    fn root_live_fs_constructor_sets_paths_equal() {
        let p = live("/live", engine());

        assert_eq!(p.effective_path(), Path::new("/live"));
        assert_eq!(p.original_project_path(), Path::new("/live"));
        assert!(matches!(p, ExecutionProvenance::RootLiveProject { .. }));
        assert_eq!(p.project_source(), ProjectSourceKind::LiveFs);
        assert!(!p.is_borrowed_child());
    }

    #[test]
    fn state_root_override_does_not_replace_durable_live_project_authority() {
        let provenance =
            live("/live", engine()).with_state_root(Some(PathBuf::from("/separate-state")));

        assert_eq!(
            provenance.state_root_override(),
            Some(Path::new("/separate-state"))
        );
        assert_eq!(
            provenance.durable_live_read_root().unwrap(),
            Path::new("/live")
        );
        assert_eq!(
            provenance.durable_live_write_root("project").unwrap(),
            Path::new("/live")
        );
    }

    #[test]
    fn descriptor_rooted_live_authority_preserves_root_identity_and_masks() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(ryeos_engine::AI_DIR)).unwrap();
        let policy = crate::execution_policy::ExecutionPolicy::local_live(
            crate::execution_policy::ExecutionResponse::Wait,
        );
        let project_authority = policy
            .resolve_live_project_authority(
                project.path(),
                ryeos_state::objects::LiveFilesystemConfinement::standard_fixed_parents(),
                vec![crate::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
            )
            .unwrap();

        assert_eq!(
            isolation_project_authority_for_project(&project_authority),
            ryeos_engine::isolation::IsolationProjectAuthority::External
        );
        let live_access = isolation_live_access_authority_for_project(&project_authority)
            .unwrap()
            .expect("live authority");
        let ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedFixedParents {
            root: retained_root,
            root_device_id,
            root_inode,
            denied_control_paths,
            authorized_write_namespaces,
        } = live_access
        else {
            panic!("expected descriptor-rooted confinement");
        };
        let root = lillux::secure_fs::PinnedDirectory::open(project.path())
            .unwrap()
            .expect("project root");
        assert_eq!((root_device_id, root_inode), root.device_inode().unwrap());
        assert!(retained_root.is_same_directory(&root).unwrap());
        assert_eq!(
            denied_control_paths,
            ryeos_state::project_sync::live_execution_denied_control_paths()
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(authorized_write_namespaces, vec!["project".to_string()]);
    }

    #[test]
    fn descriptor_rooted_live_authority_detects_path_replacement_without_losing_its_inode() {
        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join(ryeos_engine::AI_DIR)).unwrap();
        let policy = crate::execution_policy::ExecutionPolicy::local_live(
            crate::execution_policy::ExecutionResponse::Wait,
        );
        let project_authority = policy
            .resolve_live_project_authority(
                &project,
                ryeos_state::objects::LiveFilesystemConfinement::standard_fixed_parents(),
                vec![crate::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
            )
            .unwrap();
        let live_access = isolation_live_access_authority_for_project(&project_authority)
            .unwrap()
            .unwrap();
        let ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedFixedParents {
            root,
            root_device_id,
            root_inode,
            ..
        } = live_access
        else {
            panic!("expected descriptor-rooted authority");
        };

        let displaced = parent.path().join("displaced");
        std::fs::rename(&project, &displaced).unwrap();
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join(ryeos_engine::AI_DIR)).unwrap();

        assert!(root.ensure_path_binding().is_err());
        assert_eq!((root_device_id, root_inode), root.device_inode().unwrap());
        let displaced_root = lillux::PinnedDirectory::open(&displaced).unwrap().unwrap();
        assert!(root.is_same_directory(&displaced_root).unwrap());
    }

    #[test]
    fn pinned_local_materialization_remains_pinned_for_borrowed_children() {
        let dir = tempfile::tempdir().unwrap();
        let effective_path = dir.path().to_path_buf();
        let lifeline = Arc::new(TempDirGuard::new(effective_path.clone()));
        let original_path = PathBuf::from("/home/operator/project");
        let parent = ExecutionProvenance::root_pushed_head_for_test(
            effective_path.clone(),
            original_path.clone(),
            engine(),
            lifeline,
            "ab".repeat(32),
            pinned(&original_path, &"ab".repeat(32)),
        )
        .unwrap();

        assert_eq!(parent.effective_path(), effective_path);
        assert_eq!(parent.original_project_path(), original_path);
        assert_eq!(parent.project_source(), ProjectSourceKind::PushedHead);
        assert!(!parent.is_borrowed_child());

        let child = parent.clone_for_borrowed_child();
        assert!(matches!(
            child,
            ExecutionProvenance::ChildPinnedGeneration { .. }
        ));
        assert_eq!(child.effective_path(), effective_path);
        assert_eq!(child.original_project_path(), original_path);
        assert!(child.is_borrowed_child());
    }

    #[test]
    fn immutable_workspace_children_preserve_independent_candidate_evaluation_scope() {
        use crate::thread_lifecycle::{
            AdmittedProjectBinding, CandidateEvaluationAuthority,
            CandidateEvaluationExecutionScope, CandidateOperationPurpose,
        };
        use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
        use ryeos_state::objects::{
            EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
        };

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let request_engine = engine();
        let owner = format!("fp:{}", "a".repeat(64));
        let base_hash = "b".repeat(64);
        let candidate_hash = "c".repeat(64);
        let input_hash = "d".repeat(64);
        let materialize = |name: &str, hash: &str| {
            let root = directory.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let proof = ryeos_state::PinnedProjectMaterialization::from_observed_tree_for_test(
                hash.to_owned(),
                &root,
                Default::default(),
            )
            .unwrap();
            (proof, Arc::new(TempDirGuard::new(root)))
        };
        let provenance = |name: &str, hash: &str| {
            let (proof, lifeline) = materialize(name, hash);
            let authority = ExecutionProjectAuthority::pinned(
                "site:test:candidate-evaluation".to_owned(),
                Some(original.clone()),
                hash.to_owned(),
                PinnedProjectRealization::ReadOnly,
                EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap();
            ExecutionProvenance::root_pushed_head(
                original.clone(),
                request_engine.clone(),
                lifeline,
                proof,
                authority,
            )
            .unwrap()
        };
        let base = provenance("base", &base_hash);
        let context = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: owner.clone(),
                scopes: Vec::new(),
            }),
            project_context: ProjectContext::LocalPath {
                path: base.effective_path().to_owned(),
            },
            subject_resolution_authority: base.subject_resolution_authority(),
            current_site_id: "site:test".to_owned(),
            origin_site_id: "site:test".to_owned(),
            execution_hints: Default::default(),
            scheduled_fire: None,
            validate_only: false,
        };
        let binding =
            AdmittedProjectBinding::from_provenance(&request_engine, &context, &base).unwrap();
        let scope = Arc::new(
            CandidateEvaluationExecutionScope::admit(
                CandidateEvaluationAuthority {
                    schema_version: CandidateEvaluationAuthority::SCHEMA_VERSION,
                    source_chain_root_id: crate::thread_lifecycle::new_thread_id(),
                    source_placement_thread_id: crate::thread_lifecycle::new_thread_id(),
                    owner_principal: owner,
                    base_snapshot_hash: base_hash.clone(),
                    candidate_snapshot_hash: candidate_hash.clone(),
                    candidate_validation_hash: "e".repeat(64),
                    integration_operation_hash: None,
                    integration_launch_id: None,
                    purpose: CandidateOperationPurpose::Evaluate,
                },
                context,
                binding,
            )
            .unwrap(),
        );
        let candidate = provenance("candidate", &candidate_hash)
            .with_candidate_evaluation_scope(scope.clone())
            .unwrap();
        let (input, input_lifeline) = materialize("input", &input_hash);
        let input_generation = ryeos_state::objects::WorkspaceGenerationPair {
            snapshot_hash: input_hash.clone(),
            output_capture_hash: Some("f".repeat(64)),
        };
        let child = candidate
            .clone_for_borrowed_child()
            .with_immutable_workspace_input(input_generation.clone(), input, input_lifeline)
            .unwrap();
        for borrowed in [&child, &child.clone_for_borrowed_child()] {
            assert!(Arc::ptr_eq(
                borrowed.candidate_evaluation_scope().unwrap(),
                &scope
            ));
            assert!(Arc::ptr_eq(
                borrowed.request_engine(),
                base.request_engine()
            ));
            assert_eq!(
                borrowed.subject_resolution_authority(),
                candidate.subject_resolution_authority()
            );
            assert_eq!(
                borrowed.immutable_workspace_input_snapshot_hash(),
                Some(input_hash.as_str())
            );
            assert_eq!(
                borrowed.immutable_workspace_input_generation(),
                Some(input_generation.clone())
            );
            assert_eq!(
                borrowed
                    .execution_input_materialization()
                    .unwrap()
                    .snapshot_hash(),
                input_hash
            );
            assert_eq!(
                borrowed.pinned_materialization().unwrap().snapshot_hash(),
                candidate_hash
            );
            assert_eq!(
                borrowed
                    .candidate_evaluation_scope()
                    .unwrap()
                    .authority()
                    .base_snapshot_hash,
                base_hash
            );
            assert!(borrowed.candidate_item_authoring_root().unwrap().is_none());
        }
    }

    #[test]
    fn root_pushed_head_constructor_succeeds_with_matching_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let lifeline = Arc::new(TempDirGuard::new(path.clone()));

        let p = ExecutionProvenance::root_pushed_head_for_test(
            path.clone(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();

        assert!(matches!(
            p,
            ExecutionProvenance::RootPinnedGeneration { .. }
        ));
        assert_eq!(p.effective_path(), path.as_path());
        assert_eq!(p.original_project_path(), Path::new("/laptop"));
        assert_eq!(p.project_source(), ProjectSourceKind::PushedHead);
    }

    #[test]
    fn root_pushed_head_constructor_rejects_lifeline_path_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));

        let error = ExecutionProvenance::root_pushed_head_for_test(
            PathBuf::from("/somewhere/else"),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its effective path")
        );
    }

    #[test]
    fn root_pushed_head_constructor_rejects_disarmed_lifeline() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        lifeline.disarm();

        let error = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("disarmed"));
    }

    #[test]
    fn clone_for_borrowed_child_from_live_fs_root_produces_borrowed_live_fs() {
        let parent = live("/live", engine());
        let child = parent.clone_for_borrowed_child();

        assert!(matches!(
            child,
            ExecutionProvenance::ChildLiveProject { .. }
        ));
        assert!(child.is_borrowed_child());
        assert_eq!(child.project_source(), ProjectSourceKind::LiveFs);
        assert_eq!(child.effective_path(), Path::new("/live"));
    }

    #[test]
    fn durable_live_write_uses_authority_root_not_materialized_execution_view() {
        let durable = tempfile::tempdir().unwrap();
        let materialized = tempfile::tempdir().unwrap();
        let authority =
            crate::execution_policy::synthetic_test_live_project_authority(durable.path());
        let lifeline = Arc::new(TempDirGuard::new(materialized.path().to_path_buf()));
        let provenance = ExecutionProvenance::ChildLiveProject {
            request_engine: engine(),
            project_path: materialized.path().to_path_buf(),
            original_project_path: durable.path().to_path_buf(),
            workspace_lifeline: Some(lifeline),
            state_root: None,
            project_authority: authority,
            candidate_evaluation: None,
            __seal: ProvenanceSeal(()),
        };

        assert_eq!(provenance.effective_path(), materialized.path());
        assert_eq!(
            provenance.durable_live_write_root("project").unwrap(),
            durable.path()
        );
    }

    #[test]
    fn pinned_item_publication_uses_admitted_original_root_not_execution_view() {
        let original = tempfile::tempdir().unwrap();
        let materialized = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(materialized.path().to_path_buf()));
        let provenance = ExecutionProvenance::root_pushed_head_for_test(
            materialized.path().to_path_buf(),
            original.path().to_path_buf(),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned_with_item_publication(original.path(), &"a".repeat(64)),
        )
        .unwrap()
        .clone_for_borrowed_child();

        assert_eq!(provenance.effective_path(), materialized.path());
        assert!(provenance.durable_live_write_root("project").is_err());
        assert_eq!(
            provenance.durable_item_publication_root("project").unwrap(),
            original.path()
        );
        assert!(
            provenance
                .durable_item_publication_root("unsupported")
                .is_err()
        );
    }

    #[test]
    fn pinned_item_publication_refuses_authority_without_original_project_binding() {
        let original = tempfile::tempdir().unwrap();
        let materialized = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(materialized.path().to_path_buf()));
        let provenance = ExecutionProvenance::root_pushed_head_for_test(
            materialized.path().to_path_buf(),
            original.path().to_path_buf(),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(original.path(), &"a".repeat(64)),
        )
        .unwrap();

        let error = provenance
            .durable_item_publication_root("project")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no original project authority binding"),
            "{error}"
        );
    }

    #[test]
    fn clone_for_borrowed_child_from_pushed_root_produces_borrowed_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let parent = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();

        let child = parent.clone_for_borrowed_child();

        assert!(matches!(
            child,
            ExecutionProvenance::ChildPinnedGeneration { .. }
        ));
        assert!(child.is_borrowed_child());
        assert_eq!(child.project_source(), ProjectSourceKind::PushedHead);
        assert_eq!(child.original_project_path(), Path::new("/laptop"));
    }

    #[test]
    fn clone_for_borrowed_child_preserves_engine_arc_identity() {
        let eng = engine();
        let parent = live("/x", eng.clone());
        let child = parent.clone_for_borrowed_child();

        assert!(Arc::ptr_eq(parent.request_engine(), child.request_engine()));
        assert!(Arc::ptr_eq(child.request_engine(), &eng));
    }

    #[test]
    fn clone_for_borrowed_child_preserves_lifeline_arc_identity_through_nesting() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline.clone(),
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();
        let child = root.clone_for_borrowed_child();
        let grandchild = child.clone_for_borrowed_child();

        let extract = |p: &ExecutionProvenance| -> Arc<TempDirGuard> {
            match p {
                ExecutionProvenance::RootPinnedGeneration {
                    workspace_lifeline, ..
                }
                | ExecutionProvenance::ChildPinnedGeneration {
                    workspace_lifeline, ..
                } => workspace_lifeline.clone(),
                _ => panic!("expected pushed variant"),
            }
        };
        let l_root = extract(&root);
        let l_child = extract(&child);
        let l_grand = extract(&grandchild);

        assert!(Arc::ptr_eq(&l_root, &l_child));
        assert!(Arc::ptr_eq(&l_child, &l_grand));
        assert!(Arc::ptr_eq(&l_root, &lifeline));
    }

    #[test]
    fn state_root_override_defaults_to_none_and_round_trips() {
        let p = live("/live", engine());
        assert_eq!(p.state_root_override(), None);

        let p = p.with_state_root(Some(PathBuf::from("/tmp/smoke")));
        assert_eq!(p.state_root_override(), Some(Path::new("/tmp/smoke")));
        // Resolution anchors are unchanged by the override.
        assert_eq!(p.effective_path(), Path::new("/live"));
        assert_eq!(p.original_project_path(), Path::new("/live"));
    }

    #[test]
    fn state_root_override_is_inherited_by_borrowed_children() {
        let parent = live("/live", engine()).with_state_root(Some(PathBuf::from("/tmp/smoke")));
        let child = parent.clone_for_borrowed_child();
        let grandchild = child.clone_for_borrowed_child();

        assert_eq!(child.state_root_override(), Some(Path::new("/tmp/smoke")));
        assert_eq!(
            grandchild.state_root_override(),
            Some(Path::new("/tmp/smoke"))
        );
    }

    #[test]
    #[should_panic(expected = "live-fs control")]
    fn with_state_root_panics_on_pushed_head() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();
        let _ = root.with_state_root(Some(PathBuf::from("/tmp/smoke")));
    }

    #[test]
    fn is_borrowed_child_true_only_for_borrowed_variants() {
        let eng = engine();
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));

        let live_root = live("/x", eng.clone());
        let pushed_root = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/y"),
            eng,
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/y"), &"a".repeat(64)),
        )
        .unwrap();
        let live_child = live_root.clone_for_borrowed_child();
        let pushed_child = pushed_root.clone_for_borrowed_child();

        assert!(!live_root.is_borrowed_child());
        assert!(!pushed_root.is_borrowed_child());
        assert!(live_child.is_borrowed_child());
        assert!(pushed_child.is_borrowed_child());
    }

    #[test]
    fn borrowed_pushed_child_has_no_snapshot_hash_field() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();

        let child = root.clone_for_borrowed_child();
        match child {
            ExecutionProvenance::ChildPinnedGeneration { .. } => {}
            other => panic!("expected ChildPinnedGeneration, got {other:?}"),
        }
    }

    #[test]
    fn root_pushed_head_carries_snapshot_hash_only_on_root_variant() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head_for_test(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();

        match &root {
            ExecutionProvenance::RootPinnedGeneration { snapshot_hash, .. } => {
                assert_eq!(snapshot_hash, &"a".repeat(64));
            }
            other => panic!("expected RootPinnedGeneration, got {other:?}"),
        }
    }
}
