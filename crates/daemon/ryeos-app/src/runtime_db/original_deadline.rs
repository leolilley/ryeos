//! Original RuntimeDb connection scope, owned only under StateStore's guard.
//! No schema/epoch change, connection replacement, timeout renewal or worker.
use super::*;
use lillux::time::{Duration, MonotonicDeadline};
use std::sync::atomic::{AtomicBool, Ordering};

struct OriginalDeadlineScope<'a> {
    db: &'a RuntimeDb,
    connection: &'a Connection,
    process_attachment_admission_open: &'a AtomicBool,
    process_attachment_admission_closed: &'a tokio::sync::Notify,
    prior_busy_milliseconds: u64,
    installed: bool,
}

impl OriginalDeadlineScope<'_> {
    fn restore(&mut self) -> Result<()> {
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        self.connection.progress_handler(0, None::<fn() -> bool>);
        let result = self
            .connection
            .busy_timeout(Duration::from_millis(self.prior_busy_milliseconds));
        self.db.original_deadline_active.set(false);
        if let Err(error) = result {
            self.process_attachment_admission_open
                .store(false, Ordering::Release);
            // SAME existing owner signal; no StateStore/registry re-lock.
            self.process_attachment_admission_closed.notify_waiters();
            return Err(error).context("restore ORIGINAL RuntimeDb deadline scope");
        }
        Ok(())
    }
}
impl Drop for OriginalDeadlineScope<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

// The lower native scope restores its accounting hook before this outer
// notification guard runs, including unwinding. No second connection owner.
struct NativeDeadlineNotification<'a> {
    db: &'a RuntimeDb,
    owner: &'a lillux_sqlite::OriginalNativeConnection,
    admission: &'a AtomicBool,
    closed: &'a tokio::sync::Notify,
}
impl Drop for NativeDeadlineNotification<'_> {
    fn drop(&mut self) {
        if self.owner.require_original_process().is_err() {
            return;
        }
        self.db.original_deadline_active.set(false);
        if self.owner.sql_owner_refused() {
            self.admission.store(false, Ordering::Release);
            self.closed.notify_waiters();
        }
    }
}

