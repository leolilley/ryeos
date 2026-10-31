#!/usr/bin/env python3
"""Fail when a runtime image ships a bundle whose services need a daemon feature
the image's ryeosd was not compiled with.

An installed operational service with no compiled handler stops the daemon at
startup. The handler exists only when the matching ryeosd Cargo feature is on,
so a bundle set and its daemon's feature list must agree. This check reads
Dockerfile.release without running Cargo:

  * every final stage that sets RYEOS_INIT_NODE_PROFILE names an installed
    bundle set (through bundle-sets.sh);
  * the bundle set needs a feature for each bundle that carries services whose
    handlers are feature gated (SERVICE_FEATURES below, matched against the
    bundle trees tracked in git);
  * the daemon the stage copies is either a node pair stage, whose feature list
    is what its own `ryeos_bundle_set_cargo_features` call selects, a
    publication stage (built with the feature list of the `release-artifacts`
    set), or the scheduler-free pair (no features).

SERVICE_FEATURES is also cross-checked against the signed payload-ownership
selection, so the cargo feature derivation and the service inventory cannot
drift from each other either.

Usage: python3 -B scripts/release/check-composition-daemon-features.py [--root DIR]
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

# (bundle, services subdirectory) -> ryeosd feature compiling its handlers.
SERVICE_FEATURES = {
    ("directive", "model-providers"): "model-provider-setup",
    ("scheduling", "scheduler"): "scheduling",
}
PUBLICATION_STAGES = {"composition-publication", "contained-publication", "compiled-publication"}
PUBLICATION_SET = "release-artifacts"
NO_FEATURE_STAGES = {"scheduler-free-publication"}


def sh(root, script):
    result = subprocess.run(["bash", "-c", f'source "{root}/scripts/pkg/bundle-sets.sh"; {script}'],
                            capture_output=True, text=True, cwd=root)
    if result.returncode != 0:
        raise SystemExit(f"bundle-sets.sh: {script}: {result.stderr.strip()}")
    return result.stdout.split()


def stages(text):
    """Map stage name to its instructions, continuation lines joined."""
    joined = re.sub(r"\\\n\s*", " ", text)
    result, current = {}, None
    for line in joined.splitlines():
        match = re.match(r"FROM\s+\S+\s+AS\s+(\S+)", line)
        if match:
            current = match.group(1)
            result[current] = []
        elif current and line.strip() and not line.lstrip().startswith("#"):
            result[current].append(line.strip())
    return {name: "\n".join(lines) for name, lines in result.items()}


def tracked_service_dirs(root):
    files = subprocess.run(["git", "ls-files", "bundles"], capture_output=True, text=True,
                           cwd=root, check=True).stdout.split("\n")
    found = set()
    for path in files:
        match = re.match(r"bundles/([^/]+)/\.ai/services/([^/]+)/", path)
        if match:
            found.add((match.group(1), match.group(2)))
    return found


def needed_features(bundles, service_dirs):
    return {feature for (bundle, subdir), feature in SERVICE_FEATURES.items()
            if bundle in bundles and (bundle, subdir) in service_dirs}


def daemon_features(root, stage_map, source):
    """The ryeosd features a copied daemon was compiled with, or None if unknown."""
    if source in NO_FEATURE_STAGES:
        return set()
    if source in PUBLICATION_STAGES:
        return selected(root, PUBLICATION_SET)
    body = stage_map.get(source)
    if body is None:
        return None
    match = re.search(r"ryeos_bundle_set_cargo_features (\S+) ", body)
    if not match:
        return set()
    if "--no-default-features -p ryeosd" not in body:
        return None
    return selected(root, match.group(1))


def selected(root, set_name):
    out = sh(root, f"ryeos_bundle_set_cargo_features {set_name} ryeosd")
    return {item.split("/", 1)[1] for entry in out for item in entry.split(",") if item}


def check(root, dockerfile_text, service_dirs):
    stage_map = stages(dockerfile_text)
    problems, checked = [], []
    for name, body in stage_map.items():
        profile = re.search(r"^ENV RYEOS_INIT_NODE_PROFILE=(\S+)$", body, re.M)
        daemon = re.search(r"^COPY --from=(\S+) \S+/ryeosd\s+/usr/local/bin/ryeosd$", body, re.M)
        if not profile or not daemon:
            continue
        set_name = sh(root, f"ryeos_node_init_profile_bundle_set {profile.group(1)}")[0]
        need = needed_features(set(sh(root, f"ryeos_bundle_set_names {set_name}")), service_dirs)
        have = daemon_features(root, stage_map, daemon.group(1))
        checked.append((name, set_name, daemon.group(1), sorted(need)))
        if have is None:
            problems.append(f"{name}: cannot determine the features of daemon stage {daemon.group(1)}")
        elif need - have:
            problems.append(f"{name}: bundle set {set_name} needs ryeosd feature(s) "
                            f"{sorted(need - have)} but its daemon from {daemon.group(1)} has {sorted(have)}")
    return problems, checked


def ownership_agrees(root):
    problems = []
    for (bundle, _), feature in SERVICE_FEATURES.items():
        for set_name in sh(root, "ryeos_bundle_set_ids") + [PUBLICATION_SET]:
            members = sh(root, f"ryeos_bundle_set_names {set_name}")
            if bundle in members and feature not in selected(root, set_name):
                problems.append(f"set {set_name} contains {bundle} but ownership selects no ryeosd/{feature}")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=str(Path(__file__).resolve().parents[2]))
    root = Path(parser.parse_args(argv).root)
    text = (root / "Dockerfile.release").read_text()
    problems, checked = check(root, text, tracked_service_dirs(root))
    problems += ownership_agrees(root)
    for name, set_name, source, need in checked:
        print(f"{name}: set {set_name}, daemon from {source}, needs {need}")
    for problem in problems:
        print(f"FAIL {problem}", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
