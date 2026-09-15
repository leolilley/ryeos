//! Canonical execution-policy wire contract.
//!
//! This module owns only the portable, signed policy vocabulary and its
//! structural validation. Daemon-side resolution of that policy into local
//! filesystem, vault, isolation, and publication authority remains in
//! `ryeos-app`.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub const EXECUTION_POLICY_SCHEMA_VERSION: u32 = 2;
pub const LIVE_PROJECT_READ_CAPABILITY: &str = "ryeos.read.project.live";
pub const LIVE_PROJECT_WRITE_CAPABILITY: &str = "ryeos.write.project.live";

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
pub enum ExecutionEnvironmentNamePolicy {
    DeclaredRequired,
    Exact { names: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionEnvironmentPolicy {
    None,
    ProjectOverlay {
        include_operator_vault: bool,
        name_policy: ExecutionEnvironmentNamePolicy,
    },
    Vault {
        namespace: String,
        name_policy: ExecutionEnvironmentNamePolicy,
    },
    Delegated {
        provider: String,
        grant_id: String,
        name_policy: ExecutionEnvironmentNamePolicy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveAccess {
    ReadOnly,
    ReadWrite,
}

impl LiveAccess {
    pub const fn required_capability(self) -> &'static str {
        match self {
            Self::ReadOnly => LIVE_PROJECT_READ_CAPABILITY,
            Self::ReadWrite => LIVE_PROJECT_WRITE_CAPABILITY,
        }
    }
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
    RetainCurrentHead,
    AdvanceHead {
        head_ref: String,
        expected_hash: String,
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
    CowRetainResult,
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
                name_policy: ExecutionEnvironmentNamePolicy::DeclaredRequired,
            },
            project: ProjectExecutionPolicy::LiveDirect {
                access: LiveAccess::ReadWrite,
                child_policy: ChildProjectPolicy::Inherit,
            },
        }
    }

    pub fn local_pinned_capture(response: ExecutionResponse) -> Self {
        Self {
            project: ProjectExecutionPolicy::Pinned {
                source: PinnedSource::CaptureLive {
                    scope: ProjectCaptureScope::FullProject,
                },
                realization: PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainResult,
                },
                child_policy: ChildProjectPolicy::Inherit,
            },
            ..Self::local_live(response)
        }
    }

