//! `records.get`, `records.list` and `records.kinds`: read a signed record of
//! this node by its declared kind, and say what was checked.
//!
//! A record kind is a signed `config` item shipped by an installed bundle at
//! `config:ryeos/core/records/<record_kind>`. The item names where the record
//! is stored, which verifier proves it, how its key is formed and which
//! freshness rule applies. Storage, verifier, key rule and freshness rule come
//! from small closed sets implemented here, because each is a mechanism; which
//! records exist is data. An item that names a mechanism this build does not
//! implement is reported unusable, never guessed at.
//!
//! Every answer carries a `verify` object. A record whose verification fails
//! is not disclosed: the answer states the failure and withholds the body,
//! unless the caller holds the capability to inspect unverified records and
//! asks for them.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node::capability_report::{ReportReadFailure, SignedNodeCapabilityReport};

/// Bare-id prefix under which installed bundles declare record kinds.
const KIND_ID_PREFIX: &str = "ryeos/core/records/";
/// Capability that lets a caller ask for the body of a record whose
/// verification failed.
pub(crate) const INCLUDE_UNVERIFIED_CAP: &str = "ryeos.read.records.unverified";
/// The key of a singleton record.
const SINGLETON_KEY: &str = "singleton";
const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 200;

/// Where a record is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Storage {
    NodeFile,
    BundleFile,
    CasObject,
    StateLedger,
}

impl Storage {
    fn name(self) -> &'static str {
        match self {
            Self::NodeFile => "node_file",
            Self::BundleFile => "bundle_file",
            Self::CasObject => "cas_object",
            Self::StateLedger => "state_ledger",
        }
    }

    /// The `stored_as` word an answer uses.
    fn stored_as(self) -> &'static str {
        match self {
            Self::NodeFile => "file",
            Self::BundleFile => "bundle_file",
            Self::CasObject => "cas_object",
            Self::StateLedger => "state_ledger",
        }
    }

    fn implemented(self) -> bool {
        matches!(self, Self::NodeFile)
    }
}

/// How a record is proved. Only the schemes this build implements exist as
/// variants; a declared name outside this set makes its kind unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verifier {
    /// A JSON record carrying its own digest and the node key's signature
    /// over it, read through the node's own verifying reader.
    NodeSignedJsonDigest,
}

impl Verifier {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "node_signed_json_digest" => Ok(Self::NodeSignedJsonDigest),
            other => Err(format!(
                "verifier `{other}` is not implemented on this node"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::NodeSignedJsonDigest => "node_signed_json_digest",
        }
    }
}

/// How a record's key is formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyRule {
    /// One record of this kind exists.
    Singleton,
}

