#!/usr/bin/env python3
"""Import an exported F5R guest-product bundle into this controller.

Runs ON the controller as the `ryeos` user (RYEOSD_URL, default
http://127.0.0.1:8000). Takes the bundle written by f5r-guest-products.py
--export, verifies every sha256 against ship.json, places each product tree
under an admitted external-content import root, and stages it through the
substrate's own `ryeos external-content import` filesystem source. The import
captures the tree into the controller's CAS; the resulting manifest hash must
equal the origin's manifest hash. Every CLI call is logged with its argv and
scrubbed output. A state file makes a re-run skip finished products.
"""
import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

SHIP_SCHEMA = "ryeos.f5r_guest_product_ship.v1"
DEFAULT_BASE_URL = "http://127.0.0.1:8000"
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
SECRET_ENV_RE = re.compile(r"(TOKEN|SECRET|PASSWORD|API_?KEY|PRIVATE)", re.I)
PRODUCTS = ("codex", "owner")
STORAGES = ("content", "large_content")
PLACEMENT_DIR = "f5r-ship"


class Refusal(Exception):
    pass


class StepFailed(Exception):
    pass


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def write_atomic(path, text):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    with open(tmp, "w") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)


def parse_json(text):
    text = text.strip()
    if not text:
        return None
    try:
        return json.loads(text)
    except ValueError:
        pass
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def safe_relative(path):
    parts = path.split("/") if isinstance(path, str) else []
    if not parts or any(p in ("", ".", "..") for p in parts) or path.startswith("/"):
        raise Refusal(f"path {path!r} is not a canonical relative path")
    return path


def tree_files(tree):
    """Every file and directory below `tree` as relative posix paths; symlinks are refused."""
    files, dirs = {}, set()
    for current, names, leaves in os.walk(tree, followlinks=False):
        for name in names + leaves:
            full = Path(current) / name
            rel = full.relative_to(tree).as_posix()
            if full.is_symlink():
                raise Refusal(f"{full} is a symlink")
            if full.is_dir():
                dirs.add(rel)
            elif full.is_file():
                files[rel] = full
            else:
                raise Refusal(f"{full} is not a regular file or directory")
    return files, dirs


def verify_tree(tree, record):
    """The tree on disk must be exactly the record's entries with matching size and sha256."""
    tree = Path(tree)
    if not tree.is_dir():
        raise Refusal(f"{tree} is missing")
    files, dirs = tree_files(tree)
    want_files = {e["path"]: e for e in record["entries"] if e["kind"] == "file"}
    want_dirs = {e["path"] for e in record["entries"] if e["kind"] == "dir"}
    for rel in sorted(set(files) - set(want_files)):
        raise Refusal(f"unexpected file {rel} in {tree}")
    for rel in sorted(set(want_files) - set(files)):
        raise Refusal(f"missing file {rel} in {tree}")
    implied = set(want_dirs)
    for rel in list(want_files) + list(want_dirs):
        parent = os.path.dirname(rel)
        while parent:
            implied.add(parent)
            parent = os.path.dirname(parent)
    if dirs != implied:
        raise Refusal(f"directory set of {tree} differs from the manifest")
    for rel, entry in want_files.items():
        if files[rel].stat().st_size != entry["size"] or file_sha256(files[rel]) != entry["sha256"]:
            raise Refusal(f"{rel} does not match its recorded size and sha256")
        executable = bool(files[rel].stat().st_mode & 0o111)
        if executable != (entry.get("mode") == 0o755):
            raise Refusal(f"{rel} executable bit does not match recorded mode {entry.get('mode')}")


def load_ship(export):
    """Read ship.json and verify every payload file against it; return the parsed document."""
    export = Path(export)
    path = export / "ship.json"
    if not path.is_file():
        raise Refusal(f"{path} not found")
    ship = json.loads(path.read_text())
    if ship.get("schema") != SHIP_SCHEMA:
        raise Refusal(f"ship.json schema {ship.get('schema')!r} is not {SHIP_SCHEMA}")
    if not isinstance(ship.get("origin_node_fingerprint"), str) or not ship["origin_node_fingerprint"]:
        raise Refusal("ship.json carries no origin node fingerprint")
    products = ship.get("products")
    if not isinstance(products, dict) or "owner" not in products or set(products) - set(PRODUCTS):
        raise Refusal("ship.json must carry an owner product and may carry a codex product")
    files = ship.get("files") or {}
    expected = {}
    for name, record in products.items():
        if record.get("storage") not in STORAGES or record.get("shape") != "tree":
            raise Refusal(f"{name}: unsupported shape/storage")
        if not HASH_RE.match(record.get("manifest_hash", "")) or not HASH_RE.match(record.get("witness_hash", "")):
            raise Refusal(f"{name}: manifest_hash/witness_hash are not sha256 digests")
        if not isinstance(record.get("maximum_bytes"), int) or record["maximum_bytes"] < 1:
            raise Refusal(f"{name}: maximum_bytes must be a positive integer")
        if record.get("tree") != f"{name}/tree":
            raise Refusal(f"{name}: tree must be {name}/tree")
        for entry in record["entries"]:
            safe_relative(entry["path"])
            if entry["kind"] == "file":
                expected[f"{record['tree']}/{entry['path']}"] = entry["sha256"]
        verify_tree(export / record["tree"], record)
    if expected != files:
        raise Refusal("ship.json files map does not match the product entries")
    for rel, digest in files.items():
        if file_sha256(export / rel) != digest:
            raise Refusal(f"{rel} does not match its sha256 in ship.json")
    return ship


