//! Bootstrap — phased startup loading.

use ryeos_tui_core::model::AppModel;
use ryeos_tui_core::update::{self, AppEvent};

use crate::mock_transport;
use crate::transport::DaemonTransport;

/// Bootstrap result.
#[derive(Debug)]
pub struct BootstrapResult {
    pub daemon_reachable: bool,
    pub identity_available: bool,
    pub threads_loaded: usize,
    pub remotes_loaded: usize,
}

/// Phase 1 bootstrap: load blocking essentials.
pub async fn blocking_essentials(
    model: &mut AppModel,
    transport: &mut Box<dyn DaemonTransport>,
) -> BootstrapResult {
    match transport.poll_snapshot().await {
        Ok(snapshot) => {
            let thread_count = snapshot.threads.len();
            let remote_count = snapshot.remotes.len();
            let daemon_alive = snapshot.daemon_alive;
            update::update(model, AppEvent::PollSnapshot(snapshot));

            // If using mock transport, inject demo thread events
            if transport.as_ref().name() == "mock" {
                let events = mock_transport::mock_thread_events();
                update::update(model, AppEvent::DaemonBatch(events));
            }

            BootstrapResult {
                daemon_reachable: daemon_alive,
                identity_available: true, // TODO: check from transport
                threads_loaded: thread_count,
                remotes_loaded: remote_count,
            }
        }
        Err(_) => BootstrapResult {
            daemon_reachable: false,
            identity_available: false,
            threads_loaded: 0,
            remotes_loaded: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockTransport;

    #[tokio::test]
    async fn bootstrap_partial_state_renders_without_remotes() {
        let mut model = AppModel::new_default("/tmp/test");
        let mut transport: Box<dyn DaemonTransport> = Box::new(MockTransport);

        let result = blocking_essentials(&mut model, &mut transport).await;

        assert!(result.daemon_reachable);
        assert_eq!(result.threads_loaded, 3);
        assert_eq!(result.remotes_loaded, 2);
    }
}
