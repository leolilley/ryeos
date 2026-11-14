//! Node-owned signer registry: record format, key events, typed refusals.
//!
//! The registry lives under `<app_root>/.ai/node/trust/`:
//!
//! ```text
//! signers/<fingerprint>.yaml       one node-signed record per signer
//! events/<seq>-<digest>.yaml       node-signed, hash-chained key events
//! ```
//!
//! The node key is the root of trust. It has no record: the loader takes it
//! from the node's public identity document and every record and event must
//! verify under it. A record that is unsigned, signed by any other key
//! (including its own), edited after signing, misnamed, or of an unknown
//! schema is refused with a typed [`TrustError`] that names the file and the
//! cure. Nothing else in the process may add a signer to a [`TrustStore`].
//!
//! [`TrustStore`]: super::TrustStore

use std::path::{Path, PathBuf};

use base64::Engine as _;
use lillux::crypto::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::contracts::ItemSpace;

/// Schema identifier of one signer record.
pub const SIGNER_RECORD_SCHEMA: &str = "ryeos/trust-signer/v1";
/// Schema identifier of one key event.
pub const TRUST_EVENT_SCHEMA: &str = "ryeos/trust-event/v1";
/// Schema identifier of a succession statement.
pub const SUCCESSION_SCHEMA: &str = "ryeos/trust-succession/v1";
/// Kind wildcard: the signer may sign items of every kind.
pub const ALL_KINDS: &str = "*";

/// Path of the registry below `<app_root>/.ai`.
pub const NODE_TRUST_DIR: &str = "node/trust";
/// Records directory below [`NODE_TRUST_DIR`].
pub const SIGNERS_DIR: &str = "signers";
/// Events directory below [`NODE_TRUST_DIR`].
pub const EVENTS_DIR: &str = "events";

/// Hard bounds shared by every reader of the registry.
pub const MAX_TRUST_DOCUMENTS: usize = 1024;
pub const MAX_TRUST_DOCUMENT_BYTES: u64 = 1024 * 1024;
pub const MAX_TRUST_DIRECTORY_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TRUST_TRAVERSAL_ENTRIES: usize = MAX_TRUST_DOCUMENTS * 2;
pub const MAX_TRUST_TRAVERSAL_DEPTH: usize = 2;
pub const MAX_TRUST_EVENTS: usize = 65_536;

/// `<app_root>/.ai/node/trust/signers`.
pub fn signers_dir(app_root: &Path) -> PathBuf {
    app_root
        .join(crate::AI_DIR)
        .join(NODE_TRUST_DIR)
        .join(SIGNERS_DIR)
}

/// `<app_root>/.ai/node/trust/events`.
pub fn events_dir(app_root: &Path) -> PathBuf {
    app_root
        .join(crate::AI_DIR)
        .join(NODE_TRUST_DIR)
        .join(EVENTS_DIR)
}

/// `<app_root>/.ai/node/identity/public-identity.json`.
pub fn node_public_identity_path(app_root: &Path) -> PathBuf {
    app_root
        .join(crate::AI_DIR)
        .join("node")
        .join("identity")
        .join("public-identity.json")
}

// ── Typed refusals ──────────────────────────────────────────────────

