#!/usr/bin/env python3
"""Regression cases for check-workspace-versions.py. Builds nothing."""
import importlib.util
import os
import tempfile

here = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("cwv", os.path.join(here, "check-workspace-versions.py"))
cwv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cwv)


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def crate(root, rel, name, version_line):
    write(os.path.join(root, rel, "Cargo.toml"), f'[package]\nname = "{name}"\n{version_line}\n')


with tempfile.TemporaryDirectory() as root:
    write(os.path.join(root, "Cargo.toml"),
          '[workspace]\nmembers = ["crates/a", "crates/b", "crates/g/*"]\n'
          'exclude = ["crates/g/skip"]\n')
    crate(root, "crates/a", "a", 'version = "0.5.9"')
    crate(root, "crates/b", "b", 'version = "0.1.0"')
    crate(root, "crates/g/one", "one", 'version = "0.2.0"')
    crate(root, "crates/g/skip", "skip", 'version = "0.9.9"')
    write(os.path.join(root, "crates/a/pyproject.toml"), '[project]\nname = "a"\nversion = "0.5.9"\n')
    listed = ["crates/a/Cargo.toml", "crates/a/pyproject.toml"]

    assert cwv.check(root, "0.5.9", listed) == []

    # release moved but listed manifests did not
    assert len(cwv.check(root, "0.5.10", listed)) == 2

    # listed manifest missing
    assert len(cwv.check(root, "0.5.9", listed + ["crates/zz/Cargo.toml"])) == 1

    # unlisted crate bumped into the release grammar is caught
    crate(root, "crates/g/two", "two", 'version = "0.5.9"')
    bad = cwv.check(root, "0.5.9", listed)
    assert [b[0] for b in bad] == [os.path.join("crates/g/two", "Cargo.toml")], bad
    crate(root, "crates/g/two", "two", 'version = "0.6.0-rc.1"')
    assert len(cwv.check(root, "0.5.9", listed)) == 1
    # older-grammar versions (X < 5) and 1.x are not release versions
    crate(root, "crates/g/two", "two", 'version = "0.4.9"')
    assert cwv.check(root, "0.5.9", listed) == []

    mf = os.path.join(root, "list.txt")
    write(mf, "# list\ncrates/a/Cargo.toml\ncrates/a/pyproject.toml\n")
    assert cwv.read_manifests(mf) == listed
    assert cwv.main(["0.5.9", "--root", root, "--manifests", mf]) == 0
    assert cwv.main(["0.5.10", "--root", root, "--manifests", mf]) == 1

print("workspace version check cases passed")
