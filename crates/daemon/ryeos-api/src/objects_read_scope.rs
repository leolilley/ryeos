//! Root-scoped CAS reads.
//!
//! Content addressing alone is not authority: knowing a hash must not entitle
//! a caller to the bytes behind it. Every read of the CAS over the wire names
//! a typed [`ReadRoot`] the caller is entitled to see. The server resolves
//! that root to the object hashes it is anchored at, walks the schema-defined
//! closure of those hashes, and serves only hashes inside the closure.
//!
//! A hash outside the closure is refused with the typed `object_not_in_scope`
//! code whether or not the object exists, so a read is never a presence
//! oracle. A hash inside the closure whose bytes are absent is reported
//! missing, which reveals nothing the caller could not already infer from the
//! root it is entitled to.
//!
//! Root kinds and who may name them:
//! - `project_head`: the caller's own head of one project (the head namespace
//!   is keyed by the authenticated caller, so a caller can only ever name its
//!   own namespace) and that head's history.
//! - `thread_result`: a thread the caller owns. Anchors are the thread's
//!   result project snapshot and its admitted launch capsule.
//! - `bundle_export`: an installed bundle's exported file blobs. Requires the
//!   bundle export capability.
//! - `placement`: a worker placement. Either a dedicated session owned by the
//!   caller (anchors: its candidate and evidence hashes), or a handoff
//!   operation retained on this node whose configured peer is the caller
//!   (anchors: every hash the retained operation record names).
//! - `catalog_publication`: the signed head of a bundle catalog this node
//!   publishes, with the publication history it links. Requires the capability
//!   that resolves catalog heads.
//! - `admitted_subject`: a subject this node holds an accepted admission
//!   attestation for. Requires the capability that already exposes those
//!   attestations; the admission is the node's deliberate publication of the
//!   subject, so the closure it covers is readable by whoever may list it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use serde_json::Value;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;

/// Stable machine code of a read refused by scope.
pub const OBJECT_NOT_IN_SCOPE: &str = "object_not_in_scope";

/// A typed root a CAS read is anchored at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadRoot {
    /// The caller's own head of `project_path`, with that head's history.
    ProjectHead { project_path: String },
    /// A thread the caller owns.
    ThreadResult { thread_id: String },
    /// An installed bundle's exported files.
    BundleExport { bundle_name: String },
    /// A worker placement: a dedicated session id or a handoff operation or
    /// preflight id retained on the serving node.
    Placement { placement_id: String },
    /// A subject this node has accepted an admission attestation for.
    AdmittedSubject { subject_hash: String },
    /// A bundle catalog namespace this node publishes.
    CatalogPublication { catalog_namespace: String },
}

impl ReadRoot {
    fn describe(&self) -> String {
        match self {
            Self::ProjectHead { project_path } => format!("project_head {project_path}"),
            Self::ThreadResult { thread_id } => format!("thread_result {thread_id}"),
            Self::BundleExport { bundle_name } => format!("bundle_export {bundle_name}"),
            Self::Placement { placement_id } => format!("placement {placement_id}"),
            Self::AdmittedSubject { subject_hash } => format!("admitted_subject {subject_hash}"),
            Self::CatalogPublication { catalog_namespace } => {
                format!("catalog_publication {catalog_namespace}")
            }
        }
    }
}

/// Everything readable under one root.
#[derive(Debug, Default)]
pub struct ReadScope {
    pub objects: BTreeSet<String>,
    pub blobs: BTreeSet<String>,
    pub large_objects: BTreeSet<String>,
}

/// Refuse a read that names no root or a hash outside the named root.
pub fn not_in_scope(detail: &str) -> HandlerError {
    HandlerError::Structured {
        code: OBJECT_NOT_IN_SCOPE.to_owned(),
        status: 403,
        body: serde_json::json!({
            "code": OBJECT_NOT_IN_SCOPE,
            "error": format!(
                "{detail}. Name a root you may read in the request `root` field \
                 (project_head, thread_result, bundle_export, placement or \
                 admitted_subject) that reaches the hash; the node operator reads \
                 unscoped through objects/get-any"
            ),
        }),
    }
}

/// The refusal for a request that names no root at all.
pub fn root_required(service: &str) -> HandlerError {
    not_in_scope(&format!("{service} requires a root and none was given"))
}