/// Every registry refusal carries a stable `code()`, names what was seen and
/// states the cure.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error(
        "trust_record_unsigned: {path} carries no `# ryeos:signed:` line. Registry records \
         must be signed by this node's key. Remove the file with the node stopped and \
         re-pin the signer with `ryeos trust pin`"
    )]
    RecordUnsigned { path: PathBuf },

    #[error(
        "trust_record_wrong_signer: {path} is signed by {signer}, not by this node's key \
         {node}. Only the node key may sign registry records. Remove the file with the \
         node stopped and re-pin the signer with `ryeos trust pin`"
    )]
    RecordWrongSigner {
        path: PathBuf,
        signer: String,
        node: String,
    },

    #[error(
        "trust_record_tampered: {path} {reason}. The record was edited after signing or is \
         malformed. Remove the file with the node stopped and re-pin the signer with \
         `ryeos trust pin`"
    )]
    RecordTampered { path: PathBuf, reason: String },

    #[error(
        "trust_record_name_mismatch: {path} {reason}. Registry files are named \
         `<fingerprint>.yaml` after the key they describe. Remove the file with the node \
         stopped and re-pin the signer with `ryeos trust pin`"
    )]
    RecordNameMismatch { path: PathBuf, reason: String },

    #[error(
        "trust_record_schema_unsupported: {path} declares schema `{seen}`, this node reads \
         `{expected}`. Re-pin the signer with `ryeos trust pin` using a current RyeOS build"
    )]
    RecordSchemaUnsupported {
        path: PathBuf,
        seen: String,
        expected: String,
    },

    #[error(
        "trust_signer_revoked: signer {fingerprint} ({label}) is revoked and verifies \
         nothing. Inspect it with `ryeos trust show {fingerprint}`; a revoked key cannot be \
         pinned again, so pin a new key instead"
    )]
    SignerRevoked { fingerprint: String, label: String },

    #[error(
        "signer_out_of_scope: signer {fingerprint} ({label}) is pinned with scope {scope} \
         but signed a `{kind}` item in {space} space{project}. Widen the scope with \
         `ryeos trust scope {fingerprint} ...` or sign with a key whose scope covers this item"
    )]
    SignerOutOfScope {
        fingerprint: String,
        label: String,
        scope: String,
        kind: String,
        space: String,
        /// Pre-rendered ` in project <root>` suffix, empty when the item has
        /// no project root.
        project: String,
    },

    #[error(
        "trust_event_chain_broken: {path} {reason}. The key-event log is hash-chained and \
         node-signed; restore it from a backup of the node state"
    )]
    EventChainBroken { path: PathBuf, reason: String },

    #[error(
        "trust_node_identity_unavailable: node public identity {path} cannot be read: \
         {reason}. The registry root of trust is the node key; run `ryeos init` or restore \
         the node identity"
    )]
    NodeIdentityUnavailable { path: PathBuf, reason: String },

    #[error("trust_registry_unreadable: {path} cannot be read securely: {reason}")]
    Unreadable { path: PathBuf, reason: String },
}

impl TrustError {
    /// Stable machine code.
    pub fn code(&self) -> &'static str {
        match self {
            TrustError::RecordUnsigned { .. } => "trust_record_unsigned",
            TrustError::RecordWrongSigner { .. } => "trust_record_wrong_signer",
            TrustError::RecordTampered { .. } => "trust_record_tampered",
            TrustError::RecordNameMismatch { .. } => "trust_record_name_mismatch",
            TrustError::RecordSchemaUnsupported { .. } => "trust_record_schema_unsupported",
            TrustError::SignerRevoked { .. } => "trust_signer_revoked",
            TrustError::SignerOutOfScope { .. } => "signer_out_of_scope",
            TrustError::EventChainBroken { .. } => "trust_event_chain_broken",
            TrustError::NodeIdentityUnavailable { .. } => "trust_node_identity_unavailable",
            TrustError::Unreadable { .. } => "trust_registry_unreadable",
        }
    }
}

// ── Record model ────────────────────────────────────────────────────

/// What a signer is for. Roles select default scopes; the scope on the
/// record is what verification judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerRole {
    Publisher,
    Operator,
    ProjectAuthor,
    /// The node-owned key the daemon signs runtime-authored items with.
    Authoring,
}

impl SignerRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SignerRole::Publisher => "publisher",
            SignerRole::Operator => "operator",
            SignerRole::ProjectAuthor => "project_author",
            SignerRole::Authoring => "authoring",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "publisher" => Some(SignerRole::Publisher),
            "operator" => Some(SignerRole::Operator),
            "project_author" => Some(SignerRole::ProjectAuthor),
            "authoring" => Some(SignerRole::Authoring),
            _ => None,
        }
    }
}

/// Item space a signer may sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerSpace {
    Bundle,
    Project,
    Node,
}

impl SignerSpace {
    pub fn as_str(self) -> &'static str {
        match self {
            SignerSpace::Bundle => "bundle",
            SignerSpace::Project => "project",
            SignerSpace::Node => "node",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "bundle" => Some(SignerSpace::Bundle),
            "project" => Some(SignerSpace::Project),
            "node" => Some(SignerSpace::Node),
            _ => None,
        }
    }
}

impl From<ItemSpace> for SignerSpace {
    fn from(space: ItemSpace) -> Self {
        match space {
            ItemSpace::Bundle => SignerSpace::Bundle,
            ItemSpace::Project => SignerSpace::Project,
            ItemSpace::Node => SignerSpace::Node,
        }
    }
}

/// Lifecycle of a record. `retired` still verifies existing items;
/// `revoked` verifies nothing and is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerStatus {
    Active,
    Retired,
    Revoked,
}

