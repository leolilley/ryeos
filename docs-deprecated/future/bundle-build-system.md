```yaml
id: bundle-build-system
title: "Bundle Build System — From Operator Tool to Provenance-Capturing Build Protocol"
description: RYE already has a content-addressed binary store, signed manifests, hermetic execution, and a rebuild-manifest tool. This doc describes how those primitives converge into a general-purpose build system where the build itself is a signed execution captured in the chain.
category: future
tags: [build, bundle, manifest, cas, provenance, protocols, cross-platform]
version: "0.1.0"
status: exploratory
```

# Bundle Build System

> **Status:** Exploratory. The primitives described in §Current State are
> in production. The convergence path in §Target State is the design
> direction, not a scheduled implementation.

---

## The Insight

RYE already has every primitive a build system needs. They were built to
serve the daemon's execution pipeline, not to be a general-purpose build
tool. But the gap between "build system for the daemon's own tools" and
"build system for arbitrary signed artifacts" is smaller than it looks.

This doc maps what exists, what's missing, and what the architecture
looks like when the build itself becomes a signed execution captured in
the chain.

---

## Current State

### Content-addressed binary store

`ryeos-bundles/core/.ai/bin/<triple>/` holds compiled Rust binaries.
Each binary has:

- A **blob hash** — SHA-256 of the binary content, stored in CAS
  (`lillux::cas::CasStore::store_blob`).
- An **item source** — a signed JSON sidecar (`.item_source.json`)
  containing `content_blob_hash`, `integrity` (same hash with
  `sha256:` prefix), `item_ref`, `kind`, `mode` (unix permissions), and
  `signature_info` (signer fingerprint).
- A **signed envelope** — the sidecar body is signed with Ed25519 via
  `lillux::signature::sign_content`, producing a `rye:signed:` header
  line with timestamp, hash, base64 signature, and fingerprint.

The current binaries in the core bundle:

```
ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/
  rye-inspect
  rye-inspect.item_source.json
  rye-parser-yaml-document
  rye-parser-yaml-document.item_source.json
  rye-parser-yaml-header-document
  rye-parser-yaml-header-document.item_source.json
  rye-parser-regex-kv
  rye-parser-regex-kv.item_source.json
  rye-composer-extends-chain
  rye-composer-extends-chain.item_source.json
  rye-composer-graph-permissions
  rye-composer-graph-permissions.item_source.json
  rye-composer-identity
  rye-composer-identity.item_source.json
  MANIFEST.json
```

### Per-triple manifest

Each triple directory has a `MANIFEST.json` mapping binary names to:

| Field | Meaning |
|---|---|
| `blob_hash` | SHA-256 of the binary content in CAS |
| `content_blob_hash` | Same (binary IS the content) |
| `item_source_hash` | SHA-256 of the item source JSON in CAS |
| `manifest_hash` | SHA-256 of the top-level `SourceManifest` |
| `source_checksum` | SHA-256 of the binary (verification copy) |

The top-level manifest is a `SourceManifest` struct
(`ryeos-state::objects::SourceManifest`) mapping every `item_ref` to its
`item_source_hash`. It's stored in CAS, and its hex hash is written to
`.ai/refs/bundles/manifest`.

### The rebuild-manifest tool

`rye-bundle-tool rebuild-manifest` (implemented in
`ryeos-tools/src/actions/build_bundle.rs`) does the full build-to-manifest
cycle:

1. Walk `.ai/bin/<triple>/` for every triple directory
2. Sort triples and binaries for determinism
3. For each binary: read bytes → `cas.store_blob()` → SHA-256 blob hash
4. Build an `ItemSource` struct per binary (item_ref, content blob hash,
   integrity, signature info, unix mode)
