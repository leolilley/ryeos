//! AccountingDb-only deadline scope. No new process/thread supervisor. VM
//! work shares the caller's ORIGINAL Lillux absolute deadline. Kernel
//! durability I/O is not hard-contained.

use super::*;
use lillux::time::MonotonicDeadline;

impl AccountingDb {
    pub(super) fn with_conn_until<T>(
        &self,
        deadline: MonotonicDeadline,
        operation: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        // The native owner installs the deadline beside its VM-step debit and
        // restores the debit-only hook afterwards. A failed restoration
        // refuses every later SQL call on that owner.
        self.ledger.with_sql_until(deadline, |conn| {
            operation(conn).and_then(|value| {
                if deadline.has_elapsed() {
                    // The operation may have COMMITTED. This never refunds or
                    // repeats it; its original owner must retain uncertainty.
                    bail!("original funding deadline expired after ledger operation; reconcile SAME owner");
                }
                Ok(value)
            })
        })
    }
}
