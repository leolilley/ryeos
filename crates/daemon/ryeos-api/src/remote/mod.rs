//! Remote node operations.
//!
//! Outbound HTTP client, remotes config, and CAS push pipeline for
//! communicating with remote ryEOS nodes. Used by the daemon's service
//! handlers (`remote_configure`, `remote_list`, etc.).

pub mod client;
pub mod config;
pub mod forward;
pub mod import;
pub mod pull;
pub mod push;