5. Store each `ItemSource` as a CAS object → item source hash
6. Sign each item source sidecar with Ed25519 → `.item_source.json`
7. Aggregate all entries into a `SourceManifest`
8. Store manifest in CAS → manifest hash
9. Write manifest hash to `.ai/refs/bundles/manifest`
10. Stamp manifest hash into every per-triple `MANIFEST.json`

The signing key is provided explicitly (`--key <pem>` or `--seed <byte>`).
No ambient trust. No daemon involvement. Pure publisher-side operation.

### The publish action

`rye publish <bundle-source>` (in
`ryeos-tools/src/actions/publish.rs`) orchestrates the full publisher
workflow:

1. **Bootstrap-sign** kind schemas and parser tools (chicken-and-egg: kind
   schemas must be signed before the engine can load them, but the engine
   needs them to validate items)
2. **Sign all items** — `sign_bundle_items` walks every signable YAML in
   the bundle, validates metadata anchoring, signs with the author key
3. **Rebuild manifest** — `rebuild_bundle_manifest` captures all binaries
   into CAS with signed sidecars
4. **Emit publisher trust doc** — writes `PUBLISHER_TRUST.toml` with the
   author fingerprint and public key for downstream operators to pin via
   `rye trust pin`

This is already a build pipeline. It just happens to be scoped to RYE's
own artifacts.

### Hermetic execution at dispatch time

When the daemon dispatches a subprocess, the execution is hermetic:

- `env_clear()` — all parent environment variables are removed
- Only protocol-declared `EnvInjection` entries are set on the child
- Reserved env names (`PATH`, `HOME`, `LD_*`, `RUST_*`, etc.) are
  rejected at protocol-descriptor load time
- The child receives only what the vocabulary primitives produce

### Signed verification at load time

Every binary the daemon executes goes through three verification checks
at boot:

1. **Signature** — the item source sidecar's Ed25519 signature verifies
   against the content
2. **Trust store** — the signer's fingerprint is in the operator's trust
   store (`~/.ai/config/keys/trusted/`)
3. **Manifest hash** — the item source hash matches the value in the
   `SourceManifest` referenced by `.ai/refs/bundles/manifest`

If any check fails, the engine doesn't boot. Hard error. No degraded
mode.

---

## What's Missing

The current build system has the store, the signing, the manifest, and
the verification. What it doesn't have:

### 1. Build-as-execution

Right now `cargo build` runs outside RYE, then `rebuild-manifest`
captures the outputs. The build itself is not a signed execution. There's
no chain entry proving who built the binaries, with what source, under
what conditions.

A general-purpose build system needs the build to be a RYE execution —
a tool running under a protocol that takes source refs as inputs, runs
the compiler, puts outputs in CAS, and records the whole thing in the
chain.

### 2. Build input declaration

`rebuild-manifest` discovers what's in the bundle directory. A
general-purpose build declares inputs explicitly — "build tool X from
source at CAS hash H1 with dependencies at hashes H2, H3." This is a
YAML manifest, same shape as existing bundle manifests.

### 3. Multi-step build DAG

Currently one step: compile everything, then manifest. A general system
needs topological ordering: build dependency A → CAS → build dependency B
using A → CAS → etc. Each step is a chain entry. The chain already
supports ordering (hash links), so the primitive exists.

### 4. Dependency resolution at scale

Resolving a transitive dependency graph from "I need tool X" to "here
are all the CAS hashes X depends on, in order." The CAS handles
verification. The resolution algorithm is a separate problem.

### 5. CAS garbage collection

The CAS is append-only. `rebuild-manifest` adds objects but never removes
them. At scale, unreachable objects accumulate. See `docs/future/GC-ADVANCED-PATH.md`
for the GC design space.

### 6. Cross-triple build and distribution

The store is triple-aware (`x86_64-unknown-linux-gnu/`). The manifest is
per-triple. What's missing is: CI that builds for multiple triples, a
distribution mechanism for multi-triple bundles, and a sync protocol
that fetches only the triple you need.

---

## Target State

### Build protocol

