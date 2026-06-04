// Public library interface for ryeosd
use once_cell::sync::OnceCell;

pub mod bootstrap;
pub mod config;
pub mod engine_init;
pub mod reconcile;
pub mod scheduler_impl;
pub mod uds;

// ── Extracted crates ─────────────────────────────────────────────
pub use ryeos_scheduler as scheduler;

static SHUTDOWN_TX: OnceCell<tokio::sync::broadcast::Sender<()>> = OnceCell::new();

pub fn init_shutdown_channel() {
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(4);
    let _ = SHUTDOWN_TX.set(shutdown_tx);
}

pub fn request_shutdown() {
    if let Some(tx) = SHUTDOWN_TX.get() {
        let _ = tx.send(());
    }
}

pub fn subscribe_shutdown() -> Option<tokio::sync::broadcast::Receiver<()>> {
    SHUTDOWN_TX.get().map(|tx| tx.subscribe())
}