impl SignerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SignerStatus::Active => "active",
            SignerStatus::Retired => "retired",
            SignerStatus::Revoked => "revoked",
        }
    }
}

/// Where, and for which kinds, a signer's signature counts as trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerScope {
    /// Spaces the signer may sign in. Never empty.
    pub spaces: Vec<SignerSpace>,
    /// `["*"]` or an explicit list of item kinds. Empty admits no kind.
    pub kinds: Vec<String>,
    /// Project roots the signer may sign for; empty means every project.
    /// Only meaningful when `spaces` contains `project`.
    #[serde(default)]
    pub projects: Vec<String>,
}

impl SignerScope {
    /// Default scope of a publisher: bundle space, every kind.
    pub fn publisher_default() -> Self {
        Self {
            spaces: vec![SignerSpace::Bundle],
            kinds: vec![ALL_KINDS.to_string()],
            projects: Vec::new(),
        }
    }

    /// Default scope of the operator: project and node space, every kind.
    pub fn operator_default() -> Self {
        Self {
            spaces: vec![SignerSpace::Project, SignerSpace::Node],
            kinds: vec![ALL_KINDS.to_string()],
            projects: Vec::new(),
        }
    }

    /// Default scope of the node's authoring key: project space and no
    /// kind. Items it signs are machine drafts until the operator widens the
    /// scope for a kind or re-signs the item.
    pub fn authoring_default() -> Self {
        Self {
            spaces: vec![SignerSpace::Project],
            kinds: Vec::new(),
            projects: Vec::new(),
        }
    }

    /// Every space and kind. For fixtures that stand in for a fully
    /// trusted signer; never a production default.
    pub fn everywhere() -> Self {
        Self {
            spaces: vec![SignerSpace::Bundle, SignerSpace::Project, SignerSpace::Node],
            kinds: vec![ALL_KINDS.to_string()],
            projects: Vec::new(),
        }
    }

    /// Whether the scope admits one signature context.
    pub fn admits(&self, space: SignerSpace, kind: &str, project_root: Option<&Path>) -> bool {
        if !self.spaces.contains(&space) {
            return false;
        }
        if !self.kinds.iter().any(|k| k == ALL_KINDS || k == kind) {
            return false;
        }
        if space == SignerSpace::Project && !self.projects.is_empty() {
            let Some(root) = project_root else {
                return false;
            };
            return self.projects.iter().any(|p| Path::new(p) == root);
        }
        true
    }

    /// Compact human-readable rendering used in refusals and listings.
    pub fn describe(&self) -> String {
        let spaces = self
            .spaces
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let mut out = format!("{{spaces: [{spaces}], kinds: [{}]", self.kinds.join(","));
        if !self.projects.is_empty() {
            out.push_str(&format!(", projects: [{}]", self.projects.join(",")));
        }
        out.push('}');
        out
    }

    /// Structural validity shared by the parser and every writer.
    pub fn validate(&self) -> Result<(), String> {
        if self.spaces.is_empty() {
            return Err("scope.spaces must list at least one space".to_string());
        }
        let mut seen = std::collections::BTreeSet::new();
        for space in &self.spaces {
            if !seen.insert(*space) {
                return Err(format!("scope.spaces lists `{}` twice", space.as_str()));
            }
        }
        if self.kinds.iter().any(|k| k == ALL_KINDS) && self.kinds.len() != 1 {
            return Err("scope.kinds mixes \"*\" with named kinds".to_string());
        }
        if self.kinds.iter().any(|k| k.trim().is_empty()) {
            return Err("scope.kinds contains an empty kind".to_string());
        }
        if !self.projects.is_empty() && !self.spaces.contains(&SignerSpace::Project) {
            return Err("scope.projects is only meaningful with the `project` space".to_string());
        }
        for project in &self.projects {
            if !Path::new(project).is_absolute() {
                return Err(format!("scope.projects entry `{project}` is not absolute"));
            }
        }
        Ok(())
    }
}

/// One node-signed signer record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerRecord {
    pub schema: String,
    pub fingerprint: String,
    /// `ed25519:<base64>`.
    pub public_key: String,
    /// Informational label.
    pub label: String,
    pub role: SignerRole,
    pub scope: SignerScope,
    pub status: SignerStatus,
    /// Digest of the key event that produced this state.
    pub event: String,
}

