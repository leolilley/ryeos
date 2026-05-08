//! `ryos-core-tools identity` — return the node's public identity document.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityParams {
    #[serde(default)]
    pub system_space_dir: Option<String>,
}

pub fn run_identity(params: IdentityParams) -> Result<Value> {
    let system_space_dir = match params.system_space_dir {
        Some(ref p) => std::path::PathBuf::from(p),
        None => {
            // 1. RYEOS_SYSTEM_SPACE_DIR (set by the daemon for subprocess tools)
            // 2. XDG data dir / ryeos
            if let Ok(env_dir) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
                std::path::PathBuf::from(env_dir)
            } else {
                dirs::data_dir()
                    .map(|d| d.join("ryeos"))
                    .ok_or_else(|| anyhow!("could not determine system space directory (no system_space_dir param, no RYEOS_SYSTEM_SPACE_DIR env, no XDG data dir)"))?
            }
        }
    };

    let identity_path = system_space_dir
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
    let doc: Value = serde_json::from_slice(&data).context("failed to parse public identity document")?;
    Ok(doc)
}
