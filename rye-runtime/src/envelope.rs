use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchEnvelope {
    pub envelope_version: u32,
    pub invocation_id: String,
    pub thread_id: String,
    pub roots: EnvelopeRoots,
    pub target: EnvelopeTarget,
    pub request: EnvelopeRequest,
    pub policy: EnvelopePolicy,
    pub callback: EnvelopeCallback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeRoots {
    pub project_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_root: Option<PathBuf>,
    pub system_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeTarget {
    pub item_id: String,
    pub kind: String,
    pub path: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeRequest {
    #[serde(default)]
    pub inputs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardLimits {
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub spend_usd: f64,
    #[serde(default)]
    pub spawns: u32,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub duration_seconds: u64,
}

impl Default for HardLimits {
    fn default() -> Self {
        Self {
            turns: 25,
            tokens: 200_000,
            spend_usd: 2.0,
            spawns: 10,
            depth: 5,
            duration_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopePolicy {
    #[serde(default)]
    pub effective_caps: Vec<String>,
    pub hard_limits: HardLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeCallback {
    pub socket_path: PathBuf,
    pub token: String,
    pub allowed_primaries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeResult {
    pub success: bool,
    pub status: String,
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default)]
    pub outputs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCost {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_usd: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let envelope = LaunchEnvelope {
            envelope_version: ENVELOPE_VERSION,
            invocation_id: "inv-abc".to_string(),
            thread_id: "T-test".to_string(),
            roots: EnvelopeRoots {
                project_root: PathBuf::from("/project"),
                user_root: None,
                system_roots: vec![],
            },
            target: EnvelopeTarget {
                item_id: "directive:my/agent".to_string(),
                kind: "directive".to_string(),
                path: ".ai/directives/my/agent.directive.md".to_string(),
                digest: "sha256:abc".to_string(),
            },
            request: EnvelopeRequest {
                inputs: serde_json::json!({}),
                previous_thread_id: None,
                parent_thread_id: None,
                parent_capabilities: None,
                depth: 0,
            },
            policy: EnvelopePolicy {
                effective_caps: vec!["rye.execute.tool.*".to_string()],
                hard_limits: HardLimits::default(),
            },
            callback: EnvelopeCallback {
                socket_path: PathBuf::from("/tmp/ryeosd.sock"),
                token: "cbt-test".to_string(),
                allowed_primaries: vec!["execute".to_string()],
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: LaunchEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id, "T-test");
        assert_eq!(parsed.envelope_version, ENVELOPE_VERSION);
    }

    #[test]
    fn runtime_result_round_trip() {
        let result = RuntimeResult {
            success: true,
            status: "completed".to_string(),
            thread_id: "T-test".to_string(),
            result: Some("Done".to_string()),
            outputs: serde_json::json!({"answer": "42"}),
            cost: Some(RuntimeCost {
                input_tokens: 100,
                output_tokens: 50,
                total_usd: 0.01,
            }),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: RuntimeResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.cost.unwrap().input_tokens, 100);
    }

    #[test]
    fn hard_limits_defaults() {
        let limits = HardLimits::default();
        assert_eq!(limits.turns, 25);
        assert_eq!(limits.tokens, 200_000);
        assert_eq!(limits.spawns, 10);
    }
}
