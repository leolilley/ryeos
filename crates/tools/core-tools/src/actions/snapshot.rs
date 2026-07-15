//! Project snapshot ergonomics — local/offline status, log, create, and show.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use lillux::cas::{sha256_hex, CasStore};
use lillux::crypto::Verifier as _;
use ryeos_state::ignore::IgnoreMatcher;
use ryeos_state::objects::{ItemSource, ProjectSnapshot, SourceManifest};
use ryeos_state::project_sync::ProjectSyncScope;
use ryeos_state::refs::{self, ProjectHeadLock, TrustStore as StateTrustStore};
use ryeos_state::signer::Signer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStatusParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default)]
    pub include_unchanged: bool,
    /// Worktree scan budget in milliseconds. `0` disables the cap.
    /// When the budget is exhausted the report is returned with
    /// partial results instead of blocking the caller.
    #[serde(default = "default_status_budget_ms")]
    pub time_budget_ms: u64,
}

fn default_status_budget_ms() -> u64 {
    0
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotLogParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default = "default_limit", deserialize_with = "deserialize_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub allow_empty: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotShowParams {
    pub snapshot_hash: String,
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotStatusReport {
    pub kind: &'static str,
    pub project_path: String,
    pub project_hash: String,
    pub principal_key: String,
    pub baseline: &'static str,
    pub head_snapshot_hash: Option<String>,
    pub deployed_snapshot_hash: Option<String>,
    pub dirty: bool,
    /// False when the worktree scan hit its time budget. Partial
    /// reports only cover scanned files: added/modified counts are
    /// lower bounds and deletions were not evaluated at all.
    pub scan_complete: bool,
    pub scan_elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub counts: ChangeCounts,
    pub changes: Vec<PathChange>,
}

#[derive(Debug, Default, Serialize)]
pub struct ChangeCounts {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

#[derive(Debug, Serialize)]
pub struct PathChange {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_item_source_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_integrity: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotLogReport {
    pub kind: &'static str,
    pub project_path: String,
    pub project_hash: String,
    pub principal_key: String,
    pub head_snapshot_hash: Option<String>,
    pub entries: Vec<SnapshotLogEntry>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotLogEntry {
    pub snapshot_hash: String,
    pub created_at: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub project_manifest_hash: String,
    pub parent_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotCreateReport {
    pub kind: &'static str,
    pub project_path: String,
    pub project_hash: String,
    pub principal_key: String,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_snapshot_hash: Option<String>,
    pub parent_hashes: Vec<String>,
    pub manifest_hash: String,
    pub manifest_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotShowReport {
    pub kind: &'static str,
    pub snapshot_hash: String,
    pub created_at: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub project_manifest_hash: String,
    pub project_sync_scope: ProjectSyncScope,
    pub parent_hashes: Vec<String>,
    pub manifest_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_principal_head: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_deployed: Option<bool>,
}

pub fn run_status(params: SnapshotStatusParams) -> Result<SnapshotStatusReport> {
    let started = Instant::now();
    let deadline =
        (params.time_budget_ms > 0).then(|| started + Duration::from_millis(params.time_budget_ms));

    let ctx = SnapshotContext::for_project(&params.project_path)?;
    let head_hash = refs::read_verified_project_head_ref(
        &ctx.refs_root,
        &ctx.principal_key,
        &ctx.project_hash,
        &ctx.ref_trust_store,
    )?;
    let deployed_hash = refs::read_verified_deployed_project_ref(
        &ctx.refs_root,
        &ctx.project_hash,
        &ctx.ref_trust_store,
    )?
    .map(|r| r.target_hash);

    let head_manifest = head_hash
        .as_deref()
        .map(|hash| load_snapshot_and_manifest(&ctx.cas, hash))
        .transpose()?
        .map(|(_, manifest)| manifest);
    let head_items = match head_manifest.as_ref() {
        Some(manifest) => manifest.item_source_hashes.clone(),
        None => HashMap::new(),
    };
    let head_state = manifest_state_map(&ctx.cas, &head_items)?;
    let scan = build_worktree_state_map(&ctx.project_path, &ctx.ignore, deadline)?;
    let worktree = scan.states;

    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(worktree.keys().cloned());
    // Head-only paths surface as deletions; with a truncated scan a
    // not-yet-visited file is indistinguishable from a deleted one,
    // so deletions are only evaluated on a complete scan.
    if scan.complete {
        paths.extend(head_items.keys().cloned());
    }

    let mut counts = ChangeCounts::default();
    let mut changes = Vec::new();
    for path in paths {
        let head = head_state.get(&path);
        let work = worktree.get(&path);
        let (status, include) = match (head, work) {
            (None, Some(_)) => {
                counts.added += 1;
                ("added", true)
            }
            (Some(_), None) => {
                counts.deleted += 1;
                ("deleted", true)
            }
            (Some(h), Some(w)) if h != w => {
                counts.modified += 1;
                ("modified", true)
            }
            (Some(_), Some(_)) => {
                counts.unchanged += 1;
                ("unchanged", params.include_unchanged)
            }
            (None, None) => continue,
        };
        if include {
            changes.push(PathChange {
                path: path.clone(),
                status: status.to_string(),
                head_item_source_hash: head_items.get(&path).cloned(),
                worktree_integrity: work.map(|state| state.integrity.clone()),
            });
        }
    }

    let note = (!scan.complete).then(|| {
        format!(
            "worktree scan exceeded its {}ms budget after {} file(s); \
             added/modified counts are partial and deletions were not evaluated. \
             Re-run with params {{\"time_budget_ms\": 0}} for a full scan.",
            params.time_budget_ms,
            worktree.len(),
        )
    });

    Ok(SnapshotStatusReport {
        kind: "snapshot_status",
        project_path: ctx.project_path.display().to_string(),
        project_hash: ctx.project_hash,
        principal_key: ctx.principal_key,
        baseline: "principal_head",
        head_snapshot_hash: head_hash,
        deployed_snapshot_hash: deployed_hash,
        dirty: counts.added > 0 || counts.modified > 0 || counts.deleted > 0,
        scan_complete: scan.complete,
        scan_elapsed_ms: started.elapsed().as_millis() as u64,
        note,
        counts,
        changes,
    })
}

pub fn run_log(params: SnapshotLogParams) -> Result<SnapshotLogReport> {
    let ctx = SnapshotContext::for_project(&params.project_path)?;
    let head_hash = refs::read_verified_project_head_ref(
        &ctx.refs_root,
        &ctx.principal_key,
        &ctx.project_hash,
        &ctx.ref_trust_store,
    )?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut current = head_hash.clone();
    let limit = params.limit.max(1);

    while let Some(hash) = current.take() {
        if entries.len() >= limit || !seen.insert(hash.clone()) {
            break;
        }
        let snapshot = load_snapshot(&ctx.cas, &hash)?;
        current = snapshot.parent_hashes.first().cloned();
        entries.push(SnapshotLogEntry {
            snapshot_hash: hash,
            created_at: snapshot.created_at,
            source: snapshot.source,
            message: snapshot.message,
            project_manifest_hash: snapshot.project_manifest_hash,
            parent_hashes: snapshot.parent_hashes,
        });
    }

    Ok(SnapshotLogReport {
        kind: "snapshot_log",
        project_path: ctx.project_path.display().to_string(),
        project_hash: ctx.project_hash,
        principal_key: ctx.principal_key,
        head_snapshot_hash: head_hash,
        entries,
    })
}

pub fn run_create(params: SnapshotCreateParams) -> Result<SnapshotCreateReport> {
    let ctx = SnapshotContext::for_project(&params.project_path)?;
    let initial_head = refs::read_verified_project_head_ref(
        &ctx.refs_root,
        &ctx.principal_key,
        &ctx.project_hash,
        &ctx.ref_trust_store,
    )?;
    let current_snapshot = initial_head
        .as_deref()
        .map(|hash| load_snapshot(&ctx.cas, hash))
        .transpose()?;

    let manifest = build_manifest_into_cas(&ctx.cas, &ctx.project_path, &ctx.ignore)?;
    let manifest_hash = ctx.cas.store_object(&manifest.to_value())?;
    let manifest_entries = manifest.item_source_hashes.len();

    if !params.allow_empty {
        if let Some(ref snapshot) = current_snapshot {
            if snapshot.project_manifest_hash == manifest_hash {
                return Ok(SnapshotCreateReport {
                    kind: "snapshot_create",
                    project_path: ctx.project_path.display().to_string(),
                    project_hash: ctx.project_hash,
                    principal_key: ctx.principal_key,
                    created: false,
                    reason: Some("clean".to_string()),
                    snapshot_hash: None,
                    head_snapshot_hash: initial_head,
                    parent_hashes: snapshot.parent_hashes.clone(),
                    manifest_hash,
                    manifest_entries,
                    message: params.message,
                });
            }
        }
    }

    let parent_hashes = initial_head.iter().cloned().collect::<Vec<_>>();
    let snapshot = ProjectSnapshot {
        project_manifest_hash: manifest_hash.clone(),
        user_manifest_hash: None,
        message: params.message.clone(),
        project_sync_scope: current_snapshot
            .as_ref()
            .map(|snapshot| snapshot.project_sync_scope)
            .unwrap_or(ProjectSyncScope::FullProject),
        parent_hashes: parent_hashes.clone(),
        created_at: lillux::time::iso8601_now(),
        source: "snapshot_create".to_string(),
    };
    let snapshot_hash = ctx.cas.store_object(&snapshot.to_value())?;

    let node_signer = NodeFileSigner::load(&ctx.app_root)?;
    let trusted_node_key = ctx
        .ref_trust_store
        .get(node_signer.fingerprint())
        .ok_or_else(|| anyhow!("node signing key is not the pinned node ref authority"))?;
    if trusted_node_key != &node_signer.verifying_key() {
        bail!("node signing key does not match the pinned node ref authority");
    }
    let _lock = ProjectHeadLock::acquire(&ctx.refs_root, &ctx.principal_key, &ctx.project_hash)?;
    let locked_head = refs::read_verified_project_head_ref(
        &ctx.refs_root,
        &ctx.principal_key,
        &ctx.project_hash,
        &ctx.ref_trust_store,
    )?;
    if locked_head != initial_head {
        bail!(
            "project head changed while creating snapshot; expected {:?}, got {:?}. Rerun `ryeos snapshot create`.",
            initial_head,
            locked_head
        );
    }
    let head_outcome = match initial_head.as_deref() {
        Some(current) => refs::advance_verified_project_head_ref(
            &ctx.refs_root,
            &ctx.principal_key,
            &ctx.project_hash,
            &snapshot_hash,
            current,
            &ctx.ref_trust_store,
            &node_signer,
        ),
        None => refs::write_project_head_ref(
            &ctx.refs_root,
            &ctx.principal_key,
            &ctx.project_hash,
            &snapshot_hash,
            &node_signer,
        ),
    };
    if let refs::RefWriteOutcome::DurabilityUncertain(error) =
        refs::classify_ref_write(head_outcome, "publishing project snapshot head")?
    {
        tracing::warn!(%error, "project snapshot head committed with uncertain durability");
    }

    Ok(SnapshotCreateReport {
        kind: "snapshot_create",
        project_path: ctx.project_path.display().to_string(),
        project_hash: ctx.project_hash,
        principal_key: ctx.principal_key,
        created: true,
        reason: None,
        snapshot_hash: Some(snapshot_hash.clone()),
        head_snapshot_hash: Some(snapshot_hash),
        parent_hashes,
        manifest_hash,
        manifest_entries,
        message: params.message,
    })
}

pub fn run_show(params: SnapshotShowParams) -> Result<SnapshotShowReport> {
    let app_root = app_root()?;
    let cas = CasStore::new(runtime_state_dir(&app_root).join("objects"));
    let refs_root = runtime_state_dir(&app_root).join("refs");
    let snapshot = load_snapshot(&cas, &params.snapshot_hash)?;
    let manifest = load_manifest(&cas, &snapshot.project_manifest_hash)?;

    let (is_principal_head, is_deployed) = if let Some(project_path) = params.project_path {
        let ref_trust_store = load_node_ref_trust_store(&app_root)?;
        let canonical = canonical_project_path(&project_path)?;
        let project_hash = refs::deployed_project_key(&canonical.display().to_string());
        let principal_key = operator_principal_key()?;
        let principal_head = refs::read_verified_project_head_ref(
            &refs_root,
            &principal_key,
            &project_hash,
            &ref_trust_store,
        )?;
        let deployed = refs::read_verified_deployed_project_ref(
            &refs_root,
            &project_hash,
            &ref_trust_store,
        )?;
        (
            Some(principal_head.as_deref() == Some(params.snapshot_hash.as_str())),
            Some(
                deployed
                    .as_ref()
                    .is_some_and(|r| r.target_hash == params.snapshot_hash),
            ),
        )
    } else {
        (None, None)
    };

    Ok(SnapshotShowReport {
        kind: "snapshot_show",
        snapshot_hash: params.snapshot_hash,
        created_at: snapshot.created_at,
        source: snapshot.source,
        message: snapshot.message,
        project_manifest_hash: snapshot.project_manifest_hash,
        project_sync_scope: snapshot.project_sync_scope,
        parent_hashes: snapshot.parent_hashes,
        manifest_entries: manifest.item_source_hashes.len(),
        is_principal_head,
        is_deployed,
    })
}

fn default_limit() -> usize {
    20
}

fn deserialize_limit<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| serde::de::Error::custom("limit must be a non-negative integer")),
        serde_json::Value::String(s) => s
            .parse::<usize>()
            .map_err(|_| serde::de::Error::custom("limit must be a non-negative integer")),
        other => Err(serde::de::Error::custom(format!(
            "limit must be an integer or integer string, got {other}"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    integrity: String,
    mode: Option<u32>,
}

struct SnapshotContext {
    app_root: PathBuf,
    project_path: PathBuf,
    project_hash: String,
    principal_key: String,
    cas: CasStore,
    refs_root: PathBuf,
    ref_trust_store: StateTrustStore,
    ignore: IgnoreMatcher,
}

impl SnapshotContext {
    fn for_project(project_path: &Path) -> Result<Self> {
        let app_root = app_root()?;
        let project_path = canonical_project_path(project_path)?;
        let project_hash = refs::deployed_project_key(&project_path.display().to_string());
        let principal_key = operator_principal_key()?;
        let runtime_state_dir = runtime_state_dir(&app_root);
        let cas = CasStore::new(runtime_state_dir.join("objects"));
        let refs_root = runtime_state_dir.join("refs");
        let ref_trust_store = load_node_ref_trust_store(&app_root)?;
        fs::create_dir_all(cas.root()).context("create CAS root")?;
        fs::create_dir_all(&refs_root).context("create refs root")?;
        let ignore = load_ignore(&app_root);
        Ok(Self {
            app_root,
            project_path,
            project_hash,
            principal_key,
            cas,
            refs_root,
            ref_trust_store,
            ignore,
        })
    }
}

struct NodeFileSigner {
    fingerprint: String,
    signing_key: lillux::crypto::SigningKey,
}

impl NodeFileSigner {
    fn load(app_root: &Path) -> Result<Self> {
        let key_path = app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("identity")
            .join("private_key.pem");
        let signing_key = lillux::crypto::load_signing_key(&key_path)
            .with_context(|| format!("load node identity key {}", key_path.display()))?;
        let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
        Ok(Self {
            fingerprint,
            signing_key,
        })
    }
}

impl Signer for NodeFileSigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as _;
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        self.signing_key.verifying_key()
    }
}

fn app_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("RYEOS_APP_ROOT") {
        return Ok(PathBuf::from(p));
    }
    dirs::data_dir()
        .map(|d| d.join("ryeos"))
        .ok_or_else(|| anyhow!("could not determine app rootectory"))
}

fn runtime_state_dir(app_root: &Path) -> PathBuf {
    app_root.join(ryeos_engine::AI_DIR).join("state")
}

/// Load the node's public verification authority for node-owned mutable refs.
///
/// The public identity selects the local node from the operator trust tier; it
/// is not itself treated as an unanchored trust source. The returned state
/// trust store deliberately contains only that node key, so another trusted
/// item publisher cannot become an authority for project or deployed refs.
fn load_node_ref_trust_store(app_root: &Path) -> Result<StateTrustStore> {
    let runtime_root = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf());
    let operator_trust = ryeos_engine::trust::TrustStore::load(None, &runtime_root.config())
        .context("load operator trust store for node-owned state refs")?;
    let identity_path = runtime_root
        .node()
        .join("identity")
        .join("public-identity.json");
    let identity = ryeos_app::identity::NodeIdentity::load_public_identity(&identity_path)?;
    if identity.kind != "identity/v1" {
        bail!(
            "unsupported node public identity kind `{}` at {}",
            identity.kind,
            identity_path.display()
        );
    }
    let fingerprint = identity
        .principal_id
        .strip_prefix("fp:")
        .ok_or_else(|| {
            anyhow!(
                "node public identity principal_id must start with `fp:` at {}",
                identity_path.display()
            )
        })?
        .to_string();
    if identity.signature.signer != identity.principal_id {
        bail!(
            "node public identity signer mismatch at {}: expected {}, got {}",
            identity_path.display(),
            identity.principal_id,
            identity.signature.signer
        );
    }

    let encoded_key = identity.signing_key.strip_prefix("ed25519:").ok_or_else(|| {
        anyhow!(
            "node public identity signing_key must start with `ed25519:` at {}",
            identity_path.display()
        )
    })?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded_key)
        .context("decode node public identity signing key")?;
    let key_bytes: [u8; 32] = key_bytes.try_into().map_err(|_| {
        anyhow!(
            "node public identity signing key must contain exactly 32 bytes at {}",
            identity_path.display()
        )
    })?;
    let identity_key = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)
        .context("parse node public identity signing key")?;
    let actual_fingerprint = lillux::crypto::fingerprint(&identity_key);
    if actual_fingerprint != fingerprint {
        bail!(
            "node public identity fingerprint mismatch at {}: declared {}, computed {}",
            identity_path.display(),
            fingerprint,
            actual_fingerprint
        );
    }

    let trusted = operator_trust.get(&fingerprint).ok_or_else(|| {
        anyhow!(
            "node public identity {} is not pinned in the operator trust store",
            identity.principal_id
        )
    })?;
    if trusted.verifying_key != identity_key {
        bail!(
            "node public identity key does not match the operator trust pin for {}",
            identity.principal_id
        );
    }

    let unsigned = serde_json::json!({
        "kind": identity.kind,
        "principal_id": identity.principal_id,
        "signing_key": identity.signing_key,
        "created_at": identity.created_at,
    });
    let payload = serde_json::to_vec(&unsigned).context("encode node public identity payload")?;
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&identity.signature.sig)
        .context("decode node public identity signature")?;
    let signature = lillux::crypto::Signature::from_slice(&signature_bytes)
        .context("parse node public identity signature")?;
    identity_key
        .verify(&payload, &signature)
        .context("verify node public identity signature")?;

    let mut state_trust = StateTrustStore::new();
    state_trust.insert(fingerprint, identity_key);
    Ok(state_trust)
}

