# ryeos:signed:2026-09-20T11:15:48Z:4559c6fc1427a36be5490c0592e9269f63642d33c67a20bde1e0d88e8698a55b:JC9lb1YdoKKG8mI7s1Z8ggpZry2S1TKZBmceZvS+cM0Utjq+s0F5gANdM8z+C6+Uc3gnMs+wrkJG00YRNS01CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/python3
"""Materialize one canonical receipt-only substrate release product."""
import json
import pathlib
import re
import sys

HASH = re.compile(r"[0-9a-f]{64}")
TRIPLE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}")


def fail(message):
    raise SystemExit(message)


def target(value):
    if value == {"kind": "portable"}:
        return
    if (not isinstance(value, dict) or set(value) != {"kind", "triple"}
            or value.get("kind") != "triple" or not isinstance(value.get("triple"), str)
            or not TRIPLE.fullmatch(value["triple"])):
        fail("invalid closed substrate target")


request = json.load(sys.stdin)
if not isinstance(request, dict) or set(request) != {"receipt"}:
    fail("closed substrate build request required")
receipt = request["receipt"]
required = {
    "schema", "kind", "substrate_image_digest", "substrate_protocol",
    "target", "core_generation_hash",
}
if not isinstance(receipt, dict) or set(receipt) != required:
    fail("substrate receipt has an open or incomplete schema")
if receipt["schema"] != "ryeos.substrate_build_receipt.v1" or receipt["kind"] != "substrate_build_receipt":
    fail("substrate receipt contract is not current")
image = receipt["substrate_image_digest"]
if not isinstance(image, str) or not image.startswith("sha256:") or not HASH.fullmatch(image[7:]):
    fail("substrate image digest must be exact sha256")
protocol = receipt["substrate_protocol"]
if isinstance(protocol, bool) or not isinstance(protocol, int) or protocol <= 0 or protocol > 0xFFFFFFFF:
    fail("substrate protocol must be a nonzero u32")
target(receipt["target"])
if not isinstance(receipt["core_generation_hash"], str) or not HASH.fullmatch(receipt["core_generation_hash"]):
    fail("Core generation hash must be lowercase 64-hex")

root = pathlib.Path.cwd() / "products/substrate-release/.ai"
if root.parent.exists():
    fail("substrate product root already exists")
root.mkdir(parents=True, exist_ok=False)
encoded = json.dumps(receipt, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
(root / "substrate-release.json").write_bytes(encoded)
json.dump({
    "schema": "ryeos.substrate_release_build.v1",
    "product_name": "substrate_release",
    "output_root": "substrate_release",
    "receipt": receipt,
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
