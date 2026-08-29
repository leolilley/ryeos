#!/usr/bin/env python3
"""Author exact publisher realization archives for local inference.

This release-only utility is never installed into a RyeOS bundle and never runs
on an activation target. Every download has an exact upstream identity and
SHA-256. The resulting already-final trees are published as canonical archives;
node activation performs no transforms.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.request


PYTHON_ARCHIVE = (
    "cpython-3.14.7+20260807-x86_64-unknown-linux-musl-install_only_stripped.tar.gz",
    "1fe25b50644b50b3333afa0d4013cc9cbab4dde4284c0154aebef4f53523ed99",
    "https://github.com/astral-sh/python-build-standalone/releases/download/20260807/"
    "cpython-3.14.7%2B20260807-x86_64-unknown-linux-musl-install_only_stripped.tar.gz",
)
PYTHON_FULL_ARCHIVE = (
    "cpython-3.14.7+20260807-x86_64-unknown-linux-musl-lto-full.tar.zst",
    "89874e4bff9cc1bafd69f7eb02f9002835b2f84b557eabccf281169c95bdaef8",
    "https://github.com/astral-sh/python-build-standalone/releases/download/20260807/"
    "cpython-3.14.7%2B20260807-x86_64-unknown-linux-musl-lto-full.tar.zst",
)
PYTHON_ZSTD_SOURCE_ARCHIVE = (
    "cpython-source-deps-zstd-1.5.7.tar.gz",
    "f24b52470d12f466e9fa4fcc94e6c530625ada51d7b36de7fdc6ed7e6f499c8e",
    "https://github.com/python/cpython-source-deps/archive/refs/tags/"
    "zstd-1.5.7.tar.gz",
)
MUSL_APK = (
    "musl-1.2.5-r12.apk",
    "4990a5e0ba312e478f94cfe431a70efef1538004eb361c8ae424516848be45bb",
    "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/musl-1.2.5-r12.apk",
)
MUSL_SOURCE_ARCHIVE = (
    "musl-1.2.5.tar.gz",
    "a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4",
    "https://musl.libc.org/releases/musl-1.2.5.tar.gz",
)
LLVM_TOOLS_APK = (
    "llvm20-20.1.8-r0.apk",
    "1f7e27c9ca7dbf24a514909a3824616800706d35ef41747d09df1016ea47d6df",
    "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/llvm20-20.1.8-r0.apk",
)
TINYGRAD_ARCHIVE = (
    "tinygrad-4c206a52b1a72a98db8c97576959b54fa2a38232.tar.gz",
    "2e93802821a85027031162a0c6e1b543934064cc35fdb2d0730f7f38330a9ce0",
    "https://github.com/tinygrad/tinygrad/archive/"
    "4c206a52b1a72a98db8c97576959b54fa2a38232.tar.gz",
)

# Exact upstream notices for the binary subset retained from Alpine packages.
# Package-owned .PKGINFO records are retained separately so the resulting
# realization carries both Alpine's exact license classification and the
# corresponding upstream terms/notices. These are release inputs just like
# the binaries: offline authoring requires the digest-pinned cache entries.
TOOLCHAIN_LICENSE_ARTIFACTS = [
    (
        "llvm-project-20.1.8-LICENSE.TXT",
        "8d85c1057d742e597985c7d4e6320b015a9139385cff4cbae06ffc0ebe89afee",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/LICENSE.TXT",
    ),
    (
        "llvm-project-20.1.8-ConvertUTF.cpp",
        "d425e131c4c1e59ad19139ba7bdbebb2cb78cd5253b568b0359001bf08a8a25e",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/ConvertUTF.cpp",
    ),
    (
        "llvm-project-20.1.8-UnicodeNameToCodepointGenerated.cpp",
        "cf183ee415e1b249b0a4f1755b5a11a95d94c7f723010667ca0f6e4964369be7",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/UnicodeNameToCodepointGenerated.cpp",
    ),
    (
        "llvm-project-20.1.8-xxhash.cpp",
        "b47e89a65e40f34c7e336a58f1902c958b7bd90b3370bd497c8cb788eb40c2d4",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/xxhash.cpp",
    ),
    (
        "llvm-project-20.1.8-COPYRIGHT.regex",
        "0424e57d4303164dc59a8509c20dae0518b853692e5c2b0e98b11816fdbc97c7",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/COPYRIGHT.regex",
    ),
    (
        "llvm-project-20.1.8-BLAKE3-LICENSE",
        "6a94bedb8b707ed97f6e310d0d015ab14e0683ffa0a612b02958581b9cc9fc0e",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/BLAKE3/LICENSE",
    ),
    (
        "llvm-project-20.1.8-MD5.cpp",
        "44256f3d849f65a77140514d87474a00f03322038a40f14c71918b29481977a4",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/MD5.cpp",
    ),
    (
        "llvm-project-20.1.8-SHA1.cpp",
        "cc6c4b80b5c2a85f915fd336b72a87aeac696a03c30ce87756e71b060c5ca8a9",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/SHA1.cpp",
    ),
    (
        "llvm-project-20.1.8-SHA256.cpp",
        "9b1f22d8181e5776527fe8d45948dc31d99d264a68065f6da6d8fab0db7ea232",
        "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-20.1.8/"
        "llvm/lib/Support/SHA256.cpp",
    ),
    (
        "gcc-14.2.0-COPYING3",
        "8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903",
        "https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-14.2.0/COPYING3",
    ),
    (
        "gcc-14.2.0-COPYING.RUNTIME",
        "9d6b43ce4d8de0c878bf16b54d8e7a10d9bd42b75178153e3af6a815bdc90f74",
        "https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-14.2.0/COPYING.RUNTIME",
    ),
    (
        "gcc-14.2.0-COPYING.LIB",
        "a9bdde5616ecdd1e980b44f360600ee8783b1f99b8cc83a2beb163a0a390e861",
        "https://raw.githubusercontent.com/gcc-mirror/gcc/releases/gcc-14.2.0/COPYING.LIB",
    ),
    (
        "libffi-3.4.8-LICENSE",
        "67894089811f93fca47a76f85e017da6f8582d4ba0905963c6e0f1ad6df7a195",
        "https://raw.githubusercontent.com/libffi/libffi/v3.4.8/LICENSE",
    ),
    (
        "libxml2-2.13.9-Copyright",
        "c99aae1afe013e50b8b3701e089222b351258043c3025b64053a233fd25b4be7",
        "https://raw.githubusercontent.com/GNOME/libxml2/v2.13.9/Copyright",
    ),
    (
        "zlib-1.3.2-LICENSE",
        "e32ff4e00d9d94930537635291da39e7e612703334bf6fde8c7f1686fe8a45a2",
        "https://raw.githubusercontent.com/madler/zlib/v1.3.2/LICENSE",
    ),
    (
        "zstd-1.5.7-LICENSE",
        "7055266497633c9025b777c78eb7235af13922117480ed5c674677adc381c9d8",
        "https://raw.githubusercontent.com/facebook/zstd/v1.5.7/LICENSE",
    ),
    (
        "zstd-1.5.7-COPYING",
        "f9c375a1be4a41f7b70301dd83c91cb89e41567478859b77eef375a52d782505",
        "https://raw.githubusercontent.com/facebook/zstd/v1.5.7/COPYING",
    ),
    (
        "xz-5.8.3-COPYING",
        "616a3ad264ce29b8f1cb97e53037b139d406899ca8d1f799651e17bfa09830b8",
        "https://raw.githubusercontent.com/tukaani-project/xz/v5.8.3/COPYING",
    ),
    (
        "xz-5.8.3-COPYING.0BSD",
        "0b01625d853911cd0e2e088dcfb743261034a091bb379246cb25a14cc4c74bf1",
        "https://raw.githubusercontent.com/tukaani-project/xz/v5.8.3/COPYING.0BSD",
    ),
    (
        "xz-5.8.3-COPYING.GPLv2",
        "edaef632cbb643e4e7a221717a6c441a4c1a7c918e6e4d56debc3d8739b233f6",
        "https://raw.githubusercontent.com/tukaani-project/xz/v5.8.3/COPYING.GPLv2",
    ),
    (
        "xz-5.8.3-COPYING.LGPLv2.1",
        "20e50fe7aae3e56378ebf0417d9de904f55a0e61e4df315333e632a4d3555d95",
        "https://raw.githubusercontent.com/tukaani-project/xz/v5.8.3/COPYING.LGPLv2.1",
    ),
]

# The realized toolchain retains Alpine's libgcc, libstdc++, and xz-libs
# binaries. Publish the exact corresponding upstream sources and exact Alpine
# packaging trees beside the immutable realization assets. These are release
# artifacts, not activation inputs.
CORRESPONDING_SOURCE_GROUPS = [
    {
        "packages": ["libgcc-14.2.0-r6", "libstdc++-14.2.0-r6"],
        "upstream": (
            "gcc-14.2.0.tar.xz",
            "a7b39bc69cbf9e25826c5a60ab26477001f7c08d85cec04bc0e29cabed6f3cc9",
            "https://ftp.gnu.org/gnu/gcc/gcc-14.2.0/gcc-14.2.0.tar.xz",
        ),
        "packaging": (
            "alpine-aports-fbf60319be3bbaf6dd32ef55cc6fb7189e05c266.tar.gz",
            "f18eb64afcfbc1c7c0bf179951541141e6dd557df0f130df27ac933e0b868096",
            "https://github.com/alpinelinux/aports/archive/"
            "fbf60319be3bbaf6dd32ef55cc6fb7189e05c266.tar.gz",
        ),
        "packaging_commit": "fbf60319be3bbaf6dd32ef55cc6fb7189e05c266",
    },
    {
        "packages": ["xz-libs-5.8.3-r0"],
        "upstream": (
            "xz-5.8.3.tar.gz",
            "8ec1767fa517642ecb4cf08b891ce667ba6f143551e382b07c7ef437bda335e2",
            "https://github.com/tukaani-project/xz/releases/download/"
            "v5.8.3/xz-5.8.3.tar.gz",
        ),
        "packaging": (
            "alpine-aports-57e3e2dd527538456ee622bf3328f2c34575dd65.tar.gz",
            "a8eb0c86c815899b937ca67293d310163573a5f83c6d646a36f3c473fee79b23",
            "https://github.com/alpinelinux/aports/archive/"
            "57e3e2dd527538456ee622bf3328f2c34575dd65.tar.gz",
        ),
        "packaging_commit": "57e3e2dd527538456ee622bf3328f2c34575dd65",
    },
]

ALPINE_PACKAGES = [
    ("clang20", "20.1.8-r0", "1eb2e8cfbf97e96ffc514dd557d81cea180bfbfe0a08c405102b4d216952df49"),
    ("clang20-libs", "20.1.8-r0", "2c07083a734330bb1aeba30d0e41d29e671ee53582d1ad2d19733be1210cd8da"),
    ("compiler-rt", "20.1.8-r0", "814c1ed97c36569389337be6e56f225347281bc9c7bb4014d16514997b74e1c5"),
    ("lld20", "20.1.8-r0", "51b54c45a929fc255396cd1f5e4a3332d3a7361dd8855a88acda2b49e0b600b9"),
    ("lld20-libs", "20.1.8-r0", "42e90a7d2809b56bf92a07789d463c8b1178ff8baab773bdb8ec8e65baa049d6"),
    ("llvm20-libs", "20.1.8-r0", "d0926e20a9e2e65f03f673c1d9d2e69f807e5858920146a0eaedaa68b30fe443"),
    ("libgcc", "14.2.0-r6", "04f3467bc967e705221a843fe4d3de5850db826e571686e0c0ed453d38cb5c59"),
    ("libstdc++", "14.2.0-r6", "939f7c99898f3e8154207a17f4acbe8bc40437e1bb1b43f5525620ca9e452a2e"),
    ("libffi", "3.4.8-r0", "9a75cb9024693c1e52c3d8d7c9afb7c79e6e20f6c08df28effdb8dd816095083"),
    ("libxml2", "2.13.9-r1", "be361698c6f728f492b99550e99e898ae478940fe593d8c11b14c5f3d1b5a938"),
    ("zlib", "1.3.2-r0", "1f3d5f463f490dad3a68097376711bfe5e8156e9e8daff3070513aa4378cdeca"),
    ("zstd-libs", "1.5.7-r0", "1bdd6e57cfbfbfd6e8481cad37ddd5d199950715bec1879b3afb600272dbb09e"),
    ("xz-libs", "5.8.3-r0", "ebb49f0e8efb6ac774d5d8f21a7cf7d63c4658fa36ab1ebaf48ebd67efd60142"),
]

MODEL_REVISION = "c1899de289a04d12100db370d81485cdf75e47ca"
MODEL_FILES = {
    "config.json": "660db3b73d788119c04535e48cf9be5f55bc3100841a718637ae695b442f27dd",
    "generation_config.json": "2325da0f15bb848e018c5ae071b7943332e9f871d6b60e2ed22ca97d4cb993d2",
    "model.safetensors": "f47f71177f32bcd101b7573ec9171e6a57f4f4d31148d38e382306f42996874b",
    "tokenizer.json": "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4",
    "tokenizer_config.json": "d5d09f07b48c3086c508b30d1c9114bd1189145b74e982a265350c923acd8101",
    "LICENSE": "832dd9e00a68dd83b3c3fb9f5588dad7dcf337a0db50f7d9483f310cd292e92e",
}

RELEASE_CONTRACT_PATH = Path(__file__).with_name(
    "local-inference-qwen3-0.6b-v1.json"
)
RELEASE_CONTRACT = json.loads(RELEASE_CONTRACT_PATH.read_text(encoding="utf-8"))
REALIZATION_RELEASE_TAG = RELEASE_CONTRACT["release_tag"]
REALIZATION_PREFIX = "ryeos-local-inference-qwen3-0.6b"
CONTENT_FILE_LIMIT = 32 * 1024 * 1024
LARGE_CHUNK_BYTES = 64 * 1024 * 1024
def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def obtain(cache: Path, artifact: tuple[str, str, str], offline: bool) -> Path:
    name, expected, url = artifact
    destination = cache / name
    if destination.is_file() and sha256(destination) == expected:
        return destination
    if destination.exists():
        raise RuntimeError(f"cached artifact {destination} has the wrong digest")
    if offline:
        raise RuntimeError(f"offline realization authoring is missing {name}")
    temporary = cache / f".{name}.download"
    if temporary.is_symlink() or temporary.is_file():
        temporary.unlink()
    elif temporary.exists():
        raise RuntimeError(f"download staging path is not a regular file: {temporary}")
    request = urllib.request.Request(
        url,
        headers={"User-Agent": "RyeOS-local-inference-release-authoring/1"},
    )
    try:
        with urllib.request.urlopen(request) as response, temporary.open("xb") as output:
            shutil.copyfileobj(response, output, 1024 * 1024)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    if sha256(temporary) != expected:
        raise RuntimeError(f"downloaded artifact {name} has the wrong digest")
    os.replace(temporary, destination)
    return destination


def extract(archive: Path, destination: Path) -> None:
    destination.mkdir(parents=True)
    subprocess.run(
        [
            "tar",
            "--warning=no-unknown-keyword",
            "-xf",
            str(archive),
            "-C",
            str(destination),
        ],
        check=True,
    )


def copy(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination, follow_symlinks=True)


def write_origin(directory: Path, value: dict) -> None:
    (directory / "ORIGIN.json").write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def validate_python_license_metadata(
    metadata: Path,
    retained_licenses: Path,
    exceptions: dict[str, str],
) -> None:
    declared: set[str] = set()

    def visit(value: object) -> None:
        if isinstance(value, dict):
            for key, child in value.items():
                if key == "license_path" and isinstance(child, str):
                    declared.add(child)
                elif key == "license_paths" and isinstance(child, list):
                    if not all(isinstance(path, str) for path in child):
                        raise RuntimeError("PYTHON.json has a non-string license path")
                    declared.update(child)
                else:
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    visit(json.loads(metadata.read_text(encoding="utf-8")))
    absent: set[str] = set()
    for path in declared:
        if not path.startswith("licenses/") or Path(path).is_absolute() or ".." in Path(path).parts:
            raise RuntimeError(f"PYTHON.json has a non-canonical license path: {path}")
        relative = Path(path).relative_to("licenses")
        if not (retained_licenses / relative).is_file():
            absent.add(path)
    if absent != set(exceptions):
        raise RuntimeError(
            "PYTHON.json license closure changed: absent="
            f"{sorted(absent)}, admitted exceptions={sorted(exceptions)}"
        )


def retain_package_metadata(source: Path, destination: Path) -> None:
    metadata = source / ".PKGINFO"
    if not metadata.is_file():
        raise RuntimeError(f"Alpine package metadata is absent: {metadata}")
    copy(metadata, destination)


def remove_exact(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.is_dir():
        shutil.rmtree(path)
    else:
        raise RuntimeError(f"expected runtime exclusion is absent: {path}")


def write_toolchain_origin(directory: Path, packages: list[dict], selection: str) -> None:
    """Preserve the already-admitted canonical authoring bytes exactly."""
    lines = [
        "{",
        '  "distribution": "Alpine Linux",',
        '  "release": "v3.22",',
        '  "repository": "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/",',
        '  "packages": [',
    ]
    for index, package in enumerate(packages):
        suffix = "," if index + 1 < len(packages) else ""
        lines.append("    " + json.dumps(package, separators=(", ", ": ")) + suffix)
    lines.extend([
        "  ],",
        '  "selection": ' + json.dumps(selection),
        "}",
    ])
    origin = directory / "ORIGIN.json"
    origin.write_text("\n".join(lines) + "\n", encoding="utf-8")


def build_runtime(root: Path, cache: Path, scratch: Path, offline: bool) -> None:
    runtime = root / "runtime"
    extract(obtain(cache, PYTHON_ARCHIVE, offline), runtime)
    python = runtime / "python"
    for relative in (
        "bin/idle3",
        "bin/idle3.14",
        "lib/itcl4.3.8",
        "lib/libtcl9.0.so",
        "lib/libtcl9tk9.0.so",
        "lib/python3.14/idlelib",
        "lib/python3.14/lib-dynload/_dbm.cpython-314-x86_64-linux-musl.so",
        "lib/python3.14/lib-dynload/_tkinter.cpython-314-x86_64-linux-musl.so",
        "lib/python3.14/tkinter",
        "lib/tcl9",
        "lib/tcl9.0",
        "lib/thread3.0.6",
        "lib/tk9.0",
    ):
        remove_exact(python / relative)
    musl = scratch / "musl"
    extract(obtain(cache, MUSL_APK, offline), musl)
    copy(musl / "lib/ld-musl-x86_64.so.1", runtime / "lib/ld-musl-x86_64.so.1")
    copy(musl / "lib/ld-musl-x86_64.so.1", runtime / "lib/libc.so")
    licenses = runtime / "LICENSES"
    licenses.mkdir()
    python_metadata = scratch / "python-full-metadata"
    python_metadata.mkdir()
    full_archive = obtain(cache, PYTHON_FULL_ARCHIVE, offline)
    subprocess.run(
        [
            "tar",
            "--extract",
            "--file",
            str(full_archive),
            "--directory",
            str(python_metadata),
            "python/PYTHON.json",
            "python/licenses",
        ],
        check=True,
    )
    retained_python_licenses = licenses / "python-build-standalone"
    shutil.copytree(
        python_metadata / "python/licenses",
        retained_python_licenses,
        symlinks=True,
    )
    copy(
        python_metadata / "python/PYTHON.json",
        licenses / "python-build-standalone-PYTHON.json",
    )
    python_zstd_source = scratch / "python-zstd-source"
    extract(obtain(cache, PYTHON_ZSTD_SOURCE_ARCHIVE, offline), python_zstd_source)
    python_zstd_root = python_zstd_source / "cpython-source-deps-zstd-1.5.7"
    copy(python_zstd_root / "LICENSE", retained_python_licenses / "LICENSE.zstd.txt")
    copy(python_zstd_root / "COPYING", retained_python_licenses / "COPYING.zstd.txt")
    python_license_exceptions = {
        "licenses/LICENSE.zlib-ng.txt": (
            "stale metadata: the exact 20260807 Unix build-zlib.sh selects "
            "zlib-${ZLIB_VERSION}; the retained runtime reports zlib 1.3.2 and "
            "LICENSE.zlib.txt is present"
        ),
    }
    validate_python_license_metadata(
        python_metadata / "python/PYTHON.json",
        retained_python_licenses,
        python_license_exceptions,
    )
    retain_package_metadata(musl, licenses / "alpine-musl-1.2.5-r12.PKGINFO")
    musl_source = scratch / "musl-source"
    extract(obtain(cache, MUSL_SOURCE_ARCHIVE, offline), musl_source)
    copy(musl_source / "musl-1.2.5/COPYRIGHT", licenses / "musl-1.2.5-COPYRIGHT")
    write_origin(licenses, {
        "coverage": {
            "python_and_static_dependency_closure": [
                "python-build-standalone-PYTHON.json",
                "python-build-standalone/",
            ],
            "musl": [
                "alpine-musl-1.2.5-r12.PKGINFO",
                "musl-1.2.5-COPYRIGHT",
            ],
        },
        "musl_source_archive": {
            "artifact": MUSL_SOURCE_ARCHIVE[0],
            "sha256": MUSL_SOURCE_ARCHIVE[1],
            "source": MUSL_SOURCE_ARCHIVE[2],
        },
        "python_license_metadata_archive": {
            "artifact": PYTHON_FULL_ARCHIVE[0],
            "sha256": PYTHON_FULL_ARCHIVE[1],
            "source": PYTHON_FULL_ARCHIVE[2],
        },
        "python_zstd_source_archive": {
            "artifact": PYTHON_ZSTD_SOURCE_ARCHIVE[0],
            "sha256": PYTHON_ZSTD_SOURCE_ARCHIVE[1],
            "source": PYTHON_ZSTD_SOURCE_ARCHIVE[2],
        },
        "metadata_exceptions": python_license_exceptions,
    })
    write_origin(runtime, {
        "python": {
            "distribution": "astral-sh/python-build-standalone", "release": "20260807",
            "version": "3.14.7", "target": "x86_64-unknown-linux-musl",
            "variant": "install_only_stripped", "artifact": PYTHON_ARCHIVE[0],
            "archive_sha256": PYTHON_ARCHIVE[1],
            "source": "https://github.com/astral-sh/python-build-standalone/releases/tag/20260807",
        },
        "dynamic_runtime": {
            "distribution": "Alpine Linux", "release": "v3.22",
            "package": MUSL_APK[0], "package_sha256": MUSL_APK[1],
            "source": "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/",
            "selection": "ld-musl-x86_64.so.1 and libc.musl-x86_64.so.1",
            "selected_sha256": "1cc5c3f502179e2a4befae29658fb109102effefc75742893aa4a449fd3fbb03",
        },
        "python_executable_sha256": "2d8886b2bad1105014ce23e2439b20d47a78416281b906e21ac0e37085d19e9d",
        "libpython_sha256": "4f0a44d3b4e19d8e07b09127e03ca5ca3d03a190686fda7f13aae065ad0e115a",
        "exclusions": [
            "_dbm extension; _tkinter extension, tkinter and idlelib modules/launchers; and the bundled Tcl/Tk/Itcl/Thread shared libraries and resource trees"
        ],
    })


def build_tinygrad(root: Path, cache: Path, scratch: Path, offline: bool) -> None:
    extracted = scratch / "tinygrad-source"
    extract(obtain(cache, TINYGRAD_ARCHIVE, offline), extracted)
    source = extracted / "tinygrad-4c206a52b1a72a98db8c97576959b54fa2a38232"
    target = root / "tinygrad"
    target.mkdir()
    shutil.copytree(source / "tinygrad", target / "tinygrad", symlinks=True)
    copy(source / "LICENSE", target / "LICENSE")
    write_origin(target, {
        "commit": "4c206a52b1a72a98db8c97576959b54fa2a38232",
        "repository": "https://github.com/tinygrad/tinygrad",
        "selection": "tinygrad package and LICENSE",
    })


def build_toolchain(root: Path, cache: Path, scratch: Path, offline: bool) -> None:
    extracted: dict[str, Path] = {}
    package_origins = []
    for name, version, digest in ALPINE_PACKAGES:
        filename = f"{name}-{version}.apk"
        archive = obtain(cache, (
            filename, digest,
            f"https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/{filename}",
        ), offline)
        destination = scratch / f"apk-{name}"
        extract(archive, destination)
        extracted[name] = destination
        package_origins.append({"name": name, "version": version, "sha256": digest})
    llvm_tools = scratch / "apk-llvm20-authoring"
    extract(obtain(cache, LLVM_TOOLS_APK, offline), llvm_tools)
    package_origins.append({
        "name": "llvm20",
        "version": "20.1.8-r0",
        "sha256": LLVM_TOOLS_APK[1],
        "role": "publisher_authoring_only",
    })
    target = root / "toolchain"
    target.mkdir()
    licenses = target / "LICENSES"
    alpine_metadata = licenses / "alpine"
    upstream_licenses = licenses / "upstream"
    alpine_metadata.mkdir(parents=True)
    upstream_licenses.mkdir()
    for name, version, _ in ALPINE_PACKAGES:
        retain_package_metadata(
            extracted[name], alpine_metadata / f"{name}-{version}.PKGINFO"
        )
    for artifact in TOOLCHAIN_LICENSE_ARTIFACTS:
        copy(obtain(cache, artifact, offline), upstream_licenses / artifact[0])
    write_origin(licenses, {
        "alpine_package_metadata": [
            f"alpine/{name}-{version}.PKGINFO"
            for name, version, _ in ALPINE_PACKAGES
        ],
        "upstream_notices": [
            {"path": f"upstream/{name}", "sha256": digest, "source": url}
            for name, digest, url in TOOLCHAIN_LICENSE_ARTIFACTS
        ],
    })
    selections = {
        "bin/clang": ("clang20", "usr/lib/llvm20/bin/clang"),
        "bin/ld.lld": ("lld20", "usr/bin/ld.lld"),
        "lib/libLLVM.so.20.1": ("llvm20-libs", "usr/lib/libLLVM.so.20.1"),
        "lib/libclang-cpp.so.20.1": ("clang20-libs", "usr/lib/libclang-cpp.so.20.1"),
        "lib/libgcc_s.so.1": ("libgcc", "usr/lib/libgcc_s.so.1"),
        "lib/libstdc++.so.6.0.33": ("libstdc++", "usr/lib/libstdc++.so.6.0.33"),
        "lib/libffi.so.8.1.4": ("libffi", "usr/lib/libffi.so.8.1.4"),
        "lib/libxml2.so.2.13.9": ("libxml2", "usr/lib/libxml2.so.2.13.9"),
        "lib/libz.so.1.3.2": ("zlib", "usr/lib/libz.so.1.3.2"),
        "lib/libzstd.so.1.5.7": ("zstd-libs", "usr/lib/libzstd.so.1.5.7"),
        "lib/liblzma.so.5.8.3": ("xz-libs", "usr/lib/liblzma.so.5.8.3"),
    }
    for library in ["COFF", "Common", "ELF", "MachO", "MinGW", "Wasm"]:
        selections[f"lib/liblld{library}.so.20.1"] = (
            "lld20-libs", f"usr/lib/liblld{library}.so.20.1",
        )
    for destination, (package, source) in selections.items():
        copy(extracted[package] / source, target / destination)
    builtins = target / "lib/clang/20/lib/x86_64-alpine-linux-musl/libclang_rt.builtins-x86_64.a"
    copy(
        extracted["compiler-rt"]
        / "usr/lib/llvm20/lib/clang/20/lib/x86_64-alpine-linux-musl/libclang_rt.builtins-x86_64.a",
        builtins,
    )
    if sha256(builtins) != "7f5b9b31fb8405678e3f9985d1f7f4279d2ac626ad25b88ee3adeef4395ca258":
        raise RuntimeError("upstream compiler-rt archive does not match its exact selected bytes")
    musl = scratch / "musl"
    if not musl.is_dir():
        extract(obtain(cache, MUSL_APK, offline), musl)
    llvm_strip = llvm_tools / "usr/lib/llvm20/bin/llvm-strip"
    library_path = ":".join(
        str(path)
        for path in (
            extracted["llvm20-libs"] / "usr/lib",
            extracted["libstdc++"] / "usr/lib",
            extracted["libgcc"] / "usr/lib",
            extracted["libffi"] / "usr/lib",
            extracted["libxml2"] / "usr/lib",
            extracted["zlib"] / "usr/lib",
            extracted["zstd-libs"] / "usr/lib",
            extracted["xz-libs"] / "usr/lib",
            musl / "lib",
        )
    )
    subprocess.run(
        [
            str(musl / "lib/ld-musl-x86_64.so.1"),
            "--library-path",
            library_path,
            str(llvm_strip),
            "-g",
            str(builtins),
        ],
        check=True,
    )
    if sha256(builtins) != "aa95aabd8c9c36097acc608c72137e71b6200ccf7abdc97ec4541920f58eff05":
        raise RuntimeError("pinned llvm-strip did not reproduce the admitted compiler-rt bytes")
    for link, destination in {
        "lib/libstdc++.so.6": "libstdc++.so.6.0.33",
        "lib/libffi.so.8": "libffi.so.8.1.4",
        "lib/libxml2.so.2": "libxml2.so.2.13.9",
        "lib/libz.so.1": "libz.so.1.3.2",
        "lib/libzstd.so.1": "libzstd.so.1.5.7",
        "lib/liblzma.so.5": "liblzma.so.5.8.3",
    }.items():
        (target / link).symlink_to(destination)
    write_toolchain_origin(
        target,
        package_origins,
        "clang and lld with their complete runtime shared-library closure plus compiler-rt builtins stripped only of debug metadata by exact Alpine llvm20 20.1.8-r0 llvm-strip (input sha256 7f5b9b31fb8405678e3f9985d1f7f4279d2ac626ad25b88ee3adeef4395ca258, output sha256 aa95aabd8c9c36097acc608c72137e71b6200ccf7abdc97ec4541920f58eff05); the authoring tool is not retained; no headers, standard libraries, or unrelated compiler tools",
    )


def build_model(
    root: Path,
    cache: Path,
    offline: bool,
    model_source: Path | None,
) -> None:
    target = root / "model"
    target.mkdir()
    for name, digest in MODEL_FILES.items():
        if model_source is not None and name != "LICENSE":
            source = model_source / name
            if not source.is_file() or sha256(source) != digest:
                raise RuntimeError(
                    f"local model source {source} does not match the exact admitted digest"
                )
        else:
            source = obtain(cache, (
                f"qwen3-0.6b-{MODEL_REVISION}-{name}", digest,
                f"https://huggingface.co/Qwen/Qwen3-0.6B/resolve/{MODEL_REVISION}/{name}",
            ), offline)
        copy(source, target / name)
    write_origin(target, {
        "model": "Qwen/Qwen3-0.6B", "revision": MODEL_REVISION,
        "selection": "config.json, generation_config.json, model.safetensors, tokenizer.json, tokenizer_config.json, LICENSE",
        "source": "https://huggingface.co/Qwen/Qwen3-0.6B", "sha256": MODEL_FILES,
    })


def canonical_tree_entries(root: Path) -> list[Path]:
    entries: list[Path] = []

    def visit(directory: Path) -> None:
        with os.scandir(directory) as scanned:
            children = sorted(scanned, key=lambda entry: os.fsencode(entry.name))
        for child in children:
            path = Path(child.path)
            relative = path.relative_to(root).as_posix()
            if (
                not relative
                or relative.startswith("/")
                or relative.endswith("/")
                or "\\" in relative
                or any(part in ("", ".", "..") for part in relative.split("/"))
                or len(relative.encode()) > 4096
            ):
                raise RuntimeError(f"non-canonical realization path: {relative!r}")
            entries.append(path)
            if child.is_dir(follow_symlinks=False):
                visit(path)
            elif not (
                child.is_file(follow_symlinks=False)
                or child.is_symlink()
            ):
                raise RuntimeError(f"special realization entry: {relative}")

    visit(root)
    if not entries:
        raise RuntimeError(f"refusing empty realization tree: {root}")
    return entries


def canonical_tar_info(path: Path, archive_name: str) -> tarfile.TarInfo:
    observed = path.lstat()
    info = tarfile.TarInfo(archive_name)
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = 0
    info.pax_headers = {}
    if stat.S_ISDIR(observed.st_mode):
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        info.size = 0
    elif stat.S_ISREG(observed.st_mode):
        info.type = tarfile.REGTYPE
        info.mode = 0o755 if observed.st_mode & 0o111 else 0o644
        info.size = observed.st_size
    elif stat.S_ISLNK(observed.st_mode):
        target = os.readlink(path)
        if not target or os.path.isabs(target) or "\0" in target:
            raise RuntimeError(f"invalid realization symlink target: {path}")
        depth = len(Path(archive_name).parent.parts) - 1
        for part in Path(target).parts:
            if part == "..":
                if depth == 0:
                    raise RuntimeError(f"escaping realization symlink: {path} -> {target}")
                depth -= 1
            elif part != ".":
                depth += 1
        info.type = tarfile.SYMTYPE
        info.mode = 0o777
        info.size = 0
        info.linkname = target
    else:
        raise RuntimeError(f"special realization entry: {path}")
    return info


def manifest_file_hashes(path: Path, large: bool) -> dict[str, object]:
    whole = hashlib.sha256()
    chunks: list[str] = []
    with path.open("rb") as source:
        while True:
            chunk = source.read(LARGE_CHUNK_BYTES if large else 1024 * 1024)
            if not chunk:
                break
            whole.update(chunk)
            if large:
                chunks.append(hashlib.sha256(chunk).hexdigest())
    result: dict[str, object] = {"sha256": whole.hexdigest()}
    if large:
        result["chunk_hashes"] = chunks
    return result


def realization_manifest(component: str, tree: Path, storage: str) -> tuple[str, int]:
    manifest_entries: list[dict[str, object]] = []
    total_bytes = 0
    for path in sorted(
        canonical_tree_entries(tree),
        key=lambda candidate: candidate.relative_to(tree).as_posix().encode(),
    ):
        relative = path.relative_to(tree).as_posix()
        observed = path.lstat()
        if stat.S_ISDIR(observed.st_mode):
            entry: dict[str, object] = {"path": relative, "kind": "dir"}
        elif stat.S_ISLNK(observed.st_mode):
            entry = {
                "path": relative,
                "kind": "symlink",
                "target": os.readlink(path),
            }
        elif stat.S_ISREG(observed.st_mode):
            total_bytes += observed.st_size
            entry = {
                "path": relative,
                "kind": "file",
                "mode": 0o755 if observed.st_mode & 0o111 else 0o644,
                "size": observed.st_size,
            }
            use_large_object = storage == "large_content" and observed.st_size > CONTENT_FILE_LIMIT
            digests = manifest_file_hashes(path, use_large_object)
            if use_large_object:
                entry.update({
                    "file_sha256": digests["sha256"],
                    "chunk_size": LARGE_CHUNK_BYTES,
                    "chunk_hashes": digests["chunk_hashes"],
                })
            else:
                entry["blob_hash"] = digests["sha256"]
        else:
            raise RuntimeError(f"special realization entry: {relative}")
        manifest_entries.append(entry)
    manifest = {
        "schema": (
            "ryeos.external_content.tree.v2"
            if storage == "content"
            else "ryeos.external_content.large.v2"
        ),
        "kind": (
            "external_content_manifest"
            if storage == "content"
            else "external_large_content_manifest"
        ),
        "entries": manifest_entries,
        "entry_count": len(manifest_entries),
        "total_bytes": total_bytes,
    }
    canonical = json.dumps(
        manifest,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode()
    return hashlib.sha256(canonical).hexdigest(), len(manifest_entries)


def publish_corresponding_sources(
    cache: Path,
    destination: Path,
    offline: bool,
) -> list[dict[str, object]]:
    published: set[str] = set()
    result: list[dict[str, object]] = []
    for group in CORRESPONDING_SOURCE_GROUPS:
        item: dict[str, object] = {
            "packages": group["packages"],
            "packaging_commit": group["packaging_commit"],
        }
        for role in ("upstream", "packaging"):
            artifact = group[role]
            name, digest, origin_url = artifact
            source = obtain(cache, artifact, offline)
            if name not in published:
                copy(source, destination / name)
                (destination / f"{name}.sha256").write_text(
                    f"{digest}  {name}\n",
                    encoding="utf-8",
                )
                published.add(name)
            item[role] = {
                "archive": name,
                "sha256": digest,
                "origin_url": origin_url,
                "url": (
                    "https://github.com/leolilley/ryeos/releases/download/"
                    f"{REALIZATION_RELEASE_TAG}/{name}"
                ),
            }
        result.append(item)
    return result


def author_realization_archive(
    component: str,
    tree: Path,
    destination: Path,
    storage: str,
) -> dict[str, object]:
    prefix = f"{REALIZATION_PREFIX}-{component}-v1"
    archive_name = f"{prefix}.tar.gz"
    archive_path = destination / archive_name
    entries = canonical_tree_entries(tree)
    manifest_hash, manifest_entries = realization_manifest(component, tree, storage)
    root_info = tarfile.TarInfo(prefix)
    root_info.type = tarfile.DIRTYPE
    root_info.mode = 0o755
    root_info.uid = root_info.gid = root_info.mtime = 0
    root_info.uname = root_info.gname = ""
    root_info.size = 0
    root_info.pax_headers = {}

    with archive_path.open("xb") as raw:
        with gzip.GzipFile(
            filename="",
            mode="wb",
            compresslevel=9,
            fileobj=raw,
            mtime=0,
        ) as compressed:
            with tarfile.open(
                fileobj=compressed,
                mode="w|",
                format=tarfile.PAX_FORMAT,
            ) as archive:
                archive.addfile(root_info)
                for path in entries:
                    relative = path.relative_to(tree).as_posix()
                    info = canonical_tar_info(path, f"{prefix}/{relative}")
                    if info.isreg():
                        with path.open("rb") as source:
                            archive.addfile(info, source)
                    else:
                        archive.addfile(info)
        raw.flush()
        os.fsync(raw.fileno())

    expanded_bytes = 0
    with gzip.open(archive_path, "rb") as expanded:
        for chunk in iter(lambda: expanded.read(1024 * 1024), b""):
            expanded_bytes += len(chunk)
    observations = [(path, path.lstat()) for path in entries]
    maximum_file_bytes = max(
        (
            observed.st_size
            for _, observed in observations
            if stat.S_ISREG(observed.st_mode)
        ),
        default=0,
    )
    total_bytes = sum(
        observed.st_size
        for _, observed in observations
        if stat.S_ISREG(observed.st_mode)
    )
    maximum_depth = max(
        len(path.relative_to(tree).parts)
        + int(stat.S_ISDIR(observed.st_mode))
        for path, observed in observations
    )
    digest = sha256(archive_path)
    (destination / f"{archive_name}.sha256").write_text(
        f"{digest}  {archive_name}\n",
        encoding="utf-8",
    )
    return {
        "component": component,
        "storage": storage,
        "manifest_hash": manifest_hash,
        "manifest_entries": manifest_entries,
        "archive": archive_name,
        "url": (
            "https://github.com/leolilley/ryeos/releases/download/"
            f"{REALIZATION_RELEASE_TAG}/{archive_name}"
        ),
        "format": "tar_gzip",
        "sha256": digest,
        "maximum_compressed_bytes": archive_path.stat().st_size,
        "maximum_expanded_bytes": expanded_bytes,
        "maximum_entries": len(entries) + 1,
        "prefix": prefix,
        "bounds": {
            "maximum_entries": len(entries),
            "maximum_depth": maximum_depth,
            "maximum_file_bytes": maximum_file_bytes,
            "maximum_total_bytes": total_bytes,
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument(
        "--model-source",
        type=Path,
        help="optional exact local Qwen source directory for release authoring",
    )
    parser.add_argument(
        "--proposed-contract",
        type=Path,
        help=(
            "write a non-authoritative observed contract to an absent path when "
            "reviewed pins differ"
        ),
    )
    args = parser.parse_args()
    output = args.output.resolve(strict=False)
    cache = args.cache.resolve(strict=False)
    model_source = (
        args.model_source.resolve(strict=True)
        if args.model_source is not None
        else None
    )
    if output.exists():
        raise SystemExit(f"refusing existing output path: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    staging = Path(
        tempfile.mkdtemp(prefix=".ryeos-local-inference-artifacts-", dir=output.parent)
    )
    scratch = Path(
        tempfile.mkdtemp(prefix=".ryeos-local-inference-authoring-", dir=output.parent)
    )
    realizations = scratch / "realizations"
    realizations.mkdir()
    try:
        build_runtime(realizations, cache, scratch, args.offline)
        build_tinygrad(realizations, cache, scratch, args.offline)
        build_toolchain(realizations, cache, scratch, args.offline)
        build_model(realizations, cache, args.offline, model_source)
        pins = [
            author_realization_archive(component, realizations / component, staging, storage)
            for component, storage in (
                ("runtime", "content"),
                ("tinygrad", "content"),
                ("toolchain", "large_content"),
                ("model", "large_content"),
            )
        ]
        corresponding_sources = publish_corresponding_sources(
            cache, staging, args.offline
        )
        observed_contract = {
            "schema": "ryeos.local_inference_realization_release.v1",
            "release_tag": REALIZATION_RELEASE_TAG,
            "realizations": pins,
            "corresponding_sources": corresponding_sources,
        }
        if observed_contract != RELEASE_CONTRACT:
            if args.proposed_contract is not None:
                proposed = args.proposed_contract.resolve(strict=False)
                if proposed.exists():
                    raise RuntimeError(
                        f"refusing existing proposed-contract path: {proposed}"
                    )
                proposed.parent.mkdir(parents=True, exist_ok=True)
                proposed.write_text(
                    json.dumps(observed_contract, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
            raise RuntimeError(
                "authored realization archives do not match the reviewed release contract:\n"
                + json.dumps(observed_contract, indent=2, sort_keys=True)
            )
        (staging / "realizations.json").write_text(
            json.dumps(observed_contract, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.replace(staging, output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print((output / "realizations.json").read_text(encoding="utf-8"), end="")


if __name__ == "__main__":
    main()