impl SignerRecord {
    /// Build a record for a key. `event` is filled by the writer once the
    /// event that produces the record exists.
    pub fn new(
        verifying_key: &VerifyingKey,
        label: &str,
        role: SignerRole,
        scope: SignerScope,
        status: SignerStatus,
    ) -> Self {
        Self {
            schema: SIGNER_RECORD_SCHEMA.to_string(),
            fingerprint: super::compute_fingerprint(verifying_key),
            public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes())
            ),
            label: label.to_string(),
            role,
            scope,
            status,
            event: String::new(),
        }
    }

    /// Decode `public_key` and check it hashes to `fingerprint`.
    pub fn verifying_key(&self) -> Result<VerifyingKey, String> {
        let encoded = self
            .public_key
            .strip_prefix("ed25519:")
            .ok_or_else(|| "public_key must be ed25519:<base64>".to_string())?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("public_key is not valid base64: {e}"))?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "public_key must decode to 32 bytes".to_string())?;
        let key = VerifyingKey::from_bytes(&array)
            .map_err(|e| format!("public_key is not an Ed25519 key: {e}"))?;
        let actual = super::compute_fingerprint(&key);
        if actual != self.fingerprint {
            return Err(format!(
                "public_key hashes to {actual}, not the declared fingerprint {}",
                self.fingerprint
            ));
        }
        Ok(key)
    }

    /// Digest of the record without its `event` field. This is what a key
    /// event commits to; the record then names the event.
    pub fn content_digest(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.remove("event");
        }
        digest_value(&value)
    }

    /// Digest of the complete record including `event`.
    pub fn full_digest(&self) -> String {
        digest_value(&serde_json::to_value(self).unwrap_or(serde_json::Value::Null))
    }

    /// Render the node-signed on-disk form.
    pub fn render_signed(&self, node_key: &SigningKey) -> Result<String, TrustError> {
        let body = serde_yaml::to_string(self).map_err(|e| TrustError::RecordTampered {
            path: PathBuf::from(format!("{}.yaml", self.fingerprint)),
            reason: format!("cannot serialize record: {e}"),
        })?;
        Ok(lillux::signature::sign_content(&body, node_key, "#", None))
    }

    /// File name of this record inside `signers/`.
    pub fn file_name(&self) -> String {
        format!("{}.yaml", self.fingerprint)
    }
}

fn digest_value(value: &serde_json::Value) -> String {
    match lillux::cas::canonical_json(value) {
        Ok(text) => lillux::cas::sha256_hex(text.as_bytes()),
        // Plain records and events never contain non-finite numbers; hashing
        // the lossy rendering keeps this infallible without hiding a change.
        Err(_) => lillux::cas::sha256_hex(value.to_string().as_bytes()),
    }
}

/// Digest over a set of records: sorted by fingerprint, each contributing
/// its full digest. Changes with any status, scope or event change.
pub fn registry_digest<'a>(records: impl IntoIterator<Item = &'a SignerRecord>) -> String {
    let mut entries = records
        .into_iter()
        .map(|record| (record.fingerprint.clone(), record.full_digest()))
        .collect::<Vec<_>>();
    entries.sort();
    let mut bytes = Vec::new();
    for (fingerprint, digest) in entries {
        for field in [fingerprint.as_bytes(), digest.as_bytes()] {
            bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
            bytes.extend_from_slice(field);
        }
    }
    lillux::cas::sha256_hex(&bytes)
}

// ── Signer entries handed to runtimes ───────────────────────────────

/// One verified signer as handed to a runtime inside its launch envelope.
/// The daemon has already verified the registry; the runtime only matches
/// signatures against these keys and judges their scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedSignerEntry {
    pub fingerprint: String,
    /// `ed25519:<base64>`.
    pub public_key: String,
    pub label: String,
    pub scope: SignerScope,
}

/// Digest of a signer-entry list, order independent. Runtimes compare it to
/// the digest the daemon sealed with the launch.
pub fn trusted_signers_digest(entries: &[TrustedSignerEntry]) -> String {
    let mut sorted = entries.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
    let value = serde_json::to_value(&sorted).unwrap_or(serde_json::Value::Null);
    digest_value(&value)
}

// ── Signed-document parsing shared by records and events ────────────

fn envelope() -> crate::contracts::SignatureEnvelope {
    crate::contracts::SignatureEnvelope {
        prefix: "#".to_string(),
        suffix: None,
        after_shebang: false,
    }
}

