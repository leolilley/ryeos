//! Export a remote descriptor trust pin from local node identity.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRemoteDescriptorParams {
    /// System space directory for the node being described.
    #[serde(default)]
    pub system_space_dir: Option<String>,
    /// Name callers should use for the remote.
    pub name: String,
    /// Public HTTPS URL callers should use to reach the node.
    pub url: String,
    /// Informational capability labels advertised by this node/provider.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Optional admission mode label. Defaults to `token`.
    #[serde(default)]
    pub admission_mode: Option<String>,
    /// Optional provider/operator label.
    #[serde(default)]
    pub provider_name: Option<String>,
    /// Optional output path. If omitted, only prints the descriptor YAML.
    #[serde(default)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ExportRemoteDescriptorResult {
    pub descriptor: RemoteDescriptorFile,
    pub yaml: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct RemoteDescriptorFile {
    pub version: u32,
    pub name: String,
    pub url: String,
    pub node: RemoteDescriptorNode,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderDescriptor>,
}

#[derive(Debug, Serialize)]
pub struct RemoteDescriptorNode {
    pub public_key: String,
    pub fingerprint: String,
}

#[derive(Debug, Serialize)]
pub struct AdmissionDescriptor {
    pub mode: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderDescriptor {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicIdentityDoc {
    principal_id: String,
    signing_key: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default, rename = "_signature")]
    signature: Option<serde_json::Value>,
}

pub fn run_export_remote_descriptor(
    params: ExportRemoteDescriptorParams,
) -> Result<ExportRemoteDescriptorResult> {
    let name = params.name.trim();
    if name.is_empty() {
        bail!("name must not be empty");
    }
    let url = params.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        bail!("url must not be empty");
    }

    let system_space_dir = resolve_system_space_dir(params.system_space_dir)?;
    let identity_path = system_space_dir
        .join(".ai")
        .join("node")
        .join("identity")
        .join("public-identity.json");
    let identity: PublicIdentityDoc =
        serde_json::from_slice(&std::fs::read(&identity_path).with_context(|| {
            format!(
                "public identity not found at {} — run `ryeos init` first",
                identity_path.display()
            )
        })?)
        .context("failed to parse public identity document")?;
    let fingerprint = identity
        .principal_id
        .strip_prefix("fp:")
        .unwrap_or(identity.principal_id.as_str())
        .to_string();

    let mut capabilities = params
        .capabilities
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    capabilities.sort();
    capabilities.dedup();

    let admission = params
        .admission_mode
        .unwrap_or_else(|| "token".to_string())
        .trim()
        .to_string();
    let admission = if admission.is_empty() {
        None
    } else {
        Some(AdmissionDescriptor { mode: admission })
    };
    let provider = params.provider_name.and_then(|name| {
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(ProviderDescriptor { name })
    });

    let descriptor = RemoteDescriptorFile {
        version: 1,
        name: name.to_string(),
        url,
        node: RemoteDescriptorNode {
            public_key: identity.signing_key,
            fingerprint,
        },
        capabilities,
        admission,
        provider,
    };
    let yaml = serde_yaml::to_string(&descriptor).context("failed to serialize descriptor YAML")?;

    if let Some(path) = params.output {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "failed to create descriptor output dir {}",
                        parent.display()
                    )
                })?;
            }
        }
        std::fs::write(&path, &yaml)
            .with_context(|| format!("failed to write descriptor {}", path.display()))?;
        Ok(ExportRemoteDescriptorResult {
            descriptor,
            yaml,
            path: Some(path),
        })
    } else {
        Ok(ExportRemoteDescriptorResult {
            descriptor,
            yaml,
            path: None,
        })
    }
}

fn resolve_system_space_dir(opt: Option<String>) -> Result<PathBuf> {
    if let Some(path) = opt {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
        return Ok(PathBuf::from(path));
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .ok_or_else(|| anyhow::anyhow!("could not determine system space directory"))
}
