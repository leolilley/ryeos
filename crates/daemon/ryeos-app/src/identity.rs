use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::{DecodePrivateKey, EncodePrivateKey};
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct NodeIdentity {
    signing_key: SigningKey,
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
        // Zeroizing: the PEM buffer holds the private key; wipe it once
        // the SigningKey (which zeroizes itself on drop) is built.
        let pem = zeroize::Zeroizing::new(fs::read_to_string(key_path).with_context(|| {
            format!(
                "signing key not found at {} — run 'ryeos init' first",
                key_path.display()
            )
        })?);
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

    /// Like [`Self::write_public_identity`] but takes the timestamp explicitly,
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

const AUTHORIZED_KEY_SCHEMA_VERSION: u32 = 2;

/// Exact capability an admitted source-node key must carry before it may
/// co-sign a configured operator's forwarded request.
pub const FORWARDED_OPERATOR_ATTESTATION_SCOPE: &str = "ryeos.attest.request.forwarded-operator";

/// Validate the one wire spelling accepted for authenticated RyeOS site
/// identities. Site ids are protocol identifiers, not display names: keeping
/// the alphabet deliberately small makes the value byte-stable across TOML,
/// JSON, signatures, and remote admission claims.
pub fn validate_canonical_site_id(site_id: &str) -> Result<()> {
    let Some(name) = site_id.strip_prefix("site:") else {
        bail!("site id must begin with `site:`");
    };
    if name.is_empty() || site_id.len() > 255 {
        bail!("site id must contain a name and be at most 255 bytes");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("site id may contain only ASCII letters, digits, `.`, `_`, and `-` after `site:`");
    }
    Ok(())
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
        if principal_class == Some(AuthorizedKeyPrincipalClass::RemoteOperator) {
            bail!("remote_operator execution requires a signed forwarding-origin assertion");
        }
        return Ok(());
    };
    if principal_class != Some(AuthorizedKeyPrincipalClass::RemoteOperator) {
        bail!("forwarding-origin assertions are valid only for remote_operator principals");
    }
    validate_canonical_site_id(required_origin_site_id)?;
    if authenticated_origin_site_id != Some(required_origin_site_id) {
        bail!("authenticated origin does not match the signed forwarding assertion");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizedKeyPrincipalClass {
    LocalClient,
    RemoteNode,
    RemoteOperator,
}

/// Closed create-only publication failure used by online authorization paths.
/// Callers may expose this as a conflict without parsing an anyhow message;
/// all other publication failures remain internal errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthorizedKeyCreateError {
    #[error("authorized-key fingerprint already has a grant")]
    AlreadyExists,
}

impl AuthorizedKeyPrincipalClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalClient => "local_client",
            Self::RemoteNode => "remote_node",
            Self::RemoteOperator => "remote_operator",
        }
    }

    pub const fn is_remote(self) -> bool {
        matches!(self, Self::RemoteNode | Self::RemoteOperator)
    }
}

