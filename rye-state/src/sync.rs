//! Object sync protocol (Rust port).
//!
//! TODO(Phase 0.5F): Implement sync protocol.
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
