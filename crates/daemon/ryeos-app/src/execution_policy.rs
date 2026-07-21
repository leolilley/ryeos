use anyhow::Context as _;
use serde::{Deserialize, Serialize};

pub const EXECUTION_POLICY_SCHEMA_VERSION: u32 = 2;

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
    /// Permit exactly the environment names declared as required by the
    /// admitted launch contract.
    DeclaredRequired,
    /// Permit only this canonical, sorted set of environment names.
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
                name_policy: ExecutionEnvironmentNamePolicy::DeclaredRequired,
            },
            project: ProjectExecutionPolicy::LiveDirect {
                access: LiveAccess::ReadWrite,
                child_policy: ChildProjectPolicy::Inherit,
            },
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
            crate::identity::validate_canonical_site_id(site_id)
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
        }
        Ok(())
    }

    /// Compile the live-project leg of an execution policy into the exact
    /// authority consumed by provenance. This is the sole constructor used by
    /// daemon entry points; they may not manufacture a read/write or environment
    /// profile inside `ExecutionProvenance`.
    pub fn resolve_live_project_authority(
        &self,
        project_path: &std::path::Path,
        confinement: ryeos_state::objects::LiveFilesystemConfinement,
        capability_ceiling: Vec<String>,
    ) -> anyhow::Result<ryeos_state::objects::ExecutionProjectAuthority> {
        self.validate()?;
        let ProjectExecutionPolicy::LiveDirect {
            access,
            child_policy,
        } = &self.project
        else {
            anyhow::bail!("live project authority requires a live-direct execution policy");
        };
        let root = project_path.canonicalize().with_context(|| {
            format!(
                "canonicalize live execution project {}",
                project_path.display()
            )
        })?;
        let name_authority = |policy: &ExecutionEnvironmentNamePolicy| match policy {
            ExecutionEnvironmentNamePolicy::DeclaredRequired => {
                ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired
            }
            ExecutionEnvironmentNamePolicy::Exact { names } => {
                ryeos_state::objects::EnvironmentNameAuthority::Exact {
                    names: names.clone(),
                }
            }
        };
        let environment = match &self.environment {
            ExecutionEnvironmentPolicy::None => ryeos_state::objects::EnvironmentAuthority::None,
            ExecutionEnvironmentPolicy::ProjectOverlay {
                include_operator_vault,
                name_policy,
            } => ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id: "pending".to_string(),
                source_identity: format!("dotenv:{}", root.join(".env").display()),
                include_operator_vault: *include_operator_vault,
                name_authority: name_authority(name_policy),
            },
            ExecutionEnvironmentPolicy::Vault {
                namespace,
                name_policy,
            } => ryeos_state::objects::EnvironmentAuthority::Vault {
                namespace: namespace.clone(),
                name_authority: name_authority(name_policy),
            },
            ExecutionEnvironmentPolicy::Delegated {
                provider,
                grant_id,
                name_policy,
            } => ryeos_state::objects::EnvironmentAuthority::Delegated {
                provider: provider.clone(),
                grant_id: grant_id.clone(),
                name_authority: name_authority(name_policy),
            },
        };
        let child_policy = match child_policy {
            ChildProjectPolicy::Inherit => {
                ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit
            }
            ChildProjectPolicy::PinAtSpawn { realization } => {
                ryeos_state::objects::ChildProjectAuthorityPolicy::PinAtSpawn {
                    realization: match realization {
                        PinnedChildRealization::ReadOnly => {
                            ryeos_state::objects::PinnedChildProjectRealization::ReadOnly
                        }
                        PinnedChildRealization::CowDiscard => {
                            ryeos_state::objects::PinnedChildProjectRealization::CowDiscard
                        }
                    },
                }
            }
        };
        ryeos_state::objects::ExecutionProjectAuthority::live(
            root.clone(),
            format!("local:{}", root.display()),
            match access {
                LiveAccess::ReadOnly => ryeos_state::objects::LiveProjectAccess::ReadOnly,
                LiveAccess::ReadWrite => ryeos_state::objects::LiveProjectAccess::ReadWrite,
            },
            confinement,
            environment,
            capability_ceiling,
        )?
        .with_child_policy(child_policy)
    }
}

