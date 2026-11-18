#!/usr/bin/env python3
# ryeos:signed:2026-11-18T05:02:16Z:4d3287f3a1e2bf25ec48fbaf321a81c2d6a5bef8cafb3533a8709695c87f68fc:kyIdmr+LxdftSXKjawGuFI0o80F1/ZvFLf5zQdXM6W2Ayk+09AWJAUFqX/G4ynqbpAn1k7klPiQTx7PBIDRgBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Fail when the pinned Rune renderer declares a JavaScript snippet that rune-snippets.toml
# does not list, or the list names one Rune no longer declares. The set comes from Rune's
# `#[wasm_bindgen(module = "...")]` declarations in the source cargo resolved for the lock,
# so it needs no wasm build; inline snippets cannot be flattened and are refused outright.
# Needs the dependencies fetched (cargo metadata --locked). The renderer is its own Cargo
# workspace (crates/clients/rune), so this reads that workspace's lock.
import json, pathlib, re, subprocess, tomllib

here = pathlib.Path(__file__).resolve().parent
root = here.parents[5]
listed = set(tomllib.loads((here / "rune-snippets.toml").read_text())["snippets"])

meta = json.loads(subprocess.run(
    ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", "wasm32-unknown-unknown",
     "--manifest-path", "crates/clients/rune/Cargo.toml"],
    cwd=root, check=True, capture_output=True, text=True).stdout)
rune = [p for p in meta["packages"] if p["name"] == "rune"]
if len(rune) != 1:
    raise SystemExit(f"expected exactly one rune package in the lock, found {len(rune)}")
crate = pathlib.Path(rune[0]["manifest_path"]).parent

declared = set()
for source in sorted((crate / "src").rglob("*.rs")):
    text = source.read_text()
    if re.search(r"\binline_js\b", text):
        raise SystemExit(f"{source}: inline_js snippets cannot be flattened into the closed asset set")
    for module in re.findall(r"""wasm_bindgen\(\s*module\s*=\s*"([^"]+)"\s*\)""", text):
        declared.add(module.lstrip("/"))

if declared != listed:
    raise SystemExit(
        "Rune's JS snippets differ from rune-snippets.toml: "
        f"undeclared={sorted(declared - listed)} stale={sorted(listed - declared)}"
    )
print(f"rune snippets match: {len(listed)}")
