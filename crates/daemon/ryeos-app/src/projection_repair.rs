use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::state_store::StateStore;

pub async fn run(state_store: Arc<StateStore>, shutdown: impl Future<Output = ()>) -> Result<()> {
    tokio::pin!(shutdown);
    let health = state_store.projection_health();
    let mut retry_delay = Duration::from_secs(1);

    loop {
        if health.is_current() {
            tokio::select! {
                _ = health.notified() => {}
                _ = &mut shutdown => return Ok(()),
            }
        }

        let Some(generation) = health.begin_repair() else {
            continue;
        };
        let store = state_store.clone();
        let repaired = tokio::task::spawn_blocking(move || store.repair_thread_projection())
            .await
            .map_err(|error| anyhow::anyhow!("projection repair task failed: {error}"))
            .and_then(|result| result);
        health.finish_repair(generation, &repaired);

        if repaired.is_ok() {
            retry_delay = Duration::from_secs(1);
            continue;
        }
        tracing::warn!(
            error = %repaired.unwrap_err(),
            retry_seconds = retry_delay.as_secs(),
            "thread projection repair failed"
        );
        tokio::select! {
            _ = tokio::time::sleep(retry_delay) => {}
            _ = health.notified() => {}
            _ = &mut shutdown => return Ok(()),
        }
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}
