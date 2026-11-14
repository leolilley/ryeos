//! Node-owned bundle state homes.
//!
//! A bundle's own verified code may keep durable state in a directory the node
//! owns and the node's signed record vouches for:
//!
//! ```text
//! <app_root>/.ai/state/bundle-state/<bundle_id>/<namespace>/   0700
//! <app_root>/.ai/state/bundle-state/<bundle_id>/.home.json     node-signed record
//! ```
//!
//! Who may open which namespace is decided elsewhere (the signed manifest backs
//! the declaration, the caller's grant carries the `use` capability); this
//! module owns the directory mechanics: creating homes under a lock, signing
//! and verifying their records, refusing wider-than-0700 directories, and
//! inventorying what exists for the doctor. It never interprets the bytes a
//! bundle stores.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use lillux::crypto::{Signature, Signer as _, Verifier as _, VerifyingKey};
use ryeos_bundle::runtime_authority::{
    StateHomeRefusal, bundle_state_use_cap, validate_state_home_segment,
};
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};

/// Directory under `<app_root>/.ai/state` that holds every bundle's homes.
pub const BUNDLE_STATE_DIR: &str = "bundle-state";
/// The node-signed record at the top of one bundle's home.
pub const HOME_RECORD_NAME: &str = ".home.json";
/// Format of the signed home record.
pub const HOME_RECORD_SCHEMA: u32 = 1;
/// Where isolation mounts the launch's one namespace, so the path a workload
/// sees does not depend on where the node keeps its app root.
pub const BUNDLE_STATE_MOUNT_DESTINATION: &str =
    ryeos_engine::isolation::BUNDLE_STATE_ISOLATION_DESTINATION;

const MAX_RECORD_BYTES: usize = 16 * 1024;
/// Most entries and deepest nesting the doctor will walk in one namespace.
const INVENTORY_BUDGET: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget::new(1_000_000, 64);
const INVENTORY_MAX_BYTES: u64 = u64::MAX / 2;

/// The directory holding every bundle state home of the node at `app_root`.
pub fn root_path(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(BUNDLE_STATE_DIR)
}

/// One namespace directory of one bundle.
pub fn namespace_path(app_root: &Path, bundle_id: &str, namespace: &str) -> PathBuf {
    root_path(app_root).join(bundle_id).join(namespace)
}

/// The signed record a bundle's home carries. The node signs the unsigned body
/// (canonical JSON) with its identity key; a record that does not verify under
/// this node's key is foreign or tampered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeRecord {
    pub schema: u32,
    pub bundle_id: String,
    pub created_at: String,
    /// Fingerprint of the bundle generation the node served when it created
    /// the home.
    pub created_by_generation: String,
    /// `fp:<fingerprint>` of the signing node.
    pub signer: String,
    /// Base64 ed25519 signature over [`HomeRecord::signing_payload`].
    pub signature: String,
}

impl HomeRecord {
    fn unsigned_body(
        bundle_id: &str,
        created_at: &str,
        created_by_generation: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": HOME_RECORD_SCHEMA,
            "bundle_id": bundle_id,
            "created_at": created_at,
            "created_by_generation": created_by_generation,
        })
    }

    fn signing_payload(&self) -> Result<Vec<u8>> {
        Ok(lillux::canonical_json(&Self::unsigned_body(
            &self.bundle_id,
            &self.created_at,
            &self.created_by_generation,
        ))?
        .into_bytes())
    }

    /// Create and sign a record for `bundle_id`.
    pub fn sign(
        identity: &NodeIdentity,
        bundle_id: &str,
        created_at: &str,
        created_by_generation: &str,
    ) -> Result<Self> {
        let payload = lillux::canonical_json(&Self::unsigned_body(
            bundle_id,
            created_at,
            created_by_generation,
        ))?
        .into_bytes();
        let signature: Signature = identity.signing_key().sign(&payload);
        Ok(Self {
            schema: HOME_RECORD_SCHEMA,
            bundle_id: bundle_id.to_owned(),
            created_at: created_at.to_owned(),
            created_by_generation: created_by_generation.to_owned(),
            signer: identity.principal_id(),
            signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Verify the record against `bundle_id` (the directory it sits in) and the
    /// node's verifying key.
    pub fn verify(&self, bundle_id: &str, node_key: &VerifyingKey) -> Result<()> {
        anyhow::ensure!(
            self.schema == HOME_RECORD_SCHEMA,
            "home record schema {} is not the supported schema {HOME_RECORD_SCHEMA}",
            self.schema
        );
        anyhow::ensure!(
            self.bundle_id == bundle_id,
            "home record names bundle `{}`, not `{bundle_id}`",
            self.bundle_id
        );
        let expected_signer = format!("fp:{}", lillux::sha256_hex(node_key.as_bytes()));
        anyhow::ensure!(
            self.signer == expected_signer,
            "home record was signed by `{}`, not this node (`{expected_signer}`)",
            self.signer
        );
        let signature = Signature::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(&self.signature)
                .context("home record signature is not base64")?,
        )
        .context("home record signature is malformed")?;
        node_key
            .verify(&self.signing_payload()?, &signature)
            .context("home record signature does not verify under this node's key")
    }
}

/// An opened namespace directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedStateHome {
    pub bundle_id: String,
    pub namespace: String,
    /// The namespace directory on the host.
    pub path: PathBuf,
}

