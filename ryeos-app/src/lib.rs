// ryeos-app: shared application state and domain services.
//
// Phase B: extraction in progress. Modules are added as files are moved
// out of ryeosd. Each move keeps the workspace compiling.

pub mod callback_token;
pub mod command_service;
pub mod config;
pub mod event_store_service;
pub mod event_stream;
pub mod handler_context;
pub mod handler_error;
pub mod identity;
pub mod ignore;
#[path = "io/mod.rs"]
pub mod io;
pub mod kind_profiles;
pub mod launch_metadata;
pub mod node_config;
pub mod process;
pub mod project_discovery;
pub mod route_raw;
pub mod runtime_db;
pub mod service_registry;
pub mod standalone_audit;
pub mod state;
pub mod state_lock;
pub mod state_store;
pub mod thread_lifecycle;
pub mod vault;
pub mod write_barrier;
