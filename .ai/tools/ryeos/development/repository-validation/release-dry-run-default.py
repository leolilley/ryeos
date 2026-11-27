#!/usr/bin/env python3
# ryeos:signed:2026-11-27T05:57:19Z:2d98ff83807b1923f890e2de77010e88bf4b9a950448a50a18546ff275bd90d2:zplBMTnpYAK3n6LD2A4gnRewjhhhBlI2qV6N/PdFOp0AVIo9z98La8cWUpvuF0HJx8TRvX3jJ/WVnKCSiLh1DA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Check that every publish-* workflow rehearses by default, refuses a real dispatch, and gates every publishing step
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

"""A dispatch of a publish-* workflow never publishes.

Every .github/workflows/publish-*.yml publishes with the official key or to a
registry. A real release comes only from a pushed tag, which carries no
inputs. For each such file this check holds:

1. `on.push` names tags only.
2. If the file has `workflow_dispatch`, it has a boolean input `dry_run` whose
   default is `true`.
3. The first step of the first job refuses a dispatch with `dry_run` false
   (`if:` names `github.event_name == 'workflow_dispatch'` and
   `!inputs.dry_run`, and the step runs `exit 1`), before any step or job
   header reads a secret. Every other job needs another job.
4. Every step that writes outside the runner, found by the closed marker list
   below, carries `!inputs.dry_run` in its own `if:` or its job's `if:`. A
   bake is allowed ungated only if its text sets `cacheonly` under
   `inputs.dry_run`. A new kind of publishing step must be added to
   MARKERS; until then the check cannot see it.
5. No line reads `github.event.inputs.dry_run` (a string, where "false" is
   truthy).

The files are read as text by indentation, with the standard library only.
Anything not found is a failure, never a pass.

--check       check every publish-*.yml under --project-path; exit 1 on failure
--self-test   run the check over altered copies and require each alteration
              to fail with its own message and the unaltered text to pass

Usage: release-dry-run-default.py (--check | --self-test) [--project-path DIR]
"""

import argparse
import re
import sys
from pathlib import Path

WORKFLOW_DIR = ".github/workflows"

# Operations that write outside the runner. Closed: extend it with a new kind
# of publishing step, with a fixture in self_test.
MARKERS = {
    "gh release write": re.compile(r"\bgh\s+release\s+(create|upload|edit|delete)\b"),
    "gh api write": re.compile(r"\bgh\s+api\b[^\n]*(--method|-X)\s+(POST|PUT|PATCH|DELETE)\b"),
    "registry login": re.compile(r"docker/login-action@"),
    "cosign sign": re.compile(r"\bcosign\s+(sign|attest)\b"),
    "attestation action": re.compile(r"actions/attest[-\w]*@"),
    "image push": re.compile(r"--push\b|\bdocker\s+push\b|\bimagetools\s+create\b|docker/build-push-action@"),
    "bake": re.compile(r"docker/bake-action@"),
}


def _live(lines):
    return [l for l in lines if l.strip() and not l.lstrip().startswith("#")]


def _block(lines, start, indent):
    """Lines after lines[start] that are blank or indented deeper than indent."""
    end = start + 1
    while end < len(lines) and (
        not lines[end].strip() or len(lines[end]) - len(lines[end].lstrip(" ")) > indent
    ):
        end += 1
    return lines[start + 1 : end]


def _key_line(lines, indent, key):
    pat = re.compile(r"^ {%d}%s:(\s|$)" % (indent, re.escape(key)))
    for i, l in enumerate(lines):
        if pat.match(l):
            return i
    return None


def _scalar(block, indent, key):
    pat = re.compile(r"^ {%d}%s:\s*(.*?)\s*$" % (indent, re.escape(key)))
    for l in block:
        m = pat.match(l)
        if m:
            return m.group(1).split(" #", 1)[0].strip().strip("\"'")
    return None


def _jobs(lines):
    """[(name, header_lines, steps)] where steps are lists of raw lines."""
    i = _key_line(lines, 0, "jobs")
    if i is None:
        return None
    body = _block(lines, i, 0)
    jobs = []
    starts = [k for k, l in enumerate(body) if re.match(r"^  [\w-]+:\s*$", l)]
    for n, k in enumerate(starts):
        end = starts[n + 1] if n + 1 < len(starts) else len(body)
        job = body[k:end]
        name = job[0].strip()[:-1]
        si = _key_line(job, 4, "steps")
        if si is None:
            jobs.append((name, job, []))
            continue
        header, rest = job[: si + 1], job[si + 1 :]
        steps, cur = [], None
        for l in rest:
            if re.match(r"^ {6}- ", l):
                cur = [l]
                steps.append(cur)
            elif cur is not None:
                cur.append(l)
        jobs.append((name, header, steps))
    return jobs


