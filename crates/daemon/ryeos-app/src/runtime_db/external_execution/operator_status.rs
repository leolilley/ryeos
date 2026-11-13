//! Read-only operator view of what still holds external capacity or a
//! credential profile. Listing grants nothing: every release keeps its own
//! settlement proof, and these reads never transition a row.

use super::qualification_local_start::read_local_start;
use super::*;

/// One allocation not yet in a settled phase, with its journal times.
#[derive(Debug, Clone)]
pub struct UnsettledAllocationRow {
    pub record: ExternalAllocationRecord,
    /// The chain of the dedicated session at this placement, if one is.
    pub session_chain_root_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Who a held credential-profile lock names, resolved from the journals that
/// can own one. `Unresolved` means none of them names the lock owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeldCredentialProfileOwner {
    QualificationStart {
        placement_thread_id: String,
    },
    DedicatedSession {
        placement_thread_id: String,
        chain_root_id: String,
        state: String,
    },
    ContinuationReservation {
        reservation_id: String,
        successor_thread_id: String,
    },
    Unresolved,
}

#[derive(Debug, Clone)]
pub struct HeldCredentialProfileRow {
    pub profile: CredentialProfileRecord,
    pub owner: HeldCredentialProfileOwner,
}

fn unsettled_allocations(conn: &Connection) -> Result<Vec<UnsettledAllocationRow>> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id,created_at_ms,updated_at_ms FROM external_execution_allocation
         WHERE phase NOT IN ('no_contact','contacted_no_occurrence','terminated')
         ORDER BY created_at_ms,placement_thread_id",
    )?;
    let keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    keys.into_iter()
        .map(|(placement, created_at_ms, updated_at_ms)| {
            Ok(UnsettledAllocationRow {
                record: read(conn, &placement)?
                    .context("external allocation disappeared during listing")?,
                session_chain_root_id: conn
                    .query_row(
                        "SELECT chain_root_id FROM dedicated_session WHERE placement_thread_id=?1",
                        [&placement],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?,
                created_at_ms,
                updated_at_ms,
            })
        })
        .collect()
}

fn open_local_starts(conn: &Connection) -> Result<Vec<QualificationLocalStartRecord>> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_qualification_local_start
         WHERE phase!='closed' ORDER BY created_at_ms,placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    placements
        .into_iter()
        .map(|placement| {
            read_local_start(conn, &placement)?
                .context("Q local start disappeared during listing")
        })
        .collect()
}

