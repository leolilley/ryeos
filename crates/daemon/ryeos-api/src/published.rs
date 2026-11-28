//! The `/published` read routes: what a stranger may fetch of a publication.
//!
//! This is its own route table, built here in code. It is not read from bundle
//! route YAML, so no bundle can add a route to it, and it is matched before
//! the bundle-driven table is consulted. Its handlers take no caller: no
//! signed request, session, seat, thread, journal, vault or policy is read,
//! except the one node-policy number that caps the bytes served in a day.
//!
//! A read walks these checks in order: the publication exists, is not retired,
//! has not passed `not_after`, and its recorded operator key is still an
//! active operator; then, for a blob, the requested hash is looked up in the
//! closure derived in memory from the frozen manifest. A hash outside the
//! closure is refused with `object_not_in_scope`, the same answer whatever
//! the hash is and whether or not the store holds it, before any read of that
//! hash. Bytes are reserved against the node-wide rolling 24-hour cap before
//! they are read and served, and one read never exceeds the record's
//! `max_bytes_per_read`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ryeos_app::state::AppState;
use ryeos_engine::trust::{
    Closure, MAX_PUBLICATION_MANIFEST_BYTES, PublicationLedger, PublicationManifest,
    PublicationRefusal, PublicationStatus, StoredPublication, is_publication_hash,
    is_publication_id, load_publication, served_ledger_path,
};
use ryeos_node_runtime::node_policy::sections::publication::PublicationPolicy;
use serde_json::{Value, json};

/// The path prefix this table owns.
pub const PUBLISHED_PREFIX: &str = "/published";

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How many reservations are appended before the journal is rewritten.
const COMPACT_AFTER_APPENDS: usize = 8192;
/// Most manifests kept derived in memory.
const MAX_CACHED_MANIFESTS: usize = 16;

/// Whether `path` belongs to this table.
pub fn is_published_path(path: &str) -> bool {
    path == PUBLISHED_PREFIX
        || path
            .strip_prefix(PUBLISHED_PREFIX)
            .is_some_and(|rest| rest.starts_with('/'))
}

// ── The route table ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedRoute {
    List,
    Describe,
    Blob,
}

/// Every route this table serves, all GET. A `{name}` segment captures.
pub const PUBLISHED_ROUTES: &[(PublishedRoute, &str)] = &[
    (PublishedRoute::List, "/published"),
    (PublishedRoute::Describe, "/published/{id}"),
    (PublishedRoute::Blob, "/published/{id}/blob/{hash}"),
];

/// Match `path` against the table.
pub fn match_route(path: &str) -> Option<(PublishedRoute, BTreeMap<&'static str, String>)> {
    let path = path
        .strip_suffix('/')
        .filter(|p| !p.is_empty())
        .unwrap_or(path);
    let given: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    'routes: for (route, pattern) in PUBLISHED_ROUTES {
        let wanted: Vec<&'static str> = pattern.trim_start_matches('/').split('/').collect();
        if wanted.len() != given.len() {
            continue;
        }
        let mut captures = BTreeMap::new();
        for (want, got) in wanted.iter().zip(&given) {
            if let Some(name) = want.strip_prefix('{').and_then(|w| w.strip_suffix('}')) {
                if got.is_empty() {
                    continue 'routes;
                }
                captures.insert(name, (*got).to_owned());
            } else if want != got {
                continue 'routes;
            }
        }
        return Some((*route, captures));
    }
    None
}

// ── The daily byte cap ──────────────────────────────────────────────

/// Mode of a served-bytes journal: node state, owner-only. Set when the file is
/// created and on every open, never left to the umask.
pub const LEDGER_FILE_MODE: u32 = 0o600;
/// Mode of a journal's directory when the journal creates it.
pub const LEDGER_DIR_MODE: u32 = 0o700;

/// A rolling 24-hour journal of bytes reserved by reads, kept in a file so a
/// restart does not reset the day. A reservation is made before the bytes are
/// read and is never returned: bytes that were reserved and then not served
/// still count, so the cap bounds the worst case.
#[derive(Debug)]
pub struct ByteLedger {
    path: PathBuf,
    entries: VecDeque<(i64, u64)>,
    sum: u64,
    appended: usize,
}

/// A reservation that did not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapExhausted {
    pub used: u64,
    pub cap: u64,
}

impl ByteLedger {
    pub fn load(path: PathBuf, now_ms: i64) -> Self {
        let mut ledger = Self {
            path,
            entries: VecDeque::new(),
            sum: 0,
            appended: 0,
        };
        if let Ok(text) = std::fs::read_to_string(&ledger.path) {
            for line in text.lines() {
                let mut parts = line.split(' ');
                if let (Some(at), Some(bytes), None) = (parts.next(), parts.next(), parts.next())
                    && let (Ok(at), Ok(bytes)) = (at.parse::<i64>(), bytes.parse::<u64>())
                {
                    ledger.entries.push_back((at, bytes));
                    ledger.sum = ledger.sum.saturating_add(bytes);
                }
            }
        }
        ledger.prune(now_ms);
        ledger
    }

