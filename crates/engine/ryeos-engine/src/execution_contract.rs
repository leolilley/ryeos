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
/// Fixed capability required to admit any execution environment that reads
/// the node operator's vault: a project overlay with
/// `include_operator_vault: true`, or the `operator` vault namespace.
pub const OPERATOR_VAULT_CAPABILITY: &str = "ryeos.use.vault.operator";
/// The only vault namespace installed on a node.
pub const OPERATOR_VAULT_NAMESPACE: &str = "operator";

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
    /// Values the executing node took from its own environment at startup,
    /// restricted to the names its signed node policy declares. The required
    /// names stay in the item's `required_secrets`.
    ProviderEnvironment {
        name_policy: ExecutionEnvironmentNamePolicy,
    },
}

impl ExecutionEnvironmentPolicy {
    /// Whether admitting this environment would let the execution read the
    /// node operator's vault. Such a policy requires
    /// [`OPERATOR_VAULT_CAPABILITY`] from the principal it acts for.
    pub fn exposes_operator_vault(&self) -> bool {
        match self {
            Self::ProjectOverlay {
                include_operator_vault,
                ..
            } => *include_operator_vault,
            Self::Vault { namespace, .. } => namespace == OPERATOR_VAULT_NAMESPACE,
            Self::None | Self::Delegated { .. } | Self::ProviderEnvironment { .. } => false,
        }
    }
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

/// Whether a project-backed request asked for the live project or accepted
/// the daemon's choice of root. `Default` lets the daemon pick the root a tool
/// shape gets (the live project for an `@subprocess` tool without
/// `source_scope`, a sparse private admitted-input root for a sourced
/// runtime). `Live` is an explicit demand for the live project: a launch whose
/// resolved root is not live is refused with
/// `live_project_unavailable_to_sourced_runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveIntent {
    #[default]
    Default,
    Live,
}

impl From<LiveIntent> for ryeos_state::objects::LiveProjectIntent {
    fn from(intent: LiveIntent) -> Self {
        match intent {
            LiveIntent::Default => Self::Default,
            LiveIntent::Live => Self::Live,
        }
    }
}

impl LiveIntent {
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

/// Which root a process's inputs are laid out in. Chosen by the executor once
/// the tool's retained bindings and the project authority are known; the
/// explicit live-intent guard reads it, on the daemon and offline CLI paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessInputRootSelection {
    Existing,
    SparsePrivate,
    PinnedReadOnlyPrivate,
    CandidateIntegrationPrivate,
}

/// The root kind a launch attempt's process inputs were laid out in, as
/// recorded by the `process_input_root_selected` thread event. Names the kind
/// only, never a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessInputRootKind {
    /// The live project itself.
    Live,
    /// A private admitted-input root holding only the verified source.
    SparsePrivate,
    /// A pinned (read-only or copy-on-write) project generation.
    Pinned,
    /// A private root for candidate integration.
    CandidateIntegration,
    /// No project root (projectless launch).
    Projectless,
}

/// Payload of the `process_input_root_selected` event. Strict: one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInputRootSelectedPayload {
    pub root: ProcessInputRootKind,
}

/// Payload of the `launch_tool_reads` event: the tools of a run's inventory
/// that declare `reads`. Built by the daemon from descriptors it made; a
/// runtime cannot append it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchToolReadsPayload {
    pub tools: Vec<ToolReadsEntry>,
}

/// One inventory tool that declares what its calls read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolReadsEntry {
    /// Canonical ref of the tool item (`tool:ryeos/core/fetch`).
    pub item_ref: String,
    /// The call argument that names what the call reads.
    pub argument: String,
    /// The space the tool item resolved in. A project-space tool can declare
    /// `reads` without reading anything, so a condition that matters filters
    /// on `bundle` or `node`.
    pub space: crate::contracts::ItemSpace,
}

/// The most tools one launch records as read-declaring.
pub const MAX_LAUNCH_TOOL_READS: usize = 256;

/// The read-declaring tools of an inventory, or `None` when it has none. A
/// descriptor whose declaration is malformed (no `argument` string of
/// `[A-Za-z0-9_]`, at most 64 bytes, or no space) is left out, never guessed.
pub fn launch_tool_reads(
    inventory: &std::collections::HashMap<String, Vec<crate::registry::inventory::ItemDescriptor>>,
) -> Option<LaunchToolReadsPayload> {
    let mut tools: Vec<ToolReadsEntry> = inventory
        .get("tool")?
        .iter()
        .filter_map(|descriptor| {
            let argument = descriptor
                .extra
                .get("reads")?
                .get("argument")?
                .as_str()?
                .to_owned();
            let valid = !argument.is_empty()
                && argument.len() <= 64
                && argument
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if !valid {
                return None;
            }
            let space = serde_json::from_value(descriptor.extra.get("space")?.clone()).ok()?;
            Some(ToolReadsEntry {
                item_ref: descriptor.item_id.clone(),
                argument,
                space,
            })
        })
        .collect();
    tools.sort_by(|a, b| a.item_ref.cmp(&b.item_ref));
    tools.truncate(MAX_LAUNCH_TOOL_READS);
    (!tools.is_empty()).then_some(LaunchToolReadsPayload { tools })
}