    pub fn local_pinned_current_head(response: ExecutionResponse) -> Self {
        Self {
            project: ProjectExecutionPolicy::Pinned {
                source: PinnedSource::CurrentHead,
                realization: PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainCurrentHead,
                },
                child_policy: ChildProjectPolicy::Inherit,
            },
            ..Self::local_live(response)
        }
    }

    pub fn projectless(response: ExecutionResponse) -> Self {
        Self {
            recovery: ExecutionRecovery::RestartRecoverable,
            environment: ExecutionEnvironmentPolicy::None,
            project: ProjectExecutionPolicy::Projectless,
            ..Self::local_live(response)
        }
    }

    pub fn retain_child_results(mut self) -> anyhow::Result<Self> {
        let child_policy = ChildProjectPolicy::PinAtSpawn {
            realization: PinnedChildRealization::CowRetainResult,
        };
        match &mut self.project {
            ProjectExecutionPolicy::LiveDirect {
                child_policy: slot, ..
            }
            | ProjectExecutionPolicy::Pinned {
                child_policy: slot, ..
            } => *slot = child_policy,
            ProjectExecutionPolicy::Projectless => {
                anyhow::bail!("retained child results require project-backed execution")
            }
        }
        self.validate()?;
        Ok(self)
    }

    pub fn exclude_operator_vault(mut self) -> Self {
        if let ExecutionEnvironmentPolicy::ProjectOverlay {
            include_operator_vault,
            ..
        } = &mut self.environment
        {
            *include_operator_vault = false;
        }
        self
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
            crate::principal_contract::validate_canonical_site_id(site_id)
                .context("execution target site_id is not canonical")?;
            if matches!(&self.project, ProjectExecutionPolicy::LiveDirect { .. }) {
                anyhow::bail!(
                    "remote execution requires pinned portable project authority; request explicit pin-at-admission"
                );
            }
            if matches!(
                &self.project,
                ProjectExecutionPolicy::Pinned {
                    realization: PinnedRealization::Cow {
                        terminal_publication: TerminalPublication::AdvanceHead { .. },
                    },
                    ..
                }
            ) {
                anyhow::bail!(
                    "remote advance-head publication is not supported in v1; use retain-result and publish under destination-scoped authority explicitly"
                );
            }
            if matches!(
                &self.environment,
                ExecutionEnvironmentPolicy::ProjectOverlay { .. }
            ) {
                anyhow::bail!(
                    "remote execution cannot carry a node-local project environment overlay; select an explicit destination vault or delegated authority"
                );
            }
        }
        validate_environment_policy(
            &self.environment,
            !matches!(&self.project, ProjectExecutionPolicy::Projectless),
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
                    },
            } = realization
            {
                if head_ref.is_empty() {
                    anyhow::bail!("advance-head publication requires a target ref");
                }
                validate_hash("advance-head expected hash", expected_hash)?;
            }
            if matches!(
                realization,
                PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainCurrentHead,
                }
            ) && !matches!(source, PinnedSource::CurrentHead)
            {
                anyhow::bail!(
                    "retain-current-head publication requires current_head pinned source"
                );
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
        ExecutionEnvironmentPolicy::ProjectOverlay { name_policy, .. } => {
            if !project_backed {
                anyhow::bail!("project environment overlay requires project authority");
            }
            validate_environment_name_policy(name_policy, &validate_names)
        }
        ExecutionEnvironmentPolicy::Vault {
            namespace,
            name_policy,
        } => {
            validate_identity("vault namespace", namespace)?;
            if namespace != "operator" {
                anyhow::bail!(
                    "vault namespace {namespace:?} is not installed on this node; only `operator` is available"
                );
            }
            validate_environment_name_policy(name_policy, &validate_names)
        }
        ExecutionEnvironmentPolicy::Delegated {
            provider,
            grant_id,
            name_policy,
        } => {
            validate_identity("delegated environment provider", provider)?;
            validate_identity("delegated environment grant", grant_id)?;
            validate_environment_name_policy(name_policy, &validate_names)?;
            anyhow::bail!(
                "delegated environment provider {provider:?} is not installed on this node"
            )
        }
    }
}

fn validate_environment_name_policy(
    policy: &ExecutionEnvironmentNamePolicy,
    validate_names: &impl Fn(&[String]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match policy {
        ExecutionEnvironmentNamePolicy::DeclaredRequired => Ok(()),
        ExecutionEnvironmentNamePolicy::Exact { names } => validate_names(names),
    }
}

fn validate_identity(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        anyhow::bail!("{label} must be non-empty and canonical");
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("{label} must be a lowercase 64-character hexadecimal digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_wire_round_trip_is_strict() {
        let policy = ExecutionPolicy::local_pinned_current_head(ExecutionResponse::Accepted)
            .retain_child_results()
            .unwrap()
            .exclude_operator_vault();
        policy.validate().unwrap();
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            serde_json::from_value::<ExecutionPolicy>(value).unwrap(),
            policy
        );
    }

    #[test]
    fn policy_rejects_unknown_fields_and_uppercase_hashes() {
        let mut value = serde_json::to_value(ExecutionPolicy::local_pinned_current_head(
            ExecutionResponse::Accepted,
        ))
        .unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExecutionPolicy>(value).is_err());

        let policy = ExecutionPolicy {
            project: ProjectExecutionPolicy::Pinned {
                source: PinnedSource::Snapshot {
                    hash: "A".repeat(64),
                },
                realization: PinnedRealization::ReadOnly,
                child_policy: ChildProjectPolicy::Inherit,
            },
            ..ExecutionPolicy::projectless(ExecutionResponse::Accepted)
        };
        assert!(policy.validate().is_err());
    }
}
