use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Load the current node fingerprint through its single canonical key owner.
pub fn node_identity_fingerprint(app_root: &Path) -> Result<String> {
    let key_path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("identity")
        .join("private_key.pem");
    let identity = NodeIdentity::load(&key_path).with_context(|| {
        format!(
            "load node identity for node-owned command registration policy from {}",
            key_path.display()
        )
    })?;
    Ok(identity.fingerprint().to_string())
}

#[derive(Clone)]
pub struct NodeIdentity {
    // Immutable identity handles share the original secret allocation.
    signing_key: std::sync::Arc<SigningKey>,
    verifying_key: VerifyingKey,
    fingerprint: String,
}

/// Manual impl so a stray `{:?}` can never serialize the signing key.
impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("fingerprint", &self.fingerprint)
            .field("signing_key", &"<redacted>")
            .finish()
    }
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

impl PublicIdentityDoc {
    /// Verify the public document against its actual key, not its editable
    /// principal-id text. This proves the self-signature, not possession by a
    /// running process; live node authentication remains a separate boundary.
    pub fn verified_fingerprint(&self) -> Result<String> {
        if self.kind != "identity/v1" {
            bail!("public node identity has an unsupported kind");
        }
        let encoded = self
            .signing_key
            .strip_prefix("ed25519:")
            .context("public node identity requires an ed25519 key")?;
        let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(encoded)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid public node key length"))?;
        let key = VerifyingKey::from_bytes(&bytes)?;
        let fingerprint = lillux::sha256_hex(&bytes);
        let principal = format!("fp:{fingerprint}");
        if self.principal_id != principal || self.signature.signer != principal {
            bail!("public node identity does not name its actual signing key");
        }
        let signature = Signature::from_slice(
            &base64::engine::general_purpose::STANDARD.decode(&self.signature.sig)?,
        )?;
        // Exactly the existing authoring contract in build_public_identity_at;
        // do not introduce a second signature envelope for host supervision.
        let payload = lillux::canonical_json(&serde_json::json!({
            "kind": self.kind, "principal_id": self.principal_id,
            "signing_key": self.signing_key, "created_at": self.created_at,
        }))?
        .into_bytes();
        key.verify_strict(&payload, &signature)
            .context("invalid public node identity self-signature")?;
        Ok(fingerprint)
    }
}

impl NodeIdentity {
    /// Generate a new signing key and persist. Errors if key already exists.
    pub fn create(key_path: &Path) -> Result<Self> {
        let signing_key = lillux::crypto::create_signing_key(key_path)?;
        Self::from_signing_key(signing_key)
    }

    /// Load existing signing key. Errors if missing.
    pub fn load(key_path: &Path) -> Result<Self> {
        let signing_key = lillux::crypto::load_signing_key(key_path).with_context(|| {
            format!(
                "signing key not found at {} — run 'ryeos init' first",
                key_path.display()
            )
        })?;
        Self::from_signing_key(signing_key)
    }

    fn from_signing_key(signing_key: SigningKey) -> Result<Self> {
        let verifying_key = signing_key.verifying_key();
        let fingerprint = lillux::sha256_hex(verifying_key.as_bytes());
        Ok(Self {
            signing_key: std::sync::Arc::new(signing_key),
            verifying_key,
            fingerprint,
        })
    }

    /// Write a stable public identity document to disk. Uses
    /// `iso8601_now()` for `created_at`/`signed_at`.
    pub fn write_public_identity(&self, path: &Path) -> Result<()> {
        self.write_public_identity_at(path, &lillux::time::iso8601_now())
    }

    /// Like [`Self::write_public_identity`] but takes the timestamp explicitly,
    /// for byte-deterministic test fixtures.
    pub fn write_public_identity_at(&self, path: &Path, now: &str) -> Result<()> {
        let doc = self.build_public_identity_at(now)?;
        let json = serde_json::to_vec_pretty(&doc)?;
        let parent_path = path
            .parent()
            .context("public identity path has no parent")?;
        let name = path
            .file_name()
            .context("public identity path has no filename")?;
        let parent = lillux::PinnedDirectory::open_or_create(parent_path)?;
        let incumbent = parent.open_pinned_regular(name, false)?;
        let state = incumbent
            .as_ref()
            .map(|file| {
                let observation = file.observation()?;
                let bytes = file.read_stable_bounded(&observation, 1024 * 1024)?;
                Ok::<_, anyhow::Error>((observation, bytes))
            })
            .transpose()?;
        match state {
            Some((observation, incumbent_bytes)) => parent
                .replace_pinned_bytes_if_matches_atomic(
                    name,
                    incumbent.as_ref(),
                    move |current| {
                        let current_observation = current.observation()?;
                        anyhow::ensure!(
                            current_observation.matches_quarantined_incumbent(&observation),
                            "public identity changed before publication"
                        );
                        anyhow::ensure!(
                            current.read_stable_bounded(&current_observation, 1024 * 1024)?
                                == incumbent_bytes,
                            "public identity bytes changed before publication"
                        );
                        Ok(())
                    },
                    &json,
                    0o644,
                )
                .map_err(anyhow::Error::from)?,
            None => parent
                .replace_pinned_bytes_if_matches_atomic(name, None, |_| Ok(()), &json, 0o644)
                .map_err(anyhow::Error::from)?,
        }
        Ok(())
    }