fn internal(error: impl std::fmt::Display) -> HandlerError {
    HandlerError::Internal(format!("{error:#}"))
}

/// What a root resolves to before its closure is walked.
struct ResolvedRoot {
    /// Object hashes the root is anchored at.
    objects: Vec<String>,
    /// Blob hashes the root names directly (bundle exports have no objects).
    blobs: BTreeSet<String>,
}

impl ResolvedRoot {
    fn objects(objects: Vec<String>) -> Self {
        Self {
            objects,
            blobs: BTreeSet::new(),
        }
    }
}

/// Resolve `root` for the caller and return the set of hashes readable under
/// it. Closures of complete immutable roots are cached.
pub fn resolve_scope(
    root: &ReadRoot,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<Arc<ReadScope>, HandlerError> {
    ctx.require_verified()
        .map_err(|_| not_in_scope("an unauthenticated caller has no readable root"))?;
    let resolved = match root {
        ReadRoot::ProjectHead { project_path } => resolve_project_head(project_path, ctx, state)?,
        ReadRoot::ThreadResult { thread_id } => resolve_thread_result(thread_id, ctx, state)?,
        ReadRoot::BundleExport { bundle_name } => resolve_bundle_export(bundle_name, ctx, state)?,
        ReadRoot::Placement { placement_id } => resolve_placement(placement_id, ctx, state)?,
        ReadRoot::AdmittedSubject { subject_hash } => {
            resolve_admitted_subject(subject_hash, ctx, state)?
        }
        ReadRoot::CatalogPublication { catalog_namespace } => {
            resolve_catalog_publication(catalog_namespace, ctx, state)?
        }
    };
    closure_scope(resolved, state)
}

/// Fail unless `object_hashes` are all readable objects under `scope`.
pub fn require_objects_in_scope(
    scope: &ReadScope,
    root: &ReadRoot,
    object_hashes: &[String],
) -> Result<(), HandlerError> {
    for hash in object_hashes {
        if !scope.objects.contains(hash) {
            return Err(not_in_scope(&format!(
                "object {hash} is not reachable from root {}",
                root.describe()
            )));
        }
    }
    Ok(())
}

/// Fail unless `blob_hashes` are all readable blobs under `scope`.
pub fn require_blobs_in_scope(
    scope: &ReadScope,
    root: &ReadRoot,
    blob_hashes: &[String],
) -> Result<(), HandlerError> {
    for hash in blob_hashes {
        if !scope.blobs.contains(hash) {
            return Err(not_in_scope(&format!(
                "blob {hash} is not reachable from root {}",
                root.describe()
            )));
        }
    }
    Ok(())
}

/// Fail unless `hash` is a readable large object under `scope`.
pub fn require_large_object_in_scope(
    scope: &ReadScope,
    root: &ReadRoot,
    hash: &str,
) -> Result<(), HandlerError> {
    if scope.large_objects.contains(hash) {
        Ok(())
    } else {
        Err(not_in_scope(&format!(
            "large object {hash} is not reachable from root {}",
            root.describe()
        )))
    }
}

// ── root resolution ────────────────────────────────────────────────────────

fn resolve_project_head(
    project_path: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    let canonical = ryeos_executor::execution::project_source::canonical_project_ref(project_path)
        .map_err(|error| HandlerError::BadRequest(format!("project_path: {error}")))?;
    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)
        .map_err(|_| not_in_scope("the caller has no project head namespace"))?
        .to_owned();
    let project_hash = lillux::sha256_hex(canonical.as_bytes());
    let head = state
        .state_store
        .with_state_db(|db| db.read_project_head(&principal_key, &project_hash))
        .map_err(internal)?;
    // An absent head is a project that was never pushed: the root exists and
    // reaches nothing. A push in progress asks `objects/has` here; everything
    // it uploads is by construction not yet reachable, so it answers absent
    // and the sender uploads (the write deduplicates).
    Ok(ResolvedRoot::objects(head.into_iter().collect()))
}

