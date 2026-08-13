#!/usr/bin/env python3
"""Build the exact operator-import tree for worker:local-inference/local-tinygrad.

This is an authoring/activation utility, never part of worker execution. Every
download has an exact upstream identity and SHA-256; output is published only
after all selected bytes and the stripped compiler archive are verified.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request


PYTHON_ARCHIVE = (
    "cpython-3.14.7+20260807-x86_64-unknown-linux-musl-install_only_stripped.tar.gz",
    "1fe25b50644b50b3333afa0d4013cc9cbab4dde4284c0154aebef4f53523ed99",
    "https://github.com/astral-sh/python-build-standalone/releases/download/20260807/"
    "cpython-3.14.7%2B20260807-x86_64-unknown-linux-musl-install_only_stripped.tar.gz",
)
MUSL_APK = (
    "musl-1.2.5-r12.apk",
    "4990a5e0ba312e478f94cfe431a70efef1538004eb361c8ae424516848be45bb",
    "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/musl-1.2.5-r12.apk",
)
TINYGRAD_ARCHIVE = (
    "tinygrad-4c206a52b1a72a98db8c97576959b54fa2a38232.tar.gz",
    "2e93802821a85027031162a0c6e1b543934064cc35fdb2d0730f7f38330a9ce0",
    "https://github.com/tinygrad/tinygrad/archive/"
    "4c206a52b1a72a98db8c97576959b54fa2a38232.tar.gz",
)

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
}

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
        raise RuntimeError(f"offline assembly is missing {name}")
    temporary = cache / f".{name}.download"
    request = urllib.request.Request(url, headers={"User-Agent": "RyeOS-local-worker-assembly/1"})
    with urllib.request.urlopen(request) as response, temporary.open("xb") as output:
        shutil.copyfileobj(response, output, 1024 * 1024)
    if sha256(temporary) != expected:
        raise RuntimeError(f"downloaded artifact {name} has the wrong digest")
    os.replace(temporary, destination)
    return destination


def extract(archive: Path, destination: Path) -> None:
    destination.mkdir(parents=True)
    subprocess.run(["tar", "-xf", str(archive), "-C", str(destination)], check=True)


def copy(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination, follow_symlinks=True)


def write_origin(directory: Path, value: dict) -> None:
    (directory / "ORIGIN.json").write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


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
    expected = "ba6ac8869e267d66850781bc9adb0510c09499fcafb30213c6300f5589a446c6"
    if sha256(origin) != expected:
        raise RuntimeError("toolchain origin bytes do not reproduce the admitted identity")


def build_runtime(root: Path, cache: Path, scratch: Path, offline: bool) -> None:
    runtime = root / "runtime"
    extract(obtain(cache, PYTHON_ARCHIVE, offline), runtime)
    tkinter = runtime / "python/lib/python3.14/lib-dynload/_tkinter.cpython-314-x86_64-linux-musl.so"
    tkinter.unlink()
    musl = scratch / "musl"
    extract(obtain(cache, MUSL_APK, offline), musl)
    copy(musl / "lib/ld-musl-x86_64.so.1", runtime / "lib/ld-musl-x86_64.so.1")
    copy(musl / "lib/ld-musl-x86_64.so.1", runtime / "lib/libc.so")
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
        "exclusions": ["_tkinter extension and its Tcl/Tk dependencies"],
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
    target = root / "toolchain"
    target.mkdir()
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
    subprocess.run(["strip", "--strip-debug", str(builtins)], check=True)
    if sha256(builtins) != "0a4e258763c71ba8612696daf6daaaa66af7c861095a8f3b36cb7ea11211e472":
        raise RuntimeError("host strip did not reproduce the admitted compiler-rt archive")
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
        "clang and lld with their complete runtime shared-library closure plus the compiler-rt builtins archive; compiler-rt debug sections were stripped before capture (result sha256 0a4e258763c71ba8612696daf6daaaa66af7c861095a8f3b36cb7ea11211e472); no headers, standard libraries, or unrelated compiler tools",
    )


def build_model(root: Path, cache: Path, offline: bool) -> None:
    target = root / "model"
    target.mkdir()
    for name, digest in MODEL_FILES.items():
        source = obtain(cache, (
            f"qwen3-0.6b-{MODEL_REVISION}-{name}", digest,
            f"https://huggingface.co/Qwen/Qwen3-0.6B/resolve/{MODEL_REVISION}/{name}",
        ), offline)
        copy(source, target / name)
    write_origin(target, {
        "model": "Qwen/Qwen3-0.6B", "revision": MODEL_REVISION,
        "selection": "config.json, generation_config.json, model.safetensors, tokenizer.json, tokenizer_config.json",
        "source": "https://huggingface.co/Qwen/Qwen3-0.6B", "sha256": MODEL_FILES,
    })


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve(strict=False)
    cache = args.cache.resolve(strict=False)
    if output.exists():
        raise SystemExit(f"refusing existing output path: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".ryeos-local-worker-", dir=output.parent))
    scratch = Path(tempfile.mkdtemp(prefix="ryeos-local-worker-build-"))
    try:
        build_runtime(staging, cache, scratch, args.offline)
        build_tinygrad(staging, cache, scratch, args.offline)
        build_toolchain(staging, cache, scratch, args.offline)
        build_model(staging, cache, args.offline)
        os.replace(staging, output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print(json.dumps({
        "assembly_root": str(output),
        "next": (
            "import each component and compare its returned manifest_hash with "
            "config:ryeos-runtime/local-tinygrad-activation"
        ),
    }, indent=2))


if __name__ == "__main__":
    main()