A new protocol descriptor — `build_v1` — that describes a build
subprocess:

```yaml
kind: protocol
name: build_v1
category: rye/core
abi_version: v1
description: Hermetic build subprocess. Takes source and dependency refs, produces output artifacts in CAS.
stdin:
  shape: build_envelope_v1
stdout:
  shape: build_result_v1
  mode: terminal
env_injections:
  - { name: RYE_CAS_ROOT, source: cas_root }
  - { name: RYE_THREAD_ID, source: thread_id }
  - { name: RYE_PROJECT_PATH, source: project_path }
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle:
  mode: oneshot
callback_channel: none
```

This requires two new vocabulary primitives:

- `build_envelope_v1` — a `StdinShape` variant carrying source refs,
  dependency refs, target triple, and build configuration
- `build_result_v1` — a `StdoutShape` variant carrying output CAS hashes,
  build metadata (compiler version, flags, duration), and success/failure

Adding vocabulary primitives is a daemon code change by design (the
vocabulary IS the syscall set). The protocol descriptor that composes
them is signed YAML data.

### Build envelope

The build envelope carries everything the build tool needs:

```yaml
source:
  ref: "tool:my-org/llm-tool"
  cas_hash: "a1b2c3..."
dependencies:
  - ref: "bin/x86_64-unknown-linux-gnu/lib-foo"
    cas_hash: "d4e5f6..."
  - ref: "bin/x86_64-unknown-linux-gnu/lib-bar"
    cas_hash: "g7h8i9..."
target:
  triple: "x86_64-unknown-linux-gnu"
  output_path: ".ai/bin/x86_64-unknown-linux-gnu/my-tool"
build_config:
  compiler: "rustc"
  profile: "release"
  features: ["streaming"]
```

The daemon constructs this from a build manifest, serializes to JSON,
writes to the build tool's stdin. The build tool reads it, compiles,
writes outputs to CAS, and returns the result.

### Build result

```yaml
success: true
outputs:
  - item_ref: "bin/x86_64-unknown-linux-gnu/my-tool"
    blob_hash: "j0k1l2..."
    mode: 493
  - item_ref: "bin/x86_64-unknown-linux-gnu/my-tool-debug"
    blob_hash: "m3n4o5..."
    mode: 493
build_metadata:
  compiler_version: "rustc 1.85.0"
  duration_secs: 47
  source_hash: "a1b2c3..."
  dependency_hashes: ["d4e5f6...", "g7h8i9..."]
```

The daemon verifies the output hashes match what was actually written to
CAS, then records the whole thing in the chain.

### Build manifest

A YAML file declaring what to build and where to find inputs:

```yaml
kind: build_manifest
name: core-bundle
version: "1.0.0"
targets:
  - tool: rye-build-rust
    protocol: protocol:rye/core/build_v1
    sources:
      - ref: "src/ryeos-handler-bins"
        cas_hash: "..."
    dependencies:
      - ref: "bin/x86_64-unknown-linux-gnu/rye-parser-yaml-document"
        cas_hash: "..."
    outputs:
      - item_ref: "bin/x86_64-unknown-linux-gnu/rye-inspect"
      - item_ref: "bin/x86_64-unknown-linux-gnu/rye-parser-yaml-document"
    config:
      profile: release
      features: []
```

This is the declarative input that `rebuild-manifest` currently lacks.
It's also signed YAML, verified against the trust store.

### Sandbox integration

Build tools run under `sandbox_wrap` with a build-specific profile:

```yaml
metadata:
  sandbox:
    engine: "nsjail"
    profile: "build-hermetic"
    requires:
      capabilities: ["cgroups_v2", "seccomp"]
```

The build-hermetic profile:

- No network access (prevents dependency smuggling)
- Read-only mount of dependency CAS objects
- Read-write mount of output CAS directory only
- Resource limits (memory, CPU, time)

