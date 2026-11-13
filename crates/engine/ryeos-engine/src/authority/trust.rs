//! Item signer trust store and signature verification.
//!
//! Loads trusted signer public keys from `.ai/config/keys/trusted/`. Trust
//! is **operator-tier state only**. A project's own
//! `.ai/config/keys/trusted/` adds keys only through documents a key in the
//! node's trust store has signed (`TrustStore::with_project_key_files`);
//! unsigned, raw and self-signed project documents are refused.
//! Bundles never auto-trust their own keys — operators pin publisher keys
//! explicitly via `ryeos init` (platform author) and `ryeos trust pin <fp>`
//! (third parties). Old bundle-internal trust dirs are ignored.
//! Supports two key file formats:
//!   - Raw `.pub`/`.key` files: base64-encoded 32-byte Ed25519 keys
//!   - Signed `.toml` identity docs: structured TOML with PEM public keys
//!
//! The TOML format matches the Python `TrustStore` identity documents:
//! ```toml
//! # ryeos:signed:TIMESTAMP:HASH:SIG:FP
//! fingerprint = "16e73c5829f69d6f..."
//! owner = "leo"
//! attestation = ""
//! version = "1.0.0"
//!
//! [public_key]
//! pem = """
//! -----BEGIN PUBLIC KEY-----
//! MCowBQYDK2VwAyEA...
//! -----END PUBLIC KEY-----
//! """
//! ```
//!
//! Verifies Ed25519 item signatures and computes content hashes over
//! the post-signature-line content.
//!
//! The trust store is a simple key-value map: signer fingerprint → public key.
//! It does NOT share trust policy with daemon request auth or node auth —
//! those are distinct trust domains (see 05-trust-auth-and-signatures.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

/// Default version for TrustedKeyDoc when none is declared in the
/// TOML file, or when creating a new pin doc.
const DEFAULT_KEY_DOC_VERSION: &str = "1.0.0";
/// Shared resource contract for trusted-key discovery. Runtime config proof
/// and ordinary item verification must observe exactly the same hard bounds.
pub const MAX_TRUST_DOCUMENTS: usize = 1024;
pub const MAX_TRUST_DOCUMENT_BYTES: u64 = 1024 * 1024;
pub const MAX_TRUST_DIRECTORY_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TRUST_TRAVERSAL_ENTRIES: usize = MAX_TRUST_DOCUMENTS * 2;
pub const MAX_TRUST_TRAVERSAL_DEPTH: usize = 8;

/// Whether one catalog-relative file name is part of the project trust
/// document vocabulary. Content-authority adapters call this before reading
/// bytes so unsupported files cannot force work outside the trust bounds.
pub fn is_project_trust_document(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "toml" | "pub" | "key"))
}

use crate::contracts::{
    ResolvedItem, SignatureEnvelope, SignatureHeader, SignerFingerprint, TrustClass, VerifiedItem,
};
use crate::error::EngineError;

// ── Publisher trust document (PUBLISHER_TRUST.toml) ─────────────────

/// Canonical schema for `PUBLISHER_TRUST.toml` — the universal trust
/// artifact emitted by `ryeos publish` and consumed by `ryeos init
/// --trust-file` and `ryeos trust pin --from`.
///
/// ```toml
/// public_key = "ed25519:<base64>"   # required
/// fingerprint = "<sha256-hex>"      # required, MUST match public_key
/// owner = "<label>"                 # required, informational
/// ```
///
/// Strict parsing: `deny_unknown_fields`. One parser shared by all
/// consumers (init, trust pin, publish emit).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherTrustDoc {
    /// Ed25519 public key in `ed25519:<base64>` format.
    pub public_key: String,
    /// SHA-256 hex fingerprint of the raw 32-byte public key.
    pub fingerprint: String,
    /// Informational owner label (e.g. "ryeos-official", "RyeOS Development").
    pub owner: String,
}

impl PublisherTrustDoc {
    /// Parse a `PUBLISHER_TRUST.toml` from text. Validates that the
    /// fingerprint matches the public key bytes.
    pub fn parse(text: &str) -> Result<Self, EngineError> {
        let doc: PublisherTrustDoc = toml::from_str(text)
            .map_err(|e| EngineError::Internal(format!("invalid PUBLISHER_TRUST.toml: {e}")))?;
        doc.validate()?;
        Ok(doc)
    }

    /// Validate that the fingerprint matches the public key.
    pub fn validate(&self) -> Result<(), EngineError> {
        let vk = self.decode_verifying_key()?;
        let computed = compute_fingerprint(&vk);
        if computed != self.fingerprint {
            return Err(EngineError::Internal(format!(
                "PUBLISHER_TRUST.toml self-inconsistent: declared fingerprint {} \
                 but public_key bytes hash to {}",
                self.fingerprint, computed
            )));
        }
        Ok(())
    }

    /// Decode the `ed25519:<base64>` public key into a `VerifyingKey`.
    pub fn decode_verifying_key(&self) -> Result<VerifyingKey, EngineError> {
        let b64 = self.public_key.strip_prefix("ed25519:").ok_or_else(|| {
            EngineError::Internal("public_key must be ed25519:<base64> format".to_string())
        })?;
        let bytes: Vec<u8> = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| EngineError::Internal(format!("invalid base64 in public_key: {e}")))?;
        let len = bytes.len();
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            EngineError::Internal(format!("public_key must decode to 32 bytes, got {len}"))
        })?;
        VerifyingKey::from_bytes(&arr)
            .map_err(|e| EngineError::Internal(format!("invalid Ed25519 public key: {e}")))
    }

    /// Render to TOML text suitable for writing to disk.
    pub fn to_toml(&self) -> String {
        format!(
            "public_key = \"{}\"\nfingerprint = \"{}\"\nowner = \"{}\"\n",
            self.public_key, self.fingerprint, self.owner,
        )
    }
}

// ── Node document signers ───────────────────────────────────────────

/// Stable refusal code: a node document is signed by a key other than the
/// one key allowed to sign that kind of node state.
pub const NODE_DOCUMENT_WRONG_SIGNER: &str = "node_document_wrong_signer";

/// A node document (node policy, schedules, node config, bundle
/// registrations, the init completion record) signed by a key that may not
/// sign it. Node state verifies against exactly one signer, never against
/// every pinned key: a pinned third-party publisher key must not be able to
/// sign node state.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "{NODE_DOCUMENT_WRONG_SIGNER}: {path} is signed by {signer}, but only the {role} key {expected} may sign it; {cure}"
)]
pub struct NodeDocumentWrongSigner {
    pub path: String,
    pub signer: String,
    pub expected: String,
    /// `node` or `operator`.
    pub role: &'static str,
    pub cure: String,
}

/// The cure named for node-key documents: they are written only by the node.
pub const NODE_DOCUMENT_CURE: &str = "this is node state that only the node itself writes: remove the file and recreate it through the node's own command";

/// Refuse a node document whose signature line names any signer but
/// `expected`. Callers then verify the signature against
/// [`TrustStore::restricted_to`]`(expected)`, never the whole store.
pub fn require_document_signer(
    path: &Path,
    signer: &str,
    expected: &str,
    role: &'static str,
    cure: &str,
) -> Result<(), NodeDocumentWrongSigner> {
    if signer == expected {
        return Ok(());
    }
    Err(NodeDocumentWrongSigner {
        path: path.display().to_string(),
        signer: signer.to_string(),
        expected: expected.to_string(),
        role,
        cure: cure.to_string(),
    })
}

// ── Project trust documents ─────────────────────────────────────────

/// Refusal code: a project trust document carries no signature (this
/// includes raw `.pub`/`.key` files, which cannot carry one).
pub const PROJECT_TRUST_UNSIGNED: &str = "project_trust_document_unsigned";
/// Refusal code: a project trust document is signed only by the key it
/// declares, and the node does not trust that key.
pub const PROJECT_TRUST_SELF_SIGNED: &str = "project_trust_document_self_signed";
/// Refusal code: a project trust document is signed by a key outside the
/// node's own trust store.
pub const PROJECT_TRUST_UNTRUSTED_SIGNER: &str = "project_trust_document_untrusted_signer";

/// The cure every project trust refusal names.
pub const PROJECT_TRUST_CURE: &str = "A project trust document counts only when a key in the node's own trust store (`<app_root>/.ai/config/keys/trusted/`, such as the operator key) has signed it. Either have that key sign the document, or pin the key on the node instead with `ryeos-core-tools trust-pin --app-root <app_root> --from <PUBLISHER_TRUST.toml>` and delete the project copy";

fn project_trust_refusal(code: &'static str, path: &Path, reason: String) -> EngineError {
    EngineError::ProjectTrustDocumentRefused {
        code,
        path: path.display().to_string(),
        reason,
        cure: PROJECT_TRUST_CURE.to_string(),
    }
}

