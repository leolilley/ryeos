use anyhow::Context as _;

pub use ryeos_engine::execution_contract::*;

/// Resolve the portable execution-policy contract into daemon-local live
/// project authority. The wire policy itself lives in ryeos-engine so signed
/// schedules and every other entry point share one strict type.
pub trait ExecutionPolicyResolution {
    fn resolve_live_project_authority(
        &self,
        project_path: &std::path::Path,
        confinement: ryeos_state::objects::LiveFilesystemConfinement,
        capability_ceiling: Vec<String>,
    ) -> anyhow::Result<ryeos_state::objects::ExecutionProjectAuthority>;
}

impl ExecutionPolicyResolution for ExecutionPolicy {
    /// Compile the live-project leg of an execution policy into the exact
    /// authority consumed by provenance. This is the sole constructor used by
    /// daemon entry points; they may not manufacture a read/write or environment
    /// profile inside `ExecutionProvenance`.
    fn resolve_live_project_authority(
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
                        PinnedChildRealization::CowRetainResult => {
                            ryeos_state::objects::PinnedChildProjectRealization::CowRetainResult
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
            live_filesystem_confinement_for_isolation(isolation.inspection()),
            capability_ceiling,
        )?,
        lifecycle: policy.lifecycle_authority(),
    })
}

/// Resolve the live-project authority for a foreground offline CLI process.
///
/// This boundary is deliberately request-scoped and non-recoverable: no daemon
/// owns the child after the command exits. The local operator invocation keeps
/// the mutable-project contract explicit through its canonical capability,
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
        live_filesystem_confinement_for_isolation(isolation.inspection()),
        vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
    )
}

pub fn live_filesystem_confinement_for_isolation(
    isolation: &ryeos_engine::isolation::IsolationInspection,
) -> ryeos_state::objects::LiveFilesystemConfinement {
    match isolation.mode {
        ryeos_engine::isolation::IsolationMode::Enforce => {
            match isolation.filesystem.live_project {
                ryeos_engine::isolation::IsolationLiveProjectPolicy::FixedParents { .. } => {
                    ryeos_state::objects::LiveFilesystemConfinement::standard_fixed_parents()
                }
            }
        }
        ryeos_engine::isolation::IsolationMode::Disabled => {
            ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost
        }
    }
}

/// Authorize the standard read-write live profile before any project capture,
/// checkout, or other filesystem/CAS work begins.
pub fn authorize_standard_local_live_execution(capabilities: &[String]) -> anyhow::Result<()> {
    authorize_live_project_access(
        &ryeos_runtime::authorizer::Authorizer::new(),
        capabilities,
        LiveAccess::ReadWrite,
    )
}