This makes the build hermetic by enforcement, not by convention. The
sandbox config is data. The attestation proves the sandbox was used. The
chain proves what went in and what came out.

### The chain entry for a build

When a build completes, the chain records:

```
chain entry:
  principal: fp:<signer-fingerprint>
  tool: tool:rye/core/build-rust
  protocol: protocol:rye/core/build_v1
  input:
    source_hash: "a1b2c3..."
    dependency_hashes: ["d4e5f6...", "g7h8i9..."]
    build_config: { compiler: "rustc", profile: "release" }
    sandbox: { engine: "nsjail", profile: "build-hermetic" }
  output:
    blob_hash: "j0k1l2..."
    build_metadata: { compiler_version: "rustc 1.85.0", duration_secs: 47 }
  timestamp: "2026-05-01T12:00:00Z"
  previous: "<hash-of-previous-chain-entry>"
```

Anyone with access to the chain can verify: who built this, from what
source, with what dependencies, in what sandbox, producing what output.
The blob hash in the output either matches the binary in the CAS or it
doesn't. The source hash either matches the source in the CAS or it
doesn't. No trust required beyond the signer's key.

---

## How This Composes With What Exists

### The build replaces the manual compile step

Current flow:

```
cargo build (operator runs manually)
  → rye-bundle-tool rebuild-manifest (captures outputs)
    → rye publish (signs everything, emits trust doc)
```

Target flow:

```
rye execute build --manifest build-manifest.yaml
  → daemon spawns build tool under build_v1 protocol
    → sandbox_wrap (hermetic, no network)
      → build tool compiles, writes to CAS
        → chain records the build
          → manifest auto-updated from build result
```

`rebuild-manifest` becomes an internal step inside the build protocol,
not a separate operator workflow. The operator provides the build
manifest; the daemon does the rest.

### The bundle structure doesn't change

```
.ai/
  bin/<triple>/
    <binary>                    (same)
    <binary>.item_source.json   (same)
    MANIFEST.json               (same)
  objects/                      (CAS — same)
  refs/bundles/manifest         (same)
```

What changes is HOW the binaries get there: compiled by a signed build
tool running under a verified protocol, captured in the chain, rather
than compiled externally and ingested by `rebuild-manifest`.

### The trust model doesn't change

- The binary is signed by the publisher.
- The signer must be in the operator's trust store.
- The manifest hash must match.
- The chain proves provenance.

The build protocol adds a new layer: you can now also verify HOW the
binary was produced, not just that it was signed. The chain entry links
the output hash to the source hash, the dependency hashes, the sandbox
config, and the build tool version.

### Cross-platform is structural

The bundle is already triple-aware. The build protocol carries a target
triple. The build manifest declares which triples to build for. CI (or
an operator) runs `rye execute build` once per triple. Each build's
output lands in the correct `bin/<triple>/` directory. The manifest
aggregates all triples.

A multi-triple bundle looks like:

```
.ai/
  bin/
    x86_64-unknown-linux-gnu/
    aarch64-unknown-linux-gnu/
    x86_64-apple-darwin/
  objects/              (shared CAS — content-addressed, triple-independent)
  refs/bundles/manifest (references all triples)
```

Different machines sync the same CAS objects. The binary directory they
use depends on their triple. The manifest hash is the same everywhere.

---

## The Full Build Lifecycle

### 1. Author writes a build manifest

```yaml
kind: build_manifest
name: my-tool
sources:
  - ref: "tool:my-org/tool"
    cas_hash: "<source-hash>"
dependencies:
  - ref: "bin/x86_64-unknown-linux-gnu/some-lib"
    cas_hash: "<dep-hash>"
targets:
  - triple: x86_64-unknown-linux-gnu
    outputs:
      - "bin/x86_64-unknown-linux-gnu/my-tool"
```

Signs it. Puts it in the bundle.

### 2. Operator triggers the build

```
rye execute build --manifest build-manifest.yaml
```

