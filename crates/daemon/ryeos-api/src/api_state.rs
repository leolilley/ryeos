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

use crate::routes::RouteTable;
use crate::routes::webhook_dedupe::WebhookDedupeStore;
use ryeos_app::state::AppState;

#[derive(Clone)]
pub struct ApiState {
    /// Immutable application and Executor generation published as one unit.
    /// `None` is reserved for isolated ordinary-mode fixtures; daemon
    /// publication requires this to be present.
    pub publication: Option<Arc<ryeos_executor::SelectedApplicationPublication>>,
    pub app: Arc<AppState>,
    pub route_table: Arc<ArcSwap<RouteTable>>,
    pub webhook_dedupe: Arc<WebhookDedupeStore>,
}

#[cfg(feature = "scheduling")]
pub(crate) fn selected_scheduler(
    app: &AppState,
) -> anyhow::Result<Arc<ryeos_scheduler_service::SchedulerService>> {
    app.extensions
        .get::<ryeos_scheduler_service::SchedulerService>()
        .ok_or_else(|| anyhow::anyhow!("selected scheduler service is unavailable"))
}
