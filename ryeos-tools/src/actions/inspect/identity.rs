//! `rye-inspect identity` — return the node's public identity document.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct IdentityParams {
    #[serde(default)]
    pub state_dir: Option<String>,
}

pub fn run_identity(params: IdentityParams) -> Result<Value> {
    let state_dir = match params.state_dir {
        Some(ref p) => std::path::PathBuf::from(p),
        None => {
            // 1. RYEOS_STATE_DIR (set by the daemon for subprocess tools)
            // 2. XDG state dir / ryeosd
            if let Ok(env_dir) = std::env::var("RYEOS_STATE_DIR") {
                std::path::PathBuf::from(env_dir)
            } else {
                dirs::state_dir()
                    .map(|d| d.join("ryeosd"))
                    .ok_or_else(|| anyhow!("could not determine state directory (no state_dir param, no RYEOS_STATE_DIR env, no XDG state dir)"))?
            }
        }
    };

    let identity_path = state_dir
        .join(".ai")
        .join("node")
        .join("identity")
        .join("public-identity.json");

    let data = std::fs::read(&identity_path).with_context(|| {
        format!(
            "public identity not found at {} — run 'rye daemon init' first",
            identity_path.display()
        )
    })?;
    let doc: Value = serde_json::from_slice(&data).context("failed to parse public identity document")?;
    Ok(doc)
}
