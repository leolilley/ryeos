#!/usr/bin/env python3
import hashlib
import importlib.util
import json
import os
import shutil
import stat
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


PS = load("ps", "f5r-product-ship.py")
H = lambda c: c * 64
SECRET = "tok_SECRETVALUE123"

SHIM = r'''#!/usr/bin/env python3
import json, os, sys
scn = json.load(open(os.environ["FAKE_RYEOS_SCENARIO"]))
rest = sys.argv[1:]
with open(os.environ["FAKE_RYEOS_LOG"], "a") as f:
    f.write(json.dumps({"argv": rest, "url": os.environ.get("RYEOSD_URL")}) + "\n")
if scn.get("fail_on") and scn["fail_on"] in " ".join(rest):
    print("boom " + os.environ.get("MY_API_TOKEN", ""), file=sys.stderr)
    sys.exit(3)
if rest[:2] == ["external-content", "import"]:
    name = "codex" if rest[3].startswith("f5r-ship/codex-") else "owner"
    root = os.path.join(scn["root_dir"], rest[3])
    assert os.path.isdir(root), "placement missing"
    print(json.dumps(scn["receipts"][name])); sys.exit(0)
if rest[:2] == ["execute", "service:objects/get"]:
    mh = json.loads(rest[rest.index("--input") + 1])["object_hashes"][0]
    print(json.dumps({"entries": [{"hash": mh, "kind": "object", "value": scn["objects"][mh]}]})); sys.exit(0)
sys.exit(9)
'''


def build_bundle(export, with_codex=True):
    """Write a ship bundle the way the driver does and return (ship, scenario pieces)."""
    files = {
        "owner": {"bin/ryeos-external-guest-occurrence-owner": (b"owner-exe", 0o755),
                  "controller-root.hex": (b"r" * 64, 0o644),
                  "guest-owner-profile.json": (b"{}", 0o644)},
        "codex": {"codex": (b"codex-exe", 0o755)},
    }
    mh = {"owner": H("c"), "codex": H("4")}
    products, flat, objects = {}, {}, {}
    for name in (["codex"] if with_codex else []) + ["owner"]:
        tree = export / name / "tree"
        entries, mentries = [], []
        dirs = sorted({os.path.dirname(p) for p in files[name] if os.path.dirname(p)})
        for d in dirs:
            (tree / d).mkdir(parents=True, exist_ok=True)
            entries.append({"path": d, "kind": "dir"})
            mentries.append({"path": d, "kind": "dir"})
        total = 0
        for path, (data, mode) in files[name].items():
            full = tree / path
            full.parent.mkdir(parents=True, exist_ok=True)
            full.write_bytes(data)
            full.chmod(mode)
            digest = hashlib.sha256(data).hexdigest()
            entries.append({"path": path, "kind": "file", "mode": mode, "size": len(data),
                            "sha256": digest, "blob_hash": digest})
            mentries.append({"path": path, "kind": "file", "mode": mode, "size": len(data), "blob_hash": digest})
            flat[f"{name}/tree/{path}"] = digest
            total += len(data)
        objects[mh[name]] = {"kind": "external_content_manifest", "entries": mentries}
        products[name] = {"product_name": "runtime", "chain_root_id": "T-" + name, "thread_id": "T-" + name,
                          "witness_hash": H("d") if name == "owner" else H("e"),
                          "witness_source": {"kind": "local_capture"}, "manifest_hash": mh[name],
                          "manifest_kind": "external_content_manifest", "shape": "tree",
                          "storage": "content" if name == "owner" else "large_content",
                          "maximum_bytes": total, "entry_count": len(entries), "total_bytes": total,
                          "tree": f"{name}/tree", "entries": entries}
    ship = {"schema": PS.SHIP_SCHEMA, "origin_node_fingerprint": H("5"), "products": products, "files": flat}
    (export / "ship.json").write_text(json.dumps(ship))
    return ship, objects


