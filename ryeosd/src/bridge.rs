use anyhow::{Context, Result};
use pyo3::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct BridgeStatus {
    pub initialized: bool,
    pub python_version: String,
}

#[derive(Debug, Clone)]
pub struct PythonBridge {
    python_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeBridgeConfig {
    pub socket_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionRequest {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub project_path: String,
    pub parameters: Value,
    pub requested_by: Option<String>,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub continuation_from_id: Option<String>,
    pub model: Option<String>,
    pub runtime: RuntimeBridgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionArtifact {
    pub artifact_type: String,
    pub uri: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCost {
    #[serde(default)]
    pub turns: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub spend: f64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletion {
    pub status: String,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
    #[serde(default)]
    pub final_cost: Option<FinalCost>,
    #[serde(default)]
    pub continuation_request: Option<Value>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

impl PythonBridge {
    pub fn initialize() -> Result<Self> {
        Python::initialize();

        Python::attach(|py| -> PyResult<()> {
            let sys = py.import("sys")?;
            let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
            let paths = [
                repo_root.join("ryeos"),
                repo_root.join("ryeos/bundles/standard"),
                repo_root.join("ryeos/bundles/code"),
                repo_root.join("ryeos/bundles/core"),
                repo_root.join("ryeos/bundles/email"),
                repo_root.join("ryeos/bundles/web"),
            ];
            for path in paths {
                if path.exists() {
                    sys.getattr("path")?
                        .call_method1("insert", (0, path.display().to_string()))?;
                }
            }
            py.import("importlib")?.call_method0("invalidate_caches")?;
            Ok(())
        })
        .context("failed to bootstrap Python import paths")?;

        let python_version = Python::attach(|py| -> PyResult<String> {
            let sys = py.import("sys")?;
            sys.getattr("version")?.extract()
        })
        .context("failed to initialize embedded Python")?;

        Ok(Self { python_version })
    }

    pub fn status(&self) -> BridgeStatus {
        BridgeStatus {
            initialized: true,
            python_version: self.python_version.clone(),
        }
    }

    pub fn execute_item(&self, request: &ExecutionRequest) -> Result<ExecutionCompletion> {
        let request_json =
            serde_json::to_string(request).context("failed to encode execution request")?;

        Python::attach(|py| -> PyResult<String> {
            let json = py.import("json")?;
            let bridge = py.import("rye.runtime.daemon_bridge")?;
            let execute = bridge.getattr("execute_item")?;
            let py_request = json.call_method1("loads", (request_json.as_str(),))?;
            let py_result = execute.call1((py_request,))?;
            json.call_method1("dumps", (py_result,))?.extract()
        })
        .context("failed to call Python execute_item")
        .and_then(|response_json| {
            serde_json::from_str(&response_json)
                .context("failed to decode Python execution completion")
        })
    }
}
