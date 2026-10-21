#!/usr/bin/env python3
"""Produce the F5R guest products on the controller and write a handoff record.

Runs inside the controller container as the `ryeos` user (RYEOSD_URL
http://127.0.0.1:8000, the controller's own operator key), or against any
local daemon via --base-url and --key-path. Every step drives the `ryeos` CLI
through one run() helper that logs the argv and keeps stdout/stderr as
evidence. A state file records each step as it completes, so a re-run resumes
and never re-creates a thread, capture or activation the file says exists.
"""
import argparse
import base64
import datetime
import hashlib
import json
import os
import re
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ACTIVATION_REF = "config:codex/guest-runtime-activation"
CODEX_GRAPH = "graph:codex/guest-runtime-production"
OWNER_GRAPHS = {
    "local": "graph:codex/guest-owner-runtime-production",
    "for-controller": "graph:codex/guest-owner-runtime-production-for-controller",
}
PRODUCT_NAME = "runtime"
DEFAULT_BASE_URL = "http://127.0.0.1:8000"
KEY_SUFFIX = (".ai", "config", "keys", "signing", "private_key.pem")
TERMINAL = {"completed", "failed", "cancelled", "killed", "timed_out", "continued"}
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
SECRET_ENV_RE = re.compile(r"(TOKEN|SECRET|PASSWORD|API_?KEY|PRIVATE)", re.I)
OWNER_ENTRIES = {
    "bin": ("dir", None),
    "bin/ryeos-external-guest-occurrence-owner": ("file", 0o755),
    "controller-root.hex": ("file", 0o644),
    "guest-owner-profile.json": ("file", 0o644),
}
MAX_OWNER_EXECUTABLE_BYTES = 256 * 1024 * 1024


class Refusal(Exception):
    pass


class StepFailed(Exception):
    pass


def http_open(request, timeout):
    return urllib.request.urlopen(request, timeout=timeout)


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
    """Whole-text JSON, else the last line that parses as a JSON object."""
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


def find_key(value, key):
    """First string stored under `key` anywhere in a nested JSON value."""
    if isinstance(value, dict):
        if isinstance(value.get(key), str):
            return value[key]
        for child in value.values():
            found = find_key(child, key)
            if found:
                return found
    elif isinstance(value, list):
        for child in value:
            found = find_key(child, key)
            if found:
                return found
    return None


def normalize_public_key(raw):
    """Accept the node's `ed25519:<base64>` form or 64 hex chars; return the former."""
    raw = raw.strip()
    if raw.startswith("ed25519:"):
        if len(base64.b64decode(raw[8:], validate=True)) != 32:
            raise Refusal("controller public key must decode to 32 bytes")
        return raw
    if re.fullmatch(r"[0-9a-fA-F]{64}", raw):
        return "ed25519:" + base64.b64encode(bytes.fromhex(raw)).decode()
    raise Refusal("controller public key must be ed25519:<base64> or 64 hex characters")


def key_root_hex(public_key):
    return base64.b64decode(public_key[8:]).hex()


def fetch_public_key(base_url, opener, timeout):
    request = urllib.request.Request(base_url.rstrip("/") + "/public-key", method="GET")
    with opener(request, timeout) as response:
        body = json.loads(response.read())
    signing_key = body.get("signing_key")
    if not isinstance(signing_key, str):
        raise Refusal("GET /public-key returned no signing_key")
    return normalize_public_key(signing_key)


class State:
    """Atomic JSON journal of completed steps; flushed on every mutation."""

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

    def put_meta(self, **fields):
        self.data.update(fields)
        write_atomic(self.path, json.dumps(self.data, indent=2, sort_keys=True) + "\n")


