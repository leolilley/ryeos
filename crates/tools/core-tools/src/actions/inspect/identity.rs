//! `ryeos-core-tools identity` — return the node's public identity document.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityParams {
    #[serde(default)]
    pub app_root: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
}

pub fn run_identity(params: IdentityParams) -> Result<Value> {
    let app_root = match params.app_root {
        Some(ref p) => std::path::PathBuf::from(p),
        None => {
            // 1. RYEOS_APP_ROOT (set by the daemon for subprocess tools)
            // 2. XDG data dir / ryeos
            if let Ok(env_dir) = std::env::var("RYEOS_APP_ROOT") {
                std::path::PathBuf::from(env_dir)
            } else {
                dirs::data_dir()
                    .map(|d| d.join("ryeos"))
                    .ok_or_else(|| anyhow!("could not determine app rootectory (no app_root param, no RYEOS_APP_ROOT env, no XDG data dir)"))?
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
