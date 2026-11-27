#!/usr/bin/env python3
# ryeos:signed:2026-11-27T05:52:54Z:3fbbb24ef0f6e28b140cf50221ba0d2f723ef0677678bc3533125ac3b4ac32fe:M4Y+kUlHMsYsKAk7UV9a6s4S5zknfyV/Y0mlo9dpcp69jZEToXtw05YdcTkh8kVYaYhekYQC6BMbW3lF+pLXCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Check that a dispatch of the release workflow is a dry run unless the dispatcher asks otherwise
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

"""The release workflow's dispatch default is a dry run.

.github/workflows/publish-ryeosd.yml publishes with the official key. Its
manual dispatch takes a boolean input `dry_run`; every publishing step is
gated on `!inputs.dry_run`. A dispatch that omits the input must therefore
rehearse, never publish: the input's default must be `true`.

A pushed tag carries no inputs, so the default does not apply to it and the
tag path still publishes. The check also holds that the push trigger names
tags only, so a push to a branch never runs the workflow.

The workflow is read as text with indentation, without a YAML library, so
the check needs nothing beyond the standard library. Anything it cannot
find (the trigger block, the input, its type or default) is a failure, never
a pass.

--check       check the workflow under --project-path; exit 1 on failure
--self-test   run the check over altered copies of the workflow and require
              each alteration to fail and the unaltered text to pass

Usage: release-dry-run-default.py (--check | --self-test) [--project-path DIR]
"""

import argparse
import sys
from pathlib import Path

WORKFLOW = ".github/workflows/publish-ryeosd.yml"


def _lines(text):
    """(indent, stripped) for each non-blank, non-comment line."""
    out = []
    for raw in text.splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        out.append((len(raw) - len(raw.lstrip(" ")), stripped))
    return out


def _children(lines, start, parent_indent):
    """Lines under lines[start - 1] (indented deeper than parent_indent)."""
    end = start
    while end < len(lines) and lines[end][0] > parent_indent:
        end += 1
    return lines[start:end]


def _find_key(lines, key):
    """The block under `key:` among the shallowest lines of `lines`."""
    if not lines:
        return None
    top = min(indent for indent, _ in lines)
    for i, (indent, stripped) in enumerate(lines):
        if indent == top and stripped == f"{key}:":
            return _children(lines, i + 1, indent)
    return None


def _scalar(lines, key):
    """The value of `key: value` among the shallowest lines of `lines`."""
    if not lines:
        return None
    top = min(indent for indent, _ in lines)
    for indent, stripped in lines:
        if indent == top and stripped.startswith(f"{key}:"):
            value = stripped[len(key) + 1 :].split(" #", 1)[0].strip()
            return value.strip("\"'")
    return None


def check_text(text):
    """Return a list of failures; empty means the workflow passes."""
    lines = _lines(text)
    # The YAML key `on` is written bare or quoted.
    trigger = _find_key(lines, "on")
    if trigger is None:
        trigger = _find_key(lines, '"on"')
    if trigger is None:
        return ["no top-level `on:` block"]
    failures = []

    push = _find_key(trigger, "push")
    if push is None:
        failures.append("no `on.push` trigger: the tag path is missing")
    else:
        top = min(indent for indent, _ in push) if push else 0
        keys = [s.split(":", 1)[0] for indent, s in push if indent == top]
        if keys != ["tags"]:
            failures.append(
                f"`on.push` must name tags only, found keys {keys or 'none'}"
            )

    dispatch = _find_key(trigger, "workflow_dispatch")
    if dispatch is None:
        failures.append("no `on.workflow_dispatch` trigger")
        return failures
    dry_run = _find_key(_find_key(dispatch, "inputs") or [], "dry_run")
    if dry_run is None:
        failures.append("no `on.workflow_dispatch.inputs.dry_run` input")
        return failures
    kind = _scalar(dry_run, "type")
    if kind != "boolean":
        failures.append(f"`dry_run` must be `type: boolean`, found {kind!r}")
    default = _scalar(dry_run, "default")
    if default != "true":
        failures.append(
            f"`dry_run` must default to `true`, found {default!r}: a dispatch "
            "that omits it would publish with the official key"
        )
    return failures


def check(project):
    path = project / WORKFLOW
    if not path.is_file():
        print(f"release-dry-run-default: {WORKFLOW} not found", file=sys.stderr)
        return 1
    failures = check_text(path.read_text(encoding="utf-8"))
    for failure in failures:
        print(f"release-dry-run-default: {WORKFLOW}: {failure}", file=sys.stderr)
    if failures:
        return 1
    print(f"release-dry-run-default: {WORKFLOW}: dispatch defaults to a dry run")
    return 0


def self_test(project):
    text = (project / WORKFLOW).read_text(encoding="utf-8")
    if check_text(text):
        print("release-dry-run-default self-test: the workflow itself fails", file=sys.stderr)
        return 1
    marker = "        default: true\n        type: boolean\n"
    if text.count(marker) != 1:
        print(
            "release-dry-run-default self-test: cannot locate the dry_run "
            "default to alter",
            file=sys.stderr,
        )
        return 1
    alterations = {
        "default false": text.replace(marker, "        default: false\n        type: boolean\n"),
        "default removed": text.replace(marker, "        type: boolean\n"),
        "type string": text.replace(marker, "        default: true\n        type: string\n"),
        "input removed": text.replace("      dry_run:\n", "      dry_run_renamed:\n"),
        "branch push added": text.replace(
            "  push:\n    tags:\n", "  push:\n    branches:\n      - main\n    tags:\n"
        ),
    }
    bad = 0
    for name, altered in alterations.items():
        if altered == text:
            print(f"release-dry-run-default self-test: {name}: alteration did not apply", file=sys.stderr)
            bad += 1
        elif not check_text(altered):
            print(f"release-dry-run-default self-test: {name}: passed, must fail", file=sys.stderr)
            bad += 1
        else:
            print(f"release-dry-run-default self-test: {name}: fails as it must")
    return 1 if bad else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--self-test", action="store_true")
    parser.add_argument("--project-path", default=".")
    args = parser.parse_args()
    project = Path(args.project_path)
    return self_test(project) if args.self_test else check(project)


if __name__ == "__main__":
    sys.exit(main())
