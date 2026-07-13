use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeEnvSource {
    EnginePlan,
    RuntimeDescriptor,
    RuntimeInterpreter,
    RuntimePathMutation,
}

/// Typed bag of `DecorateSpec`-phase outputs. Each field is `Option`
/// so absence ⇒ "preserve current default". Future decorate handlers
/// add siblings here without breaking the top-level spec shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDecorations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_async: Option<NativeAsyncSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_resume: Option<NativeResumeSpec>,
}

/// Resume policy declared by the `native_resume` runtime handler.
/// Presence in the spec ⇒ the tool is replay-aware: the daemon will
/// allocate a per-thread checkpoint dir, inject `RYEOS_CHECKPOINT_DIR`
/// at spawn time, and on daemon restart attempt automatic resume up
/// to `max_auto_resume_attempts` times before marking the thread
/// failed. The tool is responsible for writing checkpoints into the
/// supplied directory and for being idempotent / replay-safe on
/// startup (`RYEOS_RESUME=1` is injected on resume spawns).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeResumeSpec {
    /// Hint to the tool for how often to checkpoint. Engine and daemon
    /// do not enforce this — purely advisory.
    pub checkpoint_interval_secs: u64,
    /// Hard ceiling on automatic resume attempts after daemon restart.
    /// `1` (default) = single retry. `0` = never auto-resume (still
    /// declares replay-awareness for manual resume tooling).
    pub max_auto_resume_attempts: u32,
}

impl Default for NativeResumeSpec {
    fn default() -> Self {
        Self {
            checkpoint_interval_secs: 30,
            max_auto_resume_attempts: 1,
        }
    }
}

impl NativeResumeSpec {
    /// Parse a `native_resume` declaration from its YAML/JSON value. Shared by
    /// the engine `native_resume` runtime handler (chain-element specs) and the
    /// runtime-registry `RuntimeYaml`, so both accept the identical shapes:
    ///   * `true` ⇒ defaults;
    ///   * an object ⇒ the rich form (each field defaults individually);
    ///   * `false` ⇒ rejected — omit the block to disable.
    ///
    /// Returns a plain `String` reason on error so each caller can wrap it in
    /// its own error type (engine `InvalidRuntimeConfig`, serde, …).
    pub fn parse_declaration(value: &Value) -> Result<Self, String> {
        match value {
            Value::Bool(true) => Ok(Self::default()),
            Value::Bool(false) => Err(
                "`native_resume: false` is not supported — omit the block to disable".to_string(),
            ),
            other => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct RichForm {
                    #[serde(default = "default_checkpoint_interval_secs")]
                    checkpoint_interval_secs: u64,
                    #[serde(default = "default_max_auto_resume_attempts")]
                    max_auto_resume_attempts: u32,
                }
                let rich: RichForm = serde_json::from_value(other.clone())
                    .map_err(|e| format!("invalid native_resume block: {e}"))?;
                Ok(Self {
                    checkpoint_interval_secs: rich.checkpoint_interval_secs,
                    max_auto_resume_attempts: rich.max_auto_resume_attempts,
                })
            }
        }
    }
}

fn default_checkpoint_interval_secs() -> u64 {
    NativeResumeSpec::default().checkpoint_interval_secs
}

fn default_max_auto_resume_attempts() -> u32 {
    NativeResumeSpec::default().max_auto_resume_attempts
}

/// Cancellation + streaming policy declared by the `native_async`
/// runtime handler. Presence in the spec ⇒ this tool drives its own
/// event stream (the runner injects `RYEOS_NATIVE_ASYNC=1`) and the
/// daemon cancellation routes through `cancellation_mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAsyncSpec {
    pub cancellation_mode: CancellationMode,
}

/// How the runner terminates the subprocess on cancellation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CancellationMode {
    /// SIGKILL the process group immediately.
    Hard,
    /// SIGTERM, wait `grace_secs`, then SIGKILL.
    Graceful { grace_secs: u64 },
}

impl Default for CancellationMode {
    fn default() -> Self {
        CancellationMode::Graceful { grace_secs: 5 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_decoration_defaults_are_stable() {
        assert_eq!(NativeResumeSpec::default().checkpoint_interval_secs, 30);
        assert_eq!(NativeResumeSpec::default().max_auto_resume_attempts, 1);
        assert_eq!(
            CancellationMode::default(),
            CancellationMode::Graceful { grace_secs: 5 }
        );
        assert_eq!(
            serde_json::to_value(ExecutionDecorations::default()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::to_value(RuntimeEnvSource::RuntimePathMutation).unwrap(),
            serde_json::json!("runtime_path_mutation")
        );
    }

    #[test]
    fn native_resume_declaration_error_semantics_are_stable() {
        assert_eq!(
            NativeResumeSpec::parse_declaration(&Value::Bool(false)).unwrap_err(),
            "`native_resume: false` is not supported — omit the block to disable"
        );
        assert!(
            NativeResumeSpec::parse_declaration(&serde_json::json!({ "unknown": true }))
                .unwrap_err()
                .starts_with("invalid native_resume block:")
        );
    }
}
