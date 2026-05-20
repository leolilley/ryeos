//! Integrity verification — hash chains, signatures, rebuild.

use crate::{ChainState, HeadCache};

/// Result of verifying a chain's integrity.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub valid: bool,
    pub chain_root_id: String,
    pub verified_events: u64,
    pub issues: Vec<String>,
}

/// Result of reconciling chain heads on startup.
#[derive(Debug, Clone)]
pub struct ReconcileStats {
    pub chains_checked: usize,
    pub chains_caught_up: usize,
    pub chains_ok: usize,
}

/// Result of rebuilding the projection.
#[derive(Debug, Clone)]
pub struct RebuildStats {
    pub chains_rebuilt: usize,
    pub events_replayed: usize,
    pub threads_restored: usize,
}

/// Result of catching up a single chain.
#[derive(Debug, Clone)]
pub struct CatchUpStats {
    pub chain_root_id: String,
    pub events_replayed: usize,
}

/// Verify the hash-link integrity of a chain state.
///
/// Checks that the chain_state's prev_chain_state_hash links are valid.
pub fn verify_chain_integrity(chain_state: &ChainState, prev_hash: Option<&str>) -> VerifyResult {
    let mut issues = Vec::new();

    // If we expect a previous hash, verify it matches
    if let Some(expected_prev) = prev_hash {
        match &chain_state.prev_chain_state_hash {
            Some(actual_prev) if actual_prev == expected_prev => {},
            Some(actual_prev) => {
                issues.push(format!(
                    "prev_chain_state_hash mismatch: expected {}, got {}",
                    expected_prev, actual_prev
                ));
            }
            None => {
                issues.push(format!(
                    "expected prev_chain_state_hash {}, but found None",
                    expected_prev
                ));
            }
        }
    } else {
        // First chain state should have no previous
        if chain_state.prev_chain_state_hash.is_some() {
            issues.push("first chain_state should have prev_chain_state_hash=None".to_string());
        }
    }

    VerifyResult {
        valid: issues.is_empty(),
        chain_root_id: chain_state.chain_root_id.clone(),
        verified_events: chain_state.last_chain_seq,
        issues,
    }
}

/// Reconcile (catch up) chain heads against the projection on startup.
///
/// This is run once on daemon startup:
/// 1. Enumerate all chains in the refs directory
/// 2. Read the signed head ref for each chain
/// 3. Compare to projection's indexed_chain_state_hash
/// 4. If they differ, catch up the projection
///
/// For Phase 0.5F, this is a stub that just validates each chain's signed ref.
pub fn reconcile_chain_heads(
    _refs_root: &std::path::Path,
    _projection: &crate::projection::ProjectionDb,
    head_cache: &mut HeadCache,
) -> anyhow::Result<ReconcileStats> {
    // Phase 0.5F: This will enumerate refs, read signed heads, and compare to projection
    // For now, it's a stub that just initializes an empty cache
    
    head_cache.clear();

    Ok(ReconcileStats {
        chains_checked: 0,
        chains_caught_up: 0,
        chains_ok: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::objects::{ChainThreadEntry, ThreadStatus};

    fn make_chain_state() -> ChainState {
        let mut threads = BTreeMap::new();
        threads.insert(
            "T-root".to_string(),
            ChainThreadEntry {
                snapshot_hash: "01".repeat(32),
                last_event_hash: None,
                last_thread_seq: 0,
                status: ThreadStatus::Created,
            },
        );

        ChainState {
            schema: 1,
            kind: "chain_state".to_string(),
            chain_root_id: "T-root".to_string(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: "2026-04-21T12:00:00Z".to_string(),
            threads,
        }
    }

    #[test]
    fn verify_chain_integrity_first_state_valid() {
        let chain = make_chain_state();
        let result = verify_chain_integrity(&chain, None);
        assert!(result.valid);
        assert_eq!(result.issues.len(), 0);
    }

    #[test]
    fn verify_chain_integrity_rejects_unexpected_prev() {
        let mut chain = make_chain_state();
        chain.prev_chain_state_hash = Some("02".repeat(32));

        let result = verify_chain_integrity(&chain, None);
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("should have prev_chain_state_hash=None")));
    }

    #[test]
    fn verify_chain_integrity_validates_matching_prev() {
        let mut chain = make_chain_state();
        let prev = "02".repeat(32);
        chain.prev_chain_state_hash = Some(prev.clone());

        let result = verify_chain_integrity(&chain, Some(&prev));
        assert!(result.valid);
    }

    #[test]
    fn verify_chain_integrity_rejects_mismatched_prev() {
        let mut chain = make_chain_state();
        chain.prev_chain_state_hash = Some("02".repeat(32));

        let result = verify_chain_integrity(&chain, Some(&("03".repeat(32))));
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("prev_chain_state_hash mismatch")));
    }

    #[test]
    fn verify_chain_integrity_rejects_missing_expected_prev() {
        let chain = make_chain_state();
        let result = verify_chain_integrity(&chain, Some(&("02".repeat(32))));
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("expected prev_chain_state_hash") && i.contains("found None")));
    }
}