class State:
    def __init__(self, path):
        self.path = Path(path)
        self.data = {"steps": {}}
        if self.path.exists():
            self.data = json.loads(self.path.read_text())
            self.data.setdefault("steps", {})

    def get(self, step):
        return self.data["steps"].get(step, {})

    def set(self, step, **fields):
        self.data["steps"].setdefault(step, {}).update(fields)
        write_atomic(self.path, json.dumps(self.data, indent=2, sort_keys=True) + "\n")


class Shipper:
    def __init__(self, args, env, state, evidence, emit, clock):
        self.args, self.env, self.state = args, env, state
        self.evidence = Path(evidence)
        self.emit, self.clock = emit, clock
        self.secrets = [v for k, v in env.items() if SECRET_ENV_RE.search(k) and len(v) >= 6]
        self.evidence.mkdir(parents=True, exist_ok=True)
        self.log_path = self.evidence / "steps.jsonl"
        self.seq = len(list(self.evidence.glob("*.stdout")))

    def scrub(self, text):
        for secret in self.secrets:
            text = text.replace(secret, "***")
        return text

    def note(self, **record):
        line = self.scrub(json.dumps(record, sort_keys=True))
        with self.log_path.open("a") as handle:
            handle.write(line + "\n")
        self.emit(line)

    def run(self, step, args, **ids):
        argv = [self.args.ryeos_bin] + args
        self.seq += 1
        stem = self.evidence / f"{self.seq:03d}-{step}"
        started = self.clock()
        try:
            done = subprocess.run(argv, env=self.env, capture_output=True, text=True,
                                  timeout=self.args.timeout)
            code, out, err = done.returncode, done.stdout, done.stderr
        except Exception as exc:
            code, out, err = 124, "", f"{type(exc).__name__}: {exc}"
        stem.with_name(stem.name + ".stdout").write_text(self.scrub(out))
        stem.with_name(stem.name + ".stderr").write_text(self.scrub(err))
        record = {"step": step, "argv": argv, "exit": code,
                  "duration_s": round(self.clock() - started, 3), "evidence": stem.name}
        record.update(ids)
        if code != 0:
            record["stderr_tail"] = self.scrub(err.strip()[-400:])
        self.note(**record)
        if code != 0:
            raise StepFailed(f"{step}: exit {code}")
        return parse_json(out)

    def place(self, name, record):
        """Copy the verified tree under the admitted root; return its path relative to that root."""
        rel = f"{PLACEMENT_DIR}/{name}-{record['manifest_hash'][:16]}"
        dest = Path(self.args.root_dir) / rel
        if dest.exists():
            verify_tree(dest, record)  # an identical earlier placement is reused
            return rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_name(dest.name + ".placing")
        if tmp.exists():
            shutil.rmtree(tmp)
        shutil.copytree(Path(self.args.export) / record["tree"], tmp, symlinks=False)
        for entry in record["entries"]:
            if entry["kind"] == "file":
                (tmp / entry["path"]).chmod(0o755 if entry.get("mode") == 0o755 else 0o644)
        verify_tree(tmp, record)
        os.replace(tmp, dest)
        return rel

    def ship_product(self, name, record):
        key = f"{name}.import"
        done = self.state.get(key)
        if done.get("receipt", {}).get("manifest_hash") == record["manifest_hash"]:
            self.note(step=key, skipped="state", manifest_hash=record["manifest_hash"])
            return done["receipt"]
        rel = self.place(name, record)
        argv = ["external-content", "import", self.args.root, rel, record["shape"],
                record["storage"], str(record["maximum_bytes"])]
        receipt = self.run(key, argv, manifest_hash=record["manifest_hash"])
        if not isinstance(receipt, dict) or receipt.get("manifest_hash") != record["manifest_hash"]:
            got = receipt.get("manifest_hash") if isinstance(receipt, dict) else None
            raise StepFailed(f"{key}: imported manifest {got} differs from the origin's "
                             f"{record['manifest_hash']}")
        self.read_back(name, record)
        self.state.set(key, receipt=receipt, placed=rel)
        return receipt

    def read_back(self, name, record):
        """The imported manifest object must exist on this node and list the origin's entries."""
        got = self.run(f"{name}.read-back", ["execute", "service:objects/get", "--no-stream", "--input",
                                             json.dumps({"object_hashes": [record["manifest_hash"]]})])
        manifest = None
        for entry in got.get("entries", []) if isinstance(got, dict) else []:
            if entry.get("hash") == record["manifest_hash"] and entry.get("kind") == "object":
                manifest = entry.get("value")
        if not isinstance(manifest, dict):
            raise StepFailed(f"{name}.read-back: imported manifest object not found")
        have = {e.get("path"): (e.get("kind"), e.get("mode"), e.get("blob_hash"))
                for e in manifest.get("entries", [])}
        want = {e["path"]: (e["kind"], e.get("mode"), e.get("blob_hash")) for e in record["entries"]}
        if have != want:
            raise StepFailed(f"{name}.read-back: imported entries differ from the origin manifest")

    def ship(self, ship, handoff):
        for name in PRODUCTS:
            if name in ship["products"]:
                handoff["import_receipt"][name] = self.ship_product(name, ship["products"][name])
        owner = ship["products"]["owner"]
        handoff["owner_manifest_hash"] = handoff["guest_runtime_manifest_hash"] = owner["manifest_hash"]
        handoff["owner_witness_hash"] = owner["witness_hash"]
        if "codex" in ship["products"]:
            handoff["codex_witness_hash"] = ship["products"]["codex"]["witness_hash"]


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--export", required=True, help="export bundle directory (already copied in)")
    p.add_argument("--root", required=True, help="admitted external-content import root NAME")
    p.add_argument("--root-dir", required=True, help="existing filesystem directory of that root")
    p.add_argument("--handoff", help="ship-handoff.json path (default <export>/ship-handoff.json)")
    p.add_argument("--state", help="state file (default <evidence-dir>/state.json)")
    p.add_argument("--evidence-dir", required=True)
    p.add_argument("--base-url", default=None, help=f"daemon URL (default RYEOSD_URL or {DEFAULT_BASE_URL})")
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--ryeos-bin", default="ryeos")
    return p


