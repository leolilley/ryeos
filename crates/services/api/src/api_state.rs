//! Daemon-only state wrapper.
//!
//! Combines the cross-cutting `AppState` (shared with future crates such as
//! `ryeos-executor` and `ryeos-api`) with daemon-private bookkeeping that
//! only the HTTP edge needs (the compiled route table and the webhook
//! delivery-id dedupe store).
//!
//! Axum router state is `ApiState`; per-route invokers receive the
//! inner `AppState` (cloned out of `app`) plus, where they need it, the
//! webhook dedupe store via the invocation context.
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::routes::webhook_dedupe::WebhookDedupeStore;
use crate::routes::RouteTable;
use ryeos_app::state::AppState;

#[derive(Clone)]
pub struct ApiState {
    pub app: Arc<AppState>,
    pub route_table: Arc<ArcSwap<RouteTable>>,
    pub webhook_dedupe: Arc<WebhookDedupeStore>,
}
