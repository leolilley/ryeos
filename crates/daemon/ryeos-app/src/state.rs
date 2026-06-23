use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{mpsc, RwLock};

use ryeos_engine::engine::Engine;
use ryeos_runtime::authorizer::Authorizer;
use ryeos_runtime::CommandRegistry;
use ryeos_scheduler::db::SchedulerDb;
use ryeos_scheduler::ReloadSignal;

use crate::callback_token::{CallbackCapabilityStore, ThreadAuthStore};
use crate::command_service::CommandService;
use crate::config::Config;
use crate::engine_cache::EngineCache;
use crate::event_store_service::EventStoreService;
use crate::event_stream::ThreadEventHub;
use crate::extension_state::ExtensionState;
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
    /// Generic extension state bag for composition-root state that
    /// doesn't belong in core (e.g., UI state). Populated by the
    /// daemon composition root. Use `extensions.get::<T>()` to
    /// retrieve typed state.
    pub extensions: Arc<ExtensionState>,
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
    /// Command registry for token routing. Built once at startup
    /// from node-config command YAMLs.
    pub command_registry: Arc<CommandRegistry>,
    /// Unified capability evaluator. Built once at startup.
    /// All enforcement sites use this shared instance instead of constructing
    /// per-request.
    pub authorizer: Arc<Authorizer>,
    /// Scheduler projection DB (SQLite, in-memory for tests, file-backed in prod).
    pub scheduler_db: Arc<SchedulerDb>,
    /// Runtime gate shared by scheduler mutation paths and the timer.
    /// Writers hold it across project/schedule mutations; the timer only
    /// dispatches while it can acquire a read guard.
    pub scheduler_runtime_gate: Arc<RwLock<()>>,
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
    /// Git revision the binary was built from. Injected at build time
    /// (`RYEOS_VCS_REF`) or derived from the local git short SHA; `"unknown"`
    /// when neither is available. Lets an operator answer "which code is
    /// actually running?" — the version string alone can be stale/cosmetic.
    pub revision: String,
    /// Build timestamp injected at build time (`RYEOS_BUILD_DATE`), or
    /// `"unknown"` for local dev builds.
    pub build_date: String,
    pub started_at: String,
    pub uptime_seconds: u64,
    pub bind: String,
    pub uds_path: String,
    pub db_path: String,
    pub active_threads: i64,
}

impl AppState {
    /// CAS store rooted at this node's state. Shared shorthand for the
    /// `cas_root()? -> CasStore::new` pair repeated across handlers.
    pub fn cas_store(&self) -> anyhow::Result<lillux::cas::CasStore> {
        Ok(lillux::cas::CasStore::new(self.state_store.cas_root()?))
    }

    /// CAS store plus the refs root, for handlers that read or advance
    /// signed refs alongside object access.
    pub fn cas_and_refs(&self) -> anyhow::Result<(lillux::cas::CasStore, std::path::PathBuf)> {
        Ok((
            lillux::cas::CasStore::new(self.state_store.cas_root()?),
            self.state_store.refs_root()?,
        ))
    }

    pub fn status(&self) -> StatusResponse {
        StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            revision: env!("RYEOS_VCS_REF").to_string(),
            build_date: env!("RYEOS_BUILD_DATE").to_string(),
            started_at: self.started_at_iso.clone(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            bind: self.config.bind.to_string(),
            uds_path: self.config.uds_path.display().to_string(),
            db_path: self.config.db_path.display().to_string(),
            active_threads: self.state_store.active_thread_count().unwrap_or(0),
        }
    }
}
