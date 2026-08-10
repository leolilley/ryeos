use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    EffectivePrincipal, ExecutionDecorations, ExecutionHints, LaunchMode, ProjectContext,
    RuntimeEnvSource,
};

/// Typed authority under which item subjects are resolved for one plan.
///
/// This is deliberately separate from [`ProjectContext`]. A local path says
/// where resolution happens; it does not say whether those bytes are live,
/// an immutable admitted generation, or a writable workspace based on an
/// admitted generation. Keeping the class and generation explicit prevents a
/// request-specific checkout pathname from becoming project identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubjectResolutionAuthority {
    Projectless,
    LiveFs,
    PinnedGeneration {
        snapshot_hash: String,
    },
    CowWorkspace {
        base_snapshot_hash: String,
        current_operational_generation: String,
    },
}

impl SubjectResolutionAuthority {
    /// Authority for a current, mutable project view, or for a bundle-only
    /// request when no project root exists.
    pub fn for_live_project_root(materialized_project_root: Option<&std::path::Path>) -> Self {
        if materialized_project_root.is_some() {
            Self::LiveFs
        } else {
            Self::Projectless
        }
    }

    pub fn validate_for_project_context(
        &self,
        project_context: &ProjectContext,
    ) -> anyhow::Result<()> {
        match self {
            Self::Projectless => {
                if !matches!(project_context, ProjectContext::None) {
                    anyhow::bail!(
                        "projectless subject resolution requires a projectless planning context"
                    );
                }
            }
            Self::LiveFs => {
                if !matches!(project_context, ProjectContext::LocalPath { .. }) {
                    anyhow::bail!(
                        "live filesystem subject resolution requires a local project path"
                    );
                }
            }
            Self::PinnedGeneration { snapshot_hash } => {
                validate_snapshot_hash(snapshot_hash)?;
                match project_context {
                    ProjectContext::LocalPath { .. } => {}
                    ProjectContext::SnapshotHash { hash } if hash == snapshot_hash => {}
                    ProjectContext::SnapshotHash { .. } => {
                        anyhow::bail!(
                            "pinned subject resolution authority differs from snapshot context"
                        )
                    }
                    ProjectContext::None | ProjectContext::ProjectRef { .. } => anyhow::bail!(
                        "pinned subject resolution requires a project planning context"
                    ),
                }
            }
            Self::CowWorkspace {
                base_snapshot_hash,
                current_operational_generation,
            } => {
                validate_snapshot_hash(base_snapshot_hash)?;
                validate_snapshot_hash(current_operational_generation)?;
                if !matches!(project_context, ProjectContext::LocalPath { .. }) {
                    anyhow::bail!(
                        "writable COW subject resolution requires an admitted workspace path"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate_for_materialized_root(
        &self,
        materialized_project_root: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        match (self, materialized_project_root) {
            (Self::Projectless, None) => Ok(()),
            (Self::Projectless, Some(_)) => {
                anyhow::bail!("projectless subject resolution cannot carry a project root")
            }
            (Self::LiveFs | Self::PinnedGeneration { .. } | Self::CowWorkspace { .. }, Some(_)) => {
                Ok(())
            }
            (Self::LiveFs | Self::PinnedGeneration { .. } | Self::CowWorkspace { .. }, None) => {
                anyhow::bail!("project subject resolution requires a materialized project root")
            }
        }
    }

    pub fn operational_generation(&self) -> Option<&str> {
        match self {
            Self::PinnedGeneration { snapshot_hash } => Some(snapshot_hash),
            Self::CowWorkspace {
                current_operational_generation,
                ..
            } => Some(current_operational_generation),
            Self::Projectless | Self::LiveFs => None,
        }
    }

    /// Whether this authority may legitimately change between observation and
    /// use. Projectless and content-addressed authorities are immutable;
    /// exactly the live filesystem class permits a bounded revalidation retry.
    pub fn permits_mutable_revalidation(&self) -> bool {
        matches!(self, Self::LiveFs)
    }
}

fn validate_snapshot_hash(value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("subject resolution snapshot identity must be a 64-character hex digest");
    }
    Ok(())
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

// ── Plan context (resolve/verify/build_plan) ─────────────────────────

/// Context for the planning phases: resolve, verify, build_plan.
///
/// Does NOT carry thread IDs or daemon runtime bindings.
/// This is what makes `validate_only` safe.
#[derive(Debug, Clone)]
pub struct PlanContext {
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    /// Exact resolution authority for `project_context`. A local path is only
    /// a materialization; this field states whether its bytes are live,
    /// immutable pinned content, or a writable COW generation.
    pub subject_resolution_authority: SubjectResolutionAuthority,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub execution_hints: ExecutionHints,
    /// When true, the daemon should not call `execute_plan` after
    /// `build_plan` succeeds. The engine does not enforce this — it is
    /// safe structurally because `PlanContext` does not carry thread IDs.
    pub validate_only: bool,
}

// ── Engine context (execute_plan) ────────────────────────────────────

/// Context for plan execution. Carries everything in `PlanContext` plus
/// daemon-allocated thread identity and runtime bindings.
#[derive(Debug, Clone)]
pub struct EngineContext {
    pub app_root: PathBuf,
    pub isolation: Arc<crate::isolation::IsolationRuntime>,
    pub isolation_project_authority: crate::isolation::IsolationProjectAuthority,
    pub isolation_filesystem_authority_ceiling:
        crate::isolation::IsolationFilesystemAuthorityCeiling,
    pub isolation_network_authority_ceiling: crate::isolation::IsolationNetworkAuthorityCeiling,
    pub isolation_live_access_authority: Option<crate::isolation::IsolationLiveAccessAuthority>,
    pub isolation_state_root: Option<PathBuf>,
    pub isolation_checkpoint_dir: Option<PathBuf>,
    /// Typed callback-socket fact paired with this plan's daemon callback env.
    pub isolation_daemon_socket_path: Option<PathBuf>,
    pub isolation_bundle_roots: Vec<PathBuf>,
    pub isolation_node_trusted_keys_dir: Option<PathBuf>,
    pub isolation_verified_code: Vec<crate::isolation::IsolationVerifiedCode>,
    /// Exact already-open command authority for an admitted direct plan.
    /// When present it must match the plan's serialized verified-command
    /// identity; dispatch never reopens that command by pathname.
    pub isolation_verified_command: Option<crate::isolation::IsolationDescriptorBoundCommand>,
    pub isolation_external_read_only_mounts: Vec<crate::isolation::IsolationReadOnlyMountAuthority>,
    /// One daemon-created connected duplex channel with a signed target
    /// environment binding. This is deliberately distinct from generic
    /// inherited descriptors and cannot be supplied as a raw fd.
    pub isolation_target_channel: Option<crate::isolation::IsolationTargetChannelAuthority>,
    /// Explicit daemon-owned execution workspace used only by isolation.
    /// This does not change item-resolution authority or project semantics;
    /// it gives projectless admitted mechanics (for example a persistent
    /// session) one bounded mount namespace for retained read-only content.
    pub isolation_workspace: Option<PathBuf>,
    /// Daemon-owned mechanical limits applied to this exact subprocess tree.
    /// Kind semantics stay outside the engine; the admitted capsule supplies
    /// these generic kernel ceilings for reusable sessions.
    pub subprocess_limits: Option<lillux::SubprocessLimits>,
    /// Already-open descriptors deliberately inherited by the child.  The
    /// descriptor numbers are paired with signed protocol environment
    /// bindings before this context is constructed; no ambient descriptor is
    /// inherited.
    pub inherited_fds: Vec<Arc<std::fs::File>>,
    pub thread_id: String,
    pub chain_root_id: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_site_id: Option<String>,
    pub upstream_thread_id: Option<String>,
    pub continuation_from_id: Option<String>,
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    pub launch_mode: LaunchMode,
}

// ── Plan IR ──────────────────────────────────────────────────────────

/// Unique identifier for a plan node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanNodeId(pub String);

/// Plan capabilities declared by the execution plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCapabilities {
    pub requires_model: bool,
    pub requires_subprocess: bool,
    pub requires_network: bool,
    pub custom: Vec<String>,
}

/// Materialization requirement for plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationRequirement {
    pub kind: String,
    pub ref_string: String,
}

/// Exact verified runtime descriptor selected while resolving the executor
/// chain for a direct item launch. This travels with the built plan so later
/// admission never re-resolves a mutable runtime reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRuntimeIdentity {
    pub runtime_ref: String,
    pub runtime_source_space: crate::contracts::ItemSpace,
    pub runtime_content_hash: String,
    pub runtime_signer_fingerprint: Option<String>,
    pub runtime_bundle_manifest_hash: Option<String>,
    pub runtime_bundle_signer_fingerprint: Option<String>,
}

/// Exact signed source authority for one executor-chain item that influenced
/// a direct execution plan. Recovery uses this compact closure only for
/// current signer revocation; it never re-resolves `canonical_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTrustAuthority {
    pub requested_id: String,
    pub canonical_ref: String,
    pub source_space: crate::contracts::ItemSpace,
    pub trust_class: crate::resolution::TrustClass,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub signer_fingerprint: Option<String>,
    pub content_hash: String,
}

/// Signed executor-manifest identity of the installed bundle that supplied a
/// `bin:` command. This authorizes executable material only; it contributes no
/// runtime or callback capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanBundleExecutorIdentity {
    pub manifest_hash: String,
    pub signer_fingerprint: String,
}

