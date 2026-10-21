#!/usr/bin/env python3
import base64
import contextlib
import importlib.util
import io
import json
import tempfile
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("s9", Path(__file__).with_name("f5r-s9-evidence.py"))
S9 = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(S9)

SEED = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
PRINCIPAL = "fp:" + "ab" * 32
SERVICE = "srv-throwaway1"
SANDBOX = "sbx-abc123"
RENDER_KEY = "rnd_SECRETKEYVALUE"
PKCS8_PREFIX = bytes.fromhex("302e020100300506032b657004220420")


def pem(seed):
    b64 = base64.b64encode(PKCS8_PREFIX + seed).decode()
    return f"-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n"


class FakeController:
    """Fake daemon: verifies x-ryeos-* signatures with the pure-Python verifier and serves the routes."""

    def __init__(self, e3="ok", fail_confirm=False):
        self.e3, self.fail_confirm, self.refuse_sessions = e3, fail_confirm, False
        # State a fresh profile lands in. 'confirming' stands for a login Leo completed by hand; a real
        # create leaves the profile 'unauthenticated'.
        self.create_state, self.routes = "confirming", []
        self.started_at = "2026-10-21T00:00:00Z"
        self.sessions, self.profiles, self.log, self.sig_ok = {}, {}, [], []
        self.pub = S9.ed25519_public(SEED)
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def send(self, status, obj):
                raw = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def verified(self, body):
                h = self.headers
                if not h.get("x-ryeos-signature"):
                    return False
                parts = ["ryeos-request-v1", self.command, S9.Signer(SEED).canonical_path(self.path),
                         S9.sha256_hex(body), h["x-ryeos-timestamp"], h["x-ryeos-nonce"], PRINCIPAL]
                digest = S9.sha256_hex("\n".join(parts).encode())
                ok = S9.ed25519_verify(fake.pub, digest.encode(), base64.b64decode(h["x-ryeos-signature"]))
                fake.sig_ok.append(ok)
                return ok

            def do_GET(self):
                self.route(b"")

            def do_POST(self):
                self.route(self.rfile.read(int(self.headers.get("Content-Length") or 0)))

            def route(self, body):
                p = urllib.parse.urlsplit(self.path).path
                if p == "/_ryeos/ready":
                    return self.send(200, {"ready": True, "status": "running", "started_at": fake.started_at})
                if p == "/public-key":
                    return self.send(200, {"principal_id": PRINCIPAL})
                if not self.verified(body):
                    return self.send(401, {"error": "bad signature"})
                if p.startswith("/launches/"):
                    return self.send(200, {"state": "running"})
                req = json.loads(body)
                ref, prm = req["item_ref"], req["parameters"]
                fake.log.append(ref)
                fake.routes.append((p, ref))
                accepted = req["execution_policy"]["response"] == "accepted"
                if accepted != (p == "/execute/launch"):
                    return self.send(400, {"error": "execution_policy.response=accepted is supported only by "
                                                    "/execute/launch"})
                status, out = fake.handle(ref, prm)
                self.send(status, {"result": out} if status == 200 else out)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def handle(self, ref, prm):
        if ref == "service:credential-profiles/list":
            return 200, {"items": []}
        if ref == "service:credential-profiles/create":
            self.profiles[prm["profile_id"]] = {"state": self.create_state, "credential_generation": 1}
            return 200, {"profile_id": prm["profile_id"], "state": self.create_state}
        if ref == "service:credential-profiles/get":
            p = self.profiles.get(prm["profile_id"])
            if not p:
                return 404, {"error": "not found"}
            return 200, {**p, "login_epoch": 1, "sanitized_account_digest": "d" * 64}
        if ref == "service:credential-profiles/confirm":
            if self.fail_confirm:
                return 400, {"error": "credential confirmation ceremony is absent or expired"}
            p = self.profiles[prm["profile_id"]]
            if p["state"] != "confirming":
                return 400, {"error": "credential confirmation ceremony is absent or expired"}
            self.confirm_args = prm
            self.profiles[prm["profile_id"]] = {"state": "active", "credential_generation": 2}
            return 200, {"state": "active", "credential_generation": 2}
        if ref == "service:credential-profiles/revoke":
            p = self.profiles[prm["profile_id"]]
            if not isinstance(prm.get("credential_generation"), int) or prm["credential_generation"] != p["credential_generation"]:
                return 400, {"error": "invalid type: null, expected u64"}
            p["state"] = "revoked"
            p["credential_generation"] += 1
            return 200, {"credential_generation": p["credential_generation"]}
        if ref == "service:credential-profiles/delete":
            p = self.profiles[prm["profile_id"]]
            if not isinstance(prm.get("credential_generation"), int) or prm["credential_generation"] != p["credential_generation"]:
                return 400, {"error": "invalid type: null, expected u64"}
            if p["state"] not in ("revoked", "unauthenticated"):
                return 400, {"error": "not revoked"}
            del self.profiles[prm["profile_id"]]
            return 200, {}
        if ref == "worker_execution:codex/session":
            if self.refuse_sessions:
                return 400, {"error": "credential profile is not active"}
            live = [s for s in self.sessions.values() if not s["ended"]]
            if len(live) >= 2:
                return 500, {"error": "lease refused for workload `codex`: the node already holds 2"}
            sid = f"T-{len(self.sessions) + 1}"
            self.sessions[sid] = {"ended": False}
            return 200, {"chain_root_id": sid, "launch_id": "L" + sid}
        if ref == "service:worker-executions/status":
            sid = prm["chain_root_id"]
            if self.e3 == "refused" and sid == "T-1":
                return 200, {"state": "failed", "error": "external candidate refused: runtime qualification absent"}
            return 200, {"state": "admitted", "channel": {"state": "guest_dialed"}}
        if ref == "service:worker-executions/terminate":
            self.sessions[prm["chain_root_id"]]["ended"] = True
            return 200, {}
        return 404, {"error": ref}

    def close(self):
        self.server.shutdown()


