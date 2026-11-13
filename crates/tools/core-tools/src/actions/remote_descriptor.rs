//! Remote descriptor v2: a node-signed trust pin a remote operator hands to
//! other operators, and the exporter that produces it.
//!
//! The descriptor binds the remote's name, URL, site identity, vault
//! fingerprint and node key together, and is signed by that node key. The
//! signature is a self-signature: it proves the producer held the key it
//! pins and makes the bound fields tamper-evident. The out-of-band check
//! that the key is the right one remains the fingerprint.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::actions::hosted_policy::{LoadedHostedNodePolicy, load_hosted_policy};

/// The only descriptor version this build reads and writes.
pub const REMOTE_DESCRIPTOR_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRemoteDescriptorParams {
    /// Node system space selected by the signed Tool contract.
    #[serde(default)]
    pub system_space_dir: Option<String>,
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
    pub descriptor: RemoteDescriptor,
    /// The signed descriptor document, exactly as written to `path`.
    pub yaml: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// The one descriptor type, read and written by every party.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteDescriptor {
    pub version: u32,
    pub name: String,
    pub url: String,
    /// The remote node's site identity (`site:<fingerprint>`).
    pub site_id: String,
    /// Fingerprint of the remote node's vault public key.
    pub vault_fingerprint: String,
    pub node: RemoteDescriptorNode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteDescriptorNode {
    /// Remote node Ed25519 public key in `ed25519:<base64>` form.
    pub public_key: String,
    /// Fingerprint of `public_key`.
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionDescriptor {
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub name: String,
}

/// Why a descriptor document was refused. Each variant has a stable code and
/// names the cure.
#[derive(Debug)]
pub enum RemoteDescriptorError {
    /// No signature line: a version 1 descriptor or a hand-written file.
    Unsigned,
    /// The document is not a version 2 descriptor.
    Malformed(String),
    /// Signed by a key other than the node key the descriptor pins.
    WrongSigner { signer: String, pinned: String },
    /// The content or signature does not verify under the pinned key.
    Tampered(String),
}

impl RemoteDescriptorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsigned => "remote_descriptor_unsigned",
            Self::Malformed(_) => "remote_descriptor_malformed",
            Self::WrongSigner { .. } => "remote_descriptor_wrong_signer",
            Self::Tampered(_) => "remote_descriptor_tampered",
        }
    }
}

impl std::fmt::Display for RemoteDescriptorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const CURE: &str = "Ask the remote operator to re-export it with \
            `ryeos remote-descriptor` on the remote node";
        match self {
            Self::Unsigned => write!(
                f,
                "remote_descriptor_unsigned: the descriptor carries no `# ryeos:signed:` line; \
                 only signed version {REMOTE_DESCRIPTOR_VERSION} descriptors are accepted. {CURE}"
            ),
            Self::Malformed(reason) => write!(
                f,
                "remote_descriptor_malformed: the descriptor is not a valid version \
                 {REMOTE_DESCRIPTOR_VERSION} descriptor: {reason}. {CURE}"
            ),
            Self::WrongSigner { signer, pinned } => write!(
                f,
                "remote_descriptor_wrong_signer: the descriptor pins node key {pinned} but is \
                 signed by {signer}. A descriptor must be signed by the node key it pins. {CURE}"
            ),
            Self::Tampered(reason) => write!(
                f,
                "remote_descriptor_tampered: the descriptor {reason}. It was edited after \
                 signing or is damaged. {CURE}"
            ),
        }
    }
}

impl std::error::Error for RemoteDescriptorError {}

impl RemoteDescriptor {
    /// Render the signed document.
    pub fn render_signed(&self, node_key: &SigningKey) -> Result<String> {
        let body = serde_yaml::to_string(self).context("failed to serialize descriptor YAML")?;
        Ok(lillux::signature::sign_content(&body, node_key, "#", None))
    }

