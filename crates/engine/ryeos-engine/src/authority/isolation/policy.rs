use ryeos_isolation_protocol::{FixedParentViewLimits, IsolationBackendSelection};
use serde::{Deserialize, Serialize};

pub const ISOLATION_POLICY_VERSION: u32 = 10;
/// Host class of the policy-free authoring and test boundary.
pub const AUTHORING_HOST_CLASS: &str = "authoring";
/// Required outright, together with the nested proc ceiling, this host
/// capability is the node's permission to grant a nested sandbox.
pub use lillux::process_control::ISOLATION_NAMESPACES;
#[cfg(any(test, feature = "test-support"))]
pub const TEST_ISOLATION_POLICY_RELATIVE_PATH: &str = "test-fixtures/isolation-policy.yaml";

/// Longest host class name a policy may declare.
pub const MAX_HOST_CLASS_NAME_BYTES: usize = 64;
/// Most host classes one policy may declare.
pub const MAX_HOST_CLASSES: usize = 8;

/// Signed wire form of the node isolation policy.
///
/// A host class names the kind of host a deployment declares it runs the
/// node on, such as `local` for a host the operator runs or `hosted` for a
/// hosting provider's container. The name says where the node runs, not how
/// strong its guarantees are: each class carries its own floor and the
/// isolation lane that floor supports, and the same class name carries a
/// different floor in another profile. Everything else is shared. Class
/// names are open data; this contract gives none of them a meaning. A
/// publisher profile may declare several classes. Initialization selects
/// one, explicitly named by the deployment, and the node's own policy then
/// declares exactly that one, so the class a node runs under is part of its
/// signed policy and never inferred from what the host proves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationPolicyDocument {
    pub version: u32,
    pub host_classes: std::collections::BTreeMap<String, IsolationHostClass>,
    /// Executable names of inits trusted to exit when their only child
    /// exits, sorted and unique, shared by every class.
    pub enclosure_inits: Vec<String>,
    pub filesystem: IsolationSharedFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

/// What one host class requires of its host and how jobs are isolated on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationHostClass {
    /// Capability floor the node must prove on a host of this class before
    /// it starts; see [`IsolationPolicy::requires_capabilities`].
    pub requires_capabilities: Vec<lillux::process_control::CapabilityRequirement>,
    pub process_scopes: IsolationProcessScopePolicy,
    pub trusted_process_group_sessions: bool,
    pub mode: IsolationMode,
    pub backend: Option<IsolationBackendSelection>,
    pub proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem,
    /// See [`IsolationPolicy::host_lifetime_bound`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_lifetime_bound: Option<lillux::process_control::HostLifetimeBound>,
}

/// Filesystem policy shared by every host class; the proc ceiling is per
/// class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationSharedFilesystemPolicy {
    pub live_project: IsolationLiveProjectPolicy,
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

/// Whether `name` is a canonical host class name: lowercase ASCII letters,
/// digits and single `_` separators, starting with a letter.
pub fn valid_host_class_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HOST_CLASS_NAME_BYTES
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__")
}

impl IsolationPolicyDocument {
    /// Declared host class names, in order.
    pub fn host_class_names(&self) -> Vec<String> {
        self.host_classes.keys().cloned().collect()
    }

    /// This document narrowed to the one named class. A class the document
    /// does not declare is refused; nothing stands in for it.
    pub fn narrowed(&self, host_class: &str) -> Result<Self, String> {
        let class = self.host_classes.get(host_class).ok_or_else(|| {
            format!(
                "host class `{host_class}` is not declared by this isolation policy; it declares {}",
                self.host_class_names().join(", ")
            )
        })?;
        let mut narrowed = self.clone();
        narrowed.host_classes = [(host_class.to_owned(), class.clone())].into();
        Ok(narrowed)
    }

