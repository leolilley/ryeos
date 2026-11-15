//! Rotation of the operator key and the authoring key.
//!
//! A rotation is a signed offline operation on a stopped node. The command
//! first generates the new key into a staging path inside the key's home and
//! has the old key sign a succession statement naming it (`prepare_rotation`).
//! The operation then (`rotate`):
//!
//! 1. records the succession in the signer registry: the new key becomes an
//!    active signer with the old key's role and scope, the old key is retired
//!    and keeps verifying what it signed;
//! 2. for the operator only, replaces the operator genesis, re-issues the
//!    operator's authorized-key grant under the new key (`never` expiry,
//!    `authorized_by` naming the rotation request), removes the old key's
//!    grant, and reseals the init completion under the new key;
//! 3. installs the new key at the canonical key path, which destroys the old
//!    private key. This is the commit point and the last effect, so a request
//!    that stopped earlier is finished by running it again.
//!
//! The node key is never rotated here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use ryeos_engine::roots::RuntimeRoot;
use ryeos_engine::trust::{SignerRecord, SignerRole, SignerStatus, SuccessionStatement};
use ryeos_node_runtime::identity::{AuthorizedBy, GrantTerms, WildcardPolicy};
use ryeos_node_runtime::state_lock::StateLock;
use serde_json::{Value, json};

use crate::trust_registry::{Actor, Change};

/// Which key a rotation replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotatedKey {
    Operator,
    Authoring,
}

impl RotatedKey {
    pub fn name(self) -> &'static str {
        match self {
            RotatedKey::Operator => "operator",
            RotatedKey::Authoring => "authoring",
        }
    }

    pub fn role(self) -> SignerRole {
        match self {
            RotatedKey::Operator => SignerRole::Operator,
            RotatedKey::Authoring => SignerRole::Authoring,
        }
    }

    /// The offline operation that applies this rotation.
    pub fn operation(self) -> &'static str {
        match self {
            RotatedKey::Operator => "node.operator.rotate",
            RotatedKey::Authoring => "node.authoring.rotate",
        }
    }

    /// The command that starts this rotation.
    pub fn command(self) -> &'static str {
        match self {
            RotatedKey::Operator => "ryeos node operator rotate",
            RotatedKey::Authoring => "ryeos node authoring rotate",
        }
    }

    fn home(self, root: &RuntimeRoot) -> PathBuf {
        match self {
            RotatedKey::Operator => root.operator_dir(),
            RotatedKey::Authoring => root.authoring_dir(),
        }
    }

    fn key_path(self, root: &RuntimeRoot) -> PathBuf {
        match self {
            RotatedKey::Operator => root.operator_signing_key_path(),
            RotatedKey::Authoring => root.authoring_signing_key_path(),
        }
    }

    /// Where the command stages the new key until the rotation installs it.
    pub fn staged_dir(self, root: &RuntimeRoot) -> PathBuf {
        self.home(root).join("rotation")
    }

    fn staged_key_path(self, root: &RuntimeRoot) -> PathBuf {
        self.staged_dir(root).join("next").join("private_key.pem")
    }
}

/// A refusal of a key rotation. Every one names its cure.
#[derive(Debug)]
pub enum KeyRotationError {
    NoCurrentKey {
        key: RotatedKey,
        path: PathBuf,
    },
    NotPinned {
        key: RotatedKey,
        fingerprint: String,
    },
    NotActive {
        key: RotatedKey,
        fingerprint: String,
        status: SignerStatus,
    },
    StagedKeyMissing {
        key: RotatedKey,
        path: PathBuf,
    },
    StagedKeyMismatch {
        key: RotatedKey,
        staged: String,
        statement: String,
    },
    OldKeyMismatch {
        key: RotatedKey,
        statement: String,
        current: String,
        path: PathBuf,
    },
    SignerNotOldOperator {
        signer: String,
        old: String,
    },
}

impl KeyRotationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoCurrentKey { .. } => "key_rotation_no_current_key",
            Self::NotPinned { .. } => "key_rotation_not_pinned",
            Self::NotActive { .. } => "key_rotation_not_active",
            Self::StagedKeyMissing { .. } => "key_rotation_staged_key_missing",
            Self::StagedKeyMismatch { .. } => "key_rotation_staged_key_mismatch",
            Self::OldKeyMismatch { .. } => "key_rotation_old_key_mismatch",
            Self::SignerNotOldOperator { .. } => "key_rotation_signer_not_old_operator",
        }
    }
}