    /// Load a persisted public identity document.
    pub fn load_public_identity(path: &Path) -> Result<PublicIdentityDoc> {
        let data =
            lillux::read_regular_file_bounded_no_follow(path, 1024 * 1024).with_context(|| {
                format!(
                    "public identity not found at {} — run 'ryeos init' first",
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
        let payload = lillux::canonical_json(&unsigned)?.into_bytes();
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

    /// Stable federation site identity derived from the node signing
    /// authority. It is independent of hostnames, app-root paths, and local
    /// supervisor labels, so moving or renaming a host cannot silently change
    /// execution-chain authority.
    pub fn site_id(&self) -> String {
        format!("site:{}", self.fingerprint)
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

/// Policy governing whether wildcard scopes are permitted in
/// an authorized-key TOML write.
///
/// Wildcard delegation is dangerous: wildcard entries authorize broad
/// capability ranges. Only two paths legitimately need it:
///
/// 1. Operator bootstrap — the node's own operator key gets `["*"]`
///    so the operator can administer everything on their own node.
/// 2. The local `ryeos authorize-key --allow-wildcard` CLI, when the
///    operator explicitly opts in.
///
/// Every other path must use [`WildcardPolicy::Reject`]. The
/// `AllowBootstrap` variant exists in the public API but is meant to
/// be constructed only by those two callsites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WildcardPolicy {
    /// Reject any scope containing `"*"`. The normal path for all
    /// delegated authorize-key writes.
    Reject,
    /// Permit wildcard scopes. Use only during operator bootstrap or when the
    /// local CLI is invoked with `--allow-wildcard`.
    AllowBootstrap,
}

/// Schema of the node-signed grant body. Version 3 added the required
/// `expires_at` and `authorized_by` fields and renamed the forwarded-principal
/// class to `remote_principal`.
pub const AUTHORIZED_KEY_SCHEMA_VERSION: u32 = 3;

/// Prefix of every offline-operation scope. An offline operation is
/// authorized only by a grant that names its exact scope; no wildcard pattern
/// satisfies one, and no grant may carry a wildcard under this prefix.
pub const OFFLINE_OPERATION_SCOPE_PREFIX: &str = "ryeos.offline.";

/// Whether `scope` is a wildcard pattern under the offline-operation prefix.
/// Such a pattern could only ever express "every offline operation", which no
/// grant may hold.
pub fn is_offline_scope_wildcard(scope: &str) -> bool {
    scope.starts_with(OFFLINE_OPERATION_SCOPE_PREFIX) && scope.contains('*')
}

/// Whether a grant's scopes reach `required` only through a wildcard pattern,
/// that is, ordinary wildcard matching would admit it although no scope names
/// it exactly. Offline operations never accept such a grant; this reports the
/// ones that would have matched.
pub fn matches_only_by_wildcard(scopes: &[String], required: &str) -> bool {
    !scopes.iter().any(|scope| scope == required)
        && scopes
            .iter()
            .any(|scope| ryeos_runtime::authorizer::cap_matches(scope, required))
}

/// Refuse every scope under the offline-operation prefix. Writers that act
/// for anyone but the node's local operator use this: only the local operator
/// may put an offline scope into a grant.
pub fn reject_offline_scopes(scopes: &[String]) -> Result<()> {
    if let Some(scope) = scopes
        .iter()
        .find(|scope| scope.starts_with(OFFLINE_OPERATION_SCOPE_PREFIX))
    {
        bail!(
            "scope `{scope}` is an offline-operation scope; only the node's local operator may grant one"
        );
    }
    Ok(())
}

/// Refuse any scope that is a wildcard under the offline-operation prefix.
pub fn reject_offline_scope_wildcards(scopes: &[String]) -> Result<()> {
    if let Some(scope) = scopes.iter().find(|scope| is_offline_scope_wildcard(scope)) {
        bail!(
            "scope `{scope}` is a wildcard under `{OFFLINE_OPERATION_SCOPE_PREFIX}`; \
             offline operations are granted one exact scope at a time"
        );
    }
    Ok(())
}

/// Exact capability an admitted source-node key must carry before it may
/// co-sign a configured operator's forwarded request.
pub const FORWARDED_OPERATOR_ATTESTATION_SCOPE: &str = "ryeos.attest.request.forwarded-operator";

/// Validate the one wire spelling accepted for authenticated RyeOS site
/// identities. Site ids are protocol identifiers, not display names: keeping
/// the alphabet deliberately small makes the value byte-stable across TOML,
/// JSON, signatures, and remote admission claims.
pub fn validate_canonical_site_id(site_id: &str) -> Result<()> {
    ryeos_engine::principal_contract::validate_canonical_site_id(site_id)
}

/// Check a caller-signed forwarding-origin assertion against origin verified
/// from the source-node co-signature and both target-signed grants. The
/// assertion can only narrow and never creates origin authority.
pub fn validate_forwarding_origin_assertion(
    required_origin_site_id: Option<&str>,
    principal_class: Option<AuthorizedKeyPrincipalClass>,
    authenticated_origin_site_id: Option<&str>,
) -> Result<()> {
    let Some(required_origin_site_id) = required_origin_site_id else {
        if principal_class == Some(AuthorizedKeyPrincipalClass::RemotePrincipal) {
            bail!("remote_principal execution requires a signed forwarding-origin assertion");
        }
        return Ok(());
    };
    if principal_class != Some(AuthorizedKeyPrincipalClass::RemotePrincipal) {
        bail!("forwarding-origin assertions are valid only for remote_principal principals");
    }
    validate_canonical_site_id(required_origin_site_id)?;
    if authenticated_origin_site_id != Some(required_origin_site_id) {
        bail!("authenticated origin does not match the signed forwarding assertion");
    }
    Ok(())
}

pub use ryeos_engine::principal_contract::AuthorizedKeyPrincipalClass;

/// Closed create-only publication failure used by online authorization paths.
/// Callers may expose this as a conflict without parsing an anyhow message;
/// all other publication failures remain internal errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthorizedKeyCreateError {
    #[error("authorized-key fingerprint already has a grant")]
    AlreadyExists,
}

/// When a grant stops authenticating its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantExpiry {
    /// The grant never lapses. Only the node's configured operator grant is
    /// issued this way.
    Never,
    /// The grant lapses at this Unix second: from then on it authenticates
    /// nothing.
    At(u64),
}

const GRANT_NEVER_EXPIRES: &str = "never";

impl GrantExpiry {
    /// The exact text a grant body carries: `never` or an RFC 3339 UTC time.
    pub fn to_grant_text(self) -> String {
        match self {
            Self::Never => GRANT_NEVER_EXPIRES.to_owned(),
            Self::At(secs) => lillux::time::iso8601_from_unix_secs(secs),
        }
    }

    pub fn from_grant_text(text: &str) -> Result<Self> {
        if text == GRANT_NEVER_EXPIRES {
            return Ok(Self::Never);
        }
        let secs = lillux::time::unix_secs_from_iso8601(text).ok_or_else(|| {
            anyhow::anyhow!("authorized-key expires_at must be `never` or an RFC 3339 UTC time")
        })?;
        if lillux::time::iso8601_from_unix_secs(secs) != text {
            bail!("authorized-key expires_at must be written in canonical UTC form");
        }
        Ok(Self::At(secs))
    }

    pub fn is_expired_at(self, now: u64) -> bool {
        matches!(self, Self::At(secs) if now >= secs)
    }
}

/// Who asked for a grant, and the exact signed request that admitted it.
/// The node signs every grant; this records the asking principal's evidence
/// next to that signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedBy {
    /// `fp:<64 hex>` of the key that asked for the grant.
    pub principal: String,
    /// `ryeos-request-v1` content hash of the signed request (or the digest
    /// of the signed claim) that caused the grant. Absent only where no signed
    /// request exists, such as the operator grant written at initialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
}

fn is_lower_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl AuthorizedBy {
    /// The grant was asked for by the key `principal_fingerprint` (bare hex,
    /// with or without the `fp:` prefix), through a signed request.
    pub fn principal_request(principal_fingerprint: &str, request_digest: Option<String>) -> Self {
        let bare = principal_fingerprint
            .strip_prefix("fp:")
            .unwrap_or(principal_fingerprint);
        Self {
            principal: format!("fp:{bare}"),
            request_digest,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self
            .principal
            .strip_prefix("fp:")
            .is_some_and(is_lower_sha256_hex)
        {
            bail!("authorized_by.principal must be `fp:` and a SHA-256 fingerprint");
        }
        if let Some(digest) = &self.request_digest
            && !is_lower_sha256_hex(digest)
        {
            bail!("authorized_by.request_digest must be a lowercase SHA-256 digest");
        }
        Ok(())
    }
}

/// The lifetime and provenance every grant is issued with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantTerms {
    pub expires_at: GrantExpiry,
    pub authorized_by: AuthorizedBy,
}

impl GrantTerms {
    /// Terms of the configured operator's own grant, the one grant that never
    /// lapses. Callers: initialization and the offline authorization reset.
    pub fn operator(authorized_by: AuthorizedBy) -> Self {
        Self {
            expires_at: GrantExpiry::Never,
            authorized_by,
        }
    }

    /// Fixed far-future terms for fixtures that do not exercise expiry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        Self {
            expires_at: GrantExpiry::At(4_102_444_800),
            authorized_by: AuthorizedBy::principal_request(&"9".repeat(64), None),
        }
    }
}

/// A requested grant lifetime the node's authorization policy refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantLifetimeError {
    #[error(
        "a {} grant may live at most {maximum_secs} seconds under this node's authorization \
         policy; {requested_secs} seconds were requested. Request a shorter lifetime",
        class.as_str()
    )]
    ExceedsPolicy {
        class: AuthorizedKeyPrincipalClass,
        requested_secs: u64,
        maximum_secs: u64,
    },
    #[error("a grant lifetime must be at least one second")]
    Zero,
}

impl GrantLifetimeError {
    /// Stable machine-readable code of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExceedsPolicy { .. } => "grant_lifetime_exceeds_policy",
            Self::Zero => "grant_lifetime_invalid",
        }
    }
}

/// A grant this node will not authenticate, with the cure each refusal names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorizedKeyError {
    #[error(
        "authorized-key grant {fingerprint} expired at {expired_at}; ask the principal that \
         granted it or the node operator to renew it with `ryeos renew-key`"
    )]
    Expired {
        fingerprint: String,
        expired_at: String,
    },
    #[error(
        "authorized-key grant {fingerprint} uses schema_version {seen} but this node admits only \
         {admitted}; re-issue it: remove the grant file and authorize the key again with \
         `ryeos authorize-client`, or run `ryeos node reset authorization` on the stopped node \
         and re-issue every key"
    )]
    StaleSchema {
        fingerprint: String,
        seen: u32,
        admitted: u32,
    },
    #[error(
        "authorized-key grant {fingerprint} has the retired class `{class}`; re-admit the key as \
         `remote_principal`: remove the grant file and authorize the key again with \
         `ryeos authorize-client --origin-site-id <site>`"
    )]
    StaleClass { fingerprint: String, class: String },
    #[error(
        "authorized-key grant {fingerprint} never expires but is not the configured operator's \
         grant; re-issue it with a bounded lifetime"
    )]
    NeverExpiresNotOperator { fingerprint: String },
}

impl AuthorizedKeyError {
    /// Stable machine-readable code of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Expired { .. } => "grant_expired",
            Self::StaleSchema { .. } => "grant_schema_stale",
            Self::StaleClass { .. } => "grant_class_stale",
            Self::NeverExpiresNotOperator { .. } => "grant_never_expires_not_operator",
        }
    }
}

/// A renewal this node refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorizedKeyRenewError {
    #[error("no authorized-key grant exists for fingerprint {fingerprint}")]
    NotFound { fingerprint: String },
    #[error(
        "only the principal that granted {fingerprint} or the node operator may renew it; \
         the grantee cannot renew its own grant"
    )]
    NotGranterOrOperator { fingerprint: String },
    #[error("the configured operator's grant never expires and is not renewed")]
    NeverExpires,
    #[error(
        "the renewing principal no longer holds the scope `{scope}` of grant {fingerprint}; \
         renew it as the node operator, or re-issue the grant with narrower scopes"
    )]
    ScopesNoLongerCovered { fingerprint: String, scope: String },
}

impl AuthorizedKeyRenewError {
    /// Stable machine-readable code of the refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "grant_not_found",
            Self::NotGranterOrOperator { .. } => "grant_renewal_not_permitted",
            Self::NeverExpires => "grant_never_expires",
            Self::ScopesNoLongerCovered { .. } => "grant_renewal_scopes_not_covered",
        }
    }
}

/// Exact semantic subject of a node-signed authorized-key grant.
///
/// Scope reconciliation must carry this explicitly. Inferring it from the
/// presence of one optional origin field previously made an admitted
/// `remote_node` grant impossible to maintain without reclassifying it.
#[derive(Debug, Clone, Copy)]
pub enum AuthorizedKeySubject<'a> {
    LocalClient,
    RemoteNode {
        origin_site_id: &'a str,
    },
    /// A configured operator key forwarded by another RyeOS node. The key
    /// remains the workflow owner, while the node-signed grant constrains the
    /// one source site whose separately admitted node key may co-sign it.
    RemotePrincipal {
        origin_site_id: &'a str,
    },
}

