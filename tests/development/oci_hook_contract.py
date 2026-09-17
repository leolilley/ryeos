#!/usr/bin/env python3
"""Non-Cargo source and state-machine checks for the installed OCI hook."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
HOOK = ROOT / "crates/host-adapters/lillux-oci-hook/src/main.rs"
CGROUP = ROOT / "crates/kernel/lillux/src/process_control/cgroup.rs"


class LeaseModel:
    def __init__(self):
        self.volumes = {}
        self.containers = {}

    def prestart(self, volume, container):
        if self.volumes.get(volume) not in (None, "released"):
            return False
        if self.containers.get(container) not in (None, "released"):
            return False
        self.volumes[volume] = self.containers[container] = "prepared"
        return True

    def activate(self, volume, container):
        assert self.volumes[volume] == self.containers[container] == "prepared"
        self.volumes[volume] = self.containers[container] = "active"

    def poststop(self, volume, container, dead, empty):
        if not dead or not empty:
            return False
        if self.volumes.get(volume) not in ("prepared", "active"):
            return False
        self.volumes[volume] = self.containers[container] = "released"
        return True


class OciHookContractTests(unittest.TestCase):
    def test_replacement_id_cannot_bypass_volume_lease(self):
        model = LeaseModel()
        self.assertTrue(model.prestart("volume-a", "container-1"))
        model.activate("volume-a", "container-1")
        self.assertFalse(model.prestart("volume-a", "container-2"))
        self.assertTrue(model.poststop("volume-a", "container-1", True, True))
        self.assertTrue(model.prestart("volume-a", "container-2"))

    def test_unknown_death_or_descendants_never_release(self):
        for dead, empty in ((False, True), (True, False), (False, False)):
            model = LeaseModel()
            model.prestart("volume-a", "container-1")
            self.assertFalse(model.poststop("volume-a", "container-1", dead, empty))
            self.assertFalse(model.prestart("volume-a", "container-2"))

    def test_interrupted_preparation_stays_quarantined(self):
        model = LeaseModel()
        self.assertTrue(model.prestart("volume-a", "container-1"))
        self.assertFalse(model.prestart("volume-a", "container-2"))
        self.assertFalse(model.poststop("volume-a", "container-1", False, True))
        self.assertFalse(model.prestart("volume-a", "container-2"))

    def test_source_keeps_authority_in_lillux_and_fixed_paths(self):
        hook = HOOK.read_text()
        cgroup = CGROUP.read_text()
        self.assertIn('HOST_STATE_ROOT: &str = "/var/lib/ryeos/contained-oci"', hook)
        self.assertIn("volume_record_name", hook)
        self.assertIn("prove_ended_and_retire", hook)
        self.assertIn('Some("recover")', hook)
        self.assertIn('Some("install-host-state")', hook)
        self.assertIn("require_installed_executable", hook)
        self.assertNotIn("docker", hook.lower())
        self.assertIn("prepare_oci_controller_root", cgroup)
        self.assertIn("libc::setns", cgroup)
        self.assertIn("retire_ended_oci_controller", cgroup)


if __name__ == "__main__":
    unittest.main()
