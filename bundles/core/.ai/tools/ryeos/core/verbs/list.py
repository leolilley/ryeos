# ryeos:signed:2026-06-08T00:42:17Z:39f02cbc1badd3eaa4e3635ba2d8dcbb44db3de3711c086da850f3ff89ff7747:tX8TFbs1ISrEL3pn0FXGjdP6tgPpyA8tBeTqr+ZkMdxHrCZXQNPlst9GAERBMPdQ+R/VpNFO2/C0GJYfvbE3Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/core/verbs
#   version: "1.0.0"
#   tool_type: python
#   description: "List all known verbs and their aliases from installed bundles"
"""Verb list tool — reads verb and alias YAMLs from installed bundles."""

import json
import os
import sys
import glob as globmod


def _discover_roots(params: dict) -> list[str]:
    """Discover bundle roots from state dir and system data dir."""
    roots = []

    # System data dir (core bundle)
    if os.environ.get("RYE_SYSTEM_SPACE"):
        roots.append(os.environ["RYE_SYSTEM_SPACE"])

    # State dir bundles
    state_dir = os.environ.get("RYEOS_STATE_DIR", "")
    if state_dir:
        bundles_dir = os.path.join(state_dir, ".ai", "bundles")
        if os.path.isdir(bundles_dir):
            for entry in os.listdir(bundles_dir):
                path = os.path.join(bundles_dir, entry)
                if os.path.isdir(path):
                    roots.append(path)

    return roots


def _read_yamls(directory: str) -> list[dict]:
    """Read all YAML files in a directory, return parsed dicts."""
    import yaml

    results = []
    if not os.path.isdir(directory):
        return results
    for path in sorted(globmod.glob(os.path.join(directory, "*.yaml"))):
        try:
            with open(path) as f:
                content = f.read()
            data = yaml.safe_load(content)
            if data and isinstance(data, dict):
                data["_source_file"] = path
                results.append(data)
        except Exception:
            continue
    return results


def execute(params: dict, project_path: str) -> dict:
    """List all known verbs and their aliases."""
    surface_filter = params.get("surface")
    roots = _discover_roots(params)

    verbs = []
    aliases = []

    for root in roots:
        ai_dir = os.path.join(root, ".ai") if not root.endswith(".ai") else root

        # Read verbs
        verb_dir = os.path.join(ai_dir, "node", "verbs")
        for v in _read_yamls(verb_dir):
            verbs.append({
                "name": v.get("name", ""),
                "execute": v.get("execute"),
                "description": v.get("description", ""),
                "source": v.get("_source_file", ""),
            })

        # Read aliases
        alias_dir = os.path.join(ai_dir, "node", "aliases")
        for a in _read_yamls(alias_dir):
            alias_surface = a.get("surface", "")
            if surface_filter and alias_surface != surface_filter:
                continue
            aliases.append({
                "surface": alias_surface,
                "tokens": a.get("tokens", []),
                "verb": a.get("verb", ""),
                "description": a.get("description", ""),
                "deprecated": a.get("deprecated", False),
            })

    # Deduplicate by name/tokens
    seen_verbs = set()
    unique_verbs = []
    for v in verbs:
        key = v["name"]
        if key not in seen_verbs:
            seen_verbs.add(key)
            unique_verbs.append(v)

    seen_aliases = set()
    unique_aliases = []
    for a in aliases:
        key = (a["surface"], tuple(a["tokens"]))
        if key not in seen_aliases:
            seen_aliases.add(key)
            unique_aliases.append(a)

    # Build verb → aliases map
    verb_aliases = {}
    for a in unique_aliases:
        verb_aliases.setdefault(a["verb"], []).append(a)

    # Combine
    result_verbs = []
    for v in sorted(unique_verbs, key=lambda x: x["name"]):
        entry = {
            "name": v["name"],
            "execute": v["execute"],
            "description": v["description"],
        }
        va = verb_aliases.get(v["name"], [])
        if va:
            entry["aliases"] = va
        result_verbs.append(entry)

    return {
        "verbs": result_verbs,
        "total_verbs": len(result_verbs),
        "total_aliases": len(unique_aliases),
    }


if __name__ == "__main__":
    params = json.loads(sys.stdin.read())
    project_path = sys.argv[1] if len(sys.argv) > 1 else "."
    result = execute(params, project_path)
    print(json.dumps(result, indent=2, default=str))
