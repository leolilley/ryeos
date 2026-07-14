use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    EffectivePrincipal, ExecutionDecorations, ExecutionHints, LaunchMode, ProjectContext,
    RuntimeEnvSource,
};

// ── Plan context (resolve/verify/build_plan) ─────────────────────────

/// Context for the planning phases: resolve, verify, build_plan.
///
/// Does NOT carry thread IDs or daemon runtime bindings.
/// This is what makes `validate_only` safe.
#[derive(Debug, Clone)]
pub struct PlanContext {
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
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
    pub sandbox: Arc<crate::sandbox::SandboxRuntime>,
    pub sandbox_project_authority: crate::sandbox::SandboxProjectAuthority,
    pub sandbox_state_root: Option<PathBuf>,
    pub sandbox_checkpoint_dir: Option<PathBuf>,
    pub sandbox_bundle_roots: Vec<PathBuf>,
    pub sandbox_operator_trusted_keys_dir: Option<PathBuf>,
    pub sandbox_verified_code: Vec<crate::sandbox::SandboxVerifiedCode>,
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

/// Normalized subprocess specification — the single source of truth for
/// what to spawn. Compiled from the executor chain's runtime config by
/// the plan builder. The dispatch layer just runs this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSubprocessSpec {
    pub cmd: String,
    /// Exact identity of a bundle/CAS command resolved while building this
    /// plan. System executables and project-local interpreters use `None`.
    pub verified_command: Option<crate::sandbox::SandboxVerifiedCode>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Source category for each env entry. This lets the daemon apply
    /// final subprocess env policy without guessing from key names.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_sources: HashMap<String, RuntimeEnvSource>,
    pub stdin_data: Option<String>,
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
        spec: PlanSubprocessSpec,
        /// Audit: the root item's source path.
        #[serde(default)]
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
            "stdin_data": null
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
}
