#!/usr/bin/env python3
"""The shared supervisor payload is built once and every consumer carries its hash."""

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("shared_static_payloads", ROOT / "scripts/release/shared-static-payloads.py")
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)

SUPERVISOR = "ryeos-external-candidate-supervisor"
LAUNCHER = "ryeos-external-candidate-launcher"
OWNER = "codex"
TRIPLE = "x86_64-unknown-linux-gnu"


def real():
    records, _identity = tool.ownership_module(ROOT).load(ROOT)
    return records, tool.load_shared(ROOT), tool.declared_binaries(ROOT)


class SharedStaticPayloads(unittest.TestCase):
    def test_the_repository_declaration_is_valid_and_covers_the_modal_manifest(self):
        records, shared, declared = real()
        tool.validate(records, shared, declared)
        self.assertIn(("modal-sandbox", "supervisor", SUPERVISOR), declared)
        self.assertIn(("render-sandbox", "supervisor", SUPERVISOR), declared)
        self.assertEqual([(e["owner_bundle"], e["consumer_bundles"]) for e in shared if e["binary"] == SUPERVISOR],
                         [(OWNER, ["modal-sandbox", "render-sandbox"])])
        self.assertEqual([(e["owner_bundle"], e["consumer_bundles"]) for e in shared if e["binary"] == LAUNCHER],
                         [(OWNER, ["render-sandbox"])])

    def test_the_payload_has_exactly_one_owner_and_one_build(self):
        records, shared, declared = real()
        self.assertEqual([r["bundle"] for r in records if r["binary"] == SUPERVISOR], [OWNER])
        self.assertEqual([r["bundle"] for r in records if r["binary"] == LAUNCHER], [OWNER])
        copies = copy.deepcopy(records)
        copies.append({**next(r for r in records if r["binary"] == SUPERVISOR), "bundle": "modal-sandbox"})
        with self.assertRaisesRegex(tool.Refused, "more than one owner"):
            tool.validate(copies, shared, declared)

    def test_an_entry_without_an_owner_is_refused(self):
        records, shared, declared = real()
        without = [r for r in records if r["binary"] != SUPERVISOR]
        with self.assertRaisesRegex(tool.Refused, "no owner"):
            tool.validate(without, shared, declared)

    def test_a_manifest_reference_without_an_entry_is_refused(self):
        records, _shared, declared = real()
        with self.assertRaisesRegex(tool.Refused, "neither owns nor declares"):
            tool.validate(records, [], declared)

    def test_a_consumer_built_where_its_owner_is_not_is_refused(self):
        records, shared, declared = real()
        narrowed = copy.deepcopy(records)
        for record in narrowed:
            if record["binary"] == SUPERVISOR:
                record["bundle_sets"] = ["f5r-controller"]
        with self.assertRaisesRegex(tool.Refused, "do not build its owner"):
            tool.validate(narrowed, shared, declared)

    def stage_tree(self, directory):
        owner = directory / OWNER
        owner.mkdir()
        (owner / SUPERVISOR).write_bytes(b"one supervisor build")
        (owner / LAUNCHER).write_bytes(b"one launcher build")
        return real()[1]

    def expected_record(self, consumers):
        record = {}
        if "render-sandbox" in consumers:
            record[LAUNCHER] = {"sha256": hashlib.sha256(b"one launcher build").hexdigest(), "owner": OWNER,
                                "consumers": ["render-sandbox"]}
        supervisor_consumers = [c for c in ("modal-sandbox", "render-sandbox") if c in consumers]
        if supervisor_consumers:
            record[SUPERVISOR] = {"sha256": hashlib.sha256(b"one supervisor build").hexdigest(), "owner": OWNER,
                                  "consumers": supervisor_consumers}
        return record

    def test_staging_installs_one_build_and_records_one_hash(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            shared = self.stage_tree(directory)
            record = tool.stage(shared, directory, {OWNER, "render-sandbox", "modal-sandbox"})
            digest = hashlib.sha256(b"one supervisor build").hexdigest()
            self.assertEqual(record, self.expected_record({"render-sandbox", "modal-sandbox"}))
            self.assertEqual(json.loads((directory / tool.RECORD).read_text()), record)
            for bundle in (OWNER, "render-sandbox", "modal-sandbox"):
                self.assertEqual(tool.sha256_file(directory / bundle / SUPERVISOR), digest)
            self.assertEqual(tool.sha256_file(directory / "render-sandbox" / LAUNCHER),
                             hashlib.sha256(b"one launcher build").hexdigest())
            self.assertFalse((directory / "modal-sandbox" / LAUNCHER).exists())

    def test_a_consumer_not_selected_receives_no_copy(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            shared = self.stage_tree(directory)
            self.assertEqual(tool.stage(shared, directory, {OWNER}), {})
            self.assertFalse((directory / "modal-sandbox").exists())
            self.assertFalse((directory / "render-sandbox").exists())

    def test_a_per_target_copy_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            shared = self.stage_tree(directory)
            (directory / "modal-sandbox").mkdir()
            (directory / "modal-sandbox" / SUPERVISOR).write_bytes(b"separately built")
            with self.assertRaisesRegex(tool.Refused, "not built per target"):
                tool.stage(shared, directory, {OWNER, "render-sandbox", "modal-sandbox"})

    def test_a_missing_owner_build_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            with self.assertRaisesRegex(tool.Refused, "not staged"):
                tool.stage(real()[1], Path(raw), {OWNER, "render-sandbox", "modal-sandbox"})

    def published(self, directory, content=None):
        data = b"one supervisor build"
        digest = hashlib.sha256(data).hexdigest()
        for bundle in (OWNER, "render-sandbox", "modal-sandbox"):
            bin_dir = directory / "bundles" / bundle / ".ai/bin" / TRIPLE
            bin_dir.mkdir(parents=True)
            (bin_dir / SUPERVISOR).write_bytes(content.get(bundle, data) if content else data)
            blob = digest if not content or bundle not in content else hashlib.sha256(content[bundle]).hexdigest()
            (bin_dir / f"{SUPERVISOR}.item_source.json").write_text(
                "# ryeos:signed:x\n" + json.dumps({"content_blob_hash": blob}))
        return {SUPERVISOR: {"sha256": digest, "owner": OWNER, "consumers": ["modal-sandbox", "render-sandbox"]}}

    def test_both_bundles_reference_the_recorded_hash(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            tool.verify(directory, self.published(directory), TRIPLE)

    def test_a_hash_mismatch_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            record = self.published(directory, {"modal-sandbox": b"another build"})
            with self.assertRaisesRegex(tool.Refused, "differs from the single build"):
                tool.verify(directory, record, TRIPLE)

    def test_a_published_item_source_under_another_hash_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            record = self.published(directory)
            source = directory / "bundles/modal-sandbox/.ai/bin" / TRIPLE / f"{SUPERVISOR}.item_source.json"
            source.write_text(json.dumps({"content_blob_hash": "0" * 64}))
            with self.assertRaisesRegex(tool.Refused, "different hash"):
                tool.verify(directory, record, TRIPLE)

    def test_an_absent_consumer_copy_is_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            record = self.published(directory)
            (directory / "bundles/modal-sandbox/.ai/bin" / TRIPLE / SUPERVISOR).unlink()
            with self.assertRaisesRegex(tool.Refused, "no installed"):
                tool.verify(directory, record, TRIPLE)


if __name__ == "__main__":
    unittest.main()