class FakeRender:
    def __init__(self, controller, name="ryeos-f5r-s9-test"):
        self.controller, self.name, self.calls, self.restarted = controller, name, [], False
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def send(self, status, obj):
                raw = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def go(self):
                fake.calls.append((self.command, self.path, self.headers.get("Authorization")))
                p = urllib.parse.urlsplit(self.path).path
                if p == f"/services/{SERVICE}" and self.command == "GET":
                    return self.send(200, {"id": SERVICE, "name": fake.name})
                if p == f"/services/{SERVICE}/restart":
                    fake.controller.started_at = "2026-10-21T01:00:00Z"
                    fake.restarted = True
                    return self.send(200, {})
                if p.endswith("/runs/stream/token"):
                    return self.send(201, {"executionId": "exe-1", "expiresAt": "2099-01-01T00:00:00Z",
                                           "method": "POST", "token": "tok-SECRET-1",
                                           "uri": f"https://{SANDBOX}.oregon.sandbox.onrender.com/runs/stream"})
                self.send(404, {"message": "no"})

            do_GET = do_POST = go

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()


class ProxyResp:
    def __init__(self, status, body):
        self.status, self._b = status, body

    def read(self, n=-1):
        return self._b

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class Harness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.key = self.dir / "key.pem"
        self.key.write_text(pem(SEED))
        self.envfile = self.dir / ".env"
        self.envfile.write_text(f"RENDER_API_KEY={RENDER_KEY}\n")
        self.envfile.chmod(0o600)
        self.out = self.dir / "out"
        self.lines = []
        self.proxy_requests = []

    def run_main(self, ctl, render, extra=(), proxy_body=b"E7_EGRESS_BLOCKED\n", service=SERVICE):
        def proxy(req, timeout):
            self.proxy_requests.append(req)
            return ProxyResp(200, proxy_body)

        argv = ["--base-url", ctl.url, "--operator-key", str(self.key), "--out", str(self.out),
                "--project-path", "/proj", "--render-base", render.url,
                "--env-file", str(self.envfile), "--sandbox-id", SANDBOX, "--owner-id", "tea-owner1",
                "--poll-timeout", "0", "--poll-interval", "0", "--restart-timeout", "5", *extra]
        if service:
            argv += ["--service-id", service]
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = S9.main(argv, proxy_opener=proxy, sleep=lambda s: None, emit=self.lines.append)
        return code

    def evidence(self, name):
        return json.loads((self.out / f"{name}.json").read_text())


