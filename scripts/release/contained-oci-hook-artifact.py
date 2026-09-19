#!/usr/bin/env python3
"""Package/verify bounded hook archives without executing the hook.

Embedded build identity and byte integrity are checked here. Authenticity belongs
to the release attestation channel, not to an adjacent checksum file.
"""
import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import io
from pathlib import Path
import re
import struct
import subprocess
import tarfile
import tempfile

LIMIT = 536870912
SECTION = ".ryeos_contained_oci_hook_build"
INVENTORY = {"LICENSE": 0o644, "RYEOS-BUILD": 0o644, "ryeos-lillux-oci-hook": 0o555}


def testimony(version, revision, date, epoch):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("invalid release version")
    if not re.fullmatch(r"[0-9a-f]{40}", revision) or epoch < 0:
        raise ValueError("invalid source revision/epoch")
    if datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ") != date:
        raise ValueError("build date and epoch differ")
    return (f"schema=ryeos.contained-oci-hook-build.v1\nqualification=exact\nversion={version}\n"
            f"source_revision={revision}\nbuild_date={date}\nsource_date_epoch={epoch}\n"
            "target=x86_64-unknown-linux-gnu\nprofile=release\n").encode()


def regular_bytes(path, maximum=LIMIT):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > maximum:
        raise ValueError(f"unsafe or oversized input: {path}")
    with path.open("rb") as stream:
        value = stream.read(maximum + 1)
    if len(value) > maximum:
        raise ValueError("input exceeded its size bound")
    return value


def verify_binary(binary, expected):
    if (len(binary) < 64 or binary[:6] != b"\x7fELF\x02\x01"
            or struct.unpack_from("<H", binary, 18)[0] != 62
            or struct.unpack_from("<H", binary, 16)[0] not in (2, 3)):
        raise ValueError("hook must be an x86-64 ELF executable")
    with tempfile.TemporaryDirectory(prefix="ryeos-hook-verify-") as directory:
        root = Path(directory)
        source = root / "hook"
        source.write_bytes(binary)
        subprocess.run(["objcopy", "--dump-section", f"{SECTION}={root / 'build'}", str(source)],
                       check=True, capture_output=True)
        if regular_bytes(root / "build", 4096) != expected:
            raise ValueError("hook binary does not embed the requested release identity")


def verify(archive, checksum, expected, version):
    name = f"ryeos-contained-oci-hook-{version}-x86_64-unknown-linux-gnu.tar.gz"
    if archive.name != name or checksum.name != name + ".sha256":
        raise ValueError("unexpected archive/checksum filename")
    data = regular_bytes(archive)
    digest = hashlib.sha256(data).hexdigest()
    if regular_bytes(checksum, 512) != f"{digest}  {name}\n".encode():
        raise ValueError("checksum does not bind this exact archive")
    # Bound decompression before tarfile parses extension records or padding.
    with gzip.GzipFile(fileobj=io.BytesIO(data)) as compressed:
        expanded = compressed.read(LIMIT + 1)
    if len(expanded) > LIMIT:
        raise ValueError("archive exceeds expanded byte bound")
    files = {}
    total = 0
    with tarfile.open(fileobj=io.BytesIO(expanded), mode="r:") as bundle:
        for member in bundle:
            if (member.name not in INVENTORY or member.name in files or not member.isfile()
                    or member.mode != INVENTORY[member.name] or member.uid != 0 or member.gid != 0):
                raise ValueError("archive inventory, type, mode, or owner is not exact")
            total += member.size
            if member.size < 0 or total > LIMIT:
                raise ValueError("archive exceeds expanded byte bound")
            files[member.name] = bundle.extractfile(member).read(member.size + 1)
            if len(files[member.name]) != member.size:
                raise ValueError("archive member size mismatch")
    if set(files) != set(INVENTORY) or files["RYEOS-BUILD"] != expected:
        raise ValueError("archive identity or inventory mismatch")
    verify_binary(files["ryeos-lillux-oci-hook"], expected)
    return files


def package(binary, license_path, output, expected, epoch, version):
    files = {"LICENSE": regular_bytes(license_path, 1048576), "RYEOS-BUILD": expected,
             "ryeos-lillux-oci-hook": regular_bytes(binary)}
    if sum(map(len, files.values())) > LIMIT - 10240:
        raise ValueError("hook package exceeds expanded byte bound")
    verify_binary(files["ryeos-lillux-oci-hook"], expected)
    checksum = Path(str(output) + ".sha256")
    if output.name != f"ryeos-contained-oci-hook-{version}-x86_64-unknown-linux-gnu.tar.gz":
        raise ValueError("unexpected output filename")
    if output.exists() or output.is_symlink() or checksum.exists() or checksum.is_symlink():
        raise ValueError("refusing to overwrite hook publication")
    # Interrupted outputs are retained for diagnosis, never overwritten.
    with output.open("xb") as stream:
        with gzip.GzipFile(filename="", fileobj=stream, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as bundle:
                for name, value in sorted(files.items()):
                    member = tarfile.TarInfo(name)
                    member.mode = INVENTORY[name]
                    member.size = len(value)
                    member.mtime = epoch
                    bundle.addfile(member, io.BytesIO(value))
    digest = hashlib.sha256(regular_bytes(output)).hexdigest()
    with checksum.open("xb") as stream:
        stream.write(f"{digest}  {output.name}\n".encode())
    verify(output, checksum, expected, version)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["package", "verify"])
    parser.add_argument("--version", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--build-date", required=True)
    parser.add_argument("--source-date-epoch", type=int, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--checksum", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--license", type=Path)
    args = parser.parse_args()
    expected = testimony(args.version, args.source_revision, args.build_date, args.source_date_epoch)
    if args.operation == "verify":
        verify(args.archive, args.checksum or Path(str(args.archive) + ".sha256"), expected, args.version)
    else:
        if args.binary is None or args.license is None:
            parser.error("package requires --binary and --license")
        package(args.binary, args.license, args.archive, expected, args.source_date_epoch, args.version)


if __name__ == "__main__":
    main()