/// Refuse unless the caller's own grant carries the `use` capability for the
/// namespace. `caller_scopes` must be the launch's caller scopes, never the
/// scopes the launch composed for the thread: the item may not lend itself
/// access to a home.
pub fn authorize_caller(
    caller_scopes: &[String],
    bundle_id: &str,
    namespace: &str,
    caller_principal: &str,
) -> Result<(), StateHomeRefusal> {
    let required = bundle_state_use_cap(bundle_id, namespace);
    if caller_scopes
        .iter()
        .any(|scope| ryeos_state::capability::pattern_covers(scope, &required))
    {
        return Ok(());
    }
    Err(StateHomeRefusal::NamespaceDenied {
        bundle_id: bundle_id.to_owned(),
        namespace: namespace.to_owned(),
        caller: caller_principal.to_owned(),
    })
}

fn unavailable(detail: impl std::fmt::Display) -> StateHomeRefusal {
    StateHomeRefusal::Unavailable {
        detail: detail.to_string(),
    }
}

fn read_record(
    bundle_dir: &lillux::PinnedDirectory,
    bundle_id: &str,
    node_key: &VerifyingKey,
) -> Result<Option<HomeRecord>> {
    let name = OsStr::new(HOME_RECORD_NAME);
    if bundle_dir.entry_no_follow(name)?.is_none() {
        return Ok(None);
    }
    let bytes = lillux::read_regular_file_bounded_no_follow(
        &bundle_dir.path().join(name),
        MAX_RECORD_BYTES as u64,
    )
    .context("read home record")?;
    let record: HomeRecord = serde_json::from_slice(&bytes).context("parse home record")?;
    record.verify(bundle_id, node_key)?;
    Ok(Some(record))
}

