#!/usr/bin/env python3
"""Source-contract checks for the distinct fail-closed contained image."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


def docker_stage(text, name):
    match = re.search(
        rf"(?ms)^FROM [^\n]+ AS {re.escape(name)}\n(.*?)(?=^FROM |\Z)", text
    )
    if not match:
        raise AssertionError(f"missing Docker stage {name}")
    return match.group(1)


class ContainedWorkflowPackagingTests(unittest.TestCase):
    def test_general_hosted_image_and_entrypoint_are_not_repurposed(self):
        general = (ROOT / "Dockerfile.hosted-workflow").read_text()
        ordinary_entry = (ROOT / "deploy/entrypoint.sh").read_text()
        self.assertNotIn("contained-workflow", general)
        self.assertNotIn("contained-workflow", ordinary_entry)

    def test_contained_target_has_exact_fail_closed_packaging(self):
        release = (ROOT / "Dockerfile.release").read_text()
        stage = docker_stage(release, "ryeos-contained-workflow")
        self.assertIn("/build/target-cache/release/lillux", stage)
        self.assertIn("/build/target-cache/release/ryeos-lillux-oci-hook", stage)
        self.assertIn("io.ryeos.image=\"contained-workflow\"", stage)
        self.assertIn("io.ryeos.required-node-profile=\"contained-workflow\"", stage)
        self.assertIn("io.ryeos.controller-uid=\"10001\"", stage)
        self.assertIn("io.ryeos.controller-gid=\"10001\"", stage)
        self.assertIn("profiles/contained-workflow.yaml", stage)
        self.assertIn('ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/contained-workflow-entrypoint"]', stage)
        self.assertNotIn("deploy/entrypoint.sh", stage)
        self.assertNotIn("RYEOS_INIT_NODE_PROFILE", stage)

    def test_entrypoint_has_one_fixed_authority_and_no_fallback(self):
        entrypoint = (ROOT / "images/contained-workflow/entrypoint.sh").read_text()
        self.assertIn("readonly BINDING=/run/ryeos/host-runtime.json", entrypoint)
        self.assertIn('exec /usr/local/bin/ryeosd host-runtime --binding "$BINDING"', entrypoint)
        for forbidden in ("RYEOS_", "cgroup", "docker", "container ID", "ryeos init"):
            self.assertNotIn(forbidden, entrypoint)

    def test_signed_profile_requires_scopes_and_enforcement(self):
        profile = (ROOT / "bundles/.ai/node/init/profiles/contained-workflow.yaml").read_text()
        self.assertTrue(profile.startswith("# ryeos:signed:"))
        self.assertRegex(profile, r"(?ms)process_scopes:\n(?:.*\n){0,5}\s+mode: required")
        self.assertRegex(profile, r"(?m)^\s+mode: enforce$")
        self.assertIn("implementation: linux-lillux", profile)
        self.assertIn("proc_filesystem: pid_namespace_nested", profile)
        self.assertNotIn("mode: unconfigured", profile)
        self.assertNotIn("mode: disabled", profile)

    def test_bake_targets_are_qualification_only(self):
        bake = (ROOT / "docker-bake.release.hcl").read_text()
        self.assertIn('target "contained-workflow"', bake)
        self.assertIn('target "contained-oci-hook-artifact"', bake)
        publish = (ROOT / ".github/workflows/publish-ryeosd.yml").read_text()
        self.assertNotIn("ryeos-contained-workflow", publish)

    def test_hook_is_a_locked_workspace_product(self):
        workspace = (ROOT / "Cargo.toml").read_text()
        lockfile = (ROOT / "Cargo.lock").read_text()
        self.assertIn('"crates/tools/lillux-oci-hook"', workspace)
        self.assertIn('name = "ryeos-lillux-oci-hook"', lockfile)


if __name__ == "__main__":
    unittest.main()