    fn prune(&mut self, now_ms: i64) {
        while let Some((at, bytes)) = self.entries.front().copied() {
            if at > now_ms.saturating_sub(DAY_MS) {
                break;
            }
            self.entries.pop_front();
            self.sum = self.sum.saturating_sub(bytes);
        }
    }

    /// Bytes reserved in the last 24 hours.
    pub fn used(&mut self, now_ms: i64) -> u64 {
        self.prune(now_ms);
        self.sum
    }

    /// Reserve `bytes` against `cap`, recording it durably, or refuse.
    pub fn reserve(
        &mut self,
        now_ms: i64,
        bytes: u64,
        cap: u64,
    ) -> Result<Result<(), CapExhausted>, String> {
        self.prune(now_ms);
        if self.sum.saturating_add(bytes) > cap {
            return Ok(Err(CapExhausted {
                used: self.sum,
                cap,
            }));
        }
        self.append(now_ms, bytes)?;
        self.entries.push_back((now_ms, bytes));
        self.sum = self.sum.saturating_add(bytes);
        Ok(Ok(()))
    }

    fn append(&mut self, now_ms: i64, bytes: u64) -> Result<(), String> {
        use std::io::Write as _;
        if self.appended >= COMPACT_AFTER_APPENDS {
            let mut text = String::new();
            for (at, size) in &self.entries {
                text.push_str(&format!("{at} {size}\n"));
            }
            text.push_str(&format!("{now_ms} {bytes}\n"));
            lillux::atomic_write_with_mode(&self.path, text.as_bytes(), LEDGER_FILE_MODE)
                .map_err(|error| format!("compact the served-bytes journal: {error:?}"))?;
            self.appended = 0;
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            lillux::create_dir_exact_mode(parent, LEDGER_DIR_MODE).map_err(|error| {
                format!("create the served-bytes journal's directory: {error:#}")
            })?;
        }
        let mut file = lillux::open_append_exact_mode(&self.path, LEDGER_FILE_MODE)
            .map_err(|error| format!("open the served-bytes journal: {error:#}"))?;
        file.write_all(format!("{now_ms} {bytes}\n").as_bytes())
            .map_err(|error| format!("append to the served-bytes journal: {error}"))?;
        self.appended += 1;
        Ok(())
    }
}

type SharedLedger = Arc<Mutex<ByteLedger>>;

fn ledger_for(app_root: &Path, now_ms: i64) -> SharedLedger {
    static LEDGERS: OnceLock<Mutex<HashMap<PathBuf, SharedLedger>>> = OnceLock::new();
    let mut map = LEDGERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(app_root.to_path_buf())
        .or_insert_with(|| {
            Arc::new(Mutex::new(ByteLedger::load(
                served_ledger_path(app_root),
                now_ms,
            )))
        })
        .clone()
}

// ── The in-flight limit ─────────────────────────────────────────────

/// A node-wide limit on `/published` requests worked at once. The table sits
/// outside the bundle-driven route table, whose per-route semaphore therefore
/// does not cover it, and the outer router carries no concurrency layer.
fn limiter_for(app_root: &Path, size: u32) -> Arc<tokio::sync::Semaphore> {
    static LIMITERS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Semaphore>>>> =
        OnceLock::new();
    LIMITERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(app_root.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(size as usize)))
        .clone()
}

