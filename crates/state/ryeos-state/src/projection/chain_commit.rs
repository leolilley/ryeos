use anyhow::Context;

use crate::head_cache::HeadCache;

use super::ProjectionDb;

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