    /// The runtime policy of the one class this document declares. A node
    /// runs under exactly one class, so a document declaring several (a
    /// publisher profile before initialization selects one) is refused.
    pub fn into_selected_policy(self) -> Result<IsolationPolicy, String> {
        if self.host_classes.len() != 1 {
            return Err(format!(
                "a node isolation policy declares exactly one host class, selected at \
                 initialization; this one declares {}",
                if self.host_classes.is_empty() {
                    "none".to_owned()
                } else {
                    self.host_class_names().join(", ")
                }
            ));
        }
        let Self {
            version,
            host_classes,
            enclosure_inits,
            filesystem,
            network,
            environment,
            limits,
        } = self;
        let Some((host_class, class)) = host_classes.into_iter().next() else {
            return Err("a node isolation policy declares no host class".to_owned());
        };
        let IsolationHostClass {
            requires_capabilities,
            process_scopes,
            trusted_process_group_sessions,
            mode,
            backend,
            proc_filesystem,
            host_lifetime_bound,
        } = class;
        let IsolationSharedFilesystemPolicy {
            live_project,
            readable,
            writable,
        } = filesystem;
        Ok(IsolationPolicy {
            version,
            host_class,
            mode,
            backend,
            process_scopes,
            requires_capabilities,
            enclosure_inits,
            host_lifetime_bound,
            trusted_process_group_sessions,
            filesystem: IsolationFilesystemPolicy {
                proc_filesystem,
                live_project,
                readable,
                writable,
            },
            network,
            environment,
            limits,
        })
    }

    /// The single-class document whose selected policy is `policy`.
    pub fn from_selected_policy(policy: IsolationPolicy) -> Self {
        let IsolationPolicy {
            version,
            host_class,
            mode,
            backend,
            process_scopes,
            requires_capabilities,
            enclosure_inits,
            host_lifetime_bound,
            trusted_process_group_sessions,
            filesystem,
            network,
            environment,
            limits,
        } = policy;
        let IsolationFilesystemPolicy {
            proc_filesystem,
            live_project,
            readable,
            writable,
        } = filesystem;
        Self {
            version,
            host_classes: [(
                host_class,
                IsolationHostClass {
                    requires_capabilities,
                    process_scopes,
                    trusted_process_group_sessions,
                    mode,
                    backend,
                    proc_filesystem,
                    host_lifetime_bound,
                },
            )]
            .into(),
            enclosure_inits,
            filesystem: IsolationSharedFilesystemPolicy {
                live_project,
                readable,
                writable,
            },
            network,
            environment,
            limits,
        }
    }
}

/// The runtime isolation policy of one node: the shared policy together
/// with the one host class the node was initialized under.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationPolicy {
    pub version: u32,
    /// The host class the deployment declared at initialization.
    pub host_class: String,
    pub mode: IsolationMode,
    pub backend: Option<IsolationBackendSelection>,
    pub process_scopes: IsolationProcessScopePolicy,
    /// Capabilities this node must prove before it starts, sorted and
    /// unique. Each entry is a name, or an `any_of` set of names any one of
    /// which meets it, so a weaker grade is accepted only where the floor
    /// names it. Names are open strings matched against the node capability
    /// report; a name this build cannot prove is refused, never ignored.
    pub requires_capabilities: Vec<lillux::process_control::CapabilityRequirement>,
    /// Executable names of inits trusted to exit when their only child
    /// exits, sorted and unique. When the node's parent is pid 1 and pid 1
    /// is one of these, the node's exit is trusted to end its container
    /// enclosure; the host proves that only when the node is pid 1 itself.
    pub enclosure_inits: Vec<String>,
    /// The hard lifetime the hosting provider imposes on this node's host
    /// class, when it imposes one. Nothing inside the host can measure it,
    /// so it is declared here, and a declared bound withholds
    /// `persistence:durable_state` from the node's capability report.
    /// Absent means none is declared, which is itself declared data: it
    /// grades that capability no higher than trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_lifetime_bound: Option<lillux::process_control::HostLifetimeBound>,
    /// Explicit opt-in for trusted dedicated workers that use RyeOS's direct
    /// process-group lifecycle without claiming qualified containment.
    pub trusted_process_group_sessions: bool,
    pub filesystem: IsolationFilesystemPolicy,
    pub network: IsolationNetworkPolicy,
    pub environment: IsolationEnvironmentPolicy,
    pub limits: IsolationLimitsPolicy,
}

