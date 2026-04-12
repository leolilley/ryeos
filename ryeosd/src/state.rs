use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use rye_engine::engine::Engine;

use crate::broker::{LiveBroker, LiveBrokerStatus};
use crate::cas::CasStore;
use crate::config::Config;
use crate::db::Database;
use crate::identity::NodeIdentity;
use crate::refs::RefStore;
use crate::registry::RegistryStore;
use crate::services::budget_service::BudgetService;
use crate::services::command_service::CommandService;
use crate::services::event_store::EventStoreService;
use crate::services::thread_lifecycle::ThreadLifecycleService;
use crate::vault::VaultStore;
use crate::webhooks::WebhookStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Database>,
    pub engine: Arc<Engine>,
    pub identity: Arc<NodeIdentity>,
    pub threads: Arc<ThreadLifecycleService>,
    pub events: Arc<EventStoreService>,
    pub broker: Arc<LiveBroker>,
    pub commands: Arc<CommandService>,
    pub budgets: Arc<BudgetService>,
    pub cas: Arc<CasStore>,
    pub refs: Arc<RefStore>,
    pub registry: Arc<RegistryStore>,
    pub vault: Arc<VaultStore>,
    pub webhooks: Arc<WebhookStore>,
    pub started_at: Instant,
    pub started_at_iso: String,
}

impl AppState {
    pub fn cas_store(&self) -> &CasStore {
        &self.cas
    }

    pub fn refs_store(&self) -> &RefStore {
        &self.refs
    }

    pub fn vault_store(&self) -> &VaultStore {
        &self.vault
    }

    pub fn registry_store(&self) -> &RegistryStore {
        &self.registry
    }

    pub fn webhook_store(&self) -> &WebhookStore {
        &self.webhooks
    }
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
    pub broker: LiveBrokerStatus,
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
            active_threads: self.db.active_thread_count().unwrap_or(0),
            broker: self.broker.status(),
        }
    }
}