/// Verify a document signed by `node_key` (a `# ryeos:signed:` line) and
/// return its body text without the signature line. Node-owned records of
/// any kind use this one verifier.
pub fn verify_node_signed_text(
    path: &Path,
    bytes: &[u8],
    node_key: &VerifyingKey,
) -> Result<String, TrustError> {
    let content = std::str::from_utf8(bytes).map_err(|_| TrustError::RecordTampered {
        path: path.to_path_buf(),
        reason: "is not UTF-8".to_string(),
    })?;
    let envelope = envelope();
    let Some(header) = crate::item_resolution::parse_signature_header(content, &envelope) else {
        let looks_signed = content.lines().any(|line| line.contains("ryeos:signed:"));
        return Err(if looks_signed {
            TrustError::RecordTampered {
                path: path.to_path_buf(),
                reason: "has a malformed signature line".to_string(),
            }
        } else {
            TrustError::RecordUnsigned {
                path: path.to_path_buf(),
            }
        });
    };
    let node_fingerprint = super::compute_fingerprint(node_key);
    if header.signer_fingerprint != node_fingerprint {
        return Err(TrustError::RecordWrongSigner {
            path: path.to_path_buf(),
            signer: header.signer_fingerprint,
            node: node_fingerprint,
        });
    }
    let actual_hash = super::content_hash_after_signature(content, &envelope).ok_or_else(|| {
        TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: "has no locatable signature line".to_string(),
        }
    })?;
    if actual_hash != header.content_hash {
        return Err(TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: "does not match its signed content hash".to_string(),
        });
    }
    if !lillux::signature::verify_signature(&header.content_hash, &header.signature_b64, node_key) {
        return Err(TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: "carries an invalid node signature".to_string(),
        });
    }
    Ok(lillux::signature::strip_signature_lines_with_envelope(
        content, "#", None,
    ))
}

/// Parse and verify one signer record file.
pub fn parse_signer_record(
    path: &Path,
    bytes: &[u8],
    node_key: &VerifyingKey,
) -> Result<SignerRecord, TrustError> {
    let body = verify_node_signed_text(path, bytes, node_key)?;
    let value: serde_yaml::Value =
        serde_yaml::from_str(&body).map_err(|e| TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: format!("is not valid YAML: {e}"),
        })?;
    let schema = value
        .get("schema")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if schema != SIGNER_RECORD_SCHEMA {
        return Err(TrustError::RecordSchemaUnsupported {
            path: path.to_path_buf(),
            seen: schema,
            expected: SIGNER_RECORD_SCHEMA.to_string(),
        });
    }
    let record: SignerRecord =
        serde_yaml::from_value(value).map_err(|e| TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: format!("does not match the signer record shape: {e}"),
        })?;
    let expected_name = record.file_name();
    let actual_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if actual_name != expected_name {
        return Err(TrustError::RecordNameMismatch {
            path: path.to_path_buf(),
            reason: format!(
                "describes signer {} but is named `{actual_name}`; expected `{expected_name}`",
                record.fingerprint
            ),
        });
    }
    record
        .verifying_key()
        .map_err(|reason| TrustError::RecordNameMismatch {
            path: path.to_path_buf(),
            reason,
        })?;
    record
        .scope
        .validate()
        .map_err(|reason| TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason,
        })?;
    if record.event.is_empty() {
        return Err(TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: "names no key event".to_string(),
        });
    }
    Ok(record)
}

/// Read the node verifying key from the node's public identity document.
pub fn load_node_verifying_key(app_root: &Path) -> Result<VerifyingKey, TrustError> {
    let path = node_public_identity_path(app_root);
    let unavailable = |reason: String| TrustError::NodeIdentityUnavailable {
        path: path.clone(),
        reason,
    };
    let bytes = lillux::open_pinned_regular_file_no_follow(&path)
        .and_then(|file| file.read_bounded(MAX_TRUST_DOCUMENT_BYTES))
        .map_err(|e| unavailable(format!("{e:#}")))?;
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| unavailable(e.to_string()))?;
    let encoded = doc
        .get("signing_key")
        .and_then(|v| v.as_str())
        .and_then(|v| v.strip_prefix("ed25519:"))
        .ok_or_else(|| unavailable("missing `signing_key` (ed25519:<base64>)".to_string()))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| unavailable(e.to_string()))?;
    let array: [u8; 32] = raw
        .try_into()
        .map_err(|_| unavailable("signing_key must decode to 32 bytes".to_string()))?;
    let key = VerifyingKey::from_bytes(&array).map_err(|e| unavailable(e.to_string()))?;
    if let Some(principal) = doc.get("principal_id").and_then(|v| v.as_str()) {
        let expected = format!("fp:{}", super::compute_fingerprint(&key));
        if principal != expected {
            return Err(unavailable(format!(
                "principal_id `{principal}` does not name its signing key ({expected})"
            )));
        }
    }
    Ok(key)
}

