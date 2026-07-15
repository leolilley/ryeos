// Public library interface for ryeosd
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod bootstrap;
pub mod config;
pub mod engine_init;
pub mod reconcile;
pub mod scheduler_impl;
pub mod uds;

// ── Extracted crates ─────────────────────────────────────────────
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

pub fn subscribe_shutdown() -> Option<tokio::sync::broadcast::Receiver<()>> {
    SHUTDOWN_TX.get().map(|tx| tx.subscribe())
}
