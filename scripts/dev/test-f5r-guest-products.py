#!/usr/bin/env python3
import base64
import hashlib
import importlib.util
import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("gp", Path(__file__).with_name("f5r-guest-products.py"))
GP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GP)

ROOT_BYTES = bytes(range(1, 33))
KEY = "ed25519:" + base64.b64encode(ROOT_BYTES).decode()
H = lambda c: c * 64
SECRET = "tok_SECRETVALUE123"

# The fake `ryeos` binary: a Python shim on PATH. It logs argv and answers from a scenario file.
SHIM = r'''#!/usr/bin/env python3
import json, os, sys
scn = json.load(open(os.environ["FAKE_RYEOS_SCENARIO"]))
log = os.environ["FAKE_RYEOS_LOG"]
rest = sys.argv[1:]
prior = [json.loads(l) for l in open(log)] if os.path.exists(log) else []
with open(log, "a") as f:
    f.write(json.dumps(rest) + "\n")
joined = " ".join(rest)
if scn.get("fail_on") and scn["fail_on"] in joined:
    print("boom " + os.environ.get("MY_API_TOKEN", ""), file=sys.stderr)
    sys.exit(3)
def out(v):
    print(json.dumps(v)); sys.exit(0)
if rest[:1] == ["--project"]:
    project = rest[1]
    rest = rest[2:]
    if not os.path.isdir(os.path.join(project, ".ai")):
        print("fake ryeos: project root lacks .ai", file=sys.stderr); sys.exit(7)
if rest[0] == "execute" and "--async" in rest:
    ref = rest[3] if rest[2] == "--pin-project" else rest[2]
    tid = "T-owner" if "owner" in ref else "T-codex"
    out(scn.get("launch") or {"status": "accepted", "thread_id": tid})
if rest[:2] == ["thread", "get"]:
    tid = rest[2]
    if scn.get("empty_threads"):
        sys.exit(0)
    result = {"manifest_hash": scn["manifest_hash"]} if tid == "T-owner" else {"sha256": "x"}
    out({"thread": {"thread_id": tid, "chain_root_id": tid, "status": scn.get("thread_status", "completed")},
         "result": {"result": result}})
if rest[:2] == ["external-content", "capture-product"]:
    w = "d" * 64 if rest[3] == "T-owner" else "e" * 64
    out({"state": scn.get("capture_state", "captured"), "witness_hash": w,
         "evidence": {"manifest_hash": scn.get("evidence_manifest", scn["manifest_hash"])}})
if rest[:2] == ["external-content", "activate"]:
    n = sum(1 for p in prior if p[:2] == ["external-content", "activate"])
    seq = scn["activation"]
    out(seq[min(n, len(seq) - 1)])
if rest[:2] == ["execute", "service:objects/get"]:
    out({"entries": [{"hash": scn["manifest_hash"], "kind": scn.get("object_kind", "object"),
                      "value": scn["manifest"]}]})
sys.exit(9)
'''


def good_manifest(extra=None, owner_mode=0o755):
    entries = [
        {"path": "bin", "kind": "dir"},
        {"path": "bin/ryeos-external-guest-occurrence-owner", "kind": "file", "mode": owner_mode,
         "blob_hash": H("a"), "size": 1000},
        {"path": "controller-root.hex", "kind": "file", "mode": 0o644, "size": 64,
         "blob_hash": hashlib.sha256(ROOT_BYTES.hex().encode()).hexdigest()},
        {"path": "guest-owner-profile.json", "kind": "file", "mode": 0o644, "size": 90, "blob_hash": H("b")},
    ]
    entries += extra or []
    return {"schema": "ryeos.external_content_manifest.v1", "kind": "external_content_manifest",
            "entries": entries, "entry_count": len(entries), "total_bytes": 1154}