/// Open `bundle_id`'s `namespace`, creating the bundle home (with its signed
/// record) and the namespace directory when absent. Runs under an exclusive
/// lock on the home root so concurrent first launches create one home.
///
/// `generation` is recorded in a newly created home's record.
pub fn open_or_create(
    app_root: &Path,
    bundle_id: &str,
    namespace: &str,
    identity: &NodeIdentity,
    generation: &str,
) -> Result<OpenedStateHome, StateHomeRefusal> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)
        .map_err(|error| StateHomeRefusal::NamespaceInvalid {
            detail: format!("{error:#}"),
        })?;
    validate_state_home_segment("namespace", namespace)
        .map_err(|detail| StateHomeRefusal::NamespaceInvalid { detail })?;

    let state_dir = app_root.join(ryeos_engine::AI_DIR).join("state");
    let state = lillux::PinnedDirectory::open(&state_dir)
        .map_err(|error| unavailable(format!("open {}: {error:#}", state_dir.display())))?
        .ok_or_else(|| unavailable(format!("{} does not exist", state_dir.display())))?;
    let root = state
        .open_or_create_child(OsStr::new(BUNDLE_STATE_DIR), 0o700)
        .map_err(|error| unavailable(format!("open the bundle state root: {error:#}")))?;
    root.require_owner_private_directory()
        .map_err(|error| unavailable(format!("bundle state root: {error:#}")))?;
    let _lock = root
        .lock_exclusive()
        .map_err(|error| unavailable(format!("lock the bundle state root: {error:#}")))?;

    let bundle_name = OsStr::new(bundle_id);
    let bundle_dir = root
        .open_or_create_child(bundle_name, 0o700)
        .map_err(|error| unavailable(format!("open home of bundle `{bundle_id}`: {error:#}")))?;
    bundle_dir
        .require_owner_private_directory()
        .map_err(|error| unavailable(format!("home of bundle `{bundle_id}`: {error:#}")))?;

    let record = read_record(&bundle_dir, bundle_id, identity.verifying_key())
        .map_err(|error| unavailable(format!("home of bundle `{bundle_id}`: {error:#}")))?;
    if record.is_none() {
        // A home with content but no record is not one this node created.
        let existing = bundle_dir
            .entry_names()
            .map_err(|error| unavailable(format!("list home of `{bundle_id}`: {error:#}")))?;
        if !existing.is_empty() {
            return Err(unavailable(format!(
                "home of bundle `{bundle_id}` has content but no {HOME_RECORD_NAME}; it is not a \
                 home this node created"
            )));
        }
        let created_at = lillux::time::iso8601_now();
        let record = HomeRecord::sign(identity, bundle_id, &created_at, generation)
            .map_err(|error| unavailable(format!("sign home record: {error:#}")))?;
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| unavailable(format!("encode home record: {error}")))?;
        lillux::atomic_write_private(&bundle_dir.path().join(HOME_RECORD_NAME), &bytes)
            .map_err(|error| unavailable(format!("write home record: {error:?}")))?;
    }

    let namespace_dir = bundle_dir
        .open_or_create_child(OsStr::new(namespace), 0o700)
        .map_err(|error| {
            unavailable(format!(
                "open namespace `{namespace}` of bundle `{bundle_id}`: {error:#}"
            ))
        })?;
    namespace_dir.require_owner_private_directory().map_err(|error| {
        unavailable(format!(
            "namespace `{namespace}` of bundle `{bundle_id}`: {error:#}"
        ))
    })?;
    bundle_dir
        .sync()
        .and_then(|()| root.sync())
        .map_err(|error| unavailable(format!("sync bundle state home: {error:#}")))?;
    Ok(OpenedStateHome {
        bundle_id: bundle_id.to_owned(),
        namespace: namespace.to_owned(),
        path: namespace_dir.path().to_path_buf(),
    })
}

/// How a home's signed record stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordState {
    Verified,
    Missing,
    /// Present but not verifying under this node's key (foreign or tampered),
    /// with the reason.
    Invalid(String),
}

/// One namespace directory as the doctor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceInventory {
    pub name: String,
    /// Regular-file bytes, when the walk succeeded.
    pub bytes: Option<u64>,
    /// Problems with this namespace: an invalid name, permissions wider than
    /// 0700, or an unreadable tree.
    pub problems: Vec<String>,
}

/// One bundle's home as the doctor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeInventory {
    pub bundle_id: String,
    pub record: RecordState,
    pub namespaces: Vec<NamespaceInventory>,
    /// Problems with the bundle directory itself.
    pub problems: Vec<String>,
}

impl HomeInventory {
    pub fn total_bytes(&self) -> u64 {
        self.namespaces
            .iter()
            .filter_map(|namespace| namespace.bytes)
            .sum()
    }

    pub fn is_sound(&self) -> bool {
        self.record == RecordState::Verified
            && self.problems.is_empty()
            && self.namespaces.iter().all(|ns| ns.problems.is_empty())
    }
}