/// The inseparable authority contract produced by the standard local-live
/// policy profile. Keeping these values together prevents an operational
/// entry point from resolving project authority under one policy while
/// independently claiming different lifecycle semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStandardLocalLiveAuthority {
    pub project: ryeos_state::objects::ExecutionProjectAuthority,
    pub lifecycle: ryeos_state::objects::ExecutionLifecycleAuthority,
}

/// Compile the standard local-live profile through the same closed policy
/// resolver used by explicit execution requests. Operational entry points use
/// this when they intentionally select that profile; provenance itself has no
/// defaults.
pub fn resolve_standard_local_live_authority(
    project_path: &std::path::Path,
    capability_ceiling: Vec<String>,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
) -> anyhow::Result<ResolvedStandardLocalLiveAuthority> {
    authorize_standard_local_live_execution(&capability_ceiling)?;
    let policy = ExecutionPolicy::local_live(ExecutionResponse::Wait);
    policy.validate()?;
    Ok(ResolvedStandardLocalLiveAuthority {
        project: policy.resolve_live_project_authority(
            project_path,
            live_filesystem_confinement_for_isolation(isolation.mode()),
            capability_ceiling,
        )?,
        lifecycle: policy.lifecycle_authority(),
    })
}

/// Resolve the live-project authority for a foreground offline CLI process.
///
/// This boundary is deliberately request-scoped and non-recoverable: no daemon
/// owns the child after the command exits. The local operator invocation keeps
/// the historical mutable-project contract explicit through `project.write`,
/// while filesystem confinement still follows the installed isolation mode.
pub fn resolve_offline_local_live_project_authority(
    project_path: &std::path::Path,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
) -> anyhow::Result<ryeos_state::objects::ExecutionProjectAuthority> {
    let policy = ExecutionPolicy {
        schema_version: EXECUTION_POLICY_SCHEMA_VERSION,
        ownership: ExecutionOwnership::RequestScoped,
        recovery: ExecutionRecovery::None,
        response: ExecutionResponse::Wait,
        target: ExecutionTarget::Here,
        environment: ExecutionEnvironmentPolicy::None,
        project: ProjectExecutionPolicy::LiveDirect {
            access: LiveAccess::ReadWrite,
            child_policy: ChildProjectPolicy::Inherit,
        },
    };
    policy.validate()?;
    policy.resolve_live_project_authority(
        project_path,
        live_filesystem_confinement_for_isolation(isolation.mode()),
        vec!["project.write".to_string()],
    )
}

pub fn live_filesystem_confinement_for_isolation(
    mode: ryeos_engine::isolation::IsolationMode,
) -> ryeos_state::objects::LiveFilesystemConfinement {
    match mode {
        ryeos_engine::isolation::IsolationMode::Enforce => {
            ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted()
        }
        ryeos_engine::isolation::IsolationMode::Disabled => {
            ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost
        }
    }
}

/// Authorize the standard read-write live profile before any project capture,
/// checkout, or other filesystem/CAS work begins.
pub fn authorize_standard_local_live_execution(capabilities: &[String]) -> anyhow::Result<()> {
    ryeos_runtime::authorizer::Authorizer::new()
        .authorize(
            capabilities,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require("project.write"),
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "standard local-live execution requires explicit project.write authority"
            )
        })
}