class Resp:
    def __init__(self, body):
        self.body = body

    def read(self):
        return json.dumps(self.body).encode()

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class Case(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        bindir = self.tmp / "bin"
        bindir.mkdir()
        shim = bindir / "ryeos"
        shim.write_text(SHIM)
        shim.chmod(shim.stat().st_mode | stat.S_IXUSR)
        self.app = self.tmp / "app"
        keydir = self.app / ".ai/config/keys/signing"
        keydir.mkdir(parents=True)
        (keydir / "private_key.pem").write_text("pem")
        self.log = self.tmp / "calls.log"
        self.scn = {"manifest": good_manifest(), "manifest_hash": H("c"),
                    "activation": [{"state": "completed", "receipt_hash": H("9"), "activation_id": "A-1"}]}
        self.evidence, self.handoff = self.tmp / "evidence", self.tmp / "handoff.json"
        self.bindir = str(bindir)

    def go(self, extra=(), scn=None, opener=None, env=None, clock=None):
        self.scn.update(scn or {})
        (self.tmp / "scn.json").write_text(json.dumps(self.scn))
        environ = {"PATH": self.bindir + os.pathsep + os.environ["PATH"],
                   "FAKE_RYEOS_SCENARIO": str(self.tmp / "scn.json"),
                   "FAKE_RYEOS_LOG": str(self.log), "MY_API_TOKEN": SECRET}
        environ.update(env or {})
        self.lines = []
        kw = {"clock": clock} if clock else {}
        code = GP.main(["--app-root", str(self.app), "--evidence-dir", str(self.evidence),
                        "--handoff", str(self.handoff), *extra],
                       opener=opener or (lambda req, t: Resp({"signing_key": KEY})),
                       sleep=lambda s: None, environ=environ, emit=self.lines.append, **kw)
        return code

    def calls(self):
        return [json.loads(l) for l in self.log.read_text().splitlines()] if self.log.exists() else []

    def project_root(self):
        return str(self.evidence.parent / "project")

    def handoff_json(self):
        return json.loads(self.handoff.read_text())

    def state(self):
        return json.loads((self.evidence / "state.json").read_text())

    def text(self):
        return "\n".join(self.lines)

    def ticking(self, step=100):
        t = [0]

        def clock():
            t[0] += step
            return t[0]
        return clock


class Tests(Case):
    def test_happy_path_sequence_and_handoff(self):
        self.assertEqual(self.go(), 0)
        calls = self.calls()
        self.assertEqual(calls, [
            ["external-content", "activate", GP.ACTIVATION_REF, "online"],
            ["--project", self.project_root(), "execute", "--async", "--pin-project", GP.CODEX_GRAPH, "--no-stream"],
            ["thread", "get", "T-codex"],
            ["external-content", "capture-product", "T-codex", "T-codex", "runtime"],
            ["--project", self.project_root(), "execute", "--async", "--pin-project", GP.OWNER_GRAPHS["local"], "--no-stream"],
            ["thread", "get", "T-owner"],
            ["external-content", "capture-product", "T-owner", "T-owner", "runtime"],
            ["execute", "service:objects/get", "--no-stream", "--input",
             json.dumps({"object_hashes": [H("c")]})],
        ])
        h = self.handoff_json()
        self.assertEqual({k: h[k] for k in ("owner_manifest_hash", "owner_witness_hash", "codex_witness_hash",
                                            "guest_runtime_manifest_hash", "controller_public_key")},
                         {"owner_manifest_hash": H("c"), "owner_witness_hash": H("d"),
                          "codex_witness_hash": H("e"), "guest_runtime_manifest_hash": H("c"),
                          "controller_public_key": KEY})
        self.assertTrue(h["complete"])
        self.assertEqual(self.state()["steps"]["owner.preflight"]["ok"], True)

    def test_every_call_logs_argv_and_keeps_output(self):
        self.go()
        steps = [json.loads(l) for l in (self.evidence / "steps.jsonl").read_text().splitlines()]
        runs = [s for s in steps if "argv" in s]
        self.assertEqual(len(runs), 8)
        self.assertTrue(all(s["argv"][0] == "ryeos" for s in runs))
        for s in runs:
            self.assertTrue((self.evidence / (s["evidence"] + ".stdout")).exists())
            self.assertTrue((self.evidence / (s["evidence"] + ".stderr")).exists())

    def test_for_controller_passes_key_input(self):
        self.assertEqual(self.go(["--owner-production", "for-controller"]), 0)
        launch = [c for c in self.calls() if "--async" in c][-1]
        self.assertIn("guest-owner-runtime-production-for-controller", launch[launch.index("--pin-project") + 1])
        self.assertEqual(json.loads(launch[launch.index("--input") + 1]), {"controller_public_key": KEY})

    def test_rerun_after_success_issues_no_cli_calls(self):
        self.go()
        before = len(self.calls())
        self.assertEqual(self.go(), 0)
        self.assertEqual(len(self.calls()), before)
        self.assertEqual(self.handoff_json()["owner_witness_hash"], H("d"))

    def test_resume_after_capture_failure_does_not_relaunch(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"capture_state": "missing"}), 1)
        self.assertEqual(self.state()["steps"]["owner.thread"]["status"], "completed")
        self.assertEqual(self.go(["--skip-codex"], scn={"capture_state": "captured"}), 0)
        launches = [c for c in self.calls() if "--async" in c]
        self.assertEqual(len(launches), 1)

    def test_resume_running_thread_repolls_same_thread(self):
        self.assertEqual(self.go(["--skip-codex", "--timeout", "300"], scn={"thread_status": "running"},
                                 clock=self.ticking()), 1)
        self.assertEqual(self.state()["steps"]["owner.thread"]["status"], "launched")
        self.assertEqual(self.go(["--skip-codex"], scn={"thread_status": "completed"}), 0)
        self.assertEqual(len([c for c in self.calls() if "--async" in c]), 1)

    def test_died_while_launching_refuses_relaunch(self):
        self.evidence.mkdir()
        (self.evidence / "state.json").write_text(json.dumps(
            {"steps": {"owner.thread": {"status": "launching"}}}))
        self.assertEqual(self.go(["--skip-codex"]), 1)
        self.assertIn("a thread may exist", self.text())
        self.assertEqual(self.calls(), [])

    def test_failed_launch_may_relaunch(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"fail_on": "--async"}), 1)
        self.assertEqual(self.state()["steps"]["owner.thread"]["status"], "launch_failed")
        self.assertEqual(self.go(["--skip-codex"], scn={"fail_on": None}), 0)

    def test_fifth_entry_fails_preflight(self):
        extra = [{"path": "extra", "kind": "file", "mode": 0o644, "size": 1, "blob_hash": H("f")}]
        self.assertEqual(self.go(scn={"manifest": good_manifest(extra)}), 1)
        h = self.handoff_json()
        self.assertEqual(h["owner_witness_hash"], H("d"))
        self.assertIsNone(h["guest_runtime_manifest_hash"])
        self.assertFalse(h["complete"])
        self.assertIn("unexpected entry extra", self.text())

    def test_wrong_mode_and_root_mismatch(self):
        self.assertEqual(self.go(scn={"manifest": good_manifest(owner_mode=0o644)}), 1)
        self.assertIn("mode 420 != 493", self.text())
        other = "ed25519:" + base64.b64encode(bytes(range(2, 34))).decode()
        self.assertTrue(any("does not match" in p for p in GP.preflight_owner_manifest(good_manifest(), other)))

    def test_preflight_unit_rules(self):
        m = good_manifest()
        m["entries"][2]["size"] = 63
        self.assertTrue(any("!= 64" in p for p in GP.preflight_owner_manifest(m)))
        m = good_manifest()
        m["entries"][0]["blob_hash"] = H("a")
        self.assertTrue(any("directory carries" in p for p in GP.preflight_owner_manifest(m)))
        m = good_manifest()
        m["entries"][3]["size"] = 4097
        self.assertTrue(any("exceeds 4096" in p for p in GP.preflight_owner_manifest(m)))
        m = good_manifest()
        m["entries"].append(dict(m["entries"][0]))
        m["entry_count"] = 5
        self.assertTrue(any("duplicate entry" in p for p in GP.preflight_owner_manifest(m)))
        m = good_manifest()
        m["entry_count"] = 3
        self.assertTrue(any("entry_count" in p for p in GP.preflight_owner_manifest(m)))

    def test_skip_codex(self):
        self.assertEqual(self.go(["--skip-codex"]), 0)
        self.assertIsNone(self.handoff_json()["codex_witness_hash"])
        self.assertFalse(any("activate" in c for c in self.calls()))

    def test_activation_polls_until_complete(self):
        act = [{"state": "planned"}, {"state": "running"},
               {"state": "completed", "receipt_hash": H("9"), "activation_id": "A-1"}]
        self.assertEqual(self.go(scn={"activation": act}), 0)
        self.assertEqual(sum(1 for c in self.calls() if c[:2] == ["external-content", "activate"]), 3)
        self.assertEqual(self.state()["steps"]["codex.activate"]["activation_id"], "A-1")

    def test_activation_stuck_times_out(self):
        self.assertEqual(self.go(["--timeout", "300"], scn={"activation": [{"state": "running"}]},
                                 clock=self.ticking()), 1)
        self.assertIn("not completed within", self.text())

    def test_failed_step_writes_partial_handoff_and_scrubs_secret(self):
        self.assertEqual(self.go(scn={"fail_on": "guest-owner-runtime-production"}), 1)
        h = self.handoff_json()
        self.assertEqual(h["codex_witness_hash"], H("e"))
        self.assertIsNone(h["owner_witness_hash"])
        self.assertFalse(h["complete"])
        everything = self.text() + "".join(p.read_text() for p in self.evidence.iterdir() if p.is_file())
        self.assertNotIn(SECRET, everything)
        self.assertIn("***", everything)

    def test_capture_must_be_captured(self):
        for state in ("missing", "absent_optional"):
            self.setUp()
            self.assertEqual(self.go(["--skip-codex"], scn={"capture_state": state}), 1)
            self.assertIn(f"state '{state}'", self.text())

    def test_manifest_hash_mismatch(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"evidence_manifest": H("7")}), 1)
        self.assertIn("differs from the captured evidence", self.text())

    def test_thread_failed_and_empty_views(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"thread_status": "failed"}), 1)
        self.assertIn("ended failed", self.text())
        self.setUp()
        self.assertEqual(self.go(["--skip-codex"], scn={"empty_threads": True}), 1)
        self.assertIn("3 empty views", self.text())

    def test_no_thread_id(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"launch": {"status": "accepted"}}), 1)
        self.assertIn("no thread id", self.text())

    def test_missing_manifest_object(self):
        self.assertEqual(self.go(["--skip-codex"], scn={"object_kind": "missing_object"}), 1)
        self.assertIn("manifest object not returned", self.text())

    def test_identity_and_url_refusals(self):
        self.assertEqual(GP.main(["--app-root", str(self.tmp / "nope"), "--evidence-dir", str(self.evidence),
                                  "--handoff", str(self.handoff)], environ={}), 2)
        self.assertEqual(GP.main(["--key-path", str(self.tmp / "k.pem"), "--evidence-dir", str(self.evidence),
                                  "--handoff", str(self.handoff)], environ={}), 2)
        self.assertEqual(GP.main(["--app-root", str(self.app), "--base-url", "http://c.example",
                                  "--evidence-dir", str(self.evidence), "--handoff", str(self.handoff)],
                                 environ={}), 2)
        self.assertFalse(self.handoff.exists())

    def test_key_path_derives_app_root_and_default_url(self):
        keyp = self.app / ".ai/config/keys/signing/private_key.pem"
        urls = []

        def opener(req, t):
            urls.append(req.full_url)
            return Resp({"signing_key": KEY})
        self.scn["manifest"] = good_manifest()
        (self.tmp / "scn.json").write_text(json.dumps(self.scn))
        code = GP.main(["--key-path", str(keyp), "--evidence-dir", str(self.evidence),
                        "--handoff", str(self.handoff), "--skip-codex"], opener=opener, sleep=lambda s: None,
                       environ={"PATH": self.bindir + os.pathsep + os.environ["PATH"],
                                "FAKE_RYEOS_SCENARIO": str(self.tmp / "scn.json"),
                                "FAKE_RYEOS_LOG": str(self.log)}, emit=lambda l: None)
        self.assertEqual(code, 0)
        self.assertEqual(urls, ["http://127.0.0.1:8000/public-key"])

    def test_public_key_forms(self):
        self.assertEqual(GP.normalize_public_key(ROOT_BYTES.hex()), KEY)
        for bad in ("zz", "ed25519:" + base64.b64encode(b"short").decode(), "ab" * 31, "ed25519:!!!"):
            with self.assertRaises((GP.Refusal, ValueError)):
                GP.normalize_public_key(bad)

    def test_state_file_is_atomic_json(self):
        self.go()
        self.assertFalse(any(p.name.endswith(".tmp") for p in self.evidence.iterdir()))
        self.assertEqual(set(self.state()["steps"]),
                         {"codex.activate", "codex.thread", "codex.capture", "owner.thread",
                          "owner.capture", "owner.preflight"})


if __name__ == "__main__":
    unittest.main()
