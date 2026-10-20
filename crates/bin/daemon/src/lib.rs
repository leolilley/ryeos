// Public library interface for ryeosd
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod accounting_outbox;
pub mod accounting_terminal;
pub mod bootstrap;
pub mod config;
pub mod engine_init;
pub mod reconcile;
#[cfg(feature = "scheduling")]
pub mod scheduler_impl;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod uds;

// ── Extracted crates ─────────────────────────────────────────────
#[cfg(feature = "scheduling")]
pub use ryeos_scheduler as scheduler;

static SHUTDOWN_TX: OnceCell<tokio::sync::broadcast::Sender<()>> = OnceCell::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn init_shutdown_channel() {
    SHUTDOWN_REQUESTED.store(false, Ordering::Release);
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(4);
    let _ = SHUTDOWN_TX.set(shutdown_tx);
}

pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::Release);
    if let Some(tx) = SHUTDOWN_TX.get() {
        let _ = tx.send(());
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Acquire)
}

static SERVICE_SQL_REFUSED: AtomicBool = AtomicBool::new(false);

/// Record that this incarnation's service SQL is refused. Only an atomic
/// store, so it is safe from the native latch wake. Never cleared.
pub fn refuse_service_sql() {
    SERVICE_SQL_REFUSED.store(true, Ordering::SeqCst);
}

/// Whether this incarnation's service SQL is refused. Service loops stop
/// before attempting another write once it is.
pub fn service_sql_refused() -> bool {
    SERVICE_SQL_REFUSED.load(Ordering::SeqCst)
}

pub fn subscribe_shutdown() -> Option<tokio::sync::broadcast::Receiver<()>> {
    SHUTDOWN_TX.get().map(|tx| tx.subscribe())
}
