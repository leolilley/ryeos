# ryeos:signed:2026-09-07T06:05:34Z:72bc7fa983891ac47500ac2a97516ccd720adb8afd7b83f2fe505d3e504dd914:3nGBU/dr+3qcHvuXDc1Hhvq9SbbETcj8/HZYeDirrbQvQXcwXt7d0dJO5PNZxLHFTR3Zqvrbd9Wd4LLcKofaDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Read-only repository validation shared by signed Tools and external CI.

The resolved project Config owns rules and input selection. This module owns
checking, not acquisition, signing, capture policy or execution authorization.
CI may call the same implementation explicitly before RyeOS exists; that is
source-test evidence, not admitted Tool execution. Never infer a repository
from this source closure's path or fall back to host commands/configuration.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
import stat
import sys
import tomllib
from pathlib import Path, PurePosixPath


SCHEMA = "ryeos.development.repository-validation.v1"


def relative(value: str) -> PurePosixPath:
    if not isinstance(value, str) or not value:
        raise ValueError("expected a project-relative input")
    path = PurePosixPath(value)
    if path.is_absolute() or path.as_posix() != value or any(p in (".", "..") for p in path.parts):
        raise ValueError("noncanonical project-relative input")
    return path


def member(root: Path, value: str) -> Path:
    path = root
    for part in relative(value).parts:
        path /= part
        if path.is_symlink():
            raise ValueError(f"validation input is a symlink: {value}")
    return path