/// Read every record under `signers_dir`, verifying each under `node_key`.
/// A missing directory is an empty registry.
pub fn load_signer_records(
    signers_dir: &Path,
    node_key: &VerifyingKey,
) -> Result<Vec<SignerRecord>, TrustError> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut count = 0_usize;
    let mut total = 0_u64;
    let refusal = |path: &Path, reason: String| TrustError::Unreadable {
        path: path.to_path_buf(),
        reason,
    };
    let mut stray: Option<PathBuf> = None;
    lillux::visit_regular_files_no_follow_bounded(
        signers_dir,
        lillux::DirectoryTraversalBudget::new(
            MAX_TRUST_TRAVERSAL_ENTRIES,
            MAX_TRUST_TRAVERSAL_DEPTH,
        ),
        |relative, is_directory| {
            if is_directory {
                stray.get_or_insert_with(|| signers_dir.join(relative));
            }
            Ok(is_directory)
        },
        |relative, file| {
            count += 1;
            if count > MAX_TRUST_DOCUMENTS {
                anyhow::bail!("registry exceeds {MAX_TRUST_DOCUMENTS} records");
            }
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
            total = total.saturating_add(bytes.len() as u64);
            if total > MAX_TRUST_DIRECTORY_BYTES {
                anyhow::bail!("registry exceeds {MAX_TRUST_DIRECTORY_BYTES} aggregate bytes");
            }
            files.push((signers_dir.join(relative), bytes));
            Ok(())
        },
    )
    .map_err(|e| refusal(signers_dir, format!("{e:#}")))?;
    if let Some(path) = stray {
        return Err(TrustError::RecordNameMismatch {
            path,
            reason: "is a directory; the registry holds only `<fingerprint>.yaml` records"
                .to_string(),
        });
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut records = Vec::with_capacity(files.len());
    for (path, bytes) in files {
        let is_yaml = path.extension().and_then(|e| e.to_str()) == Some("yaml");
        if !is_yaml {
            return Err(TrustError::RecordNameMismatch {
                path,
                reason: "is not a `<fingerprint>.yaml` record".to_string(),
            });
        }
        records.push(parse_signer_record(&path, &bytes, node_key)?);
    }
    Ok(records)
}

// ── Key events ──────────────────────────────────────────────────────

/// What a key event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustEventKind {
    Init,
    Pinned,
    ScopeChanged,
    Retired,
    Revoked,
    Succeeded,
}

impl TrustEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustEventKind::Init => "init",
            TrustEventKind::Pinned => "pinned",
            TrustEventKind::ScopeChanged => "scope_changed",
            TrustEventKind::Retired => "retired",
            TrustEventKind::Revoked => "revoked",
            TrustEventKind::Succeeded => "succeeded",
        }
    }
}

/// One node-signed, hash-chained key event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustEvent {
    pub schema: String,
    pub seq: u64,
    /// Digest of the previous event; absent for the first event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    pub kind: TrustEventKind,
    /// Fingerprint of the signer the event is about.
    pub signer: String,
    /// `SignerRecord::content_digest` of the resulting record.
    pub record_digest: String,
    /// Offline request digest, or the init completion digest.
    pub request_digest: String,
    /// Fingerprint of the operator who signed the request.
    pub operator: String,
    /// For a `succeeded` event, the fingerprint of the key whose succession
    /// statement admitted `signer`. Absent on every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<String>,
}

impl TrustEvent {
    /// Digest of the event body; the chain links through it.
    pub fn digest(&self) -> String {
        digest_value(&serde_json::to_value(self).unwrap_or(serde_json::Value::Null))
    }

    /// File name inside `events/`.
    pub fn file_name(&self) -> String {
        format!("{:08}-{}.yaml", self.seq, self.digest())
    }

    pub fn render_signed(&self, node_key: &SigningKey) -> Result<String, TrustError> {
        let body = serde_yaml::to_string(self).map_err(|e| TrustError::EventChainBroken {
            path: PathBuf::from(self.file_name()),
            reason: format!("cannot serialize event: {e}"),
        })?;
        Ok(lillux::signature::sign_content(&body, node_key, "#", None))
    }
}

