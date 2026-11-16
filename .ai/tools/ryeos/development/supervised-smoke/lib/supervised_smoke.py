# ryeos:signed:2026-11-16T08:10:46Z:9c88daace1f3ff2519f559fbcd30b799c2f7c55e53d151bcf196690e457ced45:rUzH7XLPR/MhyL5NZXmbWb0c3nyhYHtgmc608b3bsSVwblBARZOzEmIcx83t4LczCwd6n6LB+1Yw3rtKUjELCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Supervised-node smoke: read-mostly checks against a RUNNING supervised node.

CI never runs a supervised node (host-setup, runit, enforced isolation, an
empty daemon environment), so defects that only show there stay invisible
until a real node hits them. This helper drives the node's own `ryeos` CLI
through the paths those defects broke and prints a pass/fail table. It is
external evidence and grants no RyeOS authority; run it as the node's
configured operator:

    python3 -I supervised_smoke.py --app-root ~/.local/share/ryeos

Every check is independent: a failure never aborts the rest. Exit 0 when no
check FAILs (WARN and SKIP do not fail), 1 when any does, 2 on bad usage.
Nothing secret is read or printed; command output is redacted and truncated
before it reaches the table.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

PASS, FAIL, WARN, SKIP = "PASS", "FAIL", "WARN", "SKIP"

HTTPS_URL = "https://example.com"
DNS_NAME = "example.com"
CODE_LIVE = "live_project_unavailable_to_sourced_runtime"

FIXTURE_MANIFEST = 'name: supervised-smoke\nversion: "0.1.0"\n'

PY_PROBE = f'''#!/usr/bin/env python3
# ryeos-tool:
#   category: "smoke"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Supervised-node smoke: python runtime probe"

import os
import socket
import sys
import urllib.request

project = sys.argv[sys.argv.index("--project-path") + 1]


def attempt(fn):
    try:
        return "ok" if fn() else "fail"
    except Exception:
        return "fail"


def read():
    with open(os.path.join(project, "smoke-data.txt")) as handle:
        return handle.read().strip() == "smoke-data-v1"


def write():
    path = os.path.join(project, "written-by-probe.txt")
    with open(path, "w") as handle:
        handle.write("probe-wrote")
    with open(path) as handle:
        return handle.read() == "probe-wrote"


def https():
    with urllib.request.urlopen("{HTTPS_URL}", timeout=20) as reply:
        return 200 <= reply.status < 400


print(
    "SMOKE_PY"
    " interp=" + sys.executable
    + " path=" + (os.environ.get("PATH") or "unset")
    + " read=" + attempt(read)
    + " write=" + attempt(write)
    + " dns=" + attempt(lambda: socket.getaddrinfo("{DNS_NAME}", 443))
    + " https=" + attempt(https)
)
'''

SH_PROBE = f'''#!/usr/bin/env bash
# ryeos-tool:
#   category: "smoke"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/bash/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Supervised-node smoke: bash runtime probe"

project=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--project-path" ]; then project="$2"; fi
  shift
done

ok_or_fail() {{ if "$@" >/dev/null 2>&1; then printf ok; else printf fail; fi; }}
read_data() {{ [ "$(cat "$project/smoke-data.txt")" = "smoke-data-v1" ]; }}
write_data() {{
  printf 'probe-wrote' > "$project/written-by-probe.txt" &&
    [ "$(cat "$project/written-by-probe.txt")" = "probe-wrote" ]
}}
https_ok() {{ curl -fsS -o /dev/null --max-time 20 "{HTTPS_URL}"; }}

printf 'SMOKE_SH path=%s read=%s write=%s dns=%s https=%s curl=%s\\n' \\
  "${{PATH:-unset}}" \\
  "$(ok_or_fail read_data)" \\
  "$(ok_or_fail write_data)" \\
  "$(ok_or_fail getent hosts {DNS_NAME})" \\
  "$(ok_or_fail https_ok)" \\
  "$(command -v curl >/dev/null 2>&1 && echo yes || echo no)"
'''

