use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use rye_engine::engine::Engine;

use crate::config::Config;
use crate::execution::callback_token::CallbackCapabilityStore;
use crate::identity::NodeIdentity;
use crate::state_store::StateStore;
use crate::services::command_service::CommandService;
use crate::services::event_store::EventStoreService;
use crate::services::thread_lifecycle::ThreadLifecycleService;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub state_store: Arc<StateStore>,
    pub engine: Arc<Engine>,
    pub identity: Arc<NodeIdentity>,
    pub threads: Arc<ThreadLifecycleService>,
    pub events: Arc<EventStoreService>,
    pub commands: Arc<CommandService>,
    pub callback_tokens: Arc<CallbackCapabilityStore>,
    pub started_at: Instant,
    pub started_at_iso: String,
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
