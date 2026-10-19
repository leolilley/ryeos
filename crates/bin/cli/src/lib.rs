// ryeos-cli — CLI for Rye OS
//
// Public surface for integration tests.

pub mod arg_bind;
mod bundle_set_update;
mod client_handlers;
pub mod daemon_preflight;
pub mod dispatcher;
pub mod effective_metadata;
pub mod error;
pub mod exec_stream;
pub mod exit;
pub mod help;
pub mod lifecycle_commands;
mod lifecycle_external_compose;
mod lifecycle_external_credential;
mod lifecycle_external_snapshot_compose;
pub mod node_descriptors;
pub mod offline_dispatch;
pub mod presenter;
pub mod project_resolve;
#[cfg(feature = "model-provider-setup")]
pub(crate) mod setup;
#[cfg(test)]
pub(crate) mod test_env;
pub(crate) mod thread_diagnostics;
pub mod transport;
pub mod tty;
