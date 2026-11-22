#!/usr/bin/env python3
# ryeos:signed:2026-11-20T02:05:53Z:7cacc0e01c3a5d56a9444b06070d288baeb12acf7ea5d9b6f0bc0b478941fe7e:JOhKTlPwSlOcp1bn+O3nCJS5z+/TFeCbGC80xsZGCTc5qQLaHe4TbxaBntojbhmlMnFxJSuFUJoq8RAd+RAGCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Fail when the pinned Rune renderer declares a JavaScript snippet that rune-snippets.toml
# lists in neither `snippets` nor `unlinked`, or the lists name one Rune no longer declares.
# Rune is several packages; the set comes from every Rune package in the pinned checkout (the
# packages beside `rune` in the lock) and their `#[wasm_bindgen(module = "...")]`
# declarations, in the source cargo resolved for the lock, so it needs no wasm build; inline
# snippets cannot be flattened and are refused outright. Whether a declared module is linked
# is the build's check (wasm-bindgen emits only linked ones); `unlinked` records the declared
# ones the renderer does not link.
#
# Fail likewise when the files Rune fetches at runtime differ from the `fetched` list. A page
# serves only its closed asset set, so a file Rune fetches and the package lacks is a 404 in
# every browser. Rune's snippets call the browser's `fetch` in `fetchImage` (image URLs an app
# names) and `fetchBytes` (`fetch_bytes` in Rust). This check knows each `fetch_bytes` call
# site: the full Inter faces (the names in `FILES` of rune-text's `fonts::full_faces`, fetched
# when rune-text builds with `embedded-fonts`), and fonts and spelling dictionaries that only
# an app that asks for them fetches (RyeOS asks for none). A `fetch` call or a `fetch_bytes`
# call site this check does not know fails it, so a pin bump that adds a runtime fetch is
# looked at before it ships. What an app itself asks Rune to fetch is outside this check; the
# browser smoke, which fails on any 404, covers that.
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
listed = {(e["crate"], e["path"]) for e in declared_list["snippets"]}
unlinked = {(e["crate"], e["path"]) for e in declared_list["unlinked"]}
fetched = [(e["crate"], e["path"]) for e in declared_list["fetched"]]
if listed & unlinked:
    raise SystemExit(f"rune-snippets.toml lists as both linked and unlinked: {sorted(listed & unlinked)}")
flat = [f"{c.replace('-', '_')}_{pathlib.PurePosixPath(p).stem}.js" for c, p in sorted(listed)]
if len(set(flat)) != len(flat):
    raise SystemExit(f"two listed snippets flatten to one file name: {flat}")

meta = json.loads(subprocess.run(
    [os.environ.get("CARGO", "cargo"), "metadata", "--locked", "--format-version", "1",
     "--filter-platform", "wasm32-unknown-unknown", "--manifest-path", "crates/clients/rune/Cargo.toml"],
    cwd=root, check=True, capture_output=True, text=True).stdout)
by_name = {p["name"]: p for p in meta["packages"]}
if "rune" not in by_name:
    raise SystemExit("no rune package in the lock")
# Every Rune package is a sibling of `rune` in the pinned checkout's crates directory.
crates_dir = pathlib.Path(by_name["rune"]["manifest_path"]).parent.parent
crates = {
    p["name"]: pathlib.Path(p["manifest_path"]).parent
    for p in meta["packages"]
    if pathlib.Path(p["manifest_path"]).parent.parent == crates_dir
}
def features_of(name):
    return next(n["features"] for n in meta["resolve"]["nodes"] if n["id"] == by_name[name]["id"])

# Every `fetch_bytes` call site in Rune's source, by crate and file, against the known ones.
KNOWN_FETCH_SITES = {
    # An app's `font_url` fonts; RyeOS names none.
    ("rune", "src/platform/web.rs"): 1,
    # The full Inter faces and an app's `font_url_later` fonts; RyeOS names none of the latter.
    ("rune", "src/platform/web/full_faces.rs"): 1,
    # Spelling dictionaries, for text an app marks for spell checking; RyeOS marks none.
    ("rune-spell", "src/web.rs"): 2,
}
sites = {}
for name, crate in sorted(crates.items()):
    for source in sorted((crate / "src").rglob("*.rs")):
        calls = len(re.findall(r"(?<!fn )\bfetch_bytes\(", source.read_text()))
        if calls:
            sites[(name, source.relative_to(crate).as_posix())] = calls
if sites != KNOWN_FETCH_SITES:
    raise SystemExit(
        "Rune's runtime fetch sites changed; read each, update rune-snippets.toml `fetched` "
        f"and this check: found={sorted(sites.items())} known={sorted(KNOWN_FETCH_SITES.items())}"
    )
# Every call of the browser's `fetch` in Rune's snippets, linked or not.
KNOWN_SNIPPET_FETCHES = {
    ("rune", "src/platform/web/platform.js"): 1,
    ("rune-image", "src/from_url.js"): 1,
    ("rune-spell", "src/web.js"): 1,
}
snippet_fetches = {}
for name, path in sorted(listed | unlinked):
    calls = len(re.findall(r"(?<![\w.])fetch\(", (crates[name] / path).read_text()))
    if calls:
        snippet_fetches[(name, path)] = calls
if snippet_fetches != KNOWN_SNIPPET_FETCHES:
    raise SystemExit(
        "Rune's snippets fetch in places this check does not know; read each and update this "
        f"check: found={sorted(snippet_fetches.items())} known={sorted(KNOWN_SNIPPET_FETCHES.items())}"
    )
faces = re.search(
    r"pub const FILES: &\[&str\] = &\[([^\]]*)\];",
    (crates["rune-text"] / "src/fonts/full_faces.rs").read_text())
if not faces:
    raise SystemExit("Rune's rune-text fonts::full_faces::FILES was not found; its runtime font fetch changed")
needed = sorted(re.findall(r'"([^"]+)"', faces.group(1))) if "embedded-fonts" in features_of("rune-text") else []
names = sorted(pathlib.PurePosixPath(path).name for _, path in fetched)
if names != needed or len(set(names)) != len(names):
    raise SystemExit(
        "the files Rune fetches at runtime differ from rune-snippets.toml `fetched`: "
        f"fetches={needed} listed={names}"
    )
missing = [(c, p) for c, p in fetched if c not in crates or not (crates[c] / p).is_file()]
if missing:
    raise SystemExit(f"rune-snippets.toml `fetched` names files the pinned Rune lacks: {missing}")
if sys.argv[1:] == ["--fetched-paths"]:
    print("\n".join(str(crates[c] / p) for c, p in fetched))
    raise SystemExit(0)

declared = set()
for name, crate in sorted(crates.items()):
    for source in sorted((crate / "src").rglob("*.rs")):
        text = source.read_text()
        if re.search(r"\binline_js\b", text):
            raise SystemExit(f"{source}: inline_js snippets cannot be flattened into the closed asset set")
        for module in re.findall(r"""wasm_bindgen\(\s*module\s*=\s*"([^"]+)"\s*\)""", text):
            declared.add((name, module.lstrip("/")))

if declared != listed | unlinked:
    raise SystemExit(
        "Rune's JS snippets differ from rune-snippets.toml: "
        f"undeclared={sorted(declared - listed - unlinked)} stale={sorted((listed | unlinked) - declared)}"
    )
print(f"rune snippets match: {len(listed)} linked, {len(unlinked)} unlinked; runtime fetches match: {len(fetched)}")
