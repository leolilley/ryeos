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
            Ok(value) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(commit_error) => {
                    let commit_error = anyhow::Error::new(commit_error)
                        .context(format!("failed to commit {label} transaction"));
                    match self.conn.execute_batch("ROLLBACK") {
                        Ok(()) => Err(commit_error),
                        Err(rollback_error) => Err(commit_error.context(format!(
                            "failed to roll back {label} transaction after commit failure: \
                                 {rollback_error}"
                        ))),
                    }
                }
            },
            Err(error) => match self.conn.execute_batch("ROLLBACK") {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(error.context(format!(
                    "failed to roll back {label} transaction after operation failure: \
                     {rollback_error}"
                ))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProjectionDb;

    #[test]
    fn commit_failure_rolls_back_and_leaves_connection_usable() {
        let db = ProjectionDb::open_transient().unwrap();
        db.connection()
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE tx_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE tx_child (
                     parent_id INTEGER NOT NULL,
                     FOREIGN KEY (parent_id) REFERENCES tx_parent(id)
                         DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();

        let error = db
            .immediate_transaction("commit failure", || {
                db.connection()
                    .execute("INSERT INTO tx_child (parent_id) VALUES (1)", [])?;
                Ok(())
            })
            .unwrap_err();

        assert!(format!("{error:#}").contains("failed to commit commit failure transaction"));
        assert!(db.connection().is_autocommit());
        let rows: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM tx_child", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "failed commit must not leave projected rows live");
    }

    #[test]
    fn operation_failure_reports_a_failed_rollback() {
        let db = ProjectionDb::open_transient().unwrap();

        let error = db
            .immediate_transaction("rollback reporting", || -> anyhow::Result<()> {
                db.connection().execute_batch("ROLLBACK")?;
                anyhow::bail!("projection operation failed")
            })
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("projection operation failed"));
        assert!(message.contains(
            "failed to roll back rollback reporting transaction after operation failure"
        ));
    }
}
