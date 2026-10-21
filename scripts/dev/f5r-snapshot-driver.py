#!/usr/bin/env python3
"""Operator driver for the F5R runtime snapshot production sequence.

Creates a deny-all source sandbox on Render, drives snapshot production,
readiness, restored qualification and teardown through the `ryeos` CLI, and
writes a handoff file. Every provider-contacting step is attempted at most
once: progress is journaled atomically in the state file before the call, and
a rerun that finds an attempted-but-unresolved step refuses with the verb to
run instead of repeating it.
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

API = "https://api.render.com/v1"
FORBIDDEN_SERVICE = "srv-datiea7avr4c73dm14fg"
HEX64 = set("0123456789abcdef")
RESERVED_MESSAGE = ("snapshot operation is reserved without provider contact; clear the produce "
                    "step in the state file and rerun")
HINT_SNAPSHOT = ("Read the phase with `ryeos external-content runtime-snapshot {op}`; quarantined "
                 "is terminal.")


class Refusal(Exception):
    pass


class Failure(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuses every 3xx so the bearer header never follows a redirect."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def http_open(request, timeout):
    return urllib.request.build_opener(NoRedirect).open(request, timeout=timeout)


def run_cli(argv, env):
    out = subprocess.run(argv, capture_output=True, text=True, env=env)
    return out.returncode, out.stdout, out.stderr


def clean_secret(text):
    text = text.strip()
    if not text or any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in text):
        raise Failure("secret is empty or contains whitespace or control characters")
    return text


DEFAULT_ENV_FILE = Path(__file__).resolve().parents[2] / ".env"


def parse_env_file(text):
    values = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[name.strip()] = value
    return values


def read_key(args, environ=None):
    environ = os.environ if environ is None else environ
    if args.render_key_file:
        return clean_secret(Path(args.render_key_file).read_text())
    if environ.get("RENDER_API_KEY"):
        return clean_secret(environ["RENDER_API_KEY"])
    path = Path(args.env_file) if args.env_file else DEFAULT_ENV_FILE
    if path.is_file():
        if path.stat().st_mode & 0o077:
            raise Refusal(f"env file {path} is group or world readable; run chmod 600 on it")
        value = parse_env_file(path.read_text()).get("RENDER_API_KEY")
        if value:
            return clean_secret(value)
    raise Refusal("no Render API key: pass --render-key-file, set RENDER_API_KEY, or put "
                  f"RENDER_API_KEY in {path}")


def write_atomic(path, text):
    tmp = path.with_name(path.name + ".tmp")
    with open(tmp, "w") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


class State:
    """Atomic JSON journal; every mutation is flushed before the next call."""

    def __init__(self, path):
        self.path = Path(path)
        self.data = {"steps": {}}
        if self.path.exists():
            self.data = json.loads(self.path.read_text())
            self.data.setdefault("steps", {})

    def save(self):
        write_atomic(self.path, json.dumps(self.data, indent=2, sort_keys=True))

    def status(self, step):
        return self.data["steps"].get(step, {}).get("status")

    def mark(self, step, status, **fields):
        entry = self.data["steps"].setdefault(step, {})
        entry["status"] = status
        entry.update(fields)
        self.save()

    def get(self, step, key):
        return self.data["steps"].get(step, {}).get(key)


class Driver:
    def __init__(self, args, key, state, cli=run_cli, opener=http_open, sleep=time.sleep,
                 clock=time.monotonic, log=None):
        self.args = args
        self.key = key
        self.state = state
        self.cli = cli
        self.opener = opener
        self.sleep = sleep
        self.clock = clock
        self._log = log or (lambda line: print(line, file=sys.stderr))
        self.env = dict(os.environ)
        if args.app_root:
            self.env["RYEOS_APP_ROOT"] = args.app_root
        if args.controller_url:
            self.env["RYEOSD_URL"] = args.controller_url

    def log(self, **fields):
        self._log(self.scrub(json.dumps(fields, sort_keys=True)))

    def scrub(self, text):
        return text.replace(self.key, "***") if self.key else text

    # ---- CLI -------------------------------------------------------------
    def ryeos(self, verb, payload=None, positional=None):
        argv = [self.args.ryeos_bin, "external-content", verb]
        if positional is not None:
            argv.append(positional)
        if payload is not None:
            argv.append(json.dumps(payload, separators=(",", ":")))
        code, out, err = self.cli(argv, self.env)
        body = None
        try:
            body = json.loads(out) if out.strip() else None
        except ValueError:
            body = None
        record = unwrap(body)
        self.log(verb=verb, exit=code, ids=ids_of(record))
        return code, record, self.scrub(err.strip()[:300])

    # ---- Render ----------------------------------------------------------
    def render(self, method, path, body=None):
        if FORBIDDEN_SERVICE in path or FORBIDDEN_SERVICE in json.dumps(body):
            raise Refusal(f"refusing to touch protected Render service {FORBIDDEN_SERVICE}")
        data = None if body is None else json.dumps(body).encode()
        headers = {"Accept": "application/json", "Authorization": "Bearer " + self.key}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(API + path, data=data, method=method, headers=headers)
        try:
            with self.opener(request, 60) as resp:
                status, raw = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, raw = err.code, err.read()
        except (urllib.error.URLError, OSError) as err:
            self.log(method=method, path=path.split("?")[0], status="error", error=type(err).__name__)
            raise Failure(f"{method} {path.split('?')[0]}: {type(err).__name__}")
        try:
            parsed = json.loads(raw) if raw else None
        except ValueError:
            parsed = None
        self.log(method=method, path=path.split("?")[0], status=status,
                 ids={"id": parsed.get("id")} if isinstance(parsed, dict) else {})
        if status >= 400:
            raise Failure(f"{method} {path.split('?')[0]}: HTTP {status}")
        return status, parsed

    def terminate_source(self):
        sandbox = self.state.get("source_sandbox", "id")
        if not sandbox or self.state.status("source_terminate") == "done":
            return
        query = urllib.parse.urlencode({"ownerId": self.args.owner_id})
        self.state.mark("source_terminate", "attempted")
        try:
            _, parsed = self.render("POST", f"/sandboxes/{urllib.parse.quote(sandbox)}/terminate?{query}")
            if isinstance(parsed, dict) and parsed.get("id") not in (None, sandbox):
                raise Failure("terminate response names a different sandbox than the stored id")
        except Failure as err:
            self.log(step="source_terminate", error=str(err))
            raise
        self.state.mark("source_terminate", "done")

    # ---- steps -----------------------------------------------------------
    def refuse_unresolved(self, step, hint):
        raise Refusal(f"step {step} was attempted but never resolved; it will not be repeated. {hint}")

    def step_source(self):
        if self.state.status("source_sandbox") == "done":
            return
        if self.state.status("source_sandbox") == "attempted":
            self.refuse_unresolved(
                "source_sandbox",
                "The create request may have been sent without its id being recorded, so an orphan "
                "sandbox may exist; it terminates itself at its timeout. Find it in the Render "
                "dashboard (deny-all, running) and rerun with --source-sandbox-id <id>, or delete "
                "the state file to start over.")
        body = {
            "ownerId": self.args.owner_id,
            "plan": self.args.plan,
            "region": self.args.region,
            "timeoutSeconds": self.args.source_timeout_seconds,
            "networkPolicy": {"type": "deny-all"},
        }
        self.state.mark("source_sandbox", "attempted")
        _, parsed = self.render("POST", "/sandboxes", body)
        sandbox = parsed.get("id") if isinstance(parsed, dict) else None
        if not sandbox:
            raise Failure("source sandbox create response has no id")
        self.state.mark("source_sandbox", "done", id=sandbox)

    def step_source_ready(self):
        """The adapter checks the source after claiming its one attempt, so a
        source that is not yet running must never reach produce."""
        if self.state.status("source_ready") == "done":
            return
        sandbox = self.state.get("source_sandbox", "id")
        query = urllib.parse.urlencode({"ownerId": self.args.owner_id})
        deadline = self.clock() + self.args.poll_timeout
        while True:
            _, body = self.render("GET", f"/sandboxes/{urllib.parse.quote(sandbox)}?{query}")
            body = body if isinstance(body, dict) else {}
            policy = body.get("networkPolicy")
            policy_type = policy.get("type") if isinstance(policy, dict) else None
            if (body.get("id") not in (None, sandbox) or body.get("terminatedAt")
                    or policy_type != "deny-all" or body.get("region") != self.args.region
                    or body.get("plan") != self.args.plan):
                raise Failure(f"source sandbox {sandbox} is not the exact deny-all "
                              f"{self.args.plan}/{self.args.region} sandbox")
            status = body.get("status")
            if status == "running":
                self.state.mark("source_ready", "done")
                return
            if status not in ("creating", "starting", "pending"):
                raise Failure(f"source sandbox {sandbox} has status {status!r}")
            if self.clock() >= deadline:
                raise Failure(f"source sandbox {sandbox} not running within {self.args.poll_timeout}s")
            self.sleep(self.args.poll_interval)

    def step_produce(self):
        if self.state.status("produce") == "done":
            return
        if self.state.status("produce") == "reserved":
            raise Refusal(RESERVED_MESSAGE)
        if self.state.status("produce") == "attempted":
            self.refuse_unresolved(
                "produce",
                "Read the phase with `ryeos external-content runtime-snapshot <operation-id>`; the id "
                "comes from the daemon record or the logs. quarantined is terminal. If the phase is "
                "bound, rerun with --snapshot-operation-id <id>.")
        payload = {
            "binding_id": self.args.binding_id,
            "source": {
                "kind": "captured_product",
                "witness_hash": self.handoff["owner_witness_hash"],
                "witness_source": {"kind": "local_capture"},
                "source_occurrence_id": self.state.get("source_sandbox", "id"),
            },
        }
        self.state.mark("produce", "attempted")
        code, record, err = self.ryeos("produce-runtime-snapshot", payload)
        if code != 0:
            raise Failure(f"produce-runtime-snapshot failed: {err}")
        op = operation_id(record)
        if not op:
            raise Failure("produce-runtime-snapshot returned no operation id")
        phase = (record or {}).get("phase")
        if phase == "reserved":
            self.state.mark("produce", "reserved", operation_id=op, phase=phase)
            raise Refusal(RESERVED_MESSAGE)
        self.state.mark("produce", "done", operation_id=op, phase=phase)
        if phase in ("quarantined", "late_observed"):
            raise Failure(f"snapshot operation {op} is {phase}; production failed")
        if phase == "attempt_pending":
            raise Refusal(f"snapshot operation {op} is attempt_pending; the create is unresolved and "
                          "will not be repeated. " + HINT_SNAPSHOT.format(op=op))
        if phase != "bound":
            raise Failure(f"produce-runtime-snapshot returned unexpected phase {phase!r}")

    def step_readiness(self):
        if self.state.status("readiness") == "done":
            return
        op = self.state.get("produce", "operation_id")
        deadline = self.clock() + self.args.poll_timeout
        while True:
            code, record, err = self.ryeos("observe-runtime-snapshot-readiness", positional=op)
            if code == 0 and isinstance(record, dict) and record.get("readiness"):
                locator = record.get("locator") or {}
                self.state.mark("readiness", "done", snapshot_id=locator.get("snapshot_id"))
                return
            _, current, _ = self.ryeos("runtime-snapshot", positional=op)
            phase = (current or {}).get("phase")
            if phase in ("quarantined", "late_observed"):
                raise Failure(f"snapshot operation {op} is {phase}; readiness failed")
            if phase == "attempt_pending":
                raise Refusal(
                    f"snapshot operation {op} is attempt_pending; the create is unresolved and "
                    "will not be repeated. " + HINT_SNAPSHOT.format(op=op))
            if phase == "reserved":
                raise Refusal(RESERVED_MESSAGE)
            if self.clock() >= deadline:
                raise Failure(f"snapshot {op} not available within {self.args.poll_timeout}s")
            self.sleep(self.args.poll_interval)

    def step_qualification(self):
        if self.state.status("qualification") != "done":
            if self.state.status("qualification") == "attempted":
                self.refuse_unresolved(
                    "qualification",
                    "Read the phase with `ryeos external-content runtime-snapshot-qualification "
                    "<operation-id>`; the id comes from the daemon record or the logs. quarantined is "
                    "terminal. If the phase is occurrence_bound, rerun with --qualification-operation-id <id>.")
            payload = {
                "qualification_binding_id": self.args.qualification_binding_id,
                "snapshot_operation_id": self.state.get("produce", "operation_id"),
            }
            self.state.mark("qualification", "attempted")
            code, record, err = self.ryeos("create-runtime-snapshot-qualification", payload)
            if code != 0:
                raise Failure(f"create-runtime-snapshot-qualification failed: {err}")
            op = operation_id(record)
            if not op:
                raise Failure("qualification create returned no operation id")
            self.state.mark("qualification", "done", operation_id=op, phase=(record or {}).get("phase"))
        if self.state.status("verify") == "done":
            return
        if self.state.status("verify") == "attempted":
            self.refuse_unresolved(
                "verify",
                "Read `ryeos external-content runtime-snapshot-qualification <qualification-operation-id>` "
                "and, once the verifier has settled, rerun with --verify-resolved.")
        qop = self.state.get("qualification", "operation_id")
        self.state.mark("verify", "attempted")
        code, record, err = self.ryeos("verify-runtime-snapshot-qualification", positional=qop)
        if code != 0:
            raise Failure(f"verify-runtime-snapshot-qualification failed: {err}")
        phase = (record or {}).get("phase")
        if phase == "observed":
            self.state.mark("verify", "done", phase=phase)
            return
        if phase == "quarantined":
            raise Failure(f"verifier attempt for {qop} is quarantined")
        deadline = self.clock() + self.args.poll_timeout
        while True:
            self.sleep(self.args.poll_interval)
            _, current, _ = self.ryeos("runtime-snapshot-qualification", positional=qop)
            current_phase = (current or {}).get("phase")
            if current_phase == "quarantined":
                raise Failure(f"qualification {qop} is quarantined")
            if (current or {}).get("observation") or current_phase == "observed":
                self.state.mark("verify", "done", phase=current_phase)
                return
            if self.clock() >= deadline:
                raise Failure(f"verifier for {qop} not observed within {self.args.poll_timeout}s")

    def terminate_qualification(self):
        qop = self.state.get("qualification", "operation_id")
        if not qop or self.state.status("qualification_terminate") == "done":
            return
        # Termination replay only observes, so it is safe to repeat.
        self.state.mark("qualification_terminate", "attempted")
        code, _, err = self.ryeos("terminate-runtime-snapshot-qualification", positional=qop)
        if code != 0:
            raise Failure(f"terminate-runtime-snapshot-qualification failed: {err}")
        self.state.mark("qualification_terminate", "done")

    def qualify_product(self):
        """The attestation hash is the `qualification_hash` that qualify-product
        publishes for the owner product; neither the snapshot nor the restored
        verifier record carries one. Publication is idempotent, so a rerun may
        repeat it."""
        a = self.args
        coords = (a.qualify_relationship, a.qualify_verifier_chain_root_id, a.qualify_verifier_thread_id)
        if not any(coords):
            return None
        if not all(coords):
            raise Refusal("qualify-product needs --qualify-relationship, "
                          "--qualify-verifier-chain-root-id and --qualify-verifier-thread-id together")
        done = self.state.get("qualify_product", "attestation_hash")
        if self.state.status("qualify_product") == "done" and is_hash(done):
            return done
        self.state.mark("qualify_product", "attempted")
        argv = [a.ryeos_bin, "external-content", "qualify-product", self.handoff["owner_witness_hash"],
                json.dumps({"kind": "local_capture"}), a.qualify_relationship,
                a.qualify_verifier_chain_root_id, a.qualify_verifier_thread_id]
        code, out, err = self.cli(argv, self.env)
        try:
            record = unwrap(json.loads(out)) if out.strip() else None
        except ValueError:
            record = None
        self.log(verb="qualify-product", exit=code)
        hash_ = record.get("qualification_hash") if isinstance(record, dict) else None
        if code != 0 or not is_hash(hash_):
            raise Failure(f"qualify-product failed: {self.scrub(err.strip()[:300])}")
        self.state.mark("qualify_product", "done", attestation_hash=hash_)
        return hash_

    def step_finish(self):
        self.terminate_qualification()
        self.terminate_source()
        sop = self.state.get("produce", "operation_id")
        qop = self.state.get("qualification", "operation_id")
        _, snap, _ = self.ryeos("runtime-snapshot", positional=sop)
        locator = (snap or {}).get("locator") or {}
        handoff = {
            "snapshot_operation_id": sop,
            "snapshot_id": locator.get("snapshot_id") or self.state.get("readiness", "snapshot_id"),
            "snapshot_locator": locator or None,
            "qualification_operation_id": qop,
            "owner_manifest_hash": self.handoff["owner_manifest_hash"],
        }
        attestation = self.qualify_product()
        if attestation:
            handoff["attestation_hash"] = attestation
        out = Path(self.args.out)
        out.mkdir(parents=True, exist_ok=True)
        write_atomic(out / "snapshot-handoff.json", json.dumps(handoff, indent=2, sort_keys=True))
        self.state.mark("handoff", "done")
        return handoff

    def run(self):
        self.handoff = json.loads(Path(self.args.handoff).read_text())
        for name in ("owner_witness_hash", "owner_manifest_hash"):
            if not is_hash(self.handoff.get(name)):
                raise Refusal(f"handoff {name} is not a 64-hex digest")
        self.adopt()
        try:
            self.step_source()
            self.step_source_ready()
            self.step_produce()
            self.step_readiness()
            self.step_qualification()
            return self.step_finish()
        except Refusal:
            self.log(note="refused with an unresolved attempt: the source sandbox and any "
                          "qualification sandbox stay up until their provider timeouts")
            raise
        except Failure:
            self.teardown_after_failure()
            raise

    def adopt(self):
        a = self.args
        if a.source_sandbox_id and self.state.status("source_sandbox") != "done":
            self.state.mark("source_sandbox", "done", id=a.source_sandbox_id)
        if a.snapshot_operation_id and self.state.status("produce") != "done":
            self.state.mark("produce", "done", operation_id=a.snapshot_operation_id)
        if a.qualification_operation_id and self.state.status("qualification") != "done":
            self.state.mark("qualification", "done", operation_id=a.qualification_operation_id)
        if a.verify_resolved and self.state.status("verify") == "attempted":
            self.state.mark("verify", "done")

    def teardown_after_failure(self):
        for fn in (self.terminate_qualification, self.terminate_source):
            try:
                fn()
            except (Failure, Refusal) as err:
                self.log(teardown_error=str(err))


def is_hash(value):
    return isinstance(value, str) and len(value) == 64 and set(value) <= HEX64


def unwrap(body):
    if isinstance(body, dict):
        for key in ("result", "data"):
            if isinstance(body.get(key), dict):
                return body[key]
    return body


def operation_id(record):
    if isinstance(record, dict):
        intent = record.get("intent")
        if isinstance(intent, dict) and intent.get("operation_id"):
            return intent["operation_id"]
        return record.get("operation_id")
    return None


def ids_of(record):
    found = {}
    if isinstance(record, dict):
        if operation_id(record):
            found["operation_id"] = operation_id(record)
        if record.get("phase"):
            found["phase"] = record["phase"]
    return found


def build_parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--app-root")
    p.add_argument("--controller-url")
    p.add_argument("--ryeos-bin", default="ryeos")
    p.add_argument("--handoff", required=True)
    p.add_argument("--binding-id", required=True)
    p.add_argument("--qualification-binding-id", required=True)
    p.add_argument("--render-key-file")
    p.add_argument("--env-file")
    p.add_argument("--owner-id", required=True)
    p.add_argument("--region", required=True)
    p.add_argument("--plan", required=True, choices=["starter", "standard", "pro"])
    p.add_argument("--source-timeout-seconds", type=int, default=3600)
    p.add_argument("--out", required=True)
    p.add_argument("--state", required=True)
    p.add_argument("--poll-interval", type=float, default=15.0)
    p.add_argument("--poll-timeout", type=float, default=900.0)
    p.add_argument("--source-sandbox-id")
    p.add_argument("--snapshot-operation-id")
    p.add_argument("--qualification-operation-id")
    p.add_argument("--verify-resolved", action="store_true")
    p.add_argument("--qualify-relationship")
    p.add_argument("--qualify-verifier-chain-root-id")
    p.add_argument("--qualify-verifier-thread-id")
    return p


def main(argv=None, cli=run_cli, opener=http_open, sleep=time.sleep, clock=time.monotonic,
         key_reader=read_key, log=None):
    args = build_parser().parse_args(argv)
    if any(FORBIDDEN_SERVICE in str(v) for v in vars(args).values()):
        print(f"refused: protected Render service {FORBIDDEN_SERVICE}", file=sys.stderr)
        return 2
    if not (args.app_root or args.controller_url):
        print("one of --app-root or --controller-url is required", file=sys.stderr)
        return 2
    lock = Path(args.state + ".lock")
    try:
        try:
            os.close(os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
        except FileExistsError:
            raise Refusal(f"lock file {lock} exists; another driver is running, or remove it "
                          "if a previous run died")
        try:
            key = key_reader(args)
            driver = Driver(args, key, State(args.state), cli, opener, sleep, clock, log)
            handoff = driver.run()
        finally:
            lock.unlink(missing_ok=True)
    except Refusal as err:
        print(f"refused: {err}", file=sys.stderr)
        return 2
    except Failure as err:
        print(f"failed: {err} (state kept at {args.state})", file=sys.stderr)
        return 1
    print(json.dumps(handoff, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
