//! Node lease budget: concurrent leases and lease-seconds reserved before any
//! provider contact.
//!
//! Workload: remote leases a lifecycle adapter creates for placed sessions,
//! each of which costs provider spend for as long as it lives. Node policy
//! caps how many are held at once and how many budget seconds the leases
//! reserved in the last 24 hours may use. One lease-second costs as many
//! budget seconds as the budget weight its admitted provider spec declares,
//! so a costlier provider (a GPU lease, say) spends the same daily budget
//! faster. A lease reserves its whole weighted lifetime in the same
//! transaction that reserves its allocation, before the adapter is asked to
//! create anything, so the worst case stays bounded even when a create's
//! outcome is unknown. When the allocation settles, the lease is released in
//! that same settling transaction: it stops counting as held, and only the
//! seconds it actually lasted, at its weight, stay charged. A lease that does
//! not fit is refused with the limit it met; nothing is queued.
//!
//! A held lease is released only by settlement, never by elapsed time: a
//! quarantined allocation whose provider occurrence is unresolved keeps its
//! lease, because its spend is not known to have stopped.

use super::*;

/// The window the daily lease budget covers.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

pub(in crate::runtime_db) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_lease_budget (
    placement_thread_id TEXT PRIMARY KEY,
    reserved_at_ms INTEGER NOT NULL CHECK (reserved_at_ms > 0),
    reserved_seconds INTEGER NOT NULL CHECK (reserved_seconds > 0),
    budget_weight INTEGER NOT NULL CHECK (budget_weight BETWEEN 1 AND 1000),
    released_at_ms INTEGER,
    charged_seconds INTEGER,
    CHECK ((released_at_ms IS NULL AND charged_seconds IS NULL)
        OR (released_at_ms IS NOT NULL AND released_at_ms >= reserved_at_ms
            AND charged_seconds BETWEEN 0 AND reserved_seconds))
);
CREATE INDEX idx_external_execution_lease_budget_reserved
    ON external_execution_lease_budget(reserved_at_ms);
CREATE TRIGGER external_execution_lease_budget_insert_guard
BEFORE INSERT ON external_execution_lease_budget
WHEN NEW.released_at_ms IS NOT NULL
 OR NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    WHERE a.placement_thread_id=NEW.placement_thread_id AND a.phase='reserved')
BEGIN SELECT RAISE(ABORT, 'a lease budget is reserved only with a fresh allocation'); END;
CREATE TRIGGER external_execution_lease_budget_release_guard
BEFORE UPDATE ON external_execution_lease_budget
WHEN OLD.released_at_ms IS NOT NULL
 OR NEW.released_at_ms IS NULL
 OR NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.reserved_at_ms != OLD.reserved_at_ms
 OR NEW.reserved_seconds != OLD.reserved_seconds
 OR NEW.budget_weight != OLD.budget_weight
BEGIN SELECT RAISE(ABORT, 'a lease budget is released once and otherwise immutable'); END;
CREATE TRIGGER external_execution_lease_budget_held_no_delete
BEFORE DELETE ON external_execution_lease_budget
WHEN OLD.released_at_ms IS NULL
BEGIN SELECT RAISE(ABORT, 'a held lease keeps its budget until its allocation settles'); END;
CREATE TRIGGER external_execution_lease_budget_release
AFTER UPDATE OF phase ON external_execution_allocation
WHEN OLD.phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
 AND NEW.phase IN ('no_contact','contacted_no_occurrence','terminated')
BEGIN
    UPDATE external_execution_lease_budget
       SET released_at_ms=MAX(NEW.updated_at_ms, reserved_at_ms),
           charged_seconds=MIN(reserved_seconds,
               (MAX(NEW.updated_at_ms, reserved_at_ms) - reserved_at_ms + 999) / 1000)
     WHERE placement_thread_id=NEW.placement_thread_id AND released_at_ms IS NULL;
END;
"#;

/// One lease's lifetime checked against node policy, before its lifecycle
/// adapter has admitted it and so before the budget weight of the provider
/// spec that judges it is known. Only [`Self::weighted_by`] turns it into a
/// charge the budget can reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnweightedLeaseCharge {
    pub(crate) lifetime_seconds: u32,
    pub(crate) max_concurrent_leases: u32,
    pub(crate) daily_lease_seconds: u64,
}

