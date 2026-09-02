//! Export a remote descriptor trust pin from local node identity.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::actions::hosted_policy::{LoadedHostedNodePolicy, load_hosted_policy};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRemoteDescriptorParams {
    /// App root directory for the node being described.
    #[serde(default)]
    pub app_root: Option<String>,
    /// Name callers should use for the remote.
    pub name: String,
    /// Public HTTPS URL callers should use to reach the node.
    pub url: String,
    /// Informational capability labels advertised by this node/provider.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Optional assertion of the code-owned admission mode. The node policy
    /// controls whether admission is advertised.
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

    let app_root = resolve_app_root(params.app_root)?;
    let identity_path = app_root
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

    let hosted_policy = load_hosted_policy(&app_root)?;
    enforce_hosted_transport_policy(&url, &hosted_policy)?;

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
    capabilities.sort();
    capabilities.dedup();

    let admission = match (
        hosted_policy.admission_enabled,
        requested_admission_mode.as_deref(),
    ) {
        (true, None | Some("one_time_token")) => Some(AdmissionDescriptor {
            mode: "one_time_token".to_string(),
        }),
        (true, Some(_)) => {
            bail!(
                "hosted-node policy from {} supports only one_time_token admission",
                hosted_policy.source_file.display()
            )
        }
        (false, None) => None,
        (false, Some(_)) => {
            bail!("admission_mode cannot be advertised while hosted-node admission is disabled")
        }
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
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create descriptor output dir {}",
                    parent.display()
                )
            })?;
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

fn resolve_app_root(opt: Option<String>) -> Result<PathBuf> {
    if let Some(path) = opt {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("RYEOS_APP_ROOT") {
        return Ok(PathBuf::from(path));
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .ok_or_else(|| anyhow::anyhow!("could not determine app rootectory"))
}

fn enforce_hosted_transport_policy(url: &str, policy: &LoadedHostedNodePolicy) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if policy.allow_loopback_http && is_loopback_http_url(url) {
        return Ok(());
    }
    bail!(
        "hosted-node policy requires HTTPS descriptor URLs except for explicitly allowed loopback HTTP; policy source {}",
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
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;
    struct HostedPolicyFixture {
        _user: std::path::PathBuf,
        _bootstrap_key: lillux::crypto::SigningKey,
    }

    impl HostedPolicyFixture {
        fn new(root: &std::path::Path) -> Self {
            let user = root.join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            write_node_bootstrap(root, &trust_dir, &key);
            Self {
                _user: user,
                _bootstrap_key: key,
            }
        }
    }

    fn write_hosted_policy_with_choices(
        app_root: &std::path::Path,
        admission_enabled: bool,
        allow_loopback_http: bool,
    ) {
        let identity = ryeos_app::identity::NodeIdentity::load(
            &app_root.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let path = app_root.join(".ai/node/policies/hosted.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ttl = admission_enabled
            .then_some("admission_token_ttl_secs: 600\n")
            .unwrap_or_default();
        let body = format!(
            r#"
schema: 1
admission_enabled: {admission_enabled}
{ttl}allow_loopback_http: {allow_loopback_http}
"#
        );
        std::fs::write(
            path,
            lillux::signature::sign_content(&body, identity.signing_key(), "#", None),
        )
        .unwrap();
    }

    fn write_hosted_policy(app_root: &std::path::Path) {
        write_hosted_policy_with_choices(app_root, true, true);
    }

    fn write_node_bootstrap(
        app_root: &std::path::Path,
        trust_dir: &std::path::Path,
        fallback_key: &lillux::crypto::SigningKey,
    ) {
        let app_trust_dir = app_root.join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&app_trust_dir).unwrap();
        ryeos_engine::trust::pin_key(&fallback_key.verifying_key(), "test", &app_trust_dir, None)
            .unwrap();

        let identity_dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let identity_path = identity_dir.join("private_key.pem");
        let node_identity = if identity_path.exists() {
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        } else {
            std::fs::write(
                &identity_path,
                fallback_key
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        };
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", trust_dir, None)
            .unwrap();
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", &app_trust_dir, None)
            .unwrap();

        let policy_dir = app_root.join(".ai/node/command_registration");
        std::fs::create_dir_all(&policy_dir).unwrap();
        let policy = r#"claim_rules:
  - claim:
      kind: command.root
      value: execute
    required_caps: []
system_source_caps:
  - ryeos.register.command.root.execute
"#;
        std::fs::write(
            policy_dir.join("default.yaml"),
            lillux::signature::sign_content(policy, node_identity.signing_key(), "#", None),
        )
        .unwrap();
        crate::actions::hosted_policy::write_required_non_hosted_test_policies(
            app_root,
            node_identity.signing_key(),
        );
    }

    #[test]
    fn remote_descriptor_advertises_enabled_admission_without_inventing_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert!(result.descriptor.capabilities.is_empty());
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
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "http://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("requires HTTPS descriptor URLs"),
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
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
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
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
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
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "http://127.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("requires HTTPS descriptor URLs"),
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
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: Some("provider_session".into()),
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("supports only one_time_token"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_keeps_capabilities_as_explicit_informational_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec!["provider-dashboard".into()],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert_eq!(result.descriptor.capabilities, vec!["provider-dashboard"]);
    }

    #[test]
    fn remote_descriptor_allows_capability_subset_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
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

    #[test]
    fn remote_descriptor_omits_admission_when_explicitly_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy_with_choices(tmp.path(), false, true);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-disabled".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap();

        assert!(result.descriptor.admission.is_none());
    }

    #[test]
    fn remote_descriptor_rejects_loopback_http_when_operator_disallows_it() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_app::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy_with_choices(tmp.path(), true, false);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            app_root: Some(tmp.path().to_string_lossy().to_string()),
            name: "hosted-local".into(),
            url: "http://127.0.0.1:8000".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("requires HTTPS descriptor URLs"),
            "got: {err:#}"
        );
    }
}
