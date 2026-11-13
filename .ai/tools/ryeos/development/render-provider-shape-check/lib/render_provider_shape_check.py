#!/usr/bin/env python3
# ryeos:signed:2026-11-12T23:59:42Z:4070721466463f2c4dae094579af2a117389bc8e9e05a57848bea2c35fef0b9a:02M/S3YwKRrDEhFp/8dIoWQo09jZU1q0fzHOaG1+8fixJAgKJE5o6LcGEJyYOokiI8XS7vsACl5ZvyR6Ghd4CQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Check Render's current sandbox response still fits the adapter's strict parser.

External development host tool; it grants no RyeOS authority. One free,
read-only GET of the owner's newest terminated sandbox, held to the shape the
render-sandbox adapter accepts. The accepted shape is read from the adapter's
own source (`RenderSandbox` and the structs and enums it names, with their
serde renames, defaults and `deny_unknown_fields`), so this check cannot drift
from the parser it protects. Provider drift then shows here instead of as a
refusal after a chain has spent. Prints field names and enum values, never a
credential.

  render_provider_shape_check.py [--owner-id ID] [--response FILE]
      [--worktree DIR] [--config-file F]

The owner id, API base and credential source come from
`config:development/ryeos/render-provider-shape`; --response checks a saved
response offline instead. Exit 0 fits, 1 changed, 2 cannot check.
"""

from __future__ import annotations

import argparse
import http.client
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import time
import urllib.parse
import urllib.request

_spec = importlib.util.spec_from_file_location(
    "dev_host", Path(__file__).resolve().parents[2] / "dev-host/lib/dev_host.py")
dev_host = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev_host)
ToolError = dev_host.ToolError

NAME = "render-provider-shape"
SCHEMA = "ryeos.development.render-provider-shape.v1"
KEYS = {"api_base", "owner_id", "api_key_env", "env_file", "adapter_source", "root_type"}
_ITEM = re.compile(r"((?:\s*#\[[^\]]*\]\s*)*)\s*(struct|enum)\s+([A-Za-z0-9_]+)\s*\{", re.S)


# ---- the adapter's accepted shape, from its Rust source -------------------------

def _attributes(text: str) -> str:
    return " ".join(re.findall(r"#\[serde\(([^\]]*)\)\]", text))


def _body(source: str, start: int) -> str:
    depth, index = 1, start
    while depth:
        depth += {"{": 1, "}": -1}.get(source[index], 0)
        index += 1
    return source[start:index - 1]


def _rename_all(name: str, rule: str | None) -> str:
    words = re.findall(r"[A-Z][a-z0-9]*", name)
    if rule == "lowercase":
        return name.lower()
    if rule == "kebab-case":
        return "-".join(w.lower() for w in words)
    if rule == "snake_case":
        return "_".join(w.lower() for w in words)
    if rule is None:
        return name
    raise ToolError(f"unsupported serde rename_all {rule!r}")


def adapter_shapes(source: str) -> dict:
    """Map each struct/enum name to its accepted JSON shape."""
    shapes = {}
    for match in _ITEM.finditer(source):
        attributes, kind, name = _attributes(match.group(1)), match.group(2), match.group(3)
        body = _body(source, match.end())
        body = re.sub(r"//[^\n]*", "", body)
        rename_all = re.search(r'rename_all\s*=\s*"([^"]+)"', attributes)
        rename_all = rename_all.group(1) if rename_all else None
        if kind == "enum":
            variants = re.findall(r"(?:#\[serde\(([^\]]*)\)\]\s*)?\b([A-Z][A-Za-z0-9]*)\s*(?:,|$)", body)
            values = set()
            for variant_attributes, variant in variants:
                renamed = re.search(r'rename\s*=\s*"([^"]+)"', variant_attributes or "")
                values.add(renamed.group(1) if renamed else _rename_all(variant, rename_all))
            shapes[name] = {"enum": values}
            continue
        fields = {}
        for field_attributes, field, rust_type in re.findall(
                r"((?:#\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:\s*([^,]+?)\s*,", body + ","):
            serde = _attributes(field_attributes)
            renamed = re.search(r'rename\s*=\s*"([^"]+)"', serde)
            key = renamed.group(1) if renamed else _rename_all(field, rename_all) if rename_all else field
            optional = rust_type.startswith("Option<") or re.search(r"\bdefault\b", serde) is not None
            fields[key] = {"type": rust_type.replace(" ", ""), "required": not optional}
        shapes[name] = {"fields": fields, "closed": "deny_unknown_fields" in attributes}
    return shapes


def check_shape(value, rust_type: str, shapes: dict, path: str, problems: list[str]) -> None:
    rust_type = rust_type.replace(" ", "")
    inner = re.fullmatch(r"(Option|Vec|Box)<(.+)>", rust_type)
    if inner:
        wrapper, rust_type = inner.groups()
        if wrapper == "Option" and value is None:
            return
        if wrapper == "Vec":
            if not isinstance(value, list):
                problems.append(f"{path}: expected a list")
                return
            for index, item in enumerate(value):
                check_shape(item, rust_type, shapes, f"{path}[{index}]", problems)
            return
        return check_shape(value, rust_type, shapes, path, problems)
    if rust_type == "String":
        if not isinstance(value, str):
            problems.append(f"{path}: expected a string")
    elif re.fullmatch(r"[ui](8|16|32|64|size)", rust_type):
        if type(value) is not int or (rust_type.startswith("u") and value < 0):
            problems.append(f"{path}: expected an integer ({rust_type})")
    elif rust_type == "bool":
        if not isinstance(value, bool):
            problems.append(f"{path}: expected a boolean")
    elif rust_type in shapes and "enum" in shapes[rust_type]:
        if value not in shapes[rust_type]["enum"]:
            problems.append(f"{path}: value {value!r} not in {sorted(shapes[rust_type]['enum'])}")
    elif rust_type in shapes:
        shape = shapes[rust_type]
        if not isinstance(value, dict):
            problems.append(f"{path}: expected an object")
            return
        if shape["closed"]:
            for key in sorted(set(value) - set(shape["fields"])):
                problems.append(f"{path}.{key}: field the adapter does not accept")
        for key, field in shape["fields"].items():
            if key not in value:
                if field["required"]:
                    problems.append(f"{path}.{key}: required field missing")
                continue
            check_shape(value[key], field["type"], shapes, f"{path}.{key}", problems)
    else:
        problems.append(f"{path}: adapter type {rust_type} is not understood by this check")


def check_response(response: dict, adapter_source: str, root_type: str) -> list[str]:
    shapes = adapter_shapes(adapter_source)
    if root_type not in shapes:
        raise ToolError(f"adapter source has no {root_type}")
    problems: list[str] = []
    check_shape(response, root_type, shapes, "sandbox", problems)
    return problems


# ---- the free provider read ----------------------------------------------------

def api_key(env_name: str, env_file: Path | None) -> str:
    if os.environ.get(env_name):
        return os.environ[env_name]
    if env_file is not None and env_file.is_file():
        for line in env_file.read_text().splitlines():
            line = line.strip().removeprefix("export ")
            if line.startswith(env_name + "="):
                return line.split("=", 1)[1].strip().strip("'\"")
    raise ToolError(f"no {env_name} in the environment or {env_file}")


def newest_terminated(api_base: str, owner_id: str, key: str) -> dict:
    def get(path: str, query: dict | None = None):
        url = api_base + path + "?" + urllib.parse.urlencode({"ownerId": owner_id, **(query or {})})
        request = urllib.request.Request(url, headers={
            "Authorization": "Bearer " + key, "Accept": "application/json",
            "User-Agent": "ryeos-render-provider-shape-check/1"})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    failure = None
    for attempt in range(1, 6):
        try:
            rows = get("/sandboxes", {"status": "terminated", "limit": 1})
            return get("/sandboxes/" + rows[0]["sandbox"]["id"])
        except (OSError, http.client.HTTPException, KeyError, IndexError, ValueError) as error:
            failure = type(error).__name__
            print(f"provider shape read {attempt} failed: {failure}", file=sys.stderr, flush=True)
            time.sleep(5 * attempt)
    raise ToolError(f"provider unreachable, shape not checked: {failure}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--owner-id")
    parser.add_argument("--response", type=Path, help="check a saved sandbox response, offline")
    parser.add_argument("--worktree")
    parser.add_argument("--config-file", type=Path)
    args = parser.parse_args(argv)
    try:
        worktree = dev_host.worktree_argument(args.worktree)
        path = args.config_file or dev_host.config_path(dev_host.REPOSITORY_ROOT, NAME)
        config = dev_host.load_config(path, NAME, SCHEMA, KEYS)
        base = {"WORKTREE": str(worktree)}
        source = (worktree / config["adapter_source"]).read_text()
        if args.response:
            response = json.loads(args.response.read_text())
        else:
            owner = args.owner_id or config["owner_id"]
            if not owner:
                raise ToolError("no owner id: pass --owner-id or set owner_id in a site config")
            env_file = dev_host.expand_path(config["env_file"], base) if config["env_file"] else None
            response = newest_terminated(dev_host.expand(config["api_base"], base), owner,
                                         api_key(config["api_key_env"], env_file))
        problems = check_response(response, source, config["root_type"])
    except (ToolError, OSError, ValueError) as error:
        print(f"render-provider-shape-check: {error}", file=sys.stderr)
        return 2
    print("provider shape", "CHANGED" if problems else "ok", "fields", sorted(response),
          "plan", response.get("plan"), "status", response.get("status"))
    for problem in problems:
        print("FAIL", problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
