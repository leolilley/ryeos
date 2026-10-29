#!/usr/bin/env python3
"""Golden-table gate for the ownership-derived cargo features and bundle-sets.sh."""

import base64
import hashlib
import json
import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

ROOT = Path(__file__).resolve().parents[2]
SETS_SH = ROOT / "scripts/pkg/bundle-sets.sh"
CONFIG = ROOT / "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
PACKAGES = ["ryeos-handler-bins", "ryeosd", "ryeos-cli", "ryeos-api", "ryeos-parser-bins", "ryeos-core-tools", "no-such-package"]

spec = importlib.util.spec_from_file_location("bundle_features", ROOT / "scripts/pkg/bundle-features.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


def shell(script):
    return subprocess.run(["bash", "-c", f'source "{SETS_SH}"; {script}'], check=True,
                          capture_output=True, cwd=ROOT).stdout


def set_ids():
    return shell("ryeos_bundle_set_ids; echo release-artifacts").decode().split()


# Fixtures are signed by a throwaway key that a temporary trust file pins, so
# the loader's full signature verification runs on them.
TEST_KEY = Ed25519PrivateKey.generate()
TEST_PUBLIC = TEST_KEY.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
TEST_TRUST = Path(tempfile.mkdtemp()) / "trust.toml"
TEST_TRUST.write_text(f'public_key = "ed25519:{base64.b64encode(TEST_PUBLIC).decode()}"\n'
                      f'fingerprint = "{hashlib.sha256(TEST_PUBLIC).hexdigest()}"\nowner = "test"\n')


def signed(body):
    digest = hashlib.sha256(body.encode()).hexdigest()
    signature = base64.b64encode(TEST_KEY.sign(digest.encode())).decode()
    return f"# ryeos:signed:2026-01-01T00:00:00Z:{digest}:{signature}:{hashlib.sha256(TEST_PUBLIC).hexdigest()}\n{body}"


def load_fixture(path):
    return tool.load_ownership(path, TEST_TRUST)


def fixture(edit):
    """A hash-valid ownership file built from the real body with `edit` applied."""
    body = CONFIG.read_text().partition("\n")[2]
    path = Path(tempfile.mkdtemp()) / "ownership.yaml"
    path.write_text(signed(edit(body)))
    return path


# Hand-written expectations, independent of the ownership file: Directive and
# Graph select the directive and graph handlers (Graph alone selects only the
# graph handlers; otherwise Core's handlers), Scheduling adds the scheduling
# feature and Directive the model-provider-setup feature to the node binaries,
# in canonical sorted order. Every other package gets no features.
HANDLERS_STANDARD = "directive-handlers,graph-handlers"
HANDLERS_GRAPH = "graph-handlers"
HANDLERS_CORE = "core-handlers,extends-composer"
GOLDEN = {
    "full": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "development": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "central-host": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "standard": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "standard-central-auth": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "local-inference": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "hosted-workflow": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "f5r-controller": (HANDLERS_GRAPH, ""),
    "release-authority": (HANDLERS_GRAPH, ""),
    "release-artifacts": (HANDLERS_STANDARD, "model-provider-setup,scheduling"),
    "core-scheduling": (HANDLERS_CORE, "scheduling"),
    "core-workflow": (HANDLERS_GRAPH, ""),
    "core-knowledge": (HANDLERS_CORE, ""),
    "core": (HANDLERS_CORE, ""),
    "bundle-source": (HANDLERS_CORE, ""),
}


def golden(set_name, package):
    handlers, node = GOLDEN[set_name]
    if package == "ryeos-handler-bins":
        features = handlers
    elif package in ("ryeosd", "ryeos-cli", "ryeos-api"):
        features = node
    else:
        return b""
    if not features:
        return b""
    return (",".join(f"{package}/{f}" for f in features.split(",")) + "\n").encode()


class GoldenTableTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ownership = tool.load_ownership(CONFIG)

    def test_sets_are_known(self):
        self.assertGreaterEqual(len(set_ids()), 12)

    def test_golden_table_covers_exactly_the_known_sets(self):
        self.assertEqual(sorted(GOLDEN), sorted(set_ids()))

    def test_every_set_feature_output_matches_the_golden_table(self):
        for set_name in set_ids():
            for package in PACKAGES:
                actual = tool.render(tool.derive(self.ownership, package, set_name=set_name)).encode()
                self.assertEqual(actual, golden(set_name, package), (set_name, package))

    def test_shell_wrapper_matches_the_golden_table(self):
        for set_name in set_ids():
            for package in PACKAGES:
                actual = shell(f'ryeos_bundle_set_cargo_features "{set_name}" "{package}"')
                self.assertEqual(actual, golden(set_name, package), (set_name, package))

    def test_cli_output_matches_the_golden_table(self):
        for set_name in set_ids():
            for package in ("ryeos-handler-bins", "ryeosd"):
                actual = subprocess.run(
                    ["python3", "-I", "-B", str(ROOT / "scripts/pkg/bundle-features.py"),
                     "--set", set_name, "cargo-features", package],
                    check=True, capture_output=True, cwd=ROOT).stdout
                self.assertEqual(actual, golden(set_name, package), (set_name, package))

    def test_explicit_bundle_list_matches_the_set(self):
        for set_name in set_ids():
            names = shell(f'ryeos_bundle_set_names "{set_name}"').decode().split()
            for package in PACKAGES:
                self.assertEqual(tool.derive(self.ownership, package, bundles=names),
                                 tool.derive(self.ownership, package, set_name=set_name))

    def test_membership_is_exactly_recorded_plus_data_only(self):
        data_only = set(self.ownership["data_only_bundles"])
        for set_name in set_ids():
            names = set(shell(f'ryeos_bundle_set_names "{set_name}"').decode().split())
            recorded = set(tool.set_members(self.ownership, set_name))
            self.assertEqual(recorded, names - data_only, set_name)
            self.assertEqual(names - recorded, names & data_only, set_name)

    def test_minimal_parser_agrees_with_the_checked_in_full_parse(self):
        # The fixture is the parse of the signed body by a full YAML parser;
        # regenerate it whenever the ownership item is re-signed.
        body = CONFIG.read_text().partition("\n")[2]
        expected = json.loads((ROOT / "scripts/pkg/fixtures/payload-ownership.parsed.json").read_text())
        self.assertEqual(tool.parse_yaml(body), expected)


class RejectionTests(unittest.TestCase):
    def test_unknown_bundle_is_rejected(self):
        ownership = tool.load_ownership(CONFIG)
        with self.assertRaisesRegex(tool.OwnershipError, "unknown bundle 'nope'"):
            tool.cargo_features(ownership, ["core", "nope"], "ryeosd")

    def test_data_only_bundle_is_accepted_and_contributes_nothing(self):
        ownership = tool.load_ownership(CONFIG)
        self.assertEqual(tool.cargo_features(ownership, ["core", "central-auth"], "ryeosd"), [])

    def test_unknown_set_is_rejected(self):
        with self.assertRaisesRegex(tool.OwnershipError, "unknown bundle set"):
            tool.derive(tool.load_ownership(CONFIG), "ryeosd", set_name="nope")

    def test_duplicate_package_binary_is_rejected(self):
        path = fixture(lambda body: body.replace("binary: ryeos-web-tools", "binary: ryeos-browser-tools"))
        with self.assertRaisesRegex(tool.OwnershipError, "more than one owner"):
            load_fixture(path)

    def test_duplicate_bundle_in_list_is_rejected(self):
        with self.assertRaisesRegex(tool.OwnershipError, "duplicate bundle"):
            tool.cargo_features(tool.load_ownership(CONFIG), ["core", "core"], "ryeosd")

    def test_build_class_conflict_within_a_package_is_rejected(self):
        # Two selected payloads of one Cargo package cannot be built as both
        # a release and a static artifact in one invocation.
        def edit(body):
            return body.replace("binary: ryeos-worker-execution-runtime\n          cargo_package: ryeos-structured-session\n          build_class: static",
                                "binary: ryeos-worker-execution-runtime\n          cargo_package: ryeos-structured-session\n          build_class: release")
        path = fixture(edit)
        ownership = load_fixture(path)
        with self.assertRaisesRegex(tool.OwnershipError, "disagree on build class"):
            tool.cargo_features(ownership, ["core"], "ryeos-structured-session")

    def test_host_package_that_also_owns_payloads_is_rejected_at_load(self):
        path = fixture(lambda body: body.replace("host_packages: [ryeos-api, ryeos-cli, ryeosd]",
                                                 "host_packages: [ryeos-api, ryeos-cli, ryeos-core-tools, ryeosd]"))
        with self.assertRaisesRegex(tool.OwnershipError, "both a host package and a payload package"):
            load_fixture(path)

    def test_no_default_package_payload_without_features_is_rejected_at_load(self):
        path = fixture(lambda body: body.replace("no_default_features_packages: [ryeos-handler-bins]",
                                                 "no_default_features_packages: [ryeos-handler-bins, ryeos-web-tools]"))
        with self.assertRaisesRegex(tool.OwnershipError, "explicit feature composition"):
            load_fixture(path)

    def test_non_string_plain_scalars_are_rejected(self):
        for scalar in ("yes", "No", "on", "OFF", "true", "null", "~", "1.5", "0x1f", "12", "1_000", "2026-01-01"):
            with self.subTest(scalar=scalar), self.assertRaisesRegex(tool.OwnershipError, "not a string"):
                tool.parse_yaml(f"a: {scalar}\n")
            with self.subTest(scalar=scalar, shape="flow"), self.assertRaisesRegex(tool.OwnershipError, "not a string"):
                tool.parse_yaml(f"a: [{scalar}]\n")

    def test_bare_colon_list_item_is_rejected_not_read_as_mapping(self):
        with self.assertRaises(tool.OwnershipError):
            tool.parse_yaml("a:\n  - b:c\n")
        with self.assertRaises(tool.OwnershipError):
            tool.parse_yaml("a:b\n")

    def test_quoted_and_ordinary_strings_are_accepted(self):
        self.assertEqual(tool.parse_yaml('a: "yes"\nb: [x-1, y_2]\n'), {"a": "yes", "b": ["x-1", "y_2"]})

    def test_tampered_body_is_rejected(self):
        body = CONFIG.read_text().partition("\n")[2]
        path = Path(tempfile.mkdtemp()) / "ownership.yaml"
        path.write_text(signed(body).replace("ryeos-web-tools", "ryeos-web-toolz", 1))
        with self.assertRaisesRegex(tool.OwnershipError, "body hash"):
            load_fixture(path)

    def test_wrong_schema_is_rejected(self):
        path = fixture(lambda body: body.replace("bundle_payload_ownership.v2", "bundle_payload_ownership.v1"))
        with self.assertRaisesRegex(tool.OwnershipError, "schema must be"):
            load_fixture(path)


if __name__ == "__main__":
    unittest.main()
