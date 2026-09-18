#!/usr/bin/env python3
"""Non-Cargo source and state-machine checks for the installed OCI hook."""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
HOOK = ROOT / "crates/host-adapters/lillux-oci-hook/src/main.rs"
CGROUP = ROOT / "crates/kernel/lillux/src/process_control/cgroup.rs"
LIFECYCLE = ROOT / "crates/kernel/lillux/src/process_control/oci_lifecycle.rs"
SCOPE = ROOT / "crates/kernel/lillux/src/process_control/scope.rs"
PROCESS = ROOT / "crates/kernel/lillux/src/process_control.rs"


class LeaseModel:
    def __init__(self):
        self.volumes = {}
        self.containers = {}
        self.setup = None
        self.release_journal = None

    def prestart(self, volume, container, stop_after=None):
        if self.volumes.get(volume) not in (None, "released"):
            return False
        if self.containers.get(container) not in (None, "released"):
            return False
        if self.setup is not None or self.release_journal is not None:
            return False
        self.setup = (volume, container)
        if stop_after == "setup_intent":
            return True
        self.containers[container] = "intent"
        if stop_after == "container_intent":
            return True
        self.volumes[volume] = "intent"
        self.setup = None
        if stop_after == "volume_intent":
            return True
        self.volumes[volume] = "prepared"
        if stop_after == "volume_prepared":
            return True
        self.containers[container] = "prepared"
        return True

    def activate(self, volume, container):
        assert self.volumes[volume] == self.containers[container] == "prepared"
        self.volumes[volume] = self.containers[container] = "active"

    def poststop(self, volume, container, dead, empty):
        if not dead or not empty:
            return False
        phases = {self.volumes.get(volume), self.containers.get(container)}
        if not phases <= {None, "intent", "prepared", "active", "released"}:
            return False
        if phases == {None} and self.setup != (volume, container):
            return False
        self.volumes[volume] = self.containers[container] = "released"
        if self.setup == (volume, container):
            self.setup = None
        return True

    def release_with_cut(self, volume, container, cut):
        self.release_journal = (volume, container)
        if cut == "release_journal":
            return
        self.volumes[volume] = "released"
        if cut == "released_volume":
            return
        self.containers[container] = "released"
        if cut == "released_container":
            return
        self.release_journal = None

    def recover_release(self, volume, container):
        if self.release_journal != (volume, container):
            return False
        self.volumes[volume] = self.containers[container] = "released"
        self.release_journal = None
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

    def test_every_durable_preparation_cut_recovers_only_after_death(self):
        for cut in ("setup_intent", "container_intent", "volume_intent", "volume_prepared", None):
            model = LeaseModel()
            self.assertTrue(model.prestart("volume-a", "container-1", cut))
            self.assertFalse(model.prestart("volume-a", "container-2"))
            self.assertFalse(model.poststop("volume-a", "container-1", False, True))
            self.assertTrue(model.poststop("volume-a", "container-1", True, True))
            self.assertTrue(model.prestart("volume-a", "container-2"))

    def test_reused_released_indices_never_override_new_setup_journal(self):
        for cut in ("setup_intent", "container_intent", "volume_intent"):
            model = LeaseModel()
            model.volumes["volume-a"] = "released"
            model.containers["container-1"] = "released"
            self.assertTrue(model.prestart("volume-a", "container-1", cut))
            self.assertFalse(model.poststop("volume-a", "container-1", False, True))
            self.assertTrue(model.poststop("volume-a", "container-1", True, True))
            self.assertEqual("released", model.volumes["volume-a"])
            self.assertEqual("released", model.containers["container-1"])

    def test_every_release_journal_cut_converges_before_reuse(self):
        for cut in ("release_journal", "released_volume", "released_container"):
            model = LeaseModel()
            self.assertTrue(model.prestart("volume-a", "container-1"))
            model.activate("volume-a", "container-1")
            model.release_with_cut("volume-a", "container-1", cut)
            self.assertFalse(model.prestart("volume-a", "container-2"))
            self.assertTrue(model.recover_release("volume-a", "container-1"))
            self.assertTrue(model.prestart("volume-a", "container-2"))

    def test_source_keeps_authority_in_lillux_and_fixed_paths(self):
        hook = HOOK.read_text()
        cgroup = CGROUP.read_text()
        lifecycle = LIFECYCLE.read_text()
        scope = SCOPE.read_text()
        process = PROCESS.read_text()
        self.assertIn('HOST_STATE_ROOT: &str = "/var/lib/ryeos/contained-oci"', hook)
        self.assertIn("volume_record_name", hook)
        self.assertIn("prove_ended_and_retire", hook)
        self.assertIn('Some("recover")', hook)
        self.assertIn('Some("install-host-state")', hook)
        self.assertIn("require_installed_executable", hook)
        self.assertIn("libc::flock", hook)
        self.assertIn("LifecyclePhase::Intent", hook)
        self.assertIn("reconcile_records", hook)
        self.assertIn("SETUP_TRANSACTION_NAME", hook)
        self.assertIn("require_container_id", hook)
        self.assertNotIn("docker", hook.lower())
        self.assertIn("prepare_oci_controller_root", cgroup)
        self.assertIn("libc::setns", cgroup)
        self.assertIn("libc::SYS_open_tree", cgroup)
        self.assertIn("libc::SYS_move_mount", cgroup)
        self.assertIn("MOVE_MOUNT_T_EMPTY_PATH", cgroup)
        self.assertIn("require_oci_membership", cgroup)
        self.assertIn("process_root.open_directory", cgroup)
        self.assertIn("libc::fchown", cgroup)
        self.assertIn("host cgroup root", cgroup)
        self.assertIn("retire_ended_oci_controller", cgroup)
        self.assertIn("retire_empty_oci_descendants", cgroup)
        self.assertIn("adopt_prepared_oci_controller", cgroup)
        self.assertIn("OciLifecycleIntent", lifecycle)
        self.assertIn("open_process_root", lifecycle)
        self.assertIn("cannot prove OCI init death", lifecycle)
        self.assertIn("require_intent", lifecycle)
        self.assertIn("namespace and proc-membership descriptors pin", scope)
        self.assertIn("pub struct ExactProcessRoot", process)
        self.assertIn('open_proc_magic_file(', process)
        self.assertIn('c"root"', process)
        self.assertNotIn('format!("/proc/{init_pid}/root', hook)
        self.assertNotIn('format!("/proc/{init_pid}/root', cgroup)


if __name__ == "__main__":
    unittest.main()