impl std::fmt::Display for KeyRotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Self::NoCurrentKey { key, path } => write!(
                f,
                "{code}: this node has no {} key at {}, so there is nothing to rotate. Run \
                 `ryeos init` to create it",
                key.name(),
                path.display()
            ),
            Self::NotPinned { key, fingerprint } => write!(
                f,
                "{code}: the {} key {fingerprint} has no record in the signer registry. Run \
                 `ryeos init` to pin it, then {} again",
                key.name(),
                key.command()
            ),
            Self::NotActive {
                key,
                fingerprint,
                status,
            } => write!(
                f,
                "{code}: the {} key {fingerprint} is {} in the signer registry and cannot be \
                 rotated. Inspect it with `ryeos trust show {fingerprint}`; a node whose \
                 {} key is not active needs `ryeos init` on a fresh app root",
                key.name(),
                status.as_str(),
                key.name()
            ),
            Self::StagedKeyMissing { key, path } => write!(
                f,
                "{code}: no staged new {} key at {}. The rotation operation applies a key the \
                 command stages first; run {} instead of submitting the operation by hand",
                key.name(),
                path.display(),
                key.command()
            ),
            Self::StagedKeyMismatch {
                key,
                staged,
                statement,
            } => write!(
                f,
                "{code}: the staged new {} key is {staged} but the succession statement names \
                 {statement}. Run {} again to stage a key and sign a matching statement",
                key.name(),
                key.command()
            ),
            Self::OldKeyMismatch {
                key,
                statement,
                current,
                path,
            } => write!(
                f,
                "{code}: the succession statement replaces {statement}, but the {} key at {} \
                 is {current}. Run {} again to sign a statement with the current key",
                key.name(),
                path.display(),
                key.command()
            ),
            Self::SignerNotOldOperator { signer, old } => write!(
                f,
                "{code}: an operator rotation must be signed by the key being rotated out \
                 ({old}), but the request is signed by {signer}. Run `ryeos node operator \
                 rotate` as the node's own operator"
            ),
        }
    }
}

impl std::error::Error for KeyRotationError {}

/// What the command hands to the rotation operation.
#[derive(Debug)]
pub struct PreparedRotation {
    /// The succession statement, signed by the old key.
    pub statement: Vec<u8>,
    pub old_fingerprint: String,
    pub new_fingerprint: String,
}

fn require_active(
    key: RotatedKey,
    records: &[SignerRecord],
    fingerprint: &str,
) -> Result<(), KeyRotationError> {
    let Some(record) = records.iter().find(|r| r.fingerprint == fingerprint) else {
        return Err(KeyRotationError::NotPinned {
            key,
            fingerprint: fingerprint.to_owned(),
        });
    };
    if record.role != key.role() {
        return Err(KeyRotationError::NotPinned {
            key,
            fingerprint: fingerprint.to_owned(),
        });
    }
    if record.status != SignerStatus::Active {
        return Err(KeyRotationError::NotActive {
            key,
            fingerprint: fingerprint.to_owned(),
            status: record.status,
        });
    }
    Ok(())
}

fn load_current(key: RotatedKey, root: &RuntimeRoot) -> Result<SigningKey> {
    let path = key.key_path(root);
    if lillux::read_optional_regular_file_bounded_no_follow(&path, 64 * 1024)
        .with_context(|| format!("inspect the {} key {}", key.name(), path.display()))?
        .is_none()
    {
        return Err(KeyRotationError::NoCurrentKey { key, path }.into());
    }
    lillux::crypto::load_signing_key(&path)
}

