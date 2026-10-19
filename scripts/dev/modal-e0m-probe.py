#!/usr/bin/env python3
"""E0M: one throwaway Modal CPU sandbox run that measures what the Modal
sandbox adapter will rely on, one named step at a time.

Every step is a real measurement taken from inside a sandbox or through the
SDK; a failing step records its error and the run continues. Evidence lands in
<out>/modal-e0m-evidence.json. Cleanup terminates every sandbox the run
created and then lists sandboxes by the run tag to assert none remain.
"""
import argparse
import base64
import contextlib
import hashlib
import json
import os
import socket
import ssl
import sys
import threading
import time
import traceback
import uuid
from pathlib import Path

TAG_KEY = "ryeos-e0m-run"
EVIDENCE_NAME = "modal-e0m-evidence.json"
SLEEP_SECONDS = "900"
# Backstop for a sandbox that never received the run tag: it ends by itself.
DEFAULT_LIFETIME = 240
WS_PORT = 8765
WS_PAYLOAD = "ryeos-e0m"

# Seam: tests replace this with a function returning a fake SDK module.
def load_modal():
    import modal
    return modal


# ---------------------------------------------------------------- context

class Ctx:
    """Shared state for one run: the SDK, the app, and every sandbox created."""

    def __init__(self, modal, app_name, run_id, gpu):
        self.modal = modal
        self.app_name = app_name
        self.run_id = run_id
        self.gpu = gpu
        self.app = None
        self.image = None
        self.created = []

    def ensure_app(self):
        if self.app is None:
            self.app = self.modal.App.lookup(self.app_name, create_if_missing=True)
            self.image = self.modal.Image.debian_slim()
        return self.app

    def sandbox(self, *args, **kwargs):
        """Create a tagged sandbox that idles on `sleep` unless args are given."""
        self.ensure_app()
        kwargs.setdefault("timeout", DEFAULT_LIFETIME)
        sb = self.modal.Sandbox.create(
            *(args or ("sleep", SLEEP_SECONDS)),
            app=self.app, image=self.image, **kwargs,
        )
        self.created.append(sb)
        sb.set_tags({TAG_KEY: self.run_id})
        return sb

    @contextlib.contextmanager
    def scratch(self, *args, **kwargs):
        """A tagged sandbox that is terminated when the block ends, however it ends."""
        sb = self.sandbox(*args, **kwargs)
        try:
            yield sb
        finally:
            try:
                sb.terminate()
            except Exception:
                pass


def write_file(sb, path, text):
    """Write a file through exec stdin; every Sandbox file API is refused server-side."""
    p = sb.exec("sh", "-c", f"cat > {path}", timeout=60)
    p.stdin.write(text)
    p.stdin.write_eof()
    p.stdin.drain()
    err = p.stderr.read()
    code = p.wait()
    if code != 0:
        raise RuntimeError(f"write {path} exit {code}: {err.strip()[-200:]}")


def read_file(sb, path):
    p = sb.exec("cat", path, timeout=60)
    out = p.stdout.read()
    p.wait()
    return out


def run_py(sb, code, stdin=None, timeout=60):
    """Run python in the sandbox; return (exit, stdout, stderr)."""
    p = sb.exec("python3", "-c", code, timeout=timeout)
    if stdin is not None:
        p.stdin.write(stdin)
        p.stdin.write_eof()
        p.stdin.drain()
    out = p.stdout.read()
    err = p.stderr.read()
    return p.wait(), out, err


def json_from_guest(code, out, err):
    """Parse a guest program's JSON line, keeping the exit code and stderr on failure."""
    if code != 0 or not out.strip():
        raise RuntimeError(f"guest program exit {code}, stdout {out.strip()[-120:]!r}, stderr {err.strip()[-300:]!r}")
    return json.loads(out)


def cert_summary(tls):
    cert = tls.getpeercert()
    flat = lambda rdn: {k: v for part in rdn for k, v in part}
    return {
        "subject": flat(cert.get("subject", ())),
        "issuer": flat(cert.get("issuer", ())),
        "notAfter": cert.get("notAfter"),
        "san": [v for _, v in cert.get("subjectAltName", ())][:4],
        "tls_version": tls.version(),
        "cipher": tls.cipher()[0],
    }


# ------------------------------------------------------------------ steps

STEPS = []


GROUPS = ("e0m", "launcher")


def step(name, desc, gpu=False, group="e0m"):
    if group not in GROUPS:
        raise ValueError(f"unknown group {group}")

    def deco(fn):
        STEPS.append({"name": name, "description": desc, "gpu": gpu, "group": group, "fn": fn})
        return fn
    return deco