/// Authorize one exact live-project access mode through the shared capability
/// evaluator. Write authority is intentionally not treated as an alias for the
/// distinct read capability: callers must request and hold the authority for
/// the operation they are performing.
pub fn authorize_live_project_access(
    authorizer: &ryeos_runtime::authorizer::Authorizer,
    capabilities: &[String],
    access: LiveAccess,
) -> anyhow::Result<()> {
    let required = access.required_capability();
    authorizer
        .authorize(
            capabilities,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(required),
        )
        .map_err(|_| {
            anyhow::anyhow!("live project {access:?} access requires explicit {required} authority")
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
            confinement: LiveFilesystemConfinement::standard_fixed_parents(),
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
    fn local_pinned_capture_is_daemon_owned_cow_with_retained_result() {
        let policy = ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait);
        policy.validate().unwrap();
        assert_eq!(policy.ownership, ExecutionOwnership::DaemonOwned);
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
        assert!(matches!(
            policy.project,
            ProjectExecutionPolicy::Pinned {
                source: PinnedSource::CaptureLive {
                    scope: ProjectCaptureScope::FullProject,
                },
                realization: PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainResult,
                },
                child_policy: ChildProjectPolicy::Inherit,
            }
        ));
    }

    #[test]
    fn local_pinned_current_head_is_daemon_owned_cow_with_retained_current_head() {
        let policy = ExecutionPolicy::local_pinned_current_head(ExecutionResponse::Accepted);
        assert_eq!(policy.response, ExecutionResponse::Accepted);
        assert_eq!(policy.ownership, ExecutionOwnership::DaemonOwned);
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
        assert!(matches!(
            policy.project,
            ProjectExecutionPolicy::Pinned {
                source: PinnedSource::CurrentHead,
                realization: PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainCurrentHead,
                },
                child_policy: ChildProjectPolicy::Inherit,
            }
        ));
        policy.validate().unwrap();
    }

    #[test]
    fn projectless_execution_can_be_restart_recoverable() {
        let policy = ExecutionPolicy::projectless(ExecutionResponse::Accepted);
        policy.validate().unwrap();
        assert_eq!(policy.recovery, ExecutionRecovery::RestartRecoverable);
    }

    #[test]
    fn live_access_capabilities_are_canonical_and_distinct() {
        let read = LiveAccess::ReadOnly.required_capability();
        let write = LiveAccess::ReadWrite.required_capability();
        assert_ne!(read, write);
        assert!(ryeos_runtime::authorizer::Capability::parse(read).is_ok());
        assert!(ryeos_runtime::authorizer::Capability::parse(write).is_ok());
    }

    #[test]
    fn live_access_authorization_is_exact_and_wildcard_aware() {
        let authorizer = ryeos_runtime::authorizer::Authorizer::new();
        for (access, capability) in [
            (LiveAccess::ReadOnly, LIVE_PROJECT_READ_CAPABILITY),
            (LiveAccess::ReadWrite, LIVE_PROJECT_WRITE_CAPABILITY),
        ] {
            authorize_live_project_access(&authorizer, &[capability.to_string()], access).unwrap();
            authorize_live_project_access(&authorizer, &["*".to_string()], access).unwrap();
        }

        assert!(
            authorize_live_project_access(
                &authorizer,
                &[LIVE_PROJECT_READ_CAPABILITY.to_string()],
                LiveAccess::ReadWrite,
            )
            .is_err()
        );
        assert!(
            authorize_live_project_access(
                &authorizer,
                &[LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
                LiveAccess::ReadOnly,
            )
            .is_err()
        );
        assert!(
            authorize_live_project_access(
                &authorizer,
                &["project.write".to_string()],
                LiveAccess::ReadWrite,
            )
            .is_err()
        );
    }

    #[test]
    fn standard_live_authority_requires_canonical_project_write_and_resolves_both_halves() {
        let project = tempfile::tempdir().unwrap();
        for insufficient in ["project.write", LIVE_PROJECT_READ_CAPABILITY] {
            let error = resolve_standard_local_live_authority(
                project.path(),
                vec![insufficient.to_string()],
                &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
            )
            .unwrap_err();
            assert!(error.to_string().contains(LIVE_PROJECT_WRITE_CAPABILITY));
        }

        let authority = resolve_standard_local_live_authority(
            project.path(),
            vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
            &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
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

        resolve_standard_local_live_authority(
            project.path(),
            vec!["*".to_string()],
            &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
        )
        .expect("node-local wildcard authority remains valid");
        assert_eq!(
            authority.lifecycle,
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE
        );

        let mut inspection = ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring()
            .inspection()
            .clone();
        inspection.mode = ryeos_engine::isolation::IsolationMode::Enforce;
        assert!(matches!(
            live_filesystem_confinement_for_isolation(&inspection),
            ryeos_state::objects::LiveFilesystemConfinement::DescriptorRootedFixedParents { .. }
        ));
    }

    #[test]
    fn offline_live_authority_is_explicitly_request_scoped_canonical_project_write() {
        let project = tempfile::tempdir().unwrap();
        let authority = resolve_offline_local_live_project_authority(
            project.path(),
            &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
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
                && capability_ceiling == vec![LIVE_PROJECT_WRITE_CAPABILITY.to_string()]
        ));
    }
}