impl IsolationPolicy {
    /// Per-scope guarantees this policy's capability floor requires outright:
    /// the set the scope provider is asked to qualify, and nothing more. A
    /// set of alternatives never names one, which validation enforces.
    pub fn required_process_scope_capabilities(
        &self,
    ) -> std::collections::BTreeSet<lillux::ProcessScopeCapability> {
        self.requires_capabilities
            .iter()
            .filter_map(|entry| {
                let [name] = entry.names() else {
                    return None;
                };
                lillux::ProcessScopeCapability::from_capability_name(name)
            })
            .collect()
    }

    /// Node permission to grant scoped launches a nested sandbox, derived
    /// from the floor requiring `isolation:namespaces` outright together
    /// with the `pid_namespace_nested` proc ceiling. Validation keeps the
    /// two in step, so neither alone ever means it. This is permission, not
    /// evidence that a particular launch has a scope.
    pub fn nested_sandbox(&self) -> bool {
        nested_sandbox_from(
            &self.requires_capabilities,
            self.filesystem.proc_filesystem,
        )
    }

    /// Explicit policy-free authoring/test boundary. Running nodes compile
    /// their mandatory signed isolation policy instead of using this value.
    pub fn disabled_for_authoring() -> Self {
        Self {
            version: ISOLATION_POLICY_VERSION,
            host_class: AUTHORING_HOST_CLASS.to_owned(),
            mode: IsolationMode::Disabled,
            backend: None,
            process_scopes: IsolationProcessScopePolicy::Unconfigured {},
            requires_capabilities: Vec::new(),
            enclosure_inits: Vec::new(),
            host_lifetime_bound: None,
            trusted_process_group_sessions: false,
            filesystem: IsolationFilesystemPolicy {
                proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem::Empty,
                // Values for this explicit non-enforcing authoring fixture
                // only. Never call this constructor to fill missing node
                // policy: enforced launch limits come from its signed member.
                live_project: IsolationLiveProjectPolicy::FixedParents {
                    limits: FixedParentViewLimits {
                        max_entries: 2048,
                        max_depth: 64,
                    },
                },
                readable: vec![
                    "{node_public_identity}".to_string(),
                    "{daemon_socket}".to_string(),
                    "{bundle_roots}".to_string(),
                    "{node_trusted_keys}".to_string(),
                    "{verified_code}".to_string(),
                ],
                writable: vec!["{project}".to_string(), "{checkpoint_dir}".to_string()],
            },
            network: IsolationNetworkPolicy {
                mode: IsolationNetworkMode::Host,
                runtime_files: Vec::new(),
            },
            environment: IsolationEnvironmentPolicy {
                allow: vec!["*".to_string()],
            },
            limits: IsolationLimitsPolicy {
                open_files: Some(1024),
                stdout_bytes: 8_388_608,
                stderr_bytes: 8_388_608,
                verified_artifact_file_bytes: 67_108_864,
                verified_artifact_total_bytes: 268_435_456,
                verified_artifact_files: 4_096,
            },
        }
    }
}

/// Node semantic authority for the optional host facility, not permission to
/// downgrade an execution which requires it. Unconfigured nodes must refuse
/// scope-backed launch; ordinary shared-group launches retain their
/// no-group-escape contract. Native provider selection, delegation paths, and
/// scope configuration are deliberately absent: Lillux retains and opens those
/// only from the protected host association at daemon boot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationProcessScopePolicy {
    // Keep this an empty struct variant: serde's internally-tagged unit
    // variant accepts unknown fields even with deny_unknown_fields.
    Unconfigured {},
    Required {
        control_timeout_ms: u64,
    },
}

