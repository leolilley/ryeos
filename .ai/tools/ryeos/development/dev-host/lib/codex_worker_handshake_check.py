#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:7e963169b3d9fca5c6b5edc565f927c3d6337c69926da1cabb5effbc4206be48:nOqcyZ05RxCjVxGQz1STQiKHoT29sN21MvkYr+GZ9Ao/d0R8BjtwicGYK2eSD+rLQ3946gnZf+8q28ag3loWDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Hold a Codex binary's startup to a signed Codex Worker profile, with no credential.

External development host tool; it grants no RyeOS authority, reaches no
provider and spends nothing. It launches the given Codex exactly as the
profile's Worker does (`workload_args`, the profile's baseline config, an
empty throwaway home, an empty PATH and a stand-in command environment), then
checks every admitted initialization step's response against its response
schema, and `thread/start`'s response against its schema and response
predicates. Run it whenever the Codex pin or the Worker profile changes,
before anything spends.

  codex_worker_handshake_check.py --codex PATH [--profile-dir DIR]
      [--profile external-authoring.profile.json] [--timeout 20] [--worktree DIR]

The profile dir defaults to the worktree's
`bundles/codex/.ai/workers/codex/lib/hosted`. Needs the `jsonschema` module.
Exit 0 when every gate passes, 1 naming the failures, 2 when it cannot run.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile

PROFILE_DIR = Path("bundles/codex/.ai/workers/codex/lib/hosted")
# The product's own connector environment id (crates/tools/codex-external-configuration).
ENVIRONMENT_ID = "ryeos-external-candidate"


class Session:
    def __init__(self, codex: Path, profile: dict, profile_dir: Path, root: Path, timeout: float):
        self.home, self.tmp, self.workspace = root / "home", root / "tmp", root / "workspace"
        for path in (self.home, self.tmp, self.workspace):
            path.mkdir()
        shutil.copy(profile_dir / profile["baseline_config"], self.home / profile["baseline_destination"])
        (self.home / "environments.toml").write_text(
            f'default = "{ENVIRONMENT_ID}"\ninclude_local = false\n[[environments]]\n'
            f'id = "{ENVIRONMENT_ID}"\nprogram = "/bin/cat"\ncwd = "/"\nenv = {{}}\n')
        home_env = profile.get("workload_home_env", "CODEX_HOME")
        env = {home_env: str(self.home), "HOME": str(self.home), "TMPDIR": str(self.tmp),
               "PATH": "", "LANG": "C", "LC_ALL": "C"}
        self.stderr = open(root / "stderr", "w+")
        self.timeout = timeout
        self.next_id = 0
        self.process = subprocess.Popen([str(codex), *profile["workload_args"]], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr, text=True, env=env,
                                        cwd=self.workspace, bufsize=1)

    def send(self, message: dict) -> None:
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method: str, params: dict) -> dict:
        self.next_id += 1
        self.send({"id": self.next_id, "method": method, "params": params})
        while True:
            ready, _, _ = select.select([self.process.stdout], [], [], self.timeout)
            if not ready:
                return {"error": f"no reply in {self.timeout}s"}
            line = self.process.stdout.readline()
            if not line:
                self.stderr.seek(0)
                tail = [l for l in self.stderr.read().splitlines() if l.startswith("Error")]
                return {"error": f"exited {self.process.wait()}: {tail[:1]}"}
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("id") == self.next_id:
                return message

    def close(self) -> None:
        self.process.kill()
        self.process.wait()
        self.stderr.close()


def pointer(value, path: str):
    for key in path.strip("/").split("/"):
        value = value.get(key) if isinstance(value, dict) else None
    return value


def run(codex: Path, profile_dir: Path, profile_name: str, timeout: float) -> list[tuple[bool, str]]:
    import jsonschema

    profile = json.loads((profile_dir / profile_name).read_text())
    results: list[tuple[bool, str]] = []

    def schema_errors(name: str, value) -> list[str]:
        schema = json.loads((profile_dir / name).read_text())
        return [f"{list(e.path)[:4]}: {e.message[:160]}"
                for e in jsonschema.Draft7Validator(schema).iter_errors(value)]

    root = Path(tempfile.mkdtemp(prefix="codex-worker-handshake-"))
    session = None
    try:
        session = Session(codex, profile, profile_dir, root, timeout)
        for step in profile["initialization"]:
            if step.get("notification"):
                session.send({"method": step["notification"], "params": step["params"]})
                continue
            params = step["params"]
            if step["method"] == "config/read":
                params = {**params, "cwd": str(session.workspace)}
            reply = session.call(step["method"], params)
            results.append(("result" in reply, f"{step['method']} answered {str(reply.get('error', ''))[:200]}"))
            if "result" not in reply:
                return results
            if step.get("response_schema"):
                errors = schema_errors(step["response_schema"], reply["result"])
                results.append((not errors, f"{step['method']} passes {step['response_schema']} {errors[:3] or ''}"))
        route = next(r for r in profile["routes"] if r["method"] == "thread/start")
        reply = session.call("thread/start", {**route["fixed_params"], "cwd": "/"})
        results.append(("result" in reply, f"thread/start answered {str(reply.get('error', ''))[:200]}"))
        if "result" in reply:
            errors = schema_errors(route["response_schema"], reply["result"])
            results.append((not errors, f"thread/start passes {route['response_schema']} {errors[:3] or ''}"))
            for predicate in route.get("response_predicates", []):
                value = pointer({"response": reply}, predicate["pointer"])
                results.append((value == predicate["equals"],
                                f"thread/start {predicate['pointer']} = {value!r} (needs {predicate['equals']!r})"))
        return results
    finally:
        if session is not None:
            session.close()
        shutil.rmtree(root, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--codex", type=Path, required=True, help="the pinned Codex executable")
    parser.add_argument("--profile-dir", type=Path)
    parser.add_argument("--profile", default="external-authoring.profile.json")
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--worktree", type=Path, default=Path(__file__).resolve().parents[6])
    args = parser.parse_args(argv)
    profile_dir = args.profile_dir or args.worktree / PROFILE_DIR
    try:
        import jsonschema  # noqa: F401
    except ImportError:
        print("codex-worker-handshake-check: needs the python jsonschema module", file=sys.stderr)
        return 2
    if not os.access(args.codex, os.X_OK) or not (profile_dir / args.profile).is_file():
        print(f"codex-worker-handshake-check: need an executable {args.codex} and "
              f"{profile_dir / args.profile}", file=sys.stderr)
        return 2
    try:
        results = run(args.codex, profile_dir, args.profile, args.timeout)
    except (OSError, ValueError, KeyError, StopIteration) as error:
        print(f"codex-worker-handshake-check: cannot run the profile: {error!r}", file=sys.stderr)
        return 2
    for ok, message in results:
        print(("ok   " if ok else "FAIL ") + message)
    passed = all(ok for ok, _ in results)
    print("WORKER HANDSHAKE OK" if passed else "WORKER HANDSHAKE FAILED")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