/// Inventory every bundle home under `app_root`. An absent home root is an
/// empty inventory. Read-only: this never creates or repairs anything.
pub fn inventory(app_root: &Path, node_key: &VerifyingKey) -> Result<Vec<HomeInventory>> {
    let Some(root) = lillux::PinnedDirectory::open(&root_path(app_root))
        .context("open the bundle state root")?
    else {
        return Ok(Vec::new());
    };
    let mut names = root.entry_names().context("list the bundle state root")?;
    names.sort();
    let mut homes = Vec::new();
    for name in names {
        let bundle_id = name.to_string_lossy().into_owned();
        let mut problems = Vec::new();
        if ryeos_state::objects::validate_bundle_identifier("bundle_id", &bundle_id).is_err() {
            problems.push(format!("`{bundle_id}` is not a valid bundle id"));
        }
        let Some(bundle_dir) = root
            .open_child_directory(&name)
            .with_context(|| format!("open home of `{bundle_id}`"))?
        else {
            problems.push(format!("`{bundle_id}` is not a directory"));
            homes.push(HomeInventory {
                bundle_id,
                record: RecordState::Missing,
                namespaces: Vec::new(),
                problems,
            });
            continue;
        };
        if let Err(error) = bundle_dir.require_owner_private_directory() {
            problems.push(format!("home directory is not mode 0700: {error:#}"));
        }
        let record = match read_record(&bundle_dir, &bundle_id, node_key) {
            Ok(Some(_)) => RecordState::Verified,
            Ok(None) => RecordState::Missing,
            Err(error) => RecordState::Invalid(format!("{error:#}")),
        };
        let mut namespace_names = bundle_dir
            .entry_names()
            .with_context(|| format!("list home of `{bundle_id}`"))?
            .into_iter()
            .filter(|entry| entry != OsStr::new(HOME_RECORD_NAME))
            .collect::<Vec<_>>();
        namespace_names.sort();
        let mut namespaces = Vec::new();
        for namespace in namespace_names {
            let name = namespace.to_string_lossy().into_owned();
            let mut ns_problems = Vec::new();
            if validate_state_home_segment("namespace", &name).is_err() {
                ns_problems.push(format!("`{name}` is not a valid namespace name"));
            }
            let mut bytes = None;
            match bundle_dir.open_child_directory(&namespace) {
                Ok(Some(directory)) => {
                    match directory
                        .require_owner_enclosed_tree_bounded(INVENTORY_BUDGET, INVENTORY_MAX_BYTES)
                    {
                        Ok(total) => bytes = Some(total),
                        Err(error) => ns_problems.push(format!(
                            "namespace is not an owner-private tree: {error:#}"
                        )),
                    }
                }
                Ok(None) => ns_problems.push(format!("`{name}` is not a directory")),
                Err(error) => ns_problems.push(format!("cannot open namespace: {error:#}")),
            }
            namespaces.push(NamespaceInventory {
                name,
                bytes,
                problems: ns_problems,
            });
        }
        homes.push(HomeInventory {
            bundle_id,
            record,
            namespaces,
            problems,
        });
    }
    Ok(homes)
}