// ── Trusted signer entry ────────────────────────────────────────────

/// A trusted signer loaded from the trust store.
#[derive(Debug, Clone)]
pub struct TrustedSigner {
    pub fingerprint: String,
    pub verifying_key: VerifyingKey,
    pub label: Option<String>,
}

// ── Trusted key document (TOML format) ──────────────────────────────

/// A trusted key identity document parsed from a `.toml` file.
///
/// Mirrors the Python `TrustedKeyInfo` dataclass from
/// `ryeos/rye/utils/trust_store.py`.
#[derive(Debug, Clone)]
pub struct TrustedKeyDoc {
    pub fingerprint: String,
    pub owner: String,
    pub version: String,
    pub attestation: Option<String>,
    pub verifying_key: VerifyingKey,
}

/// Raw TOML structure for deserialization.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedKeyToml {
    fingerprint: String,
    #[serde(default = "default_owner")]
    owner: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    attestation: Option<String>,
    public_key: PublicKeySection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicKeySection {
    /// PEM-encoded Ed25519 public key, or `ed25519:<base64>` format
    pem: String,
}

fn default_owner() -> String {
    "unknown".to_string()
}

impl TrustedKeyDoc {
    /// Serialize to TOML body string (without signature line).
    pub fn to_toml(&self) -> String {
        let key_b64 =
            base64::engine::general_purpose::STANDARD.encode(self.verifying_key.as_bytes());
        let attestation = self.attestation.as_deref().unwrap_or("");
        format!(
            "fingerprint = \"{fp}\"\n\
             owner = \"{owner}\"\n\
             version = \"{version}\"\n\
             attestation = \"{attestation}\"\n\
             \n\
             [public_key]\n\
             pem = \"ed25519:{key_b64}\"\n",
            fp = self.fingerprint,
            owner = self.owner,
            version = self.version,
        )
    }
}

// ── Trust store ─────────────────────────────────────────────────────

/// Item signer trust store.
///
/// Maps signer fingerprints to their Ed25519 verifying keys.
/// Loaded from `*.pub` or `*.toml` key files in the trusted keys directory.
///
/// This is the **item signer** trust domain only. Distinct from
/// principal auth and node trust.
#[derive(Debug, Clone)]
pub struct TrustStore {
    signers: HashMap<String, TrustedSigner>,
}

impl TrustStore {
    /// Create an empty trust store (no signers trusted).
    pub fn empty() -> Self {
        Self {
            signers: HashMap::new(),
        }
    }

    /// Create a trust store from a pre-built map of signers.
    pub fn from_signers(signers: Vec<TrustedSigner>) -> Self {
        let map = signers
            .into_iter()
            .map(|s| (s.fingerprint.clone(), s))
            .collect();
        Self { signers: map }
    }

    /// Add every signer from `other` that we don't already have.
    /// Returns the number of signers actually inserted (i.e. that
    /// were not already in the store under the same fingerprint).
    ///
    /// Used by the per-request engine overlay to UNION a
    /// caller-pushed trust overlay with the remote's persistent
    /// trust store, without mutating either source.
    pub fn extend_from(&mut self, other: &TrustStore) -> usize {
        let mut added = 0;
        for (fp, signer) in other.signers.iter() {
            if !self.signers.contains_key(fp) {
                self.signers.insert(fp.clone(), signer.clone());
                added += 1;
            }
        }
        added
    }

    /// Load trusted signer keys from a directory.
    ///
    /// Supports three file types:
    ///   - `.toml` — signed TOML identity documents (preferred format)
    ///   - `.pub` / `.key` — raw base64-encoded 32-byte Ed25519 public keys
    ///
    /// Files with other extensions are silently skipped.
    /// Bad `.pub`/`.key`/`.toml` files produce hard errors.
    ///
    /// The fingerprint is the SHA-256 hex digest of the raw public key bytes.
    pub fn load_from_dir(keys_dir: &Path) -> Result<Self, EngineError> {
        match Self::read_trust_dir_files(keys_dir)? {
            Some(files) => Self::load_from_files(files),
            None => Ok(Self::empty()),
        }
    }