/// Whether an execution's derived facts retain filesystem bindings (a source
/// closure or external-content realizations). Such a tool never runs from the
/// live project: it gets a private admitted-input root.
pub fn derived_has_filesystem_bindings(
    derived: &std::collections::HashMap<String, serde_json::Value>,
) -> anyhow::Result<bool> {
    let has_source = derived.contains_key(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY);
    // Runtime-root realizations are not project fold-back exclusions, but
    // still require exact filesystem binding before this process can launch.
    let has_external = derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .map(ryeos_state::objects::ExternalContentRealizationSet::from_value)
        .transpose()?
        .is_some_and(|realizations| !realizations.is_empty());
    Ok(has_source || has_external)
}

/// Stable refusal code: a live project authority was explicitly demanded for
/// a tool whose retained filesystem bindings force a private admitted-input
/// root.
pub const LIVE_PROJECT_UNAVAILABLE_CODE: &str = "live_project_unavailable_to_sourced_runtime";

/// Typed refusal for a launch whose explicit live project demand could never
/// reach the process.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct LiveProjectUnavailableToSourcedRuntime {
    pub message: String,
}

/// The guard fires wherever the request explicitly demanded the live project
/// and the resolved root is not the existing (live) root.
pub fn live_project_refusal_applies(root: ProcessInputRootSelection, demands_live: bool) -> bool {
    demands_live && root != ProcessInputRootSelection::Existing
}

