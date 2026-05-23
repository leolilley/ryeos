// ryeos-cli — CLI for Rye OS
//
// Public surface for integration tests.

pub mod arg_bind;
pub mod error;
pub mod exit;
pub mod help;
pub mod offline_dispatch;
pub mod project_resolve;
#[cfg(test)]
pub(crate) mod test_env;
pub mod transport;