/// Exact command authority selected while compiling a subprocess plan.
/// Keeping bundle resolution distinct from descriptor-rooted capture prevents
/// either path from silently degrading into the other after `bin:` has been
/// expanded to an absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanVerifiedCommand {
    BundleExecutor {
        code: crate::isolation::IsolationVerifiedCode,
        provider: PlanBundleExecutorIdentity,
    },
    CapturedContent {
        code: crate::isolation::IsolationVerifiedCode,
    },
}

impl PlanVerifiedCommand {
    pub fn code(&self) -> &crate::isolation::IsolationVerifiedCode {
        match self {
            Self::BundleExecutor { code, .. } | Self::CapturedContent { code } => code,
        }
    }
}

/// Typed stdin carried by a subprocess plan.
///
/// Runtime parameters remain structured until the final spawn so runtime-owned
/// path bindings can be relocated without treating arbitrary program input as
/// a path-bearing format. Opaque input is never parsed or rewritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanStdin {
    Opaque {
        data: String,
    },
    RuntimeParameters {
        parameters: Value,
        project_path: Option<PathBuf>,
    },
}

impl PlanStdin {
    pub fn materialize(&self) -> Result<String, String> {
        match self {
            Self::Opaque { data } => Ok(data.clone()),
            Self::RuntimeParameters {
                parameters,
                project_path,
            } => {
                let mut materialized = parameters.clone();
                if let Some(project_path) = project_path {
                    let object = materialized.as_object_mut().ok_or_else(|| {
                        "runtime parameter project_path binding requires object parameters"
                            .to_owned()
                    })?;
                    object.insert(
                        "project_path".to_owned(),
                        Value::String(project_path.to_string_lossy().into_owned()),
                    );
                }
                Ok(materialized.to_string())
            }
        }
    }
}