    /// Read the trust documents of one directory under the shared bounds,
    /// without parsing them. `None` when the directory is absent.
    pub fn read_trust_dir_files(
        keys_dir: &Path,
    ) -> Result<Option<Vec<(PathBuf, Vec<u8>)>>, EngineError> {
        let mut file_count = 0_usize;
        let mut total_bytes = 0_u64;
        let mut files = Vec::new();
        let present = lillux::visit_regular_files_no_follow_bounded(
            keys_dir,
            lillux::DirectoryTraversalBudget::new(
                MAX_TRUST_TRAVERSAL_ENTRIES,
                MAX_TRUST_TRAVERSAL_DEPTH,
            ),
            |_relative, is_directory| Ok(is_directory),
            |relative, file| {
                file_count = file_count.saturating_add(1);
                if file_count > MAX_TRUST_DOCUMENTS {
                    anyhow::bail!("trust directory exceeds {MAX_TRUST_DOCUMENTS} regular files");
                }
                let path = keys_dir.join(relative);
                if !is_project_trust_document(&path) {
                    return Ok(());
                }
                let bytes = lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
                total_bytes = total_bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("trust directory byte count overflow"))?;
                if total_bytes > MAX_TRUST_DIRECTORY_BYTES {
                    anyhow::bail!(
                        "trust directory exceeds {MAX_TRUST_DIRECTORY_BYTES} aggregate bytes"
                    );
                }
                files.push((path, bytes));
                Ok(())
            },
        )
        .map_err(|error| {
            EngineError::Internal(format!(
                "cannot securely read trust store dir {}: {error:#}",
                keys_dir.display()
            ))
        })?;
        Ok(present.then_some(files))
    }

    /// Load one deterministic set of trusted-key documents from already
    /// admitted bytes. Paths are diagnostic identities only and are never
    /// reopened.
    pub fn load_from_files(
        files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>,
    ) -> Result<Self, EngineError> {
        let mut files = files.into_iter().collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut signers = HashMap::new();
        for (path, bytes) in files {
            let ext = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            match ext {
                "toml" => {
                    let content = std::str::from_utf8(&bytes).map_err(|error| {
                        EngineError::Internal(format!(
                            "trusted key doc {} is not UTF-8: {error}",
                            path.display()
                        ))
                    })?;
                    let doc = load_trusted_key_doc_content(content, &path, &signers)?;
                    tracing::debug!(
                        fingerprint = %doc.fingerprint,
                        owner = %doc.owner,
                        path = %path.display(),
                        "loaded trusted key document"
                    );
                    signers.insert(
                        doc.fingerprint.clone(),
                        TrustedSigner {
                            fingerprint: doc.fingerprint,
                            verifying_key: doc.verifying_key,
                            label: Some(doc.owner),
                        },
                    );
                }
                "pub" | "key" => {
                    let signer = load_signer_key_content(&bytes, &path)?;
                    tracing::debug!(
                        fingerprint = %signer.fingerprint,
                        path = %path.display(),
                        "loaded trusted signer key"
                    );
                    signers.insert(signer.fingerprint.clone(), signer);
                }
                _ => {}
            }
        }
        Ok(Self { signers })
    }

    /// Load trusted keys from project and operator config roots.
    ///
    /// Trust is operator-side state. Bundles MUST NOT auto-trust their
    /// own keys. Operators pin publisher keys explicitly via `ryeos init`
    /// (platform author key) and trust-management commands.
    ///
    /// First match wins: a key present in the project tier is not
    /// overridden by operator config.
    pub fn load(
        project_root: Option<&Path>,
        operator_config_root: &Path,
    ) -> Result<Self, EngineError> {
        // Bundles are NOT a trust source. The node's own trust dir is
        // loaded first; a project's trust documents only count when a node
        // key vouches for them (see `with_project_key_files`).
        let node = Self::load_from_dir(&operator_config_root.join("keys").join("trusted"))?;
        let store = match project_root {
            Some(root) => node.with_project_keys(root)?.into_owned(),
            None => node,
        };
        if !store.is_empty() {
            tracing::info!(count = store.len(), "loaded trust store (operator-tier)");
        }
        Ok(store)
    }

    /// Warn about bundle-internal trust dirs. They are never loaded.
    pub fn warn_ignored_bundle_trust_dirs(bundle_roots: &[PathBuf]) {
        let trust_subdir = Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        for root in bundle_roots {
            let candidate = root.join(&trust_subdir);
            if candidate.is_dir() {
                tracing::warn!(
                    path = %candidate.display(),
                    "ignoring bundle-internal trust dir — pin the publisher key in operator config instead"
                );
            }
        }
    }

    /// Check whether a fingerprint is trusted.
    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.signers.contains_key(fingerprint)
    }

    /// Look up a signer's verifying key.
    pub fn get(&self, fingerprint: &str) -> Option<&TrustedSigner> {
        self.signers.get(fingerprint)
    }

    /// A store holding only `fingerprint`'s signer (empty when it is not
    /// trusted here). Node documents verify against this, not the whole
    /// store.
    pub fn restricted_to(&self, fingerprint: &str) -> Self {
        Self {
            signers: self
                .signers
                .get(fingerprint)
                .map(|signer| (signer.fingerprint.clone(), signer.clone()))
                .into_iter()
                .collect(),
        }
    }

    /// Every trusted signer, in no particular order.
    pub fn signers(&self) -> impl Iterator<Item = &TrustedSigner> {
        self.signers.values()
    }

    /// Number of trusted signers.
    pub fn len(&self) -> usize {
        self.signers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signers.is_empty()
    }

    /// Stable identity for the exact signer set used by a request snapshot.
    ///
    /// This intentionally includes public-key bytes and labels, not only map
    /// keys, so a malformed caller overlay cannot alias a prior trust state by
    /// reusing a declared fingerprint.
    pub fn fingerprint(&self) -> String {
        let mut signers = self.signers.values().collect::<Vec<_>>();
        signers.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        let mut bytes = Vec::new();
        for signer in signers {
            append_fingerprint_field(&mut bytes, signer.fingerprint.as_bytes());
            append_fingerprint_field(&mut bytes, signer.verifying_key.as_bytes());
            append_fingerprint_field(&mut bytes, signer.label.as_deref().unwrap_or("").as_bytes());
        }
        lillux::cas::sha256_hex(&bytes)
    }

    pub(crate) fn estimated_size_bytes(&self) -> usize {
        self.signers
            .values()
            .map(|signer| {
                signer.fingerprint.capacity()
                    + signer.label.as_ref().map(String::capacity).unwrap_or(0)
                    + signer.verifying_key.as_bytes().len()
            })
            .sum()
    }

    /// Return a trust store that includes project-local keys.
    ///
    /// Project keys take priority (first-wins): if a fingerprint exists
    /// in the project trust dir, it shadows the same fingerprint in this
    /// store. Keys in this store that don't conflict are preserved.
    /// Borrows `self` unchanged when the project declares no keys — the
    /// common case — instead of cloning the whole store per request.
    pub fn with_project_keys(
        &self,
        project_root: &Path,
    ) -> Result<std::borrow::Cow<'_, Self>, EngineError> {
        let trust_dir = project_root.join(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        if !trust_dir.is_dir() {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        let files = Self::read_trust_dir_files(&trust_dir)?.unwrap_or_default();
        if files.is_empty() {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        Ok(std::borrow::Cow::Owned(self.with_project_key_files(files)?))
    }

    /// Overlay project trust documents, given as already admitted bytes, on
    /// this node store. This is the one place project trust documents are
    /// parsed and verified: the daemon's engine and the runtime-side loader
    /// (`ryeos-runtime` `verified_loader`) both call it, so they cannot
    /// disagree about which project keys count. Paths are diagnostic only.
    ///
    /// A project trust document counts only when a key in this node store
    /// signed it. Unsigned documents and raw `.pub`/`.key` files are
    /// refused ([`PROJECT_TRUST_UNSIGNED`]), as are documents self-signed by
    /// a key the node does not hold ([`PROJECT_TRUST_SELF_SIGNED`]) and
    /// documents signed by any other key outside the node store
    /// ([`PROJECT_TRUST_UNTRUSTED_SIGNER`]), so writing a project directory
    /// can no longer pin a key. One project document never vouches for
    /// another.
    pub fn with_project_key_files(
        &self,
        files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>,
    ) -> Result<Self, EngineError> {
        let mut files = files.into_iter().collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut merged = HashMap::new();
        for (path, bytes) in files {
            let ext = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            match ext {
                "toml" => {
                    let content = std::str::from_utf8(&bytes).map_err(|error| {
                        EngineError::Internal(format!(
                            "trusted key doc {} is not UTF-8: {error}",
                            path.display()
                        ))
                    })?;
                    let doc = load_project_trust_doc_content(content, &path, &self.signers)?;
                    merged.insert(
                        doc.fingerprint.clone(),
                        TrustedSigner {
                            fingerprint: doc.fingerprint,
                            verifying_key: doc.verifying_key,
                            label: Some(doc.owner),
                        },
                    );
                }
                "pub" | "key" => {
                    return Err(project_trust_refusal(
                        PROJECT_TRUST_UNSIGNED,
                        &path,
                        "a raw public-key file carries no signature".to_string(),
                    ));
                }
                _ => {}
            }
        }
        for (fingerprint, signer) in &self.signers {
            merged
                .entry(fingerprint.clone())
                .or_insert_with(|| signer.clone());
        }
        Ok(Self { signers: merged })
    }

    /// Overlay project-local keys from one admitted project-content
    /// authority. File selection, formats, and bounds stay owned by the trust
    /// subsystem; the engine does not duplicate this policy.
    pub fn with_project_keys_from_content(
        &self,
        content: &dyn crate::project_content::AuthoritativeProjectContent,
    ) -> Result<Self, EngineError> {
        let prefix = Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR);
        let entries = content.list_files(&prefix, false, MAX_TRUST_DOCUMENTS)?;
        let mut total_bytes = 0_u64;
        let mut files = Vec::new();
        for entry in entries
            .into_iter()
            .filter(|entry| is_project_trust_document(&entry.relative_path))
        {
            total_bytes = total_bytes.checked_add(entry.size).ok_or_else(|| {
                EngineError::Internal("project trust-key byte count overflow".to_string())
            })?;
            if total_bytes > MAX_TRUST_DIRECTORY_BYTES {
                return Err(EngineError::Internal(format!(
                    "project trust keys exceed {MAX_TRUST_DIRECTORY_BYTES} bytes"
                )));
            }
            let project_relative = prefix.join(&entry.relative_path);
            let bytes = content
                .read_file(&project_relative, MAX_TRUST_DOCUMENT_BYTES)?
                .ok_or_else(|| {
                    EngineError::Internal(format!(
                        "admitted project trust key disappeared: {}",
                        project_relative.display()
                    ))
                })?;
            files.push((project_relative, bytes));
        }
        self.with_project_key_files(files)
    }
}

fn append_fingerprint_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

// ── Key loading ─────────────────────────────────────────────────────

fn load_signer_key_content(raw: &[u8], path: &Path) -> Result<TrustedSigner, EngineError> {
    let raw = std::str::from_utf8(raw).map_err(|error| {
        EngineError::Internal(format!("key file {} is not UTF-8: {error}", path.display()))
    })?;
    let line = raw.lines().next().unwrap_or("").trim();

    let key_b64 = if let Some(stripped) = line.strip_prefix("ed25519:") {
        stripped
    } else {
        line
    };

    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid base64 in key file {}: {e}",
                path.display()
            ))
        })?;

    let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineError::Internal(format!(
            "key file {} must contain exactly 32 bytes, got {}",
            path.display(),
            key_b64.len()
        ))
    })?;

    let verifying_key = VerifyingKey::from_bytes(&key_array).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 public key in {}: {e}",
            path.display()
        ))
    })?;

    let fingerprint = compute_fingerprint(&verifying_key);

    // Use filename stem as label
    let label = path.file_stem().and_then(|s| s.to_str()).map(String::from);

    Ok(TrustedSigner {
        fingerprint,
        verifying_key,
        label,
    })
}

// ── TOML key document loading ────────────────────────────────────────