/// Parse and verify one event file.
pub fn parse_trust_event(
    path: &Path,
    bytes: &[u8],
    node_key: &VerifyingKey,
) -> Result<TrustEvent, TrustError> {
    let body = verify_node_signed_text(path, bytes, node_key)?;
    let value: serde_yaml::Value =
        serde_yaml::from_str(&body).map_err(|e| TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: format!("is not valid YAML: {e}"),
        })?;
    let schema = value
        .get("schema")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if schema != TRUST_EVENT_SCHEMA {
        return Err(TrustError::RecordSchemaUnsupported {
            path: path.to_path_buf(),
            seen: schema,
            expected: TRUST_EVENT_SCHEMA.to_string(),
        });
    }
    let event: TrustEvent =
        serde_yaml::from_value(value).map_err(|e| TrustError::RecordTampered {
            path: path.to_path_buf(),
            reason: format!("does not match the key event shape: {e}"),
        })?;
    let expected = event.file_name();
    let actual = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if actual != expected {
        return Err(TrustError::RecordNameMismatch {
            path: path.to_path_buf(),
            reason: format!("is named `{actual}`; expected `{expected}`"),
        });
    }
    Ok(event)
}

/// Load the whole event chain in sequence order and verify its links:
/// contiguous sequence from 1, each `previous` equal to the prior event's
/// digest, every event node-signed. A missing directory is an empty chain.
pub fn load_event_chain(
    events_dir: &Path,
    node_key: &VerifyingKey,
) -> Result<Vec<TrustEvent>, TrustError> {
    Ok(load_event_chain_timed(events_dir, node_key)?
        .into_iter()
        .map(|(event, _)| event)
        .collect())
}

/// [`load_event_chain`] with the time each event was signed, as written in
/// its signature line.
pub fn load_event_chain_timed(
    events_dir: &Path,
    node_key: &VerifyingKey,
) -> Result<Vec<(TrustEvent, String)>, TrustError> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut count = 0_usize;
    lillux::visit_regular_files_no_follow_bounded(
        events_dir,
        lillux::DirectoryTraversalBudget::new(
            MAX_TRUST_EVENTS.saturating_mul(2),
            MAX_TRUST_TRAVERSAL_DEPTH,
        ),
        |_relative, is_directory| Ok(is_directory),
        |relative, file| {
            count += 1;
            if count > MAX_TRUST_EVENTS {
                anyhow::bail!("event log exceeds {MAX_TRUST_EVENTS} events");
            }
            let bytes = lillux::read_open_regular_file_bounded(file, MAX_TRUST_DOCUMENT_BYTES)?;
            files.push((events_dir.join(relative), bytes));
            Ok(())
        },
    )
    .map_err(|e| TrustError::Unreadable {
        path: events_dir.to_path_buf(),
        reason: format!("{e:#}"),
    })?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut chain: Vec<(TrustEvent, String)> = Vec::with_capacity(files.len());
    for (path, bytes) in files {
        let event = parse_trust_event(&path, &bytes, node_key)?;
        let signed_at = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| crate::item_resolution::parse_signature_header(text, &envelope()))
            .map(|header| header.timestamp)
            .unwrap_or_default();
        let expected_seq = chain.len() as u64 + 1;
        if event.seq != expected_seq {
            return Err(TrustError::EventChainBroken {
                path,
                reason: format!(
                    "has sequence {} where {expected_seq} was expected (gap or reorder)",
                    event.seq
                ),
            });
        }
        let expected_previous = chain.last().map(|(event, _)| event.digest());
        if event.previous != expected_previous {
            return Err(TrustError::EventChainBroken {
                path,
                reason: "does not link to the digest of the previous event".to_string(),
            });
        }
        chain.push((event, signed_at));
    }
    Ok(chain)
}

/// When a signer's key entered the registry: the signing time of the first
/// `init`, `pinned` or `succeeded` event about it. `None` when the chain holds
/// no such event.
pub fn key_pinned_at(chain: &[(TrustEvent, String)], fingerprint: &str) -> Option<String> {
    chain
        .iter()
        .find(|(event, _)| {
            event.signer == fingerprint
                && matches!(
                    event.kind,
                    TrustEventKind::Init | TrustEventKind::Pinned | TrustEventKind::Succeeded
                )
        })
        .map(|(_, at)| at.clone())
        .filter(|at| !at.is_empty())
}