/// Normalized subprocess specification — the single source of truth for
/// what to spawn. Compiled from the executor chain's runtime config by
/// the plan builder. The dispatch layer just runs this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSubprocessSpec {
    pub cmd: String,
    /// Exact identity of a bundle/CAS command resolved while building this
    /// plan. System executables and project-local interpreters use `None`.
    pub verified_command: Option<PlanVerifiedCommand>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Source category for each env entry. This lets the daemon apply
    /// final subprocess env policy without guessing from key names.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_sources: HashMap<String, RuntimeEnvSource>,
    pub stdin: Option<PlanStdin>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Per-tool execution policy populated by `DecorateSpec`-phase
    /// runtime handlers (`native_async`, future `native_resume`,
    /// `execution_owner`). Default = empty → preserves baseline
    /// behavior for tools that declare none of these.
    #[serde(default)]
    pub execution: ExecutionDecorations,
}

fn default_timeout_secs() -> u64 {
    300
}

/// A node in the execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "node_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanNode {
    DispatchSubprocess {
        id: PlanNodeId,
        /// The fully resolved subprocess specification.
        spec: Box<PlanSubprocessSpec>,
        /// Audit: the root item's source path.
        ///
        /// This is live diagnostic state, not executable plan state. It is
        /// deliberately excluded from the admitted plan wire identity: the
        /// source bytes/root identity are committed elsewhere, while this
        /// host installation path is inert after compilation.
        #[serde(skip)]
        tool_path: Option<PathBuf>,
        /// Audit: executor IDs traversed during chain resolution.
        #[serde(default)]
        executor_chain: Vec<String>,
    },
    Complete {
        id: PlanNodeId,
    },
}

impl PlanNode {
    pub fn id(&self) -> &PlanNodeId {
        match self {
            Self::DispatchSubprocess { id, .. } | Self::Complete { id, .. } => id,
        }
    }
}