def _step_if(step):
    for n, l in enumerate(step):
        m = re.match(r"^ {6}- if:\s*(.*)$", l) if n == 0 else re.match(r"^ {8}if:\s*(.*)$", l)
        if m:
            return m.group(1)
    return ""


def check_text(name, text):
    """Failures for one workflow file; empty means it passes."""
    lines = text.splitlines()
    live = _live(lines)
    failures = []
    for l in live:
        if "github.event.inputs.dry_run" in l:
            failures.append("reads `github.event.inputs.dry_run` (a string: \"false\" is truthy); use `inputs.dry_run`")
            break

    oi = _key_line(live, 0, "on")
    if oi is None:
        oi = _key_line(live, 0, '"on"')
    if oi is None:
        return failures + ["no top-level `on:` block"]
    trigger = _block(live, oi, 0)
    pi = _key_line(trigger, 2, "push")
    if pi is not None:
        keys = [re.match(r"^ {4}([\w-]+):", l).group(1) for l in _block(trigger, pi, 2) if re.match(r"^ {4}[\w-]+:", l)]
        if keys != ["tags"]:
            failures.append(f"`on.push` must name tags only, found keys {keys or 'none'}")
    di = _key_line(trigger, 2, "workflow_dispatch")
    if di is not None:
        inputs = _key_line(_block(trigger, di, 2), 4, "inputs")
        dry = None
        if inputs is not None:
            block = _block(_block(trigger, di, 2), inputs, 4)
            k = _key_line(block, 6, "dry_run")
            dry = _block(block, k, 6) if k is not None else None
        if dry is None:
            failures.append("workflow_dispatch has no `dry_run` input")
        else:
            if _scalar(dry, 8, "type") != "boolean":
                failures.append("`dry_run` must be `type: boolean`")
            default = _scalar(dry, 8, "default")
            if default != "true":
                failures.append(f"`dry_run` must default to `true`, found {default!r}: a dispatch that omits it would publish")

    jobs = _jobs(live)
    if not jobs:
        return failures + ["no jobs found"]
    if di is not None:
        header, steps = jobs[0][1], jobs[0][2]
        first = steps[0] if steps else []
        text0 = "\n".join(first)
        cond = _step_if(first)
        if not (
            "github.event_name == 'workflow_dispatch'" in cond
            and "!inputs.dry_run" in cond
            and re.search(r"\bexit 1\b", text0)
            and "secrets." not in text0
        ):
            failures.append(
                "the first step of the first job must refuse a dispatch with dry_run false "
                "(`if:` with github.event_name == 'workflow_dispatch' and !inputs.dry_run, running `exit 1`), "
                "before any secret is read"
            )
        if any("secrets." in l for l in header):
            failures.append("the first job's header reads a secret before the refusal step")
        for jname, jheader, _ in jobs[1:]:
            if _key_line(jheader, 4, "needs") is None:
                failures.append(f"job `{jname}` has no `needs`, so it could run without the first job's refusal")

    for jname, header, steps in jobs:
        job_if = next((m.group(1) for l in header if (m := re.match(r"^ {4}if:\s*(.*)$", l))), "")
        for step in steps:
            body = "\n".join(step)
            gated = "!inputs.dry_run" in _step_if(step) or "!inputs.dry_run" in job_if
            for label, pat in MARKERS.items():
                if not pat.search(body):
                    continue
                if label == "bake" and "cacheonly" in body and "inputs.dry_run &&" in body:
                    continue
                if not gated:
                    title = next((l.strip() for l in step if "name:" in l or "uses:" in l), step[0].strip())
                    failures.append(
                        f"job `{jname}`: ungated publishing step ({label}): {title}; "
                        "it needs `!inputs.dry_run` in its own or its job's `if:`"
                    )
                    break
    return failures


def _files(project):
    return sorted((project / WORKFLOW_DIR).glob("publish-*.yml"))


def check(project):
    files = _files(project)
    if not files:
        print(f"release-dry-run-default: no publish-*.yml under {WORKFLOW_DIR}", file=sys.stderr)
        return 1
    bad = 0
    for path in files:
        failures = check_text(path.name, path.read_text(encoding="utf-8"))
        for failure in failures:
            print(f"release-dry-run-default: {path.name}: {failure}", file=sys.stderr)
        if failures:
            bad += 1
        else:
            print(f"release-dry-run-default: {path.name}: rehearses by default, refuses a real dispatch, gates every publishing step")
    return 1 if bad else 0


