# Distribution & Deployment — The Ideal State

**Status:** future-state design  
**Audience:** Rye OS maintainers and operators  
**Companion docs:** `bundle-build-system.md`, `signing-trust-advanced.md`, `key-rotation-and-trust-policy.md`, `cluster-bootstrap.md`, `real-install-smoke.md`

---

## 1. Problem Statement

A Rye node is not one binary. It is a **graph** of artifacts:

- `ryeosd` (the daemon)
- 3 runtime drivers (`ryeos-{directive,graph,knowledge}-runtime`)
- 9+ handler bins (`rye-parser-*`, `rye-composer-*`, `rye-tool-streaming-demo`)
- 2 CLI tools (`rye-inspect`, `rye-sign`) + the `rye` CLI
- The **core bundle** — `.ai/` tree of handlers, parsers, protocols, tools,
  routes, services, plus its CAS (`objects/`, `refs/`)
- The **standard bundle** — runtime YAML, model routing, provider configs, plus
  its CAS
- One or more **project bundles** — the actual application content (directives,
  knowledge, tools)
- A **trust store** — fingerprints of every signer used for any of the above
- A **node identity** — Ed25519 keypair, X25519 vault keypair, audience id
- **Mutable state** — runtime SQLite, trace events, per-thread state

Today these are assembled by hand at deploy time. The Python predecessor solved
distribution with a single `pip install ryeos-node==0.1.10` because _its_ node
was a single Python package and the bundles were generated at first boot. Rust
ryeosd cannot collapse to one artifact: signing, CAS, multi-binary materialization,
and offline-verifiable trust are core invariants.

We need a distribution model that:

1. Produces **versioned, hash-pinned, signed** artifacts for every layer.
2. Lets a deployer fetch them by **declarative reference**, not by cloning the
   build repo.
3. Supports **independent versioning** — the daemon can ship 0.3.x while the
   standard bundle stays at 0.2.x; project bundles ship on their own cadence.
4. Composes cleanly across **deployment surfaces**: Railway, Fly, k8s, bare
   metal, edge.
5. Enables **runtime install** (`bundle.install <ref>`) so that node operators
   add capability without rebuilding images.
6. Preserves **end-to-end signature verification** from the build host to the
   running daemon.

This document specifies the target end-state and a three-tier path to reach it.

---

## 2. The Ideal Architecture

```diagram
                 ╭──────────────────────────────────────────╮
                 │   Build / Release  (ryeos-cas-as-truth)  │
                 │                                          │
                 │   GitHub Actions on tag push:            │
                 │   • cargo build --release (all bins)     │
                 │   • install bins → bundle bin/ trees     │
                 │   • rebuild-manifest with release key    │
                 │   • cosign sign (Ed25519, key in OIDC)   │
                 │   • publish 4 OCI artifacts to GHCR      │
                 ╰─────────────────────┬────────────────────╯
                                       │
              ┌────────────┬───────────┼────────────┬──────────────┐
              ▼            ▼           ▼            ▼              ▼
   ╭──────────────────╮ ╭───────╮ ╭──────────╮ ╭──────────╮ ╭──────────────╮
   │ ryeosd:vX.Y.Z    │ │bundle │ │ bundle   │ │ project  │ │ trust-bundle │
   │ container image  │ │ core  │ │ standard │ │ bundle   │ │ (signers)    │
   │ ~25 MB           │ │OCI    │ │ OCI      │ │ OCI      │ │ OCI          │
   ╰────────┬─────────╯ ╰───┬───╯ ╰─────┬────╯ ╰─────┬────╯ ╰──────┬───────╯
            │               │           │            │             │
            └────────┬──────┴───────────┴────────────┴─────────────┘
                     │
                     ▼
       ╭─────────────────────────────────────────────────────╮
       │   Deploy host  (Railway / Fly / k8s / bare metal)    │
       │                                                     │
       │   FROM ghcr.io/rye/ryeosd:vX.Y.Z                     │
       │   ENV RYE_BUNDLES="                                  │
       │     oci://ghcr.io/rye/bundle-core:vA                  │
       │     oci://ghcr.io/rye/bundle-standard:vB              │
       │     oci://ghcr.io/me/tv-tracker-bundle:vC             │
       │   "                                                 │
       │                                                     │
       │   Entrypoint:                                       │
       │     1. ryeosd --init-if-missing                     │
       │     2. for each $RYE_BUNDLES: bundle install         │
       │        (verifies signature against trust-bundle)    │
       │     3. ryeosd --bind 0.0.0.0:$PORT                  │
       ╰─────────────────────────────────────────────────────╯
```

