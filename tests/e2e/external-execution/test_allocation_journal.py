"""Exercise production SQLite DDL, without Cargo, daemon or cloud contact.

These tests execute the exact Rust-owned schema strings. They qualify SQL
constraints/atomicity only, not the Rust API, backend or external lifecycle.
"""

import json
from pathlib import Path
import re
import sqlite3
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]
RUNTIME = ROOT / "crates/daemon/ryeos-app/src/runtime_db.rs"
EXTERNAL = RUNTIME.with_suffix("") / "external_execution.rs"


def raw_constant(path, name):
    source = path.read_text()
    matches = re.findall(r'const ' + name + r': &str = r#"(.*?)"#;', source, re.S)
    if len(matches) != 1:
        raise AssertionError(f"expected one exact Rust SQL constant: {name}")
    return matches[0]


def database(path=":memory:"):
    connection = sqlite3.connect(path, isolation_level=None)
    connection.executescript(raw_constant(RUNTIME, "SCHEMA_SQL"))
    connection.executescript(raw_constant(EXTERNAL, "GUARD_SQL") + ";")
    connection.execute("INSERT INTO external_execution_guard VALUES(1,1,0)")
    connection.executescript(raw_constant(EXTERNAL, "JOURNAL_SQL"))
    connection.execute("""
        INSERT INTO credential_profile(profile_id,owner_principal,home_id,
            credential_generation,state,login_epoch,lock_owner,
            created_at_ms,updated_at_ms,authority_revision)
        VALUES('P-one','operator','home',1,'active',0,'worker',1,1,1)
    """)
    connection.execute("""
        INSERT INTO execution_workspace(workspace_id,thread_id,launch_owner,
            base_snapshot,root_path,state,created_at_ms,updated_at_ms)
        VALUES('W-one','T-one','dedicated_worker_session','base','/fixture','ready',1,1)
    """)
    connection.execute("""
        INSERT INTO dedicated_session(placement_thread_id,chain_root_id,owner_principal,
            admitted_capsule_hash,worker_instance_id,worker_boot_epoch,workspace_id,
            candidate_required,candidate_disposition,credential_profile_id,
            credential_generation,state,send_boundary,created_at_ms,updated_at_ms)
        VALUES('T-one','T-root','operator','capsule','worker',1,'W-one',
            1,'owner_decision','P-one',1,'admitted','none',1,1)
    """)
    return connection


