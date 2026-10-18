#!/usr/bin/env python3
import base64
import hashlib
import importlib.util
import json
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


class FakeCli:
    """Answers ryeos argv lists; records every call."""

    def __init__(self, manifest=None, fail_on=None, activation=None, capture_state="captured",
                 thread_status="completed", launch=None, objects=None, empty_threads=False, evidence_manifest=None):
        self.calls, self.manifest, self.fail_on = [], manifest or good_manifest(), fail_on
        self.threads = {}
        self.activation = list(activation or [{"state": "completed", "receipt_hash": H("9"), "activation_id": "A-1"}])
        self.capture_state, self.thread_status, self.launch = capture_state, thread_status, launch
        self.objects, self.empty_threads = objects, empty_threads
        self.evidence_manifest = evidence_manifest or H("c")

    def __call__(self, argv, env, timeout):
        self.calls.append(argv)
        rest = argv[1:]
        if self.fail_on and self.fail_on in " ".join(rest):
            return 3, "", f"boom {SECRET}"
        if rest[0] == "execute" and "--async" in rest:
            ref = rest[2]
            tid = "T-codex" if "guest-runtime-production" in ref and "owner" not in ref else "T-owner"
            self.threads[tid] = ref
            return 0, json.dumps(self.launch if self.launch is not None else
                                 {"status": "accepted", "launch_id": "L-x", "thread_id": tid}), ""
        if rest[:2] == ["thread", "get"]:
            tid = rest[2]
            if self.empty_threads:
                return 0, "", ""
            result = {"manifest_hash": H("c")} if tid == "T-owner" else {"sha256": "x"}
            return 0, json.dumps({"thread": {"thread_id": tid, "chain_root_id": tid, "status": self.thread_status},
                                  "result": {"result": result}}), ""
        if rest[:2] == ["external-content", "capture-product"]:
            w = H("d") if rest[3] == "T-owner" else H("e")
            return 0, json.dumps({"state": self.capture_state, "witness_hash": w,
                                  "evidence": {"manifest_hash": self.evidence_manifest}}), ""
        if rest[:2] == ["external-content", "activate"]:
            step = self.activation.pop(0) if len(self.activation) > 1 else self.activation[0]
            return 0, json.dumps(step), ""
        if rest[:2] == ["execute", "service:identity/public_key"]:
            return 0, json.dumps({"signing_key": KEY}), ""
        if rest[:2] == ["execute", "service:objects/get"]:
            if self.objects is not None:
                return 0, json.dumps(self.objects), ""
            return 0, json.dumps({"entries": [{"hash": H("c"), "kind": "object", "value": self.manifest}]}), ""
        return 9, "", "unknown"


class Resp:
    def __init__(self, body):
        self.body = body

    def read(self):
        return json.dumps(self.body).encode()

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def run(cli, extra, opener=None, clock=None):
    out = tempfile.mkdtemp()
    lines = []
    code = GP.main(["--app-root", "/app", "--out", out] + extra, runner=cli,
                   opener=opener or (lambda req, t: Resp({"signing_key": KEY})),
                   sleep=lambda s: None, environ={"MY_API_TOKEN": SECRET}, emit=lines.append,
                   **({"clock": clock} if clock else {}))
    handoff = json.loads((Path(out) / "guest-products-handoff.json").read_text())
    return code, handoff, lines, out