Five artifact kinds, three publication surfaces, one declarative deploy.

---

## 3. The Five Artifact Kinds

### 3.1 Daemon image (`ryeosd:vX.Y.Z`)

**Container image** with only the daemon binary plus the trust store seed.

```
/usr/local/bin/ryeosd            ← ~25 MB binary
/usr/local/bin/rye               ← CLI for ops use
/usr/local/bin/rye-inspect       ← verification tool
/usr/local/bin/rye-sign          ← signing tool
/etc/ryeos/release-trust.json    ← trusted release-signer fingerprint
/usr/local/bin/entrypoint.sh     ← bundle-install + boot
```

Contains **no bundles, no project content**. ~30 MB total. Multi-arch
(`linux/amd64`, `linux/arm64`).

Image labels:

```
org.opencontainers.image.source = https://github.com/leolilley/ryeos
org.opencontainers.image.version = vX.Y.Z
io.rye.host-triple = x86_64-unknown-linux-gnu
io.rye.bundle-protocol = 1.0
```

The `bundle-protocol` label lets the deployer assert compatibility before
pulling bundles built against an older daemon.

### 3.2 Bundle artifacts

Each bundle is published as an **OCI Artifact** (not a container image) using
the [OCI Artifacts spec](https://github.com/opencontainers/artifacts) — same
registry, different manifest media type:

```
mediaType: application/vnd.rye.bundle.v1+tar.zst
config:    application/vnd.rye.bundle.config.v1+json
layers:
  - mediaType: application/vnd.rye.bundle.tree.v1+tar.zst
    digest: sha256:<digest of the .ai/ tree as a tarball>
```

`pull` materializes the `.ai/` tree under a target path; the daemon's
`bundle.install` service does this on startup.

Three bundle artifacts ship from `ryeos-cas-as-truth`:

| Bundle                 | Versions independently | Contents                                                                 |
| ---------------------- | ---------------------- | ------------------------------------------------------------------------ |
| `bundle-core`          | yes                    | handlers, parsers, protocols, services, routes, kinds                    |
| `bundle-standard`      | yes                    | runtime drivers, model routing, providers, agent base                    |
| `bundle-trust-default` | yes                    | trust doc(s) for the release signer + verifying-keys for default bundles |

Project bundles (`tv-tracker-bundle`, `track-blox-bundle`, …) follow the same
shape and live in their own repos.

### 3.3 Trust artifacts

The `bundle-trust-default` artifact ships a tarball of:

```
trust/
  release-signer.json          ← Ed25519 fingerprint of the release key
  bundle-core-signer.json
  bundle-standard-signer.json
```

Operators can override by publishing their own trust bundle and pointing
`RYE_TRUST_BUNDLE` at it. Federations of organizations publish trust bundles
that include each member's signer keys (cross-trust without secret sharing).

### 3.4 Project bundles

Same OCI artifact shape as 3.2. Application repos ship their own:

```
ghcr.io/<org>/tv-tracker-bundle:0.5.0
  layers:
    .ai/
      directives/apps/tv-tracker/
      knowledge/apps/tv-tracker/
      tools/apps/tv-tracker/
        api/backend-client.{ts,js,yaml}
      config/agent.yaml
```

Built and signed by the application team's keys. Signer fingerprint is in their
own trust bundle (or installed via `rye trust add` at deploy time).

### 3.5 SBOM + provenance

Every artifact ships with a [SLSA provenance attestation](https://slsa.dev) and
SBOM (CycloneDX). cosign attaches both as referrer artifacts:

```
ghcr.io/rye/ryeosd:vX.Y.Z
ghcr.io/rye/ryeosd:vX.Y.Z.sig         ← cosign signature
ghcr.io/rye/ryeosd:vX.Y.Z.att         ← SLSA provenance
ghcr.io/rye/ryeosd:vX.Y.Z.sbom        ← CycloneDX SBOM
```

This closes the supply chain — operators verify before pulling, attest before
running.

---

## 4. Versioning Model

```
ryeosd                 — semver, daemon-only API stability
bundle-core            — semver, increments when handler/parser/route ABI changes
bundle-standard        — semver, increments when runtime/provider config changes
bundle-trust-default   — date-based (signers rotate; not API-shaped)
project bundles        — owned by app team
```

**Compatibility matrix** (machine-readable, published with each daemon release):

```yaml
# https://ghcr.io/rye/compat/v1
ryeosd: 0.3.x
compatible_bundles:
  bundle-core: ">=0.2.0,<0.4.0"
  bundle-standard: ">=0.3.0,<0.4.0"
bundle_protocol: 1.0
host_triples: [x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu]
```

The daemon refuses to install bundles that violate the matrix at install time,
not at first failed materialization. Drift is loud, not silent.

---

## 5. Runtime Bundle Installation

The daemon's existing `bundle.install` service (`ryeosd/src/service_executor.rs`)
becomes the universal install path. It accepts:

```
bundle.install <ref>

ref ::= oci://<registry>/<bundle>:<tag>
      | https://<host>/<path>.tar.zst
      | file:///absolute/path
      | rye://<bundle-id>:<version>     ← future "rye registry" canonical scheme
```

### 5.1 Install steps (per ref)

```
 1. Resolve ref → URL (rye:// uses a discovery file at well-known URL)
 2. Pull bytes (oras for OCI, plain HTTP for tarballs)
 3. Unpack to temp dir
 4. Verify cosign signature against trust store
 5. Verify bundle's `refs/bundles/manifest` against the manifest signer's
    fingerprint in the bundle's own metadata (chain-of-trust)
 6. Compute compat against running daemon (host triple, bundle protocol)
 7. Move into <system_space>/<bundle-name>/
 8. Write signed <system_space>/.ai/node/bundles/<bundle-name>.yaml
 9. Hot-reload engine (RuntimeRegistry, kinds, routes refresh)
```

### 5.2 Declarative install via env

The container entrypoint reads `RYE_BUNDLES`:

```bash
RYE_BUNDLES="
  oci://ghcr.io/rye/bundle-core:0.2.0
  oci://ghcr.io/rye/bundle-standard:0.3.1
  oci://ghcr.io/me/tv-tracker-bundle:0.5.0
"
```

For each line:

1. Skip if already installed at the same version.
2. Otherwise `rye bundle install <ref>` (or daemon does it via boot hook).

Idempotent. First boot pulls everything; subsequent boots no-op.

### 5.3 Bundle GC

Old bundle versions stay on the volume until `rye bundle gc` reaps them.
Default policy: keep the 3 most recent of each bundle name. State references
the active version; GC will not remove a bundle still referenced by an
in-flight thread checkpoint.

---

## 6. The Three Tiers — Migration Path

### Tier 1 — single composed image (week 1)

GHCR-published image with everything baked in. No runtime install. No
cross-repo dependencies. This is "PyPI-equivalent for now".

**Artifacts:**

- `ghcr.io/leolilley/ryeosd-full:vX.Y.Z` — daemon + both bundles + signer
- (Optional) `ghcr.io/leolilley/ryeosd:vX.Y.Z` — daemon-only, future-proofing

**Consumers:**

```dockerfile
# tv-tracker/Dockerfile
FROM ghcr.io/leolilley/ryeosd-full:0.2.0
COPY .ai /opt/tv-tracker/.ai
COPY deploy/tv-tracker-backend.json \
     /opt/ryeos/core/.ai/node/auth/authorized_keys/
ENV RYE_PROJECT_ROOT=/opt/tv-tracker
```

**Pros:** simplest possible. Single artifact, single pull, no registry
discovery, no signature plumbing beyond cosign on the image.

**Cons:** image is rebuilt for every bundle change even when daemon hasn't
moved. Project content still requires a downstream Dockerfile per consumer.

**Decision criteria:** ship Tier 1 if you have <5 deployers and bundles change
infrequently.

### Tier 2 — split daemon vs bundles (months 1-3)

Publish `ryeosd:vX.Y.Z` (daemon-only) **and** OCI artifacts for each bundle.
Composition happens in a downstream Dockerfile or init container.

**Artifacts:**

- `ghcr.io/leolilley/ryeosd:vX.Y.Z`
- `ghcr.io/leolilley/bundle-core:vA.B.C` (OCI artifact)
- `ghcr.io/leolilley/bundle-standard:vA.B.C` (OCI artifact)
- `ghcr.io/leolilley/bundle-trust-default:date-tag`

**Composition (init-container pattern):**

```yaml
# k8s pod spec
initContainers:
  - name: pull-bundles
    image: ghcr.io/oras-project/oras:1.2.0
    command:
      - sh
      - -c
      - |
        oras pull ghcr.io/rye/bundle-core:0.2.0    -o /data/core
        oras pull ghcr.io/rye/bundle-standard:0.3.1 -o /data/standard
    volumeMounts: [{ name: rye-data, mountPath: /data }]
containers:
  - name: ryeosd
    image: ghcr.io/leolilley/ryeosd:0.3.0
    env:
      - { name: RYE_SYSTEM_SPACE, value: /data/core }
    volumeMounts: [{ name: rye-data, mountPath: /data }]
volumes:
  - { name: rye-data, emptyDir: {} }
```

**Pros:** independent versioning, smaller daemon image (~30 MB), faster CI
(only rebuild what changed), easy rollback per bundle.

**Cons:** more moving parts at deploy. Operators must understand OCI artifacts.
Compose-time signature verification still manual.

**Decision criteria:** move to Tier 2 when bundle release cadence diverges from
daemon release cadence, or when more than two consumers exist.

### Tier 3 — runtime registry + declarative install (months 6+)

Daemon boots with **only `RYE_BUNDLES` env**. It installs everything itself,
verifies signatures end-to-end, hot-reloads, and supports add/remove without
restart.

**Container image:** just `ghcr.io/leolilley/ryeosd:vX.Y.Z` (~30 MB).

**Composition:** declarative, in the deploy spec.

```yaml
# Railway / Fly / k8s
image: ghcr.io/leolilley/ryeosd:0.3.0
env:
  RYE_BUNDLES: |
    oci://ghcr.io/rye/bundle-core:0.2.0
    oci://ghcr.io/rye/bundle-standard:0.3.1
    oci://ghcr.io/me/tv-tracker-bundle:0.5.0
  RYE_TRUST_BUNDLE: oci://ghcr.io/rye/bundle-trust-default:2026-05
```

**Pros:** absolute minimum coupling. Daemon image rarely rebuilds. New
applications deploy by adding one line to env. Full chain-of-trust
verification at install time, not at deploy-image-bake time.

**Cons:** registry availability becomes a boot dependency (mitigated by local
caching + retry). Signature plumbing must be airtight — a misverified bundle
is a compromised node.

**Decision criteria:** Tier 3 is the steady state. Get there once trust
plumbing (signed `node/bundles/*.yaml` registration, trust bundle import,
cosign verification in `bundle.install`) is hardened. Reference doc:
`signing-trust-advanced.md`.

---

## 7. The "rye Registry" Future

OCI works but is an awkward registry for non-container artifacts. The
long-term ideal is a **dedicated bundle registry** with semantics that match
Rye's needs:

```
https://bundles.rye.io/
  /v1/<namespace>/<bundle-name>/<version>
    /tree.tar.zst        ← the .ai/ payload
    /manifest.json       ← CAS manifest hash, host triples, signers
    /attestation.json    ← SLSA provenance
    /signers.json        ← Ed25519 fingerprints + verification metadata
```

API:

```
GET  /v1/discover/<namespace>/<bundle-name>             → versions list
GET  /v1/<namespace>/<bundle-name>/<version>/tree.tar.zst
POST /v1/publish                                         → upload (signed)
GET  /v1/search?q=…&triple=…&compat=…                   → discovery
```

References use the canonical scheme:

```
rye://core@0.2.0
rye://standard@0.3.1
rye://leolilley/tv-tracker@0.5.0
```

Resolution: the daemon hits a discovery file (`https://bundles.rye.io/.well-known/rye-registry`)
that maps the rye:// URL → an OCI ref or HTTPS tarball, then downloads
normally.

This is not blocking work for Tier 3. It is a pure UX layer over OCI; same
verification, same artifacts, friendlier names.

---

## 8. Trust & Signing — End-to-End

The deployment story is only safe if the trust chain is intact.

### 8.1 Build-time signing

The release CI uses a **release signing key** stored in either:

- **OIDC + Sigstore Fulcio** (preferred long-term — keyless, audit-logged)
- **GitHub Actions OIDC + cosign + KMS** (interim — KMS-backed key, no PEM in repo)
- **GHA secret PEM** (Tier 1 only — operationally simple, key rotation is manual)

Every artifact (image, OCI bundle, trust bundle) is cosign-signed.

### 8.2 Bundle internal signing

Inside each bundle's `.ai/`, every signable item (`refs/bundles/manifest`,
node-config records, individual handler/route YAMLs) carries a `rye:signed`
header. The signer fingerprint is the same release key (or a delegated bundle
signing key — see `signing-trust-advanced.md`).

### 8.3 Daemon-side verification

```
1. Pull artifact → cosign verify against trust-bundle's release fingerprint
2. Unpack tree → daemon reads .ai/refs/bundles/manifest
3. Daemon resolves manifest object from CAS, checks its rye:signed header
4. For each binary in manifest, daemon checks item_source.signature_info
   against trust store
5. Materialization only proceeds if all four checks pass
```

A trust violation at any layer fails the install loudly with the exact
fingerprint that was rejected.

### 8.4 Trust rotation

Trust bundle is its own artifact, dated. Operators publish a new
`bundle-trust-default:2026-08` when keys rotate. Old daemons keep working
against old bundles; new daemons use the new trust bundle. No simultaneous
key flag day. See `key-rotation-and-trust-policy.md` for the full scheme.

---

## 9. Mapping Existing Code to the Future

| Already exists                       | Path forward                                             |
| ------------------------------------ | -------------------------------------------------------- |
| `rye-bundle-tool rebuild-manifest`   | Wired into release CI Step 5 (Tier 1)                    |
| `rye-sign`                           | Wired into bundle YAML signing in CI                     |
| `bundle.install` service alias       | Extended to accept `oci://` and `https://` refs (Tier 3) |
| `node/bundles/*.yaml` registration   | Auto-generated by `bundle.install` (Tier 3)              |
| `resolve_native_executor_path` chain | Already host-triple-aware; no change                     |
| Trust store (`authorized_keys/`)     | Read at install time + materialization                   |
| `rye-inspect`                        | Becomes the verify-before-install tool                   |

What is missing today (rough order, smallest first):

1. GHA workflow that builds + publishes Tier 1 image ← do now
2. cosign signing of the published image
3. OCI artifact pack/unpack for bundles (Tier 2 prerequisite)
4. `bundle.install` accepting `oci://` refs
5. `RYE_BUNDLES` env-driven boot loop in entrypoint
6. Trust bundle artifact format + import
7. Signed `node/bundles/<name>.yaml` auto-generation
8. Hot-reload of engine on bundle add (currently requires restart)
9. `rye bundle gc` policy
10. `rye://` registry resolver + bundles.rye.io itself

Items 1-5 deliver Tier 3 functionally. 6-10 are productionizing.

---

## 10. What This Means for Today

For the TV Tracker migration (now): **ship Tier 1**. One GHA workflow, one
GHCR image, one downstream Dockerfile in `network-tv-tracker`.

```
ryeos-cas-as-truth/.github/workflows/release.yml
  → on tag v*: build, install bins into bundles, rebuild manifests,
    sign, build Docker image, push to ghcr.io

network-tv-tracker/Dockerfile.ryeosd
  → FROM ghcr.io/leolilley/ryeosd-full:0.2.0
    COPY .ai /opt/tv-tracker/.ai
    COPY deploy/tv-tracker-backend.json \
         /opt/ryeos/core/.ai/node/auth/authorized_keys/
```

The Tier 2 split (separate `ryeosd` + `bundle-*` artifacts) is added once a
second consumer exists. The Tier 3 declarative env-driven install is added
when the trust plumbing is hardened enough that we'd trust a registry to
serve us code.

The implementation doc at `.tmp/TV-TRACKER-RYEOSD-IMPLEMENTATION.md` already
covers Tier 1. The future docs in `signing-trust-advanced.md`,
`key-rotation-and-trust-policy.md`, and `cluster-bootstrap.md` cover the
trust hardening that gates Tier 3.

---

## 11. Non-goals

- **Rye-on-PyPI.** The daemon is Rust; the closest equivalent is a Cargo
  install, but that's only useful for CLI distribution, not server deployment.
- **Helm charts as primary distribution.** A community helm chart can wrap
  Tier 3 trivially; we don't ship it as the canonical artifact.
- **Snap/Flatpak.** Desktop packaging is downstream; the canonical
  distribution is OCI-registry-shaped.
- **Single static binary that contains all bundles.** Tempting, but breaks
  independent versioning, increases binary size by ~30 MB, and prevents
  per-bundle GC. Bundles stay separate.

---

## 12. Open Design Questions

1. **Bundle ABI versioning.** Do we expose a single `bundle_protocol` int
   (current proposal: 1.0), or fine-grained per-subsystem versions
   (kind-schema v2, route-schema v3, runtime-yaml v1)? Coarse is simpler;
   fine-grained allows independent evolution.

2. **Mirror/proxy semantics.** When `bundles.rye.io` is unreachable, should
   the daemon fall back to a mirror list? Or is local CAS cache sufficient?
   PyPI's mirror story (`extra-index-url`) is messy; we should learn from it.

3. **Private bundles.** OCI registries handle auth via `docker login`. Does
   the daemon want its own credential store, or do we rely on the OCI
   registry library's auth? Probably the latter; complexity isn't worth it.

4. **Bundle dependencies.** Do we ever need `bundle-A depends-on bundle-B`?
   So far no — bundles are flat namespaces unioned at install time. If a
   project bundle assumes `bundle-standard >= 0.3`, it should fail at install
   time via the compat matrix, not via a recursive dependency resolver.

5. **CAS deduplication across bundles.** Two bundles shipping the same blob
   (e.g. shared schema YAML hashes) currently store it twice. A union CAS
   dedupes at install time but complicates bundle removal. Probably defer
   until measurements show meaningful waste.

---

## 13. Summary

The ideal: **declarative, signed, versioned, runtime-installed bundles + a
tiny daemon image, all served from an OCI registry, eventually fronted by a
purpose-built rye registry**.

The path: ship Tier 1 today (single composed image), split to Tier 2 when
multiple consumers exist, reach Tier 3 when trust plumbing is hardened.

The whole point: a Rye node is built like Nix, distributed like a container,
verified like Sigstore, and configured like Helm — without any one of those
becoming a hard dependency.
