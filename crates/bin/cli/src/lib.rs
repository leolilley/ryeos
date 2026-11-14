// ryeos-cli — CLI for Rye OS
//
// Public surface for integration tests.

pub mod arg_bind;
mod bundle_set_update;
mod client_handlers;
mod codex_device_login;
pub mod daemon_preflight;
pub mod dispatcher;
mod doctor_external_status;
pub mod effective_metadata;
pub mod error;
pub mod exec_stream;
pub mod exit;
pub mod help;
mod lifecycle_bundle_state;
pub mod lifecycle_commands;
mod lifecycle_external_compose;
mod lifecycle_external_credential;
mod lifecycle_external_qualification_compose;
mod lifecycle_external_recompose;
mod lifecycle_external_snapshot_compose;
mod lifecycle_host_access;
mod lifecycle_key_rotation;
mod lifecycle_offline_operation;
mod lifecycle_trust;
pub mod node_descriptors;
pub mod offline_apply;
pub mod offline_dispatch;
pub mod presenter;
mod product_publish;
pub mod project_resolve;
mod runtime_snapshot_run;
#[cfg(feature = "model-provider-setup")]
pub(crate) mod setup;
#[cfg(test)]
pub(crate) mod test_env;
pub(crate) mod thread_diagnostics;
pub mod transport;
pub mod tty;