fn canonical_project_path(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize project path {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("project path is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn operator_principal_key() -> Result<String> {
    // This private key is used only to select the local operator's
    // principal-scoped project namespace. Ref signature authority is loaded
    // independently from the node's pinned public identity above.
    let key_path = ryeos_engine::roots::runtime_root()
        .map_err(|e| anyhow!("resolve app root: {e}"))?
        .operator_signing_key_path();
    let signing_key = lillux::crypto::load_signing_key(&key_path)
        .with_context(|| format!("load operator signing key {}", key_path.display()))?;
    Ok(lillux::crypto::fingerprint(&signing_key.verifying_key()))
}

fn load_ignore(app_root: &Path) -> IgnoreMatcher {
    ryeos_app::ignore::load_from_app_root(app_root)
        .unwrap_or_else(|_| ryeos_state::ignore::matcher_from_builtins())
}

fn load_snapshot(cas: &CasStore, hash: &str) -> Result<ProjectSnapshot> {
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow!("snapshot {hash} not found in local CAS"))?;
    ProjectSnapshot::from_value(&value)
}

fn load_manifest(cas: &CasStore, hash: &str) -> Result<SourceManifest> {
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow!("manifest {hash} not found in local CAS"))?;
    SourceManifest::from_value(&value)
}

