"""Synthetic GNU ELF closure checks; no acquired Python is executed."""

import copy
import hashlib
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


LIB = Path(__file__).resolve().parents[3] / ".ai/tools/ryeos/development/authoring-environment-production/lib"
sys.path.insert(0, str(LIB))
import gnu_elf as owner


RUNTIME_ROOT = "/ryeos/realizations/python-gnu/python"
ENTRY = "bin/python3.14"
LOADER = "lib/ld-linux-x86-64.so.2"
LIBPYTHON = "lib/libpython3.14.so.1.0"
LIBC = "lib/libc-2.40.so"
LIBGCC = "lib/libgcc_s.so.1"
EXTENSION = "lib/python3.14/lib-dynload/math.cpython-314-x86_64-linux-gnu.so"


class FakeTools:
    def __init__(self, root):
        self.root = root
        search = [RUNTIME_ROOT + "/lib"]
        loader = [RUNTIME_ROOT + "/" + LOADER]
        self.records = {
            ENTRY: {"dynamic": True, "interpreter": loader,
                    "needed": ["libpython3.14.so.1.0", "libc.so.6"],
                    "soname": [],
                    "runpath": search, "rpath": False, "nodeflib": True,
                    "required": {"libc.so.6": ["GLIBC_2.34"]}, "provided": []},
            LOADER: {"dynamic": True, "interpreter": [], "needed": [],
                     "soname": ["ld-linux-x86-64.so.2"],
                     "runpath": [], "rpath": False, "nodeflib": False,
                     "required": {}, "provided": ["GLIBC_PRIVATE"]},
            LIBPYTHON: {"dynamic": True, "interpreter": [], "needed": ["libc.so.6"],
                        "soname": ["libpython3.14.so.1.0"],
                        "runpath": search, "rpath": False, "nodeflib": True,
                        "required": {"libc.so.6": ["GLIBC_2.34"]},
                        "provided": ["PYTHON_3.14"]},
            LIBC: {"dynamic": True, "interpreter": [], "needed": [],
                   "soname": ["libc.so.6"],
                   "runpath": [], "rpath": False, "nodeflib": False,
                   "required": {}, "provided": ["GLIBC_2.34"]},
            EXTENSION: {"dynamic": True, "interpreter": [],
                        "needed": ["libpython3.14.so.1.0", "libc.so.6"],
                        "soname": [],
                        "runpath": search, "rpath": False, "nodeflib": True,
                        "required": {"libpython3.14.so.1.0": ["PYTHON_3.14"],
                                     "libc.so.6": ["GLIBC_2.34"]}, "provided": []},
        }
        self.headers = {name: {"class": "ELF64", "data": "2's complement, little endian",
                               "type": "DYN" if name != ENTRY else "EXEC",
                               "machine": "Advanced Micro Devices X86-64", "stack": "RW"}
                        for name in self.records}

    def member(self, path):
        return Path(path).relative_to(self.root).as_posix()

    def facts(self, path):
        record = self.records[self.member(path)]
        return {key: copy.deepcopy(record[key]) for key in
                ("dynamic", "interpreter", "needed", "soname", "runpath", "rpath", "nodeflib")}

    def run(self, name, *args):
        if name == "patchelf":
            if args[:2] == ("--no-sort", "--replace-needed"):
                old, new, path = args[2:]
                needed = self.records[self.member(path)]["needed"]
                needed[needed.index(old)] = new
            elif args[:2] == ("--no-sort", "--add-needed"):
                needed, path = args[2:]
                self.records[self.member(path)]["needed"].append(needed)
            else:
                raise AssertionError(args)
            return ""
        self.assert_readelf(name)
        member = self.member(args[-1])
        record, header = self.records[member], self.headers[member]
        if "-h" in args:
            return (f"  Class:                             {header['class']}\n"
                    f"  Data:                              {header['data']}\n"
                    f"  Type:                              {header['type']} (fixture)\n"
                    f"  Machine:                           {header['machine']}\n")
        if "-l" in args:
            interpreter = (f"[Requesting program interpreter: {record['interpreter'][0]}]\n"
                           if record["interpreter"] else "")
            if header["stack"] is None:
                return interpreter
            return interpreter + ("  GNU_STACK      0x000000 0x000000 0x000000 "
                                  f"0x000000 0x000000 {header['stack']} 0x10\n")
        if "--version-info" in args:
            lines = []
            if record["required"]:
                lines.append("Version needs section '.gnu.version_r' contains entries:")
                for provider, versions in record["required"].items():
                    lines.append(f"  000000: Version: 1  File: {provider}  Cnt: {len(versions)}")
                    lines.extend(f"  0x0010: Name: {version} Flags: none Version: 2"
                                 for version in versions)
            if record["provided"]:
                lines.append("Version definition section '.gnu.version_d' contains entries:")
                lines.extend(f"  0x001c: Rev: 1 Flags: none Index: 2 Cnt: 1 Name: {version}"
                             for version in record["provided"])
            return "\n".join(lines) + ("\n" if lines else "")
        raise AssertionError(args)

    def assert_readelf(self, name):
        if name != "readelf":
            raise AssertionError(name)

    def symbols(self, path):
        return ((self.member(path), "owned"),), ((self.member(path), "function"),)


class GnuElfTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name) / "python"
        for member in (ENTRY, LOADER, LIBPYTHON, LIBC, EXTENSION):
            path = self.root / member
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x7fELF" + member.encode())
            path.chmod(0o755 if member in (ENTRY, LOADER) else 0o644)
        os.symlink("libc-2.40.so", self.root / "lib/libc.so.6")
        os.symlink("python3.14", self.root / "bin/python3")
        self.tools = FakeTools(self.root)

    def inspect(self):
        return owner.inspect_python_runtime(self.root, self.tools, runtime_root=RUNTIME_ROOT)

    def test_inspects_nested_closure_versions_links_and_compact_evidence(self):
        inventory = self.inspect()
        self.assertEqual([item["path"] for item in inventory["objects"]],
                         sorted([ENTRY, LOADER, LIBPYTHON, LIBC, EXTENSION]))
        libc_link = next(link for link in inventory["links"] if link["path"] == "lib/libc.so.6")
        self.assertEqual(libc_link["resolved"], LIBC)
        extension = next(item for item in inventory["objects"] if item["path"] == EXTENSION)
        self.assertEqual(extension["required_versions"]["libpython3.14.so.1.0"],
                         ["PYTHON_3.14"])
        canonical = owner.canonical_inventory(inventory)
        evidence = owner.inventory_evidence(inventory)
        self.assertEqual(evidence["inventory_sha256"], hashlib.sha256(canonical).hexdigest())
        self.assertNotIn("objects", evidence)
        self.assertEqual(evidence["elf_objects"], 5)
        self.assertEqual(evidence["library_links"], 1)

    def test_relocation_delegates_every_dynamic_edge_to_existing_owner(self):
        transformed = []

        def relocate(path, tools, runtime_root):
            transformed.append(tools.member(path))
            self.assertEqual(runtime_root, RUNTIME_ROOT)
            return {"before": "0" * 64, "after": "1" * 64}

        with mock.patch.object(owner, "relocate_elf", side_effect=relocate):
            result = owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={}, needed_additions={})
        self.assertEqual(transformed, sorted([ENTRY, LIBPYTHON, EXTENSION]))
        self.assertEqual(set(result), set(transformed))

    def test_relocation_replaces_only_the_exact_declared_slash_needed_edge(self):
        old = "$ORIGIN/../lib/libpython3.14.so.1.0"
        self.tools.records[ENTRY]["needed"][0] = old

        def relocate(path, tools, runtime_root):
            return {"before": "0" * 64, "after": "1" * 64}

        with self.assertRaisesRegex(ValueError, "incomplete or unused"):
            owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={}, needed_additions={})
        with mock.patch.object(owner, "relocate_elf", side_effect=relocate):
            result = owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={ENTRY: {old: "libpython3.14.so.1.0"}},
                needed_additions={})
        self.assertEqual(self.tools.records[ENTRY]["needed"][0], "libpython3.14.so.1.0")
        self.assertEqual(result[ENTRY]["needed_replacements"],
                         [{"before": old, "after": "libpython3.14.so.1.0"}])

    def test_addition_is_exact_recorded_and_recursively_closed(self):
        path = self.root / LIBGCC
        path.write_bytes(b"\x7fELF" + LIBGCC.encode())
        path.chmod(0o644)
        search = [RUNTIME_ROOT + "/lib"]
        self.tools.records[LIBGCC] = {
            "dynamic": True, "interpreter": [],
            "needed": ["libc.so.6"], "soname": ["libgcc_s.so.1"],
            "runpath": search, "rpath": False, "nodeflib": True,
            "required": {"libc.so.6": ["GLIBC_2.34"]},
            "provided": ["GCC_3.0", "GCC_3.3", "GCC_4.2.0"],
        }
        self.tools.headers[LIBGCC] = {
            "class": "ELF64", "data": "2's complement, little endian",
            "type": "DYN", "machine": "Advanced Micro Devices X86-64", "stack": "RW",
        }

        def relocate(path, tools, runtime_root):
            return {"before": "0" * 64, "after": "1" * 64}

        with self.assertRaisesRegex(ValueError, "already present"):
            owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={}, needed_additions={ENTRY: ["libc.so.6"]})
        with mock.patch.object(owner, "relocate_elf", side_effect=relocate):
            result = owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={}, needed_additions={ENTRY: ["libgcc_s.so.1"]})
        self.assertEqual(result[ENTRY]["needed_additions"], ["libgcc_s.so.1"])
        self.assertIn("libgcc_s.so.1", self.tools.records[ENTRY]["needed"])
        inventory = self.inspect()
        entry = next(record for record in inventory["objects"] if record["path"] == ENTRY)
        provider_path = entry["resolved_needed"]["libgcc_s.so.1"]
        provider = next(record for record in inventory["objects"]
                        if record["path"] == provider_path)
        self.assertEqual(provider["soname"], "libgcc_s.so.1")
        self.assertEqual(provider["provided_versions"],
                         ["GCC_3.0", "GCC_3.3", "GCC_4.2.0"])
        self.assertEqual(provider["resolved_needed"], {"libc.so.6": LIBC})

        self.tools.records[ENTRY]["needed"].remove("libgcc_s.so.1")
        self.tools.records[LIBGCC]["soname"] = ["wrong.so.1"]
        with self.assertRaisesRegex(ValueError, "wrong SONAME"):
            owner.relocate_python_runtime(
                self.root, self.tools, runtime_root=RUNTIME_ROOT,
                needed_replacements={}, needed_additions={ENTRY: ["libgcc_s.so.1"]})

    def test_missing_dependency_and_exact_version_mismatch_refuse(self):
        (self.root / "lib/libc.so.6").unlink()
        with self.assertRaisesRegex(ValueError, "missing|malformed"):
            self.inspect()
        os.symlink("libc-2.40.so", self.root / "lib/libc.so.6")
        self.tools.records[LIBC]["provided"] = ["GLIBC_2.33"]
        with self.assertRaisesRegex(ValueError, "lacks required symbol versions"):
            self.inspect()
        self.tools.records[LIBC]["provided"] = ["GLIBC_2.34"]
        self.tools.records[LIBC]["soname"] = ["wrong.so.6"]
        with self.assertRaisesRegex(ValueError, "SONAME differs"):
            self.inspect()
        self.tools.records[LIBC]["soname"] = ["libc.so.6"]
        self.tools.records[ENTRY]["required"] = {"not-needed.so": ["VERSION_1"]}
        with self.assertRaisesRegex(ValueError, "not a DT_NEEDED"):
            self.inspect()

    def test_exact_filename_provider_without_soname_is_valid_but_mismatch_refuses(self):
        self.tools.records[LIBPYTHON]["soname"] = []
        inventory = self.inspect()
        provider = next(record for record in inventory["objects"]
                        if record["path"] == LIBPYTHON)
        self.assertIsNone(provider["soname"])
        owner.canonical_inventory(inventory)

        self.tools.records[LIBPYTHON]["soname"] = ["libpython-wrong.so"]
        with self.assertRaisesRegex(ValueError, "SONAME differs"):
            self.inspect()

    def test_interpreter_search_path_rpath_and_nodeflib_are_exact(self):
        variants = [
            ("interpreter", ["/lib64/ld-linux-x86-64.so.2"], "unclosed interpreter"),
            ("runpath", ["$ORIGIN/../lib"], "search is not closed"),
            ("rpath", True, "search is not closed"),
            ("nodeflib", False, "search is not closed"),
        ]
        for field, value, error in variants:
            with self.subTest(field=field):
                original = copy.deepcopy(self.tools.records[ENTRY][field])
                self.tools.records[ENTRY][field] = value
                with self.assertRaisesRegex(ValueError, error):
                    self.inspect()
                self.tools.records[ENTRY][field] = original

    def test_header_and_gnu_stack_refusals_are_closed(self):
        variants = [
            ("class", "ELF32", "little-endian ELF64"),
            ("data", "2's complement, big endian", "little-endian ELF64"),
            ("type", "REL", "unsupported type"),
            ("machine", "AArch64", "not x86_64"),
            ("stack", "RWE", "executable stack"),
            ("stack", "", "flags are malformed"),
            ("stack", None, "well-formed GNU_STACK"),
        ]
        for field, value, error in variants:
            with self.subTest(field=field):
                original = self.tools.headers[EXTENSION][field]
                self.tools.headers[EXTENSION][field] = value
                with self.assertRaisesRegex(ValueError, error):
                    self.inspect()
                self.tools.headers[EXTENSION][field] = original

    def test_symlink_escape_cycle_and_outside_lib_dependency_refuse(self):
        (self.root / "bin/escape").symlink_to("../../outside")
        with self.assertRaisesRegex(ValueError, "escapes"):
            self.inspect()
        (self.root / "bin/escape").unlink()
        (self.root / "bin/a").symlink_to("b")
        (self.root / "bin/b").symlink_to("a")
        with self.assertRaisesRegex(ValueError, "cyclic"):
            self.inspect()
        (self.root / "bin/a").unlink()
        (self.root / "bin/b").unlink()
        (self.root / "lib/libc.so.6").unlink()
        (self.root / "other").mkdir()
        (self.root / "other/libc.so.6").write_bytes((self.root / LIBC).read_bytes())
        (self.root / "lib/libc.so.6").symlink_to("../other/libc.so.6")
        self.tools.records["other/libc.so.6"] = copy.deepcopy(self.tools.records[LIBC])
        self.tools.headers["other/libc.so.6"] = copy.deepcopy(self.tools.headers[LIBC])
        with self.assertRaisesRegex(ValueError, "runtime/lib ELF"):
            self.inspect()

    def test_symlink_is_followed_before_later_parent_components(self):
        (self.root / "file").write_bytes(b"right")
        (self.root / "dir").mkdir()
        (self.root / "dir/file").write_bytes(b"wrong")
        (self.root / "other").mkdir()
        (self.root / "dir/a").symlink_to("../other")
        (self.root / "dir/link").symlink_to("a/../file")
        inventory = self.inspect()
        observed = next(link for link in inventory["links"] if link["path"] == "dir/link")
        self.assertEqual(observed["resolved"], "file")
        (self.root / "dir/a").unlink()
        (self.root / "dir/a").symlink_to("..")
        with self.assertRaisesRegex(ValueError, "escapes selected root"):
            self.inspect()

    def test_soabi_layout_runtime_root_and_inventory_bounds_refuse(self):
        with self.assertRaisesRegex(ValueError, "SOABI"):
            owner.inspect_python_runtime(self.root, self.tools, runtime_root=RUNTIME_ROOT,
                                         soabi="cpython-314-x86_64-linux-musl")
        with self.assertRaisesRegex(ValueError, "canonical absolute"):
            owner.inspect_python_runtime(self.root, self.tools, runtime_root="relative")
        inventory = self.inspect()
        inventory["unexpected"] = True
        with self.assertRaisesRegex(ValueError, "invalid"):
            owner.canonical_inventory(inventory)


if __name__ == "__main__":
    unittest.main()
