//! Proposed AccountingDb-only deadline scope. No new process/thread supervisor.
//! SQLite busy contention refuses immediately; VM work shares the caller's
//! ORIGINAL Lillux absolute deadline. Kernel durability I/O is not hard-contained.

use super::*;
use lillux::time::{Duration, MonotonicDeadline};
use std::sync::TryLockError;

struct DeadlineScope<'a> {
    db: &'a AccountingDb,
    conn: &'a Connection,
    prior_busy_milliseconds: u64,
    installed: bool,
}

impl<'a> DeadlineScope<'a> {
    fn install(
        db: &'a AccountingDb,
        conn: &'a Connection,
        deadline: MonotonicDeadline,
    ) -> Result<Self> {
        if deadline.has_elapsed() {
            bail!("original funding deadline expired before SQLite scope");
        }
        let prior: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
        let prior_busy_milliseconds =
            u64::try_from(prior).context("SQLite busy timeout is negative")?;
        // A relative busy timeout installed once would restart at every later
        // statement. Zero refuses contention without spending a renewed wait.
        conn.busy_timeout(Duration::ZERO)?;
        // This private connection has no other progress-hook owner. The held
        // mutex excludes concurrent/nested installation; never export it.
        conn.progress_handler(256, Some(move || deadline.has_elapsed()));
        Ok(Self {
            db,
            conn,
            prior_busy_milliseconds,
            installed: true,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        self.conn.progress_handler(0, None::<fn() -> bool>);
        if let Err(error) = self
            .conn
            .busy_timeout(Duration::from_millis(self.prior_busy_milliseconds))
        {
            self.db.disable_hard_admission();
            return Err(error).context("restore exclusive SQLite deadline scope");
        }
        Ok(())
    }
}

impl Drop for DeadlineScope<'_> {
    fn drop(&mut self) {
        // Includes unwind/early error. Restoration failure fences subsequent
        // hard funding instead of leaving an unknown reusable handler state.
        if self.restore().is_err() {
            self.db.disable_hard_admission();
        }
    }
}

impl AccountingDb {
    fn lock_conn_until<'c>(
        &self,
        conn: &'c Mutex<Connection>,
        deadline: MonotonicDeadline,
    ) -> Result<MutexGuard<'c, Connection>> {
        loop {
            if deadline.has_elapsed() {
                bail!("original funding deadline expired waiting for ledger");
            }
            match conn.try_lock() {
                Ok(conn) => {
                    if deadline.has_elapsed() {
                        bail!("original funding deadline expired at ledger acquisition");
                    }
                    return Ok(conn);
                }
                Err(TryLockError::Poisoned(_)) => bail!("accounting connection mutex poisoned"),
                Err(TryLockError::WouldBlock) => {
                    lillux::time::sleep(deadline.remaining().min(Duration::from_millis(1)))
                }
            }
        }
    }

    pub(super) fn with_conn_until<T>(
        &self,
        deadline: MonotonicDeadline,
        operation: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let conn = match &self.sql {
            LedgerSql::Stock(conn) => conn,
            LedgerSql::Native(ledger) => {
                // The native owner installs the deadline beside its VM-step
                // debit and restores the debit-only hook afterwards. A failed
                // restoration refuses every later SQL call on that owner.
                return ledger.with_sql_until(deadline, |conn| {
                    operation(conn).and_then(|value| {
                        if deadline.has_elapsed() {
                            // The operation may have COMMITTED. This never
                            // refunds or repeats it; its original owner must
                            // retain uncertainty.
                            bail!("original funding deadline expired after ledger operation; reconcile SAME owner");
                        }
                        Ok(value)
                    })
                });
            }
        };
        let conn = self.lock_conn_until(conn, deadline)?;
        let mut scope = DeadlineScope::install(self, &conn, deadline)?;
        let result = if deadline.has_elapsed() {
            Err(anyhow::anyhow!(
                "original funding deadline expired before ledger operation"
            ))
        } else {
            operation(&conn).and_then(|value| {
                if deadline.has_elapsed() {
                    // The operation may have COMMITTED. This never refunds or
                    // repeats it; its original owner must retain uncertainty.
                    bail!("original funding deadline expired after ledger operation; reconcile SAME owner");
                }
                Ok(value)
            })
        };
        match (result, scope.restore()) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(restore)) => Err(restore),
            (Err(error), Err(restore)) => {
                Err(error.context(format!("deadline restoration also failed: {restore:#}")))
            }
        }
    }
}
