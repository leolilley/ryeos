#!/usr/bin/env python3
"""Produce the F5R guest products (codex runtime, owner runtime) and write a handoff record.

Every step drives the `ryeos` CLI; nothing here reaches around the node. The
CLI subprocess runner and the HTTP opener are injectable seams.
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


def run_subprocess(argv, env, timeout):
    done = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    return done.returncode, done.stdout, done.stderr


def http_open(request, timeout):
    return urllib.request.urlopen(request, timeout=timeout)


def utc_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


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
    """First value stored under `key` anywhere in a nested JSON value."""
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


class Recorder:
    """JSON-line step log; scrubs known secret values from everything it writes."""

    def __init__(self, out_dir, secrets, emit):
        self.path = Path(out_dir) / "guest-products-steps.jsonl"
        self.secrets = [s for s in secrets if s]
        self.emit = emit
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def scrub(self, text):
        for secret in self.secrets:
            text = text.replace(secret, "***")
        return text

    def write(self, record):
        line = self.scrub(json.dumps(record, sort_keys=True))
        with self.path.open("a") as handle:
            handle.write(line + "\n")
        self.emit(line)


class Cli:
    """Runs `ryeos` subcommands; each call is recorded as one step line."""

    def __init__(self, binary, env, runner, recorder, clock):
        self.binary, self.env, self.runner = binary, env, runner
        self.recorder, self.clock = recorder, clock

    def run(self, step, args, timeout, **ids):
        argv = [self.binary] + args
        started = self.clock()
        try:
            code, out, err = self.runner(argv, self.env, timeout)
        except Exception as exc:  # timeout or spawn failure is a failed step
            code, out, err = 124, "", f"{type(exc).__name__}: {exc}"
        record = {"step": step, "command": argv, "exit": code,
                  "duration_s": round(self.clock() - started, 3)}
        record.update({k: v for k, v in ids.items() if v is not None})
        parsed = parse_json(out)
        if code != 0:
            record["stderr_tail"] = err.strip()[-400:]
        self.recorder.write(record)
        if code != 0:
            raise StepFailed(f"{step}: exit {code}")
        return parsed

    def note(self, record):
        self.recorder.write(record)


def thread_view(payload):
    if isinstance(payload, dict) and isinstance(payload.get("thread"), dict):
        return payload["thread"], payload
    return (payload or {}), (payload or {})


def run_graph(cli, step, ref, inputs, timeout, sleep, clock):
    """Launch async, wait for a terminal thread; returns (thread_id, chain_root_id, output)."""
    args = ["execute", "--async", ref, "--no-stream"]
    if inputs:
        args += ["--input", json.dumps(inputs, sort_keys=True)]
    launched = cli.run(f"{step}.execute", args, timeout)
    thread_id = (launched or {}).get("thread_id") if isinstance(launched, dict) else None
    if not thread_id:
        raise StepFailed(f"{step}: execute reported no thread id")
    cli.note({"step": f"{step}.launched", "thread_id": thread_id})
    deadline = clock() + timeout
    empty = 0
    while True:
        raw = cli.run(f"{step}.wait", ["thread", "get", thread_id], timeout, thread_id=thread_id)
        view, whole = thread_view(raw)
        empty = empty + 1 if not isinstance(raw, dict) or not raw else 0
        if empty >= 3:
            raise StepFailed(f"{step}: thread {thread_id} returned 3 empty views")
        status = view.get("status")
        if status in TERMINAL:
            break
        if clock() >= deadline:
            raise StepFailed(f"{step}: thread {thread_id} not terminal within {timeout}s (status {status})")
        sleep(2)
    if status != "completed":
        raise StepFailed(f"{step}: thread {thread_id} ended {status}")
    return thread_id, view.get("chain_root_id") or thread_id, whole


def activate(cli, timeout, sleep, clock):
    """Re-run the idempotent activation command until its receipt is complete."""
    deadline = clock() + timeout
    args = ["external-content", "activate", ACTIVATION_REF, "online"]
    while True:
        out = cli.run("codex.activate", args, timeout)
        if not isinstance(out, dict):
            raise StepFailed("codex.activate: output is not a JSON object")
        if out.get("state") == "completed" and out.get("receipt_hash"):
            cli.note({"step": "codex.activated", "activation_id": out.get("activation_id"),
                      "receipt_hash": out["receipt_hash"], "job_id": out.get("job_id")})
            return {"activation_id": out.get("activation_id"), "receipt_hash": out["receipt_hash"]}
        if clock() >= deadline:
            raise StepFailed(f"codex.activate: not completed within {timeout}s (state {out.get('state')})")
        sleep(2)


def capture(cli, step, chain_root, thread_id, timeout):
    captured = cli.run(f"{step}.capture",
                       ["external-content", "capture-product", chain_root, thread_id, PRODUCT_NAME],
                       timeout, thread_id=thread_id)
    witness = (captured or {}).get("witness_hash") if isinstance(captured, dict) else None
    state = captured.get("state") if isinstance(captured, dict) else None
    if state != "captured" or not witness:
        raise StepFailed(f"{step}: capture-product state {state!r}, expected captured with a witness")
    cli.note({"step": f"{step}.captured", "thread_id": thread_id, "witness_hash": witness})
    return witness, captured


def read_manifest(cli, manifest_hash, timeout):
    got = cli.run("owner.manifest-read",
                  ["execute", "service:objects/get", "--no-stream",
                   "--input", json.dumps({"object_hashes": [manifest_hash]})], timeout)
    for entry in (got or {}).get("entries", []) if isinstance(got, dict) else []:
        if entry.get("hash") == manifest_hash and entry.get("kind") == "object":
            return entry.get("value")
    raise StepFailed("owner.manifest-read: manifest object not returned")


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


def fetch_public_key(url, opener, timeout):
    request = urllib.request.Request(url.rstrip("/") + "/public-key", method="GET")
    with opener(request, timeout) as response:
        body = json.loads(response.read())
    signing_key = body.get("signing_key")
    if not isinstance(signing_key, str):
        raise Refusal("GET /public-key returned no signing_key")
    return normalize_public_key(signing_key)


def write_handoff(out_dir, handoff):
    path = Path(out_dir) / "guest-products-handoff.json"
    path.write_text(json.dumps(handoff, indent=2, sort_keys=True) + "\n")
    return path


def produce(args, cli, handoff, opener, sleep, clock):
    timeout = args.timeout
    key = None
    if args.controller_public_key == "auto":
        if not args.controller_url:
            served = cli.run("controller.public-key",
                             ["execute", "service:identity/public_key", "--no-stream"], timeout)
            signing_key = served.get("signing_key") if isinstance(served, dict) else None
            if not isinstance(signing_key, str):
                raise StepFailed("controller.public-key: no signing_key returned")
            key = normalize_public_key(signing_key)
        else:
            key = fetch_public_key(args.controller_url, opener, timeout)
    elif args.controller_public_key:
        key = normalize_public_key(args.controller_public_key)
    if args.owner_production == "for-controller" and not key:
        raise Refusal("for-controller owner production needs a controller public key")
    handoff["controller_public_key"] = key

    if not args.skip_codex:
        activation = activate(cli, timeout, sleep, clock)
        handoff["activation_id"] = activation["activation_id"]
        handoff["activation_receipt_hash"] = activation["receipt_hash"]
        thread_id, chain_root, _ = run_graph(cli, "codex", CODEX_GRAPH, None, timeout, sleep, clock)
        handoff["thread_ids"]["codex"] = thread_id
        handoff["codex_witness_hash"], _ = capture(cli, "codex", chain_root, thread_id, timeout)

    inputs = {"controller_public_key": key} if args.owner_production == "for-controller" else None
    thread_id, chain_root, whole = run_graph(cli, "owner", OWNER_GRAPHS[args.owner_production], inputs,
                                             timeout, sleep, clock)
    handoff["thread_ids"]["owner"] = thread_id
    produced = (whole.get("result") or {}).get("result") if isinstance(whole, dict) else None
    handoff["owner_manifest_hash"] = find_key(produced, "manifest_hash")
    witness, captured = capture(cli, "owner", chain_root, thread_id, timeout)
    handoff["owner_witness_hash"] = witness
    evidence_manifest = find_key(captured, "manifest_hash")
    if handoff["owner_manifest_hash"] is None:
        handoff["owner_manifest_hash"] = evidence_manifest
    elif evidence_manifest and evidence_manifest != handoff["owner_manifest_hash"]:
        raise StepFailed("owner: producer manifest hash differs from the captured evidence")
    if not handoff["owner_manifest_hash"]:
        raise StepFailed("owner: no manifest hash in the producer output or capture evidence")

    manifest = read_manifest(cli, handoff["owner_manifest_hash"], timeout)
    problems = preflight_owner_manifest(manifest, key)
    cli.note({"step": "owner.preflight", "manifest_hash": handoff["owner_manifest_hash"],
              "ok": not problems, "problems": problems})
    if problems:
        raise StepFailed("owner.preflight: " + "; ".join(problems))


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    where = p.add_mutually_exclusive_group(required=True)
    where.add_argument("--app-root", help="operator app root (sets RYEOS_APP_ROOT)")
    where.add_argument("--controller-url", help="controller https URL (sets RYEOSD_URL)")
    p.add_argument("--out", required=True)
    p.add_argument("--controller-public-key", default=None, help="ed25519:<base64>, 64 hex, or auto")
    p.add_argument("--owner-production", choices=sorted(OWNER_GRAPHS), default="local")
    p.add_argument("--skip-codex", action="store_true")
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument("--ryeos-bin", default="ryeos")
    return p


def main(argv=None, runner=run_subprocess, opener=http_open, sleep=time.sleep,
         clock=time.monotonic, environ=None, emit=None):
    args = parser().parse_args(argv)
    env = dict(os.environ if environ is None else environ)
    emit = emit or (lambda line: print(line, flush=True))
    if args.controller_url:
        if not args.controller_url.startswith("https://"):
            print("refused: --controller-url must be https", file=sys.stderr)
            return 2
        env["RYEOSD_URL"] = args.controller_url
    else:
        env["RYEOS_APP_ROOT"] = args.app_root
    secrets = [v for k, v in env.items() if SECRET_ENV_RE.search(k) and len(v) >= 6]
    out = Path(args.out)
    recorder = Recorder(out, secrets, emit)
    cli = Cli(args.ryeos_bin, env, runner, recorder, clock)
    handoff = {"owner_manifest_hash": None, "owner_witness_hash": None, "codex_witness_hash": None,
               "controller_public_key": None, "activation_id": None,
               "activation_receipt_hash": None, "produced_at": None, "thread_ids": {}}
    code = 0
    try:
        produce(args, cli, handoff, opener, sleep, clock)
    except (StepFailed, Refusal) as exc:
        recorder.write({"step": "failed", "error": str(exc)})
        code = 1
    except Exception as exc:
        recorder.write({"step": "failed", "error": f"{type(exc).__name__}: {exc}"})
        code = 1
    handoff["produced_at"] = utc_now()
    handoff["complete"] = code == 0
    path = write_handoff(out, handoff)
    recorder.write({"step": "handoff", "path": str(path), "complete": code == 0})
    return code


if __name__ == "__main__":
    sys.exit(main())
