#!/usr/bin/env python3
# ryeos:signed:2026-11-12T23:59:41Z:691a48e1012fa5eacc683f07834295e3f5e3f1d1149c83e19c9614ca7fa9092f:4dnDRR5atDb7BJtdKk+vdx8Z646bpFcGetUmic6cY3+h6mc4hAwC1uk9n1KY17qDlq55kIHJr63TtcXVoaFoCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Run one cargo command for a worktree under the machine's shared build guard.

External development host tool; it grants no RyeOS authority. One guarded
cargo runs at a time across every session (an exclusive flock). Before
starting it waits while a configured live-step process runs and while any
unguarded cargo/rustc runs, and refuses below the disk floor. While cargo runs
a watchdog kills cargo's whole process group if free space falls under the
lower floor. A target dir shared across worktrees gets source mtimes touched
when the worktree changes (cargo trusts mtimes) and is pruned afterwards.

Every site value comes from `config:development/ryeos/guarded-cargo`, or
from a local copy passed with --config-file.

  guarded_cargo.py [--config-file F] [--worktree DIR] [--log FILE] -- <cargo args>
  guarded_cargo.py [--config-file F] [--worktree DIR] --status
  guarded_cargo.py [--config-file F] [--worktree DIR] --prune-only

Exit: cargo's own status; 2 usage/config; 3 refused below the disk floor;
4 killed by the disk watchdog.
"""

from __future__ import annotations

import argparse
import fcntl
import fnmatch
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

_spec = importlib.util.spec_from_file_location(
    "dev_host", Path(__file__).resolve().parents[2] / "dev-host/lib/dev_host.py")
dev_host = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev_host)
ToolError = dev_host.ToolError

NAME = "guarded-cargo"
SCHEMA = "ryeos.development.guarded-cargo.v1"
KEYS = {"lock_path", "target_dir", "tmpdir", "cargo", "env", "live_step_patterns",
        "wait_for_unguarded_cargo", "poll_seconds", "disk", "priority", "shared_target", "prune"}
TEST_EXECUTABLE = re.compile(r"^(.*)-[0-9a-f]{16}(\.tmp[0-9a-f]*)?$")
EXIT_DISK_REFUSED, EXIT_WATCHDOG = 3, 4


class Settings:
    """The resolved config: every path expanded for one worktree."""

    def __init__(self, config: dict, worktree: Path):
        self.worktree = worktree
        base = {"WORKTREE": str(worktree)}
        self.target_dir = dev_host.expand_path(config["target_dir"], base)
        base["TARGET_DIR"] = str(self.target_dir)
        self.lock_path = dev_host.expand_path(config["lock_path"], base)
        self.tmpdir = dev_host.expand_path(config["tmpdir"], base) if config["tmpdir"] else None
        self.cargo = dev_host.expand(config["cargo"], base)
        env = config["env"]
        if not isinstance(env, dict) or not all(isinstance(v, str) for v in env.values()):
            raise ToolError("env must map names to strings")
        self.env = {key: dev_host.expand(value, base) for key, value in env.items()}
        patterns = config["live_step_patterns"]
        if not isinstance(patterns, list):
            raise ToolError("live_step_patterns must be a list of regular expressions")
        try:
            self.live_steps = [re.compile(dev_host.expand(p, base)) for p in patterns]
        except re.error as error:
            raise ToolError(f"invalid live_step_patterns entry: {error}") from error
        self.wait_for_unguarded = bool(config["wait_for_unguarded_cargo"])
        poll = config["poll_seconds"]
        if set(poll) != {"live_step", "unguarded_cargo", "watchdog"} or not all(
                isinstance(v, (int, float)) and v > 0 for v in poll.values()):
            raise ToolError("poll_seconds needs positive live_step, unguarded_cargo and watchdog")
        self.poll = poll
        disk = config["disk"]
        if set(disk) != {"paths", "refuse_below_mib", "kill_below_mib"}:
            raise ToolError("disk needs paths, refuse_below_mib and kill_below_mib")
        self.disk_paths = [dev_host.expand_path(p, base) for p in disk["paths"]]
        self.refuse_below, self.kill_below = disk["refuse_below_mib"], disk["kill_below_mib"]
        if not (isinstance(self.refuse_below, int) and isinstance(self.kill_below, int)
                and 0 <= self.kill_below <= self.refuse_below):
            raise ToolError("disk floors must be integers with kill_below_mib <= refuse_below_mib")
        priority = config["priority"]
        if set(priority) != {"nice", "ionice_idle", "new_session"}:
            raise ToolError("priority needs nice, ionice_idle and new_session")
        self.priority = priority
        shared = config["shared_target"]
        if set(shared) != {"touch_sources_on_worktree_change", "source_paths", "marker"}:
            raise ToolError("shared_target needs touch_sources_on_worktree_change, source_paths, marker")
        self.shared = shared
        prune = config["prune"]
        if set(prune) != {"remove_dirs", "test_executables", "deps_dirs", "stale_artifacts"}:
            raise ToolError("prune needs remove_dirs, test_executables, deps_dirs, stale_artifacts")
        if prune["test_executables"] not in ("all", "keep_newest", "none"):
            raise ToolError("prune.test_executables is one of all, keep_newest, none")
        if set(prune["stale_artifacts"]) != {"max_age_minutes", "name_globs"}:
            raise ToolError("prune.stale_artifacts needs max_age_minutes and name_globs")
        for relative in (*prune["remove_dirs"], *prune["deps_dirs"]):
            if Path(relative).is_absolute() or ".." in Path(relative).parts:
                raise ToolError(f"prune paths are relative to the target dir: {relative!r}")
        self.prune = prune


def load(config_file: Path | None, worktree: Path) -> Settings:
    path = config_file or dev_host.config_path(dev_host.REPOSITORY_ROOT, NAME)
    return Settings(dev_host.load_config(path, NAME, SCHEMA, KEYS), worktree)


# ---- host observation -------------------------------------------------------

def processes(proc_root: Path = Path("/proc")):
    """Yield (pid, comm, cmdline) for every readable process except this one."""
    own = os.getpid()
    for entry in proc_root.iterdir():
        if not entry.name.isdigit() or int(entry.name) == own:
            continue
        try:
            comm = (entry / "comm").read_text().strip()
            cmdline = (entry / "cmdline").read_bytes().rstrip(b"\0").replace(b"\0", b" ")
        except OSError:
            continue
        yield int(entry.name), comm, cmdline.decode(errors="replace")


def running_live_steps(settings: Settings, proc_root: Path = Path("/proc")) -> list[str]:
    return [f"{pid} {cmd[:160]}" for pid, _, cmd in processes(proc_root)
            if any(p.search(cmd) for p in settings.live_steps)]


def running_cargo(proc_root: Path = Path("/proc")) -> list[str]:
    return [f"{pid} {comm}" for pid, comm, _ in processes(proc_root) if comm in ("cargo", "rustc")]


def available_mib(path: Path) -> int:
    """Free MiB on the filesystem holding path (or its nearest existing parent)."""
    while not path.exists():
        path = path.parent
    stats = os.statvfs(path)
    return stats.f_bavail * stats.f_frsize // (1024 * 1024)


def low_disk(settings: Settings, floor: int, available=available_mib) -> str | None:
    readings = {str(p): available(p) for p in settings.disk_paths}
    if any(value < floor for value in readings.values()):
        return ", ".join(f"{p}={v}M" for p, v in readings.items())
    return None


# ---- the guarded run ----------------------------------------------------------

class Reporter:
    def __init__(self, log: Path | None):
        self.log = log

    def __call__(self, message: str) -> None:
        line = f"guarded-cargo: {message}"
        print(line, file=sys.stderr, flush=True)
        if self.log is not None:
            with open(self.log, "a") as handle:
                handle.write(line + "\n")


def acquire_lock(settings: Settings, say: Reporter):
    settings.lock_path.parent.mkdir(parents=True, exist_ok=True)
    handle = open(settings.lock_path, "a")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        say(f"waiting for the shared cargo lock {settings.lock_path}")
        fcntl.flock(handle, fcntl.LOCK_EX)
    return handle


def wait_until_clear(settings: Settings, say: Reporter, proc_root: Path = Path("/proc")) -> None:
    announced = False
    while live := running_live_steps(settings, proc_root):
        if not announced:
            say(f"waiting: live step running ({live[0]})")
            announced = True
        time.sleep(settings.poll["live_step"])
    announced = False
    while settings.wait_for_unguarded and (busy := running_cargo(proc_root)):
        if not announced:
            say(f"waiting: unguarded cargo/rustc running ({', '.join(busy[:3])})")
            announced = True
        time.sleep(settings.poll["unguarded_cargo"])


def touch_sources_if_moved(settings: Settings, say: Reporter) -> None:
    """Make this worktree's sources newest when the shared target last built another one."""
    if not settings.shared["touch_sources_on_worktree_change"]:
        return
    marker = settings.target_dir / settings.shared["marker"]
    try:
        last = marker.read_text()
    except OSError:
        last = None
    if last == str(settings.worktree):
        return
    listed = subprocess.run(["git", "ls-files", "-z", "--", *settings.shared["source_paths"]],
                            cwd=settings.worktree, capture_output=True, check=True).stdout
    now = time.time()
    count = 0
    for name in filter(None, listed.split(b"\0")):
        path = settings.worktree / os.fsdecode(name)
        try:
            os.utime(path, (now, now), follow_symlinks=False)
            count += 1
        except FileNotFoundError:
            pass
    settings.target_dir.mkdir(parents=True, exist_ok=True)
    marker.write_text(str(settings.worktree))
    say(f"shared target last built {last or 'nothing recorded'}; touched {count} source files")


