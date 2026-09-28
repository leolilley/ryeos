#!/usr/bin/env python3
import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("setup", Path(__file__).with_name("setup-contained-docker-node.py"))
SETUP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SETUP)


class SetupTests(unittest.TestCase):
    def test_development_publisher_trust_requires_explicit_opt_in(self):
        self.assertEqual(SETUP.publisher_trust_args(False), [])
        self.assertEqual(SETUP.publisher_trust_args(True),
                         ["--trust-file", "/opt/ryeos/.ai/PUBLISHER_TRUST.toml"])

    def image(self):
        return {"Id": "sha256:" + "a" * 64, "Os": "linux", "Architecture": "amd64", "Config": {
            "Entrypoint": SETUP.ENTRY, "Cmd": None, "Labels": {
                "io.ryeos.image": "contained-workflow", "io.ryeos.required-node-profile": "contained-workflow",
                "io.ryeos.controller-uid": "10001", "io.ryeos.controller-gid": "10001",
                "org.opencontainers.image.revision": "b" * 40}}}

    def test_general_image_and_overridden_bootstrap_refuse(self):
        SETUP.validate_image(self.image())
        for field, value in [("Entrypoint", ["/entrypoint.sh"]), ("Cmd", ["shell"]), ("Labels", {})]:
            image = self.image()
            image["Config"][field] = value
            with self.assertRaises(ValueError):
                SETUP.validate_image(image)

    def test_retained_container_requires_exact_identity_mounts_and_runtime(self):
        record = {"image": "sha256:" + "a" * 64, "port": 8001}
        state = Path("/var/lib/ryeos/contained-nodes/test")
        container = {"Image": record["image"], "Config": {"Entrypoint": SETUP.ENTRY, "Cmd": None,
                    "User": "0:0", "Labels": {SETUP.LABEL: str(state)}},
                    "HostConfig": {"Runtime": "ryeos-contained", "Privileged": False, "NetworkMode": "bridge",
                    "PortBindings": {"8000/tcp": [{"HostIp": "127.0.0.1", "HostPort": "8001"}]}},
                    "Mounts": [{"Type": "bind", "Source": str(state / name), "Destination": destination, "RW": True}
                               for name, destination in [("app", "/data/app"), ("projects", "/projects")]]}
        self.assertTrue(SETUP.container_matches(container, record, state))
        for field, value in [("Runtime", "runc"), ("Privileged", True), ("PidMode", "host"), ("NetworkMode", "host")]:
            changed = copy.deepcopy(container)
            changed["HostConfig"][field] = value
            self.assertFalse(SETUP.container_matches(changed, record, state))
        changed = copy.deepcopy(container)
        changed["Mounts"][0]["Source"] = "/another-node"
        self.assertFalse(SETUP.container_matches(changed, record, state))


if __name__ == "__main__":
    unittest.main()