impl UnweightedLeaseCharge {
    /// One lease of `lifetime_seconds` under `budget`, refused when the lease
    /// would outlive the budget's lifetime ceiling.
    pub(crate) fn under(
        budget: &ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget,
        lifetime_seconds: u32,
    ) -> Result<Self> {
        budget.validate()?;
        ensure!(
            lifetime_seconds > 0 && lifetime_seconds <= budget.max_lease_lifetime_seconds,
            "lease refused: its {lifetime_seconds}s lifetime exceeds the node's {}s lease \
             lifetime ceiling",
            budget.max_lease_lifetime_seconds
        );
        Ok(Self {
            lifetime_seconds,
            max_concurrent_leases: budget.max_concurrent_leases,
            daily_lease_seconds: budget.daily_lease_seconds,
        })
    }

    /// The charge of this lease at `budget_weight`, the weight the admitted
    /// provider spec declares. A weight outside the contract's bound is
    /// refused, never clamped.
    pub(crate) fn weighted_by(self, budget_weight: u32) -> Result<ExternalLeaseCharge> {
        let Self {
            lifetime_seconds,
            max_concurrent_leases,
            daily_lease_seconds,
        } = self;
        let charge = ExternalLeaseCharge {
            lifetime_seconds,
            budget_weight,
            max_concurrent_leases,
            daily_lease_seconds,
        };
        charge.validate()?;
        Ok(charge)
    }
}

/// One lease's charge against the node budget: the lease's whole lifetime,
/// the budget weight of the provider spec that admitted it, and the node
/// policy limits it must fit under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExternalLeaseCharge {
    pub(crate) lifetime_seconds: u32,
    pub(crate) budget_weight: u32,
    pub(crate) max_concurrent_leases: u32,
    pub(crate) daily_lease_seconds: u64,
}

impl ExternalLeaseCharge {
    fn validate(&self) -> Result<()> {
        ensure!(
            ryeos_external_execution_contract::valid_lease_budget_weight(self.budget_weight),
            "lease refused: its provider spec's budget_weight {} is outside 1..={}",
            self.budget_weight,
            ryeos_external_execution_contract::MAX_LEASE_BUDGET_WEIGHT
        );
        ensure!(
            self.lifetime_seconds > 0,
            "lease refused: its lifetime is zero"
        );
        Ok(())
    }

    /// The budget seconds the lease's whole lifetime reserves.
    pub(crate) fn weighted_lifetime_seconds(&self) -> u64 {
        u64::from(self.lifetime_seconds) * u64::from(self.budget_weight)
    }
}

/// The node's lease use at `now_ms`: leases held, and the budget seconds
/// reserved or used by the leases reserved in the last 24 hours, each
/// lease-second counted at its lease's budget weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExternalLeaseUse {
    pub(crate) held: u64,
    pub(crate) day_seconds: u64,
}

pub(super) fn lease_use(conn: &Connection, now_ms: i64) -> Result<ExternalLeaseUse> {
    let held: i64 = conn.query_row(
        "SELECT COUNT(*) FROM external_execution_lease_budget WHERE released_at_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    let day_seconds: i64 = conn.query_row(
        "SELECT COALESCE(SUM(COALESCE(charged_seconds, reserved_seconds) * budget_weight),0)
           FROM external_execution_lease_budget WHERE reserved_at_ms > ?1",
        [now_ms.saturating_sub(DAY_MS)],
        |row| row.get(0),
    )?;
    Ok(ExternalLeaseUse {
        held: u64::try_from(held).context("negative held lease count")?,
        day_seconds: u64::try_from(day_seconds).context("negative lease-second total")?,
    })
}

