use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::{DecodePrivateKey, EncodePrivateKey};
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct NodeIdentity {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    fingerprint: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureDoc {
    pub signer: String,
    pub sig: String,
    pub signed_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicIdentityDoc {
    pub kind: String,
    pub principal_id: String,
    pub signing_key: String,
    pub created_at: String,
    #[serde(rename = "_signature")]
    pub signature: SignatureDoc,
}

impl NodeIdentity {
    /// Generate a new signing key and persist. Errors if key already exists.
    pub fn create(key_path: &Path) -> Result<Self> {
        if key_path.exists() {
            bail!("signing key already exists at {}", key_path.display());
        }
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let signing_key = SigningKey::generate(&mut OsRng);
        let pem = signing_key
            .to_pkcs8_pem(Default::default())
            .context("failed to serialize signing key")?;
        fs::write(key_path, pem.as_bytes())
            .with_context(|| format!("failed to write signing key {}", key_path.display()))?;
        Self::from_signing_key(signing_key)
    }

    /// Load existing signing key. Errors if missing.
    pub fn load(key_path: &Path) -> Result<Self> {
        let pem = fs::read_to_string(key_path).with_context(|| {
            format!(
                "signing key not found at {} — run 'ryeos daemon init' first",
                key_path.display()
            )
        })?;
        let signing_key = SigningKey::from_pkcs8_pem(&pem)
            .with_context(|| format!("failed to decode signing key {}", key_path.display()))?;
        Self::from_signing_key(signing_key)
    }

    fn from_signing_key(signing_key: SigningKey) -> Result<Self> {
        let verifying_key = signing_key.verifying_key();
        let fingerprint = lillux::sha256_hex(verifying_key.as_bytes());
        Ok(Self {
            signing_key,
            verifying_key,
            fingerprint,
        })
    }

    /// Write a stable public identity document to disk. Uses
    /// `iso8601_now()` for `created_at`/`signed_at`.
    pub fn write_public_identity(&self, path: &Path) -> Result<()> {
        self.write_public_identity_at(path, &lillux::time::iso8601_now())
    }

    /// Like [`write_public_identity`] but takes the timestamp explicitly,
    /// for byte-deterministic test fixtures.
    pub fn write_public_identity_at(&self, path: &Path, now: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let doc = self.build_public_identity_at(now)?;
        let json = serde_json::to_vec_pretty(&doc)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a persisted public identity document.
    pub fn load_public_identity(path: &Path) -> Result<PublicIdentityDoc> {
        let data = fs::read(path).with_context(|| {
            format!(
                "public identity not found at {} — run 'ryeos daemon init' first",
                path.display()
            )
        })?;
        serde_json::from_slice(&data).context("failed to parse public identity document")
    }

    fn build_public_identity_at(&self, now: &str) -> Result<PublicIdentityDoc> {
        let principal_id = format!("fp:{}", self.fingerprint);
        let signing_key_str = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(self.verifying_key.as_bytes())
        );
        let unsigned = serde_json::json!({
            "kind": "identity/v1",
            "principal_id": principal_id,
            "signing_key": signing_key_str,
            "created_at": now,
        });
        let payload = serde_json::to_vec(&unsigned)?;
        let signature: Signature = self.signing_key.sign(&payload);
        Ok(PublicIdentityDoc {
            kind: "identity/v1".to_string(),
            principal_id,
            signing_key: signing_key_str,
            created_at: now.to_string(),
            signature: SignatureDoc {
                signer: format!("fp:{}", self.fingerprint),
                sig: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
                signed_at: now.to_string(),
            },
        })
    }

    pub fn principal_id(&self) -> String {
        format!("fp:{}", self.fingerprint)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    pub fn verify_hash(&self, hash_hex: &str, signature: &Signature) -> Result<()> {
        self.verifying_key
            .verify(hash_hex.as_bytes(), signature)
            .context("signature verification failed")
    }
}

/// Write a node-signed authorized-key TOML entry.
///
/// Used by bootstrap (local operator) and the authorize-key handler
/// (remote delegation). There is exactly one TOML emitter.
///
/// The TOML is signed with the node's signing key. Only the node can
/// create valid authorized keys — remote callers can only request that
/// the node create one.
pub fn write_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    node_signing_key: &lillux::crypto::SigningKey,
) -> Result<std::path::PathBuf> {
    use std::fs;

    // Build scope list as TOML array
    let scopes_str = scopes
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let body = format!(
        r#"fingerprint = "{fp}"
public_key = "ed25519:{key_b64}"
scopes = [{scopes_str}]
label = "{lbl}"
granted_by = "{granted_by}"
created_at = "{ca}"
"#,
        fp = fingerprint,
        key_b64 = public_key_b64,
        scopes_str = scopes_str,
        lbl = label.replace('"', "\\\""),
        granted_by = granted_by,
        ca = created_at,
    );

    // Sign with the NODE key
    let signed = lillux::signature::sign_content(&body, node_signing_key, "#", None);

    // Ensure directory exists
    fs::create_dir_all(auth_dir)?;

    // Atomic write: .tmp → rename
    let entry_path = auth_dir.join(format!("{fingerprint}.toml"));
    let tmp = entry_path.with_extension("tmp");
    fs::write(&tmp, signed.as_bytes())
        .with_context(|| format!("failed to write authorized-key entry {}", entry_path.display()))?;
    fs::rename(&tmp, &entry_path)
        .with_context(|| format!("failed to rename to {}", entry_path.display()))?;

    Ok(entry_path)
}