EGRESS_CODE = r"""
import json, socket, ssl
host = "example.com"
ctx = ssl.create_default_context()
raw = socket.create_connection((host, 443), timeout=15)
peer = raw.getpeername()[0]
tls = ctx.wrap_socket(raw, server_hostname=host)
import hashlib
chain = tls.get_verified_chain() if hasattr(tls, "get_verified_chain") else None
chain_sha256 = None
if chain:
    chain_sha256 = [hashlib.sha256(c if isinstance(c, bytes) else ssl.PEM_cert_to_DER_cert(c)).hexdigest() for c in chain]
paths = ssl.get_default_verify_paths()
cert = tls.getpeercert()
flat = lambda rdn: {k: v for p in rdn for k, v in p}
tls.sendall(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
buf = b""
while b"\r\n" not in buf:
    d = tls.recv(512)
    if not d: break
    buf += d
print(json.dumps({"status_line": buf.split(b"\r\n")[0].decode(),
  "tls_version": tls.version(), "cipher": tls.cipher()[0],
  "subject": flat(cert["subject"]), "issuer": flat(cert["issuer"]),
  "notAfter": cert["notAfter"], "verified_chain_len": len(chain) if chain else None, "verified_chain_sha256": chain_sha256,
  "ca_cafile": paths.cafile, "ca_openssl_cafile": paths.openssl_cafile,
  "peer": peer}))
"""


@step("egress_tls", "HTTPS GET to example.com from inside the sandbox; observe the verified certificate")
def egress_tls(ctx):
    sb = ctx.sandbox()
    code, out, err = run_py(sb, EGRESS_CODE)
    if code != 0:
        raise RuntimeError(f"egress probe exit {code}: {err.strip()[-300:]}")
    return json.loads(out)


@step("dns_residual", "resolve a public name inside a block_network sandbox; record failure or success")
def dns_residual(ctx):
    sb = ctx.sandbox(block_network=True)
    code, out, err = run_py(sb, (
        "import socket,json\n"
        "try:\n r=sorted({a[4][0] for a in socket.getaddrinfo('example.com',443)})\n print(json.dumps({'resolved':True,'addrs':r}))\n"
        "except Exception as e:\n print(json.dumps({'resolved':False,'error':type(e).__name__+': '+str(e)}))\n"))
    result = json_from_guest(code, out, err)
    conn_code, conn_out, conn_err = run_py(sb, (
        "import socket,json\n"
        "try:\n socket.create_connection(('1.1.1.1',443),timeout=8); print(json.dumps({'connected':True}))\n"
        "except Exception as e:\n print(json.dumps({'connected':False,'error':type(e).__name__}))\n"))
    result["tcp_to_ip"] = json_from_guest(conn_code, conn_out, conn_err)
    return result


@step("block_allow_rejection", "block_network with cidr_allowlist: records whether the SDK refuses it client-side (no sandbox is created) and whether the SDK has any domain allow-list parameter")
def block_allow_rejection(ctx):
    import inspect
    ev = {}
    try:
        params = list(inspect.signature(ctx.modal.Sandbox.create).parameters)
        ev["create_params_with_allow"] = [p for p in params if "allow" in p or "domain" in p or "network" in p]
        ev["domain_allow_list_in_sdk"] = any("domain" in p and p != "custom_domain" for p in params)
    except Exception as e:
        ev["signature_error"] = f"{type(e).__name__}: {str(e)[:120]}"
    try:
        ctx.sandbox("sleep", "30", block_network=True, cidr_allowlist=["1.1.1.1/32"])
        ev["sdk_refused"] = False
    except Exception as e:
        ev["sdk_refused"] = True
        ev["error_type"] = f"{type(e).__module__}.{type(e).__name__}"
        ev["error"] = str(e)[:200]
    return ev


# The SDK has no control for an allowed address, so this only shows that an
# address outside the list is unreachable; it cannot show an allowed one works.
@step("cidr_allowlist_egress", "a sandbox with a cidr_allowlist that excludes the target: record whether the excluded address is reachable")
def cidr_allowlist_egress(ctx):
    sb = ctx.sandbox(cidr_allowlist=["192.0.2.0/24"])
    code, out, err = run_py(sb, (
        "import socket,json\n"
        "try:\n socket.create_connection(('1.1.1.1',443),timeout=8); print(json.dumps({'excluded_reachable':True}))\n"
        "except Exception as e:\n print(json.dumps({'excluded_reachable':False,'error':type(e).__name__}))\n"))
    return json_from_guest(code, out, err)


WS_SERVER = r"""
import base64, hashlib, socket, struct
GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", %d)); srv.listen(4)
def rd(c, n):
    b = b""
    while len(b) < n:
        d = c.recv(n - len(b))
        if not d: raise EOFError
        b += d
    return b
while True:
    c, _ = srv.accept()
    try:
        h = b""
        while b"\r\n\r\n" not in h:
            d = c.recv(1024)
            if not d: raise EOFError
            h += d
        key = [l.split(b":",1)[1].strip() for l in h.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key")][0]
        acc = base64.b64encode(hashlib.sha1(key + GUID).digest())
        c.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + acc + b"\r\n\r\n")
        while True:
            b0, b1 = rd(c, 2)
            n = b1 & 0x7f
            if n == 126: n = struct.unpack(">H", rd(c, 2))[0]
            mask = rd(c, 4) if b1 & 0x80 else b"\0\0\0\0"
            data = bytes(x ^ mask[i %% 4] for i, x in enumerate(rd(c, n)))
            if b0 & 0x0f == 8:
                c.sendall(b"\x88\x00"); break
            c.sendall(bytes([0x80 | (b0 & 0x0f), len(data)]) + data)
    except Exception:
        pass
    finally:
        c.close()
""" % WS_PORT