class ExternalAllocationSqlTests(unittest.TestCase):
    def setUp(self):
        self.db = database()
        self.addCleanup(self.db.close)

    def reserve(self, placement="T-one"):
        self.db.execute(
            "INSERT INTO external_execution_allocation VALUES(?, 'capacity', ?, 'reserved', NULL, 1, 1)",
            (placement, json.dumps({"request_digest": "same-request"})))

    def phase(self, phase):
        self.db.execute("UPDATE external_execution_allocation SET phase=? WHERE placement_thread_id='T-one'",
                        (phase,))

    def guard(self):
        return self.db.execute("SELECT unsettled FROM external_execution_guard").fetchone()[0]

    def test_exact_stable_guard_ddl_is_preserved_by_sqlite(self):
        self.assertEqual(self.db.execute(
            "SELECT sql FROM sqlite_master WHERE name='external_execution_guard'").fetchone()[0],
            raw_constant(EXTERNAL, "GUARD_SQL"))

    def test_reservation_and_guard_commit_or_rollback_together(self):
        self.db.execute("BEGIN IMMEDIATE")
        self.reserve()
        self.assertEqual(self.guard(), 1)
        self.db.rollback()
        self.assertEqual(self.guard(), 0)
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM external_execution_allocation").fetchone()[0], 0)

    def test_missing_guard_aborts_reservation(self):
        self.db.execute("DELETE FROM external_execution_guard")
        with self.assertRaises(sqlite3.IntegrityError):
            self.reserve()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM external_execution_allocation").fetchone()[0], 0)

    def test_before_contact_cancel_settles_once(self):
        self.reserve()
        self.phase("no_contact")
        self.assertEqual(self.guard(), 0)
        with self.assertRaises(sqlite3.IntegrityError):
            self.phase("reserved")

    def test_pending_contact_cannot_become_uncontacted(self):
        self.reserve()
        self.phase("contact_pending")
        for phase in ("reserved", "no_contact"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.phase(phase)
        self.phase("quarantined")
        self.assertEqual(self.guard(), 1)
        with self.assertRaises(sqlite3.IntegrityError):
            self.phase("no_contact")

    def test_contact_claim_cas_has_only_one_winner(self):
        self.reserve()
        command = """UPDATE external_execution_allocation SET phase='contact_pending'
                     WHERE placement_thread_id='T-one' AND phase='reserved'"""
        self.assertEqual(self.db.execute(command).rowcount, 1)
        self.assertEqual(self.db.execute(command).rowcount, 0)
        self.assertEqual(self.guard(), 1)

    def test_record_and_owner_deletion_are_refused(self):
        self.reserve()
        for table in ("external_execution_allocation", "dedicated_session",
                      "credential_profile", "execution_workspace"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"DELETE FROM {table}")
        self.assertEqual(self.guard(), 1)

    def test_local_cleanup_cannot_release_credential_or_candidate(self):
        self.reserve()
        for command in (
            "UPDATE credential_profile SET lock_owner=NULL",
            "UPDATE credential_profile SET credential_generation=2",
            "UPDATE credential_profile SET home_id='replacement'",
            "UPDATE credential_profile SET owner_principal='replacement'",
            "UPDATE dedicated_session SET state='terminal'",
            "UPDATE dedicated_session SET state='freezing'",
            "UPDATE dedicated_session SET candidate_snapshot_hash='unproved'",
            "UPDATE dedicated_session SET worker_boot_epoch=2",
            "UPDATE dedicated_session SET worker_instance_id='replacement'",
        ):
            with self.subTest(command=command), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(command)
        self.db.execute("UPDATE dedicated_session SET state='outcome_unknown'")
        self.assertEqual(self.guard(), 1)

    def test_workspace_cannot_be_rebound_frozen_or_closed_locally(self):
        self.reserve()
        for assignment in ("base_snapshot='replacement'", "root_path='/replacement'",
                           "thread_id='T-other'", "launch_owner='other'",
                           "frozen_snapshot_hash='unproved'", "state='freezing'",
                           "state='destroying'", "state='closing'", "state='closed'"):
            with self.subTest(assignment=assignment), self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE execution_workspace SET {assignment}")
        self.db.execute("UPDATE execution_workspace SET state='orphaned'")
        self.assertEqual(self.guard(), 1)

    def test_no_contact_settlement_allows_original_owner_release(self):
        self.reserve()
        self.phase("no_contact")
        self.db.execute("UPDATE credential_profile SET lock_owner=NULL")
        self.db.execute("UPDATE dedicated_session SET state='terminal'")
        self.assertEqual(self.guard(), 0)

    def test_identity_rebinding_is_refused(self):
        self.reserve()
        for assignment in ("capacity_owner='new'", "reservation_json='{}'", "placement_thread_id='T-other'"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE external_execution_allocation SET {assignment}")

    def test_partial_settlement_preserves_other_obligations(self):
        self.reserve()
        self.reserve("T-other")
        self.phase("no_contact")
        self.assertEqual(self.guard(), 1)

    def test_crash_reopen_retains_pending_contact(self):
        with tempfile.TemporaryDirectory(prefix="ryeos-external-journal-test-") as directory:
            path = str(Path(directory) / "runtime.sqlite3")
            db = database(path)
            db.execute("INSERT INTO external_execution_allocation VALUES('T-one','capacity','{}','reserved',NULL,1,1)")
            db.execute("UPDATE external_execution_allocation SET phase='contact_pending'")
            db.close()
            with sqlite3.connect(path) as recovered:
                self.assertEqual(recovered.execute("SELECT unsettled FROM external_execution_guard").fetchone()[0], 1)
                self.assertEqual(recovered.execute("SELECT phase FROM external_execution_allocation").fetchone()[0], "contact_pending")
                with self.assertRaises(sqlite3.IntegrityError):
                    recovered.execute("UPDATE external_execution_allocation SET phase='reserved'")

    def channel_and_frame(self):
        self.reserve()
        self.db.execute("""INSERT INTO external_execution_channel
            VALUES('T-one','binding','{}','prepared',NULL,NULL,NULL)""")
        self.db.execute("""INSERT INTO external_execution_frame
            VALUES('binding','owner_to_supervisor',1,1,'digest','{}',2,0,'pending')""")

    def test_channel_keys_and_occurrence_are_not_replaceable(self):
        self.channel_and_frame()
        for assignment in ("binding_digest='other'", "binding_json='replacement'",
                           "placement_thread_id='T-other'"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE external_execution_channel SET {assignment}")

    def test_retained_import_is_immutable_and_cannot_be_collected_early(self):
        self.channel_and_frame()
        self.db.execute("INSERT INTO external_execution_import VALUES('binding','snapshot','evidence','completion','frame')")
        for statement in ("UPDATE external_execution_import SET snapshot_hash='other'",
                          "DELETE FROM external_execution_import"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(statement)
        self.assertEqual(self.db.execute("SELECT snapshot_hash,evidence_blob_hash FROM external_execution_import").fetchone(),
                         ("snapshot", "evidence"))

    def test_candidate_import_roots_commit_atomically(self):
        self.channel_and_frame()
        self.db.execute("BEGIN IMMEDIATE")
        self.db.execute("INSERT INTO external_execution_import VALUES('binding','snapshot','evidence','completion','frame')")
        self.db.rollback()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM external_execution_import").fetchone()[0], 0)

    def test_sticky_revocation_cannot_be_rewritten_or_cleared(self):
        self.channel_and_frame()
        self.db.execute("INSERT INTO external_execution_revocation VALUES('binding','cancel','wire')")
        for statement in ("UPDATE external_execution_revocation SET frame_digest='other'",
                          "DELETE FROM external_execution_revocation"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(statement)
        self.assertEqual(self.guard(), 1)

    def test_external_frames_are_single_claim_then_single_completion(self):
        self.channel_and_frame()
        for state in ("applied", "pending"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute("UPDATE external_execution_frame SET application=?", (state,))
        claim = "UPDATE external_execution_frame SET application='claimed' WHERE application='pending'"
        self.assertEqual(self.db.execute(claim).rowcount, 1)
        self.assertEqual(self.db.execute(claim).rowcount, 0)
        self.db.execute("UPDATE external_execution_frame SET application='applied'")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE external_execution_frame SET application='claimed'")

    def test_transcript_cannot_be_rewritten_or_deleted_before_cleanup(self):
        self.channel_and_frame()
        for assignment in ("frame_json='other'", "ordinal=2", "sequence=2",
                           "frame_digest='other'", "frame_bytes=3",
                           "acknowledged_peer_sequence=1"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"UPDATE external_execution_frame SET {assignment}")
        for table in ("external_execution_channel", "external_execution_frame"):
            with self.assertRaises(sqlite3.IntegrityError):
                self.db.execute(f"DELETE FROM {table}")

    def test_transcript_refuses_duplicate_global_ordinals(self):
        self.channel_and_frame()
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("""INSERT INTO external_execution_frame
                VALUES('binding','supervisor_to_owner',1,1,'second','{}',2,0,'pending')""")

    def test_only_unclaimed_execution_input_can_be_revoked(self):
        self.channel_and_frame()
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE external_execution_frame SET application='revoked'")
        # Use new rows: frame bytes are immutable even before application.
        wire = json.dumps({"frame": {"payload": {"kind": "protocol_bytes"}}})
        self.db.execute("""INSERT INTO external_execution_frame
            VALUES('binding','owner_to_supervisor',2,2,'next',?, ?,0,'pending')""", (wire, len(wire)))
        self.db.execute("UPDATE external_execution_frame SET application='revoked' WHERE sequence=2")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE external_execution_frame SET application='claimed' WHERE sequence=2")
        self.db.execute("""INSERT INTO external_execution_frame
            VALUES('binding','owner_to_supervisor',3,3,'last',?, ?,0,'pending')""", (wire, len(wire)))
        self.db.execute("UPDATE external_execution_frame SET application='claimed' WHERE sequence=3")
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("UPDATE external_execution_frame SET application='revoked' WHERE sequence=3")


if __name__ == "__main__":
    unittest.main()
