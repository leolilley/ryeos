use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::validate_trimmed_control_free;

/// Persisted executor boundary used to launch and recover an admitted program.
/// This is deliberately independent of item kind and canonical-ref spelling:
/// recovery dispatches from the authority that crossed admission, not from a
/// later registry lookup or string convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLaunchDriver {
    /// The verified runtime registry selected and prepared a managed envelope.
    ManagedRuntime,
    /// The item's verified executor is launched directly by the generic runner.
    DirectItemExecutor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOwnershipAuthority {
    RequestScoped,
    DaemonOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRecoveryAuthority {
    None,
    RestartRecoverable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLifecycleAuthority {
    pub ownership: ExecutionOwnershipAuthority,
    pub recovery: ExecutionRecoveryAuthority,
}

impl ExecutionLifecycleAuthority {
    pub const DAEMON_RESTARTABLE: Self = Self {
        ownership: ExecutionOwnershipAuthority::DaemonOwned,
        recovery: ExecutionRecoveryAuthority::RestartRecoverable,
    };

    pub const DAEMON_NON_RECOVERABLE: Self = Self {
        ownership: ExecutionOwnershipAuthority::DaemonOwned,
        recovery: ExecutionRecoveryAuthority::None,
    };

    pub const REQUEST_SCOPED: Self = Self {
        ownership: ExecutionOwnershipAuthority::RequestScoped,
        recovery: ExecutionRecoveryAuthority::None,
    };

    pub fn validate(self) -> anyhow::Result<()> {
        if self.ownership == ExecutionOwnershipAuthority::RequestScoped
            && self.recovery == ExecutionRecoveryAuthority::RestartRecoverable
        {
            anyhow::bail!("request-scoped execution cannot be restart-recoverable");
        }
        Ok(())
    }

    pub fn permits_durable_handoff(self) -> bool {
        self.ownership == ExecutionOwnershipAuthority::DaemonOwned
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveProjectAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveSymlinkPolicy {
    DescriptorRootedNoEscape,
}

/// The actual filesystem confinement carried by a live-project authority.
///
/// This is persisted with the admitted execution. A descriptor-rooted
/// authority cannot later be reinterpreted as host access merely because the
/// recovering node has isolation disabled, and an explicit host-access grant
/// cannot be reported as though path masks were enforced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveFilesystemConfinement {
    DescriptorRootedMasked {
        denied_control_paths: Vec<String>,
        symlink_policy: LiveSymlinkPolicy,
    },
    UnconfinedHost,
}

impl LiveFilesystemConfinement {
    pub fn standard_descriptor_rooted() -> Self {
        Self::DescriptorRootedMasked {
            denied_control_paths: crate::project_sync::live_execution_denied_control_paths(),
            symlink_policy: LiveSymlinkPolicy::DescriptorRootedNoEscape,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveAccessAuthority {
    pub access: LiveProjectAccess,
    pub authorized_write_namespaces: Vec<String>,
    pub confinement: LiveFilesystemConfinement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PinnedProjectRealization {
    ReadOnly,
    Cow {
        terminal_publication: PinnedTerminalPublication,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PinnedTerminalPublication {
    Discard,
    RetainResult,
    AdvanceHead {
        head_ref: String,
        expected_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildProjectAuthorityPolicy {
    Inherit,
    PinAtSpawn {
        realization: PinnedChildProjectRealization,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedChildProjectRealization {
    ReadOnly,
    CowDiscard,
}

/// Explicit project-authority transitions over one chain's private operational
/// generation. The admitted launch capsule remains immutable; selecting a
/// child's sealed pinned generation is distinct from advancing a running COW
/// workspace, and checkpoint/continuation advances are restricted to COW.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalProjectAuthorityTransition<'a> {
    InheritContinuation,
    SelectPinnedChildGeneration { snapshot_hash: &'a str },
    SealPinnedCowCheckpoint { snapshot_hash: &'a str },
    AdvancePinnedCowContinuation { result_snapshot_hash: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentNameAuthority {
    /// Permit exactly the names declared as required by the admitted launch
    /// contract. This is an explicit selector, not an empty-list wildcard.
    DeclaredRequired,
    /// Permit only this canonical, sorted set of environment names.
    Exact { names: Vec<String> },
}

impl EnvironmentNameAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::DeclaredRequired => Ok(()),
            Self::Exact { names } => validate_allowed_names(names),
        }
    }

    pub fn permits_declared(&self, name: &str) -> bool {
        match self {
            Self::DeclaredRequired => true,
            Self::Exact { names } => names
                .binary_search_by(|candidate| candidate.as_str().cmp(name))
                .is_ok(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentAuthority {
    None,
    ProjectOverlay {
        project_authority_id: String,
        source_identity: String,
        include_operator_vault: bool,
        name_authority: EnvironmentNameAuthority,
    },
    Vault {
        namespace: String,
        name_authority: EnvironmentNameAuthority,
    },
    Delegated {
        provider: String,
        grant_id: String,
        name_authority: EnvironmentNameAuthority,
    },
}

impl EnvironmentAuthority {
    pub fn validate(&self, project_backed: bool) -> anyhow::Result<()> {
        match self {
            Self::None => Ok(()),
            Self::ProjectOverlay {
                project_authority_id,
                source_identity,
                include_operator_vault: _,
                name_authority,
            } => {
                if !project_backed {
                    anyhow::bail!("project environment overlay requires project authority");
                }
                validate_trimmed_control_free(
                    "environment project authority id",
                    project_authority_id,
                    false,
                )?;
                validate_trimmed_control_free(
                    "environment source identity",
                    source_identity,
                    false,
                )?;
                name_authority.validate()
            }
            Self::Vault {
                namespace,
                name_authority,
            } => {
                validate_trimmed_control_free("vault namespace", namespace, false)?;
                name_authority.validate()
            }
            Self::Delegated {
                provider,
                grant_id,
                name_authority,
            } => {
                validate_trimmed_control_free("delegated environment provider", provider, false)?;
                validate_trimmed_control_free("delegated environment grant", grant_id, false)?;
                name_authority.validate()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionProjectAuthority {
    Projectless {
        environment: EnvironmentAuthority,
    },
    LiveProject {
        authority_id: String,
        authored_project_identity: String,
        canonical_root: PathBuf,
        live_access: LiveAccessAuthority,
        environment: EnvironmentAuthority,
        capability_ceiling: Vec<String>,
        child_policy: ChildProjectAuthorityPolicy,
    },
    PinnedGeneration {
        stable_project_identity: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_path: Option<PathBuf>,
        snapshot_hash: String,
        realization: PinnedProjectRealization,
        environment: EnvironmentAuthority,
        capability_ceiling: Vec<String>,
        child_policy: ChildProjectAuthorityPolicy,
    },
}

impl ExecutionProjectAuthority {
    pub const PROJECTLESS: Self = Self::Projectless {
        environment: EnvironmentAuthority::None,
    };

    pub fn projectless(environment: EnvironmentAuthority) -> anyhow::Result<Self> {
        let authority = Self::Projectless { environment };
        authority.validate()?;
        Ok(authority)
    }

    pub fn with_capability_ceiling(
        mut self,
        mut capability_ceiling: Vec<String>,
    ) -> anyhow::Result<Self> {
        capability_ceiling.sort();
        capability_ceiling.dedup();
        match &mut self {
            Self::Projectless { .. } => {
                if !capability_ceiling.is_empty() {
                    anyhow::bail!(
                        "projectless authority cannot carry a project capability ceiling"
                    );
                }
            }
            Self::LiveProject {
                capability_ceiling: slot,
                ..
            }
            | Self::PinnedGeneration {
                capability_ceiling: slot,
                ..
            } => *slot = capability_ceiling,
        }
        self.validate()?;
        Ok(self)
    }

    pub fn for_child(mut self) -> anyhow::Result<Self> {
        if let Self::PinnedGeneration { realization, .. } = &mut self {
            if matches!(realization, PinnedProjectRealization::Cow { .. }) {
                *realization = PinnedProjectRealization::Cow {
                    terminal_publication: PinnedTerminalPublication::Discard,
                };
            }
        }
        self.validate()?;
        Ok(self)
    }

    /// Rebind a pinned authority to a newly sealed operational generation.
    /// Realization, environment, capability ceiling, and child policy remain
    /// unchanged; callers cannot use this to turn live/projectless authority
    /// into pinned authority implicitly.
    fn with_pinned_snapshot_hash(mut self, snapshot_hash: String) -> anyhow::Result<Self> {
        validate_hash("project snapshot hash", &snapshot_hash)?;
        match &mut self {
            Self::PinnedGeneration {
                snapshot_hash: slot,
                ..
            } => *slot = snapshot_hash,
            Self::Projectless { .. } | Self::LiveProject { .. } => {
                anyhow::bail!("only pinned project authority can advance an operational generation")
            }
        }
        self.validate()?;
        Ok(self)
    }

    pub fn transition_operational_generation(
        &self,
        transition: OperationalProjectAuthorityTransition<'_>,
    ) -> anyhow::Result<Self> {
        match transition {
            OperationalProjectAuthorityTransition::InheritContinuation => {
                self.validate()?;
                Ok(self.clone())
            }
            OperationalProjectAuthorityTransition::SelectPinnedChildGeneration {
                snapshot_hash,
            } => self
                .clone()
                .with_pinned_snapshot_hash(snapshot_hash.to_string()),
            OperationalProjectAuthorityTransition::SealPinnedCowCheckpoint { snapshot_hash }
            | OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                result_snapshot_hash: snapshot_hash,
            } => {
                validate_hash("operational project snapshot hash", snapshot_hash)?;
                let Self::PinnedGeneration { realization, .. } = self else {
                    anyhow::bail!(
                        "only pinned COW project authority can advance a continuation generation"
                    );
                };
                if !matches!(realization, PinnedProjectRealization::Cow { .. }) {
                    anyhow::bail!(
                        "read-only pinned project authority cannot advance a continuation generation"
                    );
                }
                self.clone()
                    .with_pinned_snapshot_hash(snapshot_hash.to_string())
            }
        }
    }

    pub fn with_child_policy(
        mut self,
        child_policy: ChildProjectAuthorityPolicy,
    ) -> anyhow::Result<Self> {
        match &mut self {
            Self::Projectless { .. } => {
                if child_policy != ChildProjectAuthorityPolicy::Inherit {
                    anyhow::bail!("projectless execution cannot pin project state at child spawn");
                }
            }
            Self::LiveProject {
                child_policy: slot, ..
            }
            | Self::PinnedGeneration {
                child_policy: slot, ..
            } => *slot = child_policy,
        }
        self.validate()?;
        Ok(self)
    }

    pub fn child_policy(&self) -> ChildProjectAuthorityPolicy {
        match self {
            Self::Projectless { .. } => ChildProjectAuthorityPolicy::Inherit,
            Self::LiveProject { child_policy, .. }
            | Self::PinnedGeneration { child_policy, .. } => child_policy.clone(),
        }
    }

    pub fn terminal_publication(&self) -> Option<&PinnedTerminalPublication> {
        match self {
            Self::PinnedGeneration {
                realization:
                    PinnedProjectRealization::Cow {
                        terminal_publication,
                    },
                ..
            } => Some(terminal_publication),
            Self::Projectless { .. }
            | Self::LiveProject { .. }
            | Self::PinnedGeneration {
                realization: PinnedProjectRealization::ReadOnly,
                ..
            } => None,
        }
    }

    pub fn live(
        canonical_root: PathBuf,
        authored_project_identity: String,
        access: LiveProjectAccess,
        confinement: LiveFilesystemConfinement,
        mut environment: EnvironmentAuthority,
        capability_ceiling: Vec<String>,
    ) -> anyhow::Result<Self> {
        validate_absolute_normal_path("live project root", &canonical_root)?;
        validate_trimmed_control_free(
            "authored project identity",
            &authored_project_identity,
            false,
        )?;
        if !canonical_root.is_dir() {
            anyhow::bail!(
                "live project root is not a directory: {}",
                canonical_root.display()
            );
        }
        let authorized_write_namespaces = match access {
            LiveProjectAccess::ReadOnly => Vec::new(),
            LiveProjectAccess::ReadWrite => vec!["project".to_string()],
        };
        let live_access = LiveAccessAuthority {
            access,
            authorized_write_namespaces,
            confinement,
        };
        let authority_id = lillux::sha256_hex(
            format!(
                "live-project\0{}\0{}",
                authored_project_identity,
                canonical_root.display(),
            )
            .as_bytes(),
        );
        if let EnvironmentAuthority::ProjectOverlay {
            project_authority_id,
            ..
        } = &mut environment
        {
            project_authority_id.clone_from(&authority_id);
        }
        let authority = Self::LiveProject {
            authority_id,
            authored_project_identity,
            canonical_root,
            live_access,
            environment,
            capability_ceiling,
            child_policy: ChildProjectAuthorityPolicy::Inherit,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub fn pinned(
        stable_project_identity: String,
        display_path: Option<PathBuf>,
        snapshot_hash: String,
        realization: PinnedProjectRealization,
        environment: EnvironmentAuthority,
        capability_ceiling: Vec<String>,
    ) -> anyhow::Result<Self> {
        let authority = Self::PinnedGeneration {
            stable_project_identity,
            display_path,
            snapshot_hash,
            realization,
            environment,
            capability_ceiling,
            child_policy: ChildProjectAuthorityPolicy::Inherit,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Projectless { environment } => environment.validate(false),
            Self::LiveProject {
                authority_id,
                authored_project_identity,
                canonical_root,
                live_access,
                environment,
                capability_ceiling,
                ..
            } => {
                validate_hash("live project authority id", authority_id)?;
                validate_trimmed_control_free(
                    "authored project identity",
                    authored_project_identity,
                    false,
                )?;
                validate_absolute_normal_path("live project root", canonical_root)?;
                live_access.validate()?;
                let expected_authority_id = lillux::sha256_hex(
                    format!(
                        "live-project\0{}\0{}",
                        authored_project_identity,
                        canonical_root.display(),
                    )
                    .as_bytes(),
                );
                if authority_id != &expected_authority_id {
                    anyhow::bail!("live project authority id is not canonical");
                }
                if let EnvironmentAuthority::ProjectOverlay {
                    project_authority_id,
                    ..
                } = environment
                {
                    if project_authority_id != authority_id {
                        anyhow::bail!(
                            "live project environment authority is bound to another project"
                        );
                    }
                }
                environment.validate(true)?;
                validate_capability_ceiling(capability_ceiling)
            }
            Self::PinnedGeneration {
                stable_project_identity,
                display_path,
                snapshot_hash,
                environment,
                capability_ceiling,
                realization,
                ..
            } => {
                validate_trimmed_control_free(
                    "stable project identity",
                    stable_project_identity,
                    false,
                )?;
                if let Some(path) = display_path {
                    validate_absolute_normal_path("pinned project display path", path)?;
                }
                validate_hash("project snapshot hash", snapshot_hash)?;
                environment.validate(true)?;
                if let EnvironmentAuthority::ProjectOverlay {
                    project_authority_id,
                    ..
                } = environment
                {
                    let root = display_path.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "pinned project overlay requires its original live project path"
                        )
                    })?;
                    let expected = lillux::sha256_hex(
                        format!(
                            "live-project\0{}\0{}",
                            stable_project_identity,
                            root.display(),
                        )
                        .as_bytes(),
                    );
                    if project_authority_id != &expected {
                        anyhow::bail!(
                            "pinned project environment authority is not bound to its original live project"
                        );
                    }
                }
                if let PinnedProjectRealization::Cow {
                    terminal_publication:
                        PinnedTerminalPublication::AdvanceHead {
                            head_ref,
                            expected_hash,
                        },
                } = realization
                {
                    validate_trimmed_control_free("HEAD ref", head_ref, false)?;
                    validate_hash("HEAD expected hash", expected_hash)?;
                    // `expected_hash` fences the external HEAD observed at
                    // admission. `snapshot_hash` may advance through private
                    // operational generations before terminal publication.
                }
                validate_capability_ceiling(capability_ceiling)
            }
        }
    }

    pub fn project_root_projection(&self) -> Option<&Path> {
        match self {
            Self::Projectless { .. } => None,
            Self::LiveProject { canonical_root, .. } => Some(canonical_root),
            Self::PinnedGeneration { display_path, .. } => display_path.as_deref(),
        }
    }

    pub fn base_snapshot_projection(&self) -> Option<&str> {
        match self {
            Self::PinnedGeneration { snapshot_hash, .. } => Some(snapshot_hash),
            Self::Projectless { .. } | Self::LiveProject { .. } => None,
        }
    }

    pub fn environment(&self) -> &EnvironmentAuthority {
        match self {
            Self::Projectless { environment } => environment,
            Self::LiveProject { environment, .. } | Self::PinnedGeneration { environment, .. } => {
                environment
            }
        }
    }

    pub fn live_access(&self) -> Option<&LiveAccessAuthority> {
        match self {
            Self::LiveProject { live_access, .. } => Some(live_access),
            Self::Projectless { .. } | Self::PinnedGeneration { .. } => None,
        }
    }

    /// Open the local root used by an environment overlay through a
    /// descriptor and revalidate the persisted live-root fence against that
    /// same descriptor before any child file is read.
    pub fn open_environment_root(
        &self,
    ) -> anyhow::Result<Option<lillux::secure_fs::PinnedDirectory>> {
        let Some(path) = self.project_root_projection() else {
            return Ok(None);
        };
        let root = lillux::secure_fs::PinnedDirectory::open(path)?.ok_or_else(|| {
            anyhow::anyhow!("authorized project root is unavailable: {}", path.display())
        })?;
        Ok(Some(root))
    }

    pub fn requires_project_foldback(&self) -> bool {
        matches!(
            self,
            Self::PinnedGeneration {
                realization: PinnedProjectRealization::Cow { .. },
                ..
            }
        )
    }

    pub fn records_terminal_project_generation(&self) -> bool {
        matches!(
            self,
            Self::PinnedGeneration {
                realization: PinnedProjectRealization::Cow {
                    terminal_publication: PinnedTerminalPublication::RetainResult
                        | PinnedTerminalPublication::AdvanceHead { .. },
                },
                ..
            }
        )
    }
}

impl LiveAccessAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        match &self.confinement {
            LiveFilesystemConfinement::DescriptorRootedMasked {
                denied_control_paths,
                symlink_policy: LiveSymlinkPolicy::DescriptorRootedNoEscape,
            } => validate_sorted_relative_paths("denied live control path", denied_control_paths)?,
            LiveFilesystemConfinement::UnconfinedHost => {
                if self.access == LiveProjectAccess::ReadOnly {
                    anyhow::bail!(
                        "unconfined host live access cannot truthfully enforce read-only project authority"
                    );
                }
            }
        }
        let mut previous: Option<&str> = None;
        for namespace in &self.authorized_write_namespaces {
            validate_trimmed_control_free("live write namespace", namespace, false)?;
            if previous.is_some_and(|value| value >= namespace.as_str()) {
                anyhow::bail!("live write namespaces must be strictly sorted and unique");
            }
            previous = Some(namespace);
        }
        match self.access {
            LiveProjectAccess::ReadOnly if !self.authorized_write_namespaces.is_empty() => {
                anyhow::bail!("read-only live authority cannot carry write namespaces")
            }
            LiveProjectAccess::ReadWrite if self.authorized_write_namespaces.is_empty() => {
                anyhow::bail!("read-write live authority requires a write namespace")
            }
            _ => Ok(()),
        }
    }
}

fn validate_sorted_relative_paths(label: &str, paths: &[String]) -> anyhow::Result<()> {
    let mut previous: Option<&str> = None;
    for path in paths {
        validate_trimmed_control_free(label, path, false)?;
        let candidate = Path::new(path);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            anyhow::bail!("{label} must be a normalized relative path: {path}");
        }
        if previous.is_some_and(|value| value >= path.as_str()) {
            anyhow::bail!("{label}s must be strictly sorted and unique");
        }
        previous = Some(path);
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("{label} must be a 64-character hexadecimal digest");
    }
    Ok(())
}

fn validate_absolute_normal_path(label: &str, path: &Path) -> anyhow::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        anyhow::bail!(
            "{label} must be an absolute normalized path: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_allowed_names(names: &[String]) -> anyhow::Result<()> {
    let mut previous: Option<&str> = None;
    for name in names {
        validate_trimmed_control_free("environment name", name, false)?;
        if !name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        }) {
            anyhow::bail!("invalid canonical environment name: {name}");
        }
        if previous.is_some_and(|value| value >= name.as_str()) {
            anyhow::bail!("environment names must be strictly sorted and unique");
        }
        previous = Some(name);
    }
    Ok(())
}

fn validate_capability_ceiling(capabilities: &[String]) -> anyhow::Result<()> {
    let mut previous: Option<&str> = None;
    for capability in capabilities {
        validate_trimmed_control_free("capability ceiling entry", capability, false)?;
        if previous.is_some_and(|value| value >= capability.as_str()) {
            anyhow::bail!("capability ceiling must be strictly sorted and unique");
        }
        previous = Some(capability);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_access_schema_requires_explicit_confinement() {
        let missing = serde_json::json!({
            "access": "read_write",
            "authorized_write_namespaces": ["project"]
        });
        let error = serde_json::from_value::<LiveAccessAuthority>(missing).unwrap_err();
        assert!(error.to_string().contains("confinement"));
    }

    #[test]
    fn live_access_schema_preserves_exact_confinement() {
        let unconfined = LiveAccessAuthority {
            access: LiveProjectAccess::ReadWrite,
            authorized_write_namespaces: vec!["project".to_string()],
            confinement: LiveFilesystemConfinement::UnconfinedHost,
        };
        let encoded = serde_json::to_value(&unconfined).unwrap();
        assert_eq!(encoded["confinement"]["kind"], "unconfined_host");
        assert_eq!(
            serde_json::from_value::<LiveAccessAuthority>(encoded).unwrap(),
            unconfined
        );

        let confined = LiveAccessAuthority {
            access: LiveProjectAccess::ReadOnly,
            authorized_write_namespaces: Vec::new(),
            confinement: LiveFilesystemConfinement::DescriptorRootedMasked {
                denied_control_paths: vec![".ai".to_string()],
                symlink_policy: LiveSymlinkPolicy::DescriptorRootedNoEscape,
            },
        };
        let encoded = serde_json::to_value(&confined).unwrap();
        assert_eq!(encoded["confinement"]["kind"], "descriptor_rooted_masked");
        assert_eq!(
            serde_json::from_value::<LiveAccessAuthority>(encoded).unwrap(),
            confined
        );

        let false_read_only = LiveAccessAuthority {
            access: LiveProjectAccess::ReadOnly,
            authorized_write_namespaces: Vec::new(),
            confinement: LiveFilesystemConfinement::UnconfinedHost,
        };
        assert!(false_read_only.validate().is_err());
    }
}
