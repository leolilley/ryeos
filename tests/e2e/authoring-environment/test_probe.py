"""Bounded probe-runner checks; no Docker, RyeOS node or model execution."""

import importlib.util
from pathlib import Path
import sys
import unittest


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("authoring_probe", HERE / "qualify.py")
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class ProbeTests(unittest.TestCase):
    def test_source_owner_is_resolved_from_the_repository(self):
        self.assertEqual(
            probe.PRODUCTION_OWNER,
            HERE.parents[2]
            / ".ai/tools/ryeos/development/authoring-environment-production/lib/production.py",
        )
        self.assertTrue(probe.PRODUCTION_OWNER.is_file())
        self.assertTrue((probe.HERE / "probe.sh").is_file())
        self.assertTrue((probe.HERE / "Dockerfile.probe").is_file())

    def test_probe_collects_bounded_combined_output(self):
        status, output = probe.run_bounded([
            sys.executable, "-c", "import sys; print('ok'); sys.stderr.write('diagnostic')"
        ])
        self.assertEqual(status, 0)
        self.assertIn(b"ok", output)
        self.assertIn(b"diagnostic", output)

    def test_probe_refuses_output_overflow_with_bounded_diagnostic(self):
        with self.assertRaisesRegex(probe.ProbeRefused, "diagnostic bound") as refused:
            probe.run_bounded(
                [sys.executable, "-c", "print('x' * 4096)"], maximum_output=1024
            )
        self.assertEqual(len(refused.exception.output), 1024)

    def test_probe_refuses_a_stalled_process(self):
        with self.assertRaisesRegex(probe.ProbeRefused, "duration bound"):
            probe.run_bounded(
                [sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.1
            )


if __name__ == "__main__":
    unittest.main()