/// Load a trusted key from a signed TOML identity document.
///
/// Format:
/// ```text
/// # ryeos:signed:TIMESTAMP:HASH:SIG:FP
/// fingerprint = "..."
/// owner = "..."
/// [public_key]
/// pem = """
/// -----BEGIN PUBLIC KEY-----
/// ...
/// -----END PUBLIC KEY-----
/// """
/// ```
///
/// Performs integrity verification when a signature line is present:
/// - Self-signed (signer_fp == key_fp): verifies using the key in the file
/// - Cross-signed (signer_fp != key_fp): looks up signer in already-loaded signers
/// - Unsigned files are accepted here (node trust dir only; project
///   documents go through `load_project_trust_doc_content` first)
fn load_trusted_key_doc_content(
    content: &str,
    path: &Path,
    existing_signers: &HashMap<String, TrustedSigner>,
) -> Result<TrustedKeyDoc, EngineError> {
    let toml_body = lillux::signature::strip_signature_lines(content);

    let parsed: TrustedKeyToml = toml::from_str(&toml_body).map_err(|e| {
        EngineError::Internal(format!(
            "invalid TOML in trusted key doc {}: {e}",
            path.display()
        ))
    })?;

    // Parse the public key — supports PEM or ed25519:<base64> format
    let verifying_key = parse_public_key_field(&parsed.public_key.pem, path)?;

    // Compute actual fingerprint and verify it matches the declared one
    let actual_fp = compute_fingerprint(&verifying_key);
    if actual_fp != parsed.fingerprint {
        return Err(EngineError::Internal(format!(
            "fingerprint mismatch in {}: declared {}, actual {}",
            path.display(),
            parsed.fingerprint,
            actual_fp,
        )));
    }

    // Verify integrity if signature line is present
    verify_key_doc_integrity(content, &verifying_key, &actual_fp, existing_signers, path)?;

    let attestation = parsed.attestation.filter(|a| !a.is_empty());

    Ok(TrustedKeyDoc {
        fingerprint: actual_fp,
        owner: parsed.owner,
        version: parsed
            .version
            .unwrap_or_else(|| DEFAULT_KEY_DOC_VERSION.to_string()),
        attestation,
        verifying_key,
    })
}

/// Load one project trust document. It must carry a signature line naming
/// a key in `node_signers`; the signature is then verified against that
/// node key (or, when the document declares a key the node already holds,
/// against that same key).
fn load_project_trust_doc_content(
    content: &str,
    path: &Path,
    node_signers: &HashMap<String, TrustedSigner>,
) -> Result<TrustedKeyDoc, EngineError> {
    let Some(signature_line) = content
        .lines()
        .find(|line| line.starts_with("# ryeos:signed:"))
    else {
        return Err(project_trust_refusal(
            PROJECT_TRUST_UNSIGNED,
            path,
            "the document has no `# ryeos:signed:` line".to_string(),
        ));
    };
    let signer = signature_line.rsplit(':').next().unwrap_or("").trim();
    if !node_signers.contains_key(signer) {
        let declared =
            toml::from_str::<TrustedKeyToml>(&lillux::signature::strip_signature_lines(content))
                .map(|parsed| parsed.fingerprint)
                .unwrap_or_default();
        return Err(if signer == declared {
            project_trust_refusal(
                PROJECT_TRUST_SELF_SIGNED,
                path,
                format!(
                    "it is signed only by the key it declares ({signer}), which the node does not trust"
                ),
            )
        } else {
            project_trust_refusal(
                PROJECT_TRUST_UNTRUSTED_SIGNER,
                path,
                format!("it is signed by {signer}, which is not in the node's trust store"),
            )
        });
    }
    load_trusted_key_doc_content(content, path, node_signers)
}

/// Parse a public key from the `[public_key].pem` field.
///
/// Accepts two formats:
///   - PKCS#8 PEM: `-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----`
///   - Raw format: `ed25519:<base64>` (32-byte key)
fn parse_public_key_field(pem_str: &str, path: &Path) -> Result<VerifyingKey, EngineError> {
    let trimmed = pem_str.trim();

    // Try ed25519:<base64> format first (simpler, used by daemon auth)
    if let Some(b64) = trimmed.strip_prefix("ed25519:") {
        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| {
                EngineError::Internal(format!(
                    "invalid base64 in ed25519: key in {}: {e}",
                    path.display()
                ))
            })?;
        let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
            EngineError::Internal(format!(
                "ed25519: key in {} must be 32 bytes",
                path.display()
            ))
        })?;
        return VerifyingKey::from_bytes(&key_array).map_err(|e| {
            EngineError::Internal(format!("invalid Ed25519 key in {}: {e}", path.display()))
        });
    }

    // Try PKCS#8 PEM format
    if trimmed.contains("-----BEGIN PUBLIC KEY-----") {
        use lillux::crypto::DecodePublicKey;
        return VerifyingKey::from_public_key_pem(trimmed).map_err(|e| {
            EngineError::Internal(format!("invalid PEM public key in {}: {e}", path.display()))
        });
    }

    // Fall back to raw base64 (32 bytes)
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid base64 in public key in {}: {e}",
                path.display()
            ))
        })?;
    let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| {
        EngineError::Internal(format!("public key in {} must be 32 bytes", path.display()))
    })?;
    VerifyingKey::from_bytes(&key_array).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 public key in {}: {e}",
            path.display()
        ))
    })
}

/// Verify the integrity of a signed TOML key document.
///
/// Parses the `# ryeos:signed:TIMESTAMP:HASH:SIG:FP` line (if present),
/// recomputes the content hash, and verifies the Ed25519 signature.
///
/// Supports self-signed docs (signer == key in file) and cross-signed
/// docs (signer is another key already loaded in the trust store).
fn verify_key_doc_integrity(
    content: &str,
    key_in_file: &VerifyingKey,
    key_fingerprint: &str,
    existing_signers: &HashMap<String, TrustedSigner>,
    path: &Path,
) -> Result<(), EngineError> {
    // Find the signature line
    let sig_line = match content.lines().find(|l| l.starts_with("# ryeos:signed:")) {
        Some(line) => line,
        None => {
            tracing::debug!(path = %path.display(), "unsigned trusted key document");
            return Ok(());
        }
    };

    // Parse: # ryeos:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>
    // Timestamp may contain colons, so rsplit from the right
    let remainder = &sig_line["# ryeos:signed:".len()..];
    let parts: Vec<&str> = remainder.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        return Err(EngineError::Internal(format!(
            "malformed signature header in {}",
            path.display()
        )));
    }
    let signer_fp = parts[0];
    let sig_b64 = parts[1];
    let claimed_hash = parts[2];

    // Compute content hash over everything after the signature line
    let sig_line_with_newline = format!("{sig_line}\n");
    let after_sig = content
        .strip_prefix(&sig_line_with_newline)
        .unwrap_or_else(|| {
            // Signature line without trailing newline (EOF)
            content.strip_prefix(sig_line).unwrap_or("")
        });
    let actual_hash = sha256_hex(after_sig.as_bytes());

    if actual_hash != claimed_hash {
        return Err(EngineError::Internal(format!(
            "content tampered in {}: expected {}…, got {}…",
            path.display(),
            &claimed_hash[..claimed_hash.len().min(16)],
            &actual_hash[..actual_hash.len().min(16)],
        )));
    }

    // Decode signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|e| {
            EngineError::Internal(format!(
                "invalid signature base64 in {}: {e}",
                path.display()
            ))
        })?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        EngineError::Internal(format!(
            "invalid Ed25519 signature in {}: {e}",
            path.display()
        ))
    })?;

    // Determine which key to verify with
    if signer_fp == key_fingerprint {
        // Self-signed: verify using the key described in the file
        key_in_file
            .verify(claimed_hash.as_bytes(), &signature)
            .map_err(|_| {
                EngineError::Internal(format!(
                    "self-signed signature invalid in {}",
                    path.display()
                ))
            })?;
    } else {
        // Cross-signed: look up the signing key in already-loaded signers
        match existing_signers.get(signer_fp) {
            Some(signer) => {
                signer
                    .verifying_key
                    .verify(claimed_hash.as_bytes(), &signature)
                    .map_err(|_| {
                        EngineError::Internal(format!(
                            "cross-signed signature invalid in {}",
                            path.display()
                        ))
                    })?;
            }
            None => {
                return Err(EngineError::Internal(format!(
                    "signing key {} not found in trust store for {}",
                    signer_fp,
                    path.display()
                )));
            }
        }
    }

    Ok(())
}

// ── TOFU key pinning ────────────────────────────────────────────────