class Case(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        bindir = self.tmp / "bin"
        bindir.mkdir()
        shim = bindir / "ryeos"
        shim.write_text(SHIM)
        shim.chmod(shim.stat().st_mode | stat.S_IXUSR)
        self.bindir = str(bindir)
        self.export, self.root_dir = self.tmp / "export", self.tmp / "root"
        self.root_dir.mkdir()
        self.evidence, self.log = self.tmp / "evidence", self.tmp / "calls.log"
        self.rebuild()

    def rebuild(self, **kw):
        shutil.rmtree(self.export, ignore_errors=True)
        self.export.mkdir()
        self.ship, objects = build_bundle(self.export, **kw)
        self.scn = {"root_dir": str(self.root_dir), "objects": objects, "receipts": {
            "owner": {"staging_id": "S-o", "request_digest": H("1"), "manifest_hash": H("c"),
                      "manifest_kind": "external_content_manifest", "entry_count": 4, "total_bytes": 1},
            "codex": {"staging_id": "S-c", "request_digest": H("2"), "manifest_hash": H("4"),
                      "manifest_kind": "external_content_manifest", "entry_count": 1, "total_bytes": 1}}}

    def go(self, scn=None, extra=(), environ=None):
        self.scn.update(scn or {})
        (self.tmp / "scn.json").write_text(json.dumps(self.scn))
        env = {"PATH": self.bindir + os.pathsep + os.environ["PATH"],
               "FAKE_RYEOS_SCENARIO": str(self.tmp / "scn.json"), "FAKE_RYEOS_LOG": str(self.log),
               "MY_API_TOKEN": SECRET}
        env.update(environ or {})
        self.lines = []
        return PS.main(["--export", str(self.export), "--root", "ship-root", "--root-dir", str(self.root_dir),
                        "--evidence-dir", str(self.evidence), *extra], environ=env, emit=self.lines.append)

    def calls(self):
        return [json.loads(l) for l in self.log.read_text().splitlines()] if self.log.exists() else []

    def handoff(self):
        return json.loads((self.export / "ship-handoff.json").read_text())

    def text(self):
        return "\n".join(self.lines)


class Tests(Case):
    def test_happy_path_exact_argv_and_handoff(self):
        self.assertEqual(self.go(), 0)
        argvs = [c["argv"] for c in self.calls()]
        self.assertEqual(argvs, [
            ["external-content", "import", "ship-root", f"f5r-ship/codex-{H('4')[:16]}", "tree", "large_content",
             str(self.ship["products"]["codex"]["maximum_bytes"])],
            ["execute", "service:objects/get", "--no-stream", "--input", json.dumps({"object_hashes": [H("4")]})],
            ["external-content", "import", "ship-root", f"f5r-ship/owner-{H('c')[:16]}", "tree", "content",
             str(self.ship["products"]["owner"]["maximum_bytes"])],
            ["execute", "service:objects/get", "--no-stream", "--input", json.dumps({"object_hashes": [H("c")]})],
        ])
        self.assertTrue(all(c["url"] == "http://127.0.0.1:8000" for c in self.calls()))
        h = self.handoff()
        self.assertEqual({k: h[k] for k in ("owner_manifest_hash", "owner_witness_hash", "codex_witness_hash",
                                            "guest_runtime_manifest_hash", "origin_node_fingerprint")},
                         {"owner_manifest_hash": H("c"), "owner_witness_hash": H("d"),
                          "codex_witness_hash": H("e"), "guest_runtime_manifest_hash": H("c"),
                          "origin_node_fingerprint": H("5")})
        self.assertEqual(h["import_receipt"]["owner"]["staging_id"], "S-o")
        self.assertTrue(h["complete"])
        placed = self.root_dir / f"f5r-ship/owner-{H('c')[:16]}"
        exe = placed / "bin/ryeos-external-guest-occurrence-owner"
        self.assertEqual(exe.read_bytes(), b"owner-exe")
        self.assertTrue(exe.stat().st_mode & 0o100)
        self.assertFalse((placed / "controller-root.hex").stat().st_mode & 0o111)

    def test_tampered_file_is_refused_before_any_call(self):
        (self.export / "owner/tree/guest-owner-profile.json").write_bytes(b"{ }")
        self.assertEqual(self.go(), 1)
        self.assertIn("does not match", self.text())
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.handoff()["complete"])
        self.assertFalse((self.root_dir / "f5r-ship").exists())

    def test_extra_and_missing_files_are_refused(self):
        (self.export / "owner/tree/extra.txt").write_text("x")
        self.assertEqual(self.go(), 1)
        self.assertIn("unexpected file extra.txt", self.text())
        self.rebuild()
        (self.export / "owner/tree/controller-root.hex").unlink()
        self.assertEqual(self.go(), 1)
        self.assertIn("missing file controller-root.hex", self.text())
        self.assertEqual(self.calls(), [])

    def test_ship_json_digest_mismatch(self):
        ship = json.loads((self.export / "ship.json").read_text())
        ship["files"]["owner/tree/controller-root.hex"] = H("0")
        (self.export / "ship.json").write_text(json.dumps(ship))
        self.assertEqual(self.go(), 1)
        self.assertIn("files map", self.text())

    def test_executable_bit_is_checked(self):
        (self.export / "owner/tree/bin/ryeos-external-guest-occurrence-owner").chmod(0o644)
        self.assertEqual(self.go(), 1)
        self.assertIn("executable bit", self.text())

    def test_manifest_hash_mismatch_fails(self):
        self.scn["receipts"]["owner"]["manifest_hash"] = H("7")
        self.assertEqual(self.go(), 1)
        self.assertIn("differs from the origin's", self.text())
        self.assertFalse(self.handoff()["complete"])
        self.assertIsNone(self.handoff()["owner_manifest_hash"])

    def test_read_back_entries_must_match(self):
        self.scn["objects"][H("c")]["entries"][-1]["blob_hash"] = H("8")
        self.assertEqual(self.go(), 1)
        self.assertIn("imported entries differ", self.text())

    def test_idempotent_rerun(self):
        self.go()
        n = len(self.calls())
        self.assertEqual(self.go(), 0)
        self.assertEqual(len(self.calls()), n)
        self.assertEqual(self.handoff()["import_receipt"]["codex"]["staging_id"], "S-c")

    def test_resume_after_failure_skips_finished_product(self):
        self.assertEqual(self.go(scn={"fail_on": f"owner-{H('c')[:16]}"}), 1)
        self.assertEqual(self.go(scn={"fail_on": None}), 0)
        imports = [c["argv"] for c in self.calls() if c["argv"][:2] == ["external-content", "import"]]
        self.assertEqual(len([i for i in imports if "codex-" in i[3]]), 1)
        self.assertEqual(len([i for i in imports if "owner-" in i[3]]), 2)

    def test_owner_only_bundle(self):
        self.rebuild(with_codex=False)
        self.assertEqual(self.go(), 0)
        self.assertIsNone(self.handoff()["codex_witness_hash"])
        self.assertEqual(len(self.calls()), 2)

    def test_secret_is_scrubbed_and_cli_calls_logged(self):
        self.assertEqual(self.go(scn={"fail_on": "import"}), 1)
        everything = self.text() + "".join(p.read_text() for p in self.evidence.iterdir() if p.is_file())
        self.assertNotIn(SECRET, everything)
        self.assertIn("***", everything)
        steps = [json.loads(l) for l in (self.evidence / "steps.jsonl").read_text().splitlines()]
        self.assertEqual(steps[1]["argv"][:3], ["ryeos", "external-content", "import"])

    def test_refusals(self):
        self.assertEqual(PS.main(["--export", str(self.export), "--root", "r", "--root-dir",
                                  str(self.tmp / "nope"), "--evidence-dir", str(self.evidence)], environ={}), 2)
        self.assertEqual(PS.main(["--export", str(self.export), "--root", "r", "--root-dir", str(self.root_dir),
                                  "--evidence-dir", str(self.evidence), "--base-url", "http://c.example"],
                                 environ={}), 2)
        (self.export / "ship.json").unlink()
        self.assertEqual(self.go(), 1)
        self.assertIn("not found", self.text())

    def test_existing_placement_must_match(self):
        dest = self.root_dir / f"f5r-ship/owner-{H('c')[:16]}"
        dest.mkdir(parents=True)
        (dest / "stray").write_text("x")
        self.assertEqual(self.go(), 1)
        self.assertIn("unexpected file stray", self.text())

    def test_base_url_from_env(self):
        self.go(environ={"RYEOSD_URL": "http://127.0.0.1:9"})
        self.assertEqual(self.calls()[0]["url"], "http://127.0.0.1:9")

    def test_driver_export_ships_end_to_end(self):
        """The bundle layout the driver writes is what the ship script verifies."""
        gp = load("gp_test", "test-f5r-guest-products.py")
        case = gp.ExportTests("test_export_bundle_layout_and_digests")
        case.setUp()
        try:
            self.assertEqual(case.export_go(), 0)
            shutil.rmtree(self.export)
            shutil.copytree(case.export, self.export)
            ship = PS.load_ship(self.export)
            objects = {r["manifest_hash"]: {"kind": r["manifest_kind"], "entries": [
                {k: e.get(k) for k in ("path", "kind", "mode", "blob_hash") if k in e or k == "path"}
                for e in r["entries"]]} for r in ship["products"].values()}
            receipts = {n: {"manifest_hash": r["manifest_hash"]} for n, r in ship["products"].items()}
            self.scn["objects"], self.scn["receipts"] = objects, receipts
            self.assertEqual(self.go(), 0)
            self.assertEqual(self.handoff()["origin_node_fingerprint"], H("5"))
            self.assertEqual(self.handoff()["owner_manifest_hash"], gp.H("c"))
        finally:
            shutil.rmtree(case.tmp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