def command(settings: Settings, cargo_args: list[str]) -> list[str]:
    argv = [settings.cargo, *cargo_args]
    if settings.priority["ionice_idle"] and shutil.which("ionice"):
        argv = ["ionice", "-c3", *argv]
    return argv


def run_cargo(settings: Settings, cargo_args: list[str], log: Path | None, say: Reporter,
              available=available_mib) -> int:
    env = dict(os.environ)
    env.update(settings.env)
    env["CARGO_TARGET_DIR"] = str(settings.target_dir)
    if settings.tmpdir is not None:
        settings.tmpdir.mkdir(parents=True, exist_ok=True)
        env["TMPDIR"] = str(settings.tmpdir)
    nice = settings.priority["nice"]
    output = open(log, "a") if log is not None else None
    try:
        child = subprocess.Popen(
            command(settings, cargo_args), cwd=settings.worktree, env=env, stdin=subprocess.DEVNULL,
            stdout=output, stderr=subprocess.STDOUT if output else None,
            # A new session also gives test fixtures a process-group leader of their own.
            start_new_session=settings.priority["new_session"],
            preexec_fn=(lambda: os.nice(nice)) if nice else None)
        killed = watch(settings, child, say, available)
    finally:
        if output is not None:
            output.close()
    if killed:
        return EXIT_WATCHDOG
    say(f"cargo exit={child.returncode}")
    return child.returncode