/// The bundle ids in `homes` that `installed` does not contain: homes kept
/// after their bundle was removed.
pub fn orphaned<'a>(
    homes: &'a [HomeInventory],
    installed: &BTreeSet<String>,
) -> Vec<&'a HomeInventory> {
    homes
        .iter()
        .filter(|home| !installed.contains(&home.bundle_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(dir: &Path) -> NodeIdentity {
        NodeIdentity::create(&dir.join("key.pem")).unwrap()
    }

    fn app_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(ryeos_engine::AI_DIR).join("state");
        std::fs::create_dir_all(&state).unwrap();
        dir
    }

    #[test]
    fn home_created_0700_with_signed_record() {
        let app = app_root();
        let keys = tempfile::tempdir().unwrap();
        let id = identity(keys.path());
        let opened = open_or_create(app.path(), "central-auth", "realm-a", &id, "gen-1").unwrap();
        assert_eq!(
            opened.path.file_name().and_then(OsStr::to_str),
            Some("realm-a")
        );
        let bundle_dir = lillux::PinnedDirectory::open(&root_path(app.path()).join("central-auth"))
            .unwrap()
            .unwrap();
        // Every level is owner-private (exactly 0700).
        for dir in [
            lillux::PinnedDirectory::open(&root_path(app.path()))
                .unwrap()
                .unwrap(),
            lillux::PinnedDirectory::open(&root_path(app.path()).join("central-auth"))
                .unwrap()
                .unwrap(),
            lillux::PinnedDirectory::open(&opened.path).unwrap().unwrap(),
        ] {
            dir.require_owner_private_directory().unwrap();
        }
        let record = read_record(&bundle_dir, "central-auth", id.verifying_key())
            .unwrap()
            .unwrap();
        assert_eq!(record.created_by_generation, "gen-1");
        assert_eq!(record.signer, id.principal_id());
        // A second namespace and a second open reuse the one record.
        let again = open_or_create(app.path(), "central-auth", "realm-b", &id, "gen-2").unwrap();
        assert_ne!(again.path, opened.path);
        let kept = read_record(&bundle_dir, "central-auth", id.verifying_key())
            .unwrap()
            .unwrap();
        assert_eq!(kept, record);
        let homes = inventory(app.path(), id.verifying_key()).unwrap();
        assert_eq!(homes.len(), 1);
        assert!(homes[0].is_sound(), "{homes:?}");
        assert_eq!(
            homes[0]
                .namespaces
                .iter()
                .map(|ns| ns.name.as_str())
                .collect::<Vec<_>>(),
            ["realm-a", "realm-b"]
        );
    }

    #[test]
    fn tampered_home_record_is_refused() {
        let app = app_root();
        let keys = tempfile::tempdir().unwrap();
        let id = identity(keys.path());
        open_or_create(app.path(), "central-auth", "realm-a", &id, "gen-1").unwrap();
        let record_path = root_path(app.path())
            .join("central-auth")
            .join(HOME_RECORD_NAME);

        // A record rewritten for another bundle fails to verify.
        let original = std::fs::read_to_string(&record_path).unwrap();
        std::fs::write(&record_path, original.replace("gen-1", "gen-evil")).unwrap();
        let refusal = open_or_create(app.path(), "central-auth", "realm-a", &id, "gen-3")
            .unwrap_err();
        assert_eq!(refusal.code(), "bundle_state_home_unavailable");
        let homes = inventory(app.path(), id.verifying_key()).unwrap();
        assert!(matches!(homes[0].record, RecordState::Invalid(_)));
        assert!(!homes[0].is_sound());

        // A record signed by a different node is foreign.
        let other_keys = tempfile::tempdir().unwrap();
        let other = identity(other_keys.path());
        let foreign =
            HomeRecord::sign(&other, "central-auth", "2026-01-01T00:00:00Z", "gen-1").unwrap();
        std::fs::write(&record_path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert!(
            open_or_create(app.path(), "central-auth", "realm-a", &id, "gen-3").is_err(),
            "a foreign record must refuse the open"
        );

        // A missing record over existing content is not a home this node made.
        std::fs::remove_file(&record_path).unwrap();
        assert!(open_or_create(app.path(), "central-auth", "realm-a", &id, "gen-3").is_err());
        let homes = inventory(app.path(), id.verifying_key()).unwrap();
        assert_eq!(homes[0].record, RecordState::Missing);
    }

    #[test]
    fn caller_without_use_cap_is_denied_with_fix() {
        let granted = vec!["ryeos.use.bundle-state.central-auth/realm-a".to_owned()];
        authorize_caller(&granted, "central-auth", "realm-a", "fp:caller").unwrap();
        let wildcard = vec!["ryeos.use.bundle-state.central-auth/*".to_owned()];
        authorize_caller(&wildcard, "central-auth", "realm-b", "fp:caller").unwrap();
        authorize_caller(&["*".to_owned()], "central-auth", "realm-b", "fp:operator").unwrap();

        let denied = authorize_caller(&granted, "central-auth", "realm-b", "fp:caller")
            .unwrap_err();
        assert_eq!(denied.code(), "bundle_state_namespace_denied");
        let message = denied.to_string();
        assert!(
            message.contains("ryeos.use.bundle-state.central-auth/realm-b")
                && message.contains("authorize-key")
                && message.contains("fp:caller"),
            "{message}"
        );
        // Another bundle's grant does not carry over, and neither does an
        // execute grant for the bundle's tools.
        let other = vec![
            "ryeos.use.bundle-state.other/realm-b".to_owned(),
            "ryeos.execute.tool.central-auth/*".to_owned(),
        ];
        assert!(authorize_caller(&other, "central-auth", "realm-b", "fp:caller").is_err());
        assert!(authorize_caller(&[], "central-auth", "realm-b", "fp:caller").is_err());
    }

    #[test]
    fn namespace_from_param_must_be_a_segment() {
        let app = app_root();
        let keys = tempfile::tempdir().unwrap();
        let id = identity(keys.path());
        for bad in ["", "..", "a/b", "A", "-x", ".hidden", "a\0b"] {
            let refusal = open_or_create(app.path(), "central-auth", bad, &id, "gen-1")
                .unwrap_err();
            assert_eq!(refusal.code(), "bundle_state_namespace_invalid", "{bad:?}");
        }
        let refusal = open_or_create(app.path(), "../escape", "ok", &id, "gen-1").unwrap_err();
        assert_eq!(refusal.code(), "bundle_state_namespace_invalid");
        assert!(!root_path(app.path()).join("..").join("escape").exists());
    }

    #[test]
    fn orphaned_lists_homes_without_an_installed_bundle() {
        let app = app_root();
        let keys = tempfile::tempdir().unwrap();
        let id = identity(keys.path());
        open_or_create(app.path(), "kept", "a", &id, "g").unwrap();
        open_or_create(app.path(), "gone", "a", &id, "g").unwrap();
        let homes = inventory(app.path(), id.verifying_key()).unwrap();
        let installed = BTreeSet::from(["kept".to_owned()]);
        let orphans = orphaned(&homes, &installed);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].bundle_id, "gone");
    }
}