# The signed manifest-directives items the graph and directive checks need.
FIXTURE_REFS = [
    "graph:test/anchor_chain",
    "tool:test/anchor_demo/anchor_demo",
    "tool:test/fail_demo/fail_demo",
    "directive:test/continuation/force_continue",
]

_SECRETISH = re.compile(
    r"(sk-[A-Za-z0-9_\-]{12,}|[Bb]earer\s+[A-Za-z0-9._\-]{12,}"
    r"|(?i:(?:key|token|secret|password)[\"']?\s*[:=]\s*[\"']?)[A-Za-z0-9+/_\-]{12,})"
)


def redact(text: str, limit: int = 400) -> str:
    text = _SECRETISH.sub("<redacted>", text or "")
    text = " ".join(text.split())
    return text if len(text) <= limit else text[:limit] + "..."


class Result:
    def __init__(self, name: str, status: str, detail: str = ""):
        self.name, self.status, self.detail = name, status, detail


class Smoke:
    def __init__(self, args, run=None, now=time.time, sleep=time.sleep):
        self.args = args
        self.results: list[Result] = []
        self._run = run or self._subprocess_run
        self._now, self._sleep = now, sleep
        self.env = dict(os.environ)
        self.env["RYEOS_APP_ROOT"] = str(args.app_root)

    # -- plumbing -----------------------------------------------------------

    def _subprocess_run(self, argv, cwd=None, timeout=300):
        try:
            done = subprocess.run(
                argv, cwd=cwd, env=self.env, capture_output=True, text=True,
                timeout=timeout, stdin=subprocess.DEVNULL,
            )
            return done.returncode, done.stdout, done.stderr
        except subprocess.TimeoutExpired:
            return 124, "", f"timed out after {timeout}s"
        except OSError as error:
            return 127, "", str(error)

    def ryeos(self, *words, cwd=None, timeout=300):
        return self._run([self.args.ryeos_bin, *words], cwd=cwd, timeout=timeout)

    def record(self, name, status, detail=""):
        self.results.append(Result(name, status, detail))

    def check(self, name, fn):
        """Run one independent check; an exception is that check's FAIL."""
        try:
            outcome = fn()
        except Exception as error:  # noqa: BLE001 - a check never aborts the run
            outcome = (FAIL, f"check crashed: {type(error).__name__}: {redact(str(error))}")
        if outcome is not None:
            self.record(name, *outcome)

    # -- node ---------------------------------------------------------------

    def node_status(self):
        rc, out, err = self.ryeos("node", "status", "--json", "--app-root", str(self.args.app_root), timeout=60)
        report = parse_json(out)
        if rc != 0 or report is None:
            return FAIL, f"rc={rc} {redact(out or err)}"
        if not has_key(report, "Running"):
            return FAIL, f"node is not running: {redact(out)}"
        window = find_key(report, "service_window")
        if find_key(report, "ready_at"):
            note = f"service window {json.dumps(window)}" if window else "ready"
            return PASS, redact(note, 160)
        return FAIL, "running but no ready_at in status"

    def node_doctor(self):
        rc, out, err = self.ryeos("node", "doctor", "--json", "--app-root", str(self.args.app_root), timeout=180)
        report = parse_json(out)
        if report is None or "checks" not in report:
            return FAIL, f"rc={rc} no doctor report: {redact(out or err)}"
        rows = {row.get("name"): row for row in report["checks"]}
        failed = [n for n, r in rows.items() if r.get("status") == "fail"]
        warned = [n for n, r in rows.items() if r.get("status") == "warning"]
        results = []
        if failed or rc != 0:
            results.append(Result("node doctor", FAIL, f"{len(rows)} checks, failing: {', '.join(failed) or 'exit ' + str(rc)}"))
        else:
            results.append(Result("node doctor", PASS, f"{len(rows)} checks"))
        for name in warned:
            results.append(Result(f"doctor warning: {name}", WARN, redact(json.dumps(rows[name].get("detail")), 240)))
        missing = [n for n in ("host_services", "isolation", "capabilities") if n not in rows]
        bad = [n for n in ("host_services", "isolation", "capabilities") if rows.get(n, {}).get("status") in ("fail", "n/a", "unknown")]
        if missing or bad:
            results.append(Result("doctor host-service and scope rows", FAIL,
                                  f"missing: {missing or '-'}; not ok: {bad or '-'}"))
        else:
            results.append(Result("doctor host-service and scope rows", PASS,
                                  "host_services, isolation, capabilities present"))
        self.results.extend(results)
        return None

    # -- fixture ------------------------------------------------------------

    def make_fixture(self, scratch: Path):
        """Write the probe project and sign it with the node operator's key."""
        project = scratch / "probe-project"
        tools = project / ".ai/tools/smoke"
        tools.mkdir(parents=True)
        (project / ".ai/manifest.source.yaml").write_text(FIXTURE_MANIFEST)
        (project / "smoke-data.txt").write_text("smoke-data-v1\n")
        (tools / "py_probe.py").write_text(PY_PROBE)
        sh = tools / "sh_probe.sh"
        sh.write_text(SH_PROBE)
        sh.chmod(0o755)
        refs = ["tool:smoke/py_probe", "tool:smoke/sh_probe"]
        rc, out, err = self.ryeos("sign", *refs, "--project", str(project), cwd=project, timeout=120)
        if rc != 0:
            raise RuntimeError(f"ryeos sign of the probe fixture failed rc={rc}: {redact(err or out)}")
        return project

    def runtime_probe(self, project: Path, ref: str, marker: str):
        rc, out, err = self.ryeos("execute", ref, "--project", str(project), "--pin-project",
                                  cwd=project, timeout=300)
        text = (out or "") + "\n" + (err or "")
        tokens = parse_tokens(text, marker)
        return rc, tokens, text

    def runtime_checks(self, scratch: Path):
        try:
            project = self.make_fixture(scratch)
        except Exception as error:  # noqa: BLE001
            for name in ("python runtime tool runs", "bash runtime tool runs",
                         "DNS reachable from sandbox", "HTTPS reachable from sandbox",
                         "--pin-project read/write project data"):
                self.record(name, FAIL, f"fixture unavailable: {redact(str(error))}")
            return None
        probes = {}
        for label, ref, marker in (("python", "tool:smoke/py_probe", "SMOKE_PY"),
                                   ("bash", "tool:smoke/sh_probe", "SMOKE_SH")):
            rc, tokens, text = self.runtime_probe(project, ref, marker)
            probes[label] = (rc, tokens, text)
            if rc == 0 and tokens is not None:
                path = tokens.get("path", "unset")
                interp = tokens.get("interp")
                note = f"interpreter ran; PATH={path}" + (f" interp={interp}" if interp else "")
                status = PASS if path != "unset" or label == "python" else WARN
                self.record(f"{label} runtime tool runs", status, redact(note, 240))
            else:
                self.record(f"{label} runtime tool runs", FAIL, f"rc={rc} {redact(text)}")

        def combined(field, what):
            bad = [lbl for lbl, (rc, t, _) in probes.items() if rc != 0 or t is None or t.get(field) != "ok"]
            if bad:
                return FAIL, f"{what} not ok in: {', '.join(bad)}"
            return PASS, "python and bash"

        self.check("DNS reachable from sandbox", lambda: combined("dns", "DNS"))
        self.check("HTTPS reachable from sandbox", lambda: combined("https", "HTTPS"))
        self.check("--pin-project read/write project data",
                   lambda: self.pin_result(probes))
        return project

    def pin_result(self, probes):
        bad = []
        for lbl, (rc, tokens, _) in probes.items():
            if rc != 0 or tokens is None or tokens.get("read") != "ok" or tokens.get("write") != "ok":
                bad.append(lbl)
        if bad:
            return FAIL, f"pinned read/write failed in: {', '.join(bad)}"
        return PASS, "pinned copy-on-write project readable and writable (python, bash)"

    # -- scheduled live fire -------------------------------------------------

    def live_fire_refusal(self, project: Path):
        schedule_id = f"smoke-live-{uuid.uuid4().hex[:8]}"
        fire_at = datetime.datetime.fromtimestamp(self._now() + 12, datetime.timezone.utc)
        spec = {
            "schedule_id": schedule_id,
            "item_ref": "tool:smoke/py_probe",
            "ref_bindings": {},
            "schedule_type": "at",
            "expression": fire_at.strftime("%Y-%m-%dT%H:%M:%S.000Z"),
            "params": {},
            "timezone": "UTC",
            "misfire_policy": "skip",
            "overlap_policy": "skip",
            "lateness_grace_secs": 120,
            "enabled": True,
            "max_fires": 1,
            "project_root": str(project),
            "capabilities": ["ryeos.execute.tool.smoke/py_probe", "ryeos.write.project.live"],
            "execution_policy": {
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "accepted",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {"kind": "live_direct", "access": "read_write",
                            "child_policy": {"kind": "inherit"}},
            },
        }
        registered = False
        try:
            rc, out, err = self.ryeos("scheduler", "register", json.dumps(spec), cwd=project, timeout=60)
            registered = True
            if rc != 0:
                return FAIL, f"register rc={rc}: {redact(err or out)}"
            deadline = self._now() + 120
            evidence = None
            while self._now() < deadline:
                rc, out, err = self.ryeos("scheduler", "show", "fires", schedule_id, cwd=project, timeout=60)
                fires = find_key(parse_json(out), "fires") or []
                done = [f for f in fires if isinstance(f, dict) and f.get("status") in ("failed", "completed")]
                if done:
                    evidence = json.dumps(done[0])
                    break
                self._sleep(2)
            if evidence is None:
                return FAIL, "the scheduled fire never reached a terminal state in 120s"
            wrote = (project / "written-by-probe.txt").exists()
            if wrote:
                return FAIL, "the live fire ran and wrote into the live project"
            if "launch_preparation_failed" in evidence or CODE_LIVE in evidence:
                return PASS, f"fire refused before the tool ran ({CODE_LIVE}); live project untouched"
            return FAIL, f"fire ended without the launch refusal: {redact(evidence)}"
        finally:
            if registered:
                rc, out, err = self.ryeos("scheduler", "deregister", schedule_id, cwd=project, timeout=60)
                if rc != 0:
                    self.record("schedule cleanup", FAIL,
                                f"could not deregister {schedule_id}: {redact(err or out)}")

    # -- manual-directives --------------------------------------------------

    def prepare_manual(self, scratch: Path):
        source = self.args.manual_directives
        if not source or not Path(source, ".ai").is_dir():
            raise RuntimeError("tests/e2e/manual-directives not found; pass --manual-directives DIR")
        project = scratch / "manual-directives"
        shutil.copytree(Path(source) / ".ai", project / ".ai")
        rc, out, err = self.ryeos("sign", *FIXTURE_REFS, "--project", str(project), cwd=project, timeout=180)
        if rc != 0:
            raise RuntimeError(f"ryeos sign of the manual-directives items failed rc={rc}: {redact(err or out)}")
        return project

    def graph_events_text(self) -> str:
        """The step notes live in the graph thread's events, not in the execute result: replay
        the newest anchor_chain thread's events."""
        rc, out, _ = self.ryeos("--no-project", "thread", "list", "--sort", "newest", "--limit", "20",
                               timeout=60)
        if rc != 0:
            return ""
        try:
            listing = json.loads(out)
        except ValueError:
            return ""
        threads = listing.get("threads", listing) if isinstance(listing, dict) else listing
        for thread in threads if isinstance(threads, list) else []:
            if "graph:test/anchor_chain" in json.dumps(thread) and thread.get("thread_id"):
                rc, events, _ = self.ryeos("--no-project", "events", "replay", thread["thread_id"],
                                           timeout=120)
                return events if rc == 0 else ""
        return ""

    def graph_check(self, manual: Path):
        rc, out, err = self.ryeos("execute", "graph:test/anchor_chain", "--project", str(manual),
                                  cwd=manual, timeout=300)
        text = (out or "") + "\n" + (err or "")
        if rc != 0:
            return FAIL, f"rc={rc} {redact(text)}"
        if "retry not attempted" not in text and "retry not attempted" not in self.graph_events_text():
            return FAIL, "graph completed but the flaky step carries no 'retry not attempted' note"
        if "completed" not in text.lower():
            return FAIL, "no completed status in the graph result"
        return PASS, "anchor_chain completed; flaky step carries 'retry not attempted'"

    def paid_check(self, manual: Path):
        rc, out, err = self.ryeos("execute", "directive:test/continuation/force_continue",
                                  "--project", str(manual), cwd=manual, timeout=600)
        text = (out or "") + "\n" + (err or "")
        if "no checkpoint to resume from" in text:
            return FAIL, "continuation failed: predecessor has no checkpoint to resume from"
        if rc != 0:
            return FAIL, f"rc={rc} {redact(text)}"
        if "E2E_FORCE_CONTINUE_READY" not in text:
            return FAIL, "directive finished without E2E_FORCE_CONTINUE_READY"
        if "continued" not in text:
            return WARN, "ran to the marker but no 'continued' predecessor status seen in the output"
        return PASS, "predecessor continued; successor completed with the marker"

    # -- stopped-node standalone ops ----------------------------------------

    def restart_check(self):
        root = str(self.args.app_root)
        try:
            rc, out, err = self.ryeos("stop", "--app-root", root, timeout=120)
            if rc != 0:
                return FAIL, f"stop rc={rc}: {redact(err or out)}"
            rc, out, err = self.ryeos("bundle", "list", timeout=120)
            if rc != 0:
                return FAIL, f"standalone `bundle list` on the stopped node rc={rc}: {redact(err or out)}"
            return PASS, "standalone `bundle list` ran while the node was stopped"
        finally:
            # Always start again: starting a running node succeeds at once.
            rc, out, err = self.ryeos("start", "--app-root", root, timeout=1000)
            if rc != 0:
                self.record("node restart after stop", FAIL, f"start rc={rc}: {redact(err or out)}")
            else:
                self.record("node restart after stop", PASS, "node started again")

    # -- run ---------------------------------------------------------------

    def run_all(self):
        a = self.args
        self.check("node status ready", self.node_status)
        self.check("node doctor", self.node_doctor)
        scratch = Path(tempfile.mkdtemp(prefix="supervised-smoke-"))
        try:
            project = self.runtime_checks(scratch)
            if project is None:
                self.record("scheduled live fire refused", FAIL, "fixture unavailable")
            else:
                self.check("scheduled live fire refused", lambda: self.live_fire_refusal(project))
            manual = None
            try:
                manual = self.prepare_manual(scratch)
            except Exception as error:  # noqa: BLE001
                reason = redact(str(error))
                self.record("graph anchor_chain + retry note", FAIL, reason)
                if a.paid:
                    self.record("directive force_continue (paid)", FAIL, reason)
            if manual is not None:
                self.check("graph anchor_chain + retry note", lambda: self.graph_check(manual))
                if a.paid:
                    self.check("directive force_continue (paid)", lambda: self.paid_check(manual))
            if not a.paid:
                self.record("directive force_continue (paid)", SKIP, "pass --paid (DeepSeek, a few cents)")
            if a.allow_restart:
                self.check("stopped-node standalone op", self.restart_check)
            else:
                self.record("stopped-node standalone op", SKIP, "pass --allow-restart (stops and starts the node)")
        finally:
            if a.keep:
                print(f"scratch kept: {scratch}", file=sys.stderr)
            else:
                shutil.rmtree(scratch, ignore_errors=True)
        return self.results