/// Generate (or reuse) the staged new key and sign the succession statement
/// with the current key. Writes only the staged key. The caller holds the
/// stopped node.
pub fn prepare_rotation(app_root: &Path, key: RotatedKey) -> Result<PreparedRotation> {
    let root = RuntimeRoot::new(app_root.to_path_buf());
    let old = load_current(key, &root)?;
    let old_fingerprint = ryeos_engine::trust::compute_fingerprint(&old.verifying_key());
    let view = crate::trust_registry::load_view(app_root)?;
    require_active(key, &view.records, &old_fingerprint)?;
    let staged_path = key.staged_key_path(&root);
    let new = match lillux::read_optional_regular_file_bounded_no_follow(&staged_path, 64 * 1024)? {
        Some(_) => lillux::crypto::load_signing_key(&staged_path)?,
        None => lillux::crypto::create_signing_key(&staged_path)?,
    };
    let new_fingerprint = ryeos_engine::trust::compute_fingerprint(&new.verifying_key());
    let statement = SuccessionStatement::new(
        &old_fingerprint,
        &new.verifying_key(),
        &lillux::time::iso8601_now(),
    )
    .render_signed(&old)
    .map_err(|reason| anyhow::anyhow!("sign the succession statement: {reason}"))?;
    Ok(PreparedRotation {
        statement: statement.into_bytes(),
        old_fingerprint,
        new_fingerprint,
    })
}

/// What a rotation is applied with.
pub struct RotationRequest<'a> {
    pub app_root: &'a Path,
    pub state_lock: &'a StateLock,
    pub node_key: &'a SigningKey,
    pub request_digest: &'a str,
    /// Fingerprint of the key that signed the offline request.
    pub signer_fingerprint: &'a str,
    pub statement: &'a str,
}

/// Apply a rotation. Every step converges, so running a request again after
/// any interruption finishes it.
pub fn rotate(key: RotatedKey, request: RotationRequest<'_>) -> Result<Value> {
    let RotationRequest {
        app_root,
        state_lock,
        node_key,
        request_digest,
        signer_fingerprint,
        statement,
    } = request;
    let root = RuntimeRoot::new(app_root.to_path_buf());
    let current = load_current(key, &root)?;
    let current_fingerprint = ryeos_engine::trust::compute_fingerprint(&current.verifying_key());
    let named = SuccessionStatement::parse_unverified(statement)
        .map_err(|reason| crate::trust_registry::TrustChangeError::SuccessionInvalid { reason })?;
    let key_path = key.key_path(&root);

    if current_fingerprint == named.new_fingerprint {
        // The new key is already installed: the commit point passed.
        return Ok(json!({
            "key": key.name(),
            "old_fingerprint": named.old_fingerprint,
            "new_fingerprint": named.new_fingerprint,
            "already_rotated": true,
        }));
    }
    if current_fingerprint != named.old_fingerprint {
        return Err(KeyRotationError::OldKeyMismatch {
            key,
            statement: named.old_fingerprint,
            current: current_fingerprint,
            path: key_path,
        }
        .into());
    }
    let view = crate::trust_registry::load_view(app_root)?;
    require_active_or_succeeded(key, &view.records, &named)?;
    if key == RotatedKey::Operator && signer_fingerprint != named.old_fingerprint {
        return Err(KeyRotationError::SignerNotOldOperator {
            signer: signer_fingerprint.to_owned(),
            old: named.old_fingerprint,
        }
        .into());
    }

    let staged_path = key.staged_key_path(&root);
    if lillux::read_optional_regular_file_bounded_no_follow(&staged_path, 64 * 1024)?.is_none() {
        return Err(KeyRotationError::StagedKeyMissing {
            key,
            path: staged_path,
        }
        .into());
    }
    let staged = lillux::crypto::load_signing_key(&staged_path)?;
    let staged_fingerprint = ryeos_engine::trust::compute_fingerprint(&staged.verifying_key());
    if staged_fingerprint != named.new_fingerprint {
        return Err(KeyRotationError::StagedKeyMismatch {
            key,
            staged: staged_fingerprint,
            statement: named.new_fingerprint,
        }
        .into());
    }

    // 1. The succession, recorded in the registry.
    let outcome = crate::trust_registry::apply_change(
        app_root,
        state_lock,
        &Actor {
            node_key,
            operator: signer_fingerprint,
            request_digest,
        },
        &Change::Succeed {
            old_fingerprint: named.old_fingerprint.clone(),
            statement: statement.to_owned(),
        },
    )?;

    let mut result = json!({
        "key": key.name(),
        "old_fingerprint": named.old_fingerprint,
        "new_fingerprint": named.new_fingerprint,
        "already_rotated": false,
        "records": outcome.records,
        "events_written": outcome.events_written,
        "registry_digest": outcome.registry_digest,
    });

    // 2. What the operator key carries beyond the registry record.
    if key == RotatedKey::Operator {
        crate::init::rotate_operator_genesis(app_root, &staged, &named.old_fingerprint)?;
        let grants = reissue_operator_grant(
            app_root,
            node_key,
            &staged,
            &named.old_fingerprint,
            request_digest,
        )?;
        crate::init::seal_current_init_completion_as(app_root, None, &staged)
            .context("reseal the init completion under the new operator key")?;
        result["operator_grant"] = grants;
        result["genesis"] = json!({
            "operator_fingerprint": named.new_fingerprint,
            "predecessor_fingerprint": named.old_fingerprint,
        });
    }

    // 3. Install the new key. The atomic replace destroys the old private
    // key; the staged copy goes after it.
    let pem = lillux::read_regular_file_bounded_no_follow(&staged_path, 64 * 1024)?;
    lillux::atomic_write_private(&key_path, &pem)
        .map_err(|error| anyhow::anyhow!("install the new key {}: {error}", key_path.display()))?;
    lillux::remove_dir_all_durable(&key.staged_dir(&root))
        .context("remove the staged key after installing it")?;
    result["installed_at"] = json!(key_path);
    Ok(result)
}