/// Take one in-flight slot or answer `publication_busy`.
fn take_slot(
    limiter: &Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit, Refused> {
    limiter.clone().try_acquire_owned().map_err(|_| {
        let mut refused = Refused::new(StatusCode::SERVICE_UNAVAILABLE, "publication_busy");
        refused.retry_after = true;
        refused
    })
}

// ── Derived manifests ───────────────────────────────────────────────

/// A frozen manifest read from the content store, verified against its hash,
/// with the closure derived from it.
#[derive(Debug)]
pub struct LoadedManifest {
    pub bytes: Vec<u8>,
    pub manifest: PublicationManifest,
    pub closure: Closure,
}

fn manifest_cache() -> &'static Mutex<HashMap<String, Arc<LoadedManifest>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<LoadedManifest>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Run `f` with the node's content store under the shared guard.
pub(crate) fn with_cas<T>(
    app: &AppState,
    f: impl FnOnce(&lillux::cas::CasStore) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let authority = app.state_store.with_state_db(|db| db.pinned_authority())?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    f(&cas)
}

/// The manifest object `manifest_hash` names, verified and derived. The
/// derivation depends only on the hash-checked bytes, so it is cached by hash.
pub(crate) fn load_manifest(
    app: &AppState,
    manifest_hash: &str,
) -> Result<Arc<LoadedManifest>, PublicationRefusal> {
    if let Some(found) = manifest_cache()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(manifest_hash)
    {
        return Ok(found.clone());
    }
    let bytes = with_cas(app, |cas| {
        cas.get_object_bytes_bounded(manifest_hash, MAX_PUBLICATION_MANIFEST_BYTES)
    })
    .map_err(|error| PublicationRefusal::Store(format!("{error:#}")))?
    .ok_or_else(|| PublicationRefusal::Store("the frozen manifest is not in the store".into()))?;
    let (manifest, closure) = PublicationManifest::from_object_bytes(manifest_hash, &bytes)?;
    let loaded = Arc::new(LoadedManifest {
        bytes,
        manifest,
        closure,
    });
    let mut cache = manifest_cache().lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= MAX_CACHED_MANIFESTS {
        cache.clear();
    }
    cache.insert(manifest_hash.to_owned(), loaded.clone());
    Ok(loaded)
}

// ── Answers ─────────────────────────────────────────────────────────

#[derive(Debug)]
struct Refused {
    status: StatusCode,
    code: &'static str,
    extra: Value,
    retry_after: bool,
}

impl Refused {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            extra: json!({}),
            retry_after: false,
        }
    }

    fn with(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }

    /// Anything that went wrong inside the node. The cause stays in the log;
    /// a stranger is told only that the publication is unavailable.
    fn unavailable(cause: impl std::fmt::Display) -> Self {
        tracing::error!(cause = %cause, "published read failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "publication_unavailable")
    }

    fn out_of_scope() -> Self {
        Self::new(StatusCode::NOT_FOUND, "object_not_in_scope")
    }

    fn from_refusal(refusal: PublicationRefusal) -> Self {
        match refusal {
            PublicationRefusal::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "publication_not_found")
            }
            PublicationRefusal::Retired { retired_at, reason } => {
                Self::new(StatusCode::GONE, "publication_retired")
                    .with(json!({ "retired_at": retired_at, "reason": reason.as_str() }))
            }
            PublicationRefusal::Expired => Self::new(StatusCode::GONE, "publication_expired"),
            PublicationRefusal::SignerNotOperator => {
                Self::new(StatusCode::FORBIDDEN, "publication_signer_not_operator")
            }
            other => Self::unavailable(other),
        }
    }

    fn into_response(self) -> Response {
        let mut body = json!({ "code": self.code });
        if let (Some(into), Some(from)) = (body.as_object_mut(), self.extra.as_object()) {
            into.extend(from.clone());
        }
        let mut response = (self.status, axum::Json(json!({ "error": body }))).into_response();
        if self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                header::HeaderValue::from_static("3600"),
            );
        }
        response
    }
}

struct Served {
    content_type: &'static str,
    bytes: Vec<u8>,
}

impl Served {
    fn json(value: &Value) -> Result<Self, Refused> {
        Ok(Self {
            content_type: "application/json",
            bytes: serde_json::to_vec(value).map_err(Refused::unavailable)?,
        })
    }
}

fn reserve(app: &AppState, bytes: u64) -> Result<(), Refused> {
    let cap = app
        .node_policy
        .require::<PublicationPolicy>()
        .map_err(Refused::unavailable)?
        .daily_bytes;
    let now_ms = lillux::time::timestamp_millis();
    let ledger = ledger_for(&app.config.app_root, now_ms);
    let outcome = ledger
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .reserve(now_ms, bytes, cap)
        .map_err(Refused::unavailable)?;
    match outcome {
        Ok(()) => Ok(()),
        Err(_) => {
            let mut refused =
                Refused::new(StatusCode::TOO_MANY_REQUESTS, "publication_quota_exhausted");
            refused.retry_after = true;
            Err(refused)
        }
    }
}

/// The live publication `id`, whose operator key is still active.
fn load_live(app: &AppState, id: &str) -> Result<StoredPublication, Refused> {
    let node_key = app.identity.signing_key().verifying_key();
    let state =
        load_publication(&app.config.app_root, &node_key, id).map_err(Refused::from_refusal)?;
    let stored = state
        .live(lillux::time::unix_secs_now())
        .map_err(Refused::from_refusal)?
        .clone();
    stored
        .check_operator(&app.engine.trust_store)
        .map_err(Refused::from_refusal)?;
    Ok(stored)
}

