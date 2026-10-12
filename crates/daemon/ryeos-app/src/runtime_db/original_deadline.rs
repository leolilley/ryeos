//! Original RuntimeDb connection scope, owned only under StateStore's guard.
//! No schema/epoch change, connection replacement, timeout renewal or worker.
use super::*;
use lillux::time::MonotonicDeadline;
use std::sync::atomic::{AtomicBool, Ordering};

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
        match &self.conn {
            RuntimeConnection::Live(live) => {
                live.owner().require_original_process()?;
                self.original_deadline_active.set(true);
                let _notification = NativeDeadlineNotification {
                    db: self,
                    owner: live.owner(),
                    admission: process_attachment_admission_open,
                    closed: process_attachment_admission_closed,
                };
                live.with_deadline(deadline, |_| operation(self))
            }
            RuntimeConnection::OriginalMutation { owner, schema_reference: _ } => {
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
                owner.with_sql_deadline(deadline, |_| operation(self))
            }
            // A read-only inspection owner serves no mutation, so it owns no
            // original deadline scope.
            RuntimeConnection::OriginalInspection { owner: _, factory: _, schema_reference: _ } => {
                bail!("inspection RuntimeDb owns no original deadline scope")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::time::Duration;
    const UNBOUNDED_SUM: &str = "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT SUM(x) FROM n";

    fn run_until_interrupted(
        db: &RuntimeDb,
        admission: &AtomicBool,
        closed: &tokio::sync::Notify,
    ) -> MonotonicDeadline {
        let window = MonotonicDeadline::after(Duration::from_millis(10));
        let result = db.with_original_deadline(window, admission, closed, |db| {
            db.conn.with_sql(|connection| {
                Ok(connection.query_row(UNBOUNDED_SUM, [], |row| row.get::<_, i64>(0))?)
            })
        });
        // The step hook interrupts the work once the window elapses.
        assert!(result.is_err());
        assert!(window.has_elapsed());
        assert!(!db.original_deadline_active.get());
        window
    }

    #[test]
    fn live_runtime_sql_expiry_interrupts_work_and_leaves_the_connection_usable() {
        let db = crate::runtime_db::test_native::TestRuntime::open_temporary();
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        run_until_interrupted(&db, &admission, &closed);
        // The elapsed window ended only its own operation: the live owner
        // keeps serving ordinary SQL and later windows.
        assert_eq!(
            db.conn
                .with_sql(|connection| {
                    Ok(connection.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))?)
                })
                .unwrap(),
            7
        );
        let later = MonotonicDeadline::after(Duration::from_secs(2));
        let value = db
            .with_original_deadline(later, &admission, &closed, |db| {
                db.conn.with_sql(|connection| {
                    Ok(connection.query_row("SELECT 8", [], |row| row.get::<_, i64>(0))?)
                })
            })
            .unwrap();
        assert_eq!(value, 8);
        // Expiry is not a refused hook restoration, so attachment admission
        // stays open.
        assert!(admission.load(Ordering::Acquire));
    }

    #[test]
    fn mutation_runtime_sql_expiry_interrupts_work_and_anchors_the_connection() {
        use crate::runtime_db::test_native::{open_live, open_original_mutation, retire};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite3");
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        // A current store, created and closed by the live owner first.
        retire(&host, open_live(&host, &path));
        let root = std::sync::Arc::new(
            lillux::PinnedDirectory::open(directory.path()).unwrap().unwrap(),
        );
        let namespace = root.lock_exclusive().unwrap();
        let db = open_original_mutation(&host, &root, &namespace, &path);
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        run_until_interrupted(&db, &admission, &closed);
        // The offline mutation owner keeps the elapsed expiry as its anchor:
        // no later scope renews it, so the connection serves no further SQL.
        let refused = db
            .conn
            .with_sql(|connection| {
                Ok(connection.query_row("SELECT 7", [], |row| row.get::<_, i64>(0))?)
            })
            .unwrap_err();
        assert!(format!("{refused:#}").contains("no renewal"), "{refused:#}");
        assert!(admission.load(Ordering::Acquire));
        retire(&host, db);
    }
    #[test]
    fn live_runtime_sql_stays_usable_after_a_completed_window_elapses() {
        let db = crate::runtime_db::test_native::TestRuntime::open_temporary();
        let admission = AtomicBool::new(true);
        let closed = tokio::sync::Notify::new();
        let window = MonotonicDeadline::after(Duration::from_millis(50));
        let value = db
            .with_original_deadline(window, &admission, &closed, |db| {
                db.conn.with_sql(|connection| {
                    Ok(connection.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?)
                })
            })
            .unwrap();
        assert_eq!(value, 1);
        while !window.has_elapsed() {
            std::thread::sleep(Duration::from_millis(5));
        }
        // One launch window bounds only its own operation: the daemon's
        // single Runtime connection serves ordinary SQL after it elapses.
        assert_eq!(
            db.conn
                .with_sql(|connection| {
                    Ok(connection.query_row("SELECT 2", [], |row| row.get::<_, i64>(0))?)
                })
                .unwrap(),
            2
        );
        assert!(admission.load(Ordering::Acquire));
    }
    #[test]
    fn original_runtime_sql_scope_unwind_restores_busy_and_progress_owners() {
        let db = crate::runtime_db::test_native::TestRuntime::open_temporary();
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
        // The unwound scope restored the connection's own hooks: ordinary SQL
        // runs again under the connection's live deadline.
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
        let db = crate::runtime_db::test_native::TestRuntime::open_temporary();
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