The daemon:

1. Resolves the `build_v1` protocol
2. Constructs the build envelope from the manifest
3. Resolves the build tool binary from CAS (verified, signed)
4. Applies `sandbox_wrap` with the build-hermetic profile
5. Spawns the build tool
6. Build tool reads envelope from stdin, compiles, writes outputs to CAS
7. Build tool writes `build_result_v1` to stdout
8. Daemon verifies output hashes against CAS
9. Daemon records the build in the chain
10. Manifest is updated with new blob hashes and item sources

### 3. Downstream operator verifies

The downstream operator receives the bundle. They can:

- Verify the manifest hash matches `.ai/refs/bundles/manifest`
- Verify each binary's item source signature against their trust store
- Walk the chain to find the build entry for each binary
- Verify the build entry's source hash matches the source in CAS
- Verify the build entry's sandbox config matches their policy
- Verify the build entry's dependency hashes match what's in CAS
- Verify the build tool's signer is trusted

At no point does the downstream operator need to trust the build
environment. They trust the signer and verify the hashes. The chain
proves the rest.

---

## Relationship to Existing Docs

| Doc | Relationship |
|---|---|
| `node-sandboxed-execution.md` | Sandbox engines and node attestation provide the runtime enforcement layer for hermetic builds. The `sandbox_wrap` seam built in the protocols-as-data wave is the same seam the build protocol uses. |
| `cross-platform-support.md` | Cross-platform at the daemon level. The build system builds ON TOP of a running daemon. Triple-aware bundles are the distribution mechanism. |
| `GC-ADVANCED-PATH.md` | GC becomes more important when builds produce intermediate artifacts. Build DAGs may leave unreachable CAS objects. |
| `cluster-and-federation.md` | Multi-node build distribution — a build manifest could target different nodes with different capabilities (e.g., build ARM64 on an ARM node, x86 on an x86 node). |
| Protocols-as-data wave (`implementation-2-protocols-as-data/`) | Establishes the vocabulary primitive mechanism that `build_v1` would use. Adding `build_envelope_v1` and `build_result_v1` as vocabulary primitives follows the same pattern as the existing three protocols. |

---

## Implementation Triggers

Don't build this until one of these is true:

| Trigger | Why |
|---|---|
| A third party wants to publish a signed bundle with their own tools | They need a build pipeline that produces verified artifacts |
| The core bundle grows beyond Rust binaries (Python wheels, WASM modules, data files) | Different build tools need a general protocol, not just `cargo build` |
| Reproducible verification becomes an operator requirement | "Prove this binary was built from this source" needs the chain entry |
| Multi-triple distribution becomes a production need | Automated per-triple builds replace manual cross-compilation |
| CI needs to produce verified builds without human involvement | The chain replaces manual signing workflows |

If none of these are true, `rebuild-manifest` + `publish` is sufficient.
Don't generalize ahead of demand.

---

## What "Everything Is Data" Means For Builds

The build protocol is data. The build manifest is data. The sandbox
config is data. The build result is data. All of it is signed. All of it
is verified. All of it is in the chain.

The daemon doesn't know what "a build" is. It knows:

1. Resolve a protocol descriptor
2. Build a subprocess spec from the vocabulary
3. Optionally wrap with a sandbox
4. Spawn, capture stdout, record in chain

The vocabulary primitives for `build_v1` are two new variants:
`build_envelope_v1` and `build_result_v1`. Everything else composes from
existing primitives. The protocol descriptor is a YAML file. The build
tool is a signed binary in CAS.

The build system is not a separate thing from the execution system. It's
the execution system applied to a specific protocol. The same daemon that
runs tools and directives runs builds. The same chain that records tool
executions records builds. The same CAS that stores tool outputs stores
build outputs. The same trust store that verifies tool signers verifies
build outputs.

The build IS the execution. The execution IS the provenance. The
provenance IS the chain.