class Driver:
    def __init__(self, args, env, state, evidence, opener, sleep, clock, emit):
        self.args, self.env, self.state = args, env, state
        self.evidence = Path(evidence)
        self.opener, self.sleep, self.clock, self.emit = opener, sleep, clock, emit
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

    def run(self, step, args, timeout=None, **ids):
        """The single CLI entry point: log argv, keep stdout/stderr, return parsed JSON."""
        timeout = timeout or self.args.timeout
        argv = [self.args.ryeos_bin] + args
        self.seq += 1
        stem = self.evidence / f"{self.seq:03d}-{step}"
        started = self.clock()
        try:
            done = subprocess.run(argv, env=self.env, capture_output=True, text=True, timeout=timeout)
            code, out, err = done.returncode, done.stdout, done.stderr
        except Exception as exc:  # timeout or spawn failure is a failed step
            code, out, err = 124, "", f"{type(exc).__name__}: {exc}"
        stem.with_name(stem.name + ".stdout").write_text(self.scrub(out))
        stem.with_name(stem.name + ".stderr").write_text(self.scrub(err))
        record = {"step": step, "argv": argv, "exit": code,
                  "duration_s": round(self.clock() - started, 3), "evidence": stem.name}
        record.update({k: v for k, v in ids.items() if v is not None})
        if code != 0:
            record["stderr_tail"] = err.strip()[-400:]
        self.note(**record)
        if code != 0:
            raise StepFailed(f"{step}: exit {code}")
        return parse_json(out)

    # ---- steps -----------------------------------------------------------

    def activate(self):
        if self.state.get("codex.activate").get("receipt_hash"):
            self.note(step="codex.activate", skipped="state", **self.state.get("codex.activate"))
            return
        deadline = self.clock() + self.args.timeout
        argv = ["external-content", "activate", ACTIVATION_REF, "online"]
        while True:
            out = self.run("codex.activate", argv)
            if not isinstance(out, dict):
                raise StepFailed("codex.activate: output is not a JSON object")
            if out.get("state") == "completed" and out.get("receipt_hash"):
                self.state.set("codex.activate", receipt_hash=out["receipt_hash"],
                               activation_id=out.get("activation_id"))
                return
            if self.clock() >= deadline:
                raise StepFailed(f"codex.activate: not completed within {self.args.timeout}s "
                                 f"(state {out.get('state')})")
            self.sleep(2)

    def graph(self, name, ref, inputs):
        """Launch once (journalled), wait for a terminal thread; return the thread record."""
        key = f"{name}.thread"
        entry = self.state.get(key)
        if entry.get("status") == "launching":
            raise Refusal(f"{key}: an earlier run died while launching {ref}; a thread may exist. "
                          "Check `ryeos thread list`, then set the thread_id in the state file "
                          "or delete the entry")
        if not entry.get("thread_id"):
            self.state.set(key, status="launching", ref=ref)
            argv = ["execute", "--async", ref, "--no-stream"]
            if inputs:
                argv += ["--input", json.dumps(inputs, sort_keys=True)]
            try:
                launched = self.run(f"{name}.execute", argv)
            except StepFailed:
                self.state.set(key, status="launch_failed")
                raise
            thread_id = launched.get("thread_id") if isinstance(launched, dict) else None
            if not thread_id:
                self.state.set(key, status="launch_failed")
                raise StepFailed(f"{name}: execute reported no thread id")
            self.state.set(key, status="launched", thread_id=thread_id)
            entry = self.state.get(key)
        thread_id = entry["thread_id"]
        if entry.get("status") == "completed":
            return entry
        deadline = self.clock() + self.args.timeout
        empty = 0
        while True:
            raw = self.run(f"{name}.wait", ["thread", "get", thread_id], thread_id=thread_id)
            view = raw.get("thread") if isinstance(raw, dict) and isinstance(raw.get("thread"), dict) else raw
            empty = empty + 1 if not isinstance(raw, dict) or not raw else 0
            if empty >= 3:
                raise StepFailed(f"{name}: thread {thread_id} returned 3 empty views")
            status = (view or {}).get("status")
            if status in TERMINAL:
                break
            if self.clock() >= deadline:
                raise StepFailed(f"{name}: thread {thread_id} not terminal within "
                                 f"{self.args.timeout}s (status {status})")
            self.sleep(2)
        if status != "completed":
            self.state.set(key, status=status)
            raise StepFailed(f"{name}: thread {thread_id} ended {status}")
        produced = (raw.get("result") or {}).get("result") if isinstance(raw.get("result"), dict) else None
        self.state.set(key, status="completed", chain_root_id=view.get("chain_root_id") or thread_id,
                       manifest_hash=find_key(produced, "manifest_hash"))
        return self.state.get(key)

    def capture(self, name):
        key = f"{name}.capture"
        done = self.state.get(key)
        if done.get("witness_hash"):
            self.note(step=key, skipped="state", witness_hash=done["witness_hash"])
            return done
        thread = self.state.get(f"{name}.thread")
        captured = self.run(key, ["external-content", "capture-product", thread["chain_root_id"],
                                  thread["thread_id"], PRODUCT_NAME], thread_id=thread["thread_id"])
        witness = captured.get("witness_hash") if isinstance(captured, dict) else None
        state = captured.get("state") if isinstance(captured, dict) else None
        if state != "captured" or not witness:
            raise StepFailed(f"{key}: capture-product state {state!r}, expected captured with a witness")
        self.state.set(key, witness_hash=witness, manifest_hash=find_key(captured, "manifest_hash"))
        return self.state.get(key)

    def read_manifest(self, manifest_hash):
        got = self.run("owner.manifest-read",
                       ["execute", "service:objects/get", "--no-stream",
                        "--input", json.dumps({"object_hashes": [manifest_hash]})])
        for entry in got.get("entries", []) if isinstance(got, dict) else []:
            if entry.get("hash") == manifest_hash and entry.get("kind") == "object":
                return entry.get("value")
        raise StepFailed("owner.manifest-read: manifest object not returned")

    def preflight_owner(self, manifest_hash, public_key):
        if self.state.get("owner.preflight").get("ok") and \
                self.state.get("owner.preflight").get("manifest_hash") == manifest_hash:
            self.note(step="owner.preflight", skipped="state", manifest_hash=manifest_hash)
            return
        problems = preflight_owner_manifest(self.read_manifest(manifest_hash), public_key)
        self.note(step="owner.preflight", manifest_hash=manifest_hash, ok=not problems, problems=problems)
        if problems:
            raise StepFailed("owner.preflight: " + "; ".join(problems))
        self.state.set("owner.preflight", ok=True, manifest_hash=manifest_hash)

    def produce(self, handoff):
        key = fetch_public_key(self.args.base_url, self.opener, self.args.timeout)
        handoff["controller_public_key"] = key
        if not self.args.skip_codex:
            self.activate()
            self.graph("codex", CODEX_GRAPH, None)
            handoff["codex_witness_hash"] = self.capture("codex")["witness_hash"]
        inputs = {"controller_public_key": key} if self.args.owner_production == "for-controller" else None
        thread = self.graph("owner", OWNER_GRAPHS[self.args.owner_production], inputs)
        captured = self.capture("owner")
        handoff["owner_witness_hash"] = captured["witness_hash"]
        produced, evidence = thread.get("manifest_hash"), captured.get("manifest_hash")
        if produced and evidence and produced != evidence:
            raise StepFailed("owner: producer manifest hash differs from the captured evidence")
        manifest_hash = produced or evidence
        if not manifest_hash:
            raise StepFailed("owner: no manifest hash in the producer output or capture evidence")
        self.preflight_owner(manifest_hash, key)
        handoff["owner_manifest_hash"] = manifest_hash
        handoff["guest_runtime_manifest_hash"] = manifest_hash