/// Refuse a lease that does not fit, before its allocation is inserted.
pub(super) fn require_room(
    conn: &Connection,
    now_ms: i64,
    charge: &ExternalLeaseCharge,
) -> Result<()> {
    charge.validate()?;
    let used = lease_use(conn, now_ms)?;
    ensure!(
        used.held < u64::from(charge.max_concurrent_leases),
        "lease refused: the node already holds {} of its {} concurrent leases; a lease is \
         admitted only after one is released, and none is queued",
        used.held,
        charge.max_concurrent_leases
    );
    let weighted = charge.weighted_lifetime_seconds();
    ensure!(
        weighted <= charge.daily_lease_seconds,
        "lease refused: this lease's whole {}s lifetime at budget_weight {} ({weighted} budget \
         seconds) exceeds the node's daily budget of {} budget seconds; it can never be \
         admitted",
        charge.lifetime_seconds,
        charge.budget_weight,
        charge.daily_lease_seconds
    );
    let needed = used
        .day_seconds
        .checked_add(weighted)
        .context("lease-second total overflow")?;
    ensure!(
        needed <= charge.daily_lease_seconds,
        "lease refused: the node's daily lease budget is {}s and leases reserved in the last \
         24 hours already hold {}s, so this lease's whole {}s lifetime at its provider spec's \
         budget_weight {} ({weighted}s) does not fit; none is queued",
        charge.daily_lease_seconds,
        used.day_seconds,
        charge.lifetime_seconds,
        charge.budget_weight
    );
    Ok(())
}

/// Reserve the lease of the allocation just inserted, in its transaction.
/// Released rows that no longer count against the daily window are dropped.
pub(super) fn reserve(
    conn: &Connection,
    placement: &str,
    now_ms: i64,
    charge: &ExternalLeaseCharge,
) -> Result<()> {
    require_room(conn, now_ms, charge)?;
    conn.execute(
        "DELETE FROM external_execution_lease_budget
          WHERE released_at_ms IS NOT NULL AND reserved_at_ms <= ?1",
        [now_ms.saturating_sub(DAY_MS)],
    )?;
    conn.execute(
        "INSERT INTO external_execution_lease_budget VALUES(?1,?2,?3,?4,NULL,NULL)",
        params![
            placement,
            now_ms,
            i64::from(charge.lifetime_seconds),
            i64::from(charge.budget_weight)
        ],
    )?;
    Ok(())
}

/// A lease still held: when it was reserved and the lifetime it reserved.
/// The provider is contacted only after the reservation, so the lease ends
/// no earlier than this reservation's lifetime would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldExternalLease {
    pub(crate) reserved_at_ms: i64,
    pub(crate) lifetime_seconds: u32,
}

impl HeldExternalLease {
    /// The instant by which the lease is assumed to have ended: its
    /// reservation plus its whole lifetime. The real lease, created after the
    /// reservation, ends at or after this instant, so the bound is safe.
    pub(crate) fn assumed_end_ms(&self) -> Result<i64> {
        self.reserved_at_ms
            .checked_add(i64::from(self.lifetime_seconds) * 1_000)
            .context("lease end overflows the timestamp range")
    }
}

impl RuntimeDb {
    /// The lease the placement still holds, if any.
    pub(crate) fn held_external_lease(&self, placement: &str) -> Result<Option<HeldExternalLease>> {
        self.conn.with_sql(|conn| {
            let row: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT reserved_at_ms,reserved_seconds FROM external_execution_lease_budget
                      WHERE placement_thread_id=?1 AND released_at_ms IS NULL",
                    [placement],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(reserved_at_ms, seconds)| {
                Ok(HeldExternalLease {
                    reserved_at_ms,
                    lifetime_seconds: u32::try_from(seconds)
                        .context("held lease lifetime is outside its bound")?,
                })
            })
            .transpose()
        })
    }

    /// The placements holding a lease whose session worker is not a worker
    /// of `current_generation`, the running daemon incarnation. A worker is
    /// a local child of the daemon that started it, so a lease reserved for
    /// a worker of another incarnation has lost the worker it served. A lease
    /// whose worker row is absent is counted too: no worker of this
    /// incarnation holds it.
    ///
    /// Call this only at startup, before the node is ready. The query cannot
    /// tell a lease that lost its worker from one whose worker of the running
    /// incarnation has no matching row yet, and its caller terminates every
    /// lease it names. Only before ready, when no worker of this incarnation
    /// has started, is the second impossible.
    pub(crate) fn predecessor_external_lease_placements(
        &self,
        current_generation: &str,
    ) -> Result<Vec<String>> {
        self.conn.with_sql(|conn| {
            super::validate_current(conn)?;
            let mut statement = conn.prepare(
                "SELECT l.placement_thread_id
                   FROM external_execution_lease_budget l
                   JOIN external_execution_allocation a
                     ON a.placement_thread_id=l.placement_thread_id
                  WHERE l.released_at_ms IS NULL
                    AND NOT EXISTS(SELECT 1 FROM worker_process w
                      WHERE w.placement_thread_id=a.placement_thread_id
                        AND w.worker_instance_id=
                            json_extract(a.reservation_json,'$.owner.worker_instance_id')
                        AND w.boot_epoch=
                            json_extract(a.reservation_json,'$.owner.worker_boot_epoch')
                        AND w.daemon_generation_id=?1)
                  ORDER BY l.placement_thread_id",
            )?;
            statement
                .query_map([current_generation], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into)
        })
    }
}

