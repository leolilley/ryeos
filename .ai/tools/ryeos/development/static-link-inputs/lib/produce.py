# ryeos:signed:2026-09-22T04:12:04Z:b411de8ceaeab07c3859f9c37ca4dfc3abf26a8eb8383c9edcc61732f37caecd:2Re1vm5biEh42ZUcejP9/3JtOPXD99/r07qRMN6eSN+VQce0bT0qfBy2MZBM06O04n+6hAj8GMrNri8cy+SBCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Offline exact static-input reproduction; no acquisition or authority grant."""
import hashlib
import os
from pathlib import Path
import shutil
import stat


def verify_tree(root, contract):
    root = Path(root)
    if not stat.S_ISDIR(root.lstat().st_mode) or root.is_symlink():
        raise ValueError("static-input root is not an ordinary directory")
    expected = contract["inputs"]
    directories = {"."}
    for name in expected:
        directories.update(str(parent) for parent in Path(name).parents)
    seen = set()
    total = 0
    for current, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            path = Path(current) / name
            relative = path.relative_to(root).as_posix()
            info = path.lstat()
            if stat.S_ISDIR(info.st_mode):
                if relative not in directories:
                    raise ValueError("unexpected static-input directory")
                continue
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or relative not in expected:
                raise ValueError("unexpected, linked or special static-input member")
            item = expected[relative]
            if info.st_size != item["bytes"] or stat.S_IMODE(info.st_mode) != item["mode"]:
                raise ValueError("static-input size or mode mismatch")
            with path.open("rb") as stream:
                digest = hashlib.file_digest(stream, "sha256").hexdigest()
            if digest != item["sha256"]:
                raise ValueError("static-input digest mismatch")
            total += info.st_size
            seen.add(relative)
    if seen != set(expected):
        raise ValueError("static-input tree is incomplete")
    return {"file_count": len(seen), "total_file_bytes": total,
            "target": contract["target"], "publisher_image": contract["publisher_image"]}


def produce(source, destination, contract):
    """Called only with an admitted read-only source and a private output root."""
    evidence = verify_tree(source, contract)
    destination = Path(destination)
    if destination.exists() or destination.is_symlink():
        raise ValueError("static-input output already exists")
    # copytree refuses existing destinations. Source is the admitted immutable
    # realization, not the mutable external transport directory in live use.
    shutil.copytree(source, destination, symlinks=False)
    if verify_tree(destination, contract) != evidence:
        raise ValueError("static-input output differs from verified source")
    return evidence