enum AuthorizedKeySubject<'a> {
    LocalClient,
    RemoteNode {
        origin_site_id: &'a str,
    },
    /// A configured operator key forwarded by another RyeOS node. The key
    /// remains the workflow owner, while the node-signed grant constrains the
    /// one source site whose separately admitted node key may co-sign it.
    RemoteOperator {
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
    /// `remote_operator`, this is only an allow-list constraint until a
    /// separately admitted source-node key co-signs the exact request.
    pub configured_origin_site_id: Option<String>,
    /// Whole-file digest of the exact signed grant bytes used for this view.
    pub source_file_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedKeyTransition {
    pub previous_principal_class: Option<AuthorizedKeyPrincipalClass>,
    pub previous_origin_site_id: Option<String>,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub origin_site_id: Option<String>,
}

/// Load one exact node-signed authorized-key grant. Absence is distinct from
/// malformed or unverifiable content: callers may treat absence as no prior
/// grant, but must never fold corrupt authority into an empty grant.
pub fn load_verified_authorized_key(
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
    let Some(mut file) = directory.open_regular(std::ffi::OsStr::new(&name), false)? else {
        directory.ensure_path_binding()?;
        return Ok(None);
    };
    let observation = lillux::observe_open_regular_file(&file)?;
    let bytes =
        lillux::read_open_regular_file_stable_bounded(&mut file, &observation, 1024 * 1024)?;
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

    let grant: RetainedAuthorizedKeyGrantBody = toml::from_str(&body)
        .map_err(|error| anyhow::anyhow!("invalid authorized-key grant body: {error}"))?;
    if grant.schema_version != AUTHORIZED_KEY_SCHEMA_VERSION {
        bail!(
            "authorized-key grant schema_version must be exactly {} (got {})",
            AUTHORIZED_KEY_SCHEMA_VERSION,
            grant.schema_version
        );
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
        "remote_operator" => {
            let site_id = grant.origin_site_id.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "remote_operator authorized-key grant has no authenticated origin_site_id"
                )
            })?;
            validate_canonical_site_id(site_id)?;
            (
                AuthorizedKeyPrincipalClass::RemoteOperator,
                Some(site_id.to_owned()),
            )
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
    Ok(Some(VerifiedAuthorizedKeyGrant {
        public_key,
        scopes: grant.scopes,
        owner: grant.label,
        principal_class,
        configured_origin_site_id,
        source_file_hash: lillux::sha256_hex(&bytes),
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
#[allow(clippy::too_many_arguments)]
pub fn write_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
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
#[allow(clippy::too_many_arguments)]
pub fn create_authorized_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    node_signing_key: &lillux::crypto::SigningKey,
) -> Result<std::path::PathBuf> {
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
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
#[allow(clippy::too_many_arguments)]
pub fn write_authorized_remote_node_key_toml(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    origin_site_id: &str,
    node_signing_key: &lillux::crypto::SigningKey,
) -> Result<std::path::PathBuf> {
    validate_canonical_site_id(origin_site_id)
        .context("remote-node origin_site_id is not canonical")?;
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        scopes,
        label,
        granted_by,
        created_at,
        node_signing_key,
        WildcardPolicy::Reject,
        AuthorizedKeySubject::RemoteNode { origin_site_id },
    )?;
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    publish_authorized_key_bytes(&directory, fingerprint, signed.as_bytes(), Some(None))?;
    Ok(auth_dir.join(format!("{fingerprint}.toml")))
}

#[allow(clippy::too_many_arguments)]
fn write_authorized_key_toml_for_subject(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
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
        node_signing_key,
        wildcard,
        subject,
    )?;
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    publish_authorized_key_bytes(&directory, fingerprint, signed.as_bytes(), None)?;
    Ok(auth_dir.join(format!("{fingerprint}.toml")))
}

#[allow(clippy::too_many_arguments)]
fn render_authorized_key_toml_for_subject(
    fingerprint: &str,
    public_key_b64: &str,
    scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
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

    let (principal_class, origin_site_id) = match subject {
        AuthorizedKeySubject::LocalClient => ("local_client", None),
        AuthorizedKeySubject::RemoteNode { origin_site_id } => {
            ("remote_node", Some(origin_site_id))
        }
        AuthorizedKeySubject::RemoteOperator { origin_site_id } => {
            ("remote_operator", Some(origin_site_id))
        }
    };
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
    let mut expected = directory.open_regular(std::ffi::OsStr::new(&name), false)?;
    let expected_state = expected
        .as_mut()
        .map(|file| {
            let observation = lillux::observe_open_regular_file(file)?;
            let bytes =
                lillux::read_open_regular_file_stable_bounded(file, &observation, 1024 * 1024)?;
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
        move |current: &std::fs::File| {
            let current_observation = lillux::observe_open_regular_file(current)?;
            if !current_observation.matches_quarantined_incumbent(&observation) {
                bail!("authorized-key grant metadata changed before publication");
            }
            let mut current = current.try_clone()?;
            let current = lillux::read_open_regular_file_stable_bounded(
                &mut current,
                &current_observation,
                1024 * 1024,
            )?;
            if current != bytes {
                bail!("authorized-key grant bytes changed before publication");
            }
            Ok(())
        }
    });
    directory.ensure_path_binding()?;
    let result = match validation {
        Some(validate) => directory.replace_bytes_if_matches_atomic(
            std::ffi::OsStr::new(&name),
            expected.as_ref(),
            validate,
            signed,
            0o600,
        ),
        None => directory.replace_bytes_if_matches_atomic(
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

/// Reconcile and publish one client grant under a single pinned directory
/// lock. When `remote_operator_origin_site_id` is present, the exact operator
/// key remains the principal while the node-signed grant constrains its
/// allowed forwarding site. Actual transit requires a separately admitted
/// source-node co-signature. Remote-operator grants never accept wildcard
/// scopes. The verified incumbent is the exact
/// compare-and-swap input, so concurrent merge operations cannot silently
/// lose scopes.
#[allow(clippy::too_many_arguments)]
pub fn reconcile_authorized_key_toml_scopes(
    auth_dir: &Path,
    fingerprint: &str,
    public_key_b64: &str,
    requested_scopes: &[String],
    label: &str,
    granted_by: &str,
    created_at: &str,
    node_identity: &NodeIdentity,
    wildcard: WildcardPolicy,
    merge: bool,
    remote_operator_origin_site_id: Option<&str>,
    allow_semantic_conversion: bool,
) -> Result<(std::path::PathBuf, Vec<String>, AuthorizedKeyTransition)> {
    if let Some(origin_site_id) = remote_operator_origin_site_id {
        validate_canonical_site_id(origin_site_id)
            .context("remote-operator origin_site_id is not canonical")?;
        if wildcard != WildcardPolicy::Reject {
            bail!("remote-operator grants require exact, non-wildcard scopes");
        }
    }
    let directory = lillux::PinnedDirectory::open_or_create(auth_dir)?;
    let _lock = directory.lock_exclusive()?;
    directory.ensure_path_binding()?;
    let existing =
        load_verified_authorized_key_from_directory(fingerprint, &directory, node_identity)?;
    let requested_class = if remote_operator_origin_site_id.is_some() {
        AuthorizedKeyPrincipalClass::RemoteOperator
    } else {
        AuthorizedKeyPrincipalClass::LocalClient
    };
    if merge
        && let Some(existing) = existing.as_ref()
        && (existing.principal_class != requested_class
            || existing.configured_origin_site_id.as_deref() != remote_operator_origin_site_id)
    {
        bail!(
            "cannot merge scopes while changing authorized-key principal class or origin site; rerun without --merge-scopes as an explicit stopped-daemon conversion"
        );
    }
    if let Some(existing) = existing.as_ref()
        && (existing.principal_class != requested_class
            || existing.configured_origin_site_id.as_deref() != remote_operator_origin_site_id)
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
        origin_site_id: remote_operator_origin_site_id.map(str::to_owned),
    };
    let subject = match remote_operator_origin_site_id {
        Some(origin_site_id) => AuthorizedKeySubject::RemoteOperator { origin_site_id },
        None => AuthorizedKeySubject::LocalClient,
    };
    let signed = render_authorized_key_toml_for_subject(
        fingerprint,
        public_key_b64,
        &final_scopes,
        label,
        granted_by,
        created_at,
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

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

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
        assert!(body.contains("schema_version = 2"));
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
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source"),
        )
        .unwrap();
        assert!(
            validate_forwarding_origin_assertion(
                None,
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
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
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
                Some("site:different")
            )
            .is_err()
        );
    }

    #[test]
    fn remote_operator_reconciliation_preserves_owner_and_origin() {
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
            AuthorizedKeyPrincipalClass::RemoteOperator
        );
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("principal_class = \"remote_operator\""));
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
            AuthorizedKeyPrincipalClass::RemoteOperator
        );
        assert_eq!(
            grant.configured_origin_site_id.as_deref(),
            Some("site:source")
        );
    }

    #[test]
    fn remote_operator_reconciliation_rejects_invalid_origin_and_wildcard_mode() {
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
            AuthorizedKeyPrincipalClass::RemoteOperator
        );
        assert_eq!(
            grant.configured_origin_site_id.as_deref(),
            Some("site:source")
        );
    }
}