def watch(settings: Settings, child: subprocess.Popen, say: Reporter, available=available_mib) -> bool:
    """Wait for cargo; kill its process group if disk falls under the kill floor."""
    while True:
        try:
            child.wait(timeout=settings.poll["watchdog"])
            return False
        except subprocess.TimeoutExpired:
            pass
        reading = low_disk(settings, settings.kill_below, available)
        if reading is not None:
            say(f"WATCHDOG: disk below {settings.kill_below}M ({reading}); killing cargo")
            try:
                if settings.priority["new_session"]:
                    os.killpg(child.pid, signal.SIGKILL)
                else:
                    child.kill()
            except ProcessLookupError:
                pass
            child.wait()
            return True


def prune(settings: Settings, say: Reporter) -> None:
    """Drop rebuildable target content that a shared target accumulates."""
    target, rules = settings.target_dir, settings.prune
    removed = 0
    for relative in rules["remove_dirs"]:
        path = target / relative
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path, ignore_errors=True)
            removed += 1
    files = 0
    cutoff = time.time() - 60 * rules["stale_artifacts"]["max_age_minutes"]
    for relative in rules["deps_dirs"]:
        deps = target / relative
        if not deps.is_dir():
            continue
        groups: dict[str, list[tuple[float, Path]]] = {}
        for entry in deps.iterdir():
            try:
                info = entry.lstat()
            except FileNotFoundError:
                continue
            if not entry.is_file() or entry.is_symlink():
                continue
            match = TEST_EXECUTABLE.match(entry.name)
            if match and info.st_mode & 0o111:
                groups.setdefault(match.group(1), []).append((info.st_mtime, entry))
            elif (info.st_mtime < cutoff and
                  any(fnmatch.fnmatch(entry.name, g) for g in rules["stale_artifacts"]["name_globs"])):
                files += _unlink(entry)
        if rules["test_executables"] != "none":
            for members in groups.values():
                members.sort(reverse=True)
                doomed = members if rules["test_executables"] == "all" else members[1:]
                files += sum(_unlink(path) for _, path in doomed)
    say(f"pruned {removed} dirs and {files} files under {target}")


