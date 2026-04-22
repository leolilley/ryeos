//! rye-state — CAS-as-truth persistence plane for RyeOS.
//!
//! This crate owns the answer to "what happened and how to prove it."
//! It provides CAS object type definitions, chain operations, signed ref
//! management, and will eventually include SQLite projection and sync.
//!
//! The crate does NOT own the signing key (daemon passes a [`Signer`] trait),
//! item resolution (engine), or low-level CAS primitives (lillux).

pub mod chain;
pub mod gc;
pub mod head_cache;
pub mod locators;
pub mod objects;
pub mod projection;
pub mod queries;
pub mod reachability;
pub mod rebuild;
pub mod refs;
pub mod signer;
pub mod state_db;
pub mod sync;
pub mod verify;

pub use chain::{AppendResult, CreateResult, ReadSnapshotResult, SnapshotUpdate};
pub use head_cache::{CachedHead, HeadCache};
pub use locators::ThreadLocator;
pub use objects::{thread_event::ThreadEvent, thread_snapshot::ThreadSnapshot, ChainState};
pub use projection::ProjectionDb;
pub use refs::{SignedRef, TrustStore};
pub use signer::Signer;
pub use state_db::StateDb;
