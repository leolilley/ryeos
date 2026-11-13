//! Remote descriptor v2: a node-signed trust pin a remote operator hands to
//! other operators, and the in-daemon builder that produces it.
//!
//! The descriptor binds the remote's name, URL, site identity, vault
//! fingerprint and node key together, and is signed by that node key. The
//! signature is a self-signature: it proves the producer held the key it
//! pins and makes the bound fields tamper-evident. The out-of-band check
//! that the key is the right one remains the fingerprint. The node key never
//! leaves the daemon: only the daemon builds and signs descriptors.

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::{SigningKey, VerifyingKey};
use ryeos_core_tools::actions::hosted_policy::LoadedHostedNodePolicy;
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};

/// The only descriptor version this build reads and writes.
pub const REMOTE_DESCRIPTOR_VERSION: u32 = 2;

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

/// What the operator asks a descriptor to say.
#[derive(Debug, Default)]
pub struct DescriptorRequest {
    pub name: String,
    pub url: String,
    pub capabilities: Vec<String>,
    pub admission_mode: Option<String>,
    pub provider_name: Option<String>,
}

/// Build and sign the descriptor of this node. The hosted-node policy decides
/// which URLs and admission modes may be advertised.
pub fn build_signed_descriptor(
    request: DescriptorRequest,
    identity: &NodeIdentity,
    vault_fingerprint: &str,
    hosted_policy: &LoadedHostedNodePolicy,
) -> Result<(RemoteDescriptor, String)> {
    let name = request.name.trim();
    if name.is_empty() {
        bail!("name must not be empty");
    }
    let url = request.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        bail!("url must not be empty");
    }
    if vault_fingerprint.trim().is_empty() {
        bail!("this node has no vault public key; run `ryeos init` first");
    }
    enforce_hosted_transport_policy(&url, hosted_policy)?;

    let requested_admission_mode = request
        .admission_mode
        .as_deref()
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(String::from);
    let mut capabilities = request
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
        (true, Some(_)) => bail!(
            "hosted-node policy from {} supports only one_time_token admission",
            hosted_policy.source_file.display()
        ),
        (false, None) => None,
        (false, Some(_)) => {
            bail!("admission_mode cannot be advertised while hosted-node admission is disabled")
        }
    };
    let provider = request.provider_name.and_then(|name| {
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(ProviderDescriptor { name })
    });
    let descriptor = RemoteDescriptor {
        version: REMOTE_DESCRIPTOR_VERSION,
        name: name.to_string(),
        url,
        site_id: identity.site_id(),
        vault_fingerprint: vault_fingerprint.to_string(),
        node: RemoteDescriptorNode {
            public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD
                    .encode(identity.verifying_key().as_bytes())
            ),
            fingerprint: identity.fingerprint().to_string(),
        },
        capabilities,
        admission,
        provider,
    };
    let yaml = descriptor.render_signed(identity.signing_key())?;
    Ok((descriptor, yaml))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn identity() -> (tempfile::TempDir, NodeIdentity) {
        let dir = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::create(&dir.path().join("node-key.pem")).unwrap();
        (dir, identity)
    }

    fn policy(admission: bool, loopback: bool) -> LoadedHostedNodePolicy {
        LoadedHostedNodePolicy {
            policy: serde_json::from_value(serde_json::json!({
                "schema": 1,
                "admission_enabled": admission,
                "admission_token_ttl_secs": if admission { Some(600) } else { None },
                "allow_loopback_http": loopback,
            }))
            .expect("hosted policy shape"),
            source_file: std::path::PathBuf::from("hosted.yaml"),
        }
    }

    fn request(url: &str) -> DescriptorRequest {
        DescriptorRequest {
            name: "hosted-prod".into(),
            url: url.into(),
            ..Default::default()
        }
    }

    fn build(url: &str, identity: &NodeIdentity) -> (RemoteDescriptor, String) {
        build_signed_descriptor(request(url), identity, "sha256:vault", &policy(true, true))
            .unwrap()
    }

    #[test]
    fn built_descriptor_is_signed_by_the_node_key_and_verifies() {
        let (_dir, identity) = identity();
        let (descriptor, yaml) = build("https://node.example.com/", &identity);
        assert_eq!(RemoteDescriptor::parse_signed(&yaml).unwrap(), descriptor);
        assert_eq!(descriptor.url, "https://node.example.com");
        assert_eq!(descriptor.site_id, identity.site_id());
        assert_eq!(descriptor.admission.unwrap().mode, "one_time_token");
    }

    #[test]
    fn admission_is_omitted_when_the_policy_disables_it() {
        let (_dir, identity) = identity();
        let (descriptor, _) = build_signed_descriptor(
            request("https://node.example.com"),
            &identity,
            "sha256:vault",
            &policy(false, true),
        )
        .unwrap();
        assert!(descriptor.admission.is_none());
    }

    #[test]
    fn hosted_policy_gates_urls_and_admission_modes() {
        let (_dir, identity) = identity();
        let refuse = |request: DescriptorRequest, policy: LoadedHostedNodePolicy| {
            build_signed_descriptor(request, &identity, "sha256:vault", &policy)
                .unwrap_err()
                .to_string()
        };
        for (url, policy) in [
            ("http://node.example.com", policy(true, true)),
            ("http://127.example.com", policy(true, true)),
            ("http://127.0.0.1:8000", policy(true, false)),
        ] {
            assert!(refuse(request(url), policy).contains("requires HTTPS descriptor URLs"));
        }
        for url in ["http://127.0.0.1:8000", "http://[::1]:8000"] {
            build(url, &identity);
        }
        let mut other_mode = request("https://node.example.com");
        other_mode.admission_mode = Some("provider_session".into());
        assert!(refuse(other_mode, policy(true, true)).contains("supports only one_time_token"));
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
        let (_dir, identity) = identity();
        let (descriptor, _) = build("https://node.example.com", &identity);
        let forged = descriptor
            .render_signed(&SigningKey::generate(&mut OsRng))
            .unwrap();
        let err = RemoteDescriptor::parse_signed(&forged).unwrap_err();
        assert_eq!(err.code(), "remote_descriptor_wrong_signer");
    }

    #[test]
    fn edited_descriptor_is_detected() {
        let (_dir, identity) = identity();
        let (_, yaml) = build("https://node.example.com", &identity);
        let edited = yaml.replace("https://node.example.com", "https://evil.example.com");
        let err = RemoteDescriptor::parse_signed(&edited).unwrap_err();
        assert_eq!(err.code(), "remote_descriptor_tampered");
    }
}