def ws_client_echo(host, port, payload):
    """Minimal RFC 6455 client over TLS: handshake, one masked text frame, read the echo."""
    ctx = ssl.create_default_context()
    raw = socket.create_connection((host, port), timeout=20)
    tls = ctx.wrap_socket(raw, server_hostname=host)
    key = base64.b64encode(os.urandom(16)).decode()
    tls.sendall((f"GET / HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                 f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
    head = b""
    while b"\r\n\r\n" not in head:
        d = tls.recv(1024)
        if not d:
            raise RuntimeError("closed during handshake")
        head += d
    status = head.split(b"\r\n")[0].decode()
    expect = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
    accept_ok = expect.encode() in head
    body = payload.encode()
    mask = os.urandom(4)
    tls.sendall(bytes([0x81, 0x80 | len(body)]) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(body)))
    buf = head.split(b"\r\n\r\n", 1)[1]
    while len(buf) < 2 or len(buf) < 2 + (buf[1] & 0x7f):
        d = tls.recv(1024)
        if not d:
            break
        buf += d
    echoed = buf[2:2 + (buf[1] & 0x7f)].decode(errors="replace")
    summary = cert_summary(tls)
    tls.close()
    return {"status_line": status, "accept_header_ok": accept_ok, "echoed": echoed, "tls": summary}


@step("websocket_echo", "tiny RFC 6455 echo server inside the sandbox behind an encrypted tunnel; stdlib client checks TLS and echo")
def websocket_echo(ctx):
    sb = ctx.sandbox(encrypted_ports=[WS_PORT])
    write_file(sb, "/tmp/ws_server.py", WS_SERVER)
    server = sb.exec("python3", "/tmp/ws_server.py")
    tunnel = sb.tunnels()[WS_PORT]
    last = None
    for _ in range(10):
        try:
            ev = ws_client_echo(tunnel.host, tunnel.port, WS_PAYLOAD)
            break
        except (OSError, RuntimeError, IndexError) as e:
            last = e
            time.sleep(1)
    else:
        raise RuntimeError(f"tunnel never answered: {last!r}")
    ev["transport"] = "in-sandbox stdlib server via sb.tunnels() encrypted port"
    ev["tunnel_host"] = tunnel.host
    ev["server_running"] = server.poll() is None
    if ev["echoed"] != WS_PAYLOAD:
        raise RuntimeError(f"echo mismatch: {ev['echoed']!r}")
    return ev


@step("exec_stdin_stream_exit", "exec: stdin round trip, incremental stdout arrival, nonzero exit code, stderr separation")
def exec_stdin_stream_exit(ctx):
    sb = ctx.sandbox()
    ev = {}
    p = sb.exec("python3", "-c", "import sys; sys.stdout.write(sys.stdin.read().upper())", timeout=60)
    p.stdin.write("ryeos stdin")
    p.stdin.write_eof()
    p.stdin.drain()
    ev["stdin_roundtrip"] = p.stdout.read()
    ev["stdin_exit"] = p.wait()
    p = sb.exec("python3", "-u", "-c",
                "import time\nfor i in range(4):\n print('tick',i,flush=True); time.sleep(1)", timeout=60)
    t0 = time.monotonic()
    arrivals = []
    for line in p.stdout:
        arrivals.append({"line": line.strip(), "at": round(time.monotonic() - t0, 2)})
    ev["stream_arrivals"] = arrivals
    ev["stream_exit"] = p.wait()
    ev["streamed_incrementally"] = len(arrivals) >= 2 and arrivals[-1]["at"] - arrivals[0]["at"] >= 1.5
    p = sb.exec("sh", "-c", "echo out; echo err >&2; exit 7", timeout=60)
    ev["nonzero_stdout"] = p.stdout.read().strip()
    ev["nonzero_stderr"] = p.stderr.read().strip()
    ev["nonzero_exit"] = p.wait()
    if ev["stdin_roundtrip"] != "RYEOS STDIN" or ev["nonzero_exit"] != 7 or not ev["streamed_incrementally"]:
        raise RuntimeError(f"exec behaviour differs from expectation: {json.dumps(ev)[:400]}")
    return ev


@step("lost_stream", "terminate the sandbox during a streaming exec; record the client-side outcome and error type")
def lost_stream(ctx):
    sb = ctx.sandbox()
    p = sb.exec("python3", "-u", "-c",
                "import time\nwhile True:\n print('beat',flush=True); time.sleep(0.5)", timeout=120)
    ev = {"lines_before_loss": 0}
    t0 = time.monotonic()

    def drain():
        try:
            for _ in p.stdout:
                ev["lines_before_loss"] += 1
            ev["iteration"] = "ended_without_exception"
        except Exception as e:
            ev["iteration"] = "raised"
            ev["error_type"] = f"{type(e).__module__}.{type(e).__name__}"
            ev["error"] = str(e)[:200]
        ev["seconds_to_loss"] = round(time.monotonic() - t0, 2)
        try:
            ev["wait_returncode"] = p.wait()
        except Exception as e:
            ev["wait_error_type"] = f"{type(e).__module__}.{type(e).__name__}"

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    time.sleep(3.0)
    sb.terminate()
    reader.join(timeout=45)
    if reader.is_alive():
        ev["hung_after_terminate"] = True
        raise RuntimeError(f"stream neither ended nor raised 45s after terminate: {json.dumps(ev)}")
    return ev


@step("file_write", "write a file through exec stdin, read it back through exec cat, and hash it inside the sandbox")
def file_write(ctx):
    sb = ctx.sandbox()
    data = "ryeos-e0m file\n" * 64
    write_file(sb, "/tmp/e0m.txt", data)
    back = read_file(sb, "/tmp/e0m.txt")
    code, out, _ = run_py(sb, "import hashlib;print(hashlib.sha256(open('/tmp/e0m.txt','rb').read()).hexdigest())")
    want = hashlib.sha256(data.encode()).hexdigest()
    ev = {"bytes": len(data), "readback_equal": back == data, "guest_sha256": out.strip(), "expected_sha256": want,
          "write_path": "exec stdin to sh cat; Sandbox.open/ls/mkdir/rm share one file API the server refuses (UnimplementedError), so none is used"}
    if not ev["readback_equal"] or ev["guest_sha256"] != want:
        raise RuntimeError("file content differs after write")
    return ev


@step("guest_facts", "collect kernel, user, cgroup, memory, cpu, mounts and Modal env names from inside the guest")
def guest_facts(ctx):
    sb = ctx.sandbox()
    cmds = {
        "uname": "uname -srm",
        "os_release": "grep -E '^(ID|VERSION_ID)=' /etc/os-release | tr '\\n' ' '",
        "id": "id",
        "nproc": "nproc",
        "mem_total": "grep MemTotal /proc/meminfo",
        "cgroup": "cat /proc/self/cgroup | head -3",
        "root_mount": "grep ' / ' /proc/self/mountinfo | head -1 | cut -d' ' -f5-9",
        "df_root": "df -P / | tail -1",
        "python": "python3 -V",
        "modal_env_names": "env | cut -d= -f1 | grep -i '^MODAL' | sort | tr '\\n' ' '",
        "cap_eff": "grep CapEff /proc/self/status",
        "clock": "date -u +%s",
    }
    ev = {}
    for key, cmd in cmds.items():
        p = sb.exec("sh", "-c", cmd, timeout=60)
        ev[key] = p.stdout.read().strip()
        p.wait()
    ev["local_clock"] = int(time.time())
    return ev


@step("terminate_observe", "terminate once, then observe the terminal state by poll, wait and a second terminate")
def terminate_observe(ctx):
    sb = ctx.sandbox()
    ev = {"poll_before": sb.poll()}
    t0 = time.monotonic()
    sb.terminate()
    rc = None
    for _ in range(60):
        rc = sb.poll()
        if rc is not None:
            break
        time.sleep(1)
    ev["seconds_to_terminal"] = round(time.monotonic() - t0, 2)
    ev["terminal_returncode"] = rc
    try:
        sb.wait(raise_on_termination=True)
        ev["wait_raises"] = None
    except BaseException as e:
        ev["wait_raises"] = f"{type(e).__module__}.{type(e).__name__}"
    try:
        sb.terminate()
        ev["second_terminate"] = "noop"
    except BaseException as e:
        ev["second_terminate"] = f"{type(e).__name__}: {str(e)[:120]}"
    ev["poll_after"] = sb.poll()
    if rc is None:
        raise RuntimeError("no terminal state observed within 60s")
    return ev


@step("timeout_exit", "a sandbox with a short lifetime ends by itself; record how wait and poll report it")
def timeout_exit(ctx):
    sb = ctx.sandbox(timeout=10)
    t0 = time.monotonic()
    ev = {}
    try:
        sb.wait()
        ev["wait"] = "returned"
    except BaseException as e:
        ev["wait"] = f"{type(e).__module__}.{type(e).__name__}"
    ev["seconds_to_end"] = round(time.monotonic() - t0, 2)
    ev["returncode"] = sb.poll()
    if ev["returncode"] is None:
        raise RuntimeError("sandbox still running after wait returned")
    return ev


@step("name_uniqueness", "create two sandboxes with the same name in one app; record whether Modal refuses, and by-name lookup")
def name_uniqueness(ctx):
    name = f"e0m-{ctx.run_id}"
    first = ctx.sandbox(name=name)
    ev = {"name": name}
    try:
        ctx.sandbox(name=name)
        ev["second_create"] = "accepted"
    except Exception as e:
        ev["second_create"] = "refused"
        ev["error_type"] = f"{type(e).__module__}.{type(e).__name__}"
        ev["error"] = str(e)[:200]
    try:
        found = ctx.modal.Sandbox.from_name(ctx.app_name, name)
        ev["from_name_matches_first"] = found.object_id == first.object_id
    except Exception as e:
        ev["from_name_error"] = f"{type(e).__module__}.{type(e).__name__}"
    return ev


@step("gpu_t4", "optional: T4 sandbox reports the device", gpu=True)
def gpu_t4(ctx):
    sb = ctx.sandbox(gpu="T4")
    p = sb.exec("nvidia-smi", "-L")
    return {"devices": p.stdout.read().strip(), "exit": p.wait()}


# ------------------------------------------------------- launcher group

MIB = 1024 * 1024
STAGE_DIR = "/ryeos/stage"
STAGE_SLOT = "/ryeos/stage/slot"
APPEND_SCRIPT = 'mkdir -p /ryeos/stage && cat >> "$1"'
APPEND_B64_SCRIPT = 'mkdir -p /ryeos/stage && base64 -d >> "$1"'
STDIN_PIECE = MIB
LARGE_TOTAL = 1024 * MIB
LARGE_CHUNK = 32 * MIB
LARGE_DEADLINE = 600.0
LONG_STREAM_TICKS = 15
LONG_STREAM_PERIOD = 10
TLS_HOST = "example.com"
OFF_LIST_ADDRS = ("1.1.1.1", "8.8.8.8")


def chunk_plan(total, chunk):
    """(offset, size) pairs covering total bytes in chunk-sized pieces."""
    if total < 0 or chunk <= 0:
        raise ValueError("total must be >= 0 and chunk > 0")
    return [(off, min(chunk, total - off)) for off in range(0, total, chunk)]


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def mb_per_s(nbytes, seconds):
    return round(nbytes / MIB / seconds, 2) if seconds > 0 else None


class Deadline:
    """A hard client-side time bound; the clock is injectable for tests."""

    def __init__(self, seconds, clock=time.monotonic):
        self.clock = clock
        self.end = clock() + seconds

    def remaining(self):
        return self.end - self.clock()

    def expired(self):
        return self.remaining() <= 0


def exec_run(sb, *argv, stdin=None, timeout=60):
    """Run a fixed argv in the sandbox; return (exit, stdout, stderr)."""
    p = sb.exec(*argv, timeout=timeout)
    if stdin is not None:
        for off in range(0, len(stdin), STDIN_PIECE):
            p.stdin.write(stdin[off:off + STDIN_PIECE])
            p.stdin.drain()
        p.stdin.write_eof()
        p.stdin.drain()
    out = p.stdout.read()
    err = p.stderr.read()
    return p.wait(), out, err


def attempt(sb, *argv, timeout=60):
    """Record one command outcome verbatim; an SDK error is recorded, not raised."""
    rec = {"argv": list(argv)}
    try:
        code, out, err = exec_run(sb, *argv, timeout=timeout)
        rec.update({"exit": code, "stdout": out.strip()[-400:], "stderr": err.strip()[-400:]})
    except Exception as e:
        rec["error"] = f"{type(e).__module__}.{type(e).__name__}: {str(e)[:200]}"
    return rec


def guest_size(sb, path):
    code, out, err = exec_run(sb, "stat", "-c", "%s", path)
    if code != 0:
        raise RuntimeError(f"stat {path} exit {code}: {err.strip()[-200:]}")
    return int(out.strip())


def guest_sha256(sb, path):
    code, out, err = exec_run(sb, "sha256sum", path, timeout=300)
    if code != 0:
        raise RuntimeError(f"sha256sum {path} exit {code}: {err.strip()[-200:]}")
    return out.split()[0]


def append_chunk(sb, data, encoding="raw", timeout=300):
    """Append bytes to the stage slot through one exec process."""
    if encoding == "raw":
        script, payload = APPEND_SCRIPT, data
    else:
        script, payload = APPEND_B64_SCRIPT, base64.b64encode(data)
    code, _, err = exec_run(sb, "sh", "-c", script, "sh", STAGE_SLOT, stdin=payload, timeout=timeout)
    if code != 0:
        raise RuntimeError(f"append exit {code}: {err.strip()[-200:]}")


@step("exec_append_binary", "append 32 MiB of random bytes through one exec stdin process, hash inside the sandbox, compare", group="launcher")
def exec_append_binary(ctx):
    data = os.urandom(32 * MIB)
    want = sha256_hex(data)
    ev = {"bytes": len(data), "client_sha256": want, "attempts": []}
    with ctx.scratch(timeout=600) as sb:
        for encoding in ("raw", "base64"):
            exec_run(sb, "rm", "-f", STAGE_SLOT)
            t0 = time.monotonic()
            rec = {"encoding": encoding}
            try:
                append_chunk(sb, data, encoding)
                rec["seconds"] = round(time.monotonic() - t0, 2)
                rec["mb_per_s"] = mb_per_s(len(data), time.monotonic() - t0)
                rec["guest_size"] = guest_size(sb, STAGE_SLOT)
                rec["guest_sha256"] = guest_sha256(sb, STAGE_SLOT)
                rec["match"] = rec["guest_sha256"] == want and rec["guest_size"] == len(data)
            except Exception as e:
                rec["error"] = f"{type(e).__name__}: {str(e)[:200]}"
                rec["match"] = False
            ev["attempts"].append(rec)
            if rec["match"]:
                ev["working_encoding"] = encoding
                break
    if "working_encoding" not in ev:
        raise RuntimeError(f"no encoding delivered the bytes intact: {json.dumps(ev)[:400]}")
    return ev


@step("staged_large_write", "append 1 GiB in 32 MiB chunks through separate exec processes, size-checked before each chunk, under a hard deadline", group="launcher")
def staged_large_write(ctx):
    block = os.urandom(LARGE_CHUNK)
    deadline = Deadline(LARGE_DEADLINE)
    ev = {"total_bytes": LARGE_TOTAL, "chunk_bytes": LARGE_CHUNK, "deadline_seconds": LARGE_DEADLINE,
          "chunks_done": 0, "bytes_done": 0, "completed": False}
    t0 = time.monotonic()
    with ctx.scratch(timeout=int(LARGE_DEADLINE) + 300) as sb:
        try:
            exec_run(sb, "rm", "-f", STAGE_SLOT)
            for off, size in chunk_plan(LARGE_TOTAL, LARGE_CHUNK):
                if deadline.expired():
                    ev["deadline_hit"] = True
                    break
                have = guest_size(sb, STAGE_SLOT) if off else 0
                if have != off:
                    raise RuntimeError(f"offset mismatch before chunk at {off}: guest has {have}")
                append_chunk(sb, block[:size], timeout=max(1, int(deadline.remaining())))
                ev["chunks_done"] += 1
                ev["bytes_done"] = off + size
            else:
                ev["completed"] = guest_size(sb, STAGE_SLOT) == LARGE_TOTAL
        except Exception as e:
            ev["error"] = f"{type(e).__name__}: {str(e)[:300]}"
            ev["completed"] = False
        ev["seconds"] = round(time.monotonic() - t0, 2)
        ev["mb_per_s"] = mb_per_s(ev["bytes_done"], time.monotonic() - t0)
        try:
            ev["final_size"] = guest_size(sb, STAGE_SLOT)
        except Exception as e:
            ev["final_size_error"] = f"{type(e).__name__}: {str(e)[:200]}"
        try:
            ev["df_after"] = exec_run(sb, "df", "-P", STAGE_DIR)[1].strip()
        except Exception as e:
            ev["df_after_error"] = f"{type(e).__name__}: {str(e)[:200]}"
    return ev


@step("tag_limits", "set tags of growing value length and count; record which the server accepts", group="launcher")
def tag_limits(ctx):
    ev = {"value_lengths": [], "counts": []}
    base = {TAG_KEY: ctx.run_id}
    with ctx.scratch() as sb:
        def try_tags(extra):
            try:
                sb.set_tags({**base, **extra})
                return {"accepted": True}
            except Exception as e:
                return {"accepted": False, "error_type": f"{type(e).__module__}.{type(e).__name__}",
                        "error": str(e)[:200]}
        try:
            for n in (64, 256, 1024):
                ev["value_lengths"].append({"length": n, **try_tags({"ryeos_stage": "v" * n})})
            for n in (1, 2, 4, 8):
                ev["counts"].append({"count": n, **try_tags({f"ryeos_stage_{i}": "x" for i in range(n)})})
        finally:
            sb.set_tags(base)
    return ev


@step("account_switch", "as root in debian_slim: user creation tools, create uid 10000, drop to it with setpriv or a fallback", group="launcher")
def account_switch(ctx):
    ev = {}
    with ctx.scratch() as sb:
        ev["id"] = attempt(sb, "id")
        ev["tools"] = attempt(sb, "sh", "-c", "command -v useradd adduser setpriv runuser su")
        ev["useradd"] = attempt(sb, "useradd", "--uid", "10000", "--create-home", "--shell", "/bin/sh", "ryeos")
        ev["ryeos_entry"] = attempt(sb, "getent", "passwd", "ryeos")
        ev["setpriv"] = attempt(sb, "setpriv", "--reuid=10000", "--regid=10000", "--clear-groups", "--no-new-privs", "id")
        if ev["setpriv"].get("exit") != 0:
            ev["runuser"] = attempt(sb, "runuser", "-u", "ryeos", "--", "id")
            if ev["runuser"].get("exit") != 0:
                ev["su"] = attempt(sb, "su", "ryeos", "-s", "/bin/sh", "-c", "id")
    return ev


@step("glibc_and_namespaces", "glibc version, kernel release, and whether unshare and tmpfs mounts work under the sandbox runtime", group="launcher")
def glibc_and_namespaces(ctx):
    ev = {}
    with ctx.scratch() as sb:
        ldd = attempt(sb, "ldd", "--version")
        ldd["first_line"] = ldd.get("stdout", "").split("\n")[0]
        ev["ldd"] = ldd
        ev["kernel"] = attempt(sb, "uname", "-r")
        ev["unshare_user_mount"] = attempt(sb, "unshare", "-Urm", "true")
        ev["unshare_mount"] = attempt(sb, "unshare", "-m", "true")
        ev["unshare_pid"] = attempt(sb, "unshare", "-p", "-f", "true")
        ev["mount_tmpfs"] = attempt(sb, "mount", "-t", "tmpfs", "none", "/mnt")
    return ev


TLS_DIAL_CODE = r"""
import json, socket, ssl, sys
ip, host, other = sys.argv[1:4]
out = {}
ctx = ssl.create_default_context()
raw = socket.create_connection((ip, 443), timeout=15)
tls = ctx.wrap_socket(raw, server_hostname=host)
cert = tls.getpeercert()
flat = lambda rdn: {k: v for p in rdn for k, v in p}
out["dial"] = {"verified": True, "subject": flat(cert["subject"]), "issuer": flat(cert["issuer"]),
               "san": [v for _, v in cert.get("subjectAltName", ())][:4], "tls_version": tls.version()}
tls.close()
try:
    socket.create_connection((other, 443), timeout=8).close()
    out["other"] = {"connected": True}
except Exception as e:
    out["other"] = {"connected": False, "error": type(e).__name__}
print(json.dumps(out))
"""


def pick_off_list(ip):
    """A well-known address that is not the allowed one."""
    return next(a for a in OFF_LIST_ADDRS if a != ip)


@step("tls_dial_by_ip", "cidr_allowlist of one /32: TLS to that IP with SNI set to the hostname verifies; a different IP times out", group="launcher")
def tls_dial_by_ip(ctx):
    ip = socket.getaddrinfo(TLS_HOST, 443, socket.AF_INET, socket.SOCK_STREAM)[0][4][0]
    other = pick_off_list(ip)
    ev = {"host": TLS_HOST, "allowed_ip": ip, "other_ip": other}
    ctx.ensure_app()
    with ctx.scratch(cidr_allowlist=[f"{ip}/32"]) as sb:
        p = sb.exec("python3", "-c", TLS_DIAL_CODE, ip, TLS_HOST, other, timeout=90)
        out, err = p.stdout.read(), p.stderr.read()
        ev.update(json_from_guest(p.wait(), out, err))
    return ev


LONG_STREAM_CODE = ("import time\nfor i in range(%d):\n print('tick', i, flush=True)\n time.sleep(%d)\n"
                    % (LONG_STREAM_TICKS, LONG_STREAM_PERIOD))


@step("long_exec_stream", "one exec printing every 10 s for 150 s: does the stdout stream survive past 120 s", group="launcher")
def long_exec_stream(ctx):
    expected = LONG_STREAM_TICKS
    ev = {"expected_lines": expected, "arrivals": []}
    with ctx.scratch(timeout=400) as sb:
        p = sb.exec("python3", "-u", "-c", LONG_STREAM_CODE, timeout=300)
        t0 = time.monotonic()
        try:
            for line in p.stdout:
                ev["arrivals"].append({"line": line.strip(), "at": round(time.monotonic() - t0, 1)})
        except Exception as e:
            ev["stream_error"] = f"{type(e).__module__}.{type(e).__name__}: {str(e)[:200]}"
        ev["exit"] = p.wait()
    last = ev["arrivals"][-1]["at"] if ev["arrivals"] else 0
    ev["last_arrival"] = last
    ev["survived_past_120s"] = last > 120 and "stream_error" not in ev
    ev["complete"] = len(ev["arrivals"]) == expected and ev["exit"] == 0
    return ev


# ------------------------------------------------------------------- core

def plan_for(gpu, only, group="e0m"):
    """Ordered step descriptors the run will execute (cleanup is added by the runner).

    `group` is one of GROUPS or "all"; naming steps with `only` selects from every group.
    """
    if group != "all" and group not in GROUPS:
        raise SystemExit(f"unknown group: {group}")
    chosen = [s for s in STEPS if gpu or not s["gpu"]]
    if group != "all" and not only:
        chosen = [s for s in chosen if s["group"] == group]
    if only:
        known = {s["name"] for s in STEPS}
        unknown = sorted(set(only) - known)
        if unknown:
            raise SystemExit(f"unknown step(s): {', '.join(unknown)}")
        chosen = [s for s in chosen if s["name"] in only]
    return chosen


def run_step(name, fn, ctx):
    """Run one step; never raises. Always returns a full record."""
    t0 = time.monotonic()
    rec = {"name": name, "ok": False, "evidence": None, "error": None, "seconds": 0.0}
    try:
        rec["evidence"] = fn(ctx)
        rec["ok"] = True
    except Exception as e:
        rec["error"] = f"{type(e).__name__}: {e}"
        rec["evidence"] = {"traceback": traceback.format_exc()[-1500:]}
    rec["seconds"] = round(time.monotonic() - t0, 2)
    return rec


def cleanup_step(ctx):
    """Terminate everything created, then assert the run tag lists nothing."""
    ev = {"terminated": 0, "errors": []}
    for sb in ctx.created:
        try:
            sb.terminate()
            ev["terminated"] += 1
        except Exception as e:
            ev["errors"].append(f"{type(e).__name__}: {str(e)[:120]}")
    remaining = []
    for _ in range(30):
        remaining = []
        for s in ctx.modal.Sandbox.list(tags={TAG_KEY: ctx.run_id}):
            remaining.append(s.object_id)
            try:
                s.terminate()
            except Exception as e:
                ev["errors"].append(f"{type(e).__name__}: {str(e)[:120]}")
        if not remaining:
            break
        time.sleep(2)
    ev["created"] = len(ctx.created)
    ev["remaining_by_tag"] = remaining
    if remaining or ev["errors"]:
        raise RuntimeError(f"cleanup incomplete: {len(remaining)} remain, {len(ev['errors'])} errors")
    return ev


def exit_code(records):
    return 1 if any(not r["ok"] for r in records) else 0


def build_evidence(run_id, args, modal_version, plan, records, dry_run):
    return {
        "run_id": run_id,
        "app": args.app,
        "gpu": bool(args.gpu),
        "group": args.group,
        "keep": bool(args.keep),
        "dry_run": dry_run,
        "modal_version": modal_version,
        "plan": [{"name": s["name"], "description": s["description"], "gpu": s["gpu"], "group": s["group"]} for s in plan],
        "steps": records,
        "ok": (not dry_run) and exit_code(records) == 0,
    }


def write_evidence(out_dir, evidence):
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    path = out / EVIDENCE_NAME
    path.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    return path


def parse_args(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--app", default="ryeos-e0m")
    ap.add_argument("--out", default=".")
    ap.add_argument("--gpu", action="store_true", help="also run the T4 step")
    ap.add_argument("--keep", action="store_true", help="skip cleanup")
    ap.add_argument("--dry-run", action="store_true", help="write the plan without contacting Modal")
    ap.add_argument("--group", default="e0m", choices=list(GROUPS) + ["all"],
                    help="step group: e0m (default), launcher, or all")
    ap.add_argument("--only", action="append", default=[], metavar="STEP", help="run only this step (repeatable)")
    return ap.parse_args(argv)


def main(argv=None):
    args = parse_args(sys.argv[1:] if argv is None else argv)
    run_id = uuid.uuid4().hex[:12]
    plan = plan_for(args.gpu, args.only, args.group)
    if args.dry_run:
        path = write_evidence(args.out, build_evidence(run_id, args, None, plan, [], True))
        for s in plan:
            print(f"PLAN {s['name']}: {s['description']}")
        print(f"dry run, evidence plan written to {path}")
        return 0
    modal = load_modal()
    ctx = Ctx(modal, args.app, run_id, args.gpu)
    records = []

    def report(rec):
        print(f"{'PASS' if rec['ok'] else 'FAIL'} {rec['name']} ({rec['seconds']}s)"
              + ("" if rec["ok"] else f" {rec['error']}"), flush=True)

    try:
        for s in plan:
            records.append(run_step(s["name"], s["fn"], ctx))
            report(records[-1])
    except BaseException as e:
        records.append({"name": "interrupted", "ok": False, "evidence": None,
                        "error": f"{type(e).__name__}", "seconds": 0.0})
        print(f"FAIL interrupted ({type(e).__name__}); cleaning up", flush=True)
    finally:
        if args.keep:
            print(f"KEEP cleanup skipped; tag {TAG_KEY}={run_id}")
        else:
            records.append(run_step("cleanup", cleanup_step, ctx))
            report(records[-1])
        path = write_evidence(args.out, build_evidence(run_id, args, getattr(modal, "__version__", None), plan, records, False))
        print(f"evidence: {path}")
    return exit_code(records)


if __name__ == "__main__":
    sys.exit(main())
