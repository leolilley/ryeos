#!/usr/bin/env python3
# ryeos:signed:2026-11-18T05:02:16Z:94a3c78471de1923246ba0230a9df16619c2fd68f61944fc39869cbb48aad2e1:J7w+IUWDhjye7ZYKYWiKR8ZcrANXNRrdSvV3tKP3HWHYC99qw8juIGvQgMHOuazJdnpARxWo3htx5bX2XM4FAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Check the one-way dependency rule between client crates and daemon-side crates
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

"""Client/daemon layering check over `cargo metadata --no-deps`.

Covers both Cargo workspaces: the root one and the browser renderer's own
(crates/clients/rune). Reads workspace manifests only (no compilation, no network). Dependency kinds
are kept apart: normal, build and dev. Rules:

1. A client crate has no normal or build dependency on a daemon-side crate.
2. A daemon-side crate has no normal or build dependency on a client crate.
3. Dev-dependencies are checked against the same sets, and only the pairs in
   DEV_EXCEPTIONS are permitted.
4. `ryeos-cli` is neither client nor daemon-side. Its daemon-side
   dependencies must be listed in CLI_ALLOWED: a new one fails, removing one
   is always fine.

`ryeos-ui` is the daemon-hosted presentation surface that serves the client
crates, so it consumes them by design. It is not in DAEMON_SIDE (rule 2 does
not apply to it) but it is in CLIENT_FORBIDS, so no client crate may depend on
it. Rule 2 also has no reach into `ryeos-cli`, which uses the client transport.

Usage: client-layering.py [--project-path DIR]
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

CLIENT = frozenset({
    "ryeos-client-base",
    "ryeos-client-rune",
    "ryeos-client-runtime",
    "ryeos-client-terminal",
    "ryeos-client-transport",
    "ryeos-client-web",
    "ryeos-ui-presentation-contract",
})

DAEMON_SIDE = frozenset({
    "ryeos-api",
    "ryeos-app",
    "ryeos-bundle",
    "ryeos-node",
    "ryeos-scheduler",
    "ryeos-scheduler-service",
    "ryeos-state",
    "ryeosd",
})

# Crates a client must not depend on: the daemon-side set plus the
# daemon-hosted UI surface.
CLIENT_FORBIDS = DAEMON_SIDE | {"ryeos-ui"}

# (owner, dependency, kind) triples allowed despite the rules above.
DEV_EXCEPTIONS = frozenset({
    # Tests resolve the workspace root through the engine's test support.
    ("ryeos-client-base", "ryeos-engine", "dev"),
    # Daemon tests drive the client base against a live daemon.
    ("ryeosd", "ryeos-client-base", "dev"),
})

CLI = "ryeos-cli"
CLI_ALLOWED = frozenset({
    ("ryeos-api", "normal"),
    ("ryeos-app", "normal"),
    ("ryeos-bundle", "normal"),
    ("ryeos-node", "normal"),
    ("ryeos-scheduler-service", "normal"),
    ("ryeos-state", "normal"),
})


# The root workspace, then the browser renderer's own workspace.
WORKSPACES = (None, "crates/clients/rune/Cargo.toml")


def kind_of(raw):
    return raw if raw in ("build", "dev") else "normal"


def check(packages):
    names = {p["name"] for p in packages}
    for group in (CLIENT, DAEMON_SIDE, CLIENT_FORBIDS | {CLI}):
        missing = sorted(group - names)
        if missing:
            return [f"rule names crates absent from the workspace: {', '.join(missing)}"]
    failures = []
    for package in sorted(packages, key=lambda p: p["name"]):
        owner = package["name"]
        for dep in package["dependencies"]:
            name, kind = dep["name"], kind_of(dep["kind"])
            if name not in names:
                continue
            if (owner, name, kind) in DEV_EXCEPTIONS:
                continue
            if owner in CLIENT and name in CLIENT_FORBIDS:
                failures.append(f"{owner}: client crate depends on daemon-side crate {name} ({kind})")
            elif owner in DAEMON_SIDE and name in CLIENT:
                failures.append(f"{owner}: daemon-side crate depends on client crate {name} ({kind})")
            elif owner == CLI and name in DAEMON_SIDE and (name, kind) not in CLI_ALLOWED:
                failures.append(
                    f"{owner}: new daemon-side dependency {name} ({kind}) is not in the CLI allowlist")
    return failures


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", type=Path, default=Path.cwd())
    args = parser.parse_args()
    packages = {}
    for manifest in WORKSPACES:
        command = ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"]
        if manifest:
            command += ["--manifest-path", manifest]
        result = subprocess.run(command, cwd=args.project_path, capture_output=True, text=True)
        if result.returncode != 0:
            print(result.stderr, file=sys.stderr)
            return 2
        for package in json.loads(result.stdout)["packages"]:
            packages.setdefault(package["name"], package)
    failures = check(list(packages.values()))
    for line in failures:
        print("client layering: " + line, file=sys.stderr)
    if not failures:
        print("client layering: ok")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