fn resolve_lock_owner(
    conn: &Connection,
    profile_id: &str,
    lock_owner: &str,
) -> Result<HeldCredentialProfileOwner> {
    if let Some(placement_thread_id) = conn
        .query_row(
            "SELECT placement_thread_id FROM external_qualification_local_start
             WHERE json_extract(record_json,'$.credential_custody.profile_id')=?1
               AND json_extract(record_json,'$.credential_custody.lock_owner')=?2
               AND json_type(record_json,'$.credential_custody.released_at_ms')='null'
             ORDER BY placement_thread_id LIMIT 1",
            [profile_id, lock_owner],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(HeldCredentialProfileOwner::QualificationStart {
            placement_thread_id,
        });
    }
    if let Some((placement_thread_id, chain_root_id, state)) = conn
        .query_row(
            "SELECT placement_thread_id,chain_root_id,state FROM dedicated_session
             WHERE credential_profile_id=?1 AND worker_instance_id=?2
             ORDER BY updated_at_ms DESC,placement_thread_id LIMIT 1",
            [profile_id, lock_owner],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
    {
        return Ok(HeldCredentialProfileOwner::DedicatedSession {
            placement_thread_id,
            chain_root_id,
            state,
        });
    }
    if let Some((reservation_id, successor_thread_id)) = conn
        .query_row(
            "SELECT reservation_id,successor_thread_id FROM credential_profile_reservation
             WHERE reservation_id=?2 AND profile_id=?1 AND state='reserved'",
            [profile_id, lock_owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
    {
        return Ok(HeldCredentialProfileOwner::ContinuationReservation {
            reservation_id,
            successor_thread_id,
        });
    }
    Ok(HeldCredentialProfileOwner::Unresolved)
}

impl RuntimeDb {
    /// Allocations still counted against their capacity owner, oldest first.
    pub(crate) fn unsettled_external_allocations(&self) -> Result<Vec<UnsettledAllocationRow>> {
        self.conn.with_sql(|conn| {
            validate_current(conn)?;
            unsettled_allocations(conn)
        })
    }

    /// Q local starts of any daemon generation that are not closed.
    pub(crate) fn open_qualification_local_starts(
        &self,
    ) -> Result<Vec<QualificationLocalStartRecord>> {
        self.conn.with_sql(open_local_starts)
    }

    /// Credential profiles whose worker fence is held, each with the journal
    /// row that names the lock owner.
    pub(crate) fn held_credential_profiles(&self) -> Result<Vec<HeldCredentialProfileRow>> {
        self.conn.with_sql(|conn| {
            let mut statement = conn.prepare(
                "SELECT profile_id,lock_owner FROM credential_profile
                 WHERE lock_owner IS NOT NULL AND state!='deleted' ORDER BY profile_id",
            )?;
            let held = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            held.into_iter()
                .map(|(profile_id, lock_owner)| {
                    let profile = self
                        .credential_profile_projection(&profile_id)?
                        .context("held credential profile disappeared during listing")?;
                    let owner = resolve_lock_owner(conn, &profile_id, &lock_owner)?;
                    Ok(HeldCredentialProfileRow { profile, owner })
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::qualification_local_start::tests::fixture_retained_credential_journal;
    use super::super::qualification_local_start::transition;
    use super::super::tests::{reservation, reserve};
    use super::*;

    fn open() -> (tempfile::TempDir, crate::runtime_db::test_native::TestRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let db =
            crate::runtime_db::test_native::TestRuntime::open(&dir.path().join("runtime.sqlite3"));
        (dir, db)
    }

    #[test]
    fn unsettled_allocation_listed_until_settled() {
        let (_dir, db) = open();
        let first = reservation(&db, "one");
        reserve(&db, &first).unwrap();
        let rows = db.unsettled_external_allocations().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record.reservation.placement_thread_id, "T-one");
        assert_eq!(rows[0].record.phase, ExternalAllocationPhase::Reserved);
        assert_eq!(rows[0].session_chain_root_id.as_deref(), Some("T-root"));
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &first.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        assert_eq!(
            db.unsettled_external_allocations().unwrap()[0].record.phase,
            ExternalAllocationPhase::ContactPending
        );
        db.settle_external_no_occurrence(
            "T-one",
            &ExternalNoOccurrenceEvidence {
                schema: 1,
                binding_hash: first.binding_hash.clone(),
                request_digest: first.request_digest.clone(),
                provider_observation_digest: "f".repeat(64),
                operator: None,
            },
        )
        .unwrap();
        assert!(db.unsettled_external_allocations().unwrap().is_empty());
    }

    #[test]
    fn held_profile_names_session_owner() {
        let (_dir, db) = open();
        reservation(&db, "one");
        let held = db.held_credential_profiles().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].profile.profile_id, "P-one");
        assert_eq!(held[0].profile.lock_owner.as_deref(), Some("worker-one"));
        assert_eq!(
            held[0].owner,
            HeldCredentialProfileOwner::DedicatedSession {
                placement_thread_id: "T-one".into(),
                chain_root_id: "T-root".into(),
                state: "admitted".into(),
            }
        );
    }

    #[test]
    fn released_custody_not_listed() {
        let (_dir, db) = open();
        db.create_credential_profile(NewCredentialProfile {
            profile_id: "P-loose",
            owner_principal: "fp:operator",
            home_id: "home-loose",
        })
        .unwrap();
        assert!(db.held_credential_profiles().unwrap().is_empty());
        db.acquire_credential_profile("P-loose", "fp:operator", "lock-loose")
            .unwrap();
        let held = db.held_credential_profiles().unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].owner, HeldCredentialProfileOwner::Unresolved);
        db.release_credential_profile("P-loose", "lock-loose")
            .unwrap();
        assert!(db.held_credential_profiles().unwrap().is_empty());
    }

    #[test]
    fn held_profile_names_q_placement_owner() {
        crate::runtime_db::test_native::with_transient(|conn| {
            let current = fixture_retained_credential_journal(conn);
            conn.execute_batch(
                "CREATE TABLE dedicated_session(placement_thread_id TEXT,chain_root_id TEXT,state TEXT,credential_profile_id TEXT,worker_instance_id TEXT,updated_at_ms INTEGER);
                 CREATE TABLE credential_profile_reservation(reservation_id TEXT,successor_thread_id TEXT,profile_id TEXT,state TEXT);",
            )?;
            conn.execute(
                "INSERT INTO credential_profile VALUES('profile','principal','private-home',1,1,?1,'active')",
                [&current.launch_owner],
            )?;
            let mut claimed = current.clone();
            claimed.credential_custody = Some(QualificationLocalCredentialCustody {
                profile_id: "profile".into(),
                owner_principal: "principal".into(),
                home_id: "private-home".into(),
                credential_generation: 1,
                authority_revision: 1,
                lock_owner: current.launch_owner.clone(),
                workload_family: "codex".into(),
                subject_projection_contract: "codex.account.v1".into(),
                contract_digest: "a".repeat(64),
                acquired_at_ms: 1,
                released_at_ms: None,
            });
            transition(conn, &current, &claimed)?;
            assert_eq!(
                resolve_lock_owner(conn, "profile", &current.launch_owner)?,
                HeldCredentialProfileOwner::QualificationStart {
                    placement_thread_id: current.placement_thread_id.clone(),
                }
            );
            let open = open_local_starts(conn)?;
            assert_eq!(open.len(), 1);
            assert_eq!(open[0].placement_thread_id, current.placement_thread_id);

            let mut released = claimed.clone();
            released.credential_custody.as_mut().unwrap().released_at_ms = Some(2);
            released.updated_at_ms = 2;
            transition(conn, &claimed, &released)?;
            assert_eq!(
                resolve_lock_owner(conn, "profile", &current.launch_owner)?,
                HeldCredentialProfileOwner::Unresolved
            );
            Ok(())
        });
    }
}
