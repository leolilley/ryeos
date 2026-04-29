use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use serde::Serialize;

use ryeos_engine::engine::Engine;

use crate::config::Config;
use crate::event_stream::ThreadEventHub;
use crate::execution::callback_token::CallbackCapabilityStore;
use crate::identity::NodeIdentity;
use crate::node_config::NodeConfigSnapshot;
use crate::routes::RouteTable;
use crate::service_registry::ServiceRegistry;
use crate::state_store::StateStore;
use crate::services::command_service::CommandService;
use crate::services::event_store::EventStoreService;
use crate::services::thread_lifecycle::ThreadLifecycleService;
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
    pub write_barrier: Arc<WriteBarrier>,
    pub started_at: Instant,
    pub started_at_iso: String,
    /// Result of the operational tool catalog self-check at startup.
    pub catalog_health: CatalogHealth,
    /// Service handler registry for in-process `kind: service` dispatch.
    pub services: Arc<ServiceRegistry>,
    /// Node-config snapshot loaded at startup.
    pub node_config: Arc<NodeConfigSnapshot>,
    /// Compiled route table (hot-swapped on UDS reload).
    pub route_table: Arc<ArcSwap<RouteTable>>,
    /// Process-wide webhook delivery-id dedupe store. Configured per
    /// route by the `hmac` verifier; constructed once at startup.
    pub webhook_dedupe: Arc<crate::routes::webhook_dedupe::WebhookDedupeStore>,
    /// Operator-secret store. Read at request-build time and merged
    /// into the spawned subprocess env via the `vault_bindings`
    /// pipeline (see `services::thread_lifecycle::spawn_item`). The
    /// daemon stays vendor-agnostic — this trait moves opaque
    /// `String -> String` pairs and never enumerates provider names.
    pub vault: Arc<dyn crate::vault::NodeVault>,
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