/// Whether `floor` requires `name` outright: as the only name of an entry,
/// never as one alternative in a set.
pub fn floor_requires_outright(
    floor: &[lillux::process_control::CapabilityRequirement],
    name: &str,
) -> bool {
    floor
        .iter()
        .any(|entry| matches!(entry.names(), [only] if only == name))
}

/// Nested-sandbox permission as [`IsolationPolicy::nested_sandbox`] derives
/// it, for callers holding the floor and proc ceiling apart from the policy.
pub fn nested_sandbox_from(
    floor: &[lillux::process_control::CapabilityRequirement],
    proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem,
) -> bool {
    floor_requires_outright(floor, ISOLATION_NAMESPACES)
        && proc_filesystem == ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespaceNested
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    Disabled,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationFilesystemPolicy {
    pub proc_filesystem: ryeos_isolation_protocol::IsolationProcFilesystem,
    pub live_project: IsolationLiveProjectPolicy,
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

/// Explicit node-owned confined-live contract, never selected by a workload.
/// Fixed parent entries are an admitted restriction, not silent CoW behavior.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum IsolationLiveProjectPolicy {
    FixedParents { limits: FixedParentViewLimits },
}

impl IsolationLiveProjectPolicy {
    pub fn limits(self) -> FixedParentViewLimits {
        match self {
            Self::FixedParents { limits } => limits,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationNetworkPolicy {
    pub mode: IsolationNetworkMode,
    /// Explicit target-local transport inputs, independent of general host
    /// filesystem access. Empty means none; no implicit resolver/trust fallback.
    pub runtime_files: Vec<IsolationNetworkRuntimeFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationNetworkRuntimeFile {
    pub source: std::path::PathBuf,
    pub destination: std::path::PathBuf,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationNetworkMode {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationEnvironmentPolicy {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IsolationLimitsPolicy {
    pub open_files: Option<u64>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub verified_artifact_file_bytes: u64,
    pub verified_artifact_total_bytes: u64,
    pub verified_artifact_files: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::process_control::{CapabilityRequirement, CapabilityRequirementSpec};

    fn outright(name: &str, providers: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: Some(name.to_owned()),
            any_of: Vec::new(),
            accept: Vec::new(),
            providers: providers.iter().map(|id| (*id).to_owned()).collect(),
        })
    }

    fn any_of(names: &[&str]) -> CapabilityRequirement {
        CapabilityRequirement::Spec(CapabilityRequirementSpec {
            name: None,
            any_of: names.iter().map(|name| (*name).to_owned()).collect(),
            accept: Vec::new(),
            providers: Vec::new(),
        })
    }

    #[test]
    fn per_scope_requirements_count_only_when_required_outright() {
        let mut policy = IsolationPolicy::disabled_for_authoring();
        policy.requires_capabilities = vec![
            any_of(&["accounting", "residency:swap_absent"]),
            outright("quiescence:scope", &["host_service"]),
            "recovery:scope".into(),
            "termination:scope".into(),
        ];
        assert_eq!(
            policy.required_process_scope_capabilities(),
            std::collections::BTreeSet::from([
                lillux::ProcessScopeCapability::Quiescence,
                lillux::ProcessScopeCapability::Termination,
                lillux::ProcessScopeCapability::Recovery,
            ])
        );
        policy.requires_capabilities = vec![any_of(&["recovery:scope", "termination:scope"])];
        assert!(policy.required_process_scope_capabilities().is_empty());
    }

    #[test]
    fn nested_sandbox_needs_both_the_floor_entry_and_the_nested_proc() {
        let mut policy = IsolationPolicy::disabled_for_authoring();
        assert!(!policy.nested_sandbox());
        policy.requires_capabilities = vec![ISOLATION_NAMESPACES.into()];
        assert!(!policy.nested_sandbox());
        policy.filesystem.proc_filesystem =
            ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespaceNested;
        assert!(policy.nested_sandbox());
        policy.requires_capabilities = vec![outright(ISOLATION_NAMESPACES, &["host"])];
        assert!(policy.nested_sandbox());
        policy.requires_capabilities = vec![any_of(&[ISOLATION_NAMESPACES, "time"])];
        assert!(!policy.nested_sandbox());
        policy.requires_capabilities = Vec::new();
        assert!(!policy.nested_sandbox());
        assert!(!floor_requires_outright(&policy.requires_capabilities, ISOLATION_NAMESPACES));
    }

    fn two_class_document() -> IsolationPolicyDocument {
        let mut document =
            IsolationPolicyDocument::from_selected_policy(IsolationPolicy::disabled_for_authoring());
        let mut local = document.host_classes.remove(AUTHORING_HOST_CLASS).unwrap();
        local.requires_capabilities = vec!["accounting".into(), "time".into()];
        let mut hosted = local.clone();
        hosted.requires_capabilities =
            vec![any_of(&["accounting", "residency:swap_absent"]), "time".into()];
        document.host_classes = [
            ("hosted".to_owned(), hosted),
            ("local".to_owned(), local),
        ]
        .into();
        document
    }

    #[test]
    fn a_selected_policy_round_trips_through_its_single_class_document() {
        let mut policy = IsolationPolicy::disabled_for_authoring();
        policy.host_class = "hosted".to_owned();
        policy.requires_capabilities = vec!["time".into()];
        let document = IsolationPolicyDocument::from_selected_policy(policy.clone());
        assert_eq!(document.host_class_names(), ["hosted"]);
        assert_eq!(document.into_selected_policy().unwrap(), policy);
    }

    #[test]
    fn narrowing_selects_exactly_the_named_class_and_refuses_an_undeclared_one() {
        let document = two_class_document();
        let error = document.clone().into_selected_policy().unwrap_err();
        assert!(error.contains("declares hosted, local"), "{error}");
        let hosted = document.narrowed("hosted").unwrap().into_selected_policy().unwrap();
        assert_eq!(hosted.host_class, "hosted");
        assert_eq!(hosted.requires_capabilities[0].names(), ["accounting", "residency:swap_absent"]);
        let local = document.narrowed("local").unwrap().into_selected_policy().unwrap();
        assert_eq!(local.host_class, "local");
        assert_eq!(local.requires_capabilities.len(), 2);
        let refused = document.narrowed("edge").unwrap_err();
        assert!(
            refused.contains("`edge` is not declared") && refused.contains("hosted, local"),
            "{refused}"
        );
        let mut empty = document;
        empty.host_classes.clear();
        assert!(empty.into_selected_policy().unwrap_err().contains("declares none"));
    }

    #[test]
    fn host_class_names_are_canonical() {
        for good in ["hosted", "local", "edge2"] {
            assert!(valid_host_class_name(good), "{good}");
        }
        for bad in ["", "Hosted", "local-enforced", "_hosted", "hosted_", "a__b", "2edge"] {
            assert!(!valid_host_class_name(bad), "{bad}");
        }
        assert!(!valid_host_class_name(&"a".repeat(MAX_HOST_CLASS_NAME_BYTES + 1)));
    }

    #[test]
    fn the_previous_flat_wire_shape_is_refused() {
        let mut flat = serde_json::to_value(IsolationPolicyDocument::from_selected_policy(
            IsolationPolicy::disabled_for_authoring(),
        ))
        .unwrap();
        flat["requires_capabilities"] = serde_json::json!([]);
        assert!(serde_json::from_value::<IsolationPolicyDocument>(flat).is_err());
    }

    #[test]
    fn the_removed_nested_sandbox_key_is_refused() {
        let error = serde_json::from_value::<IsolationProcessScopePolicy>(serde_json::json!({
            "mode": "required", "control_timeout_ms": 1000, "nested_sandbox": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(error.contains("nested_sandbox"), "{error}");
        let required: IsolationProcessScopePolicy = serde_json::from_value(serde_json::json!({
            "mode": "required", "control_timeout_ms": 1000,
        }))
        .unwrap();
        assert_eq!(
            required,
            IsolationProcessScopePolicy::Required {
                control_timeout_ms: 1000
            }
        );
    }
}