#[derive(Serialize)]
struct AuthorizedKeyGrantBody<'a> {
    schema_version: u32,
    principal_class: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_site_id: Option<&'a str>,
    fingerprint: &'a str,
    public_key: String,
    scopes: &'a [String],
    label: &'a str,
    granted_by: &'a str,
    created_at: &'a str,
    expires_at: String,
    // A table, so it stays last in the serialized body.
    authorized_by: &'a AuthorizedBy,
}

/// Just enough of any grant body, current or stale, to name why a stale grant
/// is refused. Deliberately lenient: its job is to read old shapes.
#[derive(Debug, Deserialize)]
struct GrantBodyHeader {
    schema_version: u32,
    #[serde(default)]
    principal_class: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedAuthorizedKeyGrantBody {
    schema_version: u32,
    principal_class: String,
    #[serde(default)]
    origin_site_id: Option<String>,
    fingerprint: String,
    public_key: String,
    scopes: Vec<String>,
    label: String,
    granted_by: String,
    created_at: String,
    expires_at: String,
    authorized_by: AuthorizedBy,
}

/// One node-signed authorized-key grant after exact descriptor-relative read,
/// signature verification, subject binding, and canonical scope validation.
#[derive(Debug, Clone)]
pub struct VerifiedAuthorizedKeyGrant {
    pub public_key: VerifyingKey,
    pub scopes: Vec<String>,
    pub owner: String,
    pub principal_class: AuthorizedKeyPrincipalClass,
    /// Site to which a remote grant is constrained. For `remote_node`, the
    /// subject key itself authenticates this site when it signs a request. For
    /// `remote_principal`, this is only an allow-list constraint until a
    /// separately admitted source-node key co-signs the exact request.
    pub configured_origin_site_id: Option<String>,
    /// Whole-file digest of the exact signed grant bytes used for this view.
    pub source_file_hash: String,
    pub fingerprint: String,
    /// The granting authority: the principal that granted the key, or the
    /// admission token it was admitted with. Renewal belongs to this principal
    /// and the node operator.
    pub granted_by: String,
    pub created_at: String,
    pub expires_at: GrantExpiry,
    pub authorized_by: AuthorizedBy,
}

impl VerifiedAuthorizedKeyGrant {
    /// Refuse a grant that has lapsed at `now` (Unix seconds).
    pub fn ensure_unexpired(&self, now: u64) -> Result<(), AuthorizedKeyError> {
        match self.expires_at {
            GrantExpiry::At(secs) if self.expires_at.is_expired_at(now) => {
                Err(AuthorizedKeyError::Expired {
                    fingerprint: self.fingerprint.clone(),
                    expired_at: lillux::time::iso8601_from_unix_secs(secs),
                })
            }
            _ => Ok(()),
        }
    }

    /// Whether `principal` (`fp:<hex>` or bare hex) is the granting authority
    /// recorded on this grant.
    pub fn was_granted_by(&self, principal: &str) -> bool {
        let bare = |text: &str| text.strip_prefix("fp:").unwrap_or(text).to_owned();
        bare(&self.granted_by) == bare(principal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedKeyTransition {
    pub previous_principal_class: Option<AuthorizedKeyPrincipalClass>,
    pub previous_origin_site_id: Option<String>,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub origin_site_id: Option<String>,
}

/// Load one exact node-signed authorized-key grant that has not lapsed.
/// Absence is distinct from malformed or unverifiable content: callers may
/// treat absence as no prior grant, but must never fold corrupt authority into
/// an empty grant. A lapsed grant fails with [`AuthorizedKeyError::Expired`].
pub fn load_verified_authorized_key(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<Option<VerifiedAuthorizedKeyGrant>> {
    load_verified_authorized_key_at(
        fingerprint,
        auth_dir,
        node_identity,
        lillux::time::unix_secs_now(),
    )
}

/// [`load_verified_authorized_key`] judged at an explicit `now` (Unix seconds).
pub fn load_verified_authorized_key_at(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
    now: u64,
) -> Result<Option<VerifiedAuthorizedKeyGrant>> {
    let Some(grant) = load_verified_authorized_key_unjudged(fingerprint, auth_dir, node_identity)?
    else {
        return Ok(None);
    };
    grant.ensure_unexpired(now)?;
    Ok(Some(grant))
}

/// Load one exact node-signed grant without judging its expiry. Request
/// authentication uses it to verify the caller's signature first, so only the
/// holder of the key learns that its grant lapsed; renewal and inspection use
/// it because they act on lapsed grants. Every other reader must use
/// [`load_verified_authorized_key`].
pub fn load_verified_authorized_key_unjudged(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<Option<VerifiedAuthorizedKeyGrant>> {
    let Some(directory) = lillux::PinnedDirectory::open(auth_dir)? else {
        return Ok(None);
    };
    load_verified_authorized_key_from_directory(fingerprint, &directory, node_identity)
}

fn load_verified_authorized_key_from_directory(
    fingerprint: &str,
    directory: &lillux::PinnedDirectory,
    node_identity: &NodeIdentity,
) -> Result<Option<VerifiedAuthorizedKeyGrant>> {
    let name = format!("{fingerprint}.toml");
    let Some(file) = directory.open_regular(std::ffi::OsStr::new(&name), false)? else {
        directory.ensure_path_binding()?;
        return Ok(None);
    };
    let observation = lillux::observe_open_regular_file(&file)?;
    let bytes = lillux::read_open_regular_file_stable_bounded(&file, &observation, 1024 * 1024)?;
    directory.ensure_regular_entry_matches(std::ffi::OsStr::new(&name), Some(&file))?;
    directory.ensure_path_binding()?;
    let raw = std::str::from_utf8(&bytes).context("authorized-key grant is not UTF-8")?;
    let (body, header) =
        lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
    let header = header.ok_or_else(|| anyhow::anyhow!("unsigned key file"))?;
    if header.signer_fingerprint != node_identity.fingerprint() {
        bail!("wrong signer");
    }
    let actual_hash = lillux::sha256_hex(body.as_bytes());
    if actual_hash != header.content_hash {
        bail!("tampered key file");
    }
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .context("authorized-key signature is not valid base64")?;
    let signature = Signature::from_slice(&sig_bytes)?;
    node_identity.verify_hash(&header.content_hash, &signature)?;

    let header: GrantBodyHeader = toml::from_str(&body)
        .map_err(|error| anyhow::anyhow!("invalid authorized-key grant body: {error}"))?;
    if header.schema_version != AUTHORIZED_KEY_SCHEMA_VERSION {
        if header.principal_class.as_deref() == Some("remote_operator") {
            return Err(AuthorizedKeyError::StaleClass {
                fingerprint: fingerprint.to_owned(),
                class: "remote_operator".to_owned(),
            }
            .into());
        }
        return Err(AuthorizedKeyError::StaleSchema {
            fingerprint: fingerprint.to_owned(),
            seen: header.schema_version,
            admitted: AUTHORIZED_KEY_SCHEMA_VERSION,
        }
        .into());
    }
    let grant: RetainedAuthorizedKeyGrantBody = toml::from_str(&body)
        .map_err(|error| anyhow::anyhow!("invalid authorized-key grant body: {error}"))?;
    if grant.schema_version != AUTHORIZED_KEY_SCHEMA_VERSION {
        bail!("authorized-key grant schema_version changed while it was read");
    }
    let (principal_class, configured_origin_site_id) = match grant.principal_class.as_str() {
        "local_client" => {
            if grant.origin_site_id.is_some() {
                bail!("local_client authorized-key grant cannot carry origin_site_id");
            }
            (AuthorizedKeyPrincipalClass::LocalClient, None)
        }
        "remote_node" => {
            let site_id = grant.origin_site_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "remote_node authorized-key grant has no authenticated origin_site_id"
                )
            })?;
            validate_canonical_site_id(site_id)?;
            (
                AuthorizedKeyPrincipalClass::RemoteNode,
                Some(site_id.to_owned()),
            )
        }
        "remote_principal" => {
            let site_id = grant.origin_site_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "remote_principal authorized-key grant has no authenticated origin_site_id"
                )
            })?;
            validate_canonical_site_id(site_id)?;
            (
                AuthorizedKeyPrincipalClass::RemotePrincipal,
                Some(site_id.to_owned()),
            )
        }
        "remote_operator" => {
            return Err(AuthorizedKeyError::StaleClass {
                fingerprint: fingerprint.to_owned(),
                class: "remote_operator".to_owned(),
            }
            .into());
        }
        other => bail!("unknown authorized-key principal_class '{other}'"),
    };
    if grant.fingerprint != fingerprint {
        bail!("fingerprint mismatch");
    }
    let encoded = grant
        .public_key
        .strip_prefix("ed25519:")
        .ok_or_else(|| anyhow::anyhow!("invalid public key format"))?;
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    let public_key = VerifyingKey::from_bytes(&key_array)?;
    let computed = lillux::signature::compute_fingerprint(&public_key);
    if computed != fingerprint {
        bail!(
            "authorized-key grant public key computes to fingerprint {computed}, not {fingerprint}"
        );
    }
    for scope in &grant.scopes {
        if let Err(reason) = ryeos_runtime::authorizer::validate_scope_pattern(scope) {
            bail!("authorized-key grant contains an invalid scope: {reason}");
        }
    }
    if grant.granted_by.trim().is_empty() || grant.created_at.trim().is_empty() {
        bail!("authorized-key grant audit fields must not be empty");
    }
    grant.authorized_by.validate()?;
    let expires_at = GrantExpiry::from_grant_text(&grant.expires_at)?;
    Ok(Some(VerifiedAuthorizedKeyGrant {
        public_key,
        scopes: grant.scopes,
        owner: grant.label,
        principal_class,
        configured_origin_site_id,
        source_file_hash: lillux::sha256_hex(&bytes),
        fingerprint: grant.fingerprint,
        granted_by: grant.granted_by,
        created_at: grant.created_at,
        expires_at,
        authorized_by: grant.authorized_by,
    }))
}

