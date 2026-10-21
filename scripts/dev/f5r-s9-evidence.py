#!/usr/bin/env python3
"""F5R S9 evidence runner: drives the operator routes of one controller and writes signed evidence.

Runs on the controller (RYEOSD_URL=http://127.0.0.1:8000, the controller's own operator key) or against
an explicit --base-url/--operator-key. Standard library only. Evidence files e1..e7, manifest.json and a
detached manifest.sig (Ed25519 over the hex SHA-256 of the canonical manifest JSON) land in --out.

RISK ACCEPTED (Leo D8, 2026-10-21): the hosted floor (enclosure_trusted_init on Render) is accepted for S9
evidence only. The manifest states this.
"""
import argparse
import base64
import datetime
import hashlib
import importlib.util
import json
import os
import re
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location("s8ctl", Path(__file__).with_name("render-s8-controller.py"))
S8 = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(S8)
Failure, Refusal = S8.Failure, S8.Refusal

SCHEMA = "ryeos.f5r-s9-evidence.v1"
RISK_ACCEPTANCE = ("Hosted-floor risk accepted for S9 evidence only (Leo D8, 2026-10-21): the controller host "
                   "class is 'hosted' on Render (enclosure_trusted_init, no memlock). This evidence does not "
                   "establish a stronger enclosure guarantee.")
SKIP_RENDER_REASON = "render api key is not available on the controller"
DEFAULT_REASON = {"pass": "completed as expected", "fail": "step failed", "skipped": "step not run"}
STEP_ORDER = ["preflight", "profile", "e1", "e2", "e3", "e4", "e6", "e7"]
REFUSAL_RE = re.compile(r"lease refused for workload[^\n\"\\]*")
SERVICE_ID_RE = re.compile(r"^srv-[a-z0-9]+$")
SANDBOX_RE = re.compile(r"^sbx-[a-z0-9-]+$")
E7_PROBE = ("if curl -sS -m 10 -o /dev/null https://example.com; "
            "then echo E7_EGRESS_OPEN; else echo E7_EGRESS_BLOCKED; fi")


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


