"""Archive/input preparation contracts, without Docker, builds or node state."""

import hashlib
import gzip
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

REPOSITORY = Path(__file__).resolve().parents[3]
LIB = REPOSITORY / ".ai/tools/ryeos/development/authoring-environment-production/lib"
sys.path.insert(0, str(LIB))
import archives
import preparation
import production


def authored(name):
    text = (REPOSITORY / ".ai/config/development/ryeos" / name).read_text()
    return json.loads("\n".join(line for line in text.splitlines() if not line.startswith("# ryeos:signed:")))


def identity(data, mode=0o644):
    return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "mode": mode}


def archive(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as result:
        for name, data, kind in entries:
            entry = tarfile.TarInfo(name)
            entry.type = kind
            entry.linkname = "selected" if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE) else ""
            entry.size = len(data)
            result.addfile(entry, io.BytesIO(data))
    return output.getvalue()


def regular(entries):
    return archive([(name, data, tarfile.REGTYPE) for name, data in entries.items()])


class ArchiveTests(unittest.TestCase):
    def test_only_selected_regular_bytes_are_returned(self):
        data = archive([("../not-extracted", b"ignored", tarfile.REGTYPE),
                        ("selected", b"ok", tarfile.REGTYPE)])
        self.assertEqual(archives.read_members(io.BytesIO(data), {"selected"}), {"selected": b"ok"})

    def test_duplicate_is_refused_even_after_selection_completed(self):
        data = archive([("selected", b"a", tarfile.REGTYPE), ("selected", b"a", tarfile.REGTYPE)])
        with self.assertRaisesRegex(ValueError, "duplicate"):
            archives.read_members(io.BytesIO(data), {"selected"})

    def test_links_and_special_selected_members_refuse(self):
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.DIRTYPE, tarfile.FIFOTYPE):
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "not regular"):
                archives.read_members(io.BytesIO(archive([("selected", b"", kind)])), {"selected"})

    def test_missing_selection_refuses(self):
        with self.assertRaisesRegex(ValueError, "missing"):
            archives.read_members(io.BytesIO(regular({"other": b"a"})), {"selected"})

    def test_selection_paths_cannot_escape(self):
        for name in ("../x", "/x", "x/../y", "x//y"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                archives.read_members(io.BytesIO(), {name})

    def test_selected_byte_bounds(self):
        data = regular({"a": b"aa", "b": b"bb"})
        for limits in ({"maximum_member_bytes": 1}, {"maximum_selected_bytes": 3}):
            with self.subTest(limits=limits), self.assertRaisesRegex(ValueError, "bytes exceed"):
                archives.read_members(io.BytesIO(data), {"a", "b"}, **limits)

    def test_unselected_expansion_and_entry_bounds(self):
        data = regular({"selected": b"a", "other": b"bbbb"})
        for constant, bound in (("MAX_ARCHIVE_ENTRIES", 1), ("MAX_EXPANDED_BYTES", 4)):
            with patch.object(archives, constant, bound), self.assertRaisesRegex(ValueError, "expansion"):
                archives.read_members(io.BytesIO(data), {"selected"})

    def test_internal_metadata_size_refuses_before_reading_its_body(self):
        for kind in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.GNUTYPE_LONGNAME,
                     tarfile.GNUTYPE_LONGLINK, tarfile.SOLARIS_XHDTYPE):
            member = tarfile.TarInfo("metadata")
            member.type = kind
            member.size = archives.MAX_METADATA_BYTES + 1
            # No body is present: refusal must precede tarfile's internal read,
            # not result from discovering truncated metadata after allocation.
            data = gzip.compress(member.tobuf(format=tarfile.GNU_FORMAT))
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "metadata exceeds"):
                archives.read_members(io.BytesIO(data), {"selected"})

    def test_hidden_headers_count_and_cumulative_metadata_are_bounded(self):
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode="w", format=tarfile.PAX_FORMAT) as output:
            for name in ("other", "selected"):
                entry = tarfile.TarInfo(name)
                entry.pax_headers = {"comment": "hidden header"}
                output.addfile(entry, io.BytesIO())
        for constant, bound, message in (("MAX_ARCHIVE_ENTRIES", 1, "header count"),
                                          ("MAX_TOTAL_METADATA_BYTES", 40, "metadata exceeds")):
            with patch.object(archives, constant, bound), self.assertRaisesRegex(ValueError, message):
                archives.read_members(io.BytesIO(payload.getvalue()), {"selected"})


class PreparationTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="ryeos-authoring-preparation-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.raw = self.root / "raw"
        self.raw.mkdir()
        self.config = authored("authoring-environment-inputs.yaml")
        self.sources = authored("authoring-utility-sources.yaml")
        self.data = {name: ("synthetic:" + name).encode() for name in self.config["inputs"]}
        provenance = self.config["provenance"]
        self.source_name = next(name for name in provenance["utility_bootstrap_archives"]
                                if name.endswith("-sources.tar.gz"))
        self.binary_name = next(name for name in provenance["utility_bootstrap_archives"]
                                if name != self.source_name)
        nested = {}
        binary_members = {}
        for source in self.sources["sources"]:
            notices = {source["directory"] + "/" + notice:
                       self.data["utilities/licenses/" + source["name"] + "/" + notice]
                       for notice in source["licenses"]}
            payload = regular(notices)
            source.update({key: value for key, value in identity(payload).items() if key != "mode"})
            nested[source["archive"]] = payload
            for command in source.get("programs", {}):
                binary_members["bin/" + command] = self.data["utilities/bin/" + command]
        source_archive, binary_archive = regular(nested), regular(binary_members)
        self.data["sources/" + self.source_name] = source_archive
        for name, data in ((self.source_name, source_archive), (self.binary_name, binary_archive)):
            provenance["utility_bootstrap_archives"][name] = {
                key: value for key, value in identity(data).items() if key != "mode"}
            self.write_raw("bootstrap/" + name, data)
        package_members = {name: self.data["utilities/bin/" + Path(name).name]
                           for name in provenance["workload_members"]}
        provenance["workload_members"] = {name: hashlib.sha256(data).hexdigest()
                                          for name, data in package_members.items()}
        package = regular(package_members)
        provenance["workload_package_sha256"] = hashlib.sha256(package).hexdigest()
        self.write_raw("workload/selected-package.tar.gz", package)
        upstream_targets = set()
        for source in provenance["source_and_notice_inputs"]:
            data = self.data[source["target"]]
            source.update({key: value for key, value in identity(data).items() if key != "mode"})
            self.write_raw("upstreams/" + source["archive"], data)
            upstream_targets.add(source["target"])
        for name, data in self.data.items():
            mode = self.config["inputs"][name]["mode"]
            self.config["inputs"][name] = identity(data, mode)
            if name.startswith(("elf/", "notices/")) and name not in upstream_targets:
                self.write_raw(name, data, mode)

    def write_raw(self, name, data, mode=0o644):
        path = self.raw / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(mode)
        return path

    def prepare(self, name="prepared"):
        return preparation.prepare(self.raw, self.root / name, self.config, self.sources)

    def test_exact_preparation_reproduces_all_authored_inputs(self):
        before = production.inventory(self.raw)
        first = self.prepare()
        self.assertEqual(production.inventory(self.root / "prepared/tree"), self.config["inputs"])
        self.assertEqual(first, self.prepare("again"))
        self.assertEqual(before, production.inventory(self.raw))
        self.assertEqual((self.root / "prepared/input-contract.json").read_bytes(),
                         production.canonical_json(self.config))

    def test_same_image_glibc_compatibility_dsos_are_exact_selected_inputs(self):
        authored_config = authored("authoring-environment-inputs.yaml")
        expected = {
            "libdl.so.2": (14408, "295fa521a03cd2faa99974f378c9e23dd622021ec7d32bcad4f8ea61aec8a872"),
            "libpthread.so.0": (14408, "85e21f7dba0394411d00959176fd18b470e575b0b05f1f4f41e5636802ce0500"),
            "librt.so.1": (14552, "7b7b84d1aedda0e0b2bdfc68844362782132180b8f02be1300e21e77572e514b"),
            "libutil.so.1": (14408, "e3981d10efd152a53f083e38f5f9ddde7a049c85d0ab388a5eec91b52fc98a11"),
        }
        raw, copies, _, _ = preparation.selection(self.config, self.sources)
        for name, (size, digest) in expected.items():
            with self.subTest(name=name):
                selected = f"elf/lib/{name}"
                source = f"/usr/lib/x86_64-linux-gnu/{name}"
                destination = f"environment/lib/{name}"
                identity = {"bytes": size, "sha256": digest, "mode": 0o644}
                self.assertEqual(authored_config["inputs"][selected], identity)
                self.assertEqual(authored_config["provenance"]["runtime_image_members"][source],
                                 [selected, 0o644])
                self.assertEqual(raw[selected], self.config["inputs"][selected])
                self.assertEqual(copies[selected], selected)
                self.assertEqual(authored_config["files"][destination], selected)
                self.assertIn(destination, authored_config["relocate"])

    def test_corruption_refuses_before_creating_output(self):
        (self.raw / "bootstrap" / self.binary_name).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            self.prepare()
        self.assertFalse((self.root / "prepared").exists())

    def test_readonly_materialized_archives_preserve_portable_input_identity(self):
        for name, item in production.inventory(self.raw).items():
            (self.raw / name).chmod(item["mode"] & ~0o222)
        before = production.inventory(self.raw)
        self.assertEqual(before["bootstrap/" + self.binary_name]["mode"], 0o444)
        self.prepare()
        self.assertEqual(production.inventory(self.raw), before)
        self.assertEqual(production.inventory(self.root / "prepared/tree"), self.config["inputs"])

    def test_prepared_output_modes_remain_physical_not_portable(self):
        target = self.root / "prepared/tree" / next(
            name for name, identity in self.config["inputs"].items() if identity["mode"] == 0o644)
        chmod = Path.chmod

        def drift(path, mode, *args, **kwargs):
            return chmod(path, 0o444 if path == target else mode, *args, **kwargs)

        with patch.object(Path, "chmod", drift), self.assertRaisesRegex(ValueError, "prepared output bytes or modes"):
            self.prepare()
        self.assertFalse((self.root / "prepared/input-contract.json").exists())

    def test_extra_input_refuses_before_creating_output(self):
        self.write_raw("extra", b"extra")
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            self.prepare()
        self.assertFalse((self.root / "prepared").exists())

    def test_raw_mode_and_symlink_changes_refuse(self):
        path = self.raw / "workload/selected-package.tar.gz"
        path.chmod(0o755)
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            self.prepare()
        path.rename(path.with_name("original"))
        path.symlink_to("original")
        with self.assertRaisesRegex(ValueError, "link or special"):
            self.prepare()

    def test_no_output_overwrite(self):
        self.prepare()
        before = production.inventory(self.root / "prepared")
        with self.assertRaises(FileExistsError):
            self.prepare()
        self.assertEqual(before, production.inventory(self.root / "prepared"))

    def test_source_archive_identity_cannot_be_inferred_from_output(self):
        self.sources["sources"][0]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "corresponding source archive identity"):
            self.prepare()
        self.assertFalse((self.root / "prepared/input-contract.json").exists())

    def test_member_cannot_replace_authored_input_contract(self):
        self.config["inputs"]["utilities/bin/cat"]["sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "selected archive member identity"):
            self.prepare()
        self.assertFalse((self.root / "prepared/input-contract.json").exists())

    def test_source_and_assembly_contract_disagreement_refuses(self):
        self.sources["source_date_epoch"] += 1
        with self.assertRaisesRegex(ValueError, "contracts disagree"):
            self.prepare()
        self.assertFalse((self.root / "prepared").exists())

    def run_entry(self, resolved=None):
        request = {"resolved_config": resolved if resolved is not None else {
            preparation.INPUT_CONFIG: self.config, preparation.SOURCE_CONFIG: self.sources}}
        captured = io.StringIO()
        with patch.object(preparation, "RAW_ROOT", self.raw), \
                patch.object(sys, "argv", ["prepare.py", "--project-path", str(self.root)]), \
                patch.object(sys, "stdin", SimpleNamespace(buffer=io.BytesIO(json.dumps(request).encode()))), \
                patch.object(sys, "stdout", captured):
            preparation.run_preparation()
        return json.loads(captured.getvalue())

    def test_entry_publishes_only_complete_output_at_the_declared_path(self):
        result = self.run_entry()
        self.assertTrue(result["ok"])
        self.assertFalse(result["binding_published"])
        self.assertEqual(result["output_path"], preparation.OUTPUT + "/tree")
        self.assertEqual(production.inventory(self.root / result["output_path"]), self.config["inputs"])
        self.assertFalse((self.root / "products/authoring-preparation").exists())

    def test_entry_requires_exact_multi_configuration_keys(self):
        for resolved in ({}, self.config, {"latest": self.config}, {
                preparation.INPUT_CONFIG: self.config, preparation.SOURCE_CONFIG: self.sources, "extra": {}}):
            with self.subTest(resolved=list(resolved)), self.assertRaisesRegex(ValueError, "resolved preparation"):
                self.run_entry(resolved)
        self.assertFalse((self.root / "products").exists())

    def test_entry_refuses_final_and_staging_collisions(self):
        self.run_entry()
        before = production.inventory(self.root / preparation.OUTPUT)
        with self.assertRaisesRegex(ValueError, "output already exists"):
            self.run_entry()
        self.assertEqual(before, production.inventory(self.root / preparation.OUTPUT))
        (self.root / preparation.OUTPUT).rename(self.root / "first-output")
        (self.root / "products/authoring-preparation").mkdir()
        with self.assertRaises(FileExistsError):
            self.run_entry()
        self.assertFalse((self.root / preparation.OUTPUT).exists())

    def test_entry_refuses_symlinked_products(self):
        (self.root / "products").symlink_to(self.raw, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "ordinary directory"):
            self.run_entry()


if __name__ == "__main__":
    unittest.main()