/// Write a node-signed authorized-key TOML entry.
///
/// Used by bootstrap (local operator) and the authorize-key handler
/// (remote delegation). There is exactly one TOML emitter.
///
/// The TOML is signed with the node's signing key. Only the node can
/// create valid authorized keys — remote callers can only request that
/// the node create one.
///
/// ## Wildcard policy
///
/// `wildcard` controls whether wildcard scopes are accepted in `scopes`. See
/// [`WildcardPolicy`] for when each variant is appropriate.
// One argument per authorized_key TOML field; a dozen call sites
// (tests included) enumerate them positionally.
pub fn write_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_signing_key: &lillux::crypto::SigningKey,
    wildcard: WildcardPolicy,
) -> Result<std::path::PathBuf> {
    write_authorized_key_toml_for_subject(
        auth_dir,
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_signing_key,
        wildcard,
        AuthorizedKeySubject::LocalClient,
    )
}

/// Create a delegated local-client grant without replacing any incumbent.
///
/// Online delegation must not be able to alter the semantic class, site
/// constraint, scopes, or label of an already-authorized fingerprint. Class
/// conversion remains an explicit stopped-daemon operation through
/// `reconcile_authorized_key_toml_scopes`.
pub fn create_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_signing_key: &lillux::crypto::SigningKey,
) -> Result<std::path::PathBuf> {
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_signing_key,
        WildcardPolicy::Reject,
        AuthorizedKeySubject::LocalClient,
    )?;
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    publish_authorized_key_bytes(&directory, fingerprint, signed.as_bytes(), Some(None))?;
    Ok(auth_dir.join(format!("{fingerprint}.toml")))
}

/// Write a node-signed grant for a remote RyeOS node. Unlike a normal client
/// grant, this binds the admitted signing key to the node's authenticated site
/// identity. The binding is consumed by remote execution admission; it never
/// comes from an execute request body or header.
pub fn write_authorized_remote_node_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    origin_site_id: &str,
    node_signing_key: &lillux::crypto::SigningKey,
) -> Result<std::path::PathBuf> {
    create_authorized_key_toml_for_subject(
        auth_dir,
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_signing_key,
        AuthorizedKeySubject::RemoteNode { origin_site_id },
    )
}

/// Create a node-signed grant of any class without replacing an incumbent.
/// A remote subject's origin site must be canonical. Admission redeems a
/// token through this writer, so the class and origin come from the token.
pub fn create_authorized_key_toml_for_subject(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_signing_key: &lillux::crypto::SigningKey,
    subject: AuthorizedKeySubject<'_>,
) -> Result<std::path::PathBuf> {
    if let AuthorizedKeySubject::RemoteNode { origin_site_id }
    | AuthorizedKeySubject::RemotePrincipal { origin_site_id } = subject
    {
        validate_canonical_site_id(origin_site_id)
            .context("remote grant origin_site_id is not canonical")?;
    }
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_signing_key,
        WildcardPolicy::Reject,
        subject,
    )?;
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    publish_authorized_key_bytes(&directory, fingerprint, signed.as_bytes(), Some(None))?;
    Ok(auth_dir.join(format!("{fingerprint}.toml")))
}

fn write_authorized_key_toml_for_subject(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_signing_key: &lillux::crypto::SigningKey,
    wildcard: WildcardPolicy,
    subject: AuthorizedKeySubject<'_>,
) -> Result<std::path::PathBuf> {
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_signing_key,
        wildcard,
        subject,
    )?;
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    publish_authorized_key_bytes(&directory, fingerprint, signed.as_bytes(), None)?;
    Ok(auth_dir.join(format!("{fingerprint}.toml")))
}

fn render_authorized_key_toml_for_subject(
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_signing_key: &lillux::crypto::SigningKey,
    wildcard: WildcardPolicy,
    subject: AuthorizedKeySubject<'_>,
) -> Result<String> {
    // Reject wildcard delegation unless the policy permits it.
    if wildcard == WildcardPolicy::Reject && scopes.iter().any(|s| s.contains('*')) {
        bail!(
            "wildcard scopes rejected. \
             Wildcard delegation is only permitted during operator bootstrap. \
             Specify explicit scopes instead."
        );
    }
    for scope in scopes {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(|error| anyhow::anyhow!("authorized-key scope is not canonical: {error}"))?;
    }
    reject_offline_scope_wildcards(scopes)?;
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("authorized-key fingerprint must be a lowercase SHA-256 digest");
    }
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64)
        .context("decode authorized-key public key")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("authorized-key public key must be 32 bytes"))?;
    let public_key =
        VerifyingKey::from_bytes(&key_bytes).context("decode authorized-key Ed25519 public key")?;
    let computed_fingerprint = lillux::signature::compute_fingerprint(&public_key);
    if computed_fingerprint != fingerprint {
        bail!(
            "authorized-key fingerprint does not match its public key: declared {fingerprint}, computed {computed_fingerprint}"
        );
    }
    if granted_by.trim().is_empty() || created_at.trim().is_empty() {
        bail!("authorized-key audit fields must not be empty");
    }
    terms.authorized_by.validate()?;

    let (principal_class, origin_site_id) = match subject {
        AuthorizedKeySubject::LocalClient => ("local_client", None),
        AuthorizedKeySubject::RemoteNode { origin_site_id } => {
            ("remote_node", Some(origin_site_id))
        }
        AuthorizedKeySubject::RemotePrincipal { origin_site_id } => {
            ("remote_principal", Some(origin_site_id))
        }
    };
    if terms.expires_at == GrantExpiry::Never
        && !(matches!(subject, AuthorizedKeySubject::LocalClient)
            && wildcard == WildcardPolicy::AllowBootstrap)
    {
        bail!("only the configured operator's grant may be written without an expiry");
    }
    let body = toml::to_string(&AuthorizedKeyGrantBody {
        schema_version: AUTHORIZED_KEY_SCHEMA_VERSION,
        principal_class,
        origin_site_id,
        fingerprint,
        public_key: format!("ed25519:{public_key_b64}"),
        scopes,
        label,
        granted_by,
        created_at,
        expires_at: terms.expires_at.to_grant_text(),
        authorized_by: &terms.authorized_by,
    })
    .context("serialize authorized-key grant")?;

    Ok(lillux::signature::sign_content(
        &body,
        node_signing_key,
        "#",
        None,
    ))
}

fn publish_authorized_key_bytes(
    directory: &lillux::PinnedDirectory,
    fingerprint: &str,
    signed: &[u8],
    expected_file_hash: Option<Option<&str>>,
) -> Result<()> {
    let name = format!("{fingerprint}.toml");
    let expected = directory.open_pinned_regular(std::ffi::OsStr::new(&name), false)?;
    let expected_state = expected
        .as_ref()
        .map(|file| {
            let observation = file.observation()?;
            let bytes = file.read_stable_bounded(&observation, 1024 * 1024)?;
            Ok::<_, anyhow::Error>((observation, bytes))
        })
        .transpose()?;
    match (expected_file_hash, expected_state.as_ref()) {
        (Some(None), Some(_)) => return Err(AuthorizedKeyCreateError::AlreadyExists.into()),
        (Some(Some(_)), None) => bail!("authorized-key grant disappeared during reconciliation"),
        (Some(Some(expected_hash)), Some((_, bytes)))
            if lillux::sha256_hex(bytes) != expected_hash =>
        {
            bail!("authorized-key grant changed during reconciliation")
        }
        _ => {}
    }
    let validation = expected_state.as_ref().map(|(observation, bytes)| {
        let observation = observation.clone();
        let bytes = bytes.clone();
        move |current: &lillux::PinnedRegularFile| {
            let current_observation = current.observation()?;
            if !current_observation.matches_quarantined_incumbent(&observation) {
                bail!("authorized-key grant metadata changed before publication");
            }
            if current.read_stable_bounded(&current_observation, 1024 * 1024)? != bytes {
                bail!("authorized-key grant bytes changed before publication");
            }
            Ok(())
        }
    });
    directory.ensure_path_binding()?;
    let result = match validation {
        Some(validate) => directory.replace_pinned_bytes_if_matches_atomic(
            std::ffi::OsStr::new(&name),
            expected.as_ref(),
            validate,
            signed,
            0o600,
        ),
        None => directory.replace_pinned_bytes_if_matches_atomic(
            std::ffi::OsStr::new(&name),
            None,
            |_| Ok(()),
            signed,
            0o600,
        ),
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.namespace_committed() => {
            tracing::warn!(%error, "authorized-key grant committed; retrying its durability barrier");
            directory.sync().map_err(|sync_error| {
                anyhow::anyhow!(
                    "authorized-key grant was committed, but durability remains uncertain after retrying the directory barrier; do not repeat the mutation blindly: {error}; {sync_error:#}"
                )
            })
        }
        Err(error) => Err(anyhow::anyhow!(error)),
    }
}

