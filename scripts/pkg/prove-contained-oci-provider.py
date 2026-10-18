#!/usr/bin/env python3
"""Read-only check that a contained node's process-scope proofs come from the OCI provider.

The guarantee asserted is that every provided.* process-scope proof is measured
and attributed to external_controller:oci. Doctor has already verified the
signature of the capability report this reads. Floor entries constrained to the
OCI provider are recorded as evidence and checked when present, but their
absence never fails the run.
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

PROVIDER = "external_controller:oci"
PROFILE = "contained-workflow"
TERMINATION = "termination:scope"
SCOPE_NAMES = ("quiescence:scope", TERMINATION, "recovery:scope", "limits:scope")


def capabilities_detail(doctor):
    for check in doctor.get("checks", []):
        if check.get("name") == "capabilities":
            return check.get("detail") or {}
    return None


def label(entry):
    if isinstance(entry, str):
        return entry
    if entry.get("any_of"):
        return "any_of(" + ", ".join(entry["any_of"]) + ")"
    return entry.get("name", "")


def verify(doctor):
    """Return (failures, evidence) for one doctor JSON document."""
    detail = capabilities_detail(doctor)
    if detail is None or not isinstance(detail.get("report"), dict):
        return ["doctor output has no signed capability report under checks[capabilities]"], {}
    report = detail["report"]
    provided = report.get("provided", {})
    floor = report.get("floor", {})
    failures = []

    if detail.get("current") is not True:
        failures.append(f"capability report is not current: {detail.get('current')!r}")

    proofs = {}
    for name in SCOPE_NAMES:
        proof = provided.get(name)
        proofs[name] = proof
        if proof is None:
            failures.append(f"{name} is not provided")
            continue
        if proof.get("source") != "measured":
            failures.append(f"{name} source is {proof.get('source')!r}, want 'measured'")
        if proof.get("provider") != PROVIDER:
            failures.append(f"{name} provider is {proof.get('provider')!r}, want {PROVIDER!r}")

    constrained = {}
    for entry in floor.get("requires", []):
        if isinstance(entry, dict) and PROVIDER in entry.get("providers", []):
            name = label(entry)
            met = floor.get("met_by", {}).get(name)
            constrained[name] = met
            if met is None:
                failures.append(f"floor entry {name} constrained to {PROVIDER} has no met_by proof")
            elif met.get("provider") != PROVIDER:
                failures.append(f"floor entry {name} met by provider {met.get('provider')!r}, want {PROVIDER!r}")

    profile = report.get("composition", {}).get("profile")
    if profile != PROFILE:
        failures.append(f"node profile is {profile!r}, want {PROFILE!r}")
    if floor.get("admitted") is not True:
        failures.append(f"capability floor is not admitted: {floor.get('unmet', {})}")

    evidence = {
        "profile": profile,
        "floor_admitted": floor.get("admitted"),
        "scope_proofs": proofs,
        "provider_constrained_floor": constrained,
        "failures": failures,
    }
    return failures, evidence


def load_doctor(args):
    try:
        if args.report:
            text = Path(args.report).read_text()
            code, err = 0, ""
        else:
            run = subprocess.run(["docker", "exec", args.container, "ryeos", "node", "doctor", "--json"],
                                 capture_output=True, text=True, timeout=120)
            text, code, err = run.stdout, run.returncode, run.stderr
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(f"cannot obtain doctor output: {error}")
    try:
        doctor = json.loads(text)
    except json.JSONDecodeError:
        raise SystemExit(f"doctor produced no JSON (exit {code}): {err.strip()[:200]}")
    if not isinstance(doctor, dict):
        raise SystemExit("doctor output is not a JSON object")
    return doctor


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--container")
    source.add_argument("--report")
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    failures, evidence = verify(load_doctor(args))
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "oci-provider-evidence.json").write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