fn load_snapshot_and_manifest(
    cas: &CasStore,
    snapshot_hash: &str,
) -> Result<(ProjectSnapshot, SourceManifest)> {
    let snapshot = load_snapshot(cas, snapshot_hash)?;
    let manifest = load_manifest(cas, &snapshot.project_manifest_hash)?;
    Ok((snapshot, manifest))
}

fn manifest_state_map(
    cas: &CasStore,
    items: &HashMap<String, String>,
) -> Result<HashMap<String, FileState>> {
    let mut out = HashMap::new();
    for (path, item_hash) in items {
        let value = cas
            .get_object(item_hash)?
            .ok_or_else(|| anyhow!("item source {item_hash} for {path} not found in local CAS"))?;
        let item = ItemSource::from_value(&value)?;
        out.insert(
            path.clone(),
            FileState {
                integrity: item.integrity,
                mode: item.mode,
            },
        );
    }
    Ok(out)
}

struct WorktreeScan {
    states: HashMap<String, FileState>,
    /// False when the walk was cut short by `deadline`.
    complete: bool,
}

fn build_worktree_state_map(
    root: &Path,
    ignore: &IgnoreMatcher,
    deadline: Option<Instant>,
) -> Result<WorktreeScan> {
    let mut states = HashMap::new();
    let outcome = walk_project_files(root, root, ignore, &mut |rel, path| {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(ControlFlow::Break(()));
        }
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        states.insert(
            rel.to_string(),
            FileState {
                integrity: sha256_hex(&bytes),
                mode: unix_mode(path),
            },
        );
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(WorktreeScan {
        states,
        complete: outcome.is_continue(),
    })
}

fn build_manifest_into_cas(
    cas: &CasStore,
    root: &Path,
    ignore: &IgnoreMatcher,
) -> Result<SourceManifest> {
    let mut items = BTreeMap::new();
    // This closure never breaks, so the walk outcome carries no info.
    let _ = walk_project_files(root, root, ignore, &mut |rel, path| {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let blob_hash = cas.store_blob(&bytes)?;
        let item = ItemSource {
            item_ref: rel.to_string(),
            content_blob_hash: blob_hash,
            integrity: sha256_hex(&bytes),
            signature_info: None,
            mode: unix_mode(path),
        };
        let item_hash = cas.store_object(&item.to_value())?;
        items.insert(rel.to_string(), item_hash);
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(SourceManifest {
        item_source_hashes: items.into_iter().collect(),
    })
}

fn walk_project_files(
    root: &Path,
    dir: &Path,
    ignore: &IgnoreMatcher,
    f: &mut impl FnMut(&str, &Path) -> Result<ControlFlow<()>>,
) -> Result<ControlFlow<()>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read directory {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "state" || rel.starts_with("state/") || ignore.is_ignored(&rel) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if walk_project_files(root, &path, ignore, f)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        } else if file_type.is_file() && f(&rel, &path)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

#[cfg(unix)]
fn unix_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).ok().and_then(|meta| {
        let mode = meta.permissions().mode() & 0o7777;
        (mode & 0o111 != 0).then_some(mode)
    })
}

#[cfg(not(unix))]
fn unix_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (rel, contents) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(path, contents).expect("write");
        }
        dir
    }

    #[test]
    fn worktree_scan_without_deadline_is_complete() {
        let dir = temp_project(&[("a.txt", "a"), ("sub/b.txt", "b")]);
        let ignore = ryeos_state::ignore::matcher_from_builtins();
        let scan = build_worktree_state_map(dir.path(), &ignore, None).expect("scan");
        assert!(scan.complete);
        assert_eq!(scan.states.len(), 2);
    }

    #[test]
    fn worktree_scan_with_expired_deadline_reports_incomplete() {
        let dir = temp_project(&[("a.txt", "a"), ("sub/b.txt", "b")]);
        let ignore = ryeos_state::ignore::matcher_from_builtins();
        let expired = Instant::now() - Duration::from_millis(1);
        let scan = build_worktree_state_map(dir.path(), &ignore, Some(expired)).expect("scan");
        assert!(!scan.complete);
        // Nothing hashed: the deadline check precedes every file read.
        assert!(scan.states.is_empty());
    }

    #[test]
    fn status_params_default_budget_is_uncapped() {
        let params: SnapshotStatusParams =
            serde_json::from_value(serde_json::json!({ "project_path": "/tmp/x" }))
                .expect("params");
        assert_eq!(params.time_budget_ms, 0);
        let params: SnapshotStatusParams = serde_json::from_value(
            serde_json::json!({ "project_path": "/tmp/x", "time_budget_ms": 5_000 }),
        )
        .expect("params");
        assert_eq!(params.time_budget_ms, 5_000);
    }
}
