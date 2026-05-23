use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::mpsc;

use ryeos_engine::engine::Engine;
use ryeos_runtime::alias_registry::AliasRegistry;
use ryeos_runtime::authorizer::Authorizer;
use ryeos_runtime::verb_registry::VerbRegistry;
use ryeos_scheduler::db::SchedulerDb;
use ryeos_scheduler::ReloadSignal;

use crate::browser_session::BrowserSessionStore;
use crate::callback_token::{CallbackCapabilityStore, ThreadAuthStore};
use crate::session_bus::SessionBus;
use crate::command_service::CommandService;
use crate::config::Config;
use crate::engine_cache::EngineCache;
use crate::event_store_service::EventStoreService;
use crate::event_stream::ThreadEventHub;
use crate::identity::NodeIdentity;
use crate::ignore::IgnoreMatcher;
use crate::node_config::NodeConfigSnapshot;
use crate::service_registry::{ServiceDescriptor, ServiceRegistry};
use crate::state_store::StateStore;
use crate::thread_lifecycle::ThreadLifecycleService;
use crate::vault::NodeVault;
use crate::write_barrier::WriteBarrier;

#[derive(Debug, Clone, Serialize)]
pub struct CatalogHealth {
    pub status: String,
    pub missing_services: Vec<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub state_store: Arc<StateStore>,
    pub engine: Arc<Engine>,
    /// Per-snapshot engine cache used for `pushed_head` requests.
    /// The cache materialises user content + builds a per-request
    /// engine overlay, keyed by `(system_install_generation,
    /// snapshot_hash)`. `LiveFs` requests bypass this cache and
    /// use `engine` directly.
    pub engine_cache: EngineCache,
    pub identity: Arc<NodeIdentity>,
    pub threads: Arc<ThreadLifecycleService>,
    pub events: Arc<EventStoreService>,
    /// Per-thread live broadcast hub for SSE subscribers. Populated by
    /// the UDS callback handler after persistence so subscribers see
    /// the same `PersistedEventRecord` instances the event store
    /// recorded.
    pub event_streams: Arc<ThreadEventHub>,
    pub commands: Arc<CommandService>,
    pub callback_tokens: Arc<CallbackCapabilityStore>,
    pub thread_auth: Arc<ThreadAuthStore>,
    /// In-memory browser session store for `/ui` routes.
    /// Sessions are created via launch tokens and validated by the
    /// `browser_session` auth invoker.
    pub browser_sessions: Arc<BrowserSessionStore>,
    /// Per-session event bus for `/ui/events/session/{id}` SSE streams.
    /// Producers publish domain events; the session_events route subscribes.
    pub session_bus: Arc<SessionBus>,
    pub write_barrier: Arc<WriteBarrier>,
    pub started_at: Instant,
    pub started_at_iso: String,
    /// Result of the operational tool catalog self-check at startup.
    pub catalog_health: CatalogHealth,
    /// Service handler registry for in-process `kind: service` dispatch.
    pub services: Arc<ServiceRegistry>,
    /// Catalog of all known service descriptors. Source of truth for
    /// per-endpoint availability lookups. Populated at startup from the
    /// daemon's `services::handlers::ALL` static table.
    pub service_descriptors: &'static [ServiceDescriptor],
    /// Node-config snapshot loaded at startup.
    pub node_config: Arc<NodeConfigSnapshot>,
    /// Operator-secret store. Read at request-build time and merged
    /// into the spawned subprocess env via the `vault_bindings`
    /// pipeline (see `thread_lifecycle::spawn_item`). The daemon stays
    /// vendor-agnostic — this trait moves opaque `String -> String`
    /// pairs and never enumerates provider names.
    pub vault: Arc<dyn NodeVault>,
    /// Verb registry for capability checking. Built once at startup
    /// from node-config verb YAMLs.
    pub verb_registry: Arc<VerbRegistry>,
    /// Alias registry for token routing. Built once at startup
    /// from node-config alias YAMLs.
    pub alias_registry: Arc<AliasRegistry>,
    /// Unified capability evaluator. Built once at startup from `verb_registry`.
    /// All enforcement sites use this shared instance instead of constructing
    /// per-request.
    pub authorizer: Arc<Authorizer>,
    /// Scheduler projection DB (SQLite, in-memory for tests, file-backed in prod).
    pub scheduler_db: Arc<SchedulerDb>,
    /// Channel to request scheduler reload after register/deregister/pause/resume.
    /// `None` when the scheduler is not running (e.g. in unit tests).
    pub scheduler_reload_tx: Option<mpsc::Sender<ReloadSignal>>,
    /// Ingest ignore matcher. Loaded once from `.ai/node/ingest/ignore.yaml`
    /// at startup. Used by ingest_walk, walk_and_diff, and push-head
    /// validation.
    pub ignore_matcher: Arc<IgnoreMatcher>,
    /// Vault X25519 public key fingerprint. `None` when using
    /// `EmptyVault` or when the vault public key file doesn't exist.
    /// Populated at startup from `<system>/.ai/node/vault/public_key.pem`.
    pub vault_fingerprint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub version: String,
    pub started_at: String,
    pub uptime_seconds: u64,
    pub bind: String,
    pub uds_path: String,
    pub db_path: String,
    pub active_threads: i64,
}

impl AppState {
    pub fn status(&self) -> StatusResponse {
        StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: self.started_at_iso.clone(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            bind: self.config.bind.to_string(),
            uds_path: self.config.uds_path.display().to_string(),
            db_path: self.config.db_path.display().to_string(),
            active_threads: self.state_store.active_thread_count().unwrap_or(0),
        }
    }
}