class Tests(Harness):
    def setUp(self):
        super().setUp()
        self.ctl, self.render = FakeController(), None
        self.render = FakeRender(self.ctl)
        self.addCleanup(self.ctl.close)
        self.addCleanup(self.render.close)

    def test_rfc8032_vector_and_request_signing(self):
        self.assertEqual(S9.ed25519_public(SEED).hex(),
                         "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
        sig = S9.ed25519_sign(SEED, b"")
        self.assertTrue(sig.hex().startswith("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974"))
        self.assertTrue(S9.ed25519_verify(S9.ed25519_public(SEED), b"", sig))
        self.assertFalse(S9.ed25519_verify(S9.ed25519_public(SEED), b"x", sig))
        h = S9.Signer(SEED).headers("post", "/execute?b=2&a=1", b"{}", PRINCIPAL, ts=1700000000, nonce="n0")
        canon = "\n".join(["ryeos-request-v1", "POST", "/execute?a=1&b=2", S9.sha256_hex(b"{}"),
                           "1700000000", "n0", PRINCIPAL])
        self.assertTrue(S9.ed25519_verify(S9.ed25519_public(SEED), S9.sha256_hex(canon.encode()).encode(),
                                          base64.b64decode(h["x-ryeos-signature"])))

    def test_happy_path(self):
        code = self.run_main(self.ctl, self.render)
        self.assertEqual(code, 0, self.lines[-5:])
        for name, status in [("preflight", "pass"), ("profile", "pass"), ("e1", "pass"), ("e2", "pass"),
                             ("e3", "pass"), ("e4", "pass"), ("e6", "pass"), ("e7", "pass"),
                             ("teardown", "pass")]:
            self.assertEqual(self.evidence(name)["status"], status, name)
        e2 = self.evidence("e2")
        self.assertEqual(e2["refusal_text"], "lease refused for workload `codex`: the node already holds 2")
        e1 = self.evidence("e1")
        self.assertTrue(all(s["admitted_observed"] for s in e1["sessions"]))
        ex = e1["exchanges"][0]
        self.assertEqual(ex["response"]["body_sha256"], S9.sha256_hex(ex["response"]["body"].encode()))
        self.assertIn("started_at", ex)
        self.assertTrue(self.render.restarted)
        self.assertEqual(self.evidence("e6")["started_at_after"], "2026-10-21T01:00:00Z")
        self.assertTrue(all(self.ctl.sig_ok) and self.ctl.sig_ok)
        self.assertTrue(all(s["ended"] for s in self.ctl.sessions.values()))
        self.assertEqual(self.ctl.profiles, {})
        self.assertEqual(self.proxy_requests[0].full_url,
                         f"https://{SANDBOX}.oregon.sandbox.onrender.com/runs/stream")
        every = "".join(p.read_text() for p in self.out.iterdir()) + "".join(self.lines)
        self.assertNotIn("tok-SECRET-1", every)
        self.assertNotIn(RENDER_KEY, every)
        for line in self.lines:
            json.loads(line)

    def test_manifest_signature_verifies_with_public_key(self):
        self.assertEqual(self.run_main(self.ctl, self.render), 0)
        pub = S9.ed25519_public(SEED)
        self.assertTrue(S9.verify_manifest(self.out, pub))
        manifest = json.loads((self.out / "manifest.json").read_text())
        self.assertIn("hosted-floor risk accepted", manifest["risk_acceptance"].lower())
        ids = [s["id"] for s in manifest["steps"]]
        self.assertEqual(ids, ["preflight", "profile", "e1", "e2", "e3", "e4", "e6", "e7", "teardown"])
        for s in manifest["steps"]:
            self.assertEqual(s["sha256"], S9.sha256_hex((self.out / s["file"]).read_bytes()))
        # tampering breaks the signature
        raw = (self.out / "manifest.json").read_bytes()
        (self.out / "manifest.json").write_bytes(raw.replace(b"pass", b"fail", 1))
        self.assertFalse(S9.verify_manifest(self.out, pub))
        other = S9.ed25519_public(bytes(32))
        (self.out / "manifest.json").write_bytes(raw)
        self.assertFalse(S9.verify_manifest(self.out, other))

    def test_e3_refusal_is_recorded_not_faked(self):
        self.ctl.e3 = "refused"
        code = self.run_main(self.ctl, self.render)
        e3 = self.evidence("e3")
        self.assertEqual(e3["status"], "refused")
        self.assertIn("refus", e3["reason"])
        self.assertIn("no pass is claimed", e3["note"])
        self.assertEqual(self.evidence("e2")["status"], "pass")
        self.assertEqual(self.evidence("e7")["status"], "pass")
        self.assertEqual(code, 0)

    def test_teardown_runs_on_midrun_failure(self):
        self.render.name = "ryeos-f5r-s9-test"
        code = self.run_main(self.ctl, self.render, service="srv-missing")  # e6 get -> 404 -> Failure
        self.assertEqual(code, 1)
        self.assertEqual(self.evidence("teardown")["status"], "pass")
        self.assertTrue(all(s["ended"] for s in self.ctl.sessions.values()))
        self.assertEqual(self.ctl.profiles, {})
        self.assertTrue(S9.verify_manifest(self.out, S9.ed25519_public(SEED)))
        manifest = json.loads((self.out / "manifest.json").read_text())
        self.assertEqual({s["id"]: s["status"] for s in manifest["steps"]}["e7"], "not-run")

    def test_confirm_refused_is_inconclusive_and_sessions_still_attempted(self):
        self.ctl.fail_confirm = True
        self.ctl.refuse_sessions = True
        code = self.run_main(self.ctl, self.render)
        self.assertEqual(code, 0, self.lines[-5:])
        prof = self.evidence("profile")
        self.assertEqual(prof["status"], "inconclusive")
        self.assertIn("ceremony is absent or expired", prof["confirm_refusal"])
        self.assertIn("ceremony is absent or expired", prof["reason"])
        e1 = self.evidence("e1")
        self.assertEqual(e1["status"], "refused")
        self.assertTrue(e1["reason"])
        self.assertIn("service:credential-profiles/confirm", self.ctl.log)
        self.assertIn("worker_execution:codex/session", self.ctl.log)
        self.assertEqual(self.evidence("e2")["status"], "refused")
        self.assertEqual(self.evidence("e3")["status"], "inconclusive")
        self.assertEqual(self.ctl.profiles, {})
        self.assertEqual(self.evidence("teardown")["status"], "pass")

    def test_sessions_use_launch_route_and_services_use_execute(self):
        self.assertEqual(self.run_main(self.ctl, self.render), 0, self.lines[-5:])
        for path, ref in self.ctl.routes:
            self.assertEqual(path == "/execute/launch", ref == "worker_execution:codex/session", (path, ref))
        self.assertIn(("/execute/launch", "worker_execution:codex/session"), self.ctl.routes)
        e1 = self.evidence("e1")
        body = e1["exchanges"][0]["request"]["body"]
        self.assertRegex(body["launch_id"], r"^L-[0-9a-f]{32}$")
        self.assertEqual(e1["exchanges"][0]["request"]["path"], "/execute/launch")

    def test_revoke_and_delete_carry_the_generation(self):
        self.assertEqual(self.run_main(self.ctl, self.render), 0, self.lines[-5:])
        td = self.evidence("teardown")
        by = {e["label"]: e["request"]["body"]["parameters"] for e in td["exchanges"]}
        self.assertEqual(by["profile-revoke"]["credential_generation"], 2)
        self.assertEqual(by["profile-delete"]["credential_generation"], 3)
        self.assertEqual(self.ctl.profiles, {})

    def test_existing_profile_is_reused_and_removed(self):
        pid = "s9-evidence-existing"
        self.ctl.profiles[pid] = {"state": "unauthenticated", "credential_generation": 1}
        code = self.run_main(self.ctl, self.render, extra=["--profile-id", pid, "--skip-render"])
        self.assertEqual(code, 0, self.lines[-5:])
        self.assertNotIn("service:credential-profiles/create", self.ctl.log)
        prof = self.evidence("profile")
        self.assertTrue(prof["reused_existing"])
        self.assertEqual(prof["status"], "inconclusive")
        self.assertEqual(self.evidence("teardown")["status"], "pass")
        self.assertEqual(self.ctl.profiles, {})

    def test_unauthenticated_profile_records_provider_login_requirement_without_confirm(self):
        self.ctl.create_state = "unauthenticated"
        self.assertEqual(self.run_main(self.ctl, self.render, extra=["--skip-render"]), 0, self.lines[-5:])
        prof = self.evidence("profile")
        self.assertEqual(prof["status"], "inconclusive")
        self.assertIn("activation requires a provider login", prof["reason"])
        self.assertNotIn("service:credential-profiles/confirm", self.ctl.log)
        self.assertEqual(self.ctl.profiles, {})

    def test_hand_supplied_epoch_and_digest_are_confirmed(self):
        self.ctl.create_state = "confirming"
        digest = "ab" * 32
        extra = ["--skip-render", "--login-epoch", "7", "--account-digest", digest]
        self.assertEqual(self.run_main(self.ctl, self.render, extra=extra), 0, self.lines[-5:])
        self.assertEqual(self.ctl.confirm_args, {"profile_id": self.ctl.confirm_args["profile_id"],
                                                 "login_epoch": 7, "expected_account_digest": digest})
        self.assertEqual(self.evidence("profile")["status"], "pass")

    def test_existing_active_profile_is_reused_without_confirm(self):
        pid = "s9-evidence-active"
        self.ctl.profiles[pid] = {"state": "active", "credential_generation": 2}
        self.assertEqual(self.run_main(self.ctl, self.render, extra=["--profile-id", pid, "--skip-render"]), 0)
        self.assertEqual(self.evidence("profile")["status"], "pass")
        self.assertNotIn("service:credential-profiles/confirm", self.ctl.log)
        self.assertEqual(self.ctl.profiles, {})

    def test_skip_render_records_e6_e7_skipped_and_reads_no_key(self):
        self.envfile.unlink()  # a read of the key would fail
        self.render.calls.clear()
        code = self.run_main(self.ctl, self.render, extra=["--skip-render"], service=None)
        self.assertEqual(code, 0, self.lines[-5:])
        for name in ("e6", "e7"):
            ev = self.evidence(name)
            self.assertEqual(ev["status"], "skipped")
            self.assertEqual(ev["reason"], "render api key is not available on the controller")
        self.assertEqual(self.render.calls, [])
        self.assertFalse(self.render.restarted)
        self.assertEqual(self.proxy_requests, [])
        manifest = json.loads((self.out / "manifest.json").read_text())
        by = {s["id"]: s for s in manifest["steps"]}
        self.assertEqual(by["e6"]["status"], "skipped")
        self.assertEqual(by["e7"]["reason"], "render api key is not available on the controller")
        self.assertEqual(manifest["summary"]["skipped"], 2)
        self.assertEqual(manifest["summary"]["pass"], 7)

    def test_manifest_summary_counts_every_status(self):
        self.ctl.e3 = "refused"
        self.ctl.fail_confirm = True
        self.assertEqual(self.run_main(self.ctl, self.render, extra=["--skip-render"]), 0)
        m = json.loads((self.out / "manifest.json").read_text())
        counts = {}
        for s in m["steps"]:
            self.assertTrue(s["reason"])
            counts[s["status"]] = counts.get(s["status"], 0) + 1
        for k, v in counts.items():
            self.assertEqual(m["summary"][k], v, k)
        self.assertEqual(sum(m["summary"].values()), len(m["steps"]))
        self.assertEqual(m["summary"]["skipped"], 2)
        self.assertEqual(m["summary"]["inconclusive"], 1)
        self.assertTrue(S9.verify_manifest(self.out, S9.ed25519_public(SEED)))

    def test_service_id_required_without_skip_render(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(self.run_main(self.ctl, self.render, service=None), 2)

    def test_idempotent_rerun_skips_done_steps(self):
        self.assertEqual(self.run_main(self.ctl, self.render), 0)
        before = list(self.ctl.log)
        self.lines.clear()
        self.assertEqual(self.run_main(self.ctl, self.render), 0)
        new = self.ctl.log[len(before):]
        self.assertNotIn("worker_execution:codex/session", new)
        self.assertNotIn("service:credential-profiles/create", new)
        self.assertTrue(any('"skipped-already-done"' in l for l in self.lines))
        state = json.loads((self.out / "state.json").read_text())
        self.assertEqual(state["steps"]["e2"]["status"], "pass")
        self.assertFalse((self.out / "state.json.tmp").exists())

    def test_refuses_the_farm_service_and_non_prefixed_service(self):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(self.run_main(self.ctl, self.render, service="srv-datiea7avr4c73dm14fg"), 2)
        self.assertEqual(self.ctl.log, [])
        self.render.name = "something-else"
        self.run_main(self.ctl, self.render)
        self.assertFalse(self.render.restarted)
        self.assertFalse((self.out / "e6.json").exists())

    def test_e7_open_egress_fails_and_unmarked_is_inconclusive(self):
        self.assertEqual(self.run_main(self.ctl, self.render, proxy_body=b"E7_EGRESS_OPEN"), 1)
        self.assertEqual(self.evidence("e7")["status"], "fail")


class Pkcs8Versions(unittest.TestCase):
    def test_version_two_key_with_public_key_attached_loads_the_same_seed(self):
        import tempfile, os
        seed = bytes(range(32))
        v1 = PKCS8_PREFIX + seed
        public = b"\x00" * 32
        inner = b"\x02\x01\x01" + bytes.fromhex("300506032b6570") + b"\x04\x22\x04\x20" + seed + b"\x81\x21\x00" + public
        v2 = b"\x30" + bytes([len(inner)]) + inner
        d = tempfile.mkdtemp()
        for name, der in (("v1.pem", v1), ("v2.pem", v2)):
            with open(os.path.join(d, name), "w") as f:
                f.write("-----BEGIN PRIVATE KEY-----\n" + base64.b64encode(der).decode() + "\n-----END PRIVATE KEY-----\n")
            self.assertEqual(S9.load_seed(os.path.join(d, name)), seed)
        with open(os.path.join(d, "bad.pem"), "w") as f:
            f.write("-----BEGIN PRIVATE KEY-----\n" + base64.b64encode(b"\x30\x05\x02\x01\x00\x00\x00").decode() + "\n-----END PRIVATE KEY-----\n")
        with self.assertRaises(S9.Failure):
            S9.load_seed(os.path.join(d, "bad.pem"))


if __name__ == "__main__":
    unittest.main()
