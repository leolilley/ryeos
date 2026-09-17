#!/usr/bin/env python3
"""Validate evidence collected by an installed contained-workflow adapter.

This driver intentionally performs no deployment or lifecycle mutation. The
administrator-selected adapter owns those operations and writes one bounded
evidence document. The repository source verifier deliberately refuses every
installed claim until an administrator-signed replacement verifier names and
authenticates an exact attestor. This driver therefore remains a fail-closed
qualification entry point, not a way for a checklist to attest itself.
"""

import argparse
import importlib.util
import json
from pathlib import Path
import sys


ROOT = Path(__file__).resolve().parents[3]
TOOL = ROOT / ".ai/tools/ryeos/development/hosted-oci-runtime/verify.py"
CONFIG = ROOT / ".ai/config/development/ryeos/hosted-oci-runtime.yaml"
MAX_BYTES = 262144


def load_verifier():
    spec = importlib.util.spec_from_file_location("hosted_oci_verify", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def unsigned_yaml_body(path):
    lines = path.read_text().splitlines()
    if not lines or not lines[0].startswith("# ryeos:signed:"):
        raise ValueError("qualification config is not signed")
    try:
        import yaml
    except ImportError as error:
        raise ValueError("PyYAML is required to load the signed qualification config") from error
    return yaml.safe_load("\n".join(lines[1:]))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    if not args.evidence.is_absolute() or args.evidence.is_symlink():
        raise ValueError("evidence must be one exact absolute regular file")
    size = args.evidence.stat().st_size
    if size <= 0 or size > MAX_BYTES or not args.evidence.is_file():
        raise ValueError("evidence is absent, unsafe, or exceeds the bound")
    evidence = json.loads(args.evidence.read_bytes())
    result = load_verifier().evaluate({
        "resolved_config": unsigned_yaml_body(CONFIG),
        "evidence": evidence,
    })
    print(json.dumps(result, sort_keys=True))
    if result["claim_class"] != "installed_qualification" or not result["accepted"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