def read(root: Path, value: str, limit: int) -> str:
    path = member(root, value)
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError(f"validation input is not regular: {value}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"validation input exceeds its configured bound: {value}")
    return data.decode("utf-8")


def validate_config(config: dict) -> None:
    if (not isinstance(config, dict) or set(config) != {
            "category", "name", "version", "schema", "limits", "dependency_layers", "text_checks"
    } or config["schema"] != SCHEMA):
        raise ValueError("incomplete repository validation configuration")
    limits = config["limits"]
    if (not isinstance(limits, dict) or set(limits) != {
            "max_file_bytes", "max_files", "max_total_bytes", "max_findings", "max_depth"
    } or any(type(v) is not int or v <= 0 for v in limits.values())):
        raise ValueError("validation requires positive explicit bounds")
    dependency = config["dependency_layers"]
    if not isinstance(dependency, dict) or set(dependency) != {"workspace_manifest", "forbidden_edges"}:
        raise ValueError("incomplete dependency selection")
    relative(dependency["workspace_manifest"])
    edges = dependency["forbidden_edges"]
    if not isinstance(edges, dict) or not edges or any(
        not isinstance(k, str) or not isinstance(v, list) or not v or
        any(not isinstance(item, str) or not item for item in v) for k, v in edges.items()
    ):
        raise ValueError("invalid dependency rules")
    checks = config["text_checks"]
    if not isinstance(checks, dict) or not checks:
        raise ValueError("missing text checks")
    for rules in checks.values():
        if not isinstance(rules, list) or not rules:
            raise ValueError("missing text rules")
        for rule in rules:
            if not isinstance(rule, dict) or set(rule) != {
                "roots", "exclude", "suffixes", "patterns", "regex", "ignore_case"
            }:
                raise ValueError("incomplete text rule")
            for field in ("roots", "exclude", "suffixes", "patterns"):
                if not isinstance(rule[field], list) or any(not isinstance(v, str) or not v for v in rule[field]):
                    raise ValueError("invalid text rule selection")
            if not rule["roots"] or not rule["patterns"]:
                raise ValueError("empty scan cannot establish successful validation")
            for path in rule["roots"]:
                relative(path)
            for flag in ("regex", "ignore_case"):
                if type(rule[flag]) is not bool:
                    raise ValueError("text rule flags must be explicit booleans")
            for pattern in rule["patterns"]:
                re.compile(pattern if rule["regex"] else re.escape(pattern))


def dependencies(data: dict, workspace: set[str], workspace_dependencies: dict) -> set[str]:
    found = set()
    tables = [data.get("dependencies", {}), data.get("build-dependencies", {})]
    for target in data.get("target", {}).values():
        tables.extend((target.get("dependencies", {}), target.get("build-dependencies", {})))
    for table in tables:
        for name, spec in table.items():
            # Cargo owns package identity, including aliases inherited from
            # [workspace.dependencies]. Do not infer it from a member's local
            # dependency spelling or create a second authored dependency map.
            if isinstance(spec, dict) and "workspace" in spec:
                if spec["workspace"] is not True or name not in workspace_dependencies:
                    raise ValueError("dependency inheritance is not declared by the workspace")
                spec = workspace_dependencies[name]
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if package in workspace:
                found.add(package)
    return found


def find_cycle(graph: dict[str, set[str]]) -> list[str] | None:
    active, visited = [], set()

    def visit(node):
        if node in active:
            return active[active.index(node):] + [node]
        if node in visited:
            return None
        active.append(node)
        for dependency in sorted(graph[node]):
            cycle = visit(dependency)
            if cycle:
                return cycle
        active.pop()
        visited.add(node)
        return None

    for node in sorted(graph):
        cycle = visit(node)
        if cycle:
            return cycle
    return None


def dependency_layers(root: Path, config: dict) -> tuple[list[str], int]:
    selection, limits = config["dependency_layers"], config["limits"]
    root_body = read(root, selection["workspace_manifest"], limits["max_file_bytes"])
    root_manifest = tomllib.loads(root_body)
    packages, total = {}, len(root_body.encode())
    if total > limits["max_total_bytes"]:
        raise ValueError("workspace exceeds validation bounds")
    for item in root_manifest["workspace"]["members"]:
        # This repository authors exact workspace paths. Wildcards must not
        # become implicit new input selection or silently skip dependencies.
        if any(c in item for c in "*?["):
            raise ValueError("workspace member must name an exact directory")
        value = read(root, str(relative(item) / "Cargo.toml"), limits["max_file_bytes"])
        total += len(value.encode())
        data = tomllib.loads(value)
        name = data["package"]["name"]
        if name in packages:
            raise ValueError("duplicate workspace package")
        packages[name] = data
        if len(packages) > limits["max_files"] or total > limits["max_total_bytes"]:
            raise ValueError("workspace exceeds validation bounds")
    if not packages:
        raise ValueError("empty workspace cannot establish successful validation")
    inherited = root_manifest["workspace"].get("dependencies", {})
    graph = {name: dependencies(data, set(packages), inherited) for name, data in packages.items()}
    failures = []
    for owner, denied in selection["forbidden_edges"].items():
        if owner not in graph:
            raise ValueError(f"configured dependency owner is absent: {owner}")
        for dependency in sorted(graph[owner] & set(denied)):
            failures.append(f"forbidden dependency: {owner} -> {dependency}")
    cycle = find_cycle(graph)
    if cycle:
        failures.append("workspace dependency cycle: " + " -> ".join(cycle))
    return failures, len(packages)


def text_check(root: Path, config: dict, operation: str) -> tuple[list[str], int]:
    limits, failures, checked, total, entries = config["limits"], [], set(), 0, 0
    for rule in config["text_checks"][operation]:
        patterns = [re.compile(p if rule["regex"] else re.escape(p),
                               re.IGNORECASE if rule["ignore_case"] else 0) for p in rule["patterns"]]
        selected = set()

        def walk(value: str, depth: int):
            nonlocal entries
            if any(fnmatch.fnmatchcase(value, pattern) for pattern in rule["exclude"]):
                return
            entries += 1
            if depth > limits["max_depth"] or entries > limits["max_files"]:
                raise ValueError("scan exceeds validation inventory bound")
            path = member(root, value)
            mode = path.stat().st_mode
            if stat.S_ISDIR(mode):
                for child in sorted(path.iterdir()):
                    walk(f"{value}/{child.name}", depth + 1)
            elif stat.S_ISREG(mode):
                if not rule["suffixes"] or path.suffix in rule["suffixes"]:
                    selected.add(value)
            else:
                raise ValueError(f"nonregular validation input: {value}")

        for path in rule["roots"]:
            walk(path, 0)
        if not selected:
            raise ValueError("empty text selection cannot establish successful validation")
        for value in sorted(selected):
            body = read(root, value, limits["max_file_bytes"])
            total += len(body.encode())
            if total > limits["max_total_bytes"]:
                raise ValueError("scan exceeds validation byte bound")
            checked.add(value)
            for line, text in enumerate(body.splitlines(), 1):
                if any(pattern.search(text) for pattern in patterns):
                    # Report bounded coordinates, not arbitrary source content.
                    failures.append(f"{value}:{line}: forbidden text")
                    if len(failures) >= limits["max_findings"]:
                        return failures, len(checked)
    return failures, len(checked)


def run(operation: str) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True, type=Path)
    # Explicit external CI entry; this path is never supplied by the signed
    # runtime. CI does not pretend to perform RyeOS config/signature admission.
    parser.add_argument("--config-file", type=Path)
    args = parser.parse_args()
    if not args.project_path.is_absolute() or args.project_path.is_symlink() or not args.project_path.is_dir():
        raise ValueError("expected exact absolute project context")
    if args.config_file is not None:
        with args.config_file.open("rb") as source:
            raw = source.read(262145)
        if len(raw) > 262144:
            raise ValueError("validation config exceeds input bound")
        # The selected signed Config is JSON YAML; skip only its envelope line.
        config = json.loads(raw.split(b"\n", 1)[1] if raw.startswith(b"# ryeos:signed:") else raw)
    else:
        raw = sys.stdin.buffer.read(262145)
        if len(raw) > 262144:
            raise ValueError("validation request exceeds input bound")
        request = json.loads(raw)
        config = request["resolved_config"]
    validate_config(config)
    failures, count = (dependency_layers(args.project_path, config) if operation == "dependency-layers"
                       else text_check(args.project_path, config, operation))
    print(json.dumps({"ok": not failures, "operation": operation, "checked_files": count,
                      "findings": failures[:config["limits"]["max_findings"]]}, sort_keys=True))
    raise SystemExit(1 if failures else 0)
