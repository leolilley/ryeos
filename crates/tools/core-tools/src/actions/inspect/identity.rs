//! `ryeos-core-tools identity` — return the node's public identity document.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityParams {
    #[serde(default)]
    /// Node system space selected by the signed Tool contract.  Keep this
    /// distinct from a CLI implementation flag: Tool payloads are portable
    /// data and must name the same field the descriptor admits.
    pub system_space_dir: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
}

pub fn run_identity(params: IdentityParams) -> Result<Value> {
    let app_root = match params.system_space_dir {
        Some(ref p) => std::path::PathBuf::from(p),
        None => {
            // 1. RYEOS_APP_ROOT (set by the daemon for subprocess tools)
            // 2. XDG data dir / ryeos
            if let Ok(env_dir) = std::env::var("RYEOS_APP_ROOT") {
                std::path::PathBuf::from(env_dir)
            } else {
                dirs::data_dir()
                    .map(|d| d.join("ryeos"))
                    .ok_or_else(|| anyhow!("could not determine app root directory (no system_space_dir param, no RYEOS_APP_ROOT env, no XDG data dir)"))?
            }
        }
    };

    let identity_path = app_root
        .join(".ai")
        .join("node")
        .join("identity")
        .join("public-identity.json");

    let data = std::fs::read(&identity_path).with_context(|| {
        format!(
            "public identity not found at {} — run 'ryeos init' first",
            identity_path.display()
        )
    })?;
    let doc: Value =
        serde_json::from_slice(&data).context("failed to parse public identity document")?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::IdentityParams;

    #[test]
    fn signed_tool_contract_uses_system_space_dir() {
        let params: IdentityParams = serde_json::from_value(serde_json::json!({
            "system_space_dir": "/node",
            "project_path": "/project"
        }))
        .expect("declared Tool payload must decode");
        assert_eq!(params.system_space_dir.as_deref(), Some("/node"));

        let err = serde_json::from_value::<IdentityParams>(serde_json::json!({
            "app_root": "/node"
        }))
        .expect_err("retired undeclared Tool field must not decode");
        assert!(err.to_string().contains("app_root"));
    }
}
