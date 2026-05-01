// Public library interface for ryeosd
pub mod api;
pub mod auth;
pub mod bootstrap;
pub mod config;
pub mod dispatch;
pub mod dispatch_error;
pub mod dispatch_role;
pub mod engine_init;
pub mod event_stream;
pub mod execution;
pub mod identity;
#[path = "io/mod.rs"]
pub mod io;
pub mod kind_profiles;
pub mod launch_metadata;
pub mod maintenance;
pub mod node_config;
pub mod policy;
pub mod process;
pub mod reconcile;
pub mod routes;
pub mod runtime_db;
pub mod service_executor;
pub mod service_registry;
pub mod services;
pub mod standalone_audit;
pub mod state;
pub mod state_lock;
pub mod state_store;
pub mod uds;
pub mod vault;
pub mod write_barrier;

// ── Service descriptor surface ──────────────────────────────────────
//
// `ServiceDescriptor` (declared per-handler module, collected in
// `services::handlers::ALL`) is the single source of truth for daemon-
// supported services. Re-exported here so integration tests can iterate
// the descriptor table directly without a parallel hand-maintained
// catalog.
pub use crate::service_executor::ServiceAvailability;
pub use crate::service_registry::{RawHandlerFn, ServiceDescriptor};

/// Re-export of the per-endpoint service handler modules, including
/// the canonical `ALL: &[ServiceDescriptor]` list.
pub use crate::services::handlers as service_handlers;
