// ryeos-cli — CLI for Rye OS
//
// Public surface for integration tests.

pub mod arg_bind;
pub mod daemon_preflight;
pub mod effective_metadata;
pub mod error;
pub mod exec_stream;
pub mod exit;
pub mod help;
pub mod lifecycle_commands;
pub mod node_descriptors;
pub mod offline_dispatch;
pub mod presenter;
pub mod project_resolve;
#[cfg(test)]
pub(crate) mod test_env;
pub mod transport;
pub mod tty;
