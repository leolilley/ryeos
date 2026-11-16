#!/usr/bin/env python3
"""Focused tests for the supervised-node smoke's own logic.

Every `ryeos` call is a fake; nothing starts a node, reaches a network or
spends anything.
"""

import importlib.util
import json
from pathlib import Path
import types
import unittest

ROOT = Path(__file__).resolve().parents[2]
LIB = ROOT / ".ai/tools/ryeos/development/supervised-smoke/lib/supervised_smoke.py"
spec = importlib.util.spec_from_file_location("supervised_smoke", LIB)
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

PY_OK = "SMOKE_PY interp=/usr/bin/python3 path=/usr/local/bin:/usr/bin read=ok write=ok dns=ok https=ok"
SH_OK = "SMOKE_SH path=/usr/bin read=ok write=ok dns=ok https=ok curl=yes"


def args(**extra):
    base = dict(app_root=Path("/node"), ryeos_bin="ryeos", manual_directives=None,
                allow_restart=False, paid=False, keep=False)
    base.update(extra)
    return types.SimpleNamespace(**base)


class Fake:
    """Answers `ryeos` argv by its leading words."""

    def __init__(self, replies):
        self.replies, self.calls = replies, []

    def __call__(self, argv, cwd=None, timeout=300):
        self.calls.append(argv[1:])
        for prefix, reply in self.replies:
            if argv[1:1 + len(prefix)] == list(prefix):
                return reply(argv) if callable(reply) else reply
        return 0, "", ""


class Parsing(unittest.TestCase):
    def test_tokens_survive_json_escaping(self):
        text = json.dumps({"stdout": PY_OK + "\n"})
        tokens = smoke.parse_tokens(text, "SMOKE_PY")
        self.assertEqual(tokens["read"], "ok")
        self.assertEqual(tokens["path"], "/usr/local/bin:/usr/bin")

    def test_no_marker(self):
        self.assertIsNone(smoke.parse_tokens("nothing", "SMOKE_PY"))

    def test_redaction(self):
        text = smoke.redact("Authorization: Bearer abcdefghijklmnop1234 api_key=ZZZZZZZZZZZZZZZZZZ")
        self.assertNotIn("abcdefghijkl", text)
        self.assertNotIn("ZZZZZZZZ", text)

    def test_find_key(self):
        self.assertTrue(smoke.has_key({"a": [{"Running": {}}]}, "Running"))
        self.assertIsNone(smoke.find_key({"a": 1}, "fires"))


class Checks(unittest.TestCase):
    def test_status_and_doctor(self):
        ok = {"checks": [{"name": n, "status": "ok", "detail": {}}
                         for n in ("host_services", "isolation", "capabilities")]
              + [{"name": "wall_clock", "status": "warning", "detail": {"x": 1}}]}
        run = Fake([(("node", "status"), (0, json.dumps({"Running": {"ready_at": "t", "service_window": None}}), "")),
                    (("node", "doctor"), (0, json.dumps(ok), ""))])
        s = smoke.Smoke(args(), run=run)
        s.check("status", s.node_status)
        s.check("doctor", s.node_doctor)
        got = {r.name: r.status for r in s.results}
        self.assertEqual(got["status"], "PASS")
        self.assertEqual(got["node doctor"], "PASS")
        self.assertEqual(got["doctor warning: wall_clock"], "WARN")
        self.assertEqual(got["doctor host-service and scope rows"], "PASS")

    def test_missing_rows_fail_and_not_running_fails(self):
        run = Fake([(("node", "status"), (0, json.dumps({"Stopped": {}}), "")),
                    (("node", "doctor"), (0, json.dumps({"checks": []}), ""))])
        s = smoke.Smoke(args(), run=run)
        s.check("status", s.node_status)
        s.check("doctor", s.node_doctor)
        got = {r.name: r.status for r in s.results}
        self.assertEqual(got["status"], "FAIL")
        self.assertEqual(got["doctor host-service and scope rows"], "FAIL")

    def test_a_crashing_check_is_its_own_failure(self):
        s = smoke.Smoke(args(), run=Fake([]))
        s.check("boom", lambda: 1 / 0)
        self.assertEqual(s.results[0].status, "FAIL")

    def test_graph_requires_the_retry_note(self):
        good = Fake([(("execute",), (0, "completed retry not attempted: x", ""))])
        s = smoke.Smoke(args(), run=good)
        self.assertEqual(s.graph_check(Path("/m"))[0], "PASS")
        bad = Fake([(("execute",), (0, "completed", ""))])
        self.assertEqual(smoke.Smoke(args(), run=bad).graph_check(Path("/m"))[0], "FAIL")

    def test_paid_check(self):
        s = smoke.Smoke(args(), run=Fake([(("execute",), (0, "status continued ... E2E_FORCE_CONTINUE_READY", ""))]))
        self.assertEqual(s.paid_check(Path("/m"))[0], "PASS")
        s = smoke.Smoke(args(), run=Fake([(("execute",), (1, "predecessor has no checkpoint to resume from", ""))]))
        self.assertEqual(s.paid_check(Path("/m"))[0], "FAIL")

    def test_pinned_probes_and_always_deregisters(self):
        def execute(argv):
            return 0, (PY_OK if "tool:smoke/py_probe" in argv else SH_OK), ""
        fires = json.dumps({"result": {"fires": [{"status": "failed", "error": "launch_preparation_failed"}]}})
        run = Fake([(("execute",), execute), (("scheduler", "show", "fires"), (0, fires, ""))])
        clock = iter(range(0, 10000, 1))
        s = smoke.Smoke(args(), run=run, now=lambda: next(clock), sleep=lambda _: None)
        import tempfile
        with tempfile.TemporaryDirectory() as scratch:
            project = s.runtime_checks(Path(scratch))
            self.assertTrue((project / "smoke-data.txt").exists())
            s.check("fire", lambda: s.live_fire_refusal(project))
        got = {r.name: r.status for r in s.results}
        self.assertTrue(all(v == "PASS" for v in got.values()), got)
        self.assertIn(["scheduler", "deregister"], [c[:2] for c in run.calls])

    def test_deregisters_even_when_register_fails(self):
        run = Fake([(("scheduler", "register"), (1, "", "refused"))])
        s = smoke.Smoke(args(), run=run, now=lambda: 0, sleep=lambda _: None)
        import tempfile
        with tempfile.TemporaryDirectory() as scratch:
            outcome = s.live_fire_refusal(Path(scratch))
        self.assertEqual(outcome[0], "FAIL")
        self.assertIn(["scheduler", "deregister"], [c[:2] for c in run.calls])

    def test_restart_always_starts_again(self):
        run = Fake([(("bundle", "list"), (1, "", "controller process retained supplementary groups"))])
        s = smoke.Smoke(args(), run=run)
        outcome = s.restart_check()
        self.assertEqual(outcome[0], "FAIL")
        self.assertEqual(run.calls[-1][0], "start")

    def test_default_is_read_only_and_exit_code(self):
        run = Fake([])
        clock = iter(range(0, 100000, 5))
        s = smoke.Smoke(args(manual_directives=None), run=run,
                        now=lambda: next(clock), sleep=lambda _: None)
        results = s.run_all()
        names = {r.name: r.status for r in results}
        self.assertEqual(names["directive force_continue (paid)"], "SKIP")
        self.assertEqual(names["stopped-node standalone op"], "SKIP")
        self.assertNotIn(["stop"], [c[:1] for c in run.calls])


if __name__ == "__main__":
    unittest.main()