/// Cross-check records against the event chain: every record must name an
/// event that is about the same signer and committed to the record's
/// content digest.
pub fn cross_check_records_against_events(
    records: &[SignerRecord],
    chain: &[TrustEvent],
) -> Result<(), TrustError> {
    for record in records {
        let event = chain.iter().find(|event| event.digest() == record.event);
        let consistent = event.is_some_and(|event| {
            event.signer == record.fingerprint && event.record_digest == record.content_digest()
        });
        if !consistent {
            return Err(TrustError::EventChainBroken {
                path: PathBuf::from(record.file_name()),
                reason: format!(
                    "names event {} which is absent from the chain or does not commit to \
                     this record",
                    record.event
                ),
            });
        }
    }
    Ok(())
}

// ── Succession statements ───────────────────────────────────────────

/// A statement signed by an old key naming its successor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessionStatement {
    pub schema: String,
    pub old_fingerprint: String,
    /// `ed25519:<base64>` of the new key.
    pub new_public_key: String,
    pub new_fingerprint: String,
    pub issued_at: String,
}

impl SuccessionStatement {
    pub fn new(old_fingerprint: &str, new_key: &VerifyingKey, issued_at: &str) -> Self {
        Self {
            schema: SUCCESSION_SCHEMA.to_string(),
            old_fingerprint: old_fingerprint.to_string(),
            new_public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(new_key.as_bytes())
            ),
            new_fingerprint: super::compute_fingerprint(new_key),
            issued_at: issued_at.to_string(),
        }
    }

    /// Render signed by the old key.
    pub fn render_signed(&self, old_key: &SigningKey) -> Result<String, String> {
        let body = serde_yaml::to_string(self).map_err(|e| e.to_string())?;
        Ok(lillux::signature::sign_content(&body, old_key, "#", None))
    }

    /// Read a statement's fields without verifying its signature. Only for
    /// finding which record to verify it against; trust nothing it says until
    /// [`Self::parse_and_verify`] accepts it.
    pub fn parse_unverified(text: &str) -> Result<Self, String> {
        let body = lillux::signature::strip_signature_lines_with_envelope(text, "#", None);
        serde_yaml::from_str(&body).map_err(|e| format!("invalid succession statement: {e}"))
    }

    /// Parse a statement and verify it was signed by `old_record`'s key.
    pub fn parse_and_verify(text: &str, old_record: &SignerRecord) -> Result<Self, String> {
        let old_key = old_record.verifying_key()?;
        let envelope = envelope();
        let header = crate::item_resolution::parse_signature_header(text, &envelope)
            .ok_or_else(|| "succession statement is unsigned".to_string())?;
        if header.signer_fingerprint != old_record.fingerprint {
            return Err(format!(
                "succession statement is signed by {}, not by the old key {}",
                header.signer_fingerprint, old_record.fingerprint
            ));
        }
        let actual = super::content_hash_after_signature(text, &envelope)
            .ok_or_else(|| "succession statement has no locatable signature line".to_string())?;
        if actual != header.content_hash {
            return Err("succession statement was edited after signing".to_string());
        }
        if !lillux::signature::verify_signature(
            &header.content_hash,
            &header.signature_b64,
            &old_key,
        ) {
            return Err("succession statement carries an invalid signature".to_string());
        }
        let body = lillux::signature::strip_signature_lines_with_envelope(text, "#", None);
        let statement: SuccessionStatement = serde_yaml::from_str(&body)
            .map_err(|e| format!("invalid succession statement: {e}"))?;
        if statement.schema != SUCCESSION_SCHEMA {
            return Err(format!(
                "succession statement schema `{}` is not `{SUCCESSION_SCHEMA}`",
                statement.schema
            ));
        }
        if statement.old_fingerprint != old_record.fingerprint {
            return Err("succession statement names a different old key".to_string());
        }
        let new_key = statement.new_verifying_key()?;
        if super::compute_fingerprint(&new_key) != statement.new_fingerprint {
            return Err(
                "succession statement's new key does not match its fingerprint".to_string(),
            );
        }
        Ok(statement)
    }

    pub fn new_verifying_key(&self) -> Result<VerifyingKey, String> {
        let encoded = self
            .new_public_key
            .strip_prefix("ed25519:")
            .ok_or_else(|| "new_public_key must be ed25519:<base64>".to_string())?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("new_public_key is not valid base64: {e}"))?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "new_public_key must decode to 32 bytes".to_string())?;
        VerifyingKey::from_bytes(&array).map_err(|e| format!("invalid new public key: {e}"))
    }
}
