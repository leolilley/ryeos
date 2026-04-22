//! Object sync protocol (Rust port).
//!
//! TODO: Implement cross-node sync protocol. See architecture doc
//! for protocol design (attestation/chain/permission model, object
//! discovery, bulk export/import). This is a stub with type definitions
//! only; implementation can come later.
//!
/// A single entry in a sync payload.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub hash: String,
    pub data: Vec<u8>,
}

/// Result of importing objects from a remote sync payload.
#[derive(Debug, Clone)]
pub struct ImportResult {
    pub imported: usize,
    pub already_present: usize,
}

/// Result of exporting objects for remote sync.
#[derive(Debug, Clone)]
pub struct ExportResult {
    pub exported: usize,
    pub total_bytes: usize,
}
