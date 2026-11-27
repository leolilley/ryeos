# ryeos:signed:2026-11-27T21:29:40Z:aeabdf4a9bc4a41d33432a6e8c2adfcecb6b16591732f364f782dc5dfe001f22:z9idVTGE1tVHg8xujn6NpOC+Iu0K9jzP1GhekbYDqKc6ueUmMLCC3G7FMkxWnzJKuLnHgNYThyLVBCO1/ZGBBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/evaluate
#   version: "1.0.0"
#   description: A candidate evaluator that runs the checks the project declared in signed configuration read from the base snapshot, and says which ran. No declared checks never accepts.
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: node_policy
#   network_authority: node_policy
#   config_schema:
#     type: object
#     properties:
#       candidate_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#       base_snapshot_hash: {type: string, pattern: '^[a-f0-9]{64}$'}
#       base_config: {type: [object, "null"]}
#     required: [candidate_snapshot_hash, base_snapshot_hash]
#     additionalProperties: false
"""Evaluate one frozen candidate against the checks its project declared.

The checks arrive as `base_config`, read from the base snapshot by the daemon
and sealed into this launch's parameters: `{"ref": <config ref>, "trusted":
<bool>, "digest": <sha256 of the signed config's bytes or null>, "value":
{"checks": [...]} or null, "reason": <why not trusted, when it is not>}`. The
daemon sets `trusted` only for a config its trust rules accept (the project's
trusted key, or an authoring key's per-digest endorsement). This tool never
reads a checks file from the candidate, which is untrusted. It executes nothing from the candidate. Every read of the candidate
opens each path component without following links, so a symlink anywhere on a
checked path fails that check. No checks declared (absent config, an untrusted
signer, an empty list, an invalid config) never accepts.

Check kinds, each `{id, kind, ...}`:
  path_exists {path}            a regular file exists at path
  path_absent {path}            nothing exists at path (a link counts as existing)
  content_sha256 {path, sha256} the file's bytes hash to sha256
  paths_within {allow, deny}    every file path matches an allow glob and no deny glob
  limits {max_files, max_total_bytes, max_file_bytes}
                                the candidate stays within these counts and sizes
  parses_as {path, format, max_bytes}
                                the file parses as json or yaml within max_bytes;
                                yaml with aliases, or with too many events, fails
"""

import hashlib
import json
import os
import re
import stat
import sys
from pathlib import Path

HASH = re.compile(r"[a-f0-9]{64}")
ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}")
SCOPE_RAN = "declared checks run"
SCOPE_NONE = "no checks declared"
MAX_CHECKS = 64
MAX_PATH_BYTES = 512
MAX_GLOBS = 64
MAX_GLOB_CHARS = 256
MAX_WALK_ENTRIES = 100_000
PARSE_BYTES_CEILING = 4 * 1024 * 1024
PARSE_BYTES_DEFAULT = 1024 * 1024
YAML_EVENTS_CEILING = 100_000
DETAIL_CHARS = 160


class Refused(Exception):
    """A check cannot pass; the message becomes its recorded detail."""


def clean(text: str) -> str:
    text = "".join(ch if ch.isprintable() else "?" for ch in str(text))
    return text[:DETAIL_CHARS]


def split_path(path) -> list:
    if not isinstance(path, str) or not path or len(path.encode()) > MAX_PATH_BYTES:
        raise Refused("path is not a bounded non-empty string")
    if "\x00" in path or path.startswith("/") or path.endswith("/"):
        raise Refused("path is not a canonical relative path")
    parts = path.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise Refused("path is not a canonical relative path")
    return parts