fn resolve_thread_result(
    thread_id: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    let thread = state
        .state_store
        .get_thread(thread_id)
        .map_err(internal)?
        .ok_or_else(|| not_in_scope(&format!("thread {thread_id} is not readable")))?;
    if !ctx.is_owner(thread.requested_by.as_deref()) {
        return Err(not_in_scope(&format!("thread {thread_id} is not readable")));
    }
    let mut objects = Vec::new();
    objects.extend(thread.result_project_snapshot_hash);
    objects.extend(thread.admitted_launch_capsule_hash);
    Ok(ResolvedRoot::objects(objects))
}

const BUNDLE_EXPORT_CAP: &str = "ryeos.execute.service.bundle/export";
const ADMISSION_LIST_CAP: &str = "ryeos.execute.service.admission/attestations-for-subject";

fn holds_cap(ctx: &HandlerContext, required: &str) -> bool {
    ctx.scopes
        .iter()
        .any(|granted| ryeos_runtime::authorizer::cap_matches(granted, required))
}

fn resolve_bundle_export(
    bundle_name: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    if !holds_cap(ctx, BUNDLE_EXPORT_CAP) {
        return Err(not_in_scope(&format!(
            "bundle {bundle_name} export needs the {BUNDLE_EXPORT_CAP} capability"
        )));
    }
    let blobs = crate::handlers::bundle_export::exported_blob_hashes(state, bundle_name)
        .map_err(|_| not_in_scope(&format!("bundle {bundle_name} has no readable export")))?;
    Ok(ResolvedRoot {
        objects: Vec::new(),
        blobs,
    })
}

const CATALOG_RESOLVE_CAP: &str = "ryeos.execute.service.bundle-catalog/resolve";

fn resolve_catalog_publication(
    catalog_namespace: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    if !holds_cap(ctx, CATALOG_RESOLVE_CAP) {
        return Err(not_in_scope(&format!(
            "catalog {catalog_namespace} needs the {CATALOG_RESOLVE_CAP} capability"
        )));
    }
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                ryeos_app::bundle_publication::catalog::BUNDLE_CATALOG_HEAD_NAMESPACE,
                catalog_namespace,
            )
        })
        .map_err(|_| not_in_scope(&format!("catalog {catalog_namespace} is not readable")))?
        .ok_or_else(|| not_in_scope(&format!("catalog {catalog_namespace} is not readable")))?;
    Ok(ResolvedRoot::objects(vec![head.target_hash]))
}

fn resolve_admitted_subject(
    subject_hash: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    if !holds_cap(ctx, ADMISSION_LIST_CAP) {
        return Err(not_in_scope(&format!(
            "subject {subject_hash} needs the {ADMISSION_LIST_CAP} capability"
        )));
    }
    if !lillux::valid_hash(subject_hash) || subject_hash.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(not_in_scope("the admitted subject hash is not canonical"));
    }
    let admitted = state
        .state_store
        .with_state_db(|db| db.list_admission_attestations_for_subject(subject_hash, None))
        .map_err(internal)?
        .into_iter()
        .any(|record| record.state == ryeos_state::AdmissionAttestationState::Accepted);
    if !admitted {
        return Err(not_in_scope(&format!(
            "subject {subject_hash} has no accepted admission on this node"
        )));
    }
    Ok(ResolvedRoot::objects(vec![subject_hash.to_owned()]))
}