pub fn live_project_unavailable_error(
    tool: &str,
    runtime: &str,
    has_source: bool,
    access: &str,
) -> LiveProjectUnavailableToSourcedRuntime {
    let reason = if has_source {
        "the runtime declares source_scope, so the tool runs from a private admitted-input root holding only its verified source"
    } else {
        "the tool retains external content, so it runs from a private admitted-input root"
    };
    LiveProjectUnavailableToSourcedRuntime {
        message: format!(
            "{LIVE_PROJECT_UNAVAILABLE_CODE}: `{tool}` (runtime `{runtime}`) was launched with \
             an explicit live project demand ({access}), but {reason}; the live project is never mounted \
             and project writes would be silently discarded. Use an `@subprocess` tool without \
             source_scope for live project access, or run with `--pin-project` / a pinned project \
             policy for a copy-on-write generation, or return structured results instead of writing \
             to the project."
        ),
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
    /// The acting principal's own current head of `project`. The owner is
    /// always the acting principal, never named here.
    CurrentHead {
        project: ryeos_state::project_identity::PushedProject,
    },
    Snapshot {
        hash: String,
    },
    CaptureLive {
        scope: ProjectCaptureScope,
    },
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
        /// Absent means `default`. Written only when explicit.
        #[serde(default, skip_serializing_if = "LiveIntent::is_default")]
        intent: LiveIntent,
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
                intent: LiveIntent::Default,
            },
        }
    }

    /// Demand the live project explicitly: a launch whose resolved root is
    /// not live is refused rather than run against a private copy.
    pub fn demand_live_project(mut self) -> anyhow::Result<Self> {
        match &mut self.project {
            ProjectExecutionPolicy::LiveDirect { intent, .. } => *intent = LiveIntent::Live,
            ProjectExecutionPolicy::Pinned { .. } | ProjectExecutionPolicy::Projectless => {
                anyhow::bail!("a live project demand requires a live-direct project policy")
            }
        }
        self.validate()?;
        Ok(self)
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

    pub fn local_pinned_current_head(
        response: ExecutionResponse,
        project: ryeos_state::project_identity::PushedProject,
    ) -> Self {
        Self {
            project: ProjectExecutionPolicy::Pinned {
                source: PinnedSource::CurrentHead { project },
                realization: PinnedRealization::Cow {
                    terminal_publication: TerminalPublication::RetainCurrentHead,
                },
                child_policy: ChildProjectPolicy::Inherit,
            },
            ..Self::local_live(response)
        }
    }

    /// Execute against one already-published immutable project snapshot. This
    /// is distinct from capturing a live project: callers supply the exact CAS
    /// identity and the realization is read-only for the whole execution.
    pub fn local_pinned_snapshot_read_only(
        response: ExecutionResponse,
        hash: impl Into<String>,
    ) -> Self {
        Self {
            project: ProjectExecutionPolicy::Pinned {
                source: PinnedSource::Snapshot { hash: hash.into() },
                realization: PinnedRealization::ReadOnly,
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
            ) && !matches!(source, PinnedSource::CurrentHead { .. })
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
            if namespace != OPERATOR_VAULT_NAMESPACE {
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
        ExecutionEnvironmentPolicy::ProviderEnvironment { name_policy } => {
            validate_environment_name_policy(name_policy, &validate_names)
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
        let policy = ExecutionPolicy::local_pinned_current_head(
            ExecutionResponse::Accepted,
            ryeos_state::project_identity::PushedProject::test_named("demo"),
        )
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
    fn operator_vault_exposure_is_exactly_the_overlay_flag_or_operator_namespace() {
        let declared = || ExecutionEnvironmentNamePolicy::DeclaredRequired;
        assert!(
            ExecutionPolicy::local_live(ExecutionResponse::Wait)
                .environment
                .exposes_operator_vault()
        );
        assert!(
            !ExecutionPolicy::local_live(ExecutionResponse::Wait)
                .exclude_operator_vault()
                .environment
                .exposes_operator_vault()
        );
        assert!(
            ExecutionEnvironmentPolicy::Vault {
                namespace: OPERATOR_VAULT_NAMESPACE.to_owned(),
                name_policy: declared(),
            }
            .exposes_operator_vault()
        );
        for environment in [
            ExecutionEnvironmentPolicy::None,
            ExecutionEnvironmentPolicy::ProviderEnvironment {
                name_policy: declared(),
            },
            ExecutionEnvironmentPolicy::Delegated {
                provider: "provider".to_owned(),
                grant_id: "grant".to_owned(),
                name_policy: declared(),
            },
        ] {
            assert!(!environment.exposes_operator_vault(), "{environment:?}");
        }
        assert!(
            !ExecutionPolicy::projectless(ExecutionResponse::Wait)
                .environment
                .exposes_operator_vault()
        );
    }

    #[test]
    fn provider_environment_policy_is_project_independent_and_canonical() {
        let mut policy = ExecutionPolicy::local_pinned_current_head(
            ExecutionResponse::Accepted,
            ryeos_state::project_identity::PushedProject::test_named("demo"),
        );
        policy.environment = ExecutionEnvironmentPolicy::ProviderEnvironment {
            name_policy: ExecutionEnvironmentNamePolicy::Exact {
                names: vec!["OPENAI_API_KEY".to_owned()],
            },
        };
        policy.validate().unwrap();
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(value["environment"]["kind"], "provider_environment");
        assert_eq!(
            serde_json::from_value::<ExecutionPolicy>(value).unwrap(),
            policy
        );
        policy.environment = ExecutionEnvironmentPolicy::ProviderEnvironment {
            name_policy: ExecutionEnvironmentNamePolicy::Exact {
                names: vec!["Z_KEY".to_owned(), "A_KEY".to_owned()],
            },
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn policy_rejects_unknown_fields_and_uppercase_hashes() {
        let mut value = serde_json::to_value(ExecutionPolicy::local_pinned_current_head(
            ExecutionResponse::Accepted,
            ryeos_state::project_identity::PushedProject::test_named("demo"),
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

    #[test]
    fn launch_tool_reads_keeps_only_well_formed_declarations_with_their_space() {
        use crate::registry::inventory::ItemDescriptor;
        let descriptor = |item_id: &str, extra: serde_json::Value| ItemDescriptor {
            name: item_id.to_owned(),
            item_id: item_id.to_owned(),
            extra: serde_json::from_value(extra).unwrap(),
            ..ItemDescriptor::default()
        };
        let inventory = std::collections::HashMap::from([(
            "tool".to_owned(),
            vec![
                descriptor(
                    "tool:b/fetch",
                    serde_json::json!({"reads": {"argument": "item_ref"}, "space": "bundle"}),
                ),
                descriptor(
                    "tool:a/mine",
                    serde_json::json!({"reads": {"argument": "path"}, "space": "project"}),
                ),
                descriptor(
                    "tool:c/odd",
                    serde_json::json!({"reads": {"argument": "a b"}, "space": "bundle"}),
                ),
                descriptor("tool:d/none", serde_json::json!({"space": "bundle"})),
                descriptor(
                    "tool:e/nospace",
                    serde_json::json!({"reads": {"argument": "x"}}),
                ),
            ],
        )]);
        let payload = launch_tool_reads(&inventory).expect("two entries");
        assert_eq!(
            payload
                .tools
                .iter()
                .map(|entry| (entry.item_ref.as_str(), entry.argument.as_str()))
                .collect::<Vec<_>>(),
            [("tool:a/mine", "path"), ("tool:b/fetch", "item_ref")]
        );
        assert_eq!(payload.tools[0].space, crate::contracts::ItemSpace::Project);
        assert!(launch_tool_reads(&std::collections::HashMap::new()).is_none());
    }

    #[test]
    fn process_input_root_payload_is_a_strict_closed_kind_with_no_path() {
        let payload = ProcessInputRootSelectedPayload {
            root: ProcessInputRootKind::SparsePrivate,
        };
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value, serde_json::json!({"root": "sparse_private"}));
        assert_eq!(
            serde_json::from_value::<ProcessInputRootSelectedPayload>(value).unwrap(),
            payload
        );
        assert!(
            serde_json::from_value::<ProcessInputRootSelectedPayload>(
                serde_json::json!({"root": "live", "path": "/home/x"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProcessInputRootSelectedPayload>(
                serde_json::json!({"root": "elsewhere"})
            )
            .is_err()
        );
    }
}
