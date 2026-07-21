//! Execution provenance — single source of truth, type-state encoded.
//!
//! A provenance value is constructed exactly once at the entry point
//! (HTTP route, SSE launch, scheduler tick, callback handler, resume
//! reconciler). It flows through dispatch, runner, native launch, and
//! callback token minting unchanged. Callback children are derived only
//! by cloning this value as a borrowed child.
//!
//! The five variants enumerate the five legal shapes. Invalid shapes
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

/// Single source of truth for what engine, workspace, lineage, and role
/// belong to an execution.
///
/// Construct via `root_live_fs` / `root_pushed_head`, or derive callback
/// children via `clone_for_borrowed_child`. The private seal field on
/// each variant prevents construction outside this module while still
/// permitting explicit variant matching by consumers.
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
        /// runtime-state project path advertised to the child (callback
        /// token + `RYEOSD_PROJECT_PATH`) points here. `None` = state
        /// lives under the project as usual.
        state_root: Option<PathBuf>,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
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
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
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
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
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
            ryeos_state::objects::LiveFilesystemConfinement::DescriptorRootedMasked {
                denied_control_paths,
                symlink_policy: ryeos_state::objects::LiveSymlinkPolicy::DescriptorRootedNoEscape,
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
                    ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedMasked {
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
    pub fn execution_project_authority(
        &self,
        capability_ceiling: &[String],
    ) -> anyhow::Result<ryeos_state::objects::ExecutionProjectAuthority> {
        self.project_authority()
            .clone()
            .with_capability_ceiling(capability_ceiling.to_vec())
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
            Some(root) if workspace_root_owns_effective_path(&root, &effective_path) => {}
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
                Some(path) if workspace_root_owns_effective_path(&path, self.effective_path()) => {}
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
            Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. } => {
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
                    panic!("ExecutionProvenance::with_state_root: projectless execution cannot redirect project state");
                }
            }
            Self::RootLiveProject { state_root, .. }
            | Self::ChildLiveProject { state_root, .. } => {
                *state_root = override_root;
            }
            Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. } => {
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
            Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. } => None,
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
        effective_path: PathBuf,
        original_project_path: PathBuf,
        request_engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        match workspace_lifeline.path() {
            Some(p) if workspace_root_owns_effective_path(&p, &effective_path) => {}
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
            project_authority,
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
                Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. },
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
        let projected_snapshot = authority.base_snapshot_projection();
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
        if projected_snapshot != self.pinned_snapshot_hash() {
            anyhow::bail!(
                "execution project authority snapshot does not match provenance snapshot: authority {:?}, provenance {:?}",
                projected_snapshot,
                self.pinned_snapshot_hash()
            );
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
            } => project_authority,
        }
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

    /// Derive borrowed-callback-child provenance from this parent.
    pub fn clone_for_borrowed_child(&self) -> Self {
        match self {
            Self::Projectless {
                request_engine,
                effective_path,
                workspace_lifeline,
                project_authority,
                ..
            } => Self::Projectless {
                request_engine: request_engine.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                project_authority: child_authority(project_authority),
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
                ..
            }
            | Self::ChildLiveProject {
                request_engine,
                project_path,
                original_project_path,
                workspace_lifeline,
                state_root,
                project_authority,
                ..
            } => Self::ChildLiveProject {
                request_engine: request_engine.clone(),
                project_path: project_path.clone(),
                original_project_path: original_project_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                state_root: state_root.clone(),
                project_authority: child_authority(project_authority),
                __seal: ProvenanceSeal(()),
            },
            Self::RootPinnedGeneration {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                snapshot_hash,
                project_authority,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                base_snapshot_hash: snapshot_hash.clone(),
                project_authority: child_authority(project_authority),
                __seal: ProvenanceSeal(()),
            },
            Self::ChildPinnedGeneration {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                base_snapshot_hash,
                project_authority,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                base_snapshot_hash: base_snapshot_hash.clone(),
                project_authority: child_authority(project_authority),
                __seal: ProvenanceSeal(()),
            },
        }
    }

    /// Borrow immutable identity while assigning a distinct writable
    /// workspace to a branch child.
    pub fn clone_for_borrowed_child_workspace(
        &self,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
    ) -> Self {
        match self {
            Self::Projectless { .. }
            | Self::RootLiveProject { .. }
            | Self::ChildLiveProject { .. } => {
                panic!(
                    "live direct provenance cannot acquire a replacement workspace; pin the child explicitly"
                )
            }
            Self::RootPinnedGeneration {
                request_engine,
                original_project_path,
                snapshot_hash,
                project_authority,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path,
                workspace_lifeline,
                base_snapshot_hash: snapshot_hash.clone(),
                project_authority: child_authority(project_authority),
                __seal: ProvenanceSeal(()),
            },
            Self::ChildPinnedGeneration {
                request_engine,
                original_project_path,
                base_snapshot_hash,
                project_authority,
                ..
            } => Self::ChildPinnedGeneration {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path,
                workspace_lifeline,
                base_snapshot_hash: base_snapshot_hash.clone(),
                project_authority: child_authority(project_authority),
                __seal: ProvenanceSeal(()),
            },
        }
    }

    pub fn clone_for_pinned_child_workspace(
        &self,
        request_engine: Arc<Engine>,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> anyhow::Result<Self> {
        match workspace_lifeline.path() {
            Some(root) if workspace_root_owns_effective_path(&root, &effective_path) => {}
            Some(root) => anyhow::bail!(
                "pinned child workspace lifeline {} does not own {}",
                root.display(),
                effective_path.display()
            ),
            None => anyhow::bail!("pinned child workspace lifeline is disarmed"),
        }
        if project_authority.base_snapshot_projection() != Some(snapshot_hash.as_str()) {
            anyhow::bail!("pinned child authority does not match child snapshot");
        }
        let provenance = Self::ChildPinnedGeneration {
            request_engine,
            original_project_path: self.original_project_path().to_path_buf(),
            effective_path,
            workspace_lifeline,
            base_snapshot_hash: snapshot_hash,
            project_authority,
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
        }
    }

    /// The directory the execution runs against.
    pub fn effective_path(&self) -> &Path {
        match self {
            Self::Projectless { effective_path, .. } => effective_path.as_path(),
            Self::RootLiveProject { project_path, .. }
            | Self::ChildLiveProject { project_path, .. } => project_path.as_path(),
            Self::RootPinnedGeneration { effective_path, .. }
            | Self::ChildPinnedGeneration { effective_path, .. } => effective_path.as_path(),
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
            } => Some(base_snapshot_hash),
        }
    }

    /// Project source dimension for capability gating and tracing.
    pub fn project_source(&self) -> ProjectSourceKind {
        match self {
            Self::Projectless { .. } => ProjectSourceKind::Projectless,
            Self::RootLiveProject { .. } | Self::ChildLiveProject { .. } => {
                ProjectSourceKind::LiveFs
            }
            Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. } => {
                ProjectSourceKind::PushedHead
            }
        }
    }

    /// Whether the project path is a daemon-created runtime workspace that is
    /// allowed to live beneath the otherwise protected app-root cache.
    pub fn isolation_project_authority(
        &self,
    ) -> ryeos_engine::isolation::IsolationProjectAuthority {
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
            Self::ChildLiveProject { .. } | Self::ChildPinnedGeneration { .. } => true,
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

/// A direct checkout guard owns the effective directory. A COW workspace guard
/// owns its private root while the immutable lower exposed to resolution is one
/// direct child. Accept exactly those two layouts; a broad ancestor test would
/// let an unrelated shared temp root masquerade as the workspace lifetime
/// authority.
fn workspace_root_owns_effective_path(root: &Path, effective: &Path) -> bool {
    root == effective
        || (effective.parent() == Some(root)
            && effective.file_name().and_then(|name| name.to_str()) == Some("project"))
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
                    Self::RootPinnedGeneration { .. } | Self::ChildPinnedGeneration { .. } => true,
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
                    } => Some(base_snapshot_hash.as_str()),
                },
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
    fn descriptor_rooted_live_authority_preserves_root_identity_and_masks() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(ryeos_engine::AI_DIR)).unwrap();
        let policy = crate::execution_policy::ExecutionPolicy::local_live(
            crate::execution_policy::ExecutionResponse::Wait,
        );
        let project_authority = policy
            .resolve_live_project_authority(
                project.path(),
                ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
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
        let ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedMasked {
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
                ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
                vec!["project.write".to_string()],
            )
            .unwrap();
        let live_access = isolation_live_access_authority_for_project(&project_authority)
            .unwrap()
            .unwrap();
        let ryeos_engine::isolation::IsolationLiveAccessAuthority::DescriptorRootedMasked {
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
        let parent = ExecutionProvenance::root_pushed_head(
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
    fn root_pushed_head_constructor_succeeds_with_matching_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let lifeline = Arc::new(TempDirGuard::new(path.clone()));

        let p = ExecutionProvenance::root_pushed_head(
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
    #[should_panic(expected = "does not match effective_path")]
    fn root_pushed_head_constructor_panics_on_lifeline_path_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));

        let _ = ExecutionProvenance::root_pushed_head(
            PathBuf::from("/somewhere/else"),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        );
    }

    #[test]
    #[should_panic(expected = "disarmed")]
    fn root_pushed_head_constructor_panics_on_disarmed_lifeline() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        lifeline.disarm();

        let _ = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        );
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
            __seal: ProvenanceSeal(()),
        };

        assert_eq!(provenance.effective_path(), materialized.path());
        assert_eq!(
            provenance.durable_live_write_root("project").unwrap(),
            durable.path()
        );
    }

    #[test]
    fn clone_for_borrowed_child_from_pushed_root_produces_borrowed_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let parent = ExecutionProvenance::root_pushed_head(
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
        let root = ExecutionProvenance::root_pushed_head(
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
        let root = ExecutionProvenance::root_pushed_head(
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
        let pushed_root = ExecutionProvenance::root_pushed_head(
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
        let root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "a".repeat(64),
            pinned(Path::new("/laptop"), &"a".repeat(64)),
        )
        .unwrap();

        match root.clone_for_borrowed_child() {
            ExecutionProvenance::ChildPinnedGeneration { .. } => {}
            other => panic!("expected ChildPinnedGeneration, got {other:?}"),
        }
    }

    #[test]
    fn root_pushed_head_carries_snapshot_hash_only_on_root_variant() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head(
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
