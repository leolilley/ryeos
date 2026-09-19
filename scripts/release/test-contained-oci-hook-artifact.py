#!/usr/bin/env python3
import hashlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("artifact", Path(__file__).with_name("contained-oci-hook-artifact.py"))
ART = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ART)


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.expected = ART.testimony("1.2.3", "a" * 40, "1970-01-01T00:00:01Z", 1)
        self.binary = self.root / "hook"
        build = self.root / "build"
        build.write_bytes(self.expected)
        subprocess.run(["objcopy", "--add-section", f"{ART.SECTION}={build}", shutil.which("true"), str(self.binary)], check=True, capture_output=True)
        self.license = self.root / "LICENSE"
        self.license.write_text("license\n")
        self.archive = self.root / "ryeos-contained-oci-hook-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        self.checksum = Path(str(self.archive) + ".sha256")

    def package(self):
        ART.package(self.binary, self.license, self.archive, self.expected, 1, "1.2.3")

    def test_roundtrip_is_deterministic_and_cannot_overwrite(self):
        self.package()
        first = self.archive.read_bytes()
        ART.verify(self.archive, self.checksum, self.expected, "1.2.3")
        with self.assertRaisesRegex(ValueError, "overwrite"):
            self.package()
        another = self.root / "another"
        another.mkdir()
        ART.package(self.binary, self.license, another / self.archive.name, self.expected, 1, "1.2.3")
        self.assertEqual(first, (another / self.archive.name).read_bytes())

    def test_binary_build_identity_must_match(self):
        with self.assertRaisesRegex(ValueError, "release identity"):
            ART.verify_binary(self.binary.read_bytes(), self.expected.replace(b"source_revision=a", b"source_revision=b"))

    def test_checksum_cannot_reference_another_file(self):
        self.package()
        self.checksum.write_text(hashlib.sha256(self.archive.read_bytes()).hexdigest() + "  another-file\n")
        with self.assertRaisesRegex(ValueError, "checksum"):
            ART.verify(self.archive, self.checksum, self.expected, "1.2.3")

    def test_links_duplicates_and_wrong_modes_refuse(self):
        for kind in ["symlink", "duplicate", "mode"]:
            with self.subTest(kind=kind):
                data = io.BytesIO()
                with tarfile.open(fileobj=data, mode="w:gz") as bundle:
                    member = tarfile.TarInfo("LICENSE")
                    member.mode = 0o777 if kind == "mode" else 0o644
                    member.size = 0
                    if kind == "symlink":
                        member.type = tarfile.SYMTYPE
                        member.linkname = "/etc/passwd"
                    bundle.addfile(member, io.BytesIO())
                    if kind == "duplicate":
                        bundle.addfile(member, io.BytesIO())
                self.archive.write_bytes(data.getvalue())
                self.checksum.write_text(hashlib.sha256(data.getvalue()).hexdigest() + f"  {self.archive.name}\n")
                with self.assertRaisesRegex(ValueError, "inventory"):
                    ART.verify(self.archive, self.checksum, self.expected, "1.2.3")


if __name__ == "__main__":
    unittest.main()
