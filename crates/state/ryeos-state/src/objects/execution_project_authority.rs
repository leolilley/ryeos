use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::validate_trimmed_control_free;

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
        publication_grant: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentAuthority {
    None,
    ProjectOverlay {
        project_authority_id: String,
        source_identity: String,
        include_operator_vault: bool,
        allowed_names: Vec<String>,
    },
    Vault {
        namespace: String,
        allowed_names: Vec<String>,
    },
    Delegated {
        provider: String,
        grant_id: String,
        allowed_names: Vec<String>,
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
                allowed_names,
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
                validate_allowed_names(allowed_names)
            }
            Self::Vault {
                namespace,
                allowed_names,
            } => {
                validate_trimmed_control_free("vault namespace", namespace, false)?;
                validate_allowed_names(allowed_names)
            }
            Self::Delegated {
                provider,
                grant_id,
                allowed_names,
            } => {
                validate_trimmed_control_free("delegated environment provider", provider, false)?;
                validate_trimmed_control_free("delegated environment grant", grant_id, false)?;
                validate_allowed_names(allowed_names)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionProjectAuthority {
    Projectless,
    LiveProject {
        authority_id: String,
        authored_project_identity: String,
        canonical_root: PathBuf,
        access: LiveProjectAccess,
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
    pub fn with_capability_ceiling(
        mut self,
        mut capability_ceiling: Vec<String>,
    ) -> anyhow::Result<Self> {
        capability_ceiling.sort();
        capability_ceiling.dedup();
        match &mut self {
            Self::Projectless => {
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

    pub fn with_child_policy(
        mut self,
        child_policy: ChildProjectAuthorityPolicy,
    ) -> anyhow::Result<Self> {
        match &mut self {
            Self::Projectless => {
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
            Self::Projectless => ChildProjectAuthorityPolicy::Inherit,
            Self::LiveProject { child_policy, .. }
            | Self::PinnedGeneration { child_policy, .. } => child_policy.clone(),
        }
    }

    pub fn live(
        canonical_root: PathBuf,
        authored_project_identity: String,
        access: LiveProjectAccess,
        environment: EnvironmentAuthority,
        capability_ceiling: Vec<String>,
    ) -> anyhow::Result<Self> {
        validate_absolute_normal_path("live project root", &canonical_root)?;
        validate_trimmed_control_free(
            "authored project identity",
            &authored_project_identity,
            false,
        )?;
        let authority_id =
            lillux::sha256_hex(format!("live-project\0{}", canonical_root.display()).as_bytes());
        let authority = Self::LiveProject {
            authority_id,
            authored_project_identity,
            canonical_root,
            access,
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
            Self::Projectless => Ok(()),
            Self::LiveProject {
                authority_id,
                authored_project_identity,
                canonical_root,
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
                if let PinnedProjectRealization::Cow {
                    terminal_publication:
                        PinnedTerminalPublication::AdvanceHead {
                            head_ref,
                            expected_hash,
                            publication_grant,
                        },
                } = realization
                {
                    validate_trimmed_control_free("HEAD ref", head_ref, false)?;
                    validate_hash("HEAD expected hash", expected_hash)?;
                    validate_trimmed_control_free(
                        "HEAD publication grant",
                        publication_grant,
                        false,
                    )?;
                    if expected_hash != snapshot_hash {
                        anyhow::bail!(
                            "HEAD expected hash must equal the admitted base snapshot hash"
                        );
                    }
                }
                validate_capability_ceiling(capability_ceiling)
            }
        }
    }

    pub fn project_root_projection(&self) -> Option<&Path> {
        match self {
            Self::Projectless => None,
            Self::LiveProject { canonical_root, .. } => Some(canonical_root),
            Self::PinnedGeneration { display_path, .. } => display_path.as_deref(),
        }
    }

    pub fn base_snapshot_projection(&self) -> Option<&str> {
        match self {
            Self::PinnedGeneration { snapshot_hash, .. } => Some(snapshot_hash),
            Self::Projectless | Self::LiveProject { .. } => None,
        }
    }

    pub fn environment(&self) -> &EnvironmentAuthority {
        match self {
            Self::Projectless => &EnvironmentAuthority::None,
            Self::LiveProject { environment, .. } | Self::PinnedGeneration { environment, .. } => {
                environment
            }
        }
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
