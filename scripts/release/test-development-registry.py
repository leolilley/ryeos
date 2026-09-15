"""Finite input acquisition checks; no network, Cargo, or node operations."""

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

import yaml


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("registry_bootstrap", Path(__file__).with_name("fetch-development-registry.py"))
owner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(owner)


class RegistryTransportTests(unittest.TestCase):
    def setUp(self):
        self.config = yaml.safe_load((ROOT / ".ai/config/development/ryeos/registry-acquisition.yaml").read_text())

    def test_fetch_has_independent_deadline_and_no_ambient_curl_configuration(self):
        with patch.object(owner.time, "monotonic", return_value=100):
            fetch = owner.Fetcher(self.config)
        fetch.checked_curl = True
        with patch.object(owner.time, "monotonic", return_value=1895):
            with patch.object(owner.subprocess, "run", side_effect=subprocess.TimeoutExpired("curl", 5)) as run:
                with self.assertRaises(subprocess.TimeoutExpired):
                    fetch("https://index.crates.io/ex/am/example", 100)
                args, kwargs = run.call_args
                self.assertEqual(kwargs["timeout"], 5)
                self.assertEqual(args[0][:2], ["curl", "--disable"])
                self.assertNotIn("--location", args[0])
                self.assertIn("--noproxy", args[0])
        with patch.object(owner.time, "monotonic", return_value=1900):
            with patch.object(owner.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, "lifetime exhausted"):
                    fetch("https://index.crates.io/ex/am/example", 100)
                run.assert_not_called()
        with patch.object(owner.time, "monotonic", return_value=100):
            with patch.object(owner.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as run:
                fetch("https://index.crates.io/a", 100)
                self.assertEqual(run.call_args.kwargs["timeout"], 60)

    def test_fetch_accounts_for_aggregate_bytes(self):
        fetch = owner.Fetcher(self.config)
        fetch.checked_curl = True
        fetch.config["limits"]["max_total_download_bytes"] = 2
        with patch.object(owner.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"ab", b"")):
            self.assertEqual(fetch("https://index.crates.io/a", 100), b"ab")
        with patch.object(owner.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "byte bound exhausted"):
                fetch("https://index.crates.io/b", 100)
            run.assert_not_called()

    def test_fetch_requires_streaming_download_byte_bound_capability(self):
        fetch = owner.Fetcher(self.config)
        with patch.object(owner.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, b"curl 8.3.0 (test)\n", b"")) as run:
            with self.assertRaisesRegex(ValueError, "curl >= 8.4.0"):
                fetch("https://index.crates.io/a", 100)
            self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
