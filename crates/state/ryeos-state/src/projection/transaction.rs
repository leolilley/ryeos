use anyhow::Context;

use super::ProjectionDb;

impl ProjectionDb {
    pub(crate) fn immediate_transaction<T>(
        &self,
        label: &'static str,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .with_context(|| format!("failed to begin {label} transaction"))?;
        match f() {
            Ok(value) => {
                self.conn
                    .execute_batch("COMMIT")
                    .with_context(|| format!("failed to commit {label} transaction"))?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}