fn list(app: &AppState) -> Result<Served, Refused> {
    let node_key = app.identity.signing_key().verifying_key();
    let ledger =
        PublicationLedger::load(&app.config.app_root, &node_key).map_err(Refused::from_refusal)?;
    let now = lillux::time::unix_secs_now();
    let mut live: Vec<Value> = Vec::new();
    for stored in &ledger.publications {
        let state = ledger.state_of(&stored.record.id);
        if matches!(state.status(now), PublicationStatus::Live(_))
            && stored.check_operator(&app.engine.trust_store).is_ok()
        {
            live.push(json!({
                "id": stored.record.id,
                "bundle_name": stored.record.bundle_name,
                "version": stored.record.version,
                "not_after": stored.record.not_after,
            }));
        }
    }
    live.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let served = Served::json(&json!({ "publications": live }))?;
    reserve(app, served.bytes.len() as u64)?;
    Ok(served)
}

fn describe(app: &AppState, id: &str) -> Result<Served, Refused> {
    let stored = load_live(app, id)?;
    let loaded =
        load_manifest(app, &stored.record.root.manifest_hash).map_err(Refused::from_refusal)?;
    let served = stored.served().map_err(Refused::from_refusal)?;
    let mut body = serde_json::to_value(&served).map_err(Refused::unavailable)?;
    body["manifest"] = serde_json::to_value(&loaded.manifest).map_err(Refused::unavailable)?;
    let served = Served::json(&body)?;
    reserve(app, served.bytes.len() as u64)?;
    Ok(served)
}

fn blob(app: &AppState, id: &str, hash: &str) -> Result<Served, Refused> {
    let stored = load_live(app, id)?;
    let loaded =
        load_manifest(app, &stored.record.root.manifest_hash).map_err(Refused::from_refusal)?;
    // Membership comes first and is answered from memory: nothing about the
    // requested hash is looked up in the store until it is in the closure.
    if !is_publication_hash(hash) || !loaded.closure.contains(hash) {
        return Err(Refused::out_of_scope());
    }
    let limit = stored.record.limits.max_bytes_per_read;
    let size = match loaded.closure.blob_size(hash) {
        Some(size) => size,
        None => loaded.bytes.len() as u64,
    };
    if size > limit {
        return Err(
            Refused::new(StatusCode::PAYLOAD_TOO_LARGE, "publication_read_too_large")
                .with(json!({ "max_bytes_per_read": limit })),
        );
    }
    reserve(app, size)?;
    let bytes = if loaded.closure.is_manifest(hash) {
        loaded.bytes.clone()
    } else {
        with_cas(app, |cas| cas.get_blob_bounded(hash, size))
            .map_err(Refused::unavailable)?
            .ok_or_else(|| Refused::unavailable("a closure blob is missing from the store"))?
    };
    if bytes.len() as u64 != size {
        return Err(Refused::unavailable("a closure blob changed size"));
    }
    Ok(Served {
        content_type: "application/octet-stream",
        bytes,
    })
}

fn handle(
    app: &AppState,
    route: PublishedRoute,
    captures: &BTreeMap<&'static str, String>,
) -> Result<Served, Refused> {
    let id = captures.get("id").map(String::as_str).unwrap_or_default();
    if route != PublishedRoute::List && !is_publication_id(id) {
        return Err(Refused::new(StatusCode::NOT_FOUND, "publication_not_found"));
    }
    match route {
        PublishedRoute::List => list(app),
        PublishedRoute::Describe => describe(app, id),
        PublishedRoute::Blob => blob(
            app,
            id,
            captures.get("hash").map(String::as_str).unwrap_or_default(),
        ),
    }
}

/// Serve one `/published` request. The request is read for its method and
/// path only.
pub async fn serve(app: Arc<AppState>, request: Request) -> Response {
    // The slot is taken before anything else, so a flood of invalid ids or
    // refused requests is bounded like any other.
    let slots = match app.node_policy.require::<PublicationPolicy>() {
        Ok(policy) => limiter_for(&app.config.app_root, policy.max_in_flight),
        Err(error) => return Refused::unavailable(error).into_response(),
    };
    let slot = match take_slot(&slots) {
        Ok(slot) => slot,
        Err(refused) => return refused.into_response(),
    };
    if request.method() != Method::GET {
        let mut response =
            Refused::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed").into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, header::HeaderValue::from_static("GET"));
        return response;
    }
    let path = request.uri().path().to_owned();
    drop(request);
    let Some((route, captures)) = match_route(&path) else {
        return Refused::new(StatusCode::NOT_FOUND, "not_found").into_response();
    };
    let outcome = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        handle(&app, route, &captures)
    })
    .await;
    match outcome {
        Ok(Ok(served)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, served.content_type),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from(served.bytes),
        )
            .into_response(),
        Ok(Err(refused)) => refused.into_response(),
        Err(error) => Refused::unavailable(error).into_response(),
    }
}

#[cfg(test)]
mod tests;
