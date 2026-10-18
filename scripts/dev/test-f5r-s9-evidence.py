#!/usr/bin/env python3
"""Unit tests for f5r-s9-evidence.py (python3 -B test-f5r-s9-evidence.py)."""
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("ev", Path(__file__).with_name("f5r-s9-evidence.py"))
ev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ev)

RENDER_KEY = "rnd_SECRETKEY123"
TOKEN = "tok_RUNSTREAMSECRET"
SBX = "sbx-abc"
URI = f"https://{SBX}.oregon.sandbox.onrender.com/runs/stream"
CONCURRENCY = ("lease refused: the node already holds 2 of its 2 concurrent leases; a lease is "
               "admitted only after one is released, and none is queued")


def out(rc, stdout="", stderr=""):
    return {"rc": rc, "stdout": stdout, "stderr": stderr, "timed_out": False}


def admitted(tid):
    return out(0, json.dumps({"thread_id": tid}))


REFUSED = out(1, "", "error: " + CONCURRENCY)


class Fake:
    def __init__(self):
        self.calls = []
        self.session_results = [admitted("t1"), admitted("t2"), REFUSED]
        self.script = {}          # tid -> list of (status, extra) before restart; last repeats
        self.restarted = False
        self.cancelled = set()
        self.after_restart = lambda tid: ("failed", "controller restarted")
        self.ready_after_restart = [503, 200]
        self.ready_seq = None
        self.old_still_ready = False
        self.http_override = {}
        self.probe_result = (410, json.dumps({"error": ev.OCCURRENCE_REFUSED}).encode())
        self.mint = (200, json.dumps({"token": TOKEN, "uri": URI, "method": "POST"}).encode())
        self.run_result = (500, b"curl: (6) Could not resolve host")

    def run(self, argv, env, timeout):
        words = argv[1:]
        self.calls.append(words)
        assert env["RYEOSD_URL"] and "RENDER_API_KEY" not in env
        if words[:3] == ["codex", "session", "start"]:
            return self.session_results.pop(0)
        if words[:2] == ["thread", "cancel"]:
            self.cancelled.add(words[2])
            return out(0, "{}")
        if words[:2] == ["thread", "get"]:
            tid = words[2]
            if tid in self.cancelled:
                body = {"thread_id": tid, "status": "cancelled"}
            elif self.restarted:
                status, note = self.after_restart(tid)
                body = {"thread_id": tid, "status": status, "error": note}
            else:
                seq = self.script.get(tid)
                if seq:
                    status = seq.pop(0) if len(seq) > 1 else seq[0]
                    body = {"thread_id": tid, "status": status}
                    if status == "failed":
                        body["error"] = CONCURRENCY
                else:
                    body = {"thread_id": tid, "status": "running"}
            return out(0, json.dumps(body))
        if words[:2] == ["thread", "list"]:
            return out(0, "[]")
        raise AssertionError(words)

    def http(self, method, url, headers=None, body=None, timeout=60):
        self.calls.append((method, url))
        path = url.split("?")[0]
        if path.endswith("/_ryeos/ready"):
            if self.restarted:
                if self.old_still_ready:
                    return 200, b""
                if self.ready_after_restart:
                    return self.ready_after_restart.pop(0), b""
            return 200, b""
        if path.endswith("/public-key"):
            return 200, json.dumps({"principal_id": "fp:abc"}).encode()
        if path.endswith("/restart"):
            self.restarted = True
            return self.http_override.get("restart", (202, b""))
        if path.endswith("/runs/stream/token"):
            return self.mint
        if path == URI or path.endswith("/runs/stream"):
            return self.run_result
        raise AssertionError(url)

    def probe(self, base, path):
        self.calls.append(("probe", path))
        return self.probe_result


def make(fake, tmp, **over):
    args = SimpleNamespace(controller_url="https://c.example", credential_profile="p",
                           render_service_id="srv-abc", render_service_name="ryeos-f5r-s9-x",
                           out=str(Path(tmp) / "out"), project=".", ryeos_bin="ryeos",
                           render_owner_id="own-1", sandbox_region="oregon", e7_body_json="",
                           placement=[f"pt1:{SBX}"], poll_attempts=3, poll_interval=0,
                           window_polls=2, restart_timeout=5)
    for k, v in over.items():
        setattr(args, k, v)
    logs = []
    runner = ev.Runner(args, RENDER_KEY, tmp, http=fake.http, run=fake.run, probe=fake.probe,
                       sleep=lambda s: None, log=logs.append)
    runner.logs = logs
    return runner


class Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.out = Path(self.tmp) / "out"

    def rec(self, name):
        return json.loads((self.out / name).read_text())

    def go(self, fake=None, **over):
        fake = fake or Fake()
        runner = make(fake, self.tmp, **over)
        return fake, runner, runner.execute()

    def test_full_run_passes_and_manifest_hashes(self):
        _, runner, code = self.go()
        self.assertEqual(code, 0, runner.logs)
        manifest = json.loads((self.out / "manifest.json").read_text())
        self.assertEqual({e["file"] for e in manifest["files"]},
                         {"preflight.json", "e1-sessions.json", "e2-refusal.json", "e3-running.json",
                          "e6-restart.json", "e7-egress.json", "teardown.json"})
        for entry in manifest["files"]:
            self.assertEqual(entry["sha256"], hashlib.sha256((self.out / entry["file"]).read_bytes()).hexdigest())
        self.assertIn("none is queued", self.rec("e2-refusal.json")["refusalText"])
        self.assertEqual(self.rec("e6-restart.json")["streamVerdict"], "probed")
        self.assertEqual(self.rec("teardown.json")["sandboxOccurrenceIds"], [SBX])

    def test_classification(self):
        self.assertEqual(ev.classify_session_start(REFUSED), "concurrency-refused")
        other = out(1, "", "lease refused for workload `codex`: lifetime")
        self.assertEqual(ev.classify_session_start(other), "lease-refused")
        self.assertEqual(ev.classify_session_start(out(1, "", "lease refused: no budget")), "lease-refused")
        self.assertEqual(ev.classify_session_start(admitted("t")), "admitted")
        self.assertEqual(ev.classify_session_start(out(0, "not json")), "failed")
        self.assertEqual(ev.classify_session_start(out(1, "", "boom")), "failed")

    def test_stream_classification(self):
        occ = json.dumps({"error": ev.OCCURRENCE_REFUSED})
        nolive = json.dumps({"error": ev.NO_LIVE_CHANNEL})
        self.assertEqual(ev.classify_stream_probe(410, occ), "lease-refused")
        self.assertEqual(ev.classify_stream_probe(410, ""), "unknown")
        self.assertEqual(ev.classify_stream_probe(404, nolive), "no-live-channel")
        self.assertEqual(ev.classify_stream_probe(400, ""), "probe-bug")
        self.assertEqual(ev.classify_stream_probe(101, ""), "channel-live")

    def test_status_sets(self):
        self.assertEqual(ev.LIVE_STATUSES | ev.TERMINAL_STATUSES,
                         {"created", "running", "completed", "failed", "cancelled", "killed",
                          "timed_out", "continued"})
        self.assertFalse(ev.thread_is_live(json.dumps({"status": "failed"})))
        self.assertTrue(ev.thread_is_live(json.dumps({"status": "running"})))
        self.assertTrue(ev.thread_is_live(json.dumps({"thread_id": "x"})))
        self.assertTrue(ev.thread_is_live("not json"))

    def test_third_session_admitted_fails_e2(self):
        fake = Fake()
        fake.session_results = [admitted("t1"), admitted("t2"), admitted("t3")]
        _, runner, code = self.go(fake)
        self.assertEqual(code, 1)
        self.assertIn("e2-third-refused", runner.failed)
        self.assertIn("t3", fake.cancelled)

    def test_non_concurrency_refusal_fails_e2(self):
        fake = Fake()
        fake.session_results = [admitted("t1"), admitted("t2"),
                                out(1, "", "lease refused for workload `codex`: lifetime")]
        _, runner, code = self.go(fake)
        self.assertIn("e2-third-refused", runner.failed)

    def test_refusal_arrives_on_polled_thread(self):
        fake = Fake()
        fake.session_results = [admitted("t1"), admitted("t2"), admitted("t3")]
        fake.script["t3"] = ["running", "failed"]
        _, runner, code = self.go(fake)
        self.assertEqual(code, 0, runner.logs)
        self.assertEqual(self.rec("e2-refusal.json")["polledStatuses"], ["running", "failed"])

    def test_thread_never_running_fails_e3_and_skips_restart(self):
        fake = Fake()
        fake.script["t1"] = ["created"]
        _, runner, code = self.go(fake)
        self.assertIn("e3-running", runner.failed)
        self.assertIn("e4-e6-restart-refusal", runner.failed)
        self.assertFalse(fake.restarted)
        self.assertTrue(self.rec("e6-restart.json")["skipped"])

    def test_missing_status_fails_e6_and_teardown(self):
        fake = Fake()
        fake.after_restart = lambda tid: (None, "controller restarted")
        _, runner, code = self.go(fake)
        self.assertIn("e4-e6-restart-refusal", runner.failed)

    def test_teardown_missing_status_counts_live(self):
        fake = Fake()
        real = fake.run

        def run(argv, env, timeout):
            if argv[1:3] == ["thread", "get"] and argv[3] in fake.cancelled:
                return out(0, json.dumps({"thread_id": argv[3]}))
            return real(argv, env, timeout)
        fake.run = run
        _, runner, code = self.go(fake)
        self.assertIn("teardown", runner.failed)

    def test_restart_text_required(self):
        fake = Fake()
        fake.after_restart = lambda tid: ("failed", "something else")
        _, runner, _ = self.go(fake)
        self.assertIn("e4-e6-restart-refusal", runner.failed)

    def test_probe_101_and_400_fail(self):
        for result in ((101, b""), (400, b"")):
            fake = Fake()
            fake.probe_result = result
            _, runner, _ = self.go(fake)
            self.assertIn("e4-e6-restart-refusal", runner.failed, result)

    def test_no_placement_is_unprobed_and_e7_fails(self):
        _, runner, _ = self.go(placement=[])
        self.assertEqual(self.rec("e6-restart.json")["streamVerdict"], "unprobed")
        self.assertIn("e7-unlisted-egress", runner.failed)
        self.assertNotIn("e4-e6-restart-refusal", runner.failed)

    def test_ready_never_recovers(self):
        fake = Fake()
        fake.ready_after_restart = [503] * 50
        fake.clock_ticks = 0
        runner = make(fake, self.tmp, restart_timeout=3)
        ticks = iter(range(0, 1000))
        runner.clock = lambda: next(ticks)
        self.assertEqual(runner.execute(), 1)
        self.assertIn("e4-e6-restart-refusal", runner.failed)

    def test_old_instance_still_ready_fails(self):
        fake = Fake()
        fake.old_still_ready = True
        runner = make(fake, self.tmp, restart_timeout=3)
        ticks = iter(range(0, 1000))
        runner.clock = lambda: next(ticks)
        self.assertEqual(runner.execute(), 1)
        self.assertIn("never stopped answering", self.rec("e6-restart.json")["error"])

    def test_e7_mint_failure_and_foreign_uri(self):
        fake = Fake()
        fake.mint = (403, b"")
        _, runner, _ = self.go(fake)
        self.assertEqual(runner.failed, ["e7-unlisted-egress"])
        fake = Fake()
        fake.mint = (200, json.dumps({"token": TOKEN, "uri": "https://evil.example/runs/stream"}).encode())
        _, runner, _ = self.go(fake)
        self.assertEqual(runner.failed, ["e7-unlisted-egress"])
        self.assertNotIn(TOKEN, (self.out / "e7-egress.json").read_text())

    def test_e7_inconclusive_and_reachable(self):
        fake = Fake()
        fake.run_result = (500, b"internal error")
        _, runner, _ = self.go(fake)
        rec = self.rec("e7-egress.json")
        self.assertFalse(rec["ok"])
        self.assertIsNone(rec["egressBlocked"])
        self.assertIn("inconclusive", rec["error"])
        fake = Fake()
        fake.run_result = (200, b"<h1>Example Domain</h1>")
        _, runner, _ = self.go(fake)
        self.assertIn("reachable", self.rec("e7-egress.json")["error"])

    def test_e7_exit_code_is_proof(self):
        fake = Fake()
        fake.run_result = (200, b'{"exit_code": 6}')
        _, runner, code = self.go(fake)
        self.assertEqual(code, 0, runner.logs)
        self.assertTrue(self.rec("e7-egress.json")["egressBlocked"])

    def test_owner_id_restricted(self):
        _, runner, _ = self.go(render_owner_id="own&x=1")
        self.assertIn("e7-unlisted-egress", runner.failed)

    def test_teardown_on_failure_and_step_isolation(self):
        fake = Fake()
        fake.session_results = [admitted("t1"), out(1, "", "boom")]
        _, runner, code = self.go(fake)
        self.assertEqual(code, 1)
        self.assertIn("e2-third-refused", runner.failed)
        self.assertTrue(self.rec("e2-refusal.json")["skipped"])
        self.assertIn("t1", fake.cancelled)
        self.assertTrue((self.out / "manifest.json").exists())

    def test_teardown_runs_when_step_raises(self):
        fake = Fake()
        fake.http_override["restart"] = None
        _, runner, code = self.go(fake)
        self.assertEqual(code, 1)
        self.assertEqual(fake.cancelled, {"t1", "t2"})

    def test_no_secret_leak(self):
        fake = Fake()
        fake.run_result = (500, f"curl: (6) {TOKEN} {RENDER_KEY}".encode())
        fake.session_results = [admitted("t1"), admitted("t2"),
                                out(1, "", f"{CONCURRENCY} Bearer abc.def {RENDER_KEY}")]
        _, runner, _ = self.go(fake)
        blob = "".join(p.read_text() for p in self.out.iterdir()) + "".join(runner.logs)
        for secret in (RENDER_KEY, TOKEN, "abc.def"):
            self.assertNotIn(secret, blob)

    def test_env_file_permission_refusal(self):
        path = Path(self.tmp) / "env"
        path.write_text(f"RENDER_API_KEY={RENDER_KEY}\n")
        path.chmod(0o644)
        with self.assertRaises(ev.Refusal):
            ev.read_render_key(path)
        path.chmod(0o600)
        self.assertEqual(ev.read_render_key(path), RENDER_KEY)

    def test_main_refuses_loose_env_file_and_bad_names(self):
        env = Path(self.tmp) / "env"
        env.write_text(f"RENDER_API_KEY={RENDER_KEY}\n")
        env.chmod(0o644)

        def argv(name):
            return ["--controller-url", "https://c", "--app-root-or-key", self.tmp,
                    "--credential-profile", "p", "--render-env-file", str(env),
                    "--render-service-id", "srv-abc", "--render-service-name", name,
                    "--out", str(Path(self.tmp) / "o")]
        f = Fake()
        self.assertEqual(ev.main(argv("ryeos-f5r-s9-ok"), http=f.http, run=f.run), 1)
        self.assertEqual(f.calls, [])
        # A bad name is refused before the env file is even read: a missing file would be a Failure.
        env.unlink()
        for name in ("other-service", "ryeos-f5r-s9-Farm", "ryeos-f5r-s9-SNAP-TRACK"):
            self.assertEqual(ev.main(argv(name), http=f.http, run=f.run), 1)

    def test_name_guard(self):
        ev.guard_service_name("ryeos-f5r-s9-a")
        for bad in ("x", "ryeos-f5r-s9-farm", "ryeos-f5r-s9-Snap-Track", ""):
            with self.assertRaises(ev.Refusal):
                ev.guard_service_name(bad)
        runner = make(Fake(), self.tmp, render_service_name="evil")
        with self.assertRaises(ev.Refusal):
            runner.e7()
        with self.assertRaises(ev.Refusal):
            runner.e6()

    def test_farm_service_refused(self):
        with self.assertRaises(ev.Refusal):
            ev.guard_service_id(ev.FARM_SERVICE_ID)

    def test_key_file_staged_into_app_root(self):
        key = Path(self.tmp) / "k.pem"
        key.write_text("pem")
        key.chmod(0o600)
        root = ev.prepare_app_root(key, tempfile.mkdtemp())
        self.assertTrue((root / ".ai/config/keys/signing/private_key.pem").is_file())
        key.chmod(0o644)
        with self.assertRaises(ev.Refusal):
            ev.prepare_app_root(key, tempfile.mkdtemp())


if __name__ == "__main__":
    unittest.main()