/// Reconcile and publish one client or node grant under a single pinned
/// directory lock. The requested subject is explicit so scope maintenance can
/// never accidentally convert a remote node into an operator (or vice versa).
/// Remote grants never accept wildcard scopes. The verified incumbent is the exact
/// compare-and-swap input, so concurrent merge operations cannot silently
/// lose scopes.
pub fn reconcile_authorized_key_toml_scopes_for_subject(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    requested_scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_identity: &NodeIdentity,
    wildcard: WildcardPolicy,
    merge: bool,
    subject: AuthorizedKeySubject<'_>,
    allow_semantic_conversion: bool,
) -> Result<(std::path::PathBuf, Vec<String>, AuthorizedKeyTransition)> {
    let requested_origin_site_id = match subject {
        AuthorizedKeySubject::LocalClient => None,
        AuthorizedKeySubject::RemoteNode { origin_site_id } => Some(origin_site_id),
        AuthorizedKeySubject::RemotePrincipal { origin_site_id } => Some(origin_site_id),
    };
    if let Some(origin_site_id) = requested_origin_site_id {
        validate_canonical_site_id(origin_site_id)
            .context("remote authorized-key origin_site_id is not canonical")?;
        if wildcard != WildcardPolicy::Reject {
            bail!("remote grants require exact, non-wildcard scopes");
        }
    }
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    let existing =
        load_verified_authorized_key_from_directory(fingerprint, &directory, node_identity)?;
    let requested_class = match subject {
        AuthorizedKeySubject::LocalClient => AuthorizedKeyPrincipalClass::LocalClient,
        AuthorizedKeySubject::RemoteNode { .. } => AuthorizedKeyPrincipalClass::RemoteNode,
        AuthorizedKeySubject::RemotePrincipal { .. } => {
            AuthorizedKeyPrincipalClass::RemotePrincipal
        }
    };
    if merge
        && let Some(existing) = existing.as_ref()
        && (existing.principal_class != requested_class
            || existing.configured_origin_site_id.as_deref() != requested_origin_site_id)
    {
        bail!(
            "cannot merge scopes while changing authorized-key principal class or origin site; rerun without --merge-scopes as an explicit stopped-daemon conversion"
        );
    }
    if let Some(existing) = existing.as_ref()
        && (existing.principal_class != requested_class
            || existing.configured_origin_site_id.as_deref() != requested_origin_site_id)
        && !allow_semantic_conversion
    {
        bail!(
            "authorized-key principal class or origin change requires explicit offline semantic-conversion authorization"
        );
    }
    let existing_scopes = existing
        .as_ref()
        .map(|grant| grant.scopes.as_slice())
        .unwrap_or(&[]);
    let mut final_scopes = if merge {
        existing_scopes.to_vec()
    } else {
        requested_scopes.to_vec()
    };
    if merge {
        for scope in requested_scopes {
            if !final_scopes.contains(scope) {
                final_scopes.push(scope.clone());
            }
        }
    }
    let dropped = existing_scopes
        .iter()
        .filter(|scope| !final_scopes.contains(scope))
        .cloned()
        .collect();
    let transition = AuthorizedKeyTransition {
        previous_principal_class: existing.as_ref().map(|grant| grant.principal_class),
        previous_origin_site_id: existing
            .as_ref()
            .and_then(|grant| grant.configured_origin_site_id.clone()),
        principal_class: requested_class,
        origin_site_id: requested_origin_site_id.map(str::to_owned),
    };
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        &final_scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_identity.signing_key(),
        wildcard,
        subject,
    )?;
    let expected_hash = existing
        .as_ref()
        .map(|grant| grant.source_file_hash.as_str());
    directory.ensure_path_binding()?;
    publish_authorized_key_bytes(
        &directory,
        fingerprint,
        signed.as_bytes(),
        Some(expected_hash),
    )?;
    Ok((
        auth_dir.join(format!("{fingerprint}.toml")),
        dropped,
        transition,
    ))
}

/// Convenience for callers whose contract selects only a local client or a
/// remote operator. Callers that can maintain a `remote_node` subject use
/// [`reconcile_authorized_key_toml_scopes_for_subject`] explicitly.
pub fn reconcile_authorized_key_toml_scopes(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    requested_scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    terms: &GrantTerms,
    node_identity: &NodeIdentity,
    wildcard: WildcardPolicy,
    merge: bool,
    remote_principal_origin_site_id: Option<&str>,
    allow_semantic_conversion: bool,
) -> Result<(std::path::PathBuf, Vec<String>, AuthorizedKeyTransition)> {
    let subject = match remote_principal_origin_site_id {
        Some(origin_site_id) => AuthorizedKeySubject::RemotePrincipal { origin_site_id },
        None => AuthorizedKeySubject::LocalClient,
    };
    reconcile_authorized_key_toml_scopes_for_subject(
        auth_dir,
        fingerprint,
        public_key_b64,
        requested_scopes,
        label,
        granted_by,
        created_at,
        terms,
        node_identity,
        wildcard,
        merge,
        subject,
        allow_semantic_conversion,
    )
}

/// A grant that was re-signed with a new expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewedAuthorizedKey {
    pub previous_expires_at: GrantExpiry,
    pub expires_at: GrantExpiry,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub scopes: Vec<String>,
    pub label: String,
}

/// Re-sign one existing grant with new `terms` (a new expiry and the
/// renewer's provenance), keeping its key, class, origin, scopes, label,
/// granting authority and creation time. Only the principal that granted it
/// (`renewer`, as `fp:<hex>`) or the node operator (`renewer_is_operator`) may
/// renew: the grantee cannot extend its own grant. The configured operator's
/// grant never lapses and is not renewed. A lapsed grant can be renewed.
pub fn renew_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    renewer: &str,
    renewer_is_operator: bool,
    terms: &GrantTerms,
    node_identity: &NodeIdentity,
) -> Result<RenewedAuthorizedKey> {
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    let existing =
        load_verified_authorized_key_from_directory(fingerprint, &directory, node_identity)?
            .ok_or_else(|| AuthorizedKeyRenewError::NotFound {
                fingerprint: fingerprint.to_owned(),
            })?;
    if existing.expires_at == GrantExpiry::Never {
        return Err(AuthorizedKeyRenewError::NeverExpires.into());
    }
    if !renewer_is_operator && !existing.was_granted_by(renewer) {
        return Err(AuthorizedKeyRenewError::NotGranterOrOperator {
            fingerprint: fingerprint.to_owned(),
        }
        .into());
    }
    let subject = match (
        existing.principal_class,
        existing.configured_origin_site_id.as_deref(),
    ) {
        (AuthorizedKeyPrincipalClass::LocalClient, _) => AuthorizedKeySubject::LocalClient,
        (AuthorizedKeyPrincipalClass::RemoteNode, Some(origin_site_id)) => {
            AuthorizedKeySubject::RemoteNode { origin_site_id }
        }
        (AuthorizedKeyPrincipalClass::RemotePrincipal, Some(origin_site_id)) => {
            AuthorizedKeySubject::RemotePrincipal { origin_site_id }
        }
        _ => bail!("a remote authorized-key grant lost its origin site"),
    };
    let public_key_b64 =
        base64::engine::general_purpose::STANDARD.encode(existing.public_key.as_bytes());
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        &public_key_b64,
        &existing.scopes,
        &existing.owner,
        &existing.granted_by,
        &existing.created_at,
        terms,
        node_identity.signing_key(),
        WildcardPolicy::Reject,
        subject,
    )?;
    directory.ensure_path_binding()?;
    publish_authorized_key_bytes(
        &directory,
        fingerprint,
        signed.as_bytes(),
        Some(Some(existing.source_file_hash.as_str())),
    )?;
    Ok(RenewedAuthorizedKey {
        previous_expires_at: existing.expires_at,
        expires_at: terms.expires_at,
        principal_class: existing.principal_class,
        scopes: existing.scopes,
        label: existing.owner,
    })
}