/// Normalized execution plan — the engine's output from `build_plan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlan {
    pub plan_id: String,
    pub root_executor_id: String,
    pub root_ref: String,
    pub item_kind: String,
    pub nodes: Vec<PlanNode>,
    pub entrypoint: PlanNodeId,
    pub capabilities: PlanCapabilities,
    pub materialization_requirements: Vec<MaterializationRequirement>,
    pub cache_key: String,
    /// Daemon supervision profile hint, derived from the root item's kind.
    #[serde(default)]
    pub thread_kind: Option<String>,
    /// Executor IDs traversed during chain resolution.
    #[serde(default)]
    pub executor_chain: Vec<String>,
    /// Exact source authorities for every non-root executor-chain hop that
    /// contributed to this plan.
    pub executor_authorities: Vec<PlanTrustAuthority>,
    /// Verified identity of the first executor-chain hop (the runtime
    /// descriptor) selected by this exact plan build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_identity: Option<PlanRuntimeIdentity>,
    /// When set (from `--debug-raw` via `execution_hints`), the dispatcher
    /// attaches a `debug` block (resolved cmd/args/cwd/env keys + exit code and
    /// size-limited raw stdout/stderr) to the completion. Default `false` —
    /// the normal execution path is unaffected.
    #[serde(default)]
    pub debug_raw: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprocess_defaults_and_plan_node_wire_shape_are_stable() {
        let spec: PlanSubprocessSpec = serde_json::from_value(serde_json::json!({
            "cmd": "/bin/true",
            "verified_command": null,
            "cwd": null,
            "stdin": null
        }))
        .unwrap();
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert!(spec.env_sources.is_empty());
        assert_eq!(spec.timeout_secs, 300);
        assert!(spec.execution.native_async.is_none());
        assert!(spec.execution.native_resume.is_none());

        let node = PlanNode::Complete {
            id: PlanNodeId("done".to_string()),
        };
        assert_eq!(node.id().0, "done");
        assert_eq!(
            serde_json::to_value(node).unwrap(),
            serde_json::json!({ "node_type": "complete", "id": "done" })
        );
    }

    #[test]
    fn runtime_parameter_stdin_materializes_authoritative_project_path() {
        let stdin = PlanStdin::RuntimeParameters {
            parameters: serde_json::json!({
                "message": "hello",
                "project_path": "/caller-controlled"
            }),
            project_path: Some(PathBuf::from("/trusted/project")),
        };
        let materialized: Value =
            serde_json::from_str(&stdin.materialize().unwrap()).expect("valid parameter JSON");
        assert_eq!(
            materialized,
            serde_json::json!({
                "message": "hello",
                "project_path": "/trusted/project"
            })
        );
    }

    #[test]
    fn plan_trust_authority_requires_explicit_nullable_signer() {
        let mut wire = serde_json::json!({
            "requested_id": "runtime:test/direct",
            "canonical_ref": "runtime:test/direct",
            "source_space": "bundle",
            "trust_class": "unsigned",
            "signer_fingerprint": null,
            "content_hash": "a".repeat(64),
        });
        serde_json::from_value::<PlanTrustAuthority>(wire.clone()).unwrap();
        wire.as_object_mut().unwrap().remove("signer_fingerprint");
        assert!(serde_json::from_value::<PlanTrustAuthority>(wire).is_err());
    }

    #[test]
    fn subject_resolution_authority_distinguishes_cow_base_and_current_generation() {
        let base = "a".repeat(64);
        let current = "b".repeat(64);
        let authority = SubjectResolutionAuthority::CowWorkspace {
            base_snapshot_hash: base,
            current_operational_generation: current.clone(),
        };
        authority
            .validate_for_project_context(&ProjectContext::LocalPath {
                path: PathBuf::from("/tmp/cow"),
            })
            .unwrap();
        assert_eq!(authority.operational_generation(), Some(current.as_str()));
    }

    #[test]
    fn subject_resolution_authority_rejects_projectless_path_and_bad_hashes() {
        assert!(
            SubjectResolutionAuthority::Projectless
                .validate_for_project_context(&ProjectContext::LocalPath {
                    path: PathBuf::from("/tmp/project"),
                })
                .is_err()
        );
        assert!(
            SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: "not-a-hash".to_owned(),
            }
            .validate_for_project_context(&ProjectContext::SnapshotHash {
                hash: "not-a-hash".to_owned(),
            })
            .is_err()
        );
    }

    #[test]
    fn live_and_cow_authorities_reject_snapshot_contexts() {
        let snapshot = "a".repeat(64);
        let snapshot_context = ProjectContext::SnapshotHash {
            hash: snapshot.clone(),
        };
        assert!(
            SubjectResolutionAuthority::LiveFs
                .validate_for_project_context(&snapshot_context)
                .is_err()
        );
        assert!(
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: snapshot.clone(),
                current_operational_generation: snapshot,
            }
            .validate_for_project_context(&snapshot_context)
            .is_err()
        );
    }
}