    /// Parse a signed descriptor and verify its signature under the node key
    /// the descriptor itself pins.
    pub fn parse_signed(text: &str) -> Result<Self, RemoteDescriptorError> {
        if !text.lines().any(|line| line.contains("ryeos:signed:")) {
            return Err(RemoteDescriptorError::Unsigned);
        }
        let descriptor: RemoteDescriptor = serde_yaml::from_str(text)
            .map_err(|e| RemoteDescriptorError::Malformed(e.to_string()))?;
        if descriptor.version != REMOTE_DESCRIPTOR_VERSION {
            return Err(RemoteDescriptorError::Malformed(format!(
                "version {} is not supported",
                descriptor.version
            )));
        }
        let key = decode_node_key(&descriptor.node.public_key)
            .map_err(|e| RemoteDescriptorError::Malformed(format!("node.public_key: {e}")))?;
        let actual = lillux::crypto::fingerprint(&key);
        if actual != descriptor.node.fingerprint {
            return Err(RemoteDescriptorError::Malformed(format!(
                "node.fingerprint {} does not match node.public_key (fingerprint {actual})",
                descriptor.node.fingerprint
            )));
        }
        match ryeos_engine::trust::verify_node_signed_text(
            std::path::Path::new("remote-descriptor"),
            text.as_bytes(),
            &key,
        ) {
            Ok(_) => Ok(descriptor),
            Err(ryeos_engine::trust::TrustError::RecordUnsigned { .. }) => {
                Err(RemoteDescriptorError::Unsigned)
            }
            Err(ryeos_engine::trust::TrustError::RecordWrongSigner { signer, node, .. }) => {
                Err(RemoteDescriptorError::WrongSigner {
                    signer,
                    pinned: node,
                })
            }
            Err(other) => Err(RemoteDescriptorError::Tampered(
                tamper_reason(&other).to_string(),
            )),
        }
    }
}

fn tamper_reason(error: &ryeos_engine::trust::TrustError) -> &str {
    match error {
        ryeos_engine::trust::TrustError::RecordTampered { reason, .. } => reason.as_str(),
        _ => "does not verify under its pinned node key",
    }
}

