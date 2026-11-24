"""A worker may reach RyeOS only through the tools its signed ceiling names.

No shipped worker execution and no shipped init profile may let a worker's
workload client call the candidate-disposition services (evaluate, qualify,
integrate, publish, approve): the person decides those, so a worker cannot
qualify or publish its own candidate.
"""

import glob
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
TOOLS_ONLY = "ryeos.execute.tool."


def load(path):
    return yaml.safe_load(Path(path).read_text())


def check(caps, where):
    for cap in caps or []:
        assert isinstance(cap, str), (where, cap)
        assert cap.startswith(TOOLS_ONLY), (
            f"{where}: delegation cap {cap!r} is not a tool capability; a worker "
            f"must not be granted services such as worker-executions/*"
        )


class WorkerCapCeilings(unittest.TestCase):
    def test_every_shipped_worker_execution_delegates_tools_only(self):
        paths = sorted(glob.glob(str(ROOT / "bundles/*/.ai/worker-executions/**/*.yaml"), recursive=True))
        self.assertTrue(paths, "no worker executions found")
        for path in paths:
            config = (load(path) or {}).get("config") or {}
            check(config.get("workload_client_delegation_caps"), path)

    def test_every_shipped_profile_ceiling_is_tools_only(self):
        paths = sorted(glob.glob(str(ROOT / "bundles/.ai/node/init/profiles/*.yaml")))
        self.assertTrue(paths, "no init profiles found")
        for path in paths:
            execution = ((load(path) or {}).get("policies") or {}).get("execution") or {}
            client = execution.get("workload_client")
            if client:
                check(client.get("delegation_cap_ceiling"), path)


if __name__ == "__main__":
    unittest.main()