def preflight_owner_manifest(manifest, controller_public_key=None):
    """Mirror of the engine's four-entry rule; returns a list of violations."""
    problems = []
    if not isinstance(manifest, dict):
        return ["manifest is not an object"]
    if manifest.get("kind") != "external_content_manifest":
        problems.append(f"manifest kind {manifest.get('kind')!r} is not external_content_manifest")
    entries = manifest.get("entries")
    if not isinstance(entries, list):
        return problems + ["manifest has no entries list"]
    if manifest.get("entry_count") != len(entries):
        problems.append(f"entry_count {manifest.get('entry_count')} != {len(entries)} entries")
    if not all(isinstance(entry, dict) for entry in entries):
        return problems + ["manifest entries must be objects"]
    by_path = {}
    for entry in entries:
        path = entry.get("path")
        if path in by_path:
            problems.append(f"duplicate entry {path}")
        by_path[path] = entry
    for path in by_path:
        if path not in OWNER_ENTRIES:
            problems.append(f"unexpected entry {path}")
    if len(entries) != 4:
        problems.append(f"expected exactly 4 entries, found {len(entries)}")
    for path, (kind, mode) in OWNER_ENTRIES.items():
        entry = by_path.get(path)
        if entry is None:
            problems.append(f"missing entry {path}")
            continue
        if entry.get("kind") != kind:
            problems.append(f"{path}: kind {entry.get('kind')} != {kind}")
        if kind == "dir":
            if any(entry.get(k) is not None for k in ("mode", "blob_hash", "size")):
                problems.append(f"{path}: directory carries file attributes")
            continue
        if entry.get("mode") != mode:
            problems.append(f"{path}: mode {entry.get('mode')} != {mode}")
        size, blob = entry.get("size"), entry.get("blob_hash")
        if not isinstance(blob, str) or not HASH_RE.match(blob):
            problems.append(f"{path}: missing content hash")
        if not isinstance(size, int) or size < 1:
            problems.append(f"{path}: missing size")
        elif path == "controller-root.hex" and size != 64:
            problems.append(f"{path}: size {size} != 64")
        elif path == "guest-owner-profile.json" and size > 4096:
            problems.append(f"{path}: size {size} exceeds 4096")
        elif path.startswith("bin/") and size > MAX_OWNER_EXECUTABLE_BYTES:
            problems.append(f"{path}: size {size} too large")
    root = by_path.get("controller-root.hex")
    if root and controller_public_key:
        expected = hashlib.sha256(key_root_hex(controller_public_key).encode()).hexdigest()
        if root.get("blob_hash") != expected:
            problems.append("controller-root.hex does not match the controller public key")
    return problems