def parse_json(text: str):
    """The first JSON object or array in `text`, or None."""
    decoder = json.JSONDecoder()
    for index, ch in enumerate(text or ""):
        if ch in "{[":
            try:
                value, _ = decoder.raw_decode(text[index:])
                return value
            except ValueError:
                continue
    return None


_ABSENT = object()


def _dig(value, key):
    if isinstance(value, dict):
        if key in value:
            return value[key]
        children = value.values()
    elif isinstance(value, list):
        children = value
    else:
        return _ABSENT
    for child in children:
        found = _dig(child, key)
        if found is not _ABSENT:
            return found
    return _ABSENT


def find_key(value, key):
    """Depth-first value of `key` anywhere in parsed JSON; None if absent."""
    found = _dig(value, key)
    return None if found is _ABSENT else found


def has_key(value, key) -> bool:
    return _dig(value, key) is not _ABSENT


def parse_tokens(text: str, marker: str):
    """`key=value` tokens of the last `marker` line, or None. The result may be
    JSON-escaped by the CLI, so quotes and backslashes are stripped."""
    match = None
    for match in re.finditer(re.escape(marker) + r"((?: [a-z]+=[^\s\"\\]*)+)", text or ""):
        pass
    if match is None:
        return None
    return dict(pair.split("=", 1) for pair in match.group(1).split())


