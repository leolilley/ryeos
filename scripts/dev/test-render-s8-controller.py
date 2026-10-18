#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import tempfile
import urllib.error
from pathlib import Path
from unittest import mock
import unittest

SPEC = importlib.util.spec_from_file_location("ctl", Path(__file__).with_name("render-s8-controller.py"))
CTL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CTL)

KEY = "rnd_SECRETKEYVALUE"
TOKEN = "ghp_SECRETTOKENVALUE"
DIGEST = "sha256:" + "a" * 64
IMAGE = "ghcr.io/leolilley/ryeos-f5r-controller@sha256:" + "b" * 64
NAME = "ryeos-f5r-s9-test"
READY = "https://x.onrender.com/_ryeos/ready"
SVC_URL = "https://x.onrender.com"


class Resp:
    def __init__(self, status, body):
        self.status, self._body = status, json.dumps(body).encode() if body is not None else b""

    def read(self):
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class Fake:
    """Routes (method, path) to canned responses; the last response repeats."""

    def __init__(self, routes):
        self.routes = {k: list(v) for k, v in routes.items()}
        self.requests = []

    def __call__(self, request, timeout):
        path = request.full_url.replace(CTL.API, "")
        method = request.get_method()
        self.requests.append((method, path, request.data, dict(request.header_items())))
        queue = self.routes[(method, path)]
        status, body = queue.pop(0) if len(queue) > 1 else queue[0]
        if status >= 400:
            raise urllib.error.HTTPError(request.full_url, status, "x", {},
                                         io.BytesIO(json.dumps(body).encode() if body is not None else b"{}"))
        return Resp(status, body)

    def methods(self, method):
        return [r for r in self.requests if r[0] == method]


def deploy_routes(statuses):
    svc = {"id": "srv-new", "name": NAME, "serviceDetails": {"url": SVC_URL}}
    return {
        ("GET", "/owners?limit=20"): [(200, [{"owner": {"id": "own-1", "email": "a@b.c"}, "cursor": "c"}])],
        ("GET", f"/services?name={NAME}&ownerId=own-1"): [(200, [])],
        ("GET", "/registrycredentials?limit=100"): [(200, [])],
        ("POST", "/registrycredentials"): [(201, {"id": "rcred-1"})],
        ("POST", "/services"): [(201, {"service": svc, "deployId": "dep-1"})],
        ("GET", "/services/srv-new"): [(200, svc)],
        ("GET", "/services/srv-new/deploys/dep-1"): [(200, {"id": "dep-1", "status": s}) for s in statuses],
        ("GET", READY): [(200, {})],
    }