/// The old key is `active` before the succession and `retired` after it, so a
/// rerun over a recorded succession passes. A revoked or foreign-role key is
/// refused.
fn require_active_or_succeeded(
    key: RotatedKey,
    records: &[SignerRecord],
    named: &SuccessionStatement,
) -> Result<(), KeyRotationError> {
    let Some(old) = records
        .iter()
        .find(|r| r.fingerprint == named.old_fingerprint)
    else {
        return Err(KeyRotationError::NotPinned {
            key,
            fingerprint: named.old_fingerprint.clone(),
        });
    };
    if old.role != key.role() {
        return Err(KeyRotationError::NotPinned {
            key,
            fingerprint: named.old_fingerprint.clone(),
        });
    }
    let recorded = records
        .iter()
        .any(|r| r.fingerprint == named.new_fingerprint && r.status == SignerStatus::Active);
    if old.status == SignerStatus::Active || (old.status == SignerStatus::Retired && recorded) {
        return Ok(());
    }
    Err(KeyRotationError::NotActive {
        key,
        fingerprint: named.old_fingerprint.clone(),
        status: old.status,
    })
}

/// Issue the new operator's grant (schema 3, `never`, `authorized_by` naming
/// the request that asked for the rotation) and remove the old key's.
fn reissue_operator_grant(
    app_root: &Path,
    node_key: &SigningKey,
    new_operator: &SigningKey,
    old_fingerprint: &str,
    request_digest: &str,
) -> Result<Value> {
    use base64::Engine as _;
    let directory = crate::offline_operation::authorized_keys_dir(app_root);
    let new_fingerprint = ryeos_engine::trust::compute_fingerprint(&new_operator.verifying_key());
    let grant_path = directory.join(format!("{new_fingerprint}.toml"));
    let issued =
        lillux::read_optional_regular_file_bounded_no_follow(&grant_path, 1024 * 1024)?.is_none();
    if issued {
        let public_key = base64::engine::general_purpose::STANDARD
            .encode(new_operator.verifying_key().as_bytes());
        ryeos_node_runtime::identity::write_authorized_key_toml(
            &directory,
            &new_fingerprint,
            &public_key,
            &["*".to_string()],
            "operator",
            &new_fingerprint,
            &lillux::time::iso8601_now(),
            &GrantTerms::operator(AuthorizedBy::principal_request(
                old_fingerprint,
                Some(request_digest.to_owned()),
            )),
            node_key,
            WildcardPolicy::AllowBootstrap,
        )?;
    }
    lillux::remove_file_durable(&directory.join(format!("{old_fingerprint}.toml")))
        .map_err(|error| anyhow::anyhow!("remove the retired operator's grant: {error}"))?;
    Ok(json!({
        "issued": new_fingerprint,
        "removed": old_fingerprint,
        "expires_at": "never",
        "authorized_by": {
            "principal": format!("fp:{old_fingerprint}"),
            "request_digest": request_digest,
        },
    }))
}

