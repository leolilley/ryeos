"""Canonical registry production boundaries; synthetic inputs are not live qualification."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / ".ai/tools/ryeos/development/registry-production/lib"))
import registry_inputs as owner

class RegistryProductionTests(unittest.TestCase):
    def setUp(self):
        self.config = yaml.safe_load((ROOT / ".ai/config/development/ryeos/registry-acquisition.yaml").read_text())
        self.archive = b"verified opaque crate archive; unpacking belongs to Cargo"
        self.checksum = hashlib.sha256(self.archive).hexdigest()
        self.lock = (f'version = 4\n[[package]]\nname = "example"\nversion = "1.0.0"\n'
                     f'source = "{self.config["registry_source"]}"\nchecksum = "{self.checksum}"\n').encode()
        self.row = {"name": "example", "vers": "1.0.0", "cksum": self.checksum,
                    "deps": [], "features": {}, "yanked": False}

    def fetch(self, url, maximum):
        return self.archive if url.endswith(".crate") else json.dumps(self.row).encode() + b"\n"

    def test_writes_public_local_registry_not_private_cargo_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "inputs"
            owner.assemble(self.lock, self.config, output, self.fetch, source_kind="public_https_acquisition")
            self.assertEqual((output / "example-1.0.0.crate").read_bytes(), self.archive)
            self.assertEqual(json.loads((output / "index/ex/am/example").read_bytes()), self.row)
            receipt = json.loads((output / "registry-inputs.json").read_bytes())
            self.assertEqual(receipt["lock_sha256"], hashlib.sha256(self.lock).hexdigest())
            self.assertEqual(sorted(p.name for p in output.iterdir()),
                             ["example-1.0.0.crate", "index", "registry-inputs.json"])
            with self.assertRaisesRegex(ValueError, "already exists"):
                owner.assemble(self.lock, self.config, output, self.fetch, source_kind="public_https_acquisition")

    def test_checks_both_index_and_archive_against_lock_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "inputs"
            self.row["cksum"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "index does not match"):
                owner.assemble(self.lock, self.config, output, self.fetch, source_kind="public_https_acquisition")
            self.assertFalse(output.exists())
            self.row["cksum"] = self.checksum
            self.archive = b"substitution"
            with self.assertRaisesRegex(ValueError, "archive does not match"):
                owner.assemble(self.lock, self.config, output, self.fetch, source_kind="public_https_acquisition")
            self.assertFalse(output.exists())
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_offline_assembly_reuses_canonical_validation_with_honest_provenance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original, reproduced = root / "original", root / "reproduced"
            owner.assemble(self.lock, self.config, original, self.fetch,
                           source_kind="public_https_acquisition")
            read = owner.RetainedInputs(original, self.lock, self.config)
            owner.assemble(self.lock, self.config, reproduced, read,
                           source_kind="admitted_retained_inputs")
            for member in ("example-1.0.0.crate", "index/ex/am/example"):
                self.assertEqual((original / member).read_bytes(), (reproduced / member).read_bytes())
            receipt = json.loads((reproduced / "registry-inputs.json").read_bytes())
            self.assertEqual(receipt["source_kind"], "admitted_retained_inputs")
            self.assertEqual(receipt["schema"], owner.RECEIPT_SCHEMA)
            with self.assertRaisesRegex(ValueError, "unselected"):
                read("https://index.crates.io/elsewhere", 100)

    def test_retained_read_rejects_parent_links_and_crate_substitution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = root / "original"
            owner.assemble(self.lock, self.config, original, self.fetch,
                           source_kind="public_https_acquisition")
            read = owner.RetainedInputs(original, self.lock, self.config)
            crate = original / "example-1.0.0.crate"
            crate.write_bytes(b"substitution")
            with self.assertRaisesRegex(ValueError, "archive does not match"):
                owner.assemble(self.lock, self.config, root / "bad", read,
                               source_kind="admitted_retained_inputs")
            self.assertFalse((root / "bad").exists())
            (original / "index").rename(root / "other-index")
            (original / "index").symlink_to(root / "other-index", target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "link or special"):
                read(self.config["index_base"] + "/ex/am/example", 10000)
            for member in ("../outside", "/outside", "index//ex/am/example", "."):
                with self.assertRaises(ValueError):
                    owner.ordinary_member(original, member)

    def test_compiler_checks_byte_budgets_independently_of_input_provider(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            self.config["limits"]["max_total_download_bytes"] = 1
            with self.assertRaisesRegex(ValueError, "aggregate"):
                owner.assemble(self.lock, self.config, output, self.fetch,
                               source_kind="public_https_acquisition")
            self.assertFalse(output.exists())
            self.config["limits"]["max_total_download_bytes"] = 100000
            self.config["limits"]["max_index_bytes"] = 1
            with self.assertRaisesRegex(ValueError, "exceeds its bound"):
                owner.assemble(self.lock, self.config, output, self.fetch,
                               source_kind="admitted_retained_inputs")

    def test_admitted_tool_has_exact_offline_dependencies_not_ambient_transport(self):
        text = (ROOT / ".ai/tools/ryeos/development/registry-production/assemble.py").read_text()
        header = text[text.index("# ryeos-tool:"):].split("\n\n", 1)[0]
        spec = yaml.safe_load("\n".join(line[2:] for line in header.splitlines()))["ryeos-tool"]
        self.assertEqual(spec["filesystem_authority"], "captured_execution")
        self.assertEqual(spec["network_authority"], "isolated")
        self.assertEqual(spec["workspace_access"], "immutable_current_generation")
        self.assertEqual({item["id"] for item in spec["external_content"]},
                         {"producer-python", "registry-inputs"})
        self.assertNotIn("subprocess", text)
        self.assertNotIn("import yaml", text)
        self.assertNotIn("Fetcher", text)
        compile(text, "assemble.py", "exec")

    def test_rejects_other_sources_and_unselected_urls(self):
        with tempfile.TemporaryDirectory() as directory:
            lock = self.lock.replace(self.config["registry_source"].encode(), b"git+https://example.org/source")
            with self.assertRaisesRegex(ValueError, "unsupported or unpinned"):
                owner.assemble(lock, self.config, Path(directory) / "inputs", self.fetch, source_kind="public_https_acquisition")
        for url in ("http://index.crates.io/a", "https://user:secret@index.crates.io/a",
                    "https://index.crates.io:444/a", "https://elsewhere.invalid/a"):
            with self.assertRaises(ValueError):
                owner.validate_url(url, self.config["allowed_https_hosts"])

    def test_package_bounds_and_public_index_path_rules(self):
        expected = {"a": "1/a", "ab": "2/ab", "abc": "3/a/abc", "AbCd": "ab/cd/abcd"}
        for name, path in expected.items():
            self.assertEqual(owner.index_member(name), path)
        self.config["limits"]["max_packages"] = 0
        with self.assertRaisesRegex(ValueError, "positive integers"):
            owner.validate_config(self.config)

if __name__ == "__main__":
    unittest.main()