/// Build a structurally valid live authority for unit tests whose filesystem
/// path is intentionally synthetic. Production code must always use
/// `resolve_live_project_authority`, which canonicalizes and proves the root.
#[cfg(test)]
pub(crate) fn synthetic_test_live_project_authority(
    project_path: &std::path::Path,
) -> ryeos_state::objects::ExecutionProjectAuthority {
    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, EnvironmentNameAuthority,
        ExecutionProjectAuthority, LiveAccessAuthority, LiveFilesystemConfinement,
        LiveProjectAccess,
    };

    let authored_project_identity = format!("test:{}", project_path.display());
    let authority_id = lillux::sha256_hex(
        format!(
            "live-project\0{}\0{}",
            authored_project_identity,
            project_path.display()
        )
        .as_bytes(),
    );
    let authority = ExecutionProjectAuthority::LiveProject {
        authority_id: authority_id.clone(),
        authored_project_identity,
        canonical_root: project_path.to_path_buf(),
        live_access: LiveAccessAuthority {
            access: LiveProjectAccess::ReadWrite,
            authorized_write_namespaces: vec!["project".to_string()],
            confinement: LiveFilesystemConfinement::standard_descriptor_rooted(),
        },
        environment: EnvironmentAuthority::ProjectOverlay {
            project_authority_id: authority_id,
            source_identity: format!("dotenv:{}", project_path.join(".env").display()),
            include_operator_vault: true,
            name_authority: EnvironmentNameAuthority::DeclaredRequired,
        },
        capability_ceiling: Vec::new(),
        child_policy: ChildProjectAuthorityPolicy::Inherit,
    };
    authority
        .validate()
        .expect("synthetic test live authority must be valid");
    authority
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
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("{label} must be a 64-character hexadecimal digest");
    }
    Ok(())
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn live_async_is_daemon_owned_and_restart_recoverable() {
        let policy = ExecutionPolicy::local_live(ExecutionResponse::Accepted);
        policy.validate().unwrap();
        assert_eq!(policy.ownership, ExecutionOwnership::DaemonOwned);
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
        assert_eq!(policy.response, ExecutionResponse::Accepted);
    }

    #[test]
    fn live_wait_is_daemon_owned_and_restart_recoverable() {
        let policy = ExecutionPolicy::local_live(ExecutionResponse::Wait);
        policy.validate().unwrap();
        assert_eq!(policy.ownership, ExecutionOwnership::DaemonOwned);
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
        assert_eq!(policy.response, ExecutionResponse::Wait);
    }

    #[test]
    fn projectless_execution_can_be_restart_recoverable() {
        let policy = ExecutionPolicy::projectless(ExecutionResponse::Accepted);
        policy.validate().unwrap();
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
    }

    #[test]
    fn standard_live_authority_requires_project_write_and_resolves_both_halves() {
        let project = tempfile::tempdir().unwrap();
        let error = resolve_standard_local_live_authority(
            project.path(),
            vec!["project.read".to_string()],
            &ryeos_engine::isolation::IsolationRuntime::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("project.write"));

        let authority = resolve_standard_local_live_authority(
            project.path(),
            vec!["*".to_string()],
            &ryeos_engine::isolation::IsolationRuntime::default(),
        )
        .unwrap();
        assert!(matches!(
            authority.project,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
                live_access: ryeos_state::objects::LiveAccessAuthority {
                    confinement: ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            authority.lifecycle,
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE
        );

        assert!(matches!(
            live_filesystem_confinement_for_isolation(
                ryeos_engine::isolation::IsolationMode::Enforce
            ),
            ryeos_state::objects::LiveFilesystemConfinement::DescriptorRootedMasked { .. }
        ));
    }

    #[test]
    fn offline_live_authority_is_explicitly_request_scoped_project_write() {
        let project = tempfile::tempdir().unwrap();
        let authority = resolve_offline_local_live_project_authority(
            project.path(),
            &ryeos_engine::isolation::IsolationRuntime::default(),
        )
        .unwrap();

        assert!(matches!(
            authority,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
                live_access: ryeos_state::objects::LiveAccessAuthority {
                    access: ryeos_state::objects::LiveProjectAccess::ReadWrite,
                    authorized_write_namespaces,
                    confinement: ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost,
                },
                environment: ryeos_state::objects::EnvironmentAuthority::None,
                capability_ceiling,
                ..
            } if authorized_write_namespaces == vec!["project".to_string()]
                && capability_ceiling == vec!["project.write".to_string()]
        ));
    }
}