/// Pin a public key to the trust store by writing a signed TOML doc.
///
/// Idempotent: returns `Ok(fingerprint)` without rewriting if
/// `{target_dir}/{fingerprint}.toml` already exists AND validates as a
/// trust doc whose embedded public key bytes equal `verifying_key`.
///
/// **Refuses on inconsistent state**: if an existing trust doc fails to
/// parse, has a fingerprint that doesn't match its filename, or whose
/// embedded key disagrees with `verifying_key`, this returns an error
/// rather than silently overwriting. The operator must remove the bad
/// file manually.
///
/// If `signing_key` is provided, the TOML doc is self-signed by the
/// pinned key (standard TOFU pattern). Otherwise it is written unsigned.
///
/// Returns the fingerprint of the pinned key.
pub fn pin_key(
    verifying_key: &VerifyingKey,
    owner: &str,
    target_dir: &Path,
    signing_key: Option<&lillux::crypto::SigningKey>,
) -> Result<String, EngineError> {
    let fingerprint = compute_fingerprint(verifying_key);
    let key_name = format!("{fingerprint}.toml");
    let key_file = target_dir.join(&key_name);
    let directory = lillux::PinnedDirectory::open_or_create(target_dir).map_err(|error| {
        EngineError::Internal(format!(
            "cannot establish trust dir {}: {error:#}",
            target_dir.display()
        ))
    })?;
    let _lock = directory.lock_exclusive().map_err(|error| {
        EngineError::Internal(format!(
            "cannot lock trust dir {}: {error:#}",
            target_dir.display()
        ))
    })?;

    // Idempotent — already pinned. Strictly validate the existing doc
    // rather than blindly accepting its presence.
    if let Some(existing) = directory
        .open_pinned_regular(std::ffi::OsStr::new(&key_name), false)
        .map_err(|error| {
            EngineError::Internal(format!(
                "cannot inspect trust key {}: {error:#}",
                key_file.display()
            ))
        })?
    {
        let observation = existing.observation().map_err(|error| {
            EngineError::Internal(format!(
                "cannot observe trust key {}: {error:#}",
                key_file.display()
            ))
        })?;
        let bytes = existing
            .read_stable_bounded(&observation, MAX_TRUST_DOCUMENT_BYTES)
            .map_err(|error| {
                EngineError::Internal(format!(
                    "existing trust doc {} cannot be read for idempotency check: {error:#} — remove it manually if intentional",
                    key_file.display()
                ))
            })?;
        validate_existing_pin(&bytes, &key_file, verifying_key, &fingerprint)?;
        tracing::debug!(fingerprint = %fingerprint, "key already pinned and validated, skipping");
        return Ok(fingerprint);
    }

    let doc = TrustedKeyDoc {
        fingerprint: fingerprint.clone(),
        owner: owner.to_string(),
        version: DEFAULT_KEY_DOC_VERSION.to_string(),
        attestation: None,
        verifying_key: *verifying_key,
    };
    let body = doc.to_toml();

    let content = match signing_key {
        Some(sk) => sign_toml_doc(&body, sk),
        None => body,
    };

    directory
        .atomic_write_pinned_if_same(
            std::ffi::OsStr::new(&key_name),
            None,
            content.as_bytes(),
            0o600,
            crate::ENGINE_TEMP_PURPOSE,
        )
        .map_err(|e| {
            EngineError::Internal(format!(
                "cannot publish trust key {}: {e:#}",
                key_file.display(),
            ))
        })?;

    tracing::info!(fingerprint = %fingerprint, owner = %owner, "pinned trusted key");
    Ok(fingerprint)
}

/// Prepend a `# ryeos:signed:...` line to a TOML document body.
fn sign_toml_doc(body: &str, signing_key: &lillux::crypto::SigningKey) -> String {
    lillux::signature::sign_content(body, signing_key, "#", None)
}

/// Validate that an on-disk pin file matches the key being pinned.
///
/// Refuses idempotency on:
///   - unreadable file
///   - invalid TOML
///   - fingerprint declared inside the file ≠ the filename `<fp>.toml`
///   - embedded public key bytes ≠ the bytes being pinned now
///
/// Returns `Ok(())` only when everything matches.
fn validate_existing_pin(
    bytes: &[u8],
    key_file: &Path,
    verifying_key: &VerifyingKey,
    expected_fingerprint: &str,
) -> Result<(), EngineError> {
    let content = std::str::from_utf8(bytes).map_err(|e| {
        EngineError::Internal(format!(
            "existing trust doc {} is not UTF-8: {e} \
             — remove it manually if intentional",
            key_file.display()
        ))
    })?;

    let toml_body = lillux::signature::strip_signature_lines(content);
    let parsed: TrustedKeyToml = toml::from_str(&toml_body).map_err(|e| {
        EngineError::Internal(format!(
            "existing trust doc {} is not valid TOML: {e} \
             — remove it manually if intentional",
            key_file.display()
        ))
    })?;

    let on_disk_key = parse_public_key_field(&parsed.public_key.pem, key_file)?;
    let on_disk_fp = compute_fingerprint(&on_disk_key);
    if on_disk_fp != expected_fingerprint {
        return Err(EngineError::Internal(format!(
            "existing trust doc {} hashes to {} but its filename declares {} \
             — refuse to overwrite. Remove the file manually if intentional.",
            key_file.display(),
            on_disk_fp,
            expected_fingerprint,
        )));
    }
    if parsed.fingerprint != expected_fingerprint {
        return Err(EngineError::Internal(format!(
            "existing trust doc {} embeds fingerprint {} but filename is {} \
             — refuse to overwrite. Remove the file manually if intentional.",
            key_file.display(),
            parsed.fingerprint,
            expected_fingerprint,
        )));
    }
    if on_disk_key.as_bytes() != verifying_key.as_bytes() {
        return Err(EngineError::Internal(format!(
            "existing trust doc {} contains a different public key than the \
             one being pinned — refuse to overwrite. Remove the file manually \
             if intentional.",
            key_file.display(),
        )));
    }
    Ok(())
}

// ── Fingerprint computation ─────────────────────────────────────────

/// Compute the SHA-256 hex fingerprint of an Ed25519 public key.
///
/// This matches the fingerprint computation in `crates/bin/daemon/src/identity.rs`.
pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    lillux::signature::compute_fingerprint(key)
}

// ── Content hash after signature line ───────────────────────────────

/// Compute the content hash over file bytes AFTER the signature line.
///
/// The signature line itself is excluded from the hash. This is what
/// the signer hashed when creating the signature.
///
/// Returns the SHA-256 hex digest of the post-signature content, and
/// the byte offset where the content starts (after the signature line's
/// trailing newline).
pub fn content_hash_after_signature(content: &str, envelope: &SignatureEnvelope) -> Option<String> {
    lillux::signature::content_hash_after_signature(
        content,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )
}

pub fn strip_signature_lines(content: &str) -> String {
    lillux::signature::strip_signature_lines(content)
}

// ── Signature verification ──────────────────────────────────────────

/// Verify an item's Ed25519 signature.
///
/// Steps:
/// 1. Recompute content hash over bytes after the signature line
/// 2. Compare with the content_hash in the signature header
/// 3. Verify the Ed25519 signature over the content_hash string
///
/// The signature is over the content_hash hex string (as bytes), matching
/// the pattern in `crates/bin/daemon/src/auth.rs` and `crates/bin/daemon/src/identity.rs`.
#[tracing::instrument(
    name = "engine:trust_verify",
    skip(content, trust_store),
    fields(signer = ?header.signer_fingerprint)
)]
pub fn verify_item_signature(
    content: &str,
    header: &SignatureHeader,
    envelope: &SignatureEnvelope,
    trust_store: &TrustStore,
) -> Result<(TrustClass, Option<SignerFingerprint>), EngineError> {
    // Step 1: Recompute content hash over post-signature content
    let actual_hash = content_hash_after_signature(content, envelope).ok_or_else(|| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(), // caller fills this in
            reason: "could not locate signature line in content".into(),
        }
    })?;

    verify_item_signature_with_hash(&actual_hash, header, trust_store)
}

/// Like [`verify_item_signature`], but takes the content hash the
/// caller already recomputed over the post-signature content, so the
/// content is not hashed a second time. The hash must come from
/// [`content_hash_after_signature`] on the same content — passing the
/// header's own hash would turn the integrity check into a tautology.
pub fn verify_item_signature_with_hash(
    actual_hash: &str,
    header: &SignatureHeader,
    trust_store: &TrustStore,
) -> Result<(TrustClass, Option<SignerFingerprint>), EngineError> {
    tracing::trace!(actual_hash = %actual_hash, header_hash = %header.content_hash, "comparing content hashes");

    // Step 2: Compare content hashes
    if actual_hash != header.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: String::new(),
            expected: header.content_hash.clone(),
            actual: actual_hash.to_owned(),
        });
    }

    // Step 3: Decode and verify the Ed25519 signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|e| EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid base64 in signature: {e}"),
        })?;

    let signature = Signature::from_slice(&sig_bytes).map_err(|e| {
        EngineError::SignatureVerificationFailed {
            canonical_ref: String::new(),
            reason: format!("invalid Ed25519 signature bytes: {e}"),
        }
    })?;

    // Look up signer in trust store
    let fingerprint = &header.signer_fingerprint;
    match trust_store.get(fingerprint) {
        Some(signer) => {
            // Verify: signature is over the content_hash hex string
            signer
                .verifying_key
                .verify(header.content_hash.as_bytes(), &signature)
                .map_err(|_| EngineError::SignatureVerificationFailed {
                    canonical_ref: String::new(),
                    reason: "Ed25519 signature verification failed".into(),
                })?;

            tracing::trace!(fingerprint = %fingerprint, trust_class = "trusted", "signature verified");
            Ok((
                TrustClass::Trusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
        None => {
            // Signer not in trust store — we can't verify the signature
            // cryptographically, but the content hash check passed.
            // Mark as Untrusted.
            tracing::trace!(fingerprint = %fingerprint, trust_class = "untrusted", "signer not in trust store");
            Ok((
                TrustClass::Untrusted,
                Some(SignerFingerprint(fingerprint.clone())),
            ))
        }
    }
}

/// Full item verification: takes a ResolvedItem, reads its content,
/// and produces a VerifiedItem.
#[tracing::instrument(
    name = "engine:verify_item",
    skip(item, trust_store),
    fields(canonical_ref = %item.canonical_ref)
)]
pub fn verify_resolved_item(
    item: ResolvedItem,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    let content = std::fs::read_to_string(&item.source_path).map_err(|e| {
        EngineError::Internal(format!(
            "failed to read {} for verification: {e}",
            item.source_path.display()
        ))
    })?;
    verify_resolved_item_content(item, &content, trust_store)
}