/// How long one key has been in service, as the doctor reports it. Age is a
/// report: no threshold turns it into a refusal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KeyAge {
    /// `node`, `operator`, `authoring`, `publisher` or `project_author`.
    pub role: String,
    pub label: String,
    pub fingerprint: String,
    /// `active`, `retired` or `revoked`; `implicit` for the node key.
    pub status: String,
    /// When the key entered service, as written in the record it comes from.
    pub since: Option<String>,
    pub age_days: Option<u64>,
    /// Where `since` was read: `operator_genesis`, `registry_event` or
    /// `node_identity`.
    pub source: &'static str,
}

fn age_days(since: &str, now_unix: u64) -> Option<u64> {
    let then = lillux::time::unix_secs_from_iso8601(since)?;
    Some(now_unix.saturating_sub(then) / 86_400)
}

/// The age of the node key and of every operator, authoring and publisher key
/// in the signer registry. The operator's age comes from its genesis when the
/// genesis names it, every other key's from the signing time of the registry
/// event that pinned it, and the node key's from its public identity.
pub fn key_ages(app_root: &Path, now_unix: u64) -> Result<Vec<KeyAge>> {
    use ryeos_engine::trust::{
        events_dir, key_pinned_at, load_event_chain_timed, load_node_verifying_key,
        load_signer_records, signers_dir,
    };
    let node_key = load_node_verifying_key(app_root)?;
    let records = load_signer_records(&signers_dir(app_root), &node_key)?;
    let chain = load_event_chain_timed(&events_dir(app_root), &node_key)?;
    let root = RuntimeRoot::new(app_root.to_path_buf());
    let mut ages = Vec::new();

    let node_identity = ryeos_node_runtime::identity::NodeIdentity::load_public_identity(
        &ryeos_engine::trust::node_public_identity_path(app_root),
    )
    .ok();
    ages.push(KeyAge {
        role: "node".to_owned(),
        label: "node".to_owned(),
        fingerprint: ryeos_engine::trust::compute_fingerprint(&node_key),
        status: "implicit".to_owned(),
        age_days: node_identity
            .as_ref()
            .and_then(|doc| age_days(&doc.created_at, now_unix)),
        since: node_identity.map(|doc| doc.created_at),
        source: "node_identity",
    });

    let genesis = lillux::read_optional_regular_file_bounded_no_follow(
        &root.operator_genesis_path(),
        64 * 1024,
    )?
    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let genesis_created = |fingerprint: &str| -> Option<String> {
        let genesis = genesis.as_ref()?;
        if genesis.get("operator_fingerprint")?.as_str()? != fingerprint {
            return None;
        }
        genesis.get("created_at")?.as_str().map(str::to_owned)
    };

    let mut keyed = records
        .iter()
        .filter(|record| {
            matches!(
                record.role,
                SignerRole::Operator | SignerRole::Authoring | SignerRole::Publisher
            )
        })
        .collect::<Vec<_>>();
    keyed.sort_by(|a, b| {
        (a.role.as_str(), &a.label, &a.fingerprint).cmp(&(
            b.role.as_str(),
            &b.label,
            &b.fingerprint,
        ))
    });
    for record in keyed {
        let from_genesis = if record.role == SignerRole::Operator {
            genesis_created(&record.fingerprint)
        } else {
            None
        };
        let (since, source) = match from_genesis {
            Some(created) => (Some(created), "operator_genesis"),
            None => (key_pinned_at(&chain, &record.fingerprint), "registry_event"),
        };
        ages.push(KeyAge {
            role: record.role.as_str().to_owned(),
            label: record.label.clone(),
            fingerprint: record.fingerprint.clone(),
            status: record.status.as_str().to_owned(),
            age_days: since.as_deref().and_then(|s| age_days(s, now_unix)),
            since,
            source,
        });
    }
    Ok(ages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_registry::{Actor, Change, apply_change, load_view};
    use ryeos_engine::trust::{SignerScope, SignerSpace, TrustStore};
    use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

    struct Node {
        root: tempfile::TempDir,
        key: SigningKey,
    }

    impl Node {
        fn new() -> Self {
            use base64::Engine as _;
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join(".ai/node/identity");
            std::fs::create_dir_all(&dir).unwrap();
            let key = lillux::crypto::create_signing_key(&dir.join("private_key.pem")).unwrap();
            let vk = key.verifying_key();
            let doc = json!({
                "kind": "identity/v1",
                "principal_id": format!("fp:{}", ryeos_engine::trust::compute_fingerprint(&vk)),
                "signing_key": format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(vk.as_bytes())
                ),
            });
            std::fs::write(dir.join("public-identity.json"), doc.to_string()).unwrap();
            Self { root, key }
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        /// Create the key of `key` at its canonical path and pin it.
        fn with_key(&self, key: RotatedKey, scope: SignerScope) -> String {
            let root = RuntimeRoot::new(self.path().to_path_buf());
            let created = lillux::crypto::create_signing_key(&key.key_path(&root)).unwrap();
            let lock = StateLock::acquire(&default_lock_path(self.path())).unwrap();
            apply_change(
                self.path(),
                &lock,
                &Actor {
                    node_key: &self.key,
                    operator: "op",
                    request_digest: &format!("pin-{}", key.name()),
                },
                &Change::Pin {
                    key: created.verifying_key(),
                    label: key.name().to_owned(),
                    role: key.role(),
                    scope,
                },
            )
            .unwrap();
            ryeos_engine::trust::compute_fingerprint(&created.verifying_key())
        }

        fn rotate(
            &self,
            key: RotatedKey,
            request: &str,
            signer: &str,
            statement: &str,
        ) -> Result<Value> {
            let lock = StateLock::acquire(&default_lock_path(self.path())).unwrap();
            rotate(
                key,
                RotationRequest {
                    app_root: self.path(),
                    state_lock: &lock,
                    node_key: &self.key,
                    request_digest: request,
                    signer_fingerprint: signer,
                    statement,
                },
            )
        }

        fn installed(&self, key: RotatedKey) -> String {
            let root = RuntimeRoot::new(self.path().to_path_buf());
            let loaded = lillux::crypto::load_signing_key(&key.key_path(&root)).unwrap();
            ryeos_engine::trust::compute_fingerprint(&loaded.verifying_key())
        }
    }

    fn knowledge_scope() -> SignerScope {
        SignerScope {
            spaces: vec![SignerSpace::Project],
            kinds: vec!["knowledge".to_owned()],
            projects: Vec::new(),
        }
    }

    fn error_code(error: &anyhow::Error) -> &'static str {
        error
            .downcast_ref::<KeyRotationError>()
            .map(KeyRotationError::code)
            .unwrap_or("not_a_key_rotation_error")
    }

    #[test]
    fn prepare_stages_a_key_and_signs_the_succession_with_the_old_key() {
        let node = Node::new();
        let old_fp = node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let prepared = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        assert_eq!(prepared.old_fingerprint, old_fp);
        assert_ne!(prepared.new_fingerprint, old_fp);
        let view = load_view(node.path()).unwrap();
        let old = view
            .records
            .iter()
            .find(|r| r.fingerprint == old_fp)
            .unwrap();
        let statement = SuccessionStatement::parse_and_verify(
            std::str::from_utf8(&prepared.statement).unwrap(),
            old,
        )
        .unwrap();
        assert_eq!(statement.new_fingerprint, prepared.new_fingerprint);
        // Nothing is installed yet and a second prepare reuses the staged key.
        assert_eq!(node.installed(RotatedKey::Authoring), old_fp);
        let again = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        assert_eq!(again.new_fingerprint, prepared.new_fingerprint);
    }

    #[test]
    fn authoring_rotation_retires_the_old_key_and_the_new_one_keeps_its_scope() {
        let node = Node::new();
        let old_fp = node.with_key(RotatedKey::Authoring, knowledge_scope());
        let prepared = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let statement = String::from_utf8(prepared.statement).unwrap();
        let result = node
            .rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap();
        assert_eq!(result["already_rotated"], false);
        assert_eq!(
            node.installed(RotatedKey::Authoring),
            prepared.new_fingerprint
        );
        let root = RuntimeRoot::new(node.path().to_path_buf());
        assert!(!RotatedKey::Authoring.staged_dir(&root).exists());

        let store = TrustStore::load_node_registry(node.path()).unwrap();
        let old = store.signer(&old_fp).unwrap();
        let new = store.signer(&prepared.new_fingerprint).unwrap();
        assert_eq!(old.status, SignerStatus::Retired);
        assert_eq!(new.status, SignerStatus::Active);
        assert_eq!(new.role, SignerRole::Authoring);
        assert_eq!(new.scope, knowledge_scope());
        assert!(
            store.is_trusted(&old_fp),
            "items the retired key signed still verify"
        );
    }

    #[test]
    fn rerunning_a_finished_rotation_changes_nothing() {
        let node = Node::new();
        node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let prepared = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let statement = String::from_utf8(prepared.statement).unwrap();
        node.rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap();
        let digest = load_view(node.path()).unwrap().registry_digest;
        let again = node
            .rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap();
        assert_eq!(again["already_rotated"], true);
        assert_eq!(load_view(node.path()).unwrap().registry_digest, digest);
    }

    #[test]
    fn a_rotation_that_stopped_before_installing_the_key_is_finished_by_a_rerun() {
        let node = Node::new();
        let old_fp = node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let prepared = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let statement = String::from_utf8(prepared.statement).unwrap();
        // The succession was recorded, then the process stopped.
        let lock = StateLock::acquire(&default_lock_path(node.path())).unwrap();
        apply_change(
            node.path(),
            &lock,
            &Actor {
                node_key: &node.key,
                operator: "op",
                request_digest: "req-1",
            },
            &Change::Succeed {
                old_fingerprint: old_fp.clone(),
                statement: statement.clone(),
            },
        )
        .unwrap();
        drop(lock);
        assert_eq!(node.installed(RotatedKey::Authoring), old_fp);
        node.rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap();
        assert_eq!(
            node.installed(RotatedKey::Authoring),
            prepared.new_fingerprint
        );
    }

    #[test]
    fn rotation_without_a_staged_key_names_the_command() {
        let node = Node::new();
        node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let prepared = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let statement = String::from_utf8(prepared.statement).unwrap();
        let root = RuntimeRoot::new(node.path().to_path_buf());
        std::fs::remove_dir_all(RotatedKey::Authoring.staged_dir(&root)).unwrap();
        let error = node
            .rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_staged_key_missing");
        assert!(error.to_string().contains("ryeos node authoring rotate"));
    }

    #[test]
    fn a_statement_for_another_key_is_refused() {
        let node = Node::new();
        let current = node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let other = SigningKey::from_bytes(&[7u8; 32]);
        let other_fp = ryeos_engine::trust::compute_fingerprint(&other.verifying_key());
        let statement = SuccessionStatement::new(
            &other_fp,
            &SigningKey::from_bytes(&[8u8; 32]).verifying_key(),
            "2026-01-01T00:00:00Z",
        )
        .render_signed(&other)
        .unwrap();
        let error = node
            .rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_old_key_mismatch");
        assert!(error.to_string().contains(&current));
        assert_eq!(node.installed(RotatedKey::Authoring), current);
    }

    #[test]
    fn a_staged_key_the_statement_does_not_name_is_refused() {
        let node = Node::new();
        let current = node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        prepare_rotation(node.path(), RotatedKey::Authoring).unwrap();
        let old = lillux::crypto::load_signing_key(
            &RotatedKey::Authoring.key_path(&RuntimeRoot::new(node.path().to_path_buf())),
        )
        .unwrap();
        let statement = SuccessionStatement::new(
            &current,
            &SigningKey::from_bytes(&[8u8; 32]).verifying_key(),
            "2026-01-01T00:00:00Z",
        )
        .render_signed(&old)
        .unwrap();
        let error = node
            .rotate(RotatedKey::Authoring, "req-1", &"d".repeat(64), &statement)
            .unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_staged_key_mismatch");
        assert_eq!(node.installed(RotatedKey::Authoring), current);
    }

    #[test]
    fn operator_rotate_requires_old_key_signature() {
        let node = Node::new();
        let old_fp = node.with_key(RotatedKey::Operator, SignerScope::operator_default());
        let prepared = prepare_rotation(node.path(), RotatedKey::Operator).unwrap();
        let statement = String::from_utf8(prepared.statement).unwrap();
        let before = load_view(node.path()).unwrap().registry_digest;
        let error = node
            .rotate(RotatedKey::Operator, "req-1", &"e".repeat(64), &statement)
            .unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_signer_not_old_operator");
        assert!(error.to_string().contains(&old_fp));
        assert_eq!(load_view(node.path()).unwrap().registry_digest, before);
        assert_eq!(node.installed(RotatedKey::Operator), old_fp);
    }

    #[test]
    fn rotating_a_key_with_no_record_names_init() {
        let node = Node::new();
        let root = RuntimeRoot::new(node.path().to_path_buf());
        lillux::crypto::create_signing_key(&RotatedKey::Authoring.key_path(&root)).unwrap();
        let error = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_not_pinned");
        assert!(error.to_string().contains("ryeos init"));
    }

    #[test]
    fn rotating_a_node_with_no_key_names_init() {
        let node = Node::new();
        let error = prepare_rotation(node.path(), RotatedKey::Operator).unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_no_current_key");
        assert!(error.to_string().contains("ryeos init"));
    }

    #[test]
    fn a_retired_key_cannot_be_rotated_again() {
        let node = Node::new();
        let old_fp = node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let lock = StateLock::acquire(&default_lock_path(node.path())).unwrap();
        apply_change(
            node.path(),
            &lock,
            &Actor {
                node_key: &node.key,
                operator: "op",
                request_digest: "retire",
            },
            &Change::Retire {
                fingerprint: old_fp.clone(),
            },
        )
        .unwrap();
        drop(lock);
        let error = prepare_rotation(node.path(), RotatedKey::Authoring).unwrap_err();
        assert_eq!(error_code(&error), "key_rotation_not_active");
        assert!(error.to_string().contains("ryeos trust show"));
    }

    #[test]
    fn the_reissued_operator_grant_is_schema_3_never_and_names_the_request() {
        let node = Node::new();
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &RuntimeRoot::new(node.path().to_path_buf()).node_signing_key_path(),
        )
        .unwrap();
        let old = SigningKey::from_bytes(&[3u8; 32]);
        let new = SigningKey::from_bytes(&[4u8; 32]);
        let old_fp = ryeos_engine::trust::compute_fingerprint(&old.verifying_key());
        let new_fp = ryeos_engine::trust::compute_fingerprint(&new.verifying_key());
        let directory = crate::offline_operation::authorized_keys_dir(node.path());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{old_fp}.toml")), "placeholder").unwrap();

        let request = "f".repeat(64);
        let report =
            reissue_operator_grant(node.path(), &node.key, &new, &old_fp, &request).unwrap();
        assert_eq!(report["issued"], new_fp);
        assert_eq!(report["removed"], old_fp);
        assert!(!directory.join(format!("{old_fp}.toml")).exists());
        let text = std::fs::read_to_string(directory.join(format!("{new_fp}.toml"))).unwrap();
        assert!(text.contains("schema_version = 3"), "{text}");
        let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
            &new_fp, &directory, &identity,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            grant.expires_at,
            ryeos_node_runtime::identity::GrantExpiry::Never
        );
        assert_eq!(grant.authorized_by.principal, format!("fp:{old_fp}"));
        assert_eq!(
            grant.authorized_by.request_digest.as_deref(),
            Some(request.as_str())
        );
        // Issuing again keeps the grant.
        reissue_operator_grant(node.path(), &node.key, &new, &old_fp, &request).unwrap();
    }

    #[test]
    fn key_ages_report_every_registry_key_with_its_pin_time() {
        let node = Node::new();
        node.with_key(RotatedKey::Operator, SignerScope::operator_default());
        node.with_key(RotatedKey::Authoring, SignerScope::authoring_default());
        let now = lillux::time::unix_secs_now();
        let ages = key_ages(node.path(), now + 3 * 86_400).unwrap();
        let roles = ages.iter().map(|a| a.role.as_str()).collect::<Vec<_>>();
        assert_eq!(roles, ["node", "authoring", "operator"]);
        let authoring = ages.iter().find(|a| a.role == "authoring").unwrap();
        assert_eq!(authoring.source, "registry_event");
        assert_eq!(authoring.status, "active");
        assert!(authoring.since.is_some());
        assert_eq!(authoring.age_days, Some(3));
        let node_row = ages.iter().find(|a| a.role == "node").unwrap();
        assert_eq!(node_row.status, "implicit");
    }
}
