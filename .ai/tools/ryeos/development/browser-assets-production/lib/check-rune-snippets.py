#!/usr/bin/env python3
# ryeos:signed:2026-11-18T07:43:25Z:7029f5df1fe89cb82c59a75d1fd156caf31a60780740dbcfbbc7acb8d00ddd39:8PpN3xuuquklbd6vCKvNE3QWYD/i7fqfqvDYpkq0qUkaEV/L0RjMiofeJ3GJqMbnlUP925IS+sjoMzu1YzfgBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Fail when the pinned Rune renderer declares a JavaScript snippet that rune-snippets.toml
# does not list, or the list names one Rune no longer declares. The set comes from Rune's
# `#[wasm_bindgen(module = "...")]` declarations in the source cargo resolved for the lock,
# so it needs no wasm build; inline snippets cannot be flattened and are refused outright.
#
# Fail likewise when the files Rune fetches at runtime differ from the `fetched` list. A page
# serves only its closed asset set, so a file Rune fetches and the package lacks is a 404 in
# every browser. Rune's snippets call the browser's `fetch` in two functions: `fetchImage`,
# for image URLs an app names, and `fetchBytes` (`fetch_bytes` in Rust). This check knows each
# `fetch_bytes` call site: the full Inter faces (the names in `text::full_faces::FILES`,
# fetched when the renderer builds with `embedded-fonts`), and fonts and spelling dictionaries
# that only an app that asks for them fetches (RyeOS asks for none). A `fetch` call or a
# `fetch_bytes` call site this check does not know fails it, so a pin bump that adds a runtime
# fetch is looked at before it ships. What an app itself asks Rune to fetch is outside this
# check; the browser smoke, which fails on any 404, covers that.
#
# --fetched-paths prints the absolute path of each `fetched` file in the pinned Rune source
# after the same checks, for build-rune-web.sh to package.
#
# Needs the dependencies fetched (cargo metadata --locked). The renderer is its own Cargo
# workspace (crates/clients/rune), so this reads that workspace's lock. Environment: CARGO
# (default cargo).
import json, os, pathlib, re, subprocess, sys, tomllib

if sys.argv[1:] not in ([], ["--fetched-paths"]):
    raise SystemExit("usage: check-rune-snippets.py [--fetched-paths]")

here = pathlib.Path(__file__).resolve().parent
root = here.parents[5]
declared_list = tomllib.loads((here / "rune-snippets.toml").read_text())
listed = set(declared_list["snippets"])
fetched = declared_list["fetched"]

meta = json.loads(subprocess.run(
    [os.environ.get("CARGO", "cargo"), "metadata", "--locked", "--format-version", "1",
     "--filter-platform", "wasm32-unknown-unknown", "--manifest-path", "crates/clients/rune/Cargo.toml"],
    cwd=root, check=True, capture_output=True, text=True).stdout)
rune = [p for p in meta["packages"] if p["name"] == "rune"]
if len(rune) != 1:
    raise SystemExit(f"expected exactly one rune package in the lock, found {len(rune)}")
crate = pathlib.Path(rune[0]["manifest_path"]).parent
features = next(n["features"] for n in meta["resolve"]["nodes"] if n["id"] == rune[0]["id"])

# Every `fetch_bytes` call site in Rune's source, by file, against the ones this check knows.
KNOWN_FETCH_SITES = {
    # An app's `font_url` fonts; RyeOS names none.
    "src/platform/web.rs": 1,
    # The full Inter faces and an app's `font_url_later` fonts; RyeOS names none of the latter.
    "src/platform/web/full_faces.rs": 1,
    # Spelling dictionaries, for text an app marks for spell checking; RyeOS marks none.
    "src/spell/web.rs": 2,
}
sites = {}
for source in sorted((crate / "src").rglob("*.rs")):
    calls = len(re.findall(r"(?<!fn )\bfetch_bytes\(", source.read_text()))
    if calls:
        sites[source.relative_to(crate).as_posix()] = calls
if sites != KNOWN_FETCH_SITES:
    raise SystemExit(
        "Rune's runtime fetch sites changed; read each, update rune-snippets.toml `fetched` "
        f"and this check: found={sites} known={KNOWN_FETCH_SITES}"
    )
# Every call of the browser's `fetch` in Rune's snippets: `fetchImage` and `fetchBytes`.
KNOWN_SNIPPET_FETCHES = {"src/platform/web/platform.js": 2}
snippet_fetches = {}
for path in sorted(listed):
    calls = len(re.findall(r"(?<![\w.])fetch\(", (crate / path).read_text()))
    if calls:
        snippet_fetches[path] = calls
if snippet_fetches != KNOWN_SNIPPET_FETCHES:
    raise SystemExit(
        "Rune's snippets fetch in places this check does not know; read each and update this "
        f"check: found={snippet_fetches} known={KNOWN_SNIPPET_FETCHES}"
    )
faces = re.search(
    r"pub\(crate\) const FILES: &\[&str\] = &\[([^\]]*)\];",
    (crate / "src/text/full_faces.rs").read_text())
if not faces:
    raise SystemExit("Rune's text::full_faces::FILES was not found; its runtime font fetch changed")
needed = sorted(re.findall(r'"([^"]+)"', faces.group(1))) if "embedded-fonts" in features else []
names = sorted(pathlib.PurePosixPath(path).name for path in fetched)
if names != needed or len(set(names)) != len(names):
    raise SystemExit(
        "the files Rune fetches at runtime differ from rune-snippets.toml `fetched`: "
        f"fetches={needed} listed={names}"
    )
missing = [path for path in fetched if not (crate / path).is_file()]
if missing:
    raise SystemExit(f"rune-snippets.toml `fetched` names files the pinned Rune lacks: {missing}")
if sys.argv[1:] == ["--fetched-paths"]:
    print("\n".join(str(crate / path) for path in fetched))
    raise SystemExit(0)

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
print(f"rune snippets match: {len(listed)}; runtime fetches match: {len(fetched)}")