def open_no_follow(root: Path, path: str, want_dir: bool = False) -> int:
    """Open `path` under `root`, refusing a link at every component."""
    parts = split_path(path)
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW
    fd = os.open(root, os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            nxt = os.open(part, flags | os.O_DIRECTORY, dir_fd=fd)
            os.close(fd)
            fd = nxt
        last = flags | (os.O_DIRECTORY if want_dir else os.O_NONBLOCK)
        nxt = os.open(parts[-1], last, dir_fd=fd)
    except OSError as error:
        raise Refused(f"cannot open without following links ({error.strerror})")
    finally:
        os.close(fd)
    if not want_dir and not stat.S_ISREG(os.fstat(nxt).st_mode):
        os.close(nxt)
        raise Refused("not a regular file")
    return nxt


def read_file(root: Path, path: str, limit: int) -> bytes:
    fd = open_no_follow(root, path)
    with os.fdopen(fd, "rb") as handle:
        data = handle.read(limit + 1)
    if len(data) > limit:
        raise Refused(f"file is larger than {limit} bytes")
    return data


def walk(root: Path) -> list:
    """Every entry below root as (relative posix path, lstat mode, size); links are not followed."""
    entries = []
    pending = [("", root)]
    while pending:
        prefix, directory = pending.pop()
        with os.scandir(directory) as scan:
            for entry in scan:
                info = entry.stat(follow_symlinks=False)
                relative = f"{prefix}{entry.name}"
                entries.append((relative, info.st_mode, info.st_size))
                if len(entries) > MAX_WALK_ENTRIES:
                    raise Refused(f"candidate has more than {MAX_WALK_ENTRIES} entries")
                if stat.S_ISDIR(info.st_mode):
                    pending.append((relative + "/", Path(entry.path)))
    return sorted(entries)


def glob_regex(pattern) -> "re.Pattern":
    if not isinstance(pattern, str) or not pattern or len(pattern) > MAX_GLOB_CHARS:
        raise Refused("glob is not a bounded non-empty string")
    out = []
    i = 0
    while i < len(pattern):
        if pattern.startswith("**/", i):
            out.append("(?:[^/]+/)*")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif pattern[i] == "*":
            out.append("[^/]*")
            i += 1
        elif pattern[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(pattern[i]))
            i += 1
    return re.compile("".join(out))


def globs(values) -> list:
    if not isinstance(values, list) or len(values) > MAX_GLOBS:
        raise Refused("globs are not a bounded list")
    return [glob_regex(value) for value in values]


def check_path_exists(root, spec, ctx):
    os.close(open_no_follow(root, spec["path"]))
    return "file exists"


def check_path_absent(root, spec, ctx):
    parts = split_path(spec["path"])
    current = root
    for part in parts:
        current = current / part
        try:
            os.lstat(current)
        except FileNotFoundError:
            return "nothing at path"
        except NotADirectoryError:
            return "nothing at path"
    raise Refused("something exists at path")


def check_content_sha256(root, spec, ctx):
    wanted = spec.get("sha256")
    if not isinstance(wanted, str) or not HASH.fullmatch(wanted):
        raise Refused("sha256 is not 64 lowercase hex characters")
    data = read_file(root, spec["path"], ctx["max_file_bytes"])
    if hashlib.sha256(data).hexdigest() != wanted:
        raise Refused("content hash differs")
    return "content hash matches"


def check_paths_within(root, spec, ctx):
    allow = globs(spec.get("allow"))
    deny = globs(spec.get("deny", []))
    if not allow:
        raise Refused("allow list is empty")
    for relative, mode, _size in ctx["entries"]:
        if stat.S_ISDIR(mode):
            continue
        if not any(rx.fullmatch(relative) for rx in allow):
            raise Refused(f"path outside the allow list: {relative}")
        if any(rx.fullmatch(relative) for rx in deny):
            raise Refused(f"path in the deny list: {relative}")
    return "every path is allowed"


def check_limits(root, spec, ctx):
    files = [entry for entry in ctx["entries"] if not stat.S_ISDIR(entry[1])]
    for key in ("max_files", "max_total_bytes", "max_file_bytes"):
        value = spec.get(key)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise Refused(f"{key} is not a non-negative integer")
    if len(files) > spec["max_files"]:
        raise Refused("too many files")
    if sum(size for _p, _m, size in files) > spec["max_total_bytes"]:
        raise Refused("too many bytes in total")
    if any(size > spec["max_file_bytes"] for _p, _m, size in files):
        raise Refused("a file is larger than the per-file limit")
    return "within limits"


def yaml_refuse_unsafe(data: bytes) -> None:
    try:
        import yaml
    except ImportError:
        raise Refused("yaml parser unavailable")
    count = 0
    try:
        for event in yaml.parse(data.decode("utf-8")):
            count += 1
            if count > YAML_EVENTS_CEILING:
                raise Refused("yaml has too many events")
            if isinstance(event, yaml.events.AliasEvent):
                raise Refused("yaml aliases are refused")
    except (yaml.YAMLError, UnicodeDecodeError) as error:
        raise Refused(f"yaml does not parse ({type(error).__name__})")


JSON_DEPTH_CEILING = 128


def json_refuse_deep(text: str) -> None:
    # The depth is measured here so the verdict does not depend on the
    # interpreter's recursion limit, which differs between Python versions.
    depth = 0
    in_string = False
    escaped = False
    for ch in text:
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch in "[{":
            depth += 1
            if depth > JSON_DEPTH_CEILING:
                raise Refused("json is nested too deeply")
        elif ch in "]}":
            depth -= 1


def check_parses_as(root, spec, ctx):
    limit = spec.get("max_bytes", PARSE_BYTES_DEFAULT)
    if not isinstance(limit, int) or isinstance(limit, bool) or not 0 < limit <= PARSE_BYTES_CEILING:
        raise Refused("max_bytes is outside the bounded range")
    data = read_file(root, spec["path"], limit)
    kind = spec.get("format")
    if kind == "json":
        try:
            text = data.decode("utf-8")
            json_refuse_deep(text)
            json.loads(text)
        except Refused:
            raise
        except (ValueError, RecursionError, UnicodeDecodeError) as error:
            raise Refused(f"json does not parse ({type(error).__name__})")
    elif kind == "yaml":
        yaml_refuse_unsafe(data)
    else:
        raise Refused("format is not json or yaml")
    return f"parses as {kind}"


KINDS = {
    "path_exists": ({"path"}, check_path_exists, False),
    "path_absent": ({"path"}, check_path_absent, False),
    "content_sha256": ({"path", "sha256"}, check_content_sha256, False),
    "paths_within": ({"allow", "deny"}, check_paths_within, True),
    "limits": ({"max_files", "max_total_bytes", "max_file_bytes"}, check_limits, True),
    "parses_as": ({"path", "format", "max_bytes"}, check_parses_as, False),
}


def declared_checks(base_config):
    """The validated checks, or (None, reason) when nothing usable is declared."""
    if not isinstance(base_config, dict) or base_config.get("trusted") is not True:
        reason = base_config.get("reason") if isinstance(base_config, dict) else None
        return None, clean(reason) if isinstance(reason, str) and reason else "no trusted checks config in the base snapshot"
    digest = base_config.get("digest")
    value = base_config.get("value")
    checks = value.get("checks") if isinstance(value, dict) else None
    if not isinstance(digest, str) or not HASH.fullmatch(digest):
        return None, "checks config has no valid digest"
    if not isinstance(checks, list) or not checks:
        return None, "no checks declared"
    if len(checks) > MAX_CHECKS:
        return None, "too many checks declared"
    seen = set()
    for spec in checks:
        if not isinstance(spec, dict) or not isinstance(spec.get("id"), str) or not ID.fullmatch(spec["id"]):
            return None, "a check has no valid id"
        if spec["id"] in seen:
            return None, "a check id repeats"
        seen.add(spec["id"])
        kind = KINDS.get(spec.get("kind"))
        if kind is None:
            return None, "a check has an unknown kind"
        if set(spec) - kind[0] - {"id", "kind"}:
            return None, "a check carries an unknown field"
    return checks, None


def evaluate(request: dict, project: Path) -> dict:
    candidate = request["candidate_snapshot_hash"]
    base = request["base_snapshot_hash"]
    config = request.get("base_config")
    digest = config.get("digest") if isinstance(config, dict) else None
    config_ref = config.get("ref") if isinstance(config, dict) and isinstance(config.get("ref"), str) else None
    structural = (
        bool(HASH.fullmatch(candidate))
        and bool(HASH.fullmatch(base))
        and candidate != base
        and project.is_dir()
    )
    result = {
        "schema_version": 1,
        "candidate_snapshot_hash": candidate,
        "base_snapshot_hash": base,
    }
    checks, reason = declared_checks(config)
    if not structural:
        result["accepted"] = False
        result["evidence"] = {
            "scope": SCOPE_NONE,
            "reason": "the candidate view is not a distinct, readable generation",
            "config_ref": config_ref,
            "config_digest": digest,
            "checks_declared": 0,
            "checks": [],
        }
        return result
    if checks is None:
        result["accepted"] = False
        result["evidence"] = {
            "scope": SCOPE_NONE,
            "reason": reason,
            "config_ref": config_ref,
            "config_digest": digest if isinstance(digest, str) and HASH.fullmatch(digest) else None,
            "checks_declared": 0,
            "checks": [],
        }
        return result
    ctx = {"max_file_bytes": PARSE_BYTES_CEILING, "entries": None}
    outcomes = []
    for spec in checks:
        _fields, function, needs_walk = KINDS[spec["kind"]]
        try:
            if needs_walk and ctx["entries"] is None:
                ctx["entries"] = walk(project)
            detail = function(project, spec, ctx)
            outcomes.append({"id": spec["id"], "result": "pass", "detail": clean(detail)})
        except Refused as refusal:
            outcomes.append({"id": spec["id"], "result": "fail", "detail": clean(refusal)})
        except OSError as error:
            outcomes.append({"id": spec["id"], "result": "fail", "detail": clean(error.strerror or "io error")})
    result["accepted"] = all(item["result"] == "pass" for item in outcomes)
    result["evidence"] = {
        "scope": SCOPE_RAN,
        "reason": "every declared check passed" if result["accepted"] else "a declared check failed",
        "config_ref": config_ref,
        "config_digest": digest,
        "checks_digest": hashlib.sha256(
            json.dumps(checks, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest(),
        "checks_declared": len(outcomes),
        "checks": outcomes,
    }
    return result


def main() -> None:
    request = json.loads(sys.stdin.buffer.read(1024 * 1024 + 1))
    project = Path(sys.argv[2])
    print(json.dumps(evaluate(request, project), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