impl RuntimeDb {
    /// StateStore owns the sole connection guard through this entire scope.
    /// No existing RuntimeDb path owns a progress handler; reject nesting so
    /// an inner caller cannot erase or renew an outer original scope.
    pub(crate) fn with_original_deadline<T>(
        &self,
        deadline: MonotonicDeadline,
        process_attachment_admission_open: &AtomicBool,
        process_attachment_admission_closed: &tokio::sync::Notify,
        operation: impl FnOnce(&Self) -> Result<T>,
    ) -> Result<T> {
        if deadline.has_elapsed() {
            bail!("original RuntimeDb deadline expired before scope");
        }
        if self._inspection_sql.is_some() {
            bail!("admitted inspection SQL owner cannot be replaced by another deadline scope");
        }
        if self.original_deadline_active.get() {
            bail!("original RuntimeDb deadline scope cannot nest or renew");
        }
        if let RuntimeConnection::Live(live) = &self.conn {
            live.owner().require_original_process()?;
            self.original_deadline_active.set(true);
            let _notification = NativeDeadlineNotification {
                db: self,
                owner: live.owner(),
                admission: process_attachment_admission_open,
                closed: process_attachment_admission_closed,
            };
            return live.with_deadline(deadline, |_| operation(self));
        }
        if let RuntimeConnection::OriginalMutation { owner, schema_reference: _ } = &self.conn {
            owner.require_original_process()?;
            self.original_deadline_active.set(true);
            let _notification = NativeDeadlineNotification {
                db: self,
                owner,
                admission: process_attachment_admission_open,
                closed: process_attachment_admission_closed,
            };
            // The native owner combines the original debit with expiry and
            // restores its own hook. Never install the ordinary hook on it.
            return owner.with_sql_deadline(deadline, |_| operation(self));
        }
        self.conn.with_mutable_sql(|connection| {
            let prior: i64 = connection
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let prior_busy_milliseconds =
                u64::try_from(prior).context("RuntimeDb busy timeout is negative")?;
            connection.busy_timeout(Duration::ZERO)?;
            connection
                .progress_handler(256, Some(move || deadline.has_elapsed()));
            self.original_deadline_active.set(true);
            let mut scope = OriginalDeadlineScope {
                db: self,
                connection,
                process_attachment_admission_open,
                process_attachment_admission_closed,
                prior_busy_milliseconds,
                installed: true,
            };
            let result = if deadline.has_elapsed() {
                Err(anyhow!(
                    "original RuntimeDb deadline expired before operation"
                ))
            } else {
                operation(self)
            };
            let result = result.and_then(|value| {
                if deadline.has_elapsed() { bail!("original RuntimeDb operation completed after its expiry; commit outcome may be uncertain"); }
                Ok(value)
            });
            let restored = scope.restore();
            match (result, restored) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(restore)) => {
                    Err(error.context(format!("RuntimeDb restoration refused: {restore:#}")))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_runtime_sql_expiry_interrupts_work_and_restores_actual_connection() {
        let db = RuntimeDb::new_in_memory().unwrap();
        db.conn.with_mutable_sql(|connection| {
            connection.busy_timeout(Duration::from_millis(37))?;
            Ok(())
        }).unwrap();
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        let original = MonotonicDeadline::after(Duration::from_millis(10));
        let result = db.with_original_deadline(original, &admission, &closed, |db| {
            db.conn.with_sql(|connection| {
                let sum: i64 = connection.query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT SUM(x) FROM n", [], |row| row.get(0))?;
                Ok(sum)
            })
        });
        assert!(result.is_err());
        assert!(original.has_elapsed());
        assert!(!db.original_deadline_active.get());
        assert_eq!(
            db.conn.with_sql(|connection| {
                Ok(connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?)
            }).unwrap(),
            37
        );
        assert_eq!(
            db.conn.with_sql(|connection| {
                Ok(connection.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))?)
            }).unwrap(),
            7
        );
        assert!(admission.load(Ordering::Acquire));
    }
    #[test]
    fn original_runtime_sql_scope_unwind_restores_busy_and_progress_owners() {
        let db = RuntimeDb::new_in_memory().unwrap();
        db.conn.with_mutable_sql(|connection| {
            connection.busy_timeout(Duration::from_millis(19))?;
            Ok(())
        }).unwrap();
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        let original = MonotonicDeadline::after(Duration::from_secs(2));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = db.with_original_deadline(original, &admission, &closed, |_| {
                panic!("actual scoped unwind")
            });
        }));
        assert!(result.is_err());
        assert!(!db.original_deadline_active.get());
        assert_eq!(
            db.conn.with_sql(|connection| {
                Ok(connection.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?)
            }).unwrap(),
            19
        );
        assert_eq!(
            db.conn.with_sql(|connection| {
                Ok(connection.query_row("SELECT 3", [], |row| row.get::<_, i64>(0))?)
            }).unwrap(),
            3
        );
        assert!(admission.load(Ordering::Acquire));
    }
    #[test]
    fn original_runtime_sql_scope_refuses_nested_clock_replacement() {
        let db = RuntimeDb::new_in_memory().unwrap();
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        let original = MonotonicDeadline::after(Duration::from_secs(2));
        db.with_original_deadline(original, &admission, &closed, |db| {
            let inner: Result<()> = db.with_original_deadline(
                MonotonicDeadline::after(Duration::from_secs(60)),
                &admission,
                &closed,
                |_| Ok(()),
            );
            assert!(inner.is_err());
            assert!(db.original_deadline_active.get());
            Ok(())
        })
        .unwrap();
        assert!(!db.original_deadline_active.get());
        assert!(admission.load(Ordering::Acquire));
    }
}
