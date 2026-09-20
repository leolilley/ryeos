#!/usr/bin/env python3
"""Static protocol lockstep checks for the constrained publisher boundary."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SERVER = (ROOT / "crates/daemon/ryeos-api/src/publisher_server.rs").read_text()
CLIENT = (ROOT / "crates/daemon/ryeos-app/src/bundle_publication/producer.rs").read_text()
DOC = (ROOT / "deploy/constrained-bundle-publisher.md").read_text()
COMMAND = (ROOT / "crates/bin/daemon/src/bin/ryeos-bundle-publisher.rs").read_text()

PATHS = (
    "v1/bundle-recipe/authorize-build",
    "v1/bundle-recipe/authorize-capture",
    "v1/bundle-tree/sign",
    "v1/bundle-generation/authorize",
    "v1/bundle-catalog/authorize-successor",
)

for path in PATHS:
    assert f'"{path}"' in CLIENT, f"client path missing: {path}"
    assert f'"/{path}"' in SERVER, f"server path missing: {path}"
    assert f"`POST /{path}`" in DOC, f"deployment contract missing: {path}"

assert SERVER.count(".route(") == 5, "publisher surface gained an unreviewed route"
assert "DefaultBodyLimit::max(MAX_BODY_BYTES)" in SERVER
assert "operation.validate()" in SERVER
assert "ct_eq" in SERVER
assert "generic" in SERVER.lower() and "signing route" in SERVER.lower()
assert "secret-mounted path" in DOC
assert "loopback" in DOC and "HTTPS" in DOC
for option in ("publisher_key", "bearer_file", "cas_root", "policy"):
    assert option in COMMAND
assert "is_loopback" in COMMAND
assert "LocalConstrainedPublisherAuthority::new_with_cas" in COMMAND
assert "axum::serve" in COMMAND

print("constrained publisher surface: 5/5 exact operations")