/// Verify a resolved item against exact source bytes already opened by the
/// engine.
///
/// This keeps the read/hash/verify sequence single-pass for static
/// verification caching. It is crate-visible only: callers outside the engine
/// cannot pair fabricated resolution metadata with arbitrary bytes.
pub(crate) fn verify_resolved_item_content(
    item: ResolvedItem,
    content: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    let actual_content_hash = crate::item_resolution::content_hash(content);
    verify_resolved_item_content_with_hash(item, content, &actual_content_hash, trust_store)
}

/// Verify bytes whose whole-file digest was already computed by the engine.
///
/// The supplied digest is not caller authority: it is compared with the
/// resolver's admitted digest before signature verification. Keeping this
/// helper crate-private prevents another crate from substituting unproven
/// bytes while avoiding a second SHA-256 pass in the static-evidence cache.
pub(crate) fn verify_resolved_item_content_with_hash(
    item: ResolvedItem,
    content: &str,
    actual_content_hash: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem, EngineError> {
    if actual_content_hash != item.content_hash {
        return Err(EngineError::ContentHashMismatch {
            canonical_ref: item.canonical_ref.to_string(),
            expected: item.content_hash.clone(),
            actual: actual_content_hash.to_owned(),
        });
    }
    tracing::debug!(
        item_ref = %item.canonical_ref,
        has_signature = item.signature_header.is_some(),
        "verifying resolved item"
    );

    match &item.signature_header {
        Some(header) => {
            let (trust_class, signer) =
                verify_item_signature(content, header, &item.source_format.signature, trust_store)
                    .map_err(|e| patch_canonical_ref(e, &item.canonical_ref.to_string()))?;

            Ok(VerifiedItem {
                resolved: item,
                signer,
                trust_class,
                pinned_version: None,
            })
        }
        None => {
            // No signature — Unsigned
            Ok(VerifiedItem {
                resolved: item,
                signer: None,
                trust_class: TrustClass::Unsigned,
                pinned_version: None,
            })
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn sha256_hex(data: &[u8]) -> String {
    lillux::cas::sha256_hex(data)
}

/// Patch the canonical_ref field into verification errors that were
/// created with an empty string placeholder.
pub(crate) fn patch_canonical_ref(err: EngineError, canonical_ref: &str) -> EngineError {
    match err {
        EngineError::SignatureVerificationFailed { reason, .. } => {
            EngineError::SignatureVerificationFailed {
                canonical_ref: canonical_ref.to_owned(),
                reason,
            }
        }
        EngineError::ContentHashMismatch {
            expected, actual, ..
        } => EngineError::ContentHashMismatch {
            canonical_ref: canonical_ref.to_owned(),
            expected,
            actual,
        },
        other => other,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::SignatureEnvelope;
    use lillux::crypto::{Signer, SigningKey};
    use std::fs;

    fn hash_prefix_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        }
    }

    fn html_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        }
    }

    fn shebang_envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        }
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir =
            std::env::temp_dir().join(format!("rye_trust_test_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Generate a signing key pair and return (signing_key, verifying_key, fingerprint).
    fn gen_key() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Generate a second, distinct key pair.
    fn gen_key2() -> (SigningKey, VerifyingKey, String) {
        let signing_key = SigningKey::from_bytes(&[99u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        (signing_key, verifying_key, fingerprint)
    }

    /// Build a properly signed file content string.
    fn build_signed_content(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        envelope: &SignatureEnvelope,
    ) -> String {
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("ryeos:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");

        match &envelope.suffix {
            Some(suffix) => format!("{} {payload} {suffix}\n{body}", envelope.prefix),
            None => format!("{} {payload}\n{body}", envelope.prefix),
        }
    }

    /// Build a properly signed file with shebang.
    fn build_signed_content_with_shebang(
        body: &str,
        signing_key: &SigningKey,
        fingerprint: &str,
        shebang: &str,
    ) -> String {
        let envelope = shebang_envelope();
        // The signature line is line 2, below the shebang; the content hash
        // covers every byte except that line, shebang included.
        let content_hash = sha256_hex(format!("{shebang}\n{body}").as_bytes());
        let signature: Signature = signing_key.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";

        let payload = format!("ryeos:signed:{timestamp}:{content_hash}:{sig_b64}:{fingerprint}");
        format!("{shebang}\n{} {payload}\n{body}", envelope.prefix)
    }

    // ── TrustStore tests ────────────────────────────────────────────

    #[test]
    fn empty_trust_store() {
        let store = TrustStore::empty();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.is_trusted("anything"));
    }

    #[test]
    fn trust_store_from_signers() {
        let (_, vk, fp) = gen_key();
        let signer = TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: Some("test".into()),
        };
        let store = TrustStore::from_signers(vec![signer]);
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert!(!store.is_trusted("other"));
    }

    #[test]
    fn trust_store_load_from_dir_empty() {
        let dir = tempdir();
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn trust_store_load_from_dir_nonexistent() {
        let dir = tempdir().join("nonexistent");
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn trust_store_load_ed25519_prefixed_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("signer1.pub"), format!("ed25519:{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        let loaded = store.get(&fp).unwrap();
        assert_eq!(loaded.label.as_deref(), Some("signer1"));
    }

    #[test]
    fn trust_store_load_raw_b64_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("signer1.pub"), format!("{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    #[test]
    fn trust_store_load_multiple_keys() {
        let dir = tempdir();
        let (_, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        let b1 = base64::engine::general_purpose::STANDARD.encode(vk1.as_bytes());
        let b2 = base64::engine::general_purpose::STANDARD.encode(vk2.as_bytes());

        fs::write(dir.join("signer_a.pub"), format!("ed25519:{b1}\n")).unwrap();
        fs::write(dir.join("signer_b.pub"), format!("ed25519:{b2}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_errors_on_bad_key_file() {
        let dir = tempdir();
        let (_, vk, _) = gen_key();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("good.pub"), format!("ed25519:{key_b64}\n")).unwrap();
        fs::write(dir.join("bad.pub"), "not a valid key\n").unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(_)),
            "expected Internal error for bad .pub file, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_skips_non_key_files() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        fs::write(dir.join("good.pub"), format!("ed25519:{key_b64}\n")).unwrap();
        fs::write(dir.join("readme.txt"), "this is not a key file").unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    // ── Project + operator trust tests ─────────────────────────────

    fn project_and_operator_dirs() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let project = tempdir();
        let operator_config = tempdir();
        let project_trust = project.join(Path::new(crate::AI_DIR).join(crate::TRUST_KEYS_DIR));
        let operator_trust = operator_config.join("keys").join("trusted");
        fs::create_dir_all(&project_trust).unwrap();
        fs::create_dir_all(&operator_trust).unwrap();
        (project, operator_config, project_trust, operator_trust)
    }

    fn project_refusal_code(error: EngineError) -> &'static str {
        match error {
            EngineError::ProjectTrustDocumentRefused { code, cure, .. } => {
                assert!(cure.contains("ryeos-core-tools trust-pin"), "cure: {cure}");
                code
            }
            other => panic!("expected a project trust refusal, got {other:?}"),
        }
    }

    #[test]
    fn load_project_overrides_operator() {
        let (project, operator_config, project_trust, operator_trust) = project_and_operator_dirs();
        let (sk, vk, fp) = gen_key();

        // Same fingerprint in both spaces, different labels. The project copy
        // is signed by the key itself, which the node already pins.
        let project_toml = sign_toml_doc(&build_key_doc_toml(&fp, "project_owner", &vk), &sk);
        let operator_toml = build_key_doc_toml(&fp, "operator_owner", &vk);
        fs::write(project_trust.join(format!("{fp}.toml")), &project_toml).unwrap();
        fs::write(operator_trust.join(format!("{fp}.toml")), &operator_toml).unwrap();

        let store = TrustStore::load(Some(&project), &operator_config).unwrap();

        assert_eq!(store.len(), 1);
        // Project wins — label should be "project_owner"
        assert_eq!(
            store.get(&fp).unwrap().label.as_deref(),
            Some("project_owner")
        );
    }

    #[test]
    fn load_merges_project_and_operator() {
        let (project, operator_config, project_trust, operator_trust) = project_and_operator_dirs();
        let (_, vk1, fp1) = gen_key();
        let (sk2, vk2, fp2) = gen_key2();

        // Key 1 in project only, cross-signed by the node-pinned key 2.
        let t1 = sign_toml_doc(&build_key_doc_toml(&fp1, "alice", &vk1), &sk2);
        fs::write(project_trust.join(format!("{fp1}.toml")), &t1).unwrap();

        // Key 2 in operator config only (node bootstrap docs may be unsigned).
        let t2 = build_key_doc_toml(&fp2, "bob", &vk2);
        fs::write(operator_trust.join(format!("{fp2}.toml")), &t2).unwrap();

        let store = TrustStore::load(Some(&project), &operator_config).unwrap();

        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    /// Gap C: writing an unsigned document into a project used to pin a key.
    #[test]
    fn unsigned_project_trust_doc_is_refused() {
        let (project, operator_config, project_trust, _) = project_and_operator_dirs();
        let (_, vk, fp) = gen_key();
        fs::write(
            project_trust.join(format!("{fp}.toml")),
            build_key_doc_toml(&fp, "planted", &vk),
        )
        .unwrap();
        let error = TrustStore::load(Some(&project), &operator_config).unwrap_err();
        assert_eq!(project_refusal_code(error), PROJECT_TRUST_UNSIGNED);
    }

    #[test]
    fn self_signed_project_trust_doc_is_refused() {
        let (project, operator_config, project_trust, _) = project_and_operator_dirs();
        let (sk, vk, fp) = gen_key();
        fs::write(
            project_trust.join(format!("{fp}.toml")),
            sign_toml_doc(&build_key_doc_toml(&fp, "planted", &vk), &sk),
        )
        .unwrap();
        let error = TrustStore::load(Some(&project), &operator_config).unwrap_err();
        assert_eq!(project_refusal_code(error), PROJECT_TRUST_SELF_SIGNED);
    }

    #[test]
    fn project_trust_doc_signed_by_unpinned_key_is_refused() {
        let (project, operator_config, project_trust, _) = project_and_operator_dirs();
        let (_, vk, fp) = gen_key();
        let (stranger, _, _) = gen_key2();
        fs::write(
            project_trust.join(format!("{fp}.toml")),
            sign_toml_doc(&build_key_doc_toml(&fp, "planted", &vk), &stranger),
        )
        .unwrap();
        let error = TrustStore::load(Some(&project), &operator_config).unwrap_err();
        assert_eq!(project_refusal_code(error), PROJECT_TRUST_UNTRUSTED_SIGNER);
    }

    #[test]
    fn raw_public_key_file_in_project_is_refused() {
        let (project, operator_config, project_trust, _) = project_and_operator_dirs();
        let (_, vk, _) = gen_key();
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        fs::write(
            project_trust.join("planted.pub"),
            format!("ed25519:{key_b64}\n"),
        )
        .unwrap();
        let error = TrustStore::load(Some(&project), &operator_config).unwrap_err();
        assert_eq!(project_refusal_code(error), PROJECT_TRUST_UNSIGNED);
    }

    /// One project document never vouches for another: the signer must be
    /// in the node store itself.
    #[test]
    fn project_trust_doc_cannot_vouch_for_another_project_doc() {
        let (project, operator_config, project_trust, operator_trust) = project_and_operator_dirs();
        let (node_sk, node_vk, node_fp) = gen_key();
        let (middle_sk, middle_vk, middle_fp) = gen_key2();
        let leaf_sk = SigningKey::from_bytes(&[7u8; 32]);
        let leaf_vk = leaf_sk.verifying_key();
        let leaf_fp = compute_fingerprint(&leaf_vk);
        fs::write(
            operator_trust.join(format!("{node_fp}.toml")),
            build_key_doc_toml(&node_fp, "operator", &node_vk),
        )
        .unwrap();
        // "a-" sorts first: the node-vouched middle key loads before the leaf.
        fs::write(
            project_trust.join("a-middle.toml"),
            sign_toml_doc(
                &build_key_doc_toml(&middle_fp, "middle", &middle_vk),
                &node_sk,
            ),
        )
        .unwrap();
        fs::write(
            project_trust.join("b-leaf.toml"),
            sign_toml_doc(&build_key_doc_toml(&leaf_fp, "leaf", &leaf_vk), &middle_sk),
        )
        .unwrap();
        let error = TrustStore::load(Some(&project), &operator_config).unwrap_err();
        assert_eq!(project_refusal_code(error), PROJECT_TRUST_UNTRUSTED_SIGNER);
    }

    /// The node's own bootstrap documents keep loading as init writes them:
    /// operator and node self-signed, publisher pins unsigned.
    #[test]
    fn node_trust_dir_bootstrap_documents_still_load() {
        let operator_config = tempdir();
        let operator_trust = operator_config.join("keys").join("trusted");
        fs::create_dir_all(&operator_trust).unwrap();
        let (operator_sk, operator_vk, operator_fp) = gen_key();
        let (_, publisher_vk, publisher_fp) = gen_key2();
        fs::write(
            operator_trust.join(format!("{operator_fp}.toml")),
            sign_toml_doc(
                &build_key_doc_toml(&operator_fp, "self", &operator_vk),
                &operator_sk,
            ),
        )
        .unwrap();
        fs::write(
            operator_trust.join(format!("{publisher_fp}.toml")),
            build_key_doc_toml(&publisher_fp, "ryeos-official", &publisher_vk),
        )
        .unwrap();
        let store = TrustStore::load(None, &operator_config).unwrap();
        assert!(store.is_trusted(&operator_fp));
        assert!(store.is_trusted(&publisher_fp));
    }

    #[test]
    fn load_empty_when_no_dirs_exist() {
        let store = TrustStore::load(
            Some(Path::new("/nonexistent/project")),
            Path::new("/nonexistent/operator/.ai/config"),
        )
        .unwrap();
        assert!(store.is_empty());
    }

    // ── TOML key document tests ────────────────────────────────────

    /// Build a TOML trusted key document body (without signature line).
    fn build_key_doc_toml(fingerprint: &str, owner: &str, vk: &VerifyingKey) -> String {
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        format!(
            r#"fingerprint = "{fingerprint}"
owner = "{owner}"
version = "1.0.0"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
        )
    }

    /// Build a self-signed TOML trusted key document.
    fn build_signed_key_doc(sk: &SigningKey, vk: &VerifyingKey, fp: &str, owner: &str) -> String {
        let body = build_key_doc_toml(fp, owner, vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let timestamp = "2026-04-10T00:00:00Z";
        format!("# ryeos:signed:{timestamp}:{content_hash}:{sig_b64}:{fp}\n{body}")
    }

    #[test]
    fn trust_store_load_unsigned_toml_key() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();

        let toml_content = build_key_doc_toml(&fp, "alice", &vk);
        fs::write(dir.join(format!("{fp}.toml")), &toml_content).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        let loaded = store.get(&fp).unwrap();
        assert_eq!(loaded.label.as_deref(), Some("alice"));
    }

    #[test]
    fn trust_store_load_self_signed_toml_key() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let content = build_signed_key_doc(&sk, &vk, &fp, "bob");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert_eq!(store.get(&fp).unwrap().label.as_deref(), Some("bob"));
    }

    #[test]
    fn trust_store_load_cross_signed_toml_key() {
        let dir = tempdir();
        let (sk1, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        // First key is self-signed
        let content1 = build_signed_key_doc(&sk1, &vk1, &fp1, "signer");
        // Second key is cross-signed by first key
        let body2 = build_key_doc_toml(&fp2, "signee", &vk2);
        let content_hash = sha256_hex(body2.as_bytes());
        let signature: Signature = sk1.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content2 =
            format!("# ryeos:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp1}\n{body2}");

        // Write files — sorted order matters: fp1 file must come before fp2
        // Use names that sort correctly
        fs::write(dir.join("a_signer.toml"), &content1).unwrap();
        fs::write(dir.join("b_signee.toml"), &content2).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_toml_tampered_content() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let mut content = build_signed_key_doc(&sk, &vk, &fp, "eve");
        // Tamper with the content
        content.push_str("# injected\n");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("content tampered")),
            "expected content tampered error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_toml_fingerprint_mismatch() {
        let dir = tempdir();
        let (_, vk, _fp) = gen_key();

        // Wrong fingerprint in the document
        let toml_content = build_key_doc_toml("wrong_fingerprint", "charlie", &vk);
        fs::write(dir.join("wrong.toml"), &toml_content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("fingerprint mismatch")),
            "expected fingerprint mismatch error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_toml_invalid_signature() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let (sk2, _, _) = gen_key2();

        // Sign with wrong key but claim to be self-signed
        let body = build_key_doc_toml(&fp, "mallory", &vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk2.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content =
            format!("# ryeos:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp}\n{body}");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("signature invalid")),
            "expected signature invalid error, got: {err:?}"
        );
    }

    #[test]
    fn trust_store_mixed_toml_and_pub_files() {
        let dir = tempdir();
        let (_sk, vk1, fp1) = gen_key();
        let (_, vk2, fp2) = gen_key2();

        // One TOML key doc
        let toml_content = build_key_doc_toml(&fp1, "toml_user", &vk1);
        fs::write(dir.join(format!("{fp1}.toml")), &toml_content).unwrap();

        // One .pub raw key
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk2.as_bytes());
        fs::write(dir.join("other.pub"), format!("ed25519:{key_b64}\n")).unwrap();

        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 2);
        assert!(store.is_trusted(&fp1));
        assert!(store.is_trusted(&fp2));
    }

    #[test]
    fn trust_store_toml_cross_sign_unknown_signer() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();
        let (sk2, _, fp2) = gen_key2();

        // Sign with sk2 but fp2 is not in the store
        let body = build_key_doc_toml(&fp, "orphan", &vk);
        let content_hash = sha256_hex(body.as_bytes());
        let signature: Signature = sk2.sign(content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let content =
            format!("# ryeos:signed:2026-04-10T00:00:00Z:{content_hash}:{sig_b64}:{fp2}\n{body}");
        fs::write(dir.join(format!("{fp}.toml")), &content).unwrap();

        let err = TrustStore::load_from_dir(&dir).unwrap_err();
        assert!(
            matches!(err, EngineError::Internal(ref msg) if msg.contains("not found in trust store")),
            "expected signing key not found error, got: {err:?}"
        );
    }

    // ── Pin key tests ────────────────────────────────────────────────

    #[test]
    fn pin_key_writes_unsigned_toml() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();

        let result_fp = pin_key(&vk, "alice", &dir, None).unwrap();
        assert_eq!(result_fp, fp);

        // File was created
        let key_file = dir.join(format!("{fp}.toml"));
        assert!(key_file.exists());

        // Loadable by trust store
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
        assert_eq!(store.get(&fp).unwrap().label.as_deref(), Some("alice"));
    }

    #[test]
    fn pin_key_writes_signed_toml() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        let result_fp = pin_key(&vk, "bob", &dir, Some(&sk)).unwrap();
        assert_eq!(result_fp, fp);

        // File has signature line
        let content = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();
        assert!(content.starts_with("# ryeos:signed:"));

        // Loadable and passes integrity verification
        let store = TrustStore::load_from_dir(&dir).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.is_trusted(&fp));
    }

    #[test]
    fn pin_key_idempotent() {
        let dir = tempdir();
        let (sk, vk, fp) = gen_key();

        pin_key(&vk, "first", &dir, Some(&sk)).unwrap();
        let content_before = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();

        // Second call is a no-op
        pin_key(&vk, "second", &dir, Some(&sk)).unwrap();
        let content_after = fs::read_to_string(dir.join(format!("{fp}.toml"))).unwrap();

        assert_eq!(content_before, content_after);
    }

    #[test]
    fn pin_key_creates_directory() {
        let base = tempdir();
        let nested = base.join("deep/nested/trust");
        let (_, vk, fp) = gen_key();

        pin_key(&vk, "charlie", &nested, None).unwrap();
        assert!(nested.join(format!("{fp}.toml")).exists());
    }

    #[test]
    fn pin_key_refuses_existing_doc_with_different_key() {
        let dir = tempdir();
        let (_, vk1, fp1) = gen_key();
        let (_, vk2, _fp2) = gen_key2();

        // Pretend trust dir has a (corrupt) doc named after fp1 but
        // containing key bytes for vk2.
        let body = build_key_doc_toml(&fp1, "spoofed", &vk2);
        fs::write(dir.join(format!("{fp1}.toml")), &body).unwrap();

        // Pin attempt for vk1 (whose fingerprint IS fp1) must refuse
        // because the existing on-disk key bytes don't match vk1.
        let err = pin_key(&vk1, "alice", &dir, None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("different public key") || msg.contains("hashes to"),
            "expected drift refusal, got: {msg}"
        );
    }

    #[test]
    fn pin_key_refuses_corrupt_existing_doc() {
        let dir = tempdir();
        let (_, vk, fp) = gen_key();

        // Write garbage at the canonical pin filename
        fs::write(dir.join(format!("{fp}.toml")), "this is not toml ===").unwrap();

        let err = pin_key(&vk, "alice", &dir, None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not valid TOML") || msg.contains("invalid TOML"),
            "expected corrupt-doc refusal, got: {msg}"
        );
    }

    // ── Content hash tests ──────────────────────────────────────────

    #[test]
    fn content_hash_after_sig_hash_prefix() {
        let body = "print('hello')\n";
        let content = format!("# ryeos:signed:2026-04-10T00:00:00Z:abc:sig:fp\n{body}");
        let envelope = hash_prefix_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn content_hash_after_sig_html_prefix() {
        let body = "# Hello\n";
        let content = format!("<!-- ryeos:signed:2026-04-10T00:00:00Z:abc:sig:fp -->\n{body}");
        let envelope = html_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(hash, sha256_hex(body.as_bytes()));
    }

    #[test]
    fn content_hash_after_sig_with_shebang() {
        let body = "print('hello')\n";
        let content = format!(
            "#!/usr/bin/env python3\n# ryeos:signed:2026-04-10T00:00:00Z:abc:sig:fp\n{body}"
        );
        let envelope = shebang_envelope();

        let hash = content_hash_after_signature(&content, &envelope).unwrap();
        assert_eq!(
            hash,
            sha256_hex(format!("#!/usr/bin/env python3\n{body}").as_bytes())
        );
    }

    #[test]
    fn content_hash_no_signature_line() {
        let content = "print('hello')\n";
        let envelope = hash_prefix_envelope();

        assert!(content_hash_after_signature(content, &envelope).is_none());
    }

    // ── Signature verification tests ────────────────────────────────

    #[test]
    fn verify_valid_signature_trusted() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        // Parse the header from the content
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_valid_signature_untrusted_signer() {
        let (sk, _, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        // Empty trust store — signer not trusted
        let store = TrustStore::empty();

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Untrusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_tampered_content_fails() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let mut content = build_signed_content(body, &sk, &fp, &envelope);

        // Tamper with the body
        content.push_str("# injected malicious code\n");

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: None,
        }]);

        let err = verify_item_signature(&content, &header, &envelope, &store).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "expected ContentHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn verify_wrong_signer_key_fails() {
        let (sk, _, fp) = gen_key();
        let (_, vk2, _) = gen_key2();
        let body = "print('hello')\n";
        let envelope = hash_prefix_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        // Trust store has the fingerprint but mapped to a DIFFERENT key
        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp,
            verifying_key: vk2,
            label: None,
        }]);

        let err = verify_item_signature(&content, &header, &envelope, &store).unwrap_err();
        assert!(
            matches!(err, EngineError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got: {err:?}"
        );
    }

    #[test]
    fn verify_html_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "# My Directive\n\nSome content.\n";
        let envelope = html_envelope();
        let content = build_signed_content(body, &sk, &fp, &envelope);

        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn verify_shebang_envelope_signature() {
        let (sk, vk, fp) = gen_key();
        let body = "print('hello')\n";
        let content = build_signed_content_with_shebang(body, &sk, &fp, "#!/usr/bin/env python3");

        let envelope = shebang_envelope();
        let header = crate::item_resolution::parse_signature_header(&content, &envelope).unwrap();

        let store = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: fp.clone(),
            verifying_key: vk,
            label: None,
        }]);

        let (trust_class, signer) =
            verify_item_signature(&content, &header, &envelope, &store).unwrap();
        assert_eq!(trust_class, TrustClass::Trusted);
        assert_eq!(signer.unwrap().0, fp);
    }

    #[test]
    fn fingerprint_matches_identity_rs_computation() {
        // The fingerprint should be SHA-256 of the raw 32-byte public key
        let (_, vk, fp) = gen_key();
        let expected = sha256_hex(vk.as_bytes());
        assert_eq!(fp, expected);
    }
}
