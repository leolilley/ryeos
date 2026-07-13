#!/usr/bin/env python3

"""Enforce the RyeOS workspace dependency constitution without Cargo."""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent

# These edges would move authority or orchestration into a lower layer.
FORBIDDEN: dict[str, set[str]] = {
    "lillux": {
        "ryeos-state", "ryeos-engine", "ryeos-runtime", "ryeos-app",
        "ryeos-executor", "ryeos-api", "ryeos-node", "ryeos-ui",
    },
    "ryeos-state": {
        "ryeos-engine", "ryeos-runtime", "ryeos-app", "ryeos-executor",
        "ryeos-api", "ryeos-node", "ryeos-ui",
    },
    "ryeos-engine": {
        "ryeos-app", "ryeos-executor", "ryeos-api", "ryeos-node", "ryeos-ui",
    },
    "ryeos-runtime": {
        "ryeos-app", "ryeos-executor", "ryeos-api", "ryeos-node", "ryeos-ui",
    },
    "ryeos-app": {"ryeos-executor", "ryeos-api", "ryeos-node", "ryeos-ui"},
    "ryeos-executor": {"ryeos-api", "ryeos-node", "ryeos-ui"},
}


def workspace_packages() -> dict[str, Path]:
    root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    packages: dict[str, Path] = {}
    for member in root_manifest["workspace"]["members"]:
        manifest = ROOT / member / "Cargo.toml"
        data = tomllib.loads(manifest.read_text())
        packages[data["package"]["name"]] = manifest
    return packages


def dependencies(manifest: Path, workspace: set[str]) -> set[str]:
    data = tomllib.loads(manifest.read_text())
    found: set[str] = set()
    tables = [data.get("dependencies", {}), data.get("build-dependencies", {})]
    for target in data.get("target", {}).values():
        tables.extend((target.get("dependencies", {}), target.get("build-dependencies", {})))
    for table in tables:
        for name, spec in table.items():
            package = spec.get("package", name) if isinstance(spec, dict) else name
            if package in workspace:
                found.add(package)
    return found


def find_cycle(graph: dict[str, set[str]]) -> list[str] | None:
    active: list[str] = []
    visited: set[str] = set()

    def visit(node: str) -> list[str] | None:
        if node in active:
            start = active.index(node)
            return active[start:] + [node]
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


def main() -> int:
    packages = workspace_packages()
    graph = {
        name: dependencies(manifest, set(packages))
        for name, manifest in packages.items()
    }
    failures: list[str] = []
    for owner, denied in FORBIDDEN.items():
        for dependency in sorted(graph.get(owner, set()) & denied):
            failures.append(f"forbidden dependency: {owner} -> {dependency}")
    cycle = find_cycle(graph)
    if cycle:
        failures.append(f"workspace dependency cycle: {' -> '.join(cycle)}")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("dependency layers: clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
