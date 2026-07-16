use anyhow::Context;

use crate::head_cache::HeadCache;

use super::ProjectionDb;

/// Project the first published head of a root created with initial events.
///
/// That head has an internal zero-event genesis predecessor in authoritative
/// history, while the rebuildable projection correctly has no prior cursor.
/// This is the only projection path allowed to advance from an absent cursor
/// to a committed head that names a predecessor.
pub(crate) fn project_initial_root_committed_chain(
    projection_db: &ProjectionDb,
    cache: &HeadCache,
    chain_root_id: &str,
    committed_hash: &str,
    project_rows: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let cached = cache
        .get(chain_root_id)
        .ok_or_else(|| anyhow::anyhow!("committed chain head missing from cache"))?;
    if cached.chain_state_hash != committed_hash {
        anyhow::bail!(
            "cached chain head {} does not match committed head {committed_hash}",
            cached.chain_state_hash
        );
    }

    let predecessor = cached
        .chain_state
        .prev_chain_state_hash
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "initial root committed head {committed_hash} is missing its internal genesis predecessor"
            )
        })?;
    let root_entry = cached
        .chain_state
        .threads
        .get(chain_root_id)
        .expect("ChainState validation requires its root entry");
    if cached.chain_state.threads.len() != 1
        || cached.chain_state.last_chain_seq == 0
        || cached.chain_state.last_event_hash.is_none()
        || root_entry.last_thread_seq != cached.chain_state.last_chain_seq
        || root_entry.last_event_hash != cached.chain_state.last_event_hash
        || root_entry.status != crate::objects::ThreadStatus::Created
    {
        anyhow::bail!(
            "initial root committed head {committed_hash} does not describe one created root advancing its internal genesis {predecessor}"
        );
    }

    projection_db.immediate_transaction("initial authoritative chain projection", || {
        let current = projection_db.get_projection_meta(chain_root_id)?;
        let current_hash = current
            .as_ref()
            .map(|meta| meta.indexed_chain_state_hash.as_str());
        if current_hash == Some(committed_hash) {
            return Ok(());
        }
        if current_hash.is_some() {
            anyhow::bail!(
                "initial projection cursor conflict for chain {chain_root_id}: current={:?}, expected absent cursor, committed={committed_hash}",
                current_hash,
            );
        }
        project_rows()?;
        super::project_chain_state(projection_db, &cached.chain_state, committed_hash)
            .context("advancing initial projection cursor")
    })
}

pub(crate) fn project_committed_chain(
    projection_db: &ProjectionDb,
    cache: &HeadCache,
    chain_root_id: &str,
    committed_hash: &str,
    project_rows: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let cached = cache
        .get(chain_root_id)
        .ok_or_else(|| anyhow::anyhow!("committed chain head missing from cache"))?;
    if cached.chain_state_hash != committed_hash {
        anyhow::bail!(
            "cached chain head {} does not match committed head {committed_hash}",
            cached.chain_state_hash
        );
    }
    projection_db.immediate_transaction("authoritative chain projection", || {
        let current = projection_db.get_projection_meta(chain_root_id)?;
        let current_hash = current
            .as_ref()
            .map(|meta| meta.indexed_chain_state_hash.as_str());
        if current_hash == Some(committed_hash) {
            return Ok(());
        }
        let expected = cached.chain_state.prev_chain_state_hash.as_deref();
        if current_hash != expected {
            anyhow::bail!(
                "projection cursor conflict for chain {chain_root_id}: current={:?}, expected predecessor={:?}, committed={committed_hash}",
                current_hash,
                expected,
            );
        }
        project_rows()?;
        super::project_chain_state(projection_db, &cached.chain_state, committed_hash)
            .context("advancing projection cursor")
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::head_cache::{CachedHead, HeadCache};
    use crate::objects::chain_state::{ChainStateBuilder, ChainThreadEntry};
    use crate::objects::ThreadStatus;

    use super::{project_initial_root_committed_chain, ProjectionDb};

    const ROOT: &str = "T-root";

    fn initial_root_cache(predecessor: Option<String>) -> (HeadCache, String) {
        let event_hash = "b".repeat(64);
        let state = ChainStateBuilder::new(ROOT)
            .prev_chain_state_hash(predecessor)
            .last_event_hash(Some(event_hash.clone()))
            .last_chain_seq(1)
            .updated_at("2026-07-16T00:00:01Z".to_string())
            .thread(
                ROOT,
                ChainThreadEntry {
                    snapshot_hash: "a".repeat(64),
                    last_event_hash: Some(event_hash),
                    last_thread_seq: 1,
                    status: ThreadStatus::Created,
                },
            )
            .build();
        let hash = crate::objects::chain_state::hash_chain_state(&state).unwrap();
        let mut cache = HeadCache::new();
        assert!(cache.insert(ROOT, CachedHead::new(hash.clone(), state)));
        (cache, hash)
    }

    #[test]
    fn initial_root_projection_advances_only_from_absent_and_is_idempotent() {
        let db = ProjectionDb::open_transient().unwrap();
        let (cache, committed_hash) = initial_root_cache(Some("c".repeat(64)));
        let calls = Cell::new(0usize);

        project_initial_root_committed_chain(&db, &cache, ROOT, &committed_hash, || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(
            db.get_projection_meta(ROOT)
                .unwrap()
                .unwrap()
                .indexed_chain_state_hash,
            committed_hash
        );

        project_initial_root_committed_chain(&db, &cache, ROOT, &committed_hash, || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls.get(),
            1,
            "idempotent replay must not project rows twice"
        );
    }

    #[test]
    fn initial_root_projection_rejects_missing_genesis_or_existing_cursor() {
        let db = ProjectionDb::open_transient().unwrap();
        let (without_genesis, without_genesis_hash) = initial_root_cache(None);
        let error = project_initial_root_committed_chain(
            &db,
            &without_genesis,
            ROOT,
            &without_genesis_hash,
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("missing its internal genesis predecessor"));

        db.update_projection_meta(&crate::projection::ProjectionMeta {
            chain_root_id: ROOT.to_string(),
            indexed_chain_state_hash: "d".repeat(64),
            updated_at: "2026-07-16T00:00:00Z".to_string(),
        })
        .unwrap();
        let (cache, committed_hash) = initial_root_cache(Some("c".repeat(64)));
        let error =
            project_initial_root_committed_chain(&db, &cache, ROOT, &committed_hash, || Ok(()))
                .unwrap_err();
        assert!(format!("{error:#}").contains("expected absent cursor"));
    }
}
