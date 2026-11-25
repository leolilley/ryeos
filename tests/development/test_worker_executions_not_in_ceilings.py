"""A worker's ceilings never name the worker-execution services.

The worker-execution services (approvals, resolve-approval, and every other
`service:worker-executions/*` endpoint) are the person's side of a hosted
worker: they list, accept and decide what the worker asked for. No shipped
workload-client ceiling, execution ceiling, delegation list or grant, in any
bundle or any init profile, may name one of them, in either spelling (the item
ref `service:worker-executions/...` or the capability
`ryeos.execute.service.worker-executions/...`). Person-facing session scopes
and view projections that name them are not ceilings and are not judged here.
"""

import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
FORBIDDEN_PREFIXES = (
    "service:worker-executions/",
    "ryeos.execute.service.worker-executions/",
)
# A key path is a worker ceiling or grant when any segment names one.
CEILING_SEGMENTS = ("workload_client", "ceiling", "grant", "delegation")


def forbidden_in_ceilings(node, path=()):
    """Yield (path, value) for each forbidden string under a ceiling-shaped path."""
    if isinstance(node, dict):
        for key, value in node.items():
            yield from forbidden_in_ceilings(value, path + (str(key),))
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from forbidden_in_ceilings(value, path + (str(index),))
    elif isinstance(node, str) and node.startswith(FORBIDDEN_PREFIXES):
        if any(seg in key for key in path for seg in CEILING_SEGMENTS):
            yield ".".join(path), node


def shipped_yaml():
    # rglob, not glob: the shipped items live under hidden `.ai` directories.
    bundles = ROOT / "bundles"
    return sorted(str(p) for suffix in ("yaml", "yml") for p in bundles.rglob(f"*.{suffix}"))


class WorkerExecutionsStayOutOfCeilings(unittest.TestCase):
    def test_no_shipped_ceiling_names_a_worker_execution_service(self):
        parsed = 0
        offences = []
        for path in shipped_yaml():
            text = Path(path).read_text()
            if "worker-executions/" not in text:
                continue
            try:
                document = yaml.safe_load(text)
            except yaml.YAMLError:
                # Service descriptors use a schema notation that is not plain
                # YAML; no ceiling, delegation list or grant is written in it.
                continue
            parsed += 1
            for where, value in forbidden_in_ceilings(document):
                offences.append(f"{path}: {where} = {value}")
        self.assertTrue(parsed, "no shipped file names a worker-execution service at all")
        self.assertFalse(offences, "\n".join(offences))

    def test_the_scan_sees_a_ceiling_that_does_name_one(self):
        doc = {
            "policies": {
                "execution": {
                    "workload_client": {
                        "delegation_cap_ceiling": [
                            "ryeos.execute.service.worker-executions/approvals"
                        ]
                    }
                }
            }
        }
        self.assertEqual(len(list(forbidden_in_ceilings(doc))), 1)
        grant = {"config": {"workload_client_delegation_caps": ["service:worker-executions/x"]}}
        self.assertEqual(len(list(forbidden_in_ceilings(grant))), 1)
        scope = {"policies": {"ui": {"session_scopes": ["ryeos.execute.service.worker-executions/approvals"]}}}
        self.assertEqual(list(forbidden_in_ceilings(scope)), [])


if __name__ == "__main__":
    unittest.main()
