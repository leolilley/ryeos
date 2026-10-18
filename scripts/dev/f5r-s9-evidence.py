#!/usr/bin/env python3
"""Evidence runner for the F5R S9 hosted-lease proof.

Drives the `ryeos` CLI against a remote controller (signing is done by the CLI,
so no Ed25519 code lives here) and writes one evidence file per step plus a
manifest of sha256 hashes. Raw HTTP is used only for unauthenticated routes
(/_ryeos/ready, /public-key, the channel stream refusal probe) and for the
Render REST API.

How the CLI finds the operator key in RYEOSD_URL mode: the app root comes from
RYEOS_APP_ROOT and the key is `<app_root>/.ai/config/keys/signing/private_key.pem`
(PKCS8 PEM Ed25519). `--app-root-or-key` accepts either an app root directory
or a key file; a key file is copied into a private temporary app root of that
layout for the duration of the run.

No CLI reads the lease record, so lease state is judged through proxies: the
session thread's status (created, running, or a terminal status) and the
channel stream refusal. The evidence labels each result accordingly.
"""
import argparse
import datetime
import base64
import hashlib
import http.client
import json
import os
import re
import shutil
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

RENDER_API = "https://api.render.com/v1"
FARM_SERVICE_ID = "srv-datiea7avr4c73dm14fg"
SERVICE_PREFIX = "ryeos-f5r-s9-"
ENVIRONMENT_REF = "config:codex/environments/external-authoring"
REFUSAL_TEXT = "lease refused for workload"
NO_LEASE_BUDGET_TEXT = "lease refused:"
CONCURRENCY_TEXT = "lease refused: the node already holds"
RESTART_TEXT = "controller restarted"
# Names of the channel stream refusals (a JSON `error` before the upgrade).
OCCURRENCE_REFUSED = "external_channel_stream_occurrence_refused"
NO_LIVE_CHANNEL = "external_channel_stream_no_live_channel"
STREAM_PROTOCOL = "ryeos.external-channel.stream.v1"
LIVE_STATUSES = frozenset({"created", "running"})
TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled", "killed", "timed_out", "continued"})
SERVICE_NAME_PREFIX = "ryeos-f5r-s9-"
FORBIDDEN_WORDS = ("farm", "snap-track")
OWNER_RE = re.compile(r"^[A-Za-z0-9_-]+$")
SANDBOX_RE = re.compile(r"^sbx-[a-z0-9-]+$")
CURL_ERROR_RE = re.compile(r"curl: \(\d+\)")
EXIT_CODE_RE = re.compile(r'"?exit_?[cC]ode"?\s*[:=]\s*(-?\d+)')
UNLISTED_URL = "https://example.com/"


class Refusal(Exception):
    pass


class Failure(Exception):
    pass


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_env_file(text):
    values = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, value = line.split("=", 1)
        name, value = name.strip(), value.strip()
        if name.startswith("export "):
            name = name[len("export "):].strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        values[name] = value
    return values


def read_render_key(path):
    path = Path(path)
    if not path.is_file():
        raise Failure(f"no render api key: {path} does not exist")
    if path.stat().st_mode & 0o077:
        raise Refusal(f"env file {path} is group or world readable; run chmod 600 on it")
    key = parse_env_file(path.read_text()).get("RENDER_API_KEY", "").strip()
    if not key or any(c.isspace() or ord(c) < 32 for c in key):
        raise Failure(f"RENDER_API_KEY missing or malformed in {path}")
    return key


def guard_service_id(service_id):
    if service_id.strip().lower() == FARM_SERVICE_ID:
        raise Refusal("refusing to touch the farm service id")
    if not re.match(r"^srv-[a-z0-9]+$", service_id):
        raise Refusal("--render-service-id must look like srv-<id>")


def guard_service_name(name):
    lowered = (name or "").lower()
    if not (name or "").startswith(SERVICE_NAME_PREFIX):
        raise Refusal(f"service name must start with {SERVICE_NAME_PREFIX}")
    for word in FORBIDDEN_WORDS:
        if word in lowered:
            raise Refusal(f"service name contains forbidden word {word!r}")


def find_key(value, names):
    """First value under any of `names`, searched depth first."""
    if isinstance(value, dict):
        for name in names:
            if isinstance(value.get(name), str):
                return value[name]
        for inner in value.values():
            found = find_key(inner, names)
            if found:
                return found
    elif isinstance(value, list):
        for inner in value:
            found = find_key(inner, names)
            if found:
                return found
    return None