def main(argv=None, clock=time.monotonic, environ=None, emit=None):
    args = parser().parse_args(argv)
    env = dict(os.environ if environ is None else environ)
    emit = emit or (lambda line: print(line, flush=True))
    base = (args.base_url or env.get("RYEOSD_URL") or DEFAULT_BASE_URL).rstrip("/")
    if not re.match(r"^https://|^http://(127\.0\.0\.1|localhost|\[::1\])(:\d+)?(/|$)", base):
        print("refused: non-loopback --base-url must be https", file=sys.stderr)
        return 2
    env["RYEOSD_URL"] = base
    if not Path(args.root_dir).is_dir():
        print(f"refused: --root-dir {args.root_dir} does not exist", file=sys.stderr)
        return 2
    handoff_path = Path(args.handoff) if args.handoff else Path(args.export) / "ship-handoff.json"
    evidence = Path(args.evidence_dir)
    shipper = Shipper(args, env, State(args.state or evidence / "state.json"), evidence, emit, clock)
    handoff = {"owner_manifest_hash": None, "owner_witness_hash": None, "codex_witness_hash": None,
               "guest_runtime_manifest_hash": None, "origin_node_fingerprint": None,
               "import_receipt": {}, "shipped_at": None}
    code = 0
    try:
        ship = load_ship(args.export)
        handoff["origin_node_fingerprint"] = ship["origin_node_fingerprint"]
        shipper.note(step="verify", ok=True, files=len(ship["files"]))
        shipper.ship(ship, handoff)
    except (StepFailed, Refusal, KeyError, ValueError) as exc:
        shipper.note(step="failed", error=f"{type(exc).__name__}: {exc}")
        code = 1
    except Exception as exc:
        shipper.note(step="failed", error=f"{type(exc).__name__}: {exc}")
        code = 1
    handoff["shipped_at"] = utc_now()
    handoff["complete"] = code == 0
    write_atomic(handoff_path, json.dumps(handoff, indent=2, sort_keys=True) + "\n")
    shipper.note(step="handoff", path=str(handoff_path), complete=code == 0)
    return code


if __name__ == "__main__":
    sys.exit(main())
