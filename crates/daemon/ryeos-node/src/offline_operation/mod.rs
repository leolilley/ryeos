//! Signed offline operations: changes to a stopped node, authorized by a
//! signature instead of a shell.
//!
//! A request names one operation from a table compiled into the binary, the
//! node it is for, the facts about that node it was signed against, the
//! digests of its inputs and of its expected result. It is applied before any
//! other startup check can refuse, in the order of the ledger head it pins,
//! and every application leaves a node-signed receipt in a hash-chained
//! ledger that no handler and no reset can write.
//!
//! What this does and does not protect: a signed request is refused unless a
//! valid signer authorized it, so an attacker who controls only the process
//! environment or the inbox gains nothing. Anyone who can write the node's
//! volume can still sign as the local operator, because the key sits on the
//! volume; the ledger makes that visible against an earlier chain tip, and
//! does not prevent it.
//!
//! Output artifacts the node trusts are signed, as always, by the keys on the
//! volume: the local operator key where an operator signs, and the node key
//! where the node signs. A remote signer authorizes an operation; it never
//! signs what the operation produces. Receipts are signed with the node key.

pub mod apply;
pub mod contract;
mod envelope;
pub mod facts;
pub mod handlers;
pub mod host;
pub mod seed;
mod store;
pub mod table;

pub use apply::{
    ApplyOptions, ApplyReport, LocalApplied, LocalDraft, StopReason, apply_local,
    apply_pending_offline_operations, apply_with_options, read_facts, read_ledger,
};
pub use envelope::{WildcardGrant, authorized_keys_dir, local_operator};
pub use host::{BareHost, ImageSeed, OfflineHost, TypedReport};