def parse_json_text(text):
    """Parses the whole text, else its last JSON-looking line, else None."""
    try:
        return json.loads(text)
    except ValueError:
        pass
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("{") or line.startswith("["):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def thread_status(text):
    """The thread's status string, or None when the output has none."""
    parsed = parse_json_text(text)
    status = find_key(parsed, ("status",)) if parsed is not None else None
    return status.strip().lower() if isinstance(status, str) and status.strip() else None


def thread_is_live(text):
    """Unknown or missing status counts as live."""
    return thread_status(text) not in TERMINAL_STATUSES


def classify_session_start(result):
    """admitted, concurrency-refused, lease-refused (lifetime or weight, or other), or failed."""
    text = result["stdout"] + "\n" + result["stderr"]
    if CONCURRENCY_TEXT in text:
        return "concurrency-refused"
    if REFUSAL_TEXT in text or NO_LEASE_BUDGET_TEXT in text:
        return "lease-refused"
    if result["rc"] == 0 and find_key(parse_json_text(result["stdout"]), ("thread_id",)):
        return "admitted"
    return "failed"


def concurrency_sentence(text):
    match = re.search(r"lease refused: the node already holds[^\"\n]*?none is queued", text)
    if not match:
        match = re.search(r"lease refused: the node already holds[^\"\n]*", text)
    return match.group(0) if match else None


def classify_stream_probe(status, body):
    """Classifies the response to a real channel stream upgrade attempt."""
    code = None
    try:
        code = json.loads(body).get("error")
    except (ValueError, AttributeError, TypeError):
        pass
    if status == 101:
        return "channel-live"
    if status == 410 and code == OCCURRENCE_REFUSED:
        return "lease-refused"
    if status == 404 and code == NO_LIVE_CHANNEL:
        return "no-live-channel"
    if status == 400:
        return "probe-bug"
    return "unknown"


def validate_proxy_uri(uri, sandbox, region):
    expected = f"https://{sandbox}.{region}.sandbox.onrender.com/runs/stream"
    if uri != expected:
        raise Failure("minted run-stream uri is not the sandbox's own /runs/stream")


def ws_probe(base_url, path, timeout=30):
    """One websocket upgrade attempt (http.client keeps the connection open).

    Returns (status, body); a 101 socket is closed at once."""
    parsed = urllib.parse.urlsplit(base_url)
    port = parsed.port
    if parsed.scheme == "https":
        conn = http.client.HTTPSConnection(parsed.hostname, port or 443, timeout=timeout,
                                           context=ssl.create_default_context())
    else:
        conn = http.client.HTTPConnection(parsed.hostname, port or 80, timeout=timeout)
    try:
        conn.request("GET", path, headers={
            "Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Version": "13",
            "Sec-WebSocket-Key": base64.b64encode(os.urandom(16)).decode(),
            "Sec-WebSocket-Protocol": STREAM_PROTOCOL,
        })
        resp = conn.getresponse()
        body = b"" if resp.status == 101 else resp.read()
        return resp.status, body
    except (OSError, http.client.HTTPException) as err:
        raise Failure(f"stream probe: {type(err).__name__}")
    finally:
        conn.close()