/// What reading one grant file found.
#[derive(Debug, Clone)]
pub enum GrantFileState {
    /// Signed by this node and current; it may still have lapsed.
    Current(Box<VerifiedAuthorizedKeyGrant>),
    /// Signed by this node but of a retired schema or class.
    Stale(AuthorizedKeyError),
    /// Unreadable, unsigned, tampered or malformed.
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct GrantFileInspection {
    /// The file name without its `.toml` suffix (a fingerprint when sound).
    pub name: String,
    pub state: GrantFileState,
}

/// Read every grant file in `auth_dir` and report what each is, without
/// judging expiry and without failing on the first bad file. The doctor uses
/// it to list lapsed, soon-lapsing, stale and invalid grants.
pub fn inspect_authorized_keys(
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<Vec<GrantFileInspection>> {
    let Some(directory) = lillux::PinnedDirectory::open(auth_dir)? else {
        return Ok(Vec::new());
    };
    let mut names = directory
        .entries_no_follow()?
        .into_iter()
        .map(|entry| entry.name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    let mut inspections = Vec::with_capacity(names.len());
    for file_name in names {
        let Some(name) = file_name.strip_suffix(".toml") else {
            inspections.push(GrantFileInspection {
                name: file_name.clone(),
                state: GrantFileState::Invalid(format!(
                    "`{file_name}` is not a `<fingerprint>.toml` grant file"
                )),
            });
            continue;
        };
        let state =
            match load_verified_authorized_key_from_directory(name, &directory, node_identity) {
                Ok(Some(grant)) => GrantFileState::Current(Box::new(grant)),
                Ok(None) => GrantFileState::Invalid("the grant file vanished while listing".into()),
                Err(error) => match error.downcast::<AuthorizedKeyError>() {
                    Ok(stale) => GrantFileState::Stale(stale),
                    Err(error) => GrantFileState::Invalid(format!("{error:#}")),
                },
            };
        inspections.push(GrantFileInspection {
            name: name.to_owned(),
            state,
        });
    }
    Ok(inspections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_scope_wildcards_are_recognised_and_refused() {
        for scope in [
            "ryeos.offline.*",
            "ryeos.offline.node.*",
            "ryeos.offline.node.reset.*",
        ] {
            assert!(is_offline_scope_wildcard(scope), "{scope}");
            assert!(reject_offline_scope_wildcards(&[scope.to_owned()]).is_err());
        }
        for scope in [
            "ryeos.offline.node.allow-restart",
            "ryeos.execute.service.remote/*",
            "ryeos.*",
            "*",
        ] {
            assert!(!is_offline_scope_wildcard(scope), "{scope}");
            reject_offline_scope_wildcards(&[scope.to_owned()]).unwrap();
        }
    }

    #[test]
    fn a_grant_reaches_an_offline_scope_by_wildcard_only_when_none_names_it() {
        let scope = "ryeos.offline.node.allow-restart";
        let exact = vec![scope.to_owned()];
        let wildcard = vec!["*".to_owned()];
        let both = vec!["*".to_owned(), scope.to_owned()];
        assert!(!matches_only_by_wildcard(&exact, scope));
        assert!(matches_only_by_wildcard(&wildcard, scope));
        assert!(!matches_only_by_wildcard(&both, scope));
        assert!(!matches_only_by_wildcard(
            &["ryeos.execute.service.remote/run".to_owned()],
            scope
        ));
    }

    #[test]
    fn the_grant_writer_refuses_an_offline_wildcard_whatever_the_wildcard_policy() {
        let key = SigningKey::from_bytes(&[41; 32]);
        let node = NodeIdentity::from_signing_key(key).unwrap();
        let client = SigningKey::from_bytes(&[42; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&client.verifying_key());
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(client.verifying_key().as_bytes());
        let directory = tempfile::tempdir().unwrap();
        for wildcard in [WildcardPolicy::Reject, WildcardPolicy::AllowBootstrap] {
            let error = write_authorized_key_toml(
                directory.path(),
                &fingerprint,
                &encoded,
                &["ryeos.offline.*".to_owned()],
                "client",
                "test",
                "2026-01-01T00:00:00Z",
                &GrantTerms::for_tests(),
                node.signing_key(),
                wildcard,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("wildcard"), "{error:#}");
        }
        write_authorized_key_toml(
            directory.path(),
            &fingerprint,
            &encoded,
            &["ryeos.offline.node.allow-restart".to_owned()],
            "client",
            "test",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            node.signing_key(),
            WildcardPolicy::Reject,
        )
        .unwrap();
    }

    #[test]
    fn public_identity_checks_key_and_existing_self_signature() {
        let key = SigningKey::from_bytes(&[27; 32]);
        let node = NodeIdentity::from_signing_key(key).unwrap();
        let mut document = node
            .build_public_identity_at("2026-09-10T00:00:00Z")
            .unwrap();
        assert_eq!(document.verified_fingerprint().unwrap(), node.fingerprint());
        document.principal_id = format!("fp:{}", "b".repeat(64));
        assert!(document.verified_fingerprint().is_err());
        document.principal_id = node.principal_id();
        document.created_at = "2026-09-11T00:00:00Z".to_owned();
        assert!(document.verified_fingerprint().is_err());
    }
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn cloned_identity_retains_same_secret_allocation_and_redacted_debug() {
        let original = NodeIdentity::from_signing_key(test_signing_key()).unwrap();
        let clone = original.clone();
        assert!(std::ptr::eq(original.signing_key(), clone.signing_key()));
        assert_eq!(original.fingerprint(), clone.fingerprint());
        assert!(format!("{clone:?}").contains("<redacted>"));
        drop(original);
        let signature: Signature = clone.signing_key().sign(b"same original identity");
        clone
            .verifying_key()
            .verify_strict(b"same original identity", &signature)
            .unwrap();
    }

    fn test_signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn wildcard_rejected_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let result = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &["*".to_string()],
            "test",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::Reject,
        );
        let err = result.expect_err("wildcard should be rejected");
        assert!(err.to_string().contains("wildcard scope"));
    }

    #[test]
    fn prefix_wildcard_rejected_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let result = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &["ryeos.execute.service.*".to_string()],
            "test",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::Reject,
        );
        let err = result.expect_err("prefix wildcard should be rejected");
        assert!(err.to_string().contains("wildcard scope"));
    }

    #[test]
    fn wildcard_allowed_with_bootstrap_policy() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let result = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &["*".to_string()],
            "test",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::AllowBootstrap,
        );
        assert!(
            result.is_ok(),
            "wildcard should be allowed under AllowBootstrap"
        );
    }

    #[test]
    fn canonical_scopes_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let result = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &[
                "ryeos.execute.service.remote/admin".to_string(),
                "ryeos.execute.service.bundle/install".to_string(),
            ],
            "test",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::Reject,
        );
        assert!(result.is_ok(), "canonical scopes should be accepted");
    }

    #[test]
    fn round_trip_toml_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let path = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &["ryeos.execute.service.remote/admin".to_string()],
            "test-label",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::Reject,
        )
        .unwrap();

        // Read back the file and verify the content
        let content = std::fs::read_to_string(&path).unwrap();
        // Skip signature line, find the body
        let body: String = content
            .lines()
            .filter(|l| !l.starts_with("# ryeos:signed:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("schema_version = 3"));
        assert!(body.contains("principal_class = \"local_client\""));
        assert!(!body.contains("origin_site_id"));
        assert!(body.contains(&format!("fingerprint = \"{fp}\"")));
        assert!(body.contains("ryeos.execute.service.remote/admin"));
        assert!(body.contains("test-label"));
        assert!(body.contains("test-granter"));
    }

    #[test]
    fn canonical_site_ids_have_one_exact_grammar() {
        for valid in ["site:node-1", "site:region.nz_2", "site:A"] {
            validate_canonical_site_id(valid).unwrap();
        }
        for invalid in [
            "node-1",
            "site:",
            "site:two words",
            "site:slash/value",
            "site:unicode-λ",
            " site:node",
        ] {
            assert!(
                validate_canonical_site_id(invalid).is_err(),
                "accepted invalid site id {invalid:?}"
            );
        }
    }

    #[test]
    fn forwarding_origin_assertion_can_only_narrow_authenticated_authority() {
        validate_forwarding_origin_assertion(
            None,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        )
        .unwrap();
        validate_forwarding_origin_assertion(
            None,
            Some(AuthorizedKeyPrincipalClass::RemoteNode),
            Some("site:source"),
        )
        .unwrap();
        validate_forwarding_origin_assertion(
            Some("site:source"),
            Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
            Some("site:source"),
        )
        .unwrap();
        assert!(
            validate_forwarding_origin_assertion(
                None,
                Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
                Some("site:source")
            )
            .is_err()
        );
        assert!(
            validate_forwarding_origin_assertion(
                Some("site:source"),
                Some(AuthorizedKeyPrincipalClass::LocalClient),
                None
            )
            .is_err()
        );
        assert!(
            validate_forwarding_origin_assertion(
                Some("site:source"),
                Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
                Some("site:different")
            )
            .is_err()
        );
    }

    #[test]
    fn remote_principal_reconciliation_preserves_owner_and_origin() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeIdentity::from_signing_key(test_signing_key()).unwrap();
        let client = test_signing_key().verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&client);
        let public_key = base64::engine::general_purpose::STANDARD.encode(client.as_bytes());

        let (path, dropped, transition) = reconcile_authorized_key_toml_scopes(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.execute.service.remote/run".to_owned()],
            "configured operator",
            "test",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &node,
            WildcardPolicy::Reject,
            false,
            Some("site:source"),
            false,
        )
        .unwrap();

        assert!(dropped.is_empty());
        assert_eq!(transition.previous_principal_class, None);
        assert_eq!(
            transition.principal_class,
            AuthorizedKeyPrincipalClass::RemotePrincipal
        );
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("principal_class = \"remote_principal\""));
        assert!(content.contains("origin_site_id = \"site:source\""));
        let grant = load_verified_authorized_key(&fingerprint, dir.path(), &node)
            .unwrap()
            .unwrap();
        assert_eq!(
            lillux::signature::compute_fingerprint(&grant.public_key),
            fingerprint
        );
        assert_eq!(
            grant.principal_class,
            AuthorizedKeyPrincipalClass::RemotePrincipal
        );
        assert_eq!(
            grant.configured_origin_site_id.as_deref(),
            Some("site:source")
        );
    }

    #[test]
    fn remote_principal_reconciliation_rejects_invalid_origin_and_wildcard_mode() {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeIdentity::from_signing_key(test_signing_key()).unwrap();
        let client = test_signing_key().verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&client);
        let public_key = base64::engine::general_purpose::STANDARD.encode(client.as_bytes());
        let invoke = |origin, wildcard| {
            reconcile_authorized_key_toml_scopes(
                dir.path(),
                &fingerprint,
                &public_key,
                &["ryeos.execute.service.remote/run".to_owned()],
                "configured operator",
                "test",
                "2026-01-01T00:00:00Z",
                &GrantTerms::for_tests(),
                &node,
                wildcard,
                false,
                Some(origin),
                false,
            )
        };
        assert!(invoke("source", WildcardPolicy::Reject).is_err());
        assert!(invoke("site:source", WildcardPolicy::AllowBootstrap).is_err());
    }

    #[test]
    fn authorized_key_writer_uses_toml_escaping_for_audit_fields() {
        let dir = tempfile::tempdir().unwrap();
        let key = test_signing_key();
        let vk = key.verifying_key();
        let fp = lillux::sha256_hex(vk.as_bytes());
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let path = write_authorized_key_toml(
            dir.path(),
            &fp,
            &key_b64,
            &["ryeos.execute.service.remote/admin".to_string()],
            "operator \"one\"",
            "bootstrap\\owner",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &key,
            WildcardPolicy::Reject,
        )
        .unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        let (_, body) = content.split_once('\n').unwrap();
        let parsed: toml::Value = toml::from_str(body).unwrap();
        assert_eq!(parsed["label"].as_str(), Some("operator \"one\""));
        assert_eq!(parsed["granted_by"].as_str(), Some("bootstrap\\owner"));
    }

    #[test]
    fn concurrent_authorized_key_merges_preserve_both_scope_sets() {
        let dir = tempfile::tempdir().unwrap();
        let node = std::sync::Arc::new(NodeIdentity::from_signing_key(test_signing_key()).unwrap());
        let client = test_signing_key().verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&client);
        let public_key = base64::engine::general_purpose::STANDARD.encode(client.as_bytes());
        let mut joins = Vec::new();
        for scope in ["ryeos.execute.service.alpha", "ryeos.execute.service.beta"] {
            let directory = dir.path().to_path_buf();
            let node = std::sync::Arc::clone(&node);
            let fingerprint = fingerprint.clone();
            let public_key = public_key.clone();
            let scope = scope.to_owned();
            joins.push(std::thread::spawn(move || {
                reconcile_authorized_key_toml_scopes(
                    &directory,
                    &fingerprint,
                    &public_key,
                    &[scope],
                    "client",
                    "test",
                    "2026-01-01T00:00:00Z",
                    &GrantTerms::for_tests(),
                    &node,
                    WildcardPolicy::Reject,
                    true,
                    None,
                    false,
                )
                .unwrap();
            }));
        }
        for join in joins {
            join.join().unwrap();
        }
        let grant = load_verified_authorized_key(&fingerprint, dir.path(), &node)
            .unwrap()
            .unwrap();
        assert!(
            grant
                .scopes
                .contains(&"ryeos.execute.service.alpha".to_owned())
        );
        assert!(
            grant
                .scopes
                .contains(&"ryeos.execute.service.beta".to_owned())
        );
    }

    #[test]
    fn online_create_never_replaces_an_existing_remote_grant() {
        let dir = tempfile::tempdir().unwrap();
        let node_key = lillux::crypto::SigningKey::from_bytes(&[71_u8; 32]);
        let client_key = lillux::crypto::SigningKey::from_bytes(&[72_u8; 32]);
        let client = client_key.verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&client);
        let public_key = base64::engine::general_purpose::STANDARD.encode(client.as_bytes());
        let path = write_authorized_remote_node_key_toml(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.execute.service.remote/run".to_owned()],
            "remote node",
            "test",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            "site:source",
            &node_key,
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();

        let admission_error = write_authorized_remote_node_key_toml(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.attest.request.forwarded-operator".to_owned()],
            "changed remote node",
            "new admission",
            "2026-01-02T00:00:00Z",
            &GrantTerms::for_tests(),
            "site:different",
            &node_key,
        )
        .expect_err("online admission must not replace an existing grant");
        assert!(matches!(
            admission_error.downcast_ref::<AuthorizedKeyCreateError>(),
            Some(AuthorizedKeyCreateError::AlreadyExists)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let error = create_authorized_key_toml(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.execute.service.identity/authorize-key".to_owned()],
            "reclassified",
            "remote caller",
            "2026-01-02T00:00:00Z",
            &GrantTerms::for_tests(),
            &node_key,
        )
        .expect_err("online creation must not replace a remote grant");
        assert!(matches!(
            error.downcast_ref::<AuthorizedKeyCreateError>(),
            Some(AuthorizedKeyCreateError::AlreadyExists)
        ));
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn scope_merge_cannot_change_principal_class_or_origin() {
        let dir = tempfile::tempdir().unwrap();
        let node_key = lillux::crypto::SigningKey::from_bytes(&[73_u8; 32]);
        let node = NodeIdentity::from_signing_key(node_key).unwrap();
        let client_key = lillux::crypto::SigningKey::from_bytes(&[74_u8; 32]);
        let client = client_key.verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&client);
        let public_key = base64::engine::general_purpose::STANDARD.encode(client.as_bytes());
        reconcile_authorized_key_toml_scopes(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.execute.service.remote/run".to_owned()],
            "operator",
            "offline",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &node,
            WildcardPolicy::Reject,
            false,
            Some("site:source"),
            false,
        )
        .unwrap();

        let error = reconcile_authorized_key_toml_scopes(
            dir.path(),
            &fingerprint,
            &public_key,
            &["ryeos.execute.service.vault/list".to_owned()],
            "operator",
            "offline",
            "2026-01-02T00:00:00Z",
            &GrantTerms::for_tests(),
            &node,
            WildcardPolicy::Reject,
            true,
            None,
            true,
        )
        .expect_err("merge must not conceal a class conversion");
        assert!(error.to_string().contains("cannot merge scopes"));
        let grant = load_verified_authorized_key(&fingerprint, dir.path(), &node)
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.principal_class,
            AuthorizedKeyPrincipalClass::RemotePrincipal
        );
        assert_eq!(
            grant.configured_origin_site_id.as_deref(),
            Some("site:source")
        );
    }

    // ── Grant lifetime, provenance and stale-schema refusals ────────────────

    struct GrantFixture {
        _dir: tempfile::TempDir,
        auth_dir: std::path::PathBuf,
        node: NodeIdentity,
        fingerprint: String,
        public_key: String,
    }

    fn grant_fixture() -> GrantFixture {
        let dir = tempfile::tempdir().unwrap();
        let node = NodeIdentity::from_signing_key(SigningKey::from_bytes(&[61; 32])).unwrap();
        let grantee = SigningKey::from_bytes(&[62; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&grantee.verifying_key());
        let public_key =
            base64::engine::general_purpose::STANDARD.encode(grantee.verifying_key().as_bytes());
        GrantFixture {
            auth_dir: dir.path().join("authorized_keys"),
            _dir: dir,
            node,
            fingerprint,
            public_key,
        }
    }

    fn granter_fingerprint() -> String {
        format!("fp:{}", "a".repeat(64))
    }

    fn terms_expiring(secs: u64) -> GrantTerms {
        GrantTerms {
            expires_at: GrantExpiry::At(secs),
            authorized_by: AuthorizedBy::principal_request(
                &granter_fingerprint(),
                Some("c".repeat(64)),
            ),
        }
    }

    impl GrantFixture {
        fn issue(&self, terms: &GrantTerms) {
            create_authorized_key_toml(
                &self.auth_dir,
                &self.fingerprint,
                &self.public_key,
                &["ryeos.execute.service.vault/list".to_owned()],
                "grantee",
                &granter_fingerprint(),
                "2026-01-01T00:00:00Z",
                terms,
                self.node.signing_key(),
            )
            .unwrap();
        }

        fn write_raw(&self, body: &str) {
            std::fs::create_dir_all(&self.auth_dir).unwrap();
            let signed = lillux::signature::sign_content(body, self.node.signing_key(), "#", None);
            std::fs::write(
                self.auth_dir.join(format!("{}.toml", self.fingerprint)),
                signed,
            )
            .unwrap();
        }

        fn operator_terms(&self) -> GrantTerms {
            GrantTerms::operator(AuthorizedBy::principal_request(&self.fingerprint, None))
        }

        fn write_operator_grant(&self) {
            write_authorized_key_toml(
                &self.auth_dir,
                &self.fingerprint,
                &self.public_key,
                &["*".to_owned()],
                "operator",
                &self.fingerprint,
                "2026-01-01T00:00:00Z",
                &self.operator_terms(),
                self.node.signing_key(),
                WildcardPolicy::AllowBootstrap,
            )
            .unwrap();
        }
    }

    #[test]
    fn grant_records_expiry_and_authorized_by() {
        let fx = grant_fixture();
        fx.issue(&terms_expiring(2_000_000_000));
        let grant = load_verified_authorized_key_at(&fx.fingerprint, &fx.auth_dir, &fx.node, 10)
            .unwrap()
            .unwrap();
        assert_eq!(grant.expires_at, GrantExpiry::At(2_000_000_000));
        assert_eq!(grant.authorized_by.principal, granter_fingerprint());
        assert_eq!(
            grant.authorized_by.request_digest.as_deref(),
            Some("c".repeat(64).as_str())
        );
    }

    #[test]
    fn expired_grant_is_typed_expired_and_only_the_unjudged_loader_reads_it() {
        let fx = grant_fixture();
        fx.issue(&terms_expiring(1_000));
        let error = load_verified_authorized_key_at(&fx.fingerprint, &fx.auth_dir, &fx.node, 1_000)
            .unwrap_err();
        let typed = error.downcast_ref::<AuthorizedKeyError>().unwrap();
        assert_eq!(typed.code(), "grant_expired");
        assert!(matches!(typed, AuthorizedKeyError::Expired { .. }));
        assert!(typed.to_string().contains("renew"), "{typed}");
        // One second earlier it still authenticates.
        load_verified_authorized_key_at(&fx.fingerprint, &fx.auth_dir, &fx.node, 999)
            .unwrap()
            .unwrap();
        let unjudged =
            load_verified_authorized_key_unjudged(&fx.fingerprint, &fx.auth_dir, &fx.node)
                .unwrap()
                .unwrap();
        assert!(unjudged.ensure_unexpired(1_000).is_err());
    }

    #[test]
    fn never_is_written_only_for_the_bootstrap_local_client_grant() {
        let fx = grant_fixture();
        let never = fx.operator_terms();
        let error = write_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &fx.public_key,
            &["ryeos.execute.service.vault/list".to_owned()],
            "grantee",
            "test",
            "2026-01-01T00:00:00Z",
            &never,
            fx.node.signing_key(),
            WildcardPolicy::Reject,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("without an expiry"),
            "{error:#}"
        );
        let error = write_authorized_remote_node_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &fx.public_key,
            &["ryeos.execute.service.vault/list".to_owned()],
            "node",
            "test",
            "2026-01-01T00:00:00Z",
            &never,
            "site:source",
            fx.node.signing_key(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("without an expiry"),
            "{error:#}"
        );
        fx.write_operator_grant();
        let grant = load_verified_authorized_key_unjudged(&fx.fingerprint, &fx.auth_dir, &fx.node)
            .unwrap()
            .unwrap();
        assert_eq!(grant.expires_at, GrantExpiry::Never);
        assert!(grant.ensure_unexpired(u64::MAX).is_ok());
    }

    #[test]
    fn a_schema_2_grant_is_refused_as_stale_naming_the_cure() {
        let fx = grant_fixture();
        fx.write_raw(&format!(
            "schema_version = 2\nprincipal_class = \"local_client\"\nfingerprint = \"{}\"\n\
             public_key = \"ed25519:{}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\n\
             label = \"old\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
            fx.fingerprint, fx.public_key
        ));
        let error =
            load_verified_authorized_key(&fx.fingerprint, &fx.auth_dir, &fx.node).unwrap_err();
        let typed = error.downcast_ref::<AuthorizedKeyError>().unwrap();
        assert_eq!(typed.code(), "grant_schema_stale");
        assert!(
            matches!(
                typed,
                AuthorizedKeyError::StaleSchema {
                    seen: 2,
                    admitted: 3,
                    ..
                }
            ),
            "{typed}"
        );
        assert!(typed.to_string().contains("re-issue"), "{typed}");
    }

    #[test]
    fn a_remote_operator_grant_is_refused_as_a_stale_class_naming_re_admission() {
        let fx = grant_fixture();
        fx.write_raw(&format!(
            "schema_version = 2\nprincipal_class = \"remote_operator\"\n\
             origin_site_id = \"site:source\"\nfingerprint = \"{}\"\n\
             public_key = \"ed25519:{}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\n\
             label = \"old\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
            fx.fingerprint, fx.public_key
        ));
        let error =
            load_verified_authorized_key(&fx.fingerprint, &fx.auth_dir, &fx.node).unwrap_err();
        let typed = error.downcast_ref::<AuthorizedKeyError>().unwrap();
        assert_eq!(typed.code(), "grant_class_stale");
        assert!(typed.to_string().contains("re-admit"), "{typed}");
        let inspected = inspect_authorized_keys(&fx.auth_dir, &fx.node).unwrap();
        assert!(matches!(
            inspected[0].state,
            GrantFileState::Stale(AuthorizedKeyError::StaleClass { .. })
        ));
    }

    #[test]
    fn renewal_by_the_granter_resigns_with_a_new_expiry() {
        let fx = grant_fixture();
        fx.issue(&terms_expiring(1_000));
        let renewer = AuthorizedBy::principal_request(&granter_fingerprint(), Some("d".repeat(64)));
        let renewed = renew_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &granter_fingerprint(),
            false,
            &GrantTerms {
                expires_at: GrantExpiry::At(5_000),
                authorized_by: renewer,
            },
            &fx.node,
        )
        .unwrap();
        assert_eq!(renewed.previous_expires_at, GrantExpiry::At(1_000));
        let grant = load_verified_authorized_key_at(&fx.fingerprint, &fx.auth_dir, &fx.node, 2_000)
            .unwrap()
            .unwrap();
        assert_eq!(grant.expires_at, GrantExpiry::At(5_000));
        assert_eq!(grant.granted_by, granter_fingerprint());
        assert_eq!(grant.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(grant.scopes, ["ryeos.execute.service.vault/list"]);
        assert_eq!(
            grant.authorized_by.request_digest.as_deref(),
            Some("d".repeat(64).as_str())
        );
    }

    #[test]
    fn renewal_is_refused_for_the_grantee_and_allowed_for_the_operator() {
        let fx = grant_fixture();
        fx.issue(&terms_expiring(1_000));
        let new_terms = terms_expiring(5_000);
        let grantee_principal = format!("fp:{}", fx.fingerprint);
        let error = renew_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &grantee_principal,
            false,
            &new_terms,
            &fx.node,
        )
        .unwrap_err();
        let typed = error.downcast_ref::<AuthorizedKeyRenewError>().unwrap();
        assert_eq!(typed.code(), "grant_renewal_not_permitted");
        let stranger = format!("fp:{}", "e".repeat(64));
        assert!(
            renew_authorized_key_toml(
                &fx.auth_dir,
                &fx.fingerprint,
                &stranger,
                false,
                &new_terms,
                &fx.node,
            )
            .is_err()
        );
        renew_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &stranger,
            true,
            &new_terms,
            &fx.node,
        )
        .unwrap();
    }

    #[test]
    fn the_operator_grant_is_not_renewed_and_a_missing_grant_is_not_found() {
        let fx = grant_fixture();
        let error = renew_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &granter_fingerprint(),
            true,
            &terms_expiring(5_000),
            &fx.node,
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<AuthorizedKeyRenewError>()
                .unwrap()
                .code(),
            "grant_not_found"
        );
        fx.write_operator_grant();
        let error = renew_authorized_key_toml(
            &fx.auth_dir,
            &fx.fingerprint,
            &format!("fp:{}", fx.fingerprint),
            true,
            &terms_expiring(5_000),
            &fx.node,
        )
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<AuthorizedKeyRenewError>()
                .unwrap()
                .code(),
            "grant_never_expires"
        );
    }

    #[test]
    fn grant_expiry_text_round_trips_and_refuses_other_spellings() {
        for expiry in [GrantExpiry::Never, GrantExpiry::At(1_800_000_000)] {
            assert_eq!(
                GrantExpiry::from_grant_text(&expiry.to_grant_text()).unwrap(),
                expiry
            );
        }
        for text in [
            "",
            "tomorrow",
            "2026-01-01",
            "2026-01-01T00:00:00+13:00",
            "Never",
        ] {
            assert!(GrantExpiry::from_grant_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn authorized_by_requires_a_fingerprint_principal_and_a_hex_digest() {
        AuthorizedBy::principal_request(&"a".repeat(64), None)
            .validate()
            .unwrap();
        assert!(
            AuthorizedBy {
                principal: "operator".to_owned(),
                request_digest: None
            }
            .validate()
            .is_err()
        );
        assert!(
            AuthorizedBy::principal_request(&"a".repeat(64), Some("xyz".to_owned()))
                .validate()
                .is_err()
        );
    }
}
