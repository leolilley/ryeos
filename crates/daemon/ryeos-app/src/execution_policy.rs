use serde::{Deserialize, Serialize};

pub const EXECUTION_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOwnership {
    RequestScoped,
    DaemonOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRecovery {
    None,
    RestartRecoverable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionResponse {
    Wait,
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTarget {
    Here,
    Site { site_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionEnvironmentPolicy {
    None,
    ProjectOverlay {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCaptureScope {
    FullProject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PinnedSource {
    CurrentHead,
    Snapshot { hash: String },
    CaptureLive { scope: ProjectCaptureScope },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalPublication {
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
pub enum PinnedRealization {
    ReadOnly,
    Cow {
        terminal_publication: TerminalPublication,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildProjectPolicy {
    Inherit,
    PinAtSpawn { realization: PinnedChildRealization },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinnedChildRealization {
    ReadOnly,
    CowDiscard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectExecutionPolicy {
    Projectless,
    LiveDirect {
        access: LiveAccess,
        child_policy: ChildProjectPolicy,
    },
    Pinned {
        source: PinnedSource,
        realization: PinnedRealization,
        child_policy: ChildProjectPolicy,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub schema_version: u32,
    pub ownership: ExecutionOwnership,
    pub recovery: ExecutionRecovery,
    pub response: ExecutionResponse,
    pub target: ExecutionTarget,
    pub environment: ExecutionEnvironmentPolicy,
    pub project: ProjectExecutionPolicy,
}

impl ExecutionPolicy {
    pub fn lifecycle_authority(&self) -> ryeos_state::objects::ExecutionLifecycleAuthority {
        ryeos_state::objects::ExecutionLifecycleAuthority {
            ownership: match self.ownership {
                ExecutionOwnership::RequestScoped => {
                    ryeos_state::objects::ExecutionOwnershipAuthority::RequestScoped
                }
                ExecutionOwnership::DaemonOwned => {
                    ryeos_state::objects::ExecutionOwnershipAuthority::DaemonOwned
                }
            },
            recovery: match self.recovery {
                ExecutionRecovery::None => ryeos_state::objects::ExecutionRecoveryAuthority::None,
                ExecutionRecovery::RestartRecoverable => {
                    ryeos_state::objects::ExecutionRecoveryAuthority::RestartRecoverable
                }
            },
        }
    }

    pub fn local_live(response: ExecutionResponse) -> Self {
        Self {
            schema_version: EXECUTION_POLICY_SCHEMA_VERSION,
            ownership: ExecutionOwnership::DaemonOwned,
            recovery: ExecutionRecovery::RestartRecoverable,
            response,
            target: ExecutionTarget::Here,
            environment: ExecutionEnvironmentPolicy::ProjectOverlay {
                include_operator_vault: true,
                allowed_names: Vec::new(),
            },
            project: ProjectExecutionPolicy::LiveDirect {
                access: LiveAccess::ReadWrite,
                child_policy: ChildProjectPolicy::Inherit,
            },
        }
    }

    pub fn projectless(response: ExecutionResponse) -> Self {
        Self {
            environment: ExecutionEnvironmentPolicy::None,
            project: ProjectExecutionPolicy::Projectless,
            ..Self::local_live(response)
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != EXECUTION_POLICY_SCHEMA_VERSION {
            anyhow::bail!(
                "execution policy schema_version must be exactly {} (got {})",
                EXECUTION_POLICY_SCHEMA_VERSION,
                self.schema_version
            );
        }
        self.lifecycle_authority().validate()?;
        if self.ownership == ExecutionOwnership::RequestScoped {
            if self.recovery == ExecutionRecovery::RestartRecoverable {
                anyhow::bail!("request-scoped execution cannot be restart-recoverable");
            }
            if self.response == ExecutionResponse::Accepted {
                anyhow::bail!("request-scoped execution cannot return an accepted response");
            }
        }
        if let ExecutionTarget::Site { site_id } = &self.target {
            if site_id.is_empty()
                || site_id.trim() != site_id
                || site_id.chars().any(char::is_control)
            {
                anyhow::bail!("execution target site_id must be non-empty and canonical");
            }
            if matches!(self.project, ProjectExecutionPolicy::LiveDirect { .. }) {
                anyhow::bail!(
                    "remote execution requires pinned portable project authority; request explicit pin-at-admission"
                );
            }
            if matches!(
                self.environment,
                ExecutionEnvironmentPolicy::ProjectOverlay { .. }
            ) {
                anyhow::bail!(
                    "remote execution cannot carry a node-local project environment overlay; select an explicit destination vault or delegated authority"
                );
            }
        }
        validate_environment_policy(
            &self.environment,
            !matches!(self.project, ProjectExecutionPolicy::Projectless),
        )?;
        if let ProjectExecutionPolicy::Pinned {
            source,
            realization,
            ..
        } = &self.project
        {
            if let PinnedSource::Snapshot { hash } = source {
                validate_hash("execution snapshot hash", hash)?;
            }
            if let PinnedRealization::Cow {
                terminal_publication:
                    TerminalPublication::AdvanceHead {
                        head_ref,
                        expected_hash,
                        publication_grant,
                    },
            } = realization
            {
                if head_ref.is_empty() || publication_grant.is_empty() {
                    anyhow::bail!("advance-head publication requires ref and grant authority");
                }
                validate_hash("advance-head expected hash", expected_hash)?;
            }
        }
        Ok(())
    }
}

fn validate_environment_policy(
    environment: &ExecutionEnvironmentPolicy,
    project_backed: bool,
) -> anyhow::Result<()> {
    let validate_names = |names: &[String]| -> anyhow::Result<()> {
        let mut previous: Option<&str> = None;
        for name in names {
            if name.is_empty()
                || name.trim() != name
                || name.chars().any(char::is_control)
                || !name.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_uppercase()
                        || (index > 0 && byte.is_ascii_digit())
                })
            {
                anyhow::bail!("environment allowed name is not canonical: {name:?}");
            }
            if previous.is_some_and(|value| value >= name.as_str()) {
                anyhow::bail!("environment allowed names must be sorted and unique");
            }
            previous = Some(name);
        }
        Ok(())
    };
    match environment {
        ExecutionEnvironmentPolicy::None => Ok(()),
        ExecutionEnvironmentPolicy::ProjectOverlay { allowed_names, .. } => {
            if !project_backed {
                anyhow::bail!("project environment overlay requires project authority");
            }
            validate_names(allowed_names)
        }
        ExecutionEnvironmentPolicy::Vault {
            namespace,
            allowed_names,
        } => {
            validate_identity("vault namespace", namespace)?;
            if namespace != "operator" {
                anyhow::bail!(
                    "vault namespace {namespace:?} is not installed on this node; only `operator` is available"
                );
            }
            validate_names(allowed_names)
        }
        ExecutionEnvironmentPolicy::Delegated {
            provider,
            grant_id,
            allowed_names,
        } => {
            validate_identity("delegated environment provider", provider)?;
            validate_identity("delegated environment grant", grant_id)?;
            validate_names(allowed_names)?;
            anyhow::bail!(
                "delegated environment provider {provider:?} is not installed on this node"
            )
        }
    }
}

fn validate_identity(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        anyhow::bail!("{label} must be non-empty and canonical");
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("{label} must be a 64-character hexadecimal digest");
    }
    Ok(())
}