/// A held lease always belongs to an unsettled allocation: settlement
/// releases it in the settling transaction, so a held lease whose allocation
/// settled or vanished means the journal was altered outside it. That every
/// lease allocation holds a lease is enforced where it is reserved.
pub(super) fn validate_current(conn: &Connection) -> Result<()> {
    let orphaned: i64 = conn.query_row(
        "SELECT COUNT(*) FROM external_execution_lease_budget l
          WHERE l.released_at_ms IS NULL AND NOT EXISTS(
            SELECT 1 FROM external_execution_allocation a
             WHERE a.placement_thread_id=l.placement_thread_id
               AND a.phase NOT IN ('no_contact','contacted_no_occurrence','terminated'))",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        orphaned == 0,
        "external lease budget holds a lease whose allocation has settled"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{reservation, reserve as reserve_session};
    use super::*;

    fn charge(max_concurrent_leases: u32, daily_lease_seconds: u64) -> ExternalLeaseCharge {
        ExternalLeaseCharge {
            lifetime_seconds: 3600,
            budget_weight: 1,
            max_concurrent_leases,
            daily_lease_seconds,
        }
    }

    fn open() -> (
        tempfile::TempDir,
        crate::runtime_db::test_native::TestRuntime,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            crate::runtime_db::test_native::TestRuntime::open(&dir.path().join("runtime.sqlite3"));
        (dir, db)
    }

    /// A storage-level allocation row, standing in for one the reservation
    /// transaction inserts, so several leases can be held at once.
    fn allocation(db: &RuntimeDb, placement: &str) {
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        db.conn
            .with_sql(|connection| {
                connection.execute(
                    "INSERT OR IGNORE INTO external_execution_binding_generation
                     VALUES(?1,?2,?3,1)",
                    params![
                        binding.digest(),
                        binding.capacity_owner(),
                        binding.canonical_json()?
                    ],
                )?;
                connection.execute(
                    "INSERT INTO external_execution_allocation
                     VALUES(?1,?2,'{}','reserved',NULL,1,1)",
                    params![placement, binding.capacity_owner()],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn lease(
        db: &RuntimeDb,
        placement: &str,
        now_ms: i64,
        charge: ExternalLeaseCharge,
    ) -> Result<()> {
        db.conn
            .with_sql(|connection| reserve(connection, placement, now_ms, &charge))
    }

    fn settle(db: &RuntimeDb, placement: &str, at_ms: i64) {
        db.conn
            .with_sql(|connection| {
                Ok(connection.execute(
                    "UPDATE external_execution_allocation SET phase='no_contact',updated_at_ms=?2
                     WHERE placement_thread_id=?1",
                    params![placement, at_ms],
                )?)
            })
            .unwrap();
    }

    fn used(db: &RuntimeDb, now_ms: i64) -> ExternalLeaseUse {
        db.conn
            .with_sql(|connection| lease_use(connection, now_ms))
            .unwrap()
    }

    #[test]
    fn the_lease_past_the_concurrency_ceiling_is_refused_until_one_is_released() {
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        for placement in ["T-a", "T-b", "T-c"] {
            allocation(&db, placement);
        }
        lease(&db, "T-a", now, charge(2, 8 * 3600)).unwrap();
        lease(&db, "T-b", now, charge(2, 8 * 3600)).unwrap();
        let refusal = lease(&db, "T-c", now, charge(2, 8 * 3600))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("already holds 2 of its 2 concurrent leases")
                && refusal.contains("none is queued"),
            "{refusal}"
        );
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 2,
                day_seconds: 7200
            }
        );

        // Settlement releases the lease in the settling transaction.
        settle(&db, "T-a", now + 90_000);
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 1,
                day_seconds: 3690
            }
        );
        lease(&db, "T-c", now, charge(2, 8 * 3600)).unwrap();
    }

    #[test]
    fn the_lease_past_the_daily_budget_is_refused_and_settlement_returns_the_unused_part() {
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        for placement in ["T-a", "T-b", "T-c", "T-d"] {
            allocation(&db, placement);
        }
        let budget = charge(4, 2 * 3600);
        lease(&db, "T-a", now, budget).unwrap();
        lease(&db, "T-b", now, budget).unwrap();
        let refusal = lease(&db, "T-c", now, budget).unwrap_err().to_string();
        assert!(
            refusal.contains("daily lease budget is 7200s")
                && refusal.contains("already hold 7200s"),
            "{refusal}"
        );
        // A lease that ended at once keeps no charge; its unused lifetime
        // returns to the day.
        settle(&db, "T-a", now);
        lease(&db, "T-c", now, budget).unwrap();
        assert!(lease(&db, "T-d", now, budget).is_err());

        // Leases reserved more than a day ago no longer count, and once
        // released they are dropped.
        settle(&db, "T-b", now + 1_000);
        settle(&db, "T-c", now + 1_000);
        let tomorrow = now + DAY_MS;
        assert_eq!(
            used(&db, tomorrow),
            ExternalLeaseUse {
                held: 0,
                day_seconds: 0
            }
        );
        lease(&db, "T-d", tomorrow, budget).unwrap();
        let rows: i64 = db
            .conn
            .with_sql(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM external_execution_lease_budget",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }

    fn budget(
        daily_lease_seconds: u64,
    ) -> ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget {
        ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget {
            max_concurrent_leases: 8,
            max_lease_lifetime_seconds: 3600,
            daily_lease_seconds,
            turn_end_margin_seconds: 60,
        }
    }

    #[test]
    fn a_weight_of_one_charges_exactly_the_unweighted_lifetime() {
        let unweighted = UnweightedLeaseCharge::under(&budget(7200), 3600).unwrap();
        let one = unweighted.weighted_by(1).unwrap();
        assert_eq!(
            one,
            ExternalLeaseCharge {
                budget_weight: 1,
                ..charge(8, 7200)
            }
        );
        assert_eq!(one.weighted_lifetime_seconds(), 3600);

        // The same leases, settlements and refusals as before weights
        // existed: two whole lifetimes fill a 7200s day and a third is
        // refused naming the same totals.
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        for placement in ["T-a", "T-b", "T-c"] {
            allocation(&db, placement);
        }
        lease(&db, "T-a", now, one).unwrap();
        lease(&db, "T-b", now, one).unwrap();
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 2,
                day_seconds: 7200
            }
        );
        let refusal = lease(&db, "T-c", now, one).unwrap_err().to_string();
        assert!(
            refusal.contains("daily lease budget is 7200s")
                && refusal.contains("already hold 7200s")
                && refusal.contains("budget_weight 1 (3600s) does not fit"),
            "{refusal}"
        );
        settle(&db, "T-a", now + 90_000);
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 1,
                day_seconds: 3690
            }
        );
    }

    #[test]
    fn a_weighted_lease_consumes_its_weight_and_is_refused_at_the_boundary() {
        // Weight 4 under a 4h day: one 1h lease reserves 4h of budget, so
        // exactly one fits and a second is refused at the boundary.
        let four = UnweightedLeaseCharge::under(&budget(4 * 3600), 3600)
            .unwrap()
            .weighted_by(4)
            .unwrap();
        assert_eq!(four.weighted_lifetime_seconds(), 4 * 3600);
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        for placement in ["T-a", "T-b", "T-c"] {
            allocation(&db, placement);
        }
        lease(&db, "T-a", now, four).unwrap();
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 1,
                day_seconds: 4 * 3600
            }
        );
        let refusal = lease(&db, "T-b", now, four).unwrap_err().to_string();
        assert!(
            refusal.contains(
                "the node's daily lease budget is 14400s and leases reserved in the last 24 \
                 hours already hold 14400s, so this lease's whole 3600s lifetime at its \
                 provider spec's budget_weight 4 (14400s) does not fit; none is queued"
            ),
            "{refusal}"
        );

        // Settlement releases exactly what was reserved beyond the seconds
        // used: 10s lasted at weight 4 stays charged as 40s, and the other
        // 14360s return to the day.
        settle(&db, "T-a", now + 10_000);
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 0,
                day_seconds: 40
            }
        );
        // A weight-1 lease of the remaining room fits exactly; one second
        // more of weighted room does not.
        let fill = ExternalLeaseCharge {
            lifetime_seconds: 4 * 3600 - 40,
            budget_weight: 1,
            max_concurrent_leases: 8,
            daily_lease_seconds: 4 * 3600,
        };
        let over = ExternalLeaseCharge {
            lifetime_seconds: 4 * 3600 - 39,
            ..fill
        };
        let refusal = lease(&db, "T-b", now, over).unwrap_err().to_string();
        assert!(
            refusal.contains("already hold 40s") && refusal.contains("none is queued"),
            "{refusal}"
        );
        lease(&db, "T-b", now, fill).unwrap();
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 1,
                day_seconds: 4 * 3600
            }
        );
        settle(&db, "T-b", now);
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 0,
                day_seconds: 40
            }
        );
        // A settled lease keeps its weight: the stored row is immutable.
        let weight: i64 = db
            .conn
            .with_sql(|connection| {
                Ok(connection.query_row(
                    "SELECT budget_weight FROM external_execution_lease_budget
                      WHERE placement_thread_id='T-a'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(weight, 4);
    }

    #[test]
    fn a_lease_heavier_than_the_whole_day_is_refused_as_never_admissible() {
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        allocation(&db, "T-a");
        // Weight 3 on a 1h lease is 10800 budget seconds, more than the
        // whole 7200s day even with nothing else held.
        let heavy = UnweightedLeaseCharge::under(&budget(7200), 3600)
            .unwrap()
            .weighted_by(3)
            .unwrap();
        let refusal = lease(&db, "T-a", now, heavy).unwrap_err().to_string();
        assert_eq!(
            refusal,
            "lease refused: this lease's whole 3600s lifetime at budget_weight 3 (10800 budget \
             seconds) exceeds the node's daily budget of 7200 budget seconds; it can never be \
             admitted"
        );
        assert!(!refusal.contains("already hold"), "{refusal}");
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 0,
                day_seconds: 0
            }
        );
        // Exactly the whole day fits once, and the next is the day-is-full
        // refusal, not the never-admissible one.
        let whole_day = UnweightedLeaseCharge::under(&budget(7200), 3600)
            .unwrap()
            .weighted_by(2)
            .unwrap();
        lease(&db, "T-a", now, whole_day).unwrap();
        allocation(&db, "T-b");
        let refusal = lease(&db, "T-b", now, whole_day).unwrap_err().to_string();
        assert!(
            refusal.contains("already hold 7200s")
                && refusal.contains("none is queued")
                && !refusal.contains("never be admitted"),
            "{refusal}"
        );
    }

    #[test]
    fn a_zero_or_out_of_bound_weight_is_refused_before_and_by_the_journal() {
        let unweighted = UnweightedLeaseCharge::under(&budget(7200), 60).unwrap();
        let max = ryeos_external_execution_contract::MAX_LEASE_BUDGET_WEIGHT;
        for weight in [0, max + 1, u32::MAX] {
            let refusal = unweighted.weighted_by(weight).unwrap_err().to_string();
            assert!(
                refusal.contains(&format!(
                    "lease refused: its provider spec's budget_weight {weight} is outside 1..={max}"
                )),
                "{refusal}"
            );
        }
        unweighted.weighted_by(max).unwrap();

        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        allocation(&db, "T-a");
        let zero = ExternalLeaseCharge {
            budget_weight: 0,
            ..charge(2, 7200)
        };
        assert!(lease(&db, "T-a", now, zero).is_err());
        // The journal holds the same bound as the contract.
        for (weight, accepted) in [
            (0, false),
            (i64::from(max) + 1, false),
            (i64::from(max), true),
        ] {
            let inserted = db.conn.with_sql(|connection| {
                Ok(connection.execute(
                    "INSERT INTO external_execution_lease_budget VALUES('T-a',?1,1,?2,NULL,NULL)",
                    params![now, weight],
                )?)
            });
            assert_eq!(inserted.is_ok(), accepted, "weight {weight}");
        }
        assert!(
            db.conn
                .with_sql(|connection| Ok(connection.execute(
                    "UPDATE external_execution_lease_budget SET budget_weight=1",
                    [],
                )?))
                .is_err()
        );
    }

    #[test]
    fn a_held_lease_is_immutable_until_settlement_and_released_once() {
        let (_dir, db) = open();
        let now = 10 * DAY_MS;
        assert!(lease(&db, "T-missing", now, charge(2, 7200)).is_err());
        allocation(&db, "T-a");
        lease(&db, "T-a", now, charge(2, 7200)).unwrap();
        assert!(lease(&db, "T-a", now, charge(2, 7200)).is_err());
        let refused = |sql: &str| {
            db.conn
                .with_sql(|connection| Ok(connection.execute(sql, [])?))
                .is_err()
        };
        assert!(refused("DELETE FROM external_execution_lease_budget"));
        assert!(refused(
            "UPDATE external_execution_lease_budget SET reserved_seconds=1"
        ));
        settle(&db, "T-a", now + 5_000);
        assert_eq!(
            used(&db, now),
            ExternalLeaseUse {
                held: 0,
                day_seconds: 5
            }
        );
        assert!(refused(
            "UPDATE external_execution_lease_budget SET charged_seconds=0"
        ));
    }

    fn worker_row(db: &RuntimeDb, placement: &str, worker: &str, generation: &str) {
        db.conn
            .with_sql(|connection| {
                Ok(connection.execute(
                    "INSERT INTO worker_process (
                        worker_instance_id, boot_identity_hash, session_capsule_hash,
                        boot_epoch, lifecycle_generation, process_identity,
                        control_channel_identity, state, daemon_generation_id,
                        placement_thread_id, cleanup_state, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 1, 1, '{}', 'fd:fixture', 'live', ?4, ?5,
                               'owned', 1, 1)",
                    params![
                        worker,
                        "b".repeat(64),
                        "a".repeat(64),
                        generation,
                        placement
                    ],
                )?)
            })
            .unwrap();
    }

    #[test]
    fn a_lease_of_an_earlier_incarnation_is_found_and_released_only_when_terminated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let lease_charge = ExternalLeaseCharge {
            lifetime_seconds: 60,
            ..charge(2, 8 * 3600)
        };
        let reserved = {
            let db = crate::runtime_db::test_native::TestRuntime::open(&path);
            let mut held = reservation(&db, "held");
            held.admitted_adapter_request_base64 = Some(
                ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}")
                    .unwrap(),
            );
            db.reserve_external_lease_allocation(&held, &binding, &lease_charge)
                .unwrap();
            // A worker row of the running incarnation keeps its lease.
            worker_row(&db, "T-held", "worker-held", "daemon-first");
            assert!(
                db.predecessor_external_lease_placements("daemon-first")
                    .unwrap()
                    .is_empty()
            );
            let lease = db.held_external_lease("T-held").unwrap().unwrap();
            assert_eq!(lease.lifetime_seconds, 60);
            assert_eq!(
                lease.assumed_end_ms().unwrap(),
                lease.reserved_at_ms + 60_000
            );
            db.claim_external_allocation_contact("T-held", &held.request_digest)
                .unwrap();
            db.bind_external_allocation(
                "T-held",
                &ExternalAllocationOccurrence {
                    schema: 1,
                    binding_hash: held.binding_hash.clone(),
                    request_digest: held.request_digest.clone(),
                    occurrence_id: "lease-occurrence".into(),
                    provider_observation_digest: "f".repeat(64),
                },
                ExternalObservationTiming::Startup {
                    deadline_exceeded: false,
                    live_deadline: lillux::time::MonotonicDeadline::after(
                        lillux::time::Duration::from_secs(60),
                    ),
                },
            )
            .unwrap();
            held
        };

        // The daemon restarts as a new incarnation. The lease's worker is of
        // the previous one, so the lease is found; it stays held through the
        // fence and is released only by the observed terminal.
        let db = crate::runtime_db::test_native::TestRuntime::open(&path);
        assert_eq!(
            db.predecessor_external_lease_placements("daemon-second")
                .unwrap(),
            vec!["T-held".to_owned()]
        );
        db.cancel_external_allocation("T-held").unwrap();
        assert_eq!(
            db.external_allocation("T-held").unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );
        assert!(db.held_external_lease("T-held").unwrap().is_some());
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: reserved.binding_hash.clone(),
            request_digest: reserved.request_digest.clone(),
            occurrence_id: "lease-occurrence".into(),
            termination_request_digest: "1".repeat(64),
        };
        assert!(db.begin_external_termination("T-held", &intent).unwrap());
        assert!(db.held_external_lease("T-held").unwrap().is_some());
        db.settle_external_terminal(
            "T-held",
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: reserved.binding_hash.clone(),
                request_digest: reserved.request_digest.clone(),
                occurrence_id: "lease-occurrence".into(),
                termination_request_digest: intent.termination_request_digest.clone(),
                terminal_state: "terminated".into(),
                provider_observation_digest: "2".repeat(64),
            },
        )
        .unwrap();
        assert!(db.held_external_lease("T-held").unwrap().is_none());
        assert!(
            db.predecessor_external_lease_placements("daemon-second")
                .unwrap()
                .is_empty()
        );
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        assert_eq!(used(&db, now).held, 0);
    }

    #[test]
    fn a_session_lease_reserves_with_its_allocation_and_releases_on_settlement() {
        let (_dir, db) = open();
        let binding = crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
        let mut first = reservation(&db, "one");
        first.admitted_adapter_request_base64 = Some(
            ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}")
                .unwrap(),
        );
        let one = charge(1, 8 * 3600);
        let charge_of = |seconds| ExternalLeaseCharge {
            lifetime_seconds: seconds,
            ..one
        };

        // A lease request without a charge, or a charge without a lease
        // request, is refused before anything is reserved.
        let error = db
            .reserve_external_allocation(&first, &binding)
            .unwrap_err()
            .to_string();
        assert!(error.contains("node lease budget"), "{error}");
        let plain = reservation(&db, "plain");
        assert!(
            db.reserve_external_lease_allocation(&plain, &binding, &charge_of(60))
                .is_err()
        );
        assert!(db.external_allocation("T-one").unwrap().is_none());

        db.reserve_external_lease_allocation(&first, &binding, &charge_of(60))
            .unwrap();
        // Replay reserves nothing twice.
        db.reserve_external_lease_allocation(&first, &binding, &charge_of(60))
            .unwrap();
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        assert_eq!(used(&db, now).held, 1);
        db.conn.with_sql(super::super::validate_current).unwrap();

        // The second concurrent lease is refused by the node's ceiling of one.
        let mut second = reservation(&db, "two");
        second.admitted_adapter_request_base64 = first.admitted_adapter_request_base64.clone();
        let refusal = db
            .reserve_external_lease_allocation(&second, &binding, &charge_of(60))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("1 of its 1 concurrent leases"),
            "{refusal}"
        );
        assert!(db.external_allocation("T-two").unwrap().is_none());

        db.cancel_external_allocation("T-one").unwrap();
        assert_eq!(used(&db, now).held, 0);
        db.conn.with_sql(super::super::validate_current).unwrap();
        reserve_session(&db, &plain).unwrap();
    }
}