class Case(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = self.tmp.name
        keyfile = Path(self.out) / "key"
        keyfile.write_text(KEY + "\n")
        self.keyfile = str(keyfile)

    def tearDown(self):
        self.tmp.cleanup()

    def run_main(self, argv, fake, key_reader=CTL.read_key):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()) as out:
            rc = CTL.main(["--api-key-file", self.keyfile] + argv, opener=fake,
                          sleep=lambda s: None, clock=iter(range(0, 10000, 1)).__next__,
                          key_reader=key_reader)
        return rc, err.getvalue() + out.getvalue()

    def deploy_argv(self, *extra, name=NAME, image=IMAGE):
        return ["deploy", "--image", image, "--substrate-digest", DIGEST, "--name", name,
                "--out", self.out, *extra]

    def record(self, name="s8-service.json"):
        return json.loads((Path(self.out) / name).read_text())

    def test_deploy_payload_and_live(self):
        fake = Fake(deploy_routes(["build_in_progress", "live"]))
        rc, log = self.run_main(self.deploy_argv("--operator-hold", "--owner-email", "a@b.c"), fake)
        self.assertEqual(rc, 0, log)
        payload = json.loads(fake.methods("POST")[0][2])
        env = {e["key"]: e["value"] for e in payload["envVars"]}
        self.assertEqual(env["RYEOS_INIT_HOST_CLASS"], "hosted")
        self.assertEqual(env["RYEOS_SUBSTRATE_IMAGE"], "ghcr.io/leolilley/ryeos-f5r-controller@" + DIGEST)
        self.assertEqual(env["RYEOS_SUBSTRATE_PROTOCOL"], "1")
        self.assertEqual(env["RYEOS_TRUST_BAKED_PUBLISHERS"], "1")
        self.assertEqual(env["PORT"], "8000")
        self.assertEqual(env["RYEOS_OPERATOR_HOLD"], "1")
        details = payload["serviceDetails"]
        self.assertEqual(details["disk"]["mountPath"], "/data")
        self.assertEqual(details["healthCheckPath"], "/_ryeos/ready")
        self.assertEqual((details["autoDeploy"], details["env"]), ("no", "image"))
        self.assertNotIn("envSpecificDetails", details)
        self.assertNotIn("autoDeploy", payload)
        record = self.record()
        self.assertEqual((record["state"], record["serviceId"], record["ready"]["status"]), ("live", "srv-new", 200))
        self.assertIn("RYEOS_OPERATOR_HOLD", record["envKeys"])
        self.assertNotIn("hosted", json.dumps(record))

    def test_no_hold_by_default(self):
        payload = CTL.build_payload(CTL.parser().parse_args(self.deploy_argv()), "own-1", None)
        self.assertNotIn("RYEOS_OPERATOR_HOLD", [e["key"] for e in payload["envVars"]])

    def test_bad_terminal_statuses_fail(self):
        for status in ("build_failed", "update_failed", "canceled", "deactivated", "pre_deploy_failed"):
            with self.subTest(status=status):
                rc, _ = self.run_main(self.deploy_argv(), Fake(deploy_routes(["build_in_progress", status])))
                self.assertEqual(rc, 1)
                self.assertEqual(self.record()["state"], "failed")

    def test_non_terminal_keeps_polling_then_times_out(self):
        fake = Fake(deploy_routes(["build_in_progress", "update_in_progress", "created"]))
        rc, _ = self.run_main(self.deploy_argv("--timeout", "5"), fake)
        self.assertEqual(rc, 1)
        self.assertEqual(self.record()["state"], "timeout")
        polls = [r for r in fake.requests if r[1].endswith("/deploys/dep-1")]
        self.assertGreater(len(polls), 3)

    def test_live_with_bad_probe_warns(self):
        routes = deploy_routes(["live"])
        routes[("GET", READY)] = [(503, {})]
        rc, log = self.run_main(self.deploy_argv(), Fake(routes))
        self.assertEqual(rc, 0)
        self.assertIn("warning: live but readiness probe returned 503", log)

    def test_refusals_send_no_request(self):
        cases = [
            self.deploy_argv(name="ryeos-f5r-s9-farm-x"),
            self.deploy_argv(name="snap-track-x"),
            self.deploy_argv(name="other-name"),
            self.deploy_argv(image="ghcr.io/leolilley/snap-track@sha256:" + "b" * 64),
            ["deploy", "--image", "ghcr.io/x/y:latest", "--substrate-digest", DIGEST, "--out", self.out],
            ["status", "--service-id", CTL.FARM_SERVICE_ID],
            ["status", "--service-id", " " + CTL.FARM_SERVICE_ID.upper() + "\n"],
            ["teardown", "--service-id", CTL.FARM_SERVICE_ID, "--confirm"],
        ]
        for argv in cases:
            with self.subTest(argv=argv):
                fake = Fake({})
                rc, _ = self.run_main(argv, fake)
                self.assertEqual(rc, 2)
                self.assertEqual(fake.requests, [])

    def test_same_name_service_refuses(self):
        routes = deploy_routes(["live"])
        routes[("GET", f"/services?name={NAME}&ownerId=own-1")] = [(200, [{"service": {"id": "srv-old", "name": NAME}}])]
        fake = Fake(routes)
        rc, log = self.run_main(self.deploy_argv("--ghcr-token-pass", "x"), fake)
        self.assertEqual(rc, 2)
        self.assertIn("srv-old", log)
        self.assertEqual(fake.methods("POST"), [])

    def test_existing_credential_reused(self):
        routes = deploy_routes(["live"])
        routes[("GET", "/registrycredentials?limit=100")] = [
            (200, [{"registryCredential": {"id": "rcred-old", "name": NAME + "-ghcr"}, "cursor": "c"}])]
        fake = Fake(routes)
        rc, _ = self.run_main(self.deploy_argv("--ghcr-token-pass", "x"), fake)
        self.assertEqual(rc, 0)
        self.assertEqual([r[1] for r in fake.methods("POST")], ["/services"])
        self.assertEqual(self.record()["registryCredentialId"], "rcred-old")
        self.assertFalse(self.record()["registryCredentialCreated"])

    def test_token_never_in_logs_or_files(self):
        fake = Fake(deploy_routes(["live"]))
        with mock.patch.object(CTL, "read_pass", lambda entry: TOKEN):
            rc, log = self.run_main(self.deploy_argv("--ghcr-token-pass", "ryeos/ghcr-read-token"), fake)
        self.assertEqual(rc, 0, log)
        cred = fake.methods("POST")[0]
        self.assertEqual(json.loads(cred[2])["registry"], "GITHUB")
        self.assertIn(TOKEN, cred[2].decode())
        blob = log + "".join(p.read_text() for p in Path(self.out).glob("s8-*.json"))
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn(KEY, blob)
        service_post = fake.methods("POST")[1]
        self.assertNotIn(TOKEN, service_post[2].decode())
        self.assertEqual(service_post[3]["Authorization"], "Bearer " + KEY)

    def test_error_body_secrets_not_echoed(self):
        routes = deploy_routes(["live"])
        routes[("POST", "/services")] = [(400, {"message": f"bad {TOKEN} and {KEY}"})]
        with mock.patch.object(CTL, "read_pass", lambda entry: TOKEN):
            rc, log = self.run_main(self.deploy_argv("--ghcr-token-pass", "p"), Fake(routes))
        self.assertEqual(rc, 1)
        self.assertIn("message=bad ***", log)
        blob = log + (Path(self.out) / "s8-service.json").read_text()
        self.assertNotIn(TOKEN, blob)
        self.assertNotIn(KEY, blob)

    def test_partial_failure_surfaces_credential_id(self):
        routes = deploy_routes(["live"])
        routes[("POST", "/services")] = [(500, {})]
        with mock.patch.object(CTL, "read_pass", lambda entry: TOKEN):
            rc, log = self.run_main(self.deploy_argv("--ghcr-token-pass", "p"), Fake(routes))
        self.assertEqual(rc, 1)
        self.assertIn("rcred-1", log)
        record = self.record()
        self.assertEqual((record["registryCredentialId"], record["state"]), ("rcred-1", "create-failed"))

    def test_poll_failure_keeps_service_id(self):
        routes = deploy_routes(["live"])
        routes[("GET", "/services/srv-new/deploys/dep-1")] = [(500, {})]
        rc, log = self.run_main(self.deploy_argv(), Fake(routes))
        self.assertEqual(rc, 1)
        self.assertIn("srv-new", log)
        record = self.record()
        self.assertEqual((record["serviceId"], record["state"]), ("srv-new", "poll-error"))

    def test_interrupt_keeps_service_id(self):
        fake = Fake(deploy_routes(["build_in_progress"]))
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(KeyboardInterrupt):
                CTL.main(["--api-key-file", self.keyfile] + self.deploy_argv(), opener=fake,
                         sleep=mock.Mock(side_effect=KeyboardInterrupt), clock=iter(range(0, 99)).__next__)
        self.assertEqual(self.record()["serviceId"], "srv-new")
        self.assertEqual(self.record()["state"], "interrupted")

    def teardown_routes(self, name=NAME, gone_after=True):
        return {
            ("GET", "/services/srv-new"): [(200, {"id": "srv-new", "name": name})] + ([(404, None)] if gone_after else []),
            ("DELETE", "/services/srv-new"): [(204, None)],
            ("DELETE", "/registrycredentials/rcred-1"): [(204, None)],
            ("GET", "/registrycredentials?limit=100"): [(200, [{"registryCredential": {"id": "rcred-1", "name": NAME + "-ghcr"}}])],
        }

    def test_teardown_verifies_404(self):
        fake = Fake(self.teardown_routes())
        rc, log = self.run_main(["teardown", "--service-id", "srv-new", "--registry-credential-id", "rcred-1",
                                 "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 0, log)
        self.assertTrue(self.record("s8-teardown.json")["verifiedGone"])
        self.assertEqual(len(fake.methods("DELETE")), 2)
        self.assertEqual(fake.requests[0][:2], ("GET", "/services/srv-new"))

    def test_teardown_refuses_farm_named_service(self):
        fake = Fake(self.teardown_routes(name="farm-x"))
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 2)
        self.assertEqual(fake.methods("DELETE"), [])
        fake = Fake(self.teardown_routes(name="other"))
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 2)
        self.assertEqual(fake.methods("DELETE"), [])

    def test_teardown_credential_must_belong_to_deploy(self):
        routes = self.teardown_routes()
        routes[("GET", "/registrycredentials?limit=100")] = [(200, [{"registryCredential": {"id": "rcred-1", "name": "mine"}}])]
        fake = Fake(routes)
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--registry-credential-id", "rcred-1",
                               "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 2)
        self.assertEqual(fake.methods("DELETE"), [])
        (Path(self.out) / "s8-service.json").write_text(json.dumps(
            {"serviceId": "srv-new", "registryCredentialCreated": True, "registryCredentialId": "rcred-1"}))
        fake = Fake(routes)
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--registry-credential-id", "rcred-1",
                               "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 0)

    def test_teardown_already_gone_uses_record_and_continues(self):
        (Path(self.out) / "s8-service.json").write_text(json.dumps(
            {"serviceId": "srv-new", "name": NAME, "registryCredentialCreated": True, "registryCredentialId": "rcred-1"}))
        routes = {
            ("GET", "/services/srv-new"): [(404, None)],
            ("DELETE", "/registrycredentials/rcred-1"): [(204, None)],
        }
        fake = Fake(routes)
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--registry-credential-id", "rcred-1",
                               "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 0)
        self.assertEqual([r[1] for r in fake.methods("DELETE")], ["/registrycredentials/rcred-1"])

    def test_teardown_still_present_fails_and_needs_confirm(self):
        routes = self.teardown_routes(gone_after=False)
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--out", self.out], Fake(routes))
        self.assertEqual(rc, 2)
        fake = Fake(routes)
        rc, _ = self.run_main(["teardown", "--service-id", "srv-new", "--confirm", "--out", self.out], fake)
        self.assertEqual(rc, 1)
        self.assertEqual(len([r for r in fake.requests if r[:2] == ("GET", "/services/srv-new")]), 4)

    def test_key_file_handling(self):
        def read(text):
            Path(self.keyfile).write_bytes(text)
            return CTL.read_key(CTL.parser().parse_args(["--api-key-file", self.keyfile, "status", "--service-id", "s"]))
        self.assertEqual(read((KEY + "\n").encode()), KEY)
        self.assertEqual(read((KEY + "\r\n").encode()), KEY)
        with self.assertRaises(CTL.Failure):
            read(b"abc\r\nX")
        with self.assertRaises(CTL.Failure):
            read(b"ab\x07c")

    def test_pass_failure_stderr_not_echoed(self):
        done = mock.Mock(returncode=1, stdout="", stderr="leaked " + KEY)
        with mock.patch.object(CTL.subprocess, "run", return_value=done):
            rc, log = self.run_main(["status", "--service-id", "srv-a"], Fake({}),
                                    key_reader=lambda args: CTL.read_pass("ryeos/render-api-key"))
        self.assertEqual(rc, 1)
        self.assertNotIn(KEY, log)

    def test_unexpected_exception_prints_type_only(self):
        def boom(args):
            raise RuntimeError("secret " + KEY)
        rc, log = self.run_main(["status", "--service-id", "srv-a"], Fake({}), key_reader=boom)
        self.assertEqual(rc, 1)
        self.assertIn("RuntimeError", log)
        self.assertNotIn(KEY, log)

    def test_redirect_handler_refuses(self):
        self.assertIsNone(CTL.NoRedirect().redirect_request(None, None, 302, "", {}, "https://evil/"))


if __name__ == "__main__":
    unittest.main()
