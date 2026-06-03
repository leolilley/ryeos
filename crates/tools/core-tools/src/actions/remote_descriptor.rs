//! Export a remote descriptor trust pin from local node identity.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::actions::hosted_policy::load_hosted_policy;

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
    #[serde(default, rename = "kind")]
    _kind: Option<String>,
    #[serde(default, rename = "created_at")]
    _created_at: Option<String>,
    #[serde(default, rename = "_signature")]
    _signature: Option<serde_json::Value>,
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
    let actual_fingerprint = fingerprint_for_ed25519_key(&identity.signing_key)
        .context("invalid public identity signing_key")?;
    if fingerprint != actual_fingerprint {
        bail!(
            "public identity principal_id {} does not match signing_key fingerprint {}",
            identity.principal_id,
            actual_fingerprint
        );
    }

    let hosted_policy = load_hosted_policy(&system_space_dir)?;
    if let Some(policy) = &hosted_policy {
        enforce_hosted_transport_policy(&url, policy)?;
    }

    let requested_admission_mode = params
        .admission_mode
        .as_deref()
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(String::from);

    let mut capabilities = params
        .capabilities
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if let Some(policy) = &hosted_policy {
        if capabilities.is_empty() {
            capabilities = policy.descriptor.advertised_capabilities.clone();
        } else {
            for capability in &capabilities {
                if !policy
                    .descriptor
                    .advertised_capabilities
                    .contains(capability)
                {
                    bail!(
                        "capability '{}' is not advertised by hosted-node policy from {}",
                        capability,
                        policy.source_file.display()
                    );
                }
            }
        }
    }
    capabilities.sort();
    capabilities.dedup();

    let admission = if let Some(policy) = &hosted_policy {
        if let Some(mode) = &requested_admission_mode {
            if mode != &policy.admission.mode {
                bail!(
                    "admission_mode '{}' conflicts with hosted-node policy mode '{}' from {}",
                    mode,
                    policy.admission.mode,
                    policy.source_file.display()
                );
            }
        }
        policy.admission.mode.clone()
    } else {
        requested_admission_mode.unwrap_or_else(|| "one_time_token".to_string())
    };
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

fn enforce_hosted_transport_policy(
    url: &str,
    policy: &ryeos_app::node_config::sections::hosted_node::HostedNodePolicyRecord,
) -> Result<()> {
    if !policy.transport.public_https_required || url.starts_with("https://") {
        return Ok(());
    }
    if policy.transport.loopback_http_allowed && is_loopback_http_url(url) {
        return Ok(());
    }
    bail!(
        "hosted-node policy requires public HTTPS descriptor URLs; got '{}' from {}",
        url,
        policy.source_file.display()
    )
}

fn is_loopback_http_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit_once('@')
        .map(|(_, authority)| authority)
        .unwrap_or(rest);
    let host = authority
        .strip_prefix('[')
        .and_then(|authority| authority.split_once(']').map(|(host, _)| host))
        .unwrap_or_else(|| authority.split(':').next().unwrap_or(authority));
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

fn fingerprint_for_ed25519_key(key: &str) -> Result<String> {
    let b64 = key
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("signing_key must start with ed25519:"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid base64 ed25519 public key")?;
    let key = VerifyingKey::from_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("ed25519 public key must be 32 bytes"))?,
    )
    .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))?;
    Ok(lillux::crypto::fingerprint(&key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct HostedPolicyFixture {
        _env_guard: MutexGuard<'static, ()>,
        _user: std::path::PathBuf,
        key: lillux::crypto::SigningKey,
    }

    impl HostedPolicyFixture {
        fn new(root: &std::path::Path) -> Self {
            let env_guard = ENV_MUTEX.lock().unwrap();
            let user = root.join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            std::env::set_var("USER_SPACE", &user);
            Self {
                _env_guard: env_guard,
                _user: user,
                key,
            }
        }
    }

    impl Drop for HostedPolicyFixture {
        fn drop(&mut self) {
            std::env::remove_var("USER_SPACE");
        }
    }

    fn write_hosted_policy(system_space_dir: &std::path::Path, key: &lillux::crypto::SigningKey) {
        let path = system_space_dir.join(".ai/node/hosted/policy.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = r#"
category: "hosted"
section: "hosted"
version: "0.1.0"
schema_version: "1.0.0"
description: "test hosted policy"
transport:
  public_https_required: true
  loopback_http_allowed: true
admission:
  mode: "one_time_token"
  token_ttl_secs: 600
  reject_wildcard_scopes: true
  token_delivery: "out_of_band"
descriptor:
  require_live_identity_match: true
  advertised_capabilities:
    - remote-execute
    - bundle-install
authorization:
  authority: "target_node_authorized_keys"
  central_bearer_tokens_allowed: false
  implicit_cross_node_authority_allowed: false
operations:
  audit_admission_events: true
  audit_grant_changes: true
  prefer_isolated_node_per_principal: true
  shared_daemon_multitenancy_enabled: false
"#;
        std::fs::write(path, lillux::signature::sign_content(body, key, "#", None)).unwrap();
    }

    #[test]
    fn remote_descriptor_inherits_hosted_policy_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert_eq!(
            result.descriptor.capabilities,
            vec!["bundle-install".to_string(), "remote-execute".to_string()]
        );
        assert_eq!(result.descriptor.admission.unwrap().mode, "one_time_token");
    }

    #[test]
    fn remote_descriptor_rejects_public_http_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "http://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("requires public HTTPS"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_allows_loopback_http_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-local".into(),
            url: "http://127.0.0.1:8000".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert_eq!(result.descriptor.url, "http://127.0.0.1:8000");
    }

    #[test]
    fn remote_descriptor_allows_ipv6_loopback_http_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-local".into(),
            url: "http://[::1]:8000".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert_eq!(result.descriptor.url, "http://[::1]:8000");
    }

    #[test]
    fn remote_descriptor_rejects_loopback_looking_hostname_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "http://127.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("requires public HTTPS"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_rejects_admission_mode_override_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: Some("provider_session".into()),
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("conflicts with hosted-node policy"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_rejects_capability_outside_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec!["provider-dashboard".into()],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("is not advertised by hosted-node policy"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_allows_capability_subset_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), &fixture.key);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec!["remote-execute".into()],
            admission_mode: Some("one_time_token".into()),
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert_eq!(result.descriptor.capabilities, vec!["remote-execute"]);
        assert_eq!(result.descriptor.admission.unwrap().mode, "one_time_token");
    }
}