def render(results) -> str:
    width = max([len(r.name) for r in results] + [5])
    lines = [f"{'CHECK'.ljust(width)}  STATUS  DETAIL", "-" * (width + 40)]
    for r in results:
        lines.append(f"{r.name.ljust(width)}  {r.status.ljust(6)}  {r.detail}")
    counts = {s: sum(1 for r in results if r.status == s) for s in (PASS, FAIL, WARN, SKIP)}
    lines.append("")
    lines.append("  ".join(f"{s}={n}" for s, n in counts.items()))
    return "\n".join(lines)


def default_manual_directives() -> str | None:
    here = Path(__file__).resolve()
    for parent in here.parents:
        candidate = parent / "tests/e2e/manual-directives"
        if (candidate / ".ai").is_dir():
            return str(candidate)
    return None


def main(argv=None, run=None) -> int:
    parser = argparse.ArgumentParser(description="Smoke a RUNNING supervised RyeOS node.")
    parser.add_argument("--app-root", required=True, type=Path, help="the node's app root")
    parser.add_argument("--ryeos-bin", default=None, help="ryeos binary (default: ryeos on PATH)")
    parser.add_argument("--manual-directives", default=None,
                        help="tests/e2e/manual-directives (default: found upward from this file)")
    parser.add_argument("--allow-restart", action="store_true",
                        help="also stop the node, run a standalone op, start it again")
    parser.add_argument("--paid", action="store_true",
                        help="also run directive force_continue (DeepSeek, a few cents)")
    parser.add_argument("--keep", action="store_true", help="keep the scratch projects")
    parser.add_argument("--json", action="store_true", help="print results as JSON")
    args = parser.parse_args(argv)
    if not args.app_root.is_absolute():
        parser.error("--app-root must be absolute")
    args.ryeos_bin = args.ryeos_bin or shutil.which("ryeos")
    if not args.ryeos_bin:
        parser.error("ryeos not found on PATH; pass --ryeos-bin")
    args.manual_directives = args.manual_directives or default_manual_directives()
    results = Smoke(args, run=run).run_all()
    if args.json:
        print(json.dumps([r.__dict__ for r in results], indent=2))
    else:
        print(render(results))
    return 1 if any(r.status == FAIL for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