impl KeyRule {
    fn parse(rule: &str) -> Result<Self, String> {
        match rule {
            "singleton" => Ok(Self::Singleton),
            other => Err(format!(
                "key rule `{other}` is not implemented on this node"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Singleton => "singleton",
        }
    }
}

/// Whether and how a record can have gone stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    None,
    /// Stale when the node policy generation it was made under is not the
    /// current one.
    NodePolicyGeneration,
}

impl Freshness {
    fn parse(rule: Option<&str>) -> Result<Self, String> {
        match rule {
            None | Some("none") => Ok(Self::None),
            Some("node_policy_generation") => Ok(Self::NodePolicyGeneration),
            Some(other) => Err(format!(
                "freshness rule `{other}` is not implemented on this node"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NodePolicyGeneration => "node_policy_generation",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageDecl {
    #[serde(rename = "type")]
    kind: Storage,
    path: Option<String>,
    format: Option<String>,
}

/// The declared shape of one record kind, as the signed item states it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KindDecl {
    #[serde(default, rename = "category")]
    _category: Option<String>,
    #[serde(default, rename = "version")]
    _version: Option<String>,
    #[serde(default, rename = "description")]
    _description: Option<String>,
    record_kind: String,
    storage: StorageDecl,
    verifier: String,
    key: String,
    #[serde(default)]
    holder: Option<String>,
    #[serde(default)]
    summary: Vec<String>,
    max_bytes: u64,
    #[serde(default)]
    freshness: Option<String>,
    /// Named lists the answer derives from maps in the body, so a client that
    /// can bind a list but not a map still reads them row by row.
    #[serde(default)]
    collections: Vec<CollectionDecl>,
}

/// One derived list: the object at `from` becomes one row per entry, the entry's
/// key in `key_field`, the entry's own fields beside it (a scalar entry rides
/// in `value`). A mechanism over the declared pointer; it names no record
/// field of its own.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CollectionDecl {
    name: String,
    from: String,
    key_field: String,
}

/// A record kind whose declaration this build can serve.
#[derive(Debug)]
struct UsableKind {
    decl: KindDecl,
    storage: Storage,
    verifier: Verifier,
    key: KeyRule,
    freshness: Freshness,
    /// The record file relative to the node's app root.
    path: PathBuf,
}

/// One declared kind: usable, or unusable with the reason stated.
#[derive(Debug)]
struct LoadedKind {
    name: String,
    usable: Result<UsableKind, String>,
}

fn relative_record_path(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("storage path must be a relative path with no `..` or `.` parts".to_owned());
    }
    Ok(path.to_path_buf())
}

fn pointer_is_valid(pointer: &str) -> bool {
    pointer.is_empty() || pointer.starts_with('/')
}

fn resolve_decl(name: &str, value: Value) -> Result<UsableKind, String> {
    let decl: KindDecl = serde_json::from_value(value)
        .map_err(|error| format!("declaration is invalid: {error}"))?;
    if decl.record_kind != name {
        return Err(format!(
            "record_kind `{}` does not match its item id `{name}`",
            decl.record_kind
        ));
    }
    let verifier = Verifier::parse(&decl.verifier)?;
    let key = KeyRule::parse(&decl.key)?;
    let freshness = Freshness::parse(decl.freshness.as_deref())?;
    let storage = decl.storage.kind;
    if !storage.implemented() {
        return Err(format!(
            "storage `{}` is not implemented on this node",
            storage.name()
        ));
    }
    if decl.storage.format.as_deref() != Some("json") {
        return Err("storage format must be `json`".to_owned());
    }
    let path = relative_record_path(
        decl.storage
            .path
            .as_deref()
            .ok_or_else(|| "storage path is required for node_file".to_owned())?,
    )?;
    if decl.max_bytes == 0 {
        return Err("max_bytes must be positive".to_owned());
    }
    if let Some(holder) = decl.holder.as_deref().filter(|h| !pointer_is_valid(h)) {
        return Err(format!("holder `{holder}` is not a JSON pointer"));
    }
    if let Some(bad) = decl
        .summary
        .iter()
        .find(|pointer| !pointer_is_valid(pointer))
    {
        return Err(format!("summary entry `{bad}` is not a JSON pointer"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for collection in &decl.collections {
        let name_ok = !collection.name.is_empty()
            && collection.name.len() <= 64
            && collection
                .name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !name_ok || !seen.insert(collection.name.as_str()) {
            return Err(format!(
                "collection name `{}` must be unique lowercase letters, digits and underscores",
                collection.name
            ));
        }
        if !collection.from.starts_with('/') || collection.key_field.is_empty() {
            return Err(format!(
                "collection `{}` needs a JSON pointer in `from` and a `key_field`",
                collection.name
            ));
        }
    }
    Ok(UsableKind {
        decl,
        storage,
        verifier,
        key,
        freshness,
        path,
    })
}

fn load_kind(engine: &ryeos_engine::engine::Engine, name: &str) -> LoadedKind {
    let usable = match engine.load_trusted_bundle_config(&format!("{KIND_ID_PREFIX}{name}.yaml")) {
        Ok(Some(value)) => resolve_decl(name, value),
        Ok(None) => Err("the declaration is no longer installed".to_owned()),
        Err(error) => Err(format!("the declaration did not load: {error}")),
    };
    LoadedKind {
        name: name.to_owned(),
        usable,
    }
}

/// Every record kind the installed bundles declare, each judged.
fn load_kinds(engine: &ryeos_engine::engine::Engine) -> HandlerResult<Vec<LoadedKind>> {
    let ids = engine
        .installed_bundle_config_ids(KIND_ID_PREFIX)
        .map_err(|error| HandlerError::Internal(format!("list record kinds: {error}")))?;
    Ok(ids
        .iter()
        .filter_map(|id| id.strip_prefix(KIND_ID_PREFIX))
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .map(|name| load_kind(engine, name))
        .collect())
}

fn find_kind<'a>(kinds: &'a [LoadedKind], record_kind: &str) -> HandlerResult<&'a LoadedKind> {
    kinds
        .iter()
        .find(|kind| kind.name == record_kind)
        .ok_or(HandlerError::NotFound)
}

/// What a read of one record found.
enum Found {
    /// No such record.
    Absent,
    /// The record is there and verified.
    Verified {
        record: Box<SignedNodeCapabilityReport>,
        body: Value,
        bytes: usize,
        fresh: Option<bool>,
        stale_reason: Option<String>,
    },
    /// The record is there and did not verify, for the stated reason.
    Failed { code: &'static str, message: String },
}

fn read_record(kind: &UsableKind, app_root: &Path, policy_generation: &str) -> Found {
    let path = app_root.join(&kind.path);
    match kind.verifier {
        Verifier::NodeSignedJsonDigest => {
            match SignedNodeCapabilityReport::read_verified_from(
                &path,
                app_root,
                kind.decl.max_bytes,
            ) {
                Ok(None) => Found::Absent,
                Ok(Some(signed)) => {
                    let body = match serde_json::to_value(&signed.report) {
                        Ok(body) => body,
                        Err(error) => {
                            return Found::Failed {
                                code: "unreadable",
                                message: format!("the verified record cannot be rendered: {error}"),
                            };
                        }
                    };
                    let bytes = serde_json::to_vec(&body).map_or(usize::MAX, |bytes| bytes.len());
                    if bytes as u64 > kind.decl.max_bytes {
                        return Found::Failed {
                            code: "oversized",
                            message: "the record is larger than its kind allows".to_owned(),
                        };
                    }
                    let (fresh, stale_reason) = match kind.freshness {
                        Freshness::None => (None, None),
                        Freshness::NodePolicyGeneration => {
                            match signed.staleness(Some(policy_generation)) {
                                Ok(None) => (Some(true), None),
                                Ok(Some(reason)) => (Some(false), Some(reason)),
                                Err(error) => (Some(false), Some(format!("{error:#}"))),
                            }
                        }
                    };
                    Found::Verified {
                        record: Box::new(signed),
                        body,
                        bytes,
                        fresh,
                        stale_reason,
                    }
                }
                Err(error) => match ReportReadFailure::find(&error) {
                    Some(failure) => Found::Failed {
                        code: failure.code(),
                        message: failure.to_string(),
                    },
                    None => Found::Failed {
                        code: "unreadable",
                        message: format!("{error:#}"),
                    },
                },
            }
        }
    }
}

fn digest_of(found: &Found) -> Option<&str> {
    match found {
        Found::Verified { record, .. } => Some(record.digest.as_str()),
        _ => None,
    }
}

fn signer_of(found: &Found) -> Option<&str> {
    match found {
        Found::Verified { record, .. } => Some(record.signer.as_str()),
        _ => None,
    }
}

/// The signer's standing at read time. The node's own key is `node`: the
/// verifier pins that signer to this node's identity. A signer whose standing
/// can change (an operator or a publisher key) reports `active`, `retired` or
/// `revoked` once such a kind exists and its resolver is in; no kind that needs
/// it is declared yet. `signing_principal` is absent, never null, until a
/// person-signed kind exists.
fn signer_status_of(kind: &UsableKind, found: &Found) -> Option<&'static str> {
    match (found, kind.verifier) {
        (Found::Verified { .. }, Verifier::NodeSignedJsonDigest) => Some("node"),
        _ => None,
    }
}

/// The stable code of a stale report's reason. The reason's sentence names a
/// policy generation or says the host restarted; a seat reads the code, and the
/// sentence stays in `detail` for the operator.
fn stale_code(reason: &str) -> &'static str {
    if reason.starts_with("the host restarted") {
        "host_restarted"
    } else if reason.starts_with("node policy generation is now") {
        "policy_generation_changed"
    } else if reason.starts_with("the current node policy generation is unreadable") {
        "policy_generation_unreadable"
    } else {
        "stale"
    }
}

fn verify_object(kind: &UsableKind, found: &Found) -> Value {
    let (status, reason, detail, fresh) = match found {
        Found::Verified {
            fresh,
            stale_reason,
            ..
        } => (
            "verified",
            stale_reason.as_deref().map(stale_code).map(str::to_owned),
            stale_reason.clone(),
            *fresh,
        ),
        Found::Failed { code, message } => (
            "failed",
            Some((*code).to_owned()),
            Some(message.clone()),
            None,
        ),
        Found::Absent => ("failed", Some("absent".to_owned()), None, None),
    };
    json!({
        "status": status,
        "scheme": kind.verifier.name(),
        "signer": signer_of(found),
        "signer_status": signer_status_of(kind, found),
        "reason": reason,
        "detail": detail,
        "fresh": fresh,
    })
}

/// The derived lists of a verified body: one array per declared collection, and
/// per collection a coverage word: `present`, `not_reported` (the body has no
/// such map), `not_a_map` (the pointer names an array or a scalar) or
/// `key_field_collision` (an entry has a field named like the key field; that
/// field is left out of its row, and this word says so).
fn collections_of(kind: &UsableKind, body: &Value) -> (Value, Value) {
    let mut lists = serde_json::Map::new();
    let mut coverage = serde_json::Map::new();
    for collection in &kind.decl.collections {
        let mut rows = Vec::new();
        // `present` is the normal case; the others say why the list is what it
        // is, so a format change reads as a shape error and a dropped field is
        // never silent.
        let mut state = "present";
        match body.pointer(&collection.from) {
            Some(Value::Object(entries)) => {
                for (key, entry) in entries {
                    let mut row = serde_json::Map::new();
                    row.insert(collection.key_field.clone(), Value::String(key.clone()));
                    match entry {
                        Value::Object(fields) => {
                            for (name, value) in fields {
                                if name == &collection.key_field {
                                    state = "key_field_collision";
                                } else {
                                    row.insert(name.clone(), value.clone());
                                }
                            }
                        }
                        other => {
                            row.insert("value".to_owned(), other.clone());
                        }
                    }
                    rows.push(Value::Object(row));
                }
            }
            Some(_) => state = "not_a_map",
            None => state = "not_reported",
        }
        lists.insert(collection.name.clone(), Value::Array(rows));
        coverage.insert(collection.name.clone(), Value::String(state.to_owned()));
    }
    (Value::Object(lists), Value::Object(coverage))
}

fn summary_of(kind: &UsableKind, body: &Value) -> Value {
    let mut summary = serde_json::Map::new();
    for pointer in &kind.decl.summary {
        if let Some(value) = body.pointer(pointer) {
            summary.insert(pointer.clone(), value.clone());
        }
    }
    Value::Object(summary)
}

/// Whether a record answers the read's key and hash. A key or hash that names
/// nothing here answers as absent, the same as a record that does not exist.
fn selected(kind: &UsableKind, key: Option<&str>, hash: Option<&str>, found: &Found) -> bool {
    match kind.key {
        KeyRule::Singleton => {
            if key.is_some_and(|key| key != SINGLETON_KEY) {
                return false;
            }
        }
    }
    match hash {
        Some(hash) => digest_of(found) == Some(hash),
        None => true,
    }
}

fn holder_matches(kind: &UsableKind, holder: Option<&str>, found: &Found) -> bool {
    let (Some(wanted), Some(pointer)) = (holder, kind.decl.holder.as_deref()) else {
        return true;
    };
    match found {
        Found::Verified { body, .. } => {
            body.pointer(pointer).and_then(Value::as_str) == Some(wanted)
        }
        _ => false,
    }
}

fn refuse_unsupported_holder(kind: &UsableKind, holder: Option<&str>) -> HandlerResult<()> {
    if holder.is_some() && kind.decl.holder.is_none() {
        return Err(HandlerError::BadRequest(format!(
            "records_kind_has_no_holder: `{}` records have no holder to filter by",
            kind.decl.record_kind
        )));
    }
    Ok(())
}

/// A caller may ask for the body of an unverified record only with the
/// capability that says so.
fn require_unverified_authority(include_unverified: bool, holds_cap: bool) -> HandlerResult<()> {
    if include_unverified && !holds_cap {
        return Err(HandlerError::Forbidden(format!(
            "records_unverified_requires_capability: reading an unverified record needs {INCLUDE_UNVERIFIED_CAP}"
        )));
    }
    Ok(())
}

fn unverified_kind_answer(name: &str, reason: &str) -> Value {
    json!({
        "record_kind": name,
        "key": null,
        "hash": null,
        "stored_as": null,
        "bytes": null,
        "verify": {
            "status": "unverified_kind",
            "scheme": null,
            "signer": null,
            "signer_status": null,
            "reason": reason,
            "fresh": null,
        },
        "truncated": false,
    })
}

/// What a record claims to say, read without verification and within the
/// kind's size bound. `None` when the file is absent, over the bound or not
/// JSON: an oversized record is never partly disclosed.
fn unverified_body(kind: &UsableKind, app_root: &Path) -> Option<Value> {
    use std::io::Read as _;
    let file = std::fs::File::open(app_root.join(&kind.path)).ok()?;
    let mut bytes = Vec::new();
    file.take(kind.decl.max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > kind.decl.max_bytes {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    Some(value.get("report").cloned().unwrap_or(value))
}

/// A failed record's body appears only when the caller asked for it and held
/// the capability, and then beside a `verify.status` of `failed`.
fn record_answer(kind: &UsableKind, found: &Found, unverified: Option<Value>) -> Value {
    let mut answer = json!({
        "record_kind": kind.decl.record_kind,
        "key": SINGLETON_KEY,
        "hash": digest_of(found),
        "stored_as": kind.storage.stored_as(),
        "bytes": match found { Found::Verified { bytes, .. } => json!(bytes), _ => Value::Null },
        "verify": verify_object(kind, found),
        "truncated": false,
    });
    if let Found::Verified { body, .. } = found {
        answer["body"] = body.clone();
        if !kind.decl.collections.is_empty() {
            let (collections, coverage) = collections_of(kind, body);
            answer["collections"] = collections;
            answer["collections_coverage"] = coverage;
        }
    } else if let Some(body) = unverified {
        answer["body"] = body;
    }
    answer
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    pub record_kind: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub holder: Option<String>,
    #[serde(default)]
    pub include_unverified: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub record_kind: String,
    #[serde(default)]
    pub holder: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KindsRequest {}

fn holds_unverified_cap(ctx: &HandlerContext, state: &AppState) -> bool {
    state
        .authorizer
        .authorize(
            &ctx.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(INCLUDE_UNVERIFIED_CAP),
        )
        .is_ok()
}

fn policy_generation(state: &AppState) -> String {
    format!("sha256:{}", state.node_policy.generation_digest())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> HandlerResult<T> + Send + 'static,
) -> HandlerResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| HandlerError::Internal(format!("records read task failed: {error}")))?
}

pub async fn get(
    req: GetRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    require_unverified_authority(req.include_unverified, holds_unverified_cap(&ctx, &state))?;
    blocking(move || {
        let kinds = load_kinds(&state.engine)?;
        let loaded = find_kind(&kinds, &req.record_kind)?;
        let kind = match &loaded.usable {
            Ok(kind) => kind,
            Err(reason) => return Ok(unverified_kind_answer(&loaded.name, reason)),
        };
        refuse_unsupported_holder(kind, req.holder.as_deref())?;
        let found = read_record(kind, &state.config.app_root, &policy_generation(&state));
        if matches!(found, Found::Absent)
            || !selected(kind, req.key.as_deref(), req.hash.as_deref(), &found)
            || !holder_matches(kind, req.holder.as_deref(), &found)
        {
            return Err(HandlerError::NotFound);
        }
        let unverified = (req.include_unverified && matches!(found, Found::Failed { .. }))
            .then(|| unverified_body(kind, &state.config.app_root))
            .flatten();
        Ok(record_answer(kind, &found, unverified))
    })
    .await
}

pub async fn list(
    req: ListRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let limit = req.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(HandlerError::BadRequest(format!(
            "records_limit_out_of_range: limit must be between 1 and {MAX_LIST_LIMIT}"
        )));
    }
    let offset = match req.cursor.as_deref() {
        None => 0,
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            HandlerError::BadRequest(
                "records_cursor_invalid: the cursor is not one this service issued".to_owned(),
            )
        })?,
    };
    blocking(move || {
        let kinds = load_kinds(&state.engine)?;
        let loaded = find_kind(&kinds, &req.record_kind)?;
        let kind = match &loaded.usable {
            Ok(kind) => kind,
            Err(reason) => return Ok(unverified_kind_answer(&loaded.name, reason)),
        };
        refuse_unsupported_holder(kind, req.holder.as_deref())?;
        let found = read_record(kind, &state.config.app_root, &policy_generation(&state));
        // A kind with a singleton key has at most one record.
        let mut records = Vec::new();
        if !matches!(found, Found::Absent) && holder_matches(kind, req.holder.as_deref(), &found) {
            let summary = match &found {
                Found::Verified { body, .. } => summary_of(kind, body),
                _ => Value::Null,
            };
            records.push(json!({
                "key": SINGLETON_KEY,
                "hash": digest_of(&found),
                "summary": summary,
                "verify": verify_object(kind, &found),
            }));
        }
        let total = records.len();
        let page: Vec<Value> = records.into_iter().skip(offset).take(limit).collect();
        let next = offset.saturating_add(page.len());
        Ok(json!({
            "record_kind": kind.decl.record_kind,
            "records": page,
            "next_cursor": if next < total { json!(next.to_string()) } else { Value::Null },
        }))
    })
    .await
}

pub async fn kinds(
    _req: KindsRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    blocking(move || {
        let kinds = load_kinds(&state.engine)?;
        let rows: Vec<Value> = kinds
            .iter()
            .map(|loaded| match &loaded.usable {
                Ok(kind) => json!({
                    "record_kind": loaded.name,
                    "usable": true,
                    "reason": null,
                    "storage": kind.storage.name(),
                    "verifier": kind.verifier.name(),
                    "key": kind.key.name(),
                    "holder": kind.decl.holder,
                    "freshness": kind.freshness.name(),
                    "max_bytes": kind.decl.max_bytes,
                }),
                Err(reason) => json!({
                    "record_kind": loaded.name,
                    "usable": false,
                    "reason": reason,
                }),
            })
            .collect();
        Ok(json!({ "kinds": rows }))
    })
    .await
}

pub const GET_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:records/get",
    endpoint: "records.get",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.records/get"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: GetRequest = crate::handler_error::parse_request(params)?;
            get(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:records/list",
    endpoint: "records.list",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.records/list"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ListRequest = crate::handler_error::parse_request(params)?;
            list(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const KINDS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:records/kinds",
    endpoint: "records.kinds",
    availability: ServiceAvailability::Both,
    required_caps: &["ryeos.execute.service.records/kinds"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: KindsRequest = if params.is_null() {
                KindsRequest::default()
            } else {
                crate::handler_error::parse_request(params)?
            };
            kinds(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration() -> Value {
        json!({
            "category": "ryeos/core/records",
            "version": "1.0.0",
            "description": "fixture",
            "record_kind": "capability_report",
            "storage": {"type": "node_file", "path": ".ai/state/node-capability-report.json", "format": "json"},
            "verifier": "node_signed_json_digest",
            "key": "singleton",
            "holder": null,
            "summary": ["/schema"],
            "max_bytes": 1048576,
            "freshness": "node_policy_generation",
        })
    }

    fn usable(value: Value) -> UsableKind {
        resolve_decl("capability_report", value).unwrap()
    }

    fn with(field: &str, value: Value) -> Value {
        let mut decl = declaration();
        decl[field] = value;
        decl
    }

    #[test]
    fn a_collection_turns_a_map_into_rows_with_the_key_as_a_field() {
        let kind = usable(with(
            "collections",
            json!([
                {"name": "provided", "from": "/provided", "key_field": "name"},
                {"name": "capacity", "from": "/capacity", "key_field": "name"},
                {"name": "unmet", "from": "/floor/unmet", "key_field": "label"},
                {"name": "gone", "from": "/nothing", "key_field": "name"},
            ]),
        ));
        let body = json!({
            "provided": {
                "isolation:pid_ns": {"source": "kernel", "provider": "p1", "basis": "probe"},
            },
            "capacity": {"memory_bytes": 4096},
            "floor": {"unmet": {"quiescence": "never provided", "other": "reason"}},
        });
        let (lists, coverage) = collections_of(&kind, &body);
        assert_eq!(
            lists["provided"],
            json!([{"name": "isolation:pid_ns", "source": "kernel", "provider": "p1", "basis": "probe"}])
        );
        assert_eq!(
            lists["capacity"],
            json!([{"name": "memory_bytes", "value": 4096}])
        );
        assert_eq!(
            lists["unmet"],
            json!([
                {"label": "other", "value": "reason"},
                {"label": "quiescence", "value": "never provided"}
            ])
        );
        assert_eq!(lists["gone"], json!([]));
        assert_eq!(coverage["provided"], "present");
        assert_eq!(coverage["gone"], "not_reported");

        let shaped = json!({
            "provided": ["not", "a", "map"],
            "capacity": {"memory_bytes": {"name": "clash", "bytes": 1}},
        });
        let (lists, coverage) = collections_of(&kind, &shaped);
        assert_eq!(coverage["provided"], "not_a_map");
        assert_eq!(lists["provided"], json!([]));
        assert_eq!(coverage["capacity"], "key_field_collision");
        assert_eq!(
            lists["capacity"],
            json!([{"name": "memory_bytes", "bytes": 1}])
        );
    }

    #[test]
    fn a_collection_with_a_bad_name_or_pointer_makes_the_kind_unusable() {
        for bad in [
            json!([{"name": "Bad", "from": "/a", "key_field": "name"}]),
            json!([{"name": "a", "from": "a", "key_field": "name"}]),
            json!([{"name": "a", "from": "/a", "key_field": ""}]),
            json!([
                {"name": "a", "from": "/a", "key_field": "n"},
                {"name": "a", "from": "/b", "key_field": "n"}
            ]),
        ] {
            assert!(resolve_decl("capability_report", with("collections", bad)).is_err());
        }
    }

    #[test]
    fn a_stale_reasons_sentence_stays_in_detail_and_its_code_is_the_reason() {
        for (sentence, code) in [
            ("the host restarted since this report", "host_restarted"),
            (
                "node policy generation is now a, not b",
                "policy_generation_changed",
            ),
            (
                "the current node policy generation is unreadable",
                "policy_generation_unreadable",
            ),
            ("anything else, /home/x", "stale"),
        ] {
            assert_eq!(stale_code(sentence), code);
        }
    }

    #[test]
    fn a_failed_read_has_no_signer_standing_and_no_signing_principal() {
        let kind = usable(declaration());
        let verify = verify_object(&kind, &Found::Absent);
        assert!(verify["signer_status"].is_null());
        assert!(verify.get("signing_principal").is_none());
    }

    #[test]
    fn the_shipped_shape_of_a_declaration_resolves() {
        let kind = usable(declaration());
        assert_eq!(kind.verifier, Verifier::NodeSignedJsonDigest);
        assert_eq!(kind.storage, Storage::NodeFile);
        assert_eq!(kind.freshness, Freshness::NodePolicyGeneration);
        assert_eq!(
            kind.path,
            PathBuf::from(".ai/state/node-capability-report.json")
        );
    }

    #[test]
    fn a_kind_naming_an_unimplemented_verifier_is_unusable() {
        for name in ["node_signed_yaml", "cas_attestation", "invented"] {
            let error =
                resolve_decl("capability_report", with("verifier", json!(name))).unwrap_err();
            assert!(error.contains("not implemented"), "{name}: {error}");
        }
    }

    #[test]
    fn a_kind_naming_an_unimplemented_storage_or_rule_is_unusable() {
        let mut decl = declaration();
        decl["storage"]["type"] = json!("cas_object");
        assert!(
            resolve_decl("capability_report", decl)
                .unwrap_err()
                .contains("not implemented")
        );
        assert!(
            resolve_decl("capability_report", with("key", json!("filename_stem")))
                .unwrap_err()
                .contains("not implemented")
        );
        assert!(
            resolve_decl("capability_report", with("freshness", json!("never")))
                .unwrap_err()
                .contains("not implemented")
        );
    }

    #[test]
    fn a_declaration_that_escapes_or_mismatches_is_unusable() {
        let mut decl = declaration();
        decl["storage"]["path"] = json!("../outside.json");
        assert!(resolve_decl("capability_report", decl).is_err());
        let mut decl = declaration();
        decl["storage"]["path"] = json!("/etc/passwd");
        assert!(resolve_decl("capability_report", decl).is_err());
        assert!(resolve_decl("other_name", declaration()).is_err());
        assert!(resolve_decl("capability_report", with("extra", json!(1))).is_err());
    }

    #[test]
    fn an_unknown_kind_answers_as_absent() {
        let kinds = vec![LoadedKind {
            name: "capability_report".to_owned(),
            usable: Ok(usable(declaration())),
        }];
        assert!(find_kind(&kinds, "capability_report").is_ok());
        assert!(matches!(
            find_kind(&kinds, "objects_of_any_kind"),
            Err(HandlerError::NotFound)
        ));
        assert!(matches!(
            find_kind(&[], "capability_report"),
            Err(HandlerError::NotFound)
        ));
    }

    #[test]
    fn an_absent_record_reads_as_absent() {
        let root = tempfile::tempdir().unwrap();
        let kind = usable(declaration());
        assert!(matches!(
            read_record(&kind, root.path(), "sha256:current"),
            Found::Absent
        ));
    }

    fn record_file(root: &Path, kind: &UsableKind, bytes: &[u8]) {
        let path = root.join(&kind.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_stale_format_is_a_failed_verification_with_no_body() {
        let root = tempfile::tempdir().unwrap();
        let kind = usable(declaration());
        record_file(
            root.path(),
            &kind,
            br#"{"report":{"schema":"ryeos.node.capability-report/v6","host":{"version":5}}}"#,
        );
        let found = read_record(&kind, root.path(), "sha256:current");
        assert!(matches!(
            &found,
            Found::Failed {
                code: "stale_format",
                ..
            }
        ));
        let answer = record_answer(&kind, &found, None);
        assert_eq!(answer["verify"]["status"], "failed");
        assert_eq!(answer["verify"]["reason"], "stale_format");
        assert_eq!(answer["verify"]["scheme"], "node_signed_json_digest");
        assert!(answer.get("body").is_none());
        assert_eq!(answer["truncated"], false);
    }

    #[test]
    fn an_oversized_record_fails_whole_and_discloses_nothing() {
        let root = tempfile::tempdir().unwrap();
        let mut decl = declaration();
        decl["max_bytes"] = json!(16);
        let kind = usable(decl);
        record_file(root.path(), &kind, &[b' '; 64]);
        let found = read_record(&kind, root.path(), "sha256:current");
        assert!(matches!(
            &found,
            Found::Failed {
                code: "oversized",
                ..
            }
        ));
        let answer = record_answer(&kind, &found, None);
        assert_eq!(answer["verify"]["status"], "failed");
        assert_eq!(answer["verify"]["reason"], "oversized");
        assert!(answer.get("body").is_none());
        assert_eq!(answer["truncated"], false);
    }

    #[test]
    fn an_unreadable_record_is_a_failed_verification() {
        let root = tempfile::tempdir().unwrap();
        let kind = usable(declaration());
        record_file(root.path(), &kind, b"not json");
        let found = read_record(&kind, root.path(), "sha256:current");
        assert!(matches!(
            &found,
            Found::Failed {
                code: "unreadable",
                ..
            }
        ));
    }

    #[test]
    fn the_unverified_body_needs_the_capability() {
        assert!(matches!(
            require_unverified_authority(true, false),
            Err(HandlerError::Forbidden(_))
        ));
        assert!(require_unverified_authority(true, true).is_ok());
        assert!(require_unverified_authority(false, false).is_ok());
    }

    #[test]
    fn a_key_or_hash_that_names_nothing_answers_as_absent() {
        let kind = usable(declaration());
        let failed = Found::Failed {
            code: "unreadable",
            message: String::new(),
        };
        assert!(selected(&kind, None, None, &failed));
        assert!(selected(&kind, Some("singleton"), None, &failed));
        assert!(!selected(&kind, Some("other"), None, &failed));
        // A record that did not verify has no trustworthy hash to match.
        assert!(!selected(&kind, None, Some("sha256:abc"), &failed));
    }

    #[test]
    fn a_holder_filter_on_a_kind_without_a_holder_is_refused() {
        let kind = usable(declaration());
        assert!(refuse_unsupported_holder(&kind, None).is_ok());
        assert!(matches!(
            refuse_unsupported_holder(&kind, Some("fp:abc")),
            Err(HandlerError::BadRequest(_))
        ));
    }

    #[test]
    fn an_unusable_kind_answers_unverified_kind_with_its_reason() {
        let answer = unverified_kind_answer(
            "capability_report",
            "verifier `x` is not implemented on this node",
        );
        assert_eq!(answer["verify"]["status"], "unverified_kind");
        assert!(answer.get("body").is_none());
    }
}