fn resolve_placement(
    placement_id: &str,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<ResolvedRoot, HandlerError> {
    let refuse = || not_in_scope(&format!("placement {placement_id} is not readable"));

    // A dedicated worker session owned by the caller.
    if let Some(session) = state
        .state_store
        .dedicated_session(placement_id)
        .map_err(internal)?
    {
        if !ctx.is_owner(Some(&session.owner_principal)) {
            return Err(refuse());
        }
        let mut objects = vec![session.admitted_capsule_hash.clone()];
        objects.extend(session.candidate_snapshot_hash.clone());
        objects.extend(session.candidate_validation_hash.clone());
        objects.extend(session.candidate_evaluation_hash.clone());
        if let Some(head) = state
            .state_store
            .with_state_db(|db| db.read_generic_head_ref("chains", &session.chain_root_id))
            .map_err(internal)?
        {
            objects.push(head.target_hash);
        }
        return Ok(ResolvedRoot::objects(objects));
    }

    // A handoff or follow-delivery operation retained on this node, whose
    // configured peer is the caller. The peer is recorded either as the remote
    // name or as its site id.
    let job_ids = [
        format!("worker-handoff-source:{placement_id}"),
        format!("worker-handoff-target:{placement_id}"),
        format!("worker-handoff-preflight-source:{placement_id}"),
        format!("worker-handoff-preflight-target:{placement_id}"),
        format!("worker-handoff-preflight-grant:{placement_id}"),
        format!("remote-follow-terminal-target:{placement_id}"),
    ];
    for job_id in job_ids {
        let Some(job) = state
            .state_store
            .with_state_db(|db| db.get_sync_job(&job_id))
            .map_err(internal)?
        else {
            continue;
        };
        let peer_ref = job.peer.as_deref().ok_or_else(refuse)?;
        let remotes =
            crate::remote::config::load_remotes(&state.config.app_root).map_err(internal)?;
        let peer_principal = match crate::remote::config::get_remote(&remotes, peer_ref) {
            Ok(peer) => peer.principal_id,
            Err(_) => {
                crate::remote::config::resolve_remote_by_site_id(&remotes, peer_ref)
                    .map_err(|_| refuse())?
                    .remote
                    .principal_id
            }
        };
        if peer_principal != ctx.fingerprint {
            return Err(refuse());
        }
        let mut hashes = BTreeSet::new();
        hashes.extend(job.roots.iter().cloned());
        hashes.extend(job.heads.iter().cloned());
        collect_hashes(&job.operation, &mut hashes);
        if let Some(result) = &job.result {
            collect_hashes(result, &mut hashes);
        }
        // The target publishes its terminal testimony as a signed head keyed
        // by the operation; it is part of the placement once it exists, and
        // so is the chain head that testimony is about.
        if let Some(head) = state
            .state_store
            .with_state_db(|db| {
                db.read_generic_head_ref(
                    ryeos_app::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                    placement_id,
                )
            })
            .map_err(internal)?
        {
            let cas_read = state.acquire_cas_read().map_err(internal)?;
            if let Some(value) = cas_read
                .cas()
                .get_object(&head.target_hash)
                .map_err(internal)?
                && let Ok(attestation) = ryeos_state::objects::Attestation::from_value(&value)
            {
                hashes.insert(attestation.subject_hash);
            }
            hashes.insert(head.target_hash);
        }
        return Ok(ResolvedRoot::objects(hashes.into_iter().collect()));
    }
    Err(refuse())
}

/// Every canonical lowercase SHA-256 string a retained record names.
fn collect_hashes(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(text)
            if text.len() == 64
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            out.insert(text.clone());
        }
        Value::Array(items) => items.iter().for_each(|item| collect_hashes(item, out)),
        Value::Object(map) => map.values().for_each(|item| collect_hashes(item, out)),
        _ => {}
    }
}

// ── closure membership ─────────────────────────────────────────────────────

const CLOSURE_CACHE_ENTRIES: usize = 16;

#[derive(Default)]
struct ClosureCache {
    entries: BTreeMap<String, Arc<ReadScope>>,
    order: VecDeque<String>,
}

