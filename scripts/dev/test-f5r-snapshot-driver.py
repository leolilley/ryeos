#!/usr/bin/env python3
"""Unit tests for f5r-snapshot-driver.py; run with python3 -B."""
import importlib.util
import io
import json
import tempfile
import unittest
import unittest.mock
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "driver", Path(__file__).with_name("f5r-snapshot-driver.py"))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

KEY = "rnd_SECRETKEY123"
H = "a" * 64
SNAP = "b" * 64
QUAL = "c" * 64


class FakeResp:
    def __init__(self, status, body):
        self.status = status
        self._body = json.dumps(body).encode()

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class Fakes:
    def __init__(self):
        self.cli_calls = []
        self.http_calls = []
        self.readiness = [("err", None), ("ok", None)]
        self.readiness_phase = "bound"
        self.verify_phase = "observed"
        self.fail_on = None
        self.produce_phase = "bound"
        self.source_gets = [{"status": "running"}]
        self.source_override = {}
        self.terminate_status = 200
        self.create_has_id = True

    def cli(self, argv, env):
        verb = argv[2]
        self.cli_calls.append(argv)
        if verb == self.fail_on:
            return 1, "", f"boom {KEY}"
        if verb == "produce-runtime-snapshot":
            return 0, json.dumps({"result": {"intent": {"operation_id": SNAP}, "phase": self.produce_phase}}), ""
        if verb == "observe-runtime-snapshot-readiness":
            kind, _ = self.readiness.pop(0) if self.readiness else ("ok", None)
            if kind == "err":
                return 1, "", "not available"
            return 0, json.dumps({"intent": {"operation_id": SNAP}, "phase": "bound",
                                  "locator": {"snapshot_id": "snp-1"}, "readiness": {"size_bytes": 5}}), ""
        if verb == "runtime-snapshot":
            return 0, json.dumps({"intent": {"operation_id": SNAP}, "phase": self.readiness_phase,
                                  "locator": {"snapshot_id": "snp-1"}}), ""
        if verb == "create-runtime-snapshot-qualification":
            return 0, json.dumps({"intent": {"operation_id": QUAL}, "phase": "occurrence_bound"}), ""
        if verb == "verify-runtime-snapshot-qualification":
            return 0, json.dumps({"phase": self.verify_phase}), ""
        if verb == "terminate-runtime-snapshot-qualification":
            return 0, json.dumps({"phase": "terminal"}), ""
        if verb == "runtime-snapshot-qualification":
            return 0, json.dumps({"intent": {"operation_id": QUAL}, "attestation_hash": "d" * 64}), ""
        return 1, "", "unknown"

    def opener(self, request, timeout):
        body = json.loads(request.data) if request.data else None
        self.http_calls.append((request.get_method(), request.full_url, body,
                                request.get_header("Authorization")))
        url = request.full_url.split("?")[0]
        if request.get_method() == "POST" and url.endswith("/sandboxes"):
            return FakeResp(201, {"id": "sbx-1"} if self.create_has_id else {})
        if request.get_method() == "POST" and url.endswith("/terminate"):
            if self.terminate_status >= 400:
                raise driver.urllib.error.HTTPError(url, self.terminate_status, "x", {}, io.BytesIO(b"{}"))
            return FakeResp(200, {"id": url.split("/")[-2]})
        if request.get_method() == "GET":
            step = self.source_gets.pop(0) if len(self.source_gets) > 1 else self.source_gets[0]
            base = {"id": "sbx-1", "networkPolicy": {"type": "deny-all"}, "region": "oregon", "plan": "starter"}
            base.update(step)
            base.update(self.source_override)
            return FakeResp(200, base)
        return FakeResp(200, {})


class DriverTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        (self.dir / "handoff.json").write_text(json.dumps(
            {"owner_witness_hash": H, "owner_manifest_hash": "e" * 64}))
        self.fakes = Fakes()
        self.logs = []

    def tearDown(self):
        self.tmp.cleanup()

    def run_main(self, extra=()):
        argv = ["--app-root", str(self.dir), "--handoff", str(self.dir / "handoff.json"),
                "--binding-id", "prod", "--qualification-binding-id", "qual",
                "--render-key-file", "unused", "--owner-id", "own-1", "--region", "oregon",
                "--plan", "starter", "--out", str(self.dir / "out"),
                "--state", str(self.dir / "state.json"), *extra]
        return driver.main(argv, cli=self.fakes.cli, opener=self.fakes.opener,
                           sleep=lambda s: None, clock=iter(range(0, 10000)).__next__,
                           key_reader=lambda a: KEY, log=self.logs.append)

    def verbs(self):
        return [c[2] for c in self.fakes.cli_calls]

    def test_happy_path(self):
        self.assertEqual(self.run_main(), 0)
        handoff = json.loads((self.dir / "out" / "snapshot-handoff.json").read_text())
        self.assertEqual(handoff["snapshot_operation_id"], SNAP)
        self.assertEqual(handoff["snapshot_id"], "snp-1")
        self.assertEqual(handoff["qualification_operation_id"], QUAL)
        self.assertEqual(handoff["attestation_hash"], "d" * 64)
        self.assertEqual(handoff["owner_manifest_hash"], "e" * 64)
        create = self.fakes.http_calls[0]
        self.assertEqual(create[2]["networkPolicy"], {"type": "deny-all"})
        self.assertEqual(self.fakes.http_calls[-1][1].split("?")[0],
                         "https://api.render.com/v1/sandboxes/sbx-1/terminate")
        produce = [c for c in self.fakes.cli_calls if c[2] == "produce-runtime-snapshot"][0]
        payload = json.loads(produce[3])
        self.assertEqual(payload["source"]["source_occurrence_id"], "sbx-1")
        self.assertEqual(payload["source"]["witness_source"], {"kind": "local_capture"})

    def test_resume_after_crash_between_produce_and_readiness(self):
        state = {"steps": {"source_sandbox": {"status": "done", "id": "sbx-1"},
                           "source_ready": {"status": "done"},
                           "produce": {"status": "done", "operation_id": SNAP}}}
        (self.dir / "state.json").write_text(json.dumps(state))
        self.assertEqual(self.run_main(), 0)
        self.assertNotIn("produce-runtime-snapshot", self.verbs())
        self.assertFalse([c for c in self.fakes.http_calls if c[0] == "POST" and c[1].endswith("/sandboxes")])

    def test_attempted_unresolved_produce_refuses(self):
        state = {"steps": {"source_sandbox": {"status": "done", "id": "sbx-1"},
                           "source_ready": {"status": "done"},
                           "produce": {"status": "attempted"}}}
        (self.dir / "state.json").write_text(json.dumps(state))
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.fakes.cli_calls, [])
        self.assertEqual(self.fakes.http_calls, [])

    def seed(self, **steps):
        (self.dir / "state.json").write_text(json.dumps({"steps": steps}))

    def posts(self):
        return [c for c in self.fakes.http_calls if c[0] == "POST"]

    def terminates(self):
        return [c for c in self.fakes.http_calls if c[1].split("?")[0].endswith("/terminate")]

    def test_source_polls_creating_then_running(self):
        self.fakes.source_gets = [{"status": "creating"}, {"status": "running"}]
        self.assertEqual(self.run_main(), 0)
        self.assertEqual(len([c for c in self.fakes.http_calls if c[0] == "GET"]), 2)

    def test_source_not_deny_all_fails_without_produce_and_terminates(self):
        self.fakes.source_override = {"networkPolicy": {"type": "allow-list"}}
        self.assertEqual(self.run_main(), 1)
        self.assertNotIn("produce-runtime-snapshot", self.verbs())
        self.assertEqual(len(self.terminates()), 1)

    def test_source_terminal_status_fails(self):
        self.fakes.source_gets = [{"status": "terminated"}]
        self.assertEqual(self.run_main(), 1)
        self.assertNotIn("produce-runtime-snapshot", self.verbs())
        self.assertEqual(len(self.terminates()), 1)

    def test_source_region_mismatch_fails(self):
        self.fakes.source_override = {"region": "frankfurt"}
        self.assertEqual(self.run_main(), 1)
        self.assertNotIn("produce-runtime-snapshot", self.verbs())

    def test_produce_quarantined_fails_and_tears_down(self):
        self.fakes.produce_phase = "quarantined"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(len(self.terminates()), 1)

    def test_produce_attempt_pending_refuses_without_teardown(self):
        self.fakes.produce_phase = "attempt_pending"
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.terminates(), [])
        self.assertTrue(any("stay up" in line for line in self.logs))

    def test_produce_reserved_refuses_and_stays_refused(self):
        self.fakes.produce_phase = "reserved"
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.terminates(), [])
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.verbs().count("produce-runtime-snapshot"), 1)
        self.assertNotIn("continue-runtime-snapshot-create", self.verbs())

    def test_readiness_attempt_pending_refuses_without_teardown(self):
        self.fakes.readiness = [("err", None)] * 3
        self.fakes.readiness_phase = "attempt_pending"
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.terminates(), [])

    def test_terminate_500_does_not_mask_first_error(self):
        self.fakes.readiness = [("err", None)] * 3
        self.fakes.readiness_phase = "quarantined"
        self.fakes.terminate_status = 500
        err = io.StringIO()
        with unittest.mock.patch("sys.stderr", err):
            self.assertEqual(self.run_main(), 1)
        self.assertIn("quarantined", err.getvalue())
        self.assertNotIn("HTTP 500", err.getvalue().split("(state kept")[0])

    def test_qualification_attempted_refuses(self):
        self.seed(source_sandbox={"status": "done", "id": "sbx-1"}, source_ready={"status": "done"},
                  produce={"status": "done", "operation_id": SNAP}, readiness={"status": "done"},
                  qualification={"status": "attempted"})
        self.assertEqual(self.run_main(), 2)
        self.assertNotIn("create-runtime-snapshot-qualification", self.verbs())

    def test_verify_attempted_refuses_and_resolved_flag_adopts(self):
        self.seed(source_sandbox={"status": "done", "id": "sbx-1"}, source_ready={"status": "done"},
                  produce={"status": "done", "operation_id": SNAP}, readiness={"status": "done"},
                  qualification={"status": "done", "operation_id": QUAL}, verify={"status": "attempted"})
        self.assertEqual(self.run_main(), 2)
        self.assertNotIn("verify-runtime-snapshot-qualification", self.verbs())
        self.assertEqual(self.run_main(["--verify-resolved"]), 0)
        self.assertNotIn("verify-runtime-snapshot-qualification", self.verbs())

    def test_adoption_flags(self):
        self.assertEqual(self.run_main(["--source-sandbox-id", "sbx-1", "--snapshot-operation-id", SNAP,
                                        "--qualification-operation-id", QUAL]), 0)
        self.assertEqual(self.posts()[0][1].split("?")[0].split("/")[-1], "terminate")
        self.assertNotIn("produce-runtime-snapshot", self.verbs())
        self.assertNotIn("create-runtime-snapshot-qualification", self.verbs())
        self.assertEqual([c[0] for c in self.fakes.http_calls[:1]], ["GET"])

    def test_adopted_source_is_validated(self):
        self.fakes.source_override = {"networkPolicy": {"type": "allow-list"}}
        self.assertEqual(self.run_main(["--source-sandbox-id", "sbx-1"]), 1)
        self.assertNotIn("produce-runtime-snapshot", self.verbs())

    def test_bad_handoff_digest_exits_2(self):
        (self.dir / "handoff.json").write_text(json.dumps({"owner_witness_hash": "zz", "owner_manifest_hash": H}))
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.fakes.http_calls, [])

    def test_argv_shapes(self):
        self.assertEqual(self.run_main(), 0)
        by_verb = {}
        for c in self.fakes.cli_calls:
            by_verb.setdefault(c[2], c)
        self.assertEqual(by_verb["observe-runtime-snapshot-readiness"][3:], [SNAP])
        self.assertEqual(by_verb["verify-runtime-snapshot-qualification"][3:], [QUAL])
        self.assertEqual(by_verb["terminate-runtime-snapshot-qualification"][3:], [QUAL])
        self.assertEqual(len(by_verb["produce-runtime-snapshot"]), 4)
        self.assertEqual(json.loads(by_verb["create-runtime-snapshot-qualification"][3]),
                         {"qualification_binding_id": "qual", "snapshot_operation_id": SNAP})
        self.assertEqual(len(by_verb["create-runtime-snapshot-qualification"]), 4)

    def test_terminate_uses_stored_id_only_and_never_without_id(self):
        self.assertEqual(self.run_main(), 0)
        self.assertEqual([c[1].split("?")[0] for c in self.terminates()],
                         ["https://api.render.com/v1/sandboxes/sbx-1/terminate"])

    def test_create_without_id_never_terminates(self):
        self.fakes.create_has_id = False
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(self.terminates(), [])
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(len(self.posts()), 1)

    def test_lockfile_refusal(self):
        (self.dir / "state.json.lock").write_text("")
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.fakes.http_calls, [])
        (self.dir / "state.json.lock").unlink()
        self.assertEqual(self.run_main(), 0)
        self.assertFalse((self.dir / "state.json.lock").exists())

    def test_attempted_source_create_refuses(self):
        (self.dir / "state.json").write_text(json.dumps({"steps": {"source_sandbox": {"status": "attempted"}}}))
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.fakes.http_calls, [])

    def test_readiness_failed_exits_1_keeps_state_and_tears_down_source(self):
        self.fakes.readiness = [("err", None)] * 3
        self.fakes.readiness_phase = "quarantined"
        self.assertEqual(self.run_main(), 1)
        state = json.loads((self.dir / "state.json").read_text())
        self.assertEqual(state["steps"]["produce"]["status"], "done")
        self.assertEqual(state["steps"]["source_terminate"]["status"], "done")
        self.assertTrue(self.fakes.http_calls[-1][1].split("?")[0].endswith("/sbx-1/terminate"))
        self.assertFalse((self.dir / "out" / "snapshot-handoff.json").exists())

    def test_verify_failure_terminates_qualification_and_source(self):
        self.fakes.verify_phase = "quarantined"
        self.assertEqual(self.run_main(), 1)
        self.assertIn("terminate-runtime-snapshot-qualification", self.verbs())
        self.assertTrue(self.fakes.http_calls[-1][1].split("?")[0].endswith("/terminate"))

    def test_produce_failure_not_retried(self):
        self.fakes.fail_on = "produce-runtime-snapshot"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(self.verbs().count("produce-runtime-snapshot"), 1)
        self.assertEqual(self.run_main(), 2)
        self.assertEqual(self.verbs().count("produce-runtime-snapshot"), 1)

    def test_no_key_in_logs_state_or_handoff(self):
        self.fakes.fail_on = "produce-runtime-snapshot"
        self.run_main()
        self.fakes.fail_on = None
        (self.dir / "state.json").unlink()
        self.run_main()
        blob = "\n".join(self.logs)
        for path in (self.dir / "state.json", self.dir / "out" / "snapshot-handoff.json"):
            if path.exists():
                blob += path.read_text()
        self.assertNotIn(KEY, blob)
        self.assertTrue(self.logs)

    def args_for(self, **kw):
        ns = driver.argparse.Namespace(render_key_file=None, env_file=str(self.dir / "env"))
        ns.__dict__.update(kw)
        return ns

    def test_env_file_parsing_and_precedence(self):
        env = self.dir / "env"
        env.write_text('# c\n\nOTHER=x\nRENDER_API_KEY="from-file"\n')
        env.chmod(0o600)
        self.assertEqual(driver.read_key(self.args_for(), {}), "from-file")
        self.assertEqual(driver.read_key(self.args_for(), {"RENDER_API_KEY": "from-env"}), "from-env")
        keyfile = self.dir / "k"
        keyfile.write_text("from-keyfile\n")
        self.assertEqual(driver.read_key(self.args_for(render_key_file=str(keyfile)),
                                         {"RENDER_API_KEY": "from-env"}), "from-keyfile")
        env.write_text("RENDER_API_KEY='single'\n")
        self.assertEqual(driver.read_key(self.args_for(), {}), "single")

    def test_env_file_permissions_refused_without_leak(self):
        env = self.dir / "env"
        env.write_text("RENDER_API_KEY=secretvalue99\n")
        env.chmod(0o644)
        with self.assertRaises(driver.Refusal) as ctx:
            driver.read_key(self.args_for(), {})
        self.assertIn("chmod 600", str(ctx.exception))
        self.assertNotIn("secretvalue99", str(ctx.exception))

    def test_no_key_refuses(self):
        with self.assertRaises(driver.Refusal):
            driver.read_key(self.args_for(), {})

    def test_env_file_key_not_logged_end_to_end(self):
        env = self.dir / "env"
        env.write_text("RENDER_API_KEY=envfilekey777\n")
        env.chmod(0o600)
        argv = ["--app-root", str(self.dir), "--handoff", str(self.dir / "handoff.json"),
                "--binding-id", "prod", "--qualification-binding-id", "qual",
                "--env-file", str(env), "--owner-id", "own-1", "--region", "oregon",
                "--plan", "starter", "--out", str(self.dir / "out"),
                "--state", str(self.dir / "state.json")]
        with unittest.mock.patch.dict("os.environ", {}, clear=False) as e:
            e.pop("RENDER_API_KEY", None)
            self.fakes.fail_on = "produce-runtime-snapshot"
            self.fakes.cli_calls.clear()
            orig = self.fakes.cli
            self.fakes.cli = lambda a, en: (1, "", "boom envfilekey777") if a[2] == "produce-runtime-snapshot" else orig(a, en)
            driver.main(argv, cli=self.fakes.cli, opener=self.fakes.opener, sleep=lambda s: None,
                        clock=iter(range(0, 10000)).__next__, log=self.logs.append)
        blob = "\n".join(self.logs) + (self.dir / "state.json").read_text()
        self.assertNotIn("envfilekey777", blob)
        self.assertTrue(self.logs)

    def test_requires_app_root_or_controller_url(self):
        argv = ["--handoff", "h", "--binding-id", "b", "--qualification-binding-id", "q",
                "--owner-id", "o", "--region", "r", "--plan", "starter",
                "--out", "o", "--state", "s"]
        self.assertEqual(driver.main(argv), 2)


if __name__ == "__main__":
    unittest.main()