def default_http(method, url, headers=None, body=None, timeout=60):
    """Returns (status, bytes); never follows redirects."""

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args, **kwargs):
            return None

    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.build_opener(NoRedirect).open(request, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except (urllib.error.URLError, OSError) as err:
        raise Failure(f"{method} {url.split('?')[0]}: {type(err).__name__}")


def default_run(argv, env, timeout):
    """Returns {rc, stdout, stderr, timed_out}; a timeout keeps the partial output."""
    try:
        done = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
        return {"rc": done.returncode, "stdout": done.stdout, "stderr": done.stderr, "timed_out": False}
    except subprocess.TimeoutExpired as err:
        def text(part):
            return part.decode(errors="replace") if isinstance(part, bytes) else (part or "")
        return {"rc": None, "stdout": text(err.stdout), "stderr": text(err.stderr), "timed_out": True}
    except OSError as err:
        return {"rc": 127, "stdout": "", "stderr": f"{type(err).__name__}", "timed_out": False}


class Runner:
    def __init__(self, args, render_key, app_root, http=default_http, run=default_run,
                 probe=ws_probe, sleep=time.sleep, clock=time.monotonic, log=None):
        self.args = args
        self.render_key = render_key
        self.app_root = Path(app_root)
        self.http = http
        self.run = run
        self.probe = probe
        self.sleep = sleep
        self.clock = clock
        self.log = log or (lambda line: print(line, flush=True))
        self.out = Path(args.out)
        self.secrets = [render_key]
        self.files = []
        self.failed = []
        self.threads = []
        self.leases = []
        self.state = {}
        self.placements = []
        for item in getattr(args, "placement", None) or []:
            placement, _, sandbox = item.partition(":")
            if not placement or not SANDBOX_RE.match(sandbox):
                raise Refusal("--placement must be <placement_thread_id>:sbx-<id>")
            self.placements.append({"placementThreadId": placement, "sandboxOccurrenceId": sandbox})

    def scrub(self, text):
        for secret in self.secrets:
            if secret:
                text = text.replace(secret, "***")
        text = re.sub(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]+", "Bearer ***", text)
        text = re.sub(r'(?i)("token"\s*:\s*")[^"]*(")', r"\1***\2", text)
        text = re.sub(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
                      "***", text, flags=re.S)
        return text

    def cli(self, *words, timeout=120):
        env = dict(os.environ)
        env["RYEOSD_URL"] = self.args.controller_url.rstrip("/")
        env["RYEOS_APP_ROOT"] = str(self.app_root)
        env.pop("RENDER_API_KEY", None)
        result = self.run([self.args.ryeos_bin, *words], env, timeout)
        return {k: (self.scrub(v) if isinstance(v, str) else v) for k, v in result.items()}

    def write(self, name, record):
        self.out.mkdir(parents=True, exist_ok=True)
        text = self.scrub(json.dumps(record, indent=2, sort_keys=True)) + "\n"
        (self.out / name).write_text(text)
        if name not in self.files:
            self.files.append(name)

    def step(self, name, file, fn):
        record = {"step": name, "startedAt": now()}
        try:
            record.update(fn())
            record.setdefault("ok", True)
        except Refusal as err:
            record.update(ok=False, error=self.scrub(f"refusal: {err}"))
        except Failure as err:
            record.update(ok=False, error=self.scrub(str(err)))
        except Exception as err:
            record.update(ok=False, error=self.scrub(f"{type(err).__name__}: {err}"))
        record["finishedAt"] = now()
        self.write(file, record)
        self.log(json.dumps({"step": name, "ok": record["ok"], "file": file,
                             "error": record.get("error")}))
        if not record["ok"]:
            self.failed.append(name)
        return record["ok"]

    def depends(self, name, file, needs, fn):
        missing = [n for n in needs if n in self.failed or n not in self.state.get("done", [])]
        if missing:
            self.write(file, {"step": name, "ok": False, "skipped": True,
                              "error": f"skipped: {', '.join(missing)} did not pass"})
            self.log(json.dumps({"step": name, "ok": False, "file": file, "error": "skipped"}))
            self.failed.append(name)
            return False
        return self.step(name, file, fn)

    def mark_done(self, name, ok):
        if ok:
            self.state.setdefault("done", []).append(name)

    def ready(self):
        status, _ = self.http("GET", self.args.controller_url.rstrip("/") + "/_ryeos/ready", None, None, 30)
        return status

    def principal(self):
        status, body = self.http("GET", self.args.controller_url.rstrip("/") + "/public-key", None, None, 30)
        if status != 200:
            raise Failure(f"/public-key returned {status}")
        found = find_key(parse_json_text(body.decode(errors="replace")), ("principal_id", "principal"))
        if not found:
            raise Failure("/public-key carries no principal id")
        return found

    def preflight(self):
        ready = self.ready()
        if ready != 200:
            raise Failure(f"/_ryeos/ready returned {ready}")
        principal = self.principal()
        self.state["principal"] = principal
        noop = self.cli("thread", "list", timeout=60)
        if noop["rc"] != 0:
            raise Failure("signed no-op (thread list) failed: " + noop["stderr"][:300])
        return {"ready": ready, "principal": principal, "signedNoop": {"rc": noop["rc"]}}

    def start_session(self):
        a = self.args
        return self.cli("codex", "session", "start", a.credential_profile, "--project", a.project,
                        "--environment", ENVIRONMENT_REF, "--async", "--current-head", timeout=300)

    def get_thread(self, tid):
        got = self.cli("thread", "get", tid)
        return got, got["stdout"] + "\n" + got["stderr"]

    def e1(self):
        sessions = []
        for index in (1, 2):
            result = self.start_session()
            verdict = classify_session_start(result)
            entry = {"session": index, "verdict": verdict, "rc": result["rc"]}
            if verdict == "admitted":
                parsed = parse_json_text(result["stdout"])
                tid = find_key(parsed, ("thread_id",))
                entry.update(threadId=tid, status=find_key(parsed, ("status",)))
                self.threads.append(tid)
                self.leases.append({"threadId": tid})
            else:
                entry["output"] = (result["stdout"] + result["stderr"])[:600]
            sessions.append(entry)
        bad = [s["session"] for s in sessions if s["verdict"] != "admitted"]
        if bad:
            raise Failure(f"sessions not admitted: {bad}")
        return {"sessions": sessions}

    def e2(self):
        result = self.start_session()
        verdict = classify_session_start(result)
        text = result["stdout"] + "\n" + result["stderr"]
        tid = find_key(parse_json_text(result["stdout"]), ("thread_id",))
        if tid:
            self.threads.append(tid)
        polled = []
        if verdict == "admitted":
            # An accepted async launch may be refused on its own thread afterwards.
            for _ in range(self.args.poll_attempts):
                _, text = self.get_thread(tid)
                status = thread_status(text)
                polled.append(status)
                if CONCURRENCY_TEXT in text and status in TERMINAL_STATUSES:
                    verdict = "concurrency-refused"
                    break
                if status in TERMINAL_STATUSES:
                    break
                self.sleep(self.args.poll_interval)
        sentence = concurrency_sentence(text)
        if verdict != "concurrency-refused" or not sentence:
            raise Failure(f"third session was not refused for lease concurrency (verdict {verdict})")
        return {"verdict": verdict, "refusalText": sentence, "rc": result["rc"],
                "threadId": tid, "polledStatuses": polled}

    def e3(self):
        """The session thread reaches running and stays running through a readiness window.

        No CLI reads the lease record; a running thread is the proxy for an admitted lease."""
        found = {}
        for lease in self.leases:
            tid = lease["threadId"]
            reached = False
            seen = []
            for _ in range(self.args.poll_attempts):
                _, text = self.get_thread(tid)
                seen.append(thread_status(text))
                if seen[-1] == "running":
                    reached = True
                    break
                if seen[-1] in TERMINAL_STATUSES:
                    break
                self.sleep(self.args.poll_interval)
            window = []
            if reached:
                for _ in range(self.args.window_polls):
                    self.sleep(self.args.poll_interval)
                    _, text = self.get_thread(tid)
                    window.append(thread_status(text))
            stays = reached and all(s == "running" for s in window)
            found[tid] = {"reachedRunning": reached, "windowStatuses": window,
                          "staysRunning": stays, "polledStatuses": seen}
        bad = [t for t, v in found.items() if not v["staysRunning"]]
        if bad:
            raise Failure(f"thread did not reach and hold running: {bad}")
        return {"threads": found, "proxy": "thread status running; no CLI reads the lease record"}

    def wait_restart(self):
        """Waits until the old instance has stopped answering, then until a new one is ready."""
        old = self.state.get("principal")
        deadline = self.clock() + self.args.restart_timeout
        went_down = False
        while self.clock() < deadline:
            try:
                if self.ready() != 200 or (old and self.principal() != old):
                    went_down = True
                    break
            except Failure:
                went_down = True
                break
            self.sleep(self.args.poll_interval)
        if not went_down:
            raise Failure("old controller instance never stopped answering after restart")
        deadline = self.clock() + self.args.restart_timeout
        while self.clock() < deadline:
            try:
                if self.ready() == 200:
                    return
            except Failure:
                pass
            self.sleep(self.args.poll_interval)
        raise Failure("controller did not become ready after restart")

    def e6(self):
        """Restarts the Render service, then checks each earlier lease is durably refused.

        No CLI reads the lease record: the proxies are the session thread going terminal with
        the restart refusal text (E4) and, where a placement is given, the stream upgrade
        refusal."""
        guard_service_id(self.args.render_service_id)
        guard_service_name(self.args.render_service_name)
        status, _ = self.http("POST", f"{RENDER_API}/services/{self.args.render_service_id}/restart",
                              {"Authorization": "Bearer " + self.render_key, "Accept": "application/json"},
                              None, 60)
        if status >= 300:
            raise Failure(f"restart returned HTTP {status}")
        self.wait_restart()
        rechecks, bad = [], []
        for lease in self.leases:
            entry = {"threadId": lease["threadId"], "statuses": []}
            text = ""
            for _ in range(self.args.poll_attempts):
                _, text = self.get_thread(lease["threadId"])
                entry["statuses"].append(thread_status(text))
                if entry["statuses"][-1] in TERMINAL_STATUSES:
                    break
                self.sleep(self.args.poll_interval)
            entry["terminal"] = entry["statuses"][-1] in TERMINAL_STATUSES
            entry["restartRefusalText"] = RESTART_TEXT in text
            if not (entry["terminal"] and entry["restartRefusalText"]):
                bad.append(lease["threadId"])
            rechecks.append(entry)
        streams = []
        for placement in self.placements:
            path = (f"/external-execution/channel/{placement['placementThreadId']}/"
                    f"{placement['sandboxOccurrenceId']}/stream")
            code, raw = self.probe(self.args.controller_url.rstrip("/"), path)
            verdict = classify_stream_probe(code, raw.decode(errors="replace") if raw else "")
            streams.append({**placement, "status": code, "streamVerdict": verdict})
            if verdict not in ("lease-refused", "no-live-channel"):
                bad.append(placement["placementThreadId"])
        record = {"restartStatus": status, "rechecks": rechecks, "streams": streams,
                  "streamVerdict": "probed" if streams else "unprobed",
                  "note": "no CLI reads the lease record; thread status and stream refusal are proxies"}
        if not streams:
            record["note"] += "; no --placement given, judged on thread status only"
        if bad:
            record.update(ok=False, error=f"leases not shown refused after restart: {bad}")
        return record

    def e7(self):
        a = self.args
        guard_service_name(a.render_service_name)
        if not self.placements:
            raise Failure("--placement is required for E7 (it names the sandbox)")
        if not OWNER_RE.match(a.render_owner_id or ""):
            raise Failure("--render-owner-id is required and must match [A-Za-z0-9_-]+")
        sandbox = self.placements[0]["sandboxOccurrenceId"]
        headers = {"Authorization": "Bearer " + self.render_key, "Accept": "application/json"}
        mint = f"{RENDER_API}/sandboxes/{sandbox}/runs/stream/token?ownerId={a.render_owner_id}"
        status, raw = self.http("POST", mint, headers, None, 60)
        if status >= 300:
            raise Failure(f"run-stream token mint returned HTTP {status}")
        minted = parse_json_text(raw.decode(errors="replace")) or {}
        token, uri = minted.get("token"), minted.get("uri")
        if not token or not uri:
            raise Failure("token mint response lacks token or uri")
        self.secrets.append(token)
        validate_proxy_uri(uri, sandbox, a.sandbox_region)
        body = a.e7_body_json or json.dumps({"command": ["curl", "-sS", "--max-time", "10", UNLISTED_URL]})
        code, out = self.http("POST", uri, {"Authorization": "Bearer " + token,
                                            "Content-Type": "application/json"},
                              body.encode(), 60)
        text = self.scrub(out.decode(errors="replace"))
        record = {"mintStatus": status, "mintMethodVerified": False, "runStatus": code,
                  "target": UNLISTED_URL, "bodyShapeVerified": False, "output": text[:800]}
        if code < 300 and "Example Domain" in text:
            raise Failure("unlisted host was reachable from the sandbox")
        exit_match = EXIT_CODE_RE.search(text)
        proven = bool(CURL_ERROR_RE.search(text)) or (exit_match is not None and int(exit_match.group(1)) != 0)
        if proven:
            record.update(egressBlocked=True)
        else:
            record.update(ok=False, egressBlocked=None, error="inconclusive: no exit code or curl error in the output")
        return record

    def teardown(self):
        record = {"step": "teardown", "startedAt": now(), "cancelled": [], "problems": [],
                  "sandboxOccurrenceIds": [p["sandboxOccurrenceId"] for p in self.placements],
                  "note": "cancelling a thread may not destroy its sandbox; verify sandboxes separately"}
        for tid in dict.fromkeys(t for t in self.threads if t):
            result = self.cli("thread", "cancel", tid)
            entry = {"threadId": tid, "rc": result["rc"], "statuses": []}
            for _ in range(10):
                check, text = self.get_thread(tid)
                entry["statuses"].append(thread_status(text) if check["rc"] == 0 else None)
                if check["rc"] == 0 and not thread_is_live(text):
                    break
                self.sleep(self.args.poll_interval)
            entry["stillLive"] = entry["statuses"][-1] not in TERMINAL_STATUSES
            if entry["stillLive"]:
                record["problems"].append(f"{tid} not terminal after cancel")
            record["cancelled"].append(entry)
        record["ok"] = not record["problems"]
        record["finishedAt"] = now()
        self.write("teardown.json", record)
        self.log(json.dumps({"step": "teardown", "ok": record["ok"], "file": "teardown.json",
                             "error": "; ".join(record["problems"]) or None}))
        if not record["ok"]:
            self.failed.append("teardown")

    def manifest(self):
        entries = []
        for name in self.files:
            data = (self.out / name).read_bytes()
            entries.append({"file": name, "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)})
        text = self.scrub(json.dumps({"generatedAt": now(), "failedSteps": self.failed, "files": entries},
                                     indent=2)) + "\n"
        (self.out / "manifest.json").write_text(text)
        return entries

    def execute(self):
        try:
            ok = self.step("preflight", "preflight.json", self.preflight)
            self.mark_done("preflight", ok)
            ok = self.depends("e1-admit-two", "e1-sessions.json", ["preflight"], self.e1)
            self.mark_done("e1-admit-two", ok)
            ok = self.depends("e2-third-refused", "e2-refusal.json", ["e1-admit-two"], self.e2)
            self.mark_done("e2-third-refused", ok)
            ok = self.depends("e3-running", "e3-running.json", ["e1-admit-two"], self.e3)
            self.mark_done("e3-running", ok)
            ok = self.depends("e4-e6-restart-refusal", "e6-restart.json", ["e3-running"], self.e6)
            self.mark_done("e4-e6-restart-refusal", ok)
            self.step("e7-unlisted-egress", "e7-egress.json", self.e7)
        finally:
            try:
                self.teardown()
            finally:
                self.manifest()
        return 1 if self.failed else 0


def prepare_app_root(path, scratch):
    """Returns an app root directory for the CLI; a key file is staged into one."""
    path = Path(path)
    if path.is_dir():
        return path
    if not path.is_file():
        raise Failure(f"--app-root-or-key {path} does not exist")
    if path.stat().st_mode & 0o077:
        raise Refusal(f"key file {path} is group or world readable; run chmod 600 on it")
    target = Path(scratch) / ".ai" / "config" / "keys" / "signing"
    target.mkdir(parents=True, mode=0o700)
    dest = target / "private_key.pem"
    shutil.copyfile(path, dest)
    dest.chmod(0o600)
    return Path(scratch)


def build_parser():
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--controller-url", required=True)
    p.add_argument("--app-root-or-key", required=True)
    p.add_argument("--credential-profile", required=True)
    p.add_argument("--render-env-file", required=True)
    p.add_argument("--render-service-id", required=True)
    p.add_argument("--render-service-name", required=True)
    p.add_argument("--placement", action="append", default=[],
                   help="<placement_thread_id>:<sandbox_occurrence_id>, repeatable")
    p.add_argument("--sandbox-region", default="oregon")
    p.add_argument("--window-polls", type=int, default=3)
    p.add_argument("--out", required=True)
    p.add_argument("--project", default=".")
    p.add_argument("--ryeos-bin", default="ryeos")
    p.add_argument("--render-owner-id", default="")
    p.add_argument("--e7-body-json", default="")
    p.add_argument("--poll-attempts", type=int, default=30)
    p.add_argument("--poll-interval", type=float, default=10.0)
    p.add_argument("--tail-seconds", type=float, default=5.0)
    p.add_argument("--restart-timeout", type=float, default=600.0)
    return p


def main(argv=None, http=default_http, run=default_run, probe=ws_probe):
    args = build_parser().parse_args(argv)
    scratch = tempfile.mkdtemp(prefix="f5r-s9-")
    try:
        os.chmod(scratch, 0o700)
        guard_service_id(args.render_service_id)
        guard_service_name(args.render_service_name)
        render_key = read_render_key(args.render_env_file)
        app_root = prepare_app_root(args.app_root_or_key, scratch)
        runner = Runner(args, render_key, app_root, http=http, run=run, probe=probe)
        return runner.execute()
    except (Refusal, Failure) as err:
        print(json.dumps({"step": "setup", "ok": False, "error": str(err)}), flush=True)
        return 1
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