# --- Ed25519 (RFC 8032, pure Python; the standard library has none) -----------------------------
_P = 2 ** 255 - 19
_Q = 2 ** 252 + 27742317777372353535851937790883648493
_D = (-121665 * pow(121666, _P - 2, _P)) % _P
_I = pow(2, (_P - 1) // 4, _P)


def _recover_x(y, sign):
    if y >= _P:
        return None
    x2 = (y * y - 1) * pow(_D * y * y + 1, _P - 2, _P) % _P
    if x2 == 0:
        return None if sign else 0
    x = pow(x2, (_P + 3) // 8, _P)
    if (x * x - x2) % _P:
        x = x * _I % _P
    if (x * x - x2) % _P:
        return None
    return _P - x if (x & 1) != sign else x


_GY = 4 * pow(5, _P - 2, _P) % _P
_G = (_recover_x(_GY, 0), _GY, 1, _recover_x(_GY, 0) * _GY % _P)


def _add(a, b):
    A = (a[1] - a[0]) * (b[1] - b[0]) % _P
    B = (a[1] + a[0]) * (b[1] + b[0]) % _P
    C = 2 * a[3] * b[3] * _D % _P
    D = 2 * a[2] * b[2] % _P
    E, F, G, H = B - A, D - C, D + C, B + A
    return (E * F % _P, G * H % _P, F * G % _P, E * H % _P)


def _mul(s, pt):
    r = (0, 1, 1, 0)
    while s:
        if s & 1:
            r = _add(r, pt)
        pt = _add(pt, pt)
        s >>= 1
    return r


def _enc(pt):
    zi = pow(pt[2], _P - 2, _P)
    x, y = pt[0] * zi % _P, pt[1] * zi % _P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def _dec(b):
    if len(b) != 32:
        return None
    y = int.from_bytes(b, "little")
    sign, y = y >> 255, y & ((1 << 255) - 1)
    x = _recover_x(y, sign)
    return None if x is None else (x, y, 1, x * y % _P)


def _expand(seed):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little")
    a &= (1 << 254) - 8
    a |= 1 << 254
    return a, h[32:]


def ed25519_public(seed):
    return _enc(_mul(_expand(seed)[0], _G))


def ed25519_sign(seed, msg):
    a, prefix = _expand(seed)
    pub = _enc(_mul(a, _G))
    r = int.from_bytes(hashlib.sha512(prefix + msg).digest(), "little") % _Q
    rs = _enc(_mul(r, _G))
    k = int.from_bytes(hashlib.sha512(rs + pub + msg).digest(), "little") % _Q
    return rs + ((r + k * a) % _Q).to_bytes(32, "little")


def ed25519_verify(pub, msg, sig):
    if len(sig) != 64:
        return False
    A, R = _dec(pub), _dec(sig[:32])
    s = int.from_bytes(sig[32:], "little")
    if A is None or R is None or s >= _Q:
        return False
    k = int.from_bytes(hashlib.sha512(sig[:32] + pub + msg).digest(), "little") % _Q
    lhs, rhs = _mul(s, _G), _add(R, _mul(k, A))
    return (lhs[0] * rhs[2] - rhs[0] * lhs[2]) % _P == 0 and (lhs[1] * rhs[2] - rhs[1] * lhs[2]) % _P == 0


def load_seed(path):
    """Reads a PKCS#8 Ed25519 PEM (the operator key layout) and returns the 32-byte seed."""
    text = Path(path).read_text()
    body = "".join(l for l in text.splitlines() if l and not l.startswith("-----"))
    der = base64.b64decode(body)
    # PKCS#8 v1 (48 bytes) or v2 (the public key attached): the Ed25519
    # algorithm identifier, then the seed as an OCTET STRING wrapping a
    # 32-byte OCTET STRING.
    algorithm = bytes.fromhex("300506032b6570")
    seed_tag = bytes.fromhex("04220420")
    if der[:1] != b"\x30" or algorithm not in der:
        raise Failure(f"{path}: not a PKCS#8 Ed25519 private key")
    at = der.find(seed_tag, der.index(algorithm) + len(algorithm))
    if at < 0 or len(der) < at + 4 + 32:
        raise Failure(f"{path}: not a PKCS#8 Ed25519 private key")
    return der[at + 4:at + 36]


class Signer:
    def __init__(self, seed):
        self.seed = seed
        self.public = ed25519_public(seed)
        self.fingerprint = sha256_hex(self.public)

    def canonical_path(self, path):
        if "?" not in path:
            return path
        base, query = path.split("?", 1)
        params = {}
        for pair in query.split("&"):
            if "=" in pair:
                k, v = pair.split("=", 1)
                params[k] = v
            elif pair:
                params[pair] = ""
        return base + "?" + "&".join(k if not v else f"{k}={v}" for k, v in sorted(params.items()))

    def headers(self, method, path, body, audience, ts=None, nonce=None):
        ts = str(int(time.time()) if ts is None else ts)
        nonce = nonce or secrets.token_hex(16)
        to_sign = "\n".join(["ryeos-request-v1", method.upper(), self.canonical_path(path),
                             sha256_hex(body), ts, nonce, audience])
        digest_hex = sha256_hex(to_sign.encode())
        sig = ed25519_sign(self.seed, digest_hex.encode())
        return {"x-ryeos-key-id": "fp:" + self.fingerprint, "x-ryeos-timestamp": ts,
                "x-ryeos-nonce": nonce, "x-ryeos-signature": base64.b64encode(sig).decode()}


# --- daemon client -------------------------------------------------------------------------------
class Daemon:
    def __init__(self, base, signer, opener=S8.http_open):
        self.base, self.signer, self.opener, self.audience = base.rstrip("/"), signer, opener, None

    def request(self, method, path, body=None, signed=True, timeout=60):
        data = b"" if body is None else json.dumps(body, separators=(",", ":")).encode()
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        if signed:
            headers.update(self.signer.headers(method, path, data, self.audience))
        req = urllib.request.Request(self.base + path, data=data if body is not None else None,
                                     method=method, headers=headers)
        try:
            with self.opener(req, timeout) as resp:
                status, raw = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, raw = err.code, err.read()
        except (urllib.error.URLError, OSError) as err:
            return 0, {"transport_error": type(err).__name__}, b""
        try:
            parsed = json.loads(raw) if raw else None
        except ValueError:
            parsed = {"text": raw.decode("utf-8", "replace")[:2000]}
        return status, parsed, raw


def find_key(obj, names):
    if isinstance(obj, dict):
        for name in names:
            if isinstance(obj.get(name), (str, int)) and obj[name] != "":
                return obj[name]
        for value in obj.values():
            found = find_key(value, names)
            if found is not None:
                return found
    elif isinstance(obj, list):
        for value in obj:
            found = find_key(value, names)
            if found is not None:
                return found
    return None


def unwrap(body):
    """The daemon wraps a service reply as {"result": {...}}; returns the inner object."""
    if isinstance(body, dict) and isinstance(body.get("result"), dict):
        return body["result"]
    return body if isinstance(body, dict) else {}


def collect_states(obj, out=None):
    out = [] if out is None else out
    if isinstance(obj, dict):
        for k, v in obj.items():
            if isinstance(v, str) and k in ("state", "status", "phase", "lease_state", "lease_phase", "outcome"):
                out.append(f"{k}={v}")
            collect_states(v, out)
    elif isinstance(obj, list):
        for v in obj:
            collect_states(v, out)
    return out


# --- runner --------------------------------------------------------------------------------------
class Runner:
    def __init__(self, args, daemon, signer, render=None, proxy_opener=S8.http_open,
                 sleep=time.sleep, clock=time.monotonic, emit=None):
        self.args, self.daemon, self.signer, self.render = args, daemon, signer, render
        self.proxy_opener, self.sleep, self.clock = proxy_opener, sleep, clock
        self.out = Path(args.out)
        self.state_path = Path(args.state) if args.state else self.out / "state.json"
        self.emit = emit or (lambda line: print(line, flush=True))
        self.state = {"steps": {}, "sessions": [], "profile": None, "restart": None}
        self.principal = None

    # state ------------------------------------------------------------------------------------
    def load_state(self):
        if self.state_path.exists():
            self.state = json.loads(self.state_path.read_text())

    def save_state(self):
        self.out.mkdir(parents=True, exist_ok=True)
        tmp = self.state_path.with_name(self.state_path.name + ".tmp")
        tmp.write_text(json.dumps(self.state, indent=2, sort_keys=True) + "\n")
        os.replace(tmp, self.state_path)

    def log(self, step, event, **kw):
        self.emit(json.dumps({"ts": now(), "step": step, "event": event, **kw}, sort_keys=True))

    # evidence ---------------------------------------------------------------------------------
    def call(self, ev, label, method, path, body=None, signed=True):
        started = now()
        status, parsed, raw = self.daemon.request(method, path, body, signed=signed)
        text = raw.decode("utf-8", "replace")
        ev["exchanges"].append({
            "label": label,
            "request": {"method": method, "path": path, "signed": signed,
                        "body": body, "body_sha256": sha256_hex(b"" if body is None else json.dumps(body, separators=(",", ":")).encode())},
            "response": {"status": status, "body_sha256": sha256_hex(raw), "body": text[:8192]},
            "started_at": started, "finished_at": now()})
        self.log(ev["step"], label, status=status)
        return status, parsed

    def execute(self, ev, label, item_ref, parameters, extra=None):
        body = {"item_ref": item_ref, "ref_bindings": {}, "parameters": parameters,
                "execution_policy": self.simple_policy()}
        body.update(extra or {})
        return self.call(ev, label, "POST", "/execute", body)

    @staticmethod
    def simple_policy():
        return {"schema_version": 2, "ownership": "request_scoped", "recovery": "none", "response": "wait",
                "target": {"kind": "here"}, "environment": {"kind": "none"}, "project": {"kind": "projectless"}}

    def session_policy(self):
        if self.args.session_policy_file:
            return json.loads(Path(self.args.session_policy_file).read_text())
        return {"schema_version": 2, "ownership": "daemon_owned", "recovery": "none", "response": "accepted",
                "target": {"kind": "here"}, "environment": {"kind": "none"},
                "project": {"kind": "pinned",
                            "source": {"kind": "capture_live", "scope": "full_project"},
                            "realization": {"kind": "cow", "terminal_publication": {"kind": "discard"}},
                            "child_policy": {"kind": "inherit"}}}

    def session_body(self):
        return {"item_ref": "worker_execution:codex/session",
                "launch_id": "L-" + secrets.token_hex(16),
                "ref_bindings": {"environment": self.args.environment},
                "project_path": self.args.project_path,
                "parameters": {"credential_profile_id": self.state["profile"]["id"]},
                "execution_policy": self.session_policy()}

    def new_ev(self, step, title):
        return {"schema": SCHEMA, "step": step, "title": title, "started_at": now(), "exchanges": [],
                "status": "running"}

    def finish(self, ev, status, **kw):
        ev.update(kw)
        if not ev.get("reason"):
            ev["reason"] = DEFAULT_REASON.get(status, status)
        ev["status"], ev["finished_at"] = status, now()
        name = f"{ev['step']}.json"
        self.out.mkdir(parents=True, exist_ok=True)
        (self.out / name).write_text(json.dumps(ev, indent=2, sort_keys=True) + "\n")
        self.state["steps"][ev["step"]] = {"status": status, "file": name, "reason": ev["reason"]}
        self.save_state()
        self.log(ev["step"], "done", status=status)

    def new_skip(self, step, reason):
        ev = self.new_ev(step, "not run")
        self.finish(ev, "skipped", reason=reason)

    def done(self, step):
        return self.state["steps"].get(step, {}).get("status") in ("pass", "refused", "inconclusive")

    # steps ------------------------------------------------------------------------------------
    def preflight(self):
        ev = self.new_ev("preflight", "ready, public key, signed no-op")
        st, body = self.call(ev, "ready", "GET", "/_ryeos/ready", signed=False)
        if st != 200 or not (body or {}).get("ready"):
            return self.finish(ev, "fail", reason=f"/_ryeos/ready returned {st}")
        st, body = self.call(ev, "public-key", "GET", "/public-key", signed=False)
        self.principal = (body or {}).get("principal_id")
        if st != 200 or not self.principal:
            return self.finish(ev, "fail", reason="/public-key gave no principal_id")
        self.daemon.audience = self.principal
        st, _ = self.execute(ev, "signed-noop", "service:credential-profiles/list", {"limit": 1})
        if st != 200:
            return self.finish(ev, "fail", reason=f"signed no-op returned {st}")
        self.state["principal_id"] = self.principal
        self.finish(ev, "pass", principal_id=self.principal, operator_fingerprint=self.signer.fingerprint)

    LOGIN_NOTE = ("activation requires a provider login: credential-profiles/confirm accepts only a profile in "
                  "the 'confirming' state, which is reached only when a codex login worker (worker_execution:"
                  "codex/login or codex/trusted-login, opened by the ryeos codex login open[-trusted] node "
                  "commands) observes the provider account; no provider-free activation path exists")

    def profile(self):
        ev = self.new_ev("profile", "dummy credential profile via profile-get/create + profile-confirm")
        pid = self.args.profile_id
        self.state["profile"] = {"id": pid, "created": False}
        self.save_state()
        st, got = self.execute(ev, "profile-get", "service:credential-profiles/get", {"profile_id": pid})
        if st == 404:
            st, _ = self.execute(ev, "profile-create", "service:credential-profiles/create", {"profile_id": pid})
            if st != 200:
                return self.finish(ev, "fail", reason=f"profile-create returned {st}")
            self.state["profile"]["created"] = True
            self.save_state()
            st, got = self.execute(ev, "profile-get-after-create", "service:credential-profiles/get",
                                   {"profile_id": pid})
            reused = False
        elif st == 200:
            # An earlier run left this profile; it is reused and torn down by this run.
            self.state["profile"]["created"] = True
            self.save_state()
            reused = True
        else:
            return self.finish(ev, "fail", reason=f"profile-get returned {st}")
        got = unwrap(got)
        pstate = got.get("state")
        self.state["profile"]["generation"] = got.get("credential_generation")
        if pstate == "active":
            return self.finish(ev, "pass", profile_id=pid, reused_existing=reused,
                               credential_generation=got.get("credential_generation"),
                               reason="existing active profile reused")
        epoch = self.args.login_epoch if self.args.login_epoch is not None else got.get("login_epoch")
        digest = self.args.account_digest or got.get("sanitized_account_digest")
        supplied = self.args.login_epoch is not None and bool(self.args.account_digest)
        if pstate != "confirming" and not supplied:
            self.state["profile"]["confirm_refusal"] = self.LOGIN_NOTE
            return self.finish(ev, "inconclusive", profile_id=pid, reused_existing=reused, profile_state=pstate,
                               confirm_refusal=self.LOGIN_NOTE,
                               reason=f"profile is '{pstate}': {self.LOGIN_NOTE}; pass --login-epoch and "
                               "--account-digest for a profile activated by hand; session steps are "
                               "attempted to capture their refusals")
        st, body = self.execute(ev, "profile-confirm", "service:credential-profiles/confirm",
                                {"profile_id": pid, "login_epoch": int(epoch or 0),
                                 "expected_account_digest": digest or ""})
        if 400 <= st < 500:
            refusal = json.dumps(body, sort_keys=True)[:1000]
            self.state["profile"]["confirm_refusal"] = refusal
            return self.finish(ev, "inconclusive", confirm_refusal=refusal, confirm_http_status=st,
                               profile_state=pstate, reused_existing=reused,
                               reason=f"profile-confirm refused ({st}): {refusal}; {self.LOGIN_NOTE}; session "
                               "steps are attempted to capture their refusals")
        if st != 200:
            return self.finish(ev, "fail", reason=f"profile-confirm returned {st}")
        self.state["profile"]["generation"] = unwrap(body).get("credential_generation")
        self.finish(ev, "pass", profile_id=pid, reused_existing=reused)

    def start_session(self, ev, n):
        st, body = self.call(ev, f"session-{n}-start", "POST", "/execute/launch", self.session_body())
        text = json.dumps(body)
        sess = {"n": n, "http_status": st,
                "chain_root_id": find_key(body, ("chain_root_id", "thread_id")),
                "launch_id": find_key(body, ("launch_id",))}
        refusal = REFUSAL_RE.search(text)
        sess["refusal"] = refusal.group(0) if refusal else None
        sess["error_text"] = text[:1000] if (st >= 400 or sess["refusal"]) else None
        if st < 400 and not sess["refusal"]:
            self.state["sessions"].append(sess)
            self.save_state()
        return sess, body

    def observe(self, ev, sess, label, deadline_s, until):
        """Polls the session projection (and launch status) until `until(text, states)` or timeout."""
        end = self.clock() + deadline_s
        text, states = "", []
        while True:
            if sess.get("chain_root_id"):
                _, b = self.execute(ev, label, "service:worker-executions/status",
                                    {"chain_root_id": sess["chain_root_id"]})
            elif sess.get("launch_id"):
                _, b = self.call(ev, label, "GET", f"/launches/{sess['launch_id']}")
            else:
                return text, states
            text, states = json.dumps(b), collect_states(b)
            if until(text, states) or self.clock() >= end:
                return text, states
            self.sleep(self.args.poll_interval)

    @staticmethod
    def admitted(text, states):
        return any(s.endswith("=admitted") or s.endswith("=ready") for s in states)

    def profile_unconfirmed(self):
        return self.state["steps"].get("profile", {}).get("status") == "inconclusive"

    def e1(self):
        ev = self.new_ev("e1", "sessions 1 and 2 admitted within the budget")
        result = []
        for n in (1, 2):
            sess, _ = self.start_session(ev, n)
            if sess["http_status"] >= 400 or sess["refusal"]:
                result.append({"n": n, "started": False, "refusal": sess["refusal"], "http_status": sess["http_status"],
                               "error_text": sess["error_text"]})
                if self.profile_unconfirmed():
                    return self.finish(ev, "refused", sessions=result,
                                       reason=f"session {n} refused ({sess['http_status']}): {sess['error_text']}",
                                       note="the credential profile was not confirmed; the refusal is recorded "
                                       "as evidence, no pass is claimed")
                return self.finish(ev, "fail", sessions=result, reason=f"session {n} was not accepted")
            text, states = self.observe(ev, sess, f"session-{n}-status", self.args.poll_timeout, self.admitted)
            result.append({"n": n, "started": True, "chain_root_id": sess["chain_root_id"],
                           "launch_id": sess["launch_id"], "observed_states": states,
                           "admitted_observed": self.admitted(text, states)})
        self.finish(ev, "pass", sessions=result, budget=self.args.budget)

    def e2(self):
        ev = self.new_ev("e2", "third concurrent session refused by the budget")
        sess, _ = self.start_session(ev, 3)
        refusal = sess["refusal"]
        if not refusal and sess["http_status"] < 400:
            text, _ = self.observe(ev, sess, "session-3-status", self.args.poll_timeout,
                                   lambda t, s: bool(REFUSAL_RE.search(t)))
            m = REFUSAL_RE.search(text)
            refusal = m.group(0) if m else None
        if refusal:
            return self.finish(ev, "pass", refusal_text=refusal, budget=self.args.budget)
        if sess["http_status"] >= 400 and self.profile_unconfirmed():
            return self.finish(ev, "refused", http_status=sess["http_status"], refusal_text=sess["error_text"],
                               reason=f"session 3 refused ({sess['http_status']}): {sess['error_text']}",
                               note="the credential profile was not confirmed, so this refusal is not "
                               "attributed to the budget; no pass is claimed")
        self.finish(ev, "fail", reason="third session was not refused", http_status=sess["http_status"])

    def e3(self):
        ev = self.new_ev("e3", "sandbox create and guest dial of the channel stream")
        target = (self.state["sessions"] or [None])[0]
        if not target:
            if self.profile_unconfirmed():
                return self.finish(ev, "inconclusive", reason="no live session exists: session start was "
                                   "refused because the credential profile was not confirmed")
            return self.finish(ev, "fail", reason="no live session to observe")
        text, states = self.observe(ev, target, "session-1-status", self.args.poll_timeout,
                                    lambda t, s: bool(REFUSAL_RE.search(t)) or "refus" in t.lower()
                                    or any("dial" in x or x.endswith("=ready") for x in s))
        low = text.lower()
        if "refus" in low:
            m = re.search(r"[^\"\\]*refus[^\"\\]*", text, re.I)
            return self.finish(ev, "refused", observed_states=states,
                               reason=(m.group(0) if m else "refused"),
                               note="recorded as evidence; D3 qualification may not be installed; no pass is claimed")
        if any("dial" in s or s.endswith("=ready") for s in states):
            return self.finish(ev, "pass", observed_states=states)
        self.finish(ev, "inconclusive", observed_states=states,
                    reason="no dial or refusal observed before the poll timeout")

    def e4(self):
        ev = self.new_ev("e4", "Ready/report/admit observed through the session status projection")
        seen = []
        for s in self.state["sessions"]:
            _, b = self.execute(ev, f"session-{s['n']}-status", "service:worker-executions/status",
                                {"chain_root_id": s["chain_root_id"]}) if s.get("chain_root_id") else (0, None)
            seen.append({"n": s["n"], "observed_states": collect_states(b)})
        admitted = any(self.admitted("", x["observed_states"]) for x in seen)
        self.finish(ev, "pass" if admitted else "inconclusive", sessions=seen,
                    note="no operator read route exposes the lease_ready_gate verdict; the observation is "
                         "the worker-executions status projection (and 1008/410 on the channel stream is "
                         "guest-facing, not read here)")

    def e6(self):
        ev = self.new_ev("e6", "controller restart through the Render REST API; earlier leases refused")
        if self.args.skip_render:
            return self.finish(ev, "skipped", reason=SKIP_RENDER_REASON)
        sid = self.args.service_id
        S8.guard_id(sid)
        if not sid or not SERVICE_ID_RE.match(sid):
            return self.finish(ev, "fail", reason="--service-id missing or malformed")
        _, svc = self.render.call("GET", f"/services/{sid}", base=self.args.render_base)
        name = (svc or {}).get("name") or ""
        S8.guard(name, {"name": name})
        S8.require_prefix(name)
        restart = self.state.get("restart")
        if not restart:
            st, rd = self.call(ev, "ready-before", "GET", "/_ryeos/ready", signed=False)
            restart = {"started_at_before": (rd or {}).get("started_at"), "requested_at": now()}
            self.state["restart"] = restart
            self.save_state()  # persisted first: a restart on the controller kills this process
            self.render.call("POST", f"/services/{sid}/restart", base=self.args.render_base)
            ev["restart_requested_at"] = restart["requested_at"]
            self.log("e6", "restart-requested", service=sid)
        end = self.clock() + self.args.restart_timeout
        after = None
        while True:
            st, rd = self.call(ev, "ready-after", "GET", "/_ryeos/ready", signed=False)
            if st == 200 and (rd or {}).get("ready") and rd.get("started_at") != restart["started_at_before"]:
                after = rd
                break
            if self.clock() >= end:
                break
            self.sleep(self.args.poll_interval)
        if not after:
            return self.finish(ev, "fail", reason="controller did not return ready with a new start time")
        observed = []
        for s in self.state["sessions"]:
            if s.get("chain_root_id"):
                _, b = self.execute(ev, f"session-{s['n']}-after", "service:worker-executions/status",
                                    {"chain_root_id": s["chain_root_id"]})
                observed.append({"n": s["n"], "text_has_restart_refusal": "restart" in json.dumps(b).lower(),
                                 "observed_states": collect_states(b)})
        self.finish(ev, "pass", service_id=sid, service_name=name,
                    started_at_before=restart["started_at_before"], started_at_after=after.get("started_at"),
                    sessions_after=observed)

    def e7(self):
        ev = self.new_ev("e7", "unlisted egress through the Render run-proxy token route")
        if self.args.skip_render:
            return self.finish(ev, "skipped", reason=SKIP_RENDER_REASON)
        a = self.args
        if not (a.sandbox_id and SANDBOX_RE.match(a.sandbox_id) and a.owner_id and a.region):
            return self.finish(ev, "fail", reason="--sandbox-id, --owner-id and --region are required for E7")
        mint_path = f"/sandboxes/{a.sandbox_id}/runs/stream/token?ownerId={urllib.parse.quote(a.owner_id)}"
        started = now()
        try:
            st, tok = self.render.call("POST", mint_path, base=a.render_base)
        except Failure as err:
            return self.finish(ev, "fail", reason=str(err))
        tok = tok or {}
        if tok.get("token"):
            self.render.add_secret(tok["token"])
        host = f"{a.sandbox_id}.{a.region}.sandbox.onrender.com"
        expected = f"https://{host}/runs/stream"
        ev["exchanges"].append({"label": "token-mint", "request": {"method": "POST", "path": mint_path},
                                "response": {"status": st, "executionId": tok.get("executionId"),
                                             "expiresAt": tok.get("expiresAt"), "method": tok.get("method"),
                                             "uri": tok.get("uri")}, "started_at": started, "finished_at": now()})
        if tok.get("uri") != expected or tok.get("method") != "POST" or not tok.get("token"):
            return self.finish(ev, "fail", reason="token response does not bind the exact run-stream route")
        body = json.loads(Path(a.e7_run_body_file).read_text()) if a.e7_run_body_file else \
            {"command": ["sh", "-c", E7_PROBE]}
        data = json.dumps(body).encode()
        req = urllib.request.Request(a.e7_proxy_url or expected, data=data, method="POST", headers={
            "Authorization": "Bearer " + tok["token"], "Content-Type": "application/json"})
        started = now()
        try:
            with self.proxy_opener(req, a.poll_timeout) as resp:
                pst, raw = resp.status, resp.read(1 << 20)
        except urllib.error.HTTPError as err:
            pst, raw = err.code, err.read()
        except (urllib.error.URLError, OSError) as err:
            pst, raw = 0, type(err).__name__.encode()
        text = self.render.scrub(raw.decode("utf-8", "replace"))
        ev["exchanges"].append({"label": "run-stream", "request": {"method": "POST", "path": "/runs/stream",
                                "host": host, "body": body, "body_sha256": sha256_hex(data)},
                                "response": {"status": pst, "body_sha256": sha256_hex(raw), "body": text[:8192]},
                                "started_at": started, "finished_at": now()})
        if "E7_EGRESS_BLOCKED" in text:
            return self.finish(ev, "pass", note="the unlisted host was unreachable from the lease sandbox")
        if "E7_EGRESS_OPEN" in text:
            return self.finish(ev, "fail", reason="the unlisted host was reachable")
        self.finish(ev, "inconclusive", reason="run-stream output carried neither marker (body shape for the "
                    "run request is not recorded in the tree; pass --e7-run-body-file)")

    # teardown ---------------------------------------------------------------------------------
    def teardown(self):
        ev = self.new_ev("teardown", "end sessions, remove the dummy profile")
        errors = []

        def attempt(label, fn):
            try:
                return fn()
            except Exception as err:  # teardown is best effort per item
                errors.append(f"{label}: {type(err).__name__}: {err}")
                return None

        if self.daemon.audience is None and self.state.get("principal_id"):
            self.daemon.audience = self.state["principal_id"]
        for s in self.state["sessions"]:
            if s.get("ended"):
                continue
            if s.get("chain_root_id"):
                st, _ = attempt(f"terminate-{s['n']}", lambda s=s: self.execute(
                    ev, f"terminate-{s['n']}", "service:worker-executions/terminate",
                    {"chain_root_id": s["chain_root_id"], "reason": "s9 evidence teardown"})) or (0, None)
            elif s.get("launch_id"):
                st, _ = attempt(f"cancel-{s['n']}", lambda s=s: self.call(
                    ev, f"cancel-{s['n']}", "POST", f"/launches/{s['launch_id']}/cancel")) or (0, None)
            else:
                continue
            if st == 200:
                s["ended"] = True
            else:
                errors.append(f"session {s['n']} end returned {st}")
        self.save_state()
        prof = self.state.get("profile")
        if prof and prof.get("created") and not prof.get("removed"):
            pid = prof["id"]
            st, got = attempt("profile-get", lambda: self.execute(
                ev, "profile-get", "service:credential-profiles/get", {"profile_id": pid})) or (0, None)
            if st == 404:
                prof["removed"] = True
            elif st == 200:
                got = unwrap(got)
                gen = got.get("credential_generation")
                if got.get("state") not in ("revoked", "unauthenticated"):
                    r, rb = attempt("profile-revoke", lambda: self.execute(
                        ev, "profile-revoke", "service:credential-profiles/revoke",
                        {"profile_id": pid, "credential_generation": gen})) or (0, None)
                    if r != 200:
                        errors.append(f"profile-revoke returned {r}")
                    else:
                        gen = unwrap(rb).get("credential_generation", gen)
                        _, again = self.execute(ev, "profile-get-after-revoke",
                                                "service:credential-profiles/get", {"profile_id": pid})
                        gen = unwrap(again).get("credential_generation", gen)
                d, _ = attempt("profile-delete", lambda: self.execute(
                    ev, "profile-delete", "service:credential-profiles/delete",
                    {"profile_id": pid, "credential_generation": gen})) or (0, None)
                if d == 200:
                    prof["removed"] = True
                else:
                    errors.append(f"profile-delete returned {d}")
            else:
                errors.append(f"profile-get returned {st}")
        self.save_state()
        self.finish(ev, "fail" if errors else "pass", errors=errors)
        return not errors

    # driver -----------------------------------------------------------------------------------
    def run(self):
        self.load_state()
        failed = False
        try:
            for step in STEP_ORDER:
                if self.done(step):
                    self.log(step, "skipped-already-done")
                    continue
                if failed:
                    self.new_skip(step, "an earlier step failed")
                    continue
                if self.daemon.audience is None and self.state.get("principal_id"):
                    self.daemon.audience = self.state["principal_id"]
                getattr(self, step)()
                if self.state["steps"][step]["status"] == "fail":
                    failed = True
        except BaseException as err:
            failed = True
            self.log("runner", "aborted", error=self.render.scrub(f"{type(err).__name__}: {err}") if self.render else type(err).__name__)
        finally:
            ok = self.teardown()
            self.write_manifest()
        return 0 if (not failed and ok) else 1

    def write_manifest(self):
        steps = []
        for step in STEP_ORDER + ["teardown"]:
            rec = self.state["steps"].get(step, {"status": "not-run"})
            entry = {"id": step, "status": rec["status"]}
            if rec.get("reason"):
                entry["reason"] = rec["reason"]
            if rec.get("file") and (self.out / rec["file"]).exists():
                entry["file"] = rec["file"]
                entry["sha256"] = sha256_hex((self.out / rec["file"]).read_bytes())
            steps.append(entry)
        summary = {k: 0 for k in ("pass", "refused", "inconclusive", "skipped", "fail", "not-run")}
        for entry in steps:
            summary[entry["status"]] = summary.get(entry["status"], 0) + 1
        manifest = {"schema": SCHEMA + ".manifest", "created_at": now(), "base_url": self.args.base_url,
                    "summary": summary,
                    "controller_principal_id": self.state.get("principal_id"),
                    "operator_fingerprint": self.signer.fingerprint,
                    "risk_acceptance": RISK_ACCEPTANCE, "steps": steps}
        raw = canonical(manifest)
        digest = sha256_hex(raw)
        sig = ed25519_sign(self.signer.seed, digest.encode())
        (self.out / "manifest.json").write_bytes(raw + b"\n")
        (self.out / "manifest.sig").write_text(json.dumps({
            "alg": "ed25519", "signed": "hex sha256 of canonical manifest.json (sorted keys, compact, no trailing newline)",
            "key_id": "fp:" + self.signer.fingerprint, "digest_sha256": digest,
            "signature_b64": base64.b64encode(sig).decode()}, indent=2) + "\n")
        self.log("manifest", "written", digest=digest)


def verify_manifest(out, public_key):
    """Returns True when manifest.sig verifies over manifest.json under `public_key` (32 bytes)."""
    raw = Path(out, "manifest.json").read_bytes()
    sig = json.loads(Path(out, "manifest.sig").read_text())
    digest = sha256_hex(raw.rstrip(b"\n"))
    return digest == sig["digest_sha256"] and ed25519_verify(public_key, digest.encode(),
                                                             base64.b64decode(sig["signature_b64"]))


def read_render_key(args):
    return S8.read_key(argparse.Namespace(api_key_file=args.api_key_file, env_file=args.env_file))


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--base-url", default=os.environ.get("RYEOSD_URL", "http://127.0.0.1:8000"))
    p.add_argument("--operator-key", help="operator PKCS#8 PEM; default $RYEOS_APP_ROOT/.ai/config/keys/signing/private_key.pem")
    p.add_argument("--out", required=True)
    p.add_argument("--state")
    p.add_argument("--project-path", required=True)
    p.add_argument("--environment", default="config:codex/environments/external-authoring")
    p.add_argument("--profile-id", default="s9-evidence-" + datetime.date.today().strftime("%Y%m%d"))
    p.add_argument("--login-epoch", type=int)
    p.add_argument("--account-digest")
    p.add_argument("--budget", type=int, default=2)
    p.add_argument("--session-policy-file")
    p.add_argument("--service-id", help="the throwaway controller service (never the farm); required "
                   "unless --skip-render")
    p.add_argument("--skip-render", action="store_true",
                   help="record e6/e7 as skipped and never read the Render api key")
    p.add_argument("--render-base", default=S8.API)
    p.add_argument("--env-file", default=str(S8.DEFAULT_ENV_FILE))
    p.add_argument("--api-key-file")
    p.add_argument("--sandbox-id")
    p.add_argument("--owner-id")
    p.add_argument("--region", default="oregon")
    p.add_argument("--e7-run-body-file")
    p.add_argument("--e7-proxy-url", help=argparse.SUPPRESS)
    p.add_argument("--poll-timeout", type=float, default=120)
    p.add_argument("--poll-interval", type=float, default=3)
    p.add_argument("--restart-timeout", type=float, default=900)
    return p


def main(argv=None, **seams):
    args = parser().parse_args(argv)
    try:
        if args.skip_render:
            if args.service_id:
                S8.guard_id(args.service_id)
        else:
            if not args.service_id:
                raise Refusal("--service-id is required unless --skip-render")
            S8.guard_id(args.service_id)
        key_path = args.operator_key
        if not key_path:
            root = os.environ.get("RYEOS_APP_ROOT")
            if not root:
                raise Refusal("pass --operator-key or set RYEOS_APP_ROOT")
            key_path = str(Path(root) / ".ai/config/keys/signing/private_key.pem")
        signer = Signer(load_seed(key_path))
        api = None if args.skip_render else S8.Api(read_render_key(args), opener=seams.get("render_opener", S8.http_open))
        runner = Runner(args, Daemon(args.base_url, signer), signer, render=api,
                        **{k: v for k, v in seams.items() if k in ("proxy_opener", "sleep", "clock", "emit")})
        return runner.run()
    except Refusal as err:
        print(f"refused: {err}", file=sys.stderr)
        return 2
    except Failure as err:
        print(f"failed: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