class Tests(unittest.TestCase):
    def test_happy_path_for_controller_auto_key(self):
        cli = FakeCli()
        out = tempfile.mkdtemp()
        lines = []
        code = GP.main(["--controller-url", "https://c.example", "--out", out, "--owner-production",
                        "for-controller", "--controller-public-key", "auto"], runner=cli,
                       opener=lambda req, t: Resp({"signing_key": KEY}), sleep=lambda s: None,
                       environ={}, emit=lines.append)
        h = json.loads((Path(out) / "guest-products-handoff.json").read_text())
        self.assertEqual(code, 0)
        self.assertEqual(h["codex_witness_hash"], H("e"))
        self.assertEqual(h["owner_witness_hash"], H("d"))
        self.assertEqual(h["owner_manifest_hash"], H("c"))
        self.assertEqual(h["controller_public_key"], KEY)
        self.assertEqual(h["thread_ids"], {"codex": "T-codex", "owner": "T-owner"})
        owner_launch = [c for c in cli.calls if c[1:3] == ["execute", "--async"] or "--async" in c][-1]
        self.assertIn("guest-owner-runtime-production-for-controller", " ".join(owner_launch))
        self.assertEqual(json.loads(owner_launch[owner_launch.index("--input") + 1]),
                         {"controller_public_key": KEY})

    def test_local_with_hex_key_checks_root(self):
        code, h, _, _ = run(FakeCli(), ["--controller-public-key", ROOT_BYTES.hex()])
        self.assertEqual(code, 0)
        self.assertEqual(h["controller_public_key"], KEY)

    def test_fifth_entry_fails_preflight(self):
        extra = [{"path": "extra", "kind": "file", "mode": 0o644, "size": 1, "blob_hash": H("f")}]
        code, h, lines, _ = run(FakeCli(manifest=good_manifest(extra)), [])
        self.assertEqual(code, 1)
        self.assertEqual(h["owner_witness_hash"], H("d"))
        self.assertFalse(h["complete"])
        self.assertTrue(any("unexpected entry extra" in l for l in lines))

    def test_wrong_mode_fails_preflight(self):
        code, _, lines, _ = run(FakeCli(manifest=good_manifest(owner_mode=0o644)), [])
        self.assertEqual(code, 1)
        self.assertTrue(any("mode 420 != 493" in l for l in lines))

    def test_wrong_root_size_fails(self):
        m = good_manifest()
        m["entries"][2]["size"] = 63
        self.assertTrue(any("!= 64" in p for p in GP.preflight_owner_manifest(m)))

    def test_skip_codex(self):
        cli = FakeCli()
        code, h, _, _ = run(cli, ["--skip-codex"])
        self.assertEqual(code, 0)
        self.assertIsNone(h["codex_witness_hash"])
        self.assertNotIn("codex", h["thread_ids"])
        self.assertFalse(any("activate" in c for c in cli.calls))

    def test_failed_execute_writes_partial_handoff(self):
        code, h, lines, out = run(FakeCli(fail_on="guest-owner-runtime-production"), [])
        self.assertEqual(code, 1)
        self.assertEqual(h["codex_witness_hash"], H("e"))
        self.assertIsNone(h["owner_witness_hash"])
        self.assertFalse(h["complete"])

    def test_no_secret_in_logs(self):
        code, _, lines, out = run(FakeCli(fail_on="activate"), [])
        self.assertEqual(code, 1)
        text = "\n".join(lines) + (Path(out) / "guest-products-steps.jsonl").read_text()
        self.assertNotIn(SECRET, text)
        self.assertIn("***", text)

    def test_for_controller_without_key_is_refused(self):
        code, h, lines, _ = run(FakeCli(), ["--owner-production", "for-controller"])
        self.assertEqual(code, 1)
        self.assertIsNone(h["owner_witness_hash"])
        self.assertTrue(any("needs a controller public key" in l for l in lines))

    def ticking(self, step=100):
        t = [0]

        def clock():
            t[0] += step
            return t[0]
        return clock

    def test_activation_planned_running_completed(self):
        steps = [{"state": "planned"}, {"state": "running"},
                 {"state": "completed", "receipt_hash": H("9"), "activation_id": "A-1", "job_id": "J"}]
        cli = FakeCli(activation=steps)
        code, h, lines, _ = run(cli, [])
        self.assertEqual(code, 0)
        self.assertEqual(sum(1 for c in cli.calls if c[1:3] == ["external-content", "activate"]), 3)
        self.assertEqual(h["activation_id"], "A-1")
        self.assertEqual(h["activation_receipt_hash"], H("9"))
        self.assertTrue(any("codex.activated" in l for l in lines))

    def test_activation_stuck_times_out(self):
        code, h, lines, _ = run(FakeCli(activation=[{"state": "running"}]), ["--timeout", "300"],
                                clock=self.ticking())
        self.assertEqual(code, 1)
        self.assertIsNone(h["codex_witness_hash"])
        self.assertTrue(any("not completed within" in l for l in lines))

    def test_activation_completed_without_receipt_keeps_waiting(self):
        code, _, lines, _ = run(FakeCli(activation=[{"state": "completed"}]), ["--timeout", "300"],
                                clock=self.ticking())
        self.assertEqual(code, 1)
        self.assertTrue(any("not completed within" in l for l in lines))

    def test_activation_nonzero_exit(self):
        code, _, lines, _ = run(FakeCli(fail_on="activate"), [])
        self.assertEqual(code, 1)
        self.assertTrue(any("codex.activate: exit 3" in l for l in lines))

    def test_wrong_manifest_kind(self):
        m = good_manifest()
        m["kind"] = "tree"
        code, _, lines, _ = run(FakeCli(manifest=m), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("is not external_content_manifest" in l for l in lines))

    def test_entry_count_mismatch_and_non_dict_entry(self):
        m = good_manifest()
        m["entry_count"] = 3
        self.assertTrue(any("entry_count" in p for p in GP.preflight_owner_manifest(m)))
        m = good_manifest()
        m["entries"].append("bin")
        self.assertTrue(any("must be objects" in p for p in GP.preflight_owner_manifest(m)))

    def test_capture_state_missing_and_absent_optional(self):
        for state in ("missing", "absent_optional"):
            code, _, lines, _ = run(FakeCli(capture_state=state), ["--skip-codex"])
            self.assertEqual(code, 1)
            self.assertTrue(any(f"state '{state}'" in l for l in lines))

    def test_capture_without_state_fails(self):
        self.assertEqual(run(FakeCli(capture_state=None), ["--skip-codex"])[0], 1)

    def test_manifest_hash_mismatch(self):
        code, _, lines, _ = run(FakeCli(evidence_manifest=H("7")), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("differs from the captured evidence" in l for l in lines))

    def test_thread_never_terminal_times_out(self):
        code, _, lines, _ = run(FakeCli(thread_status="running"), ["--skip-codex", "--timeout", "300"],
                                clock=self.ticking())
        self.assertEqual(code, 1)
        self.assertTrue(any("not terminal within" in l for l in lines))

    def test_thread_failed(self):
        code, _, lines, _ = run(FakeCli(thread_status="failed"), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("ended failed" in l for l in lines))

    def test_three_empty_views_fail(self):
        code, _, lines, _ = run(FakeCli(empty_threads=True), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("3 empty views" in l for l in lines))

    def test_no_thread_id(self):
        code, _, lines, _ = run(FakeCli(launch={"status": "accepted"}), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("no thread id" in l for l in lines))

    def test_http_url_refused_writes_nothing(self):
        out = tempfile.mkdtemp()
        code = GP.main(["--controller-url", "http://c.example", "--out", out], runner=FakeCli(), environ={})
        self.assertEqual(code, 2)
        self.assertEqual(list(Path(out).iterdir()), [])

    def test_invalid_key_forms(self):
        for bad in ("zz", "ed25519:" + base64.b64encode(b"short").decode(), "ab" * 31, "ed25519:!!!"):
            with self.assertRaises((GP.Refusal, ValueError)):
                GP.normalize_public_key(bad)

    def test_missing_object(self):
        objects = {"entries": [{"hash": H("c"), "kind": "missing_object"}]}
        code, _, lines, _ = run(FakeCli(objects=objects), ["--skip-codex"])
        self.assertEqual(code, 1)
        self.assertTrue(any("manifest object not returned" in l for l in lines))

    def test_controller_root_mismatch(self):
        other = "ed25519:" + base64.b64encode(bytes(range(2, 34))).decode()
        problems = GP.preflight_owner_manifest(good_manifest(), other)
        self.assertTrue(any("does not match the controller public key" in p for p in problems))

    def test_dir_entry_with_blob_hash(self):
        m = good_manifest()
        m["entries"][0]["blob_hash"] = H("a")
        self.assertTrue(any("directory carries" in p for p in GP.preflight_owner_manifest(m)))

    def test_profile_over_4096(self):
        m = good_manifest()
        m["entries"][3]["size"] = 4097
        self.assertTrue(any("exceeds 4096" in p for p in GP.preflight_owner_manifest(m)))

    def test_duplicate_entries(self):
        m = good_manifest()
        m["entries"].append(dict(m["entries"][0]))
        m["entry_count"] = 5
        self.assertTrue(any("duplicate entry" in p for p in GP.preflight_owner_manifest(m)))

    def test_local_auto_key_read_from_node(self):
        cli = FakeCli()
        code, h, _, _ = run(cli, ["--skip-codex", "--controller-public-key", "auto"])
        self.assertEqual(code, 0)
        self.assertEqual(h["controller_public_key"], KEY)
        self.assertTrue(any(c[1:3] == ["execute", "service:identity/public_key"] for c in cli.calls))

    def test_steps_log_appends(self):
        out = tempfile.mkdtemp()
        for _ in range(2):
            GP.main(["--app-root", "/a", "--out", out, "--skip-codex"], runner=FakeCli(),
                    sleep=lambda s: None, environ={}, emit=lambda l: None)
        lines = (Path(out) / "guest-products-steps.jsonl").read_text().splitlines()
        self.assertEqual(sum(1 for l in lines if '"step": "handoff"' in l), 2)


if __name__ == "__main__":
    unittest.main()