def resolve_identity(args, environ):
    """Return (app_root, key_path); the CLI loads <app_root>/.ai/config/keys/signing/private_key.pem."""
    if args.key_path:
        key_path = Path(args.key_path)
        if key_path.parts[-len(KEY_SUFFIX):] != KEY_SUFFIX:
            raise Refusal("--key-path must end in .ai/config/keys/signing/private_key.pem "
                          "(the CLI derives the key from the app root)")
        app_root = Path(*key_path.parts[:-len(KEY_SUFFIX)])
        if args.app_root and Path(args.app_root) != app_root:
            raise Refusal("--app-root and --key-path disagree")
    else:
        root = args.app_root or environ.get("RYEOS_APP_ROOT")
        if not root:
            raise Refusal("pass --app-root or --key-path, or set RYEOS_APP_ROOT")
        app_root = Path(root)
        key_path = app_root.joinpath(*KEY_SUFFIX)
    if not key_path.is_file():
        raise Refusal(f"operator key not found at {key_path}")
    return app_root, key_path


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--base-url", default=None, help=f"daemon URL (default RYEOSD_URL or {DEFAULT_BASE_URL})")
    p.add_argument("--app-root", help="operator app root (default RYEOS_APP_ROOT)")
    p.add_argument("--key-path", help="operator key; must sit at the app root's standard location")
    p.add_argument("--handoff", required=True, help="path of the handoff JSON to write")
    p.add_argument("--state", help="state file (default <evidence-dir>/state.json)")
    p.add_argument("--evidence-dir", required=True)
    p.add_argument("--owner-production", choices=sorted(OWNER_GRAPHS), default="local")
    p.add_argument("--skip-codex", action="store_true")
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--ryeos-bin", default="ryeos")
    return p


def main(argv=None, opener=http_open, sleep=time.sleep, clock=time.monotonic, environ=None, emit=None):
    args = parser().parse_args(argv)
    env = dict(os.environ if environ is None else environ)
    emit = emit or (lambda line: print(line, flush=True))
    try:
        app_root, _ = resolve_identity(args, env)
    except Refusal as exc:
        print(f"refused: {exc}", file=sys.stderr)
        return 2
    args.base_url = (args.base_url or env.get("RYEOSD_URL") or DEFAULT_BASE_URL).rstrip("/")
    if not re.match(r"^https://|^http://(127\.0\.0\.1|localhost|\[::1\])(:\d+)?(/|$)", args.base_url):
        print("refused: non-loopback --base-url must be https", file=sys.stderr)
        return 2
    env["RYEOSD_URL"] = args.base_url
    env["RYEOS_APP_ROOT"] = str(app_root)
    evidence = Path(args.evidence_dir)
    state = State(args.state or evidence / "state.json")
    driver = Driver(args, env, state, evidence, opener, sleep, clock, emit)
    handoff = {"owner_manifest_hash": None, "owner_witness_hash": None, "codex_witness_hash": None,
               "guest_runtime_manifest_hash": None, "controller_public_key": None, "produced_at": None}
    code = 0
    try:
        driver.produce(handoff)
    except (StepFailed, Refusal) as exc:
        driver.note(step="failed", error=str(exc))
        code = 1
    except Exception as exc:
        driver.note(step="failed", error=f"{type(exc).__name__}: {exc}")
        code = 1
    handoff["produced_at"] = utc_now()
    handoff["complete"] = code == 0
    write_atomic(args.handoff, json.dumps(handoff, indent=2, sort_keys=True) + "\n")
    driver.note(step="handoff", path=str(args.handoff), complete=code == 0)
    return code


if __name__ == "__main__":
    sys.exit(main())