def _unlink(path: Path) -> int:
    try:
        path.unlink()
        return 1
    except FileNotFoundError:
        return 0


def status(settings: Settings) -> dict:
    held = False
    if settings.lock_path.is_file():
        with open(settings.lock_path, "a") as handle:
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(handle, fcntl.LOCK_UN)
            except BlockingIOError:
                held = True
    return {
        "worktree": str(settings.worktree),
        "target_dir": str(settings.target_dir),
        "lock_path": str(settings.lock_path),
        "lock_held": held,
        "live_steps": running_live_steps(settings),
        "unguarded_cargo": running_cargo(),
        "disk_mib": {str(p): available_mib(p) for p in settings.disk_paths},
        "floors_mib": {"refuse_below": settings.refuse_below, "kill_below": settings.kill_below},
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--config-file", type=Path)
    parser.add_argument("--worktree", type=Path, default=Path.cwd(),
                        help="worktree cargo runs in (default: current directory)")
    parser.add_argument("--log", type=Path, help="append cargo output and guard messages here")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--status", action="store_true", help="print guard state as JSON and exit")
    mode.add_argument("--prune-only", action="store_true", help="prune the target dir under the lock")
    parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    cargo_args = args.cargo_args[1:] if args.cargo_args[:1] == ["--"] else args.cargo_args
    try:
        worktree = args.worktree.resolve()
        if not (worktree / "Cargo.toml").is_file():
            raise ToolError(f"{worktree} has no Cargo.toml")
        settings = load(args.config_file, worktree)
        if args.status:
            print(json.dumps(status(settings), indent=2))
            return 0
        if not args.prune_only and not cargo_args:
            raise ToolError("give the cargo arguments after --, e.g. -- test -p ryeos-engine")
        say = Reporter(args.log)
        lock = acquire_lock(settings, say)
        try:
            wait_until_clear(settings, say)
            if args.prune_only:
                prune(settings, say)
                return 0
            reading = low_disk(settings, settings.refuse_below)
            if reading is not None:
                say(f"refusing: disk below {settings.refuse_below}M ({reading})")
                return EXIT_DISK_REFUSED
            touch_sources_if_moved(settings, say)
            code = run_cargo(settings, cargo_args, args.log, say)
            prune(settings, say)
            return code
        finally:
            lock.close()
    except ToolError as error:
        print(f"guarded-cargo: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