fn closure_cache() -> &'static Mutex<ClosureCache> {
    static CACHE: OnceLock<Mutex<ClosureCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn closure_scope(resolved: ResolvedRoot, state: &AppState) -> Result<Arc<ReadScope>, HandlerError> {
    let mut roots = resolved.objects;
    roots.sort();
    roots.dedup();
    let mut scope = ReadScope {
        blobs: resolved.blobs,
        ..Default::default()
    };
    if roots.is_empty() {
        return Ok(Arc::new(scope));
    }
    // A closure is a function of the immutable root hashes and of which
    // objects this node holds. Only a complete closure is stable, so only a
    // complete closure is cached, keyed by the store as well as the roots.
    let key = format!("{}|{}", state.config.app_root.display(), roots.join(","));
    if let Some(hit) = closure_cache()
        .lock()
        .map_err(|_| internal("object read scope cache poisoned"))?
        .entries
        .get(&key)
    {
        if scope.blobs.is_empty() {
            return Ok(Arc::clone(hit));
        }
        let mut merged = ReadScope {
            objects: hit.objects.clone(),
            blobs: hit.blobs.clone(),
            large_objects: hit.large_objects.clone(),
        };
        merged.blobs.extend(scope.blobs);
        return Ok(Arc::new(merged));
    }

    let cas_read = state.acquire_cas_read().map_err(internal)?;
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas_read.cas(),
        roots,
        ryeos_state::object_closure::ObjectClosureLimits {
            max_objects: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECTS,
            max_blobs: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOBS,
            max_object_bytes: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_OBJECT_BYTES,
            max_total_object_bytes:
                ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_OBJECT_BYTES,
            max_blob_bytes: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_BLOB_BYTES,
            max_total_blob_bytes: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_TOTAL_BLOB_BYTES,
            max_links_per_object: ryeos_state::object_closure::REMOTE_CLOSURE_MAX_LINKS_PER_OBJECT,
        },
    )
    .map_err(internal)?;
    let complete = report.is_complete();
    let mut closure = ReadScope {
        objects: report.object_hashes,
        blobs: report.blob_hashes,
        large_objects: report.large_object_hashes,
    };
    // A referenced hash this node does not hold is still inside the root, so
    // asking for it reports it missing rather than out of scope.
    closure
        .objects
        .extend(report.missing_objects.into_iter().map(|item| item.hash));
    closure
        .blobs
        .extend(report.missing_blobs.into_iter().map(|item| item.hash));
    let closure = Arc::new(closure);
    if complete {
        let mut cache = closure_cache()
            .lock()
            .map_err(|_| internal("object read scope cache poisoned"))?;
        if !cache.entries.contains_key(&key) {
            cache.order.push_back(key.clone());
            cache.entries.insert(key, Arc::clone(&closure));
            while cache.order.len() > CLOSURE_CACHE_ENTRIES {
                if let Some(evicted) = cache.order.pop_front() {
                    cache.entries.remove(&evicted);
                }
            }
        }
    }
    if scope.blobs.is_empty() {
        return Ok(closure);
    }
    scope.objects = closure.objects.clone();
    scope.blobs.extend(closure.blobs.iter().cloned());
    scope.large_objects = closure.large_objects.clone();
    Ok(Arc::new(scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_serialises_with_its_kind_tag_and_rejects_unknown_fields() {
        let root: ReadRoot = serde_json::from_value(serde_json::json!({
            "kind": "project_head", "project_path": "/p"
        }))
        .unwrap();
        assert_eq!(
            root,
            ReadRoot::ProjectHead {
                project_path: "/p".into()
            }
        );
        assert!(
            serde_json::from_value::<ReadRoot>(serde_json::json!({
                "kind": "thread_result", "thread_id": "T-1", "extra": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ReadRoot>(serde_json::json!({"kind": "everything"})).is_err()
        );
    }

    #[test]
    fn refusal_is_typed_and_names_the_cure() {
        let HandlerError::Structured { code, status, body } = root_required("objects/get") else {
            panic!("expected a structured refusal");
        };
        assert_eq!(code, OBJECT_NOT_IN_SCOPE);
        assert_eq!(status, 403);
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("Name a root"));
        assert!(message.contains("objects/get-any"));
    }

    #[test]
    fn scope_membership_is_identical_for_absent_and_foreign_hashes() {
        let scope = ReadScope {
            objects: BTreeSet::from(["a".repeat(64)]),
            ..Default::default()
        };
        let root = ReadRoot::ThreadResult {
            thread_id: "T-1".into(),
        };
        let foreign = "b".repeat(64);
        let absent = "c".repeat(64);
        let one = require_objects_in_scope(&scope, &root, std::slice::from_ref(&foreign))
            .unwrap_err()
            .to_string();
        let two = require_objects_in_scope(&scope, &root, std::slice::from_ref(&absent))
            .unwrap_err()
            .to_string();
        assert_eq!(one.replace(&foreign, "H"), two.replace(&absent, "H"));
        assert!(require_objects_in_scope(&scope, &root, &["a".repeat(64)]).is_ok());
    }

    #[test]
    fn retained_record_hashes_are_collected_from_nested_json() {
        let hash = "ab".repeat(32);
        let value = serde_json::json!({
            "a": [hash.clone(), "not-a-hash", "AB".repeat(32)],
            "b": {"c": hash.clone()},
        });
        let mut out = BTreeSet::new();
        collect_hashes(&value, &mut out);
        assert_eq!(out, BTreeSet::from([hash]));
    }
}