fn decode_node_key(input: &str) -> Result<VerifyingKey> {
    let b64 = input
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("must start with ed25519:"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("invalid base64 ed25519 public key")?;
    VerifyingKey::from_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("ed25519 public key must be 32 bytes"))?,
    )
    .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))
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

    let app_root = resolve_app_root(params.system_space_dir)?;
    let runtime_root = ryeos_engine::roots::RuntimeRoot::new(app_root.clone());
    let identity_path = ryeos_engine::trust::node_public_identity_path(&app_root);
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
    let node_identity =
        ryeos_node_runtime::identity::NodeIdentity::load(&runtime_root.node_signing_key_path())
            .context("load the node key to sign the descriptor")?;
    if node_identity.fingerprint() != fingerprint {
        bail!(
            "node key {} does not match public identity {}",
            node_identity.fingerprint(),
            fingerprint
        );
    }
    let vault_public_key_path = runtime_root.node().join("vault").join("public_key.pem");
    let vault_fingerprint = lillux::vault::read_public_key(&vault_public_key_path)
        .with_context(|| {
            format!(
                "vault public key not found at {} — run `ryeos init` first",
                vault_public_key_path.display()
            )
        })?
        .fingerprint();

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

    let descriptor = RemoteDescriptor {
        version: REMOTE_DESCRIPTOR_VERSION,
        name: name.to_string(),
        url,
        site_id: node_identity.site_id(),
        vault_fingerprint,
        node: RemoteDescriptorNode {
            public_key: identity.signing_key,
            fingerprint,
        },
        capabilities,
        admission,
        provider,
    };
    let yaml = descriptor.render_signed(node_identity.signing_key())?;

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
        _bootstrap_key: lillux::crypto::SigningKey,
    }

    impl HostedPolicyFixture {
        fn new(root: &std::path::Path) -> Self {
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            write_node_bootstrap(root, &key);
            Self {
                _bootstrap_key: key,
            }
        }
    }

    fn write_hosted_policy_with_choices(
        app_root: &std::path::Path,
        admission_enabled: bool,
        allow_loopback_http: bool,
    ) {
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &app_root.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let path = app_root.join(".ai/node/policies/hosted.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ttl = if admission_enabled {
            "admission_token_ttl_secs: 600\n"
        } else {
            Default::default()
        };
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

    fn write_node_bootstrap(app_root: &std::path::Path, fallback_key: &lillux::crypto::SigningKey) {
        let identity_dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let identity_path = identity_dir.join("private_key.pem");
        let node_identity = if identity_path.exists() {
            ryeos_node_runtime::identity::NodeIdentity::load(&identity_path).unwrap()
        } else {
            std::fs::write(
                &identity_path,
                fallback_key
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
            ryeos_node_runtime::identity::NodeIdentity::load(&identity_path).unwrap()
        };
        node_identity
            .write_public_identity(&ryeos_engine::trust::node_public_identity_path(app_root))
            .unwrap();

        crate::actions::hosted_policy::write_required_non_hosted_test_policies(
            app_root,
            node_identity.signing_key(),
        );
        let vault_dir = app_root.join(".ai/node/vault");
        std::fs::create_dir_all(&vault_dir).unwrap();
        lillux::vault::write_public_key(
            &vault_dir.join("public_key.pem"),
            &lillux::vault::VaultSecretKey::generate().public_key(),
        )
        .unwrap();
    }

    fn exported_descriptor(tmp: &std::path::Path) -> ExportRemoteDescriptorResult {
        let identity_path = tmp.join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp);
        write_hosted_policy(tmp);
        run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.to_string_lossy().to_string()),
            name: "hosted-prod".into(),
            url: "https://node.example.com".into(),
            capabilities: vec![],
            admission_mode: None,
            provider_name: None,
            output: None,
        })
        .unwrap()
    }

    #[test]
    fn descriptor_v2_signature_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let result = exported_descriptor(tmp.path());
        let parsed = RemoteDescriptor::parse_signed(&result.yaml).unwrap();
        assert_eq!(parsed, result.descriptor);
        assert_eq!(parsed.version, REMOTE_DESCRIPTOR_VERSION);
        assert!(parsed.site_id.starts_with("site:"));
        assert!(!parsed.vault_fingerprint.is_empty());
    }

    #[test]
    fn descriptor_v1_is_refused() {
        let v1 = "version: 1\nname: x\nurl: https://node.example.com\nnode:\n  public_key: a\n  fingerprint: b\n";
        let err = RemoteDescriptor::parse_signed(v1).unwrap_err();
        assert_eq!(err.code(), "remote_descriptor_unsigned");
        assert!(err.to_string().contains("ryeos remote-descriptor"));
    }

    #[test]
    fn descriptor_signed_by_other_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let result = exported_descriptor(tmp.path());
        let other = lillux::crypto::SigningKey::generate(&mut OsRng);
        let forged = result.descriptor.render_signed(&other).unwrap();
        let err = RemoteDescriptor::parse_signed(&forged).unwrap_err();
        assert_eq!(err.code(), "remote_descriptor_wrong_signer");
    }

    #[test]
    fn edited_descriptor_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let result = exported_descriptor(tmp.path());
        let edited = result
            .yaml
            .replace("https://node.example.com", "https://evil.example.com");
        assert_ne!(edited, result.yaml);
        let err = RemoteDescriptor::parse_signed(&edited).unwrap_err();
        assert_eq!(err.code(), "remote_descriptor_tampered");
    }

    #[test]
    fn remote_descriptor_advertises_enabled_admission_without_inventing_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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

        assert!(result.descriptor.capabilities.is_empty());
        assert_eq!(result.descriptor.admission.unwrap().mode, "one_time_token");
    }

    #[test]
    fn remote_descriptor_rejects_public_http_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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
            err.to_string().contains("requires HTTPS descriptor URLs"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_allows_loopback_http_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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
            err.to_string().contains("requires HTTPS descriptor URLs"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_rejects_admission_mode_override_under_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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
            err.to_string().contains("supports only one_time_token"),
            "got: {err:#}"
        );
    }

    #[test]
    fn remote_descriptor_keeps_capabilities_as_explicit_informational_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
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
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path());

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

    #[test]
    fn remote_descriptor_omits_admission_when_explicitly_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let identity_path = tmp.path().join(".ai/node/identity/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy_with_choices(tmp.path(), false, true);

        let result = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
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
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&identity_path).unwrap();
        identity
            .write_public_identity(&tmp.path().join(".ai/node/identity/public-identity.json"))
            .unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy_with_choices(tmp.path(), true, false);

        let err = run_export_remote_descriptor(ExportRemoteDescriptorParams {
            system_space_dir: Some(tmp.path().to_string_lossy().to_string()),
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
