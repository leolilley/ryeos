//! The execution tasks of in-flight UDS requests.
//!
//! A request runs on its own task so that a forced abort of the connection
//! task cannot cut the request off halfway (the connection's `JoinHandle` is
//! then dropped, which detaches the task). A detached task still holds the
//! application state, and with it a clone of the live Store, until the request
//! returns. Shutdown must therefore join these tasks before it closes the
//! scheduler database and the live Store; a bounded wait or a retry would only
//! hide that they are still alive.
//!
//! The tracker admits a task only while open, and counts it until the task's
//! future, including everything it captured, has been dropped.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

#[derive(Default)]
struct Counts {
    closed: bool,
    live: usize,
}

#[derive(Default)]
struct Inner {
    counts: Mutex<Counts>,
    idle: Notify,
}

#[derive(Clone, Default)]
pub struct RequestOwners {
    inner: Arc<Inner>,
}

/// A request arrived after shutdown closed the tracker.
#[derive(Debug)]
pub struct RequestOwnersClosed;

impl std::fmt::Display for RequestOwnersClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the daemon is shutting down and admits no new request")
    }
}

impl std::error::Error for RequestOwnersClosed {}

/// Decrements the live count when dropped. It is declared before the future
/// inside the wrapper, so it drops after the future and its captures.
struct Member(Arc<Inner>);

impl Drop for Member {
    fn drop(&mut self) {
        let mut counts = self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        counts.live = counts.live.saturating_sub(1);
        if counts.live == 0 {
            self.0.idle.notify_waiters();
        }
    }
}

impl RequestOwners {
    pub fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, RequestOwnersClosed>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        {
            let mut counts = self.inner.counts.lock().unwrap_or_else(|p| p.into_inner());
            if counts.closed {
                return Err(RequestOwnersClosed);
            }
            counts.live += 1;
        }
        let member = Member(Arc::clone(&self.inner));
        Ok(tokio::spawn(async move {
            let _member = member;
            future.await
        }))
    }

    /// Close admission and wait until every admitted task has finished and
    /// dropped what it held. Returns how many were still running at the
    /// deadline (zero when all were joined).
    pub async fn close_and_join(&self, deadline: Duration) -> usize {
        self.inner
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed = true;
        let wait = async {
            loop {
                let notified = self.inner.idle.notified();
                tokio::pin!(notified);
                // Register before reading the count so a task that finishes
                // between the read and the wait still wakes this loop.
                notified.as_mut().enable();
                if self.live() == 0 {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(deadline, wait).await;
        self.live()
    }

    pub fn live(&self) -> usize {
        self.inner
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .live
    }
}

#[cfg(test)]
#[path = "../../../../../../tests/support/rust_source_text.rs"]
mod rust_source_text;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn close_and_join_waits_for_a_task_and_for_what_it_captured() {
        let owners = RequestOwners::default();
        let held = Arc::new(());
        let (release, gate) = oneshot::channel::<()>();
        let captured = Arc::clone(&held);
        let handle = owners
            .spawn(async move {
                let _captured = captured;
                let _ = gate.await;
            })
            .expect("open tracker admits");
        assert_eq!(owners.live(), 1);

        let join = owners.close_and_join(Duration::from_secs(30));
        tokio::pin!(join);
        tokio::select! {
            biased;
            _ = join.as_mut() => panic!("the join must not finish while the task is parked"),
            _ = std::future::ready(()) => {}
        }

        release.send(()).expect("task is waiting");
        assert_eq!(join.await, 0);
        handle.await.expect("task finished");
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "the captured clone is gone once the join returns"
        );
    }

    #[tokio::test]
    async fn a_closed_tracker_refuses_new_tasks() {
        let owners = RequestOwners::default();
        assert_eq!(owners.close_and_join(Duration::from_secs(1)).await, 0);
        assert!(owners.spawn(async {}).is_err());
    }

    #[tokio::test]
    async fn the_deadline_reports_the_tasks_still_running() {
        let owners = RequestOwners::default();
        let (_release, gate) = oneshot::channel::<()>();
        let _handle = owners
            .spawn(async move {
                let _ = gate.await;
            })
            .expect("open tracker admits");
        assert_eq!(owners.close_and_join(Duration::from_millis(50)).await, 1);
    }

    #[test]
    fn shutdown_joins_the_request_owners_before_the_scheduler_database_closes() {
        let main =
            super::rust_source_text::production_text("daemon main", include_str!("../../main.rs"));
        let join = main
            .find("join_request_owners(")
            .expect("shutdown joins the request owners");
        // An earlier use of the close belongs to another path; the one that
        // matters follows the join.
        assert!(
            main[join..].contains("close_after_shutdown_drain("),
            "the scheduler database close must follow the join of the request owners"
        );
    }

    #[test]
    fn the_transport_runs_every_request_owner_through_the_tracker() {
        let transport =
            super::rust_source_text::production_text("uds transport", include_str!("transport.rs"));
        // rustfmt may break the call across lines.
        let compact: String = transport.split_whitespace().collect();
        assert!(
            compact.contains("request_owners().spawn("),
            "a request owner is spawned through the tracker"
        );
        assert!(
            !transport.contains("tokio::spawn("),
            "a bare spawn would detach a request owner from shutdown's join"
        );
    }
}
