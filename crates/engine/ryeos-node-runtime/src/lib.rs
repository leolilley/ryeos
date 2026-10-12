//! Shared configuration, identity, document and exclusion owners.
//! Domain services, composition policy and scheduling retain their own callers.

pub mod config;
pub mod identity;
pub mod node_document;
pub mod state_lock;
