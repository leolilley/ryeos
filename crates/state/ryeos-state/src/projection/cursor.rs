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
    /// Delete every replaceable row owned by a chain using the caller's
    /// existing projection transaction.
    ///
    /// Full/delta repair uses this before a replacement replay; the public
    /// wrapper below remains available to callers that do not already own a
    /// transaction.
    pub(crate) fn delete_chain_projection_in_transaction(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<usize> {
        let thread_ids = "SELECT thread_id FROM threads WHERE chain_root_id=?1";
        let mut deleted = 0usize;
        deleted += self.conn.execute(
            &format!("DELETE FROM event_replay_index WHERE thread_id IN ({thread_ids})"),
            [chain_root_id],
        )?;
        deleted += self
            .conn
            .execute("DELETE FROM events WHERE chain_root_id=?1", [chain_root_id])?;
        deleted += self.conn.execute(
            "DELETE FROM thread_edges WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM thread_results WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM thread_artifacts WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            &format!("DELETE FROM thread_facets WHERE thread_id IN ({thread_ids})"),
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM thread_usage_latest WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM thread_usage_subjects WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM projection_meta WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM chain_retention WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        deleted += self.conn.execute(
            "DELETE FROM threads WHERE chain_root_id=?1",
            [chain_root_id],
        )?;
        Ok(deleted)
    }

    /// Delete every replaceable row owned by a chain in one projection
    /// transaction. Runtime DB/files are intentionally outside this method.
    pub fn delete_chain_projection(&self, chain_root_id: &str) -> anyhow::Result<usize> {
        self.immediate_transaction("delete chain projection", || {
            self.delete_chain_projection_in_transaction(chain_root_id)
        })
    }

    pub fn projection_instance_id(&self) -> anyhow::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT projection_instance_id FROM projection_recovery_identity WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("failed to query projection recovery identity")
    }

    pub fn set_projection_instance_id(&self, instance_id: &str) -> anyhow::Result<()> {
        if instance_id.is_empty()
            || instance_id.len() > 128
            || !instance_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            anyhow::bail!("invalid projection instance id");
        }
        self.conn
            .execute(
                "INSERT OR REPLACE INTO projection_recovery_identity (singleton, projection_instance_id) VALUES (1, ?1)",
                [instance_id],
            )
            .context("failed to set projection recovery identity")?;
        Ok(())
    }

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
