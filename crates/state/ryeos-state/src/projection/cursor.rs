use anyhow::Context;
use rusqlite::OptionalExtension;

use super::ProjectionDb;

#[derive(Debug, Clone)]
pub struct ProjectionMeta {
    pub chain_root_id: String,
    pub indexed_chain_state_hash: String,
    pub updated_at: String,
}

impl ProjectionDb {
    /// Get projection metadata for a chain.
    pub fn get_projection_meta(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<ProjectionMeta>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT chain_root_id, indexed_chain_state_hash, updated_at FROM projection_meta WHERE chain_root_id = ?")
            .context("failed to prepare query")?;

        stmt.query_row([chain_root_id], |row| {
            Ok(ProjectionMeta {
                chain_root_id: row.get(0)?,
                indexed_chain_state_hash: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .optional()
        .context("failed to query projection_meta")
    }

    /// Update projection metadata for a chain.
    pub fn update_projection_meta(&self, meta: &ProjectionMeta) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO projection_meta (chain_root_id, indexed_chain_state_hash, updated_at) VALUES (?, ?, ?)",
                rusqlite::params![&meta.chain_root_id, &meta.indexed_chain_state_hash, &meta.updated_at],
            )
            .context("failed to update projection_meta")?;
        Ok(())
    }
}