def _extra_job(text, step):
    """The workflow plus one ungated job (needing the first job) running `step`."""
    first = re.search(r"^jobs:\n  ([\w-]+):", text, re.M)
    if not first:
        return text
    return (
        text.rstrip("\n")
        + f"\n\n  fixture:\n    needs: {first.group(1)}\n    runs-on: ubuntu-latest\n    steps:\n{step}"
    )


def _alterations(text):
    """name -> (altered text, a substring its failure message must contain)."""
    out = {}
    refuse_if = "if: ${{ github.event_name == 'workflow_dispatch' && !inputs.dry_run }}"
    first_steps = "    steps:\n"
    out["default false"] = (re.sub(r"(dry_run:[^\n]*\n(?:        [^\n]*\n)*?        default: )true", r"\1false", text, count=1), "must default to `true`")
    out["default removed"] = (re.sub(r"(dry_run:[^\n]*\n(?:        [^\n]*\n)*?)        default: true\n", r"\1", text, count=1), "must default to `true`")
    out["type string"] = (re.sub(r"(dry_run:[^\n]*\n(?:        [^\n]*\n)*?        type: )boolean", r"\1string", text, count=1), "type: boolean")
    out["input removed"] = (text.replace("      dry_run:\n", "      dry_run_renamed:\n", 1), "no `dry_run` input")
    out["refusal disabled"] = (text.replace(refuse_if, "if: ${{ false }}", 1), "must refuse a dispatch")
    out["secret read before the refusal"] = (
        text.replace(first_steps, first_steps + "      - name: Early\n        env:\n          K: ${{ secrets.RYEOS_PUBLISHER_KEY }}\n        run: true\n\n", 1),
        "must refuse a dispatch",
    )
    out["event.inputs string"] = (_extra_job(text, "      - name: Late\n        if: ${{ github.event.inputs.dry_run }}\n        run: true\n"), "github.event.inputs.dry_run")
    out["branch push"] = (text.replace("  push:\n    tags:\n", "  push:\n    branches:\n      - main\n    tags:\n", 1), "tags only")
    out["ungated gh release upload"] = (_extra_job(text, "      - name: Upload\n        run: gh release upload v1 file\n"), "ungated publishing step (gh release write)")
    out["ungated gh api write"] = (_extra_job(text, "      - name: Patch\n        run: gh api --method PATCH repos/x/y\n"), "ungated publishing step (gh api write)")
    out["ungated cosign sign"] = (_extra_job(text, "      - name: Sign\n        run: cosign sign --yes img\n"), "ungated publishing step (cosign sign)")
    out["ungated image push"] = (_extra_job(text, "      - name: Push\n        run: docker buildx imagetools create -t a b\n"), "ungated publishing step (image push)")
    out["ungated registry login"] = (_extra_job(text, "      - uses: docker/login-action@abc # v3\n"), "ungated publishing step (registry login)")
    out["bake not cacheonly"] = (text.replace("cacheonly", "registry"), "ungated publishing step (bake)")
    out["job without needs"] = (text.rstrip("\n") + "\n  extra:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n", "has no `needs`")
    return out


def self_test(project):
    files = _files(project)
    if not files:
        print("release-dry-run-default self-test: no publish-*.yml", file=sys.stderr)
        return 1
    bad = 0
    applied = set()
    for path in files:
        text = path.read_text(encoding="utf-8")
        if check_text(path.name, text):
            print(f"release-dry-run-default self-test: {path.name} itself fails", file=sys.stderr)
            bad += 1
            continue
        for name, (altered, expect) in _alterations(text).items():
            if altered == text:
                continue  # this file has nothing of that shape
            applied.add(name)
            failures = check_text(path.name, altered)
            if not any(expect in f for f in failures):
                print(f"release-dry-run-default self-test: {path.name}: {name}: expected a failure containing {expect!r}, got {failures}", file=sys.stderr)
                bad += 1
            else:
                print(f"release-dry-run-default self-test: {path.name}: {name}: fails as it must")
    # every kind of alteration must have been exercised on at least one file
    for n in _alterations("x"):
        if n not in applied:
            print(f"release-dry-run-default self-test: alteration {n!r} applied to no file", file=sys.stderr)
            bad += 1
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
