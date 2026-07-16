<!-- ryeos:signed:2026-07-16T04:18:05Z:4f1586c4d0e69da43222e3c5d2e941c2292aac39d4f3f89e2bb81fea1ad5f614:h7mN3qAHaemRype2KhHiqxEcnRNGv/lNCG16MBWOTFizbn13ta/F906Cb4NQh7sRqRwLK/xTxExNYAsrK1DzBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, init, setup, bundles, trust, publisher, ryeos-node]
version: "1.3.0"
description: >
  Operator initialization contract implemented by ryeos-node: keys,
  trust, bundle discovery, bundle planning, install, and post-init checks.
---

# Operator Init (`ryeos init`)

`ryeos init` is implemented by `ryeos-node` (`init::run_init`) and is
the authoritative path for operator-owned artifacts and installed bundle
registrations. The daemon must not substitute for it.

## Inputs

```bash
ryeos init [--source <dir>] [--app-root <dir>] [--trust-file <file>...]
```

Defaults are `/usr/share/ryeos` for source and the XDG data dir
(`~/.local/share/ryeos`) for the app root, overridable via `--app-root`
or `RYEOS_APP_ROOT`. Packaged installs initialize with plain `ryeos init`.

## Init sequence

1. Validate the bundle source directory exists.
2. Create the app-root layout (`<app_root>/.ai/{node,state,bundles,config}`).
3. Load-or-create the operator Ed25519 signing key.
4. Load-or-create the node Ed25519 signing key.
5. Write self-trust docs for both keys into the node trust store.
6. Pin the official publisher key from hardcoded public key bytes.
7. Pin any additional `--trust-file` publisher docs.
8. Discover bundles in the source directory.
9. Build the bundle plan and verify manifest dependency policy.
10. Compose and admit the exact prospective source generation. An existing
    enforced policy must resolve and inspect its selected backend here even
    when ordinary test preflight is skipped.
11. Preflight-verify source bundles unless explicitly skipped by tests.
12. Install/replace bundles under `<system>/.ai/bundles/<name>/` and
    write signed registrations under `<system>/.ai/node/bundles/`.
    Before selected-backend activation, resolve and inspect its exact completed
    staging tree.
13. Create/load the vault X25519 keypair.
14. Write create-once node policy files if missing, including the disabled strict
    subprocess isolation policy and ingest-ignore config.
15. Reload trust and verify official publisher, user key, and node key
    are trusted.

The init report includes the app root, operator/node key fingerprints,
official publisher fingerprint, vault public-key fingerprint, and
installed bundle names.

## Official publisher trust

`ryeos init` pins the official publisher key without trusting any bundle
file. The compiled `OFFICIAL_PUBLISHER_PUBKEY` bytes must hash to the
compiled `OFFICIAL_PUBLISHER_FP`:

```text
c9d7301fba468b669d91a6000e9b6a4158c0e615dea4fe1f99906b8c9214bc28
```

Rotation requires a coordinated `ryeos` binary release. Development
bundles are signed with `.dev-keys/PUBLISHER_DEV.pem` and trusted with
`--trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml`.

A packaged `PUBLISHER_TRUST.toml` is only a trust pointer. Its location beside
a bundle, package, or container never grants authority by itself. Additional
publishers enter the node trust store only through an explicit trust
choice:

- `ryeos init --trust-file <PUBLISHER_TRUST.toml>` pins a named publisher;
- development/custom containers may opt in to their baked publisher set with
  `RYEOS_TRUST_BAKED_PUBLISHERS=1`; and
- the local source installer may opt in with `--trust-source-publishers`.

Release containers pass no packaged trust documents to `ryeos init`.
Development opt-ins trust every publisher document in the selected source
boundary, so use them only after independently verifying that source. The
loader validates each document's decoded key and fingerprint before pinning it,
and bundle preflight still fails closed when content is not signed by a pinned
publisher.

## Bundle discovery and planning

The source directory is scanned only for immediate child directories that
contain `.ai/`. Hidden directories and invalid bundle names are skipped;
there are no hardcoded bundle names. Planning checks provider/consumer
relationships, duplicate providers, cycles, and install order. Existing
installed bundles are atomically replaced with staging → swap and a
one-generation backup.

## What init does not do

`ryeos init` does not start the daemon and does not depend on it. Runtime
startup may repair daemon-local public identity, daemon config, vault
public key output, and local authorized-key entry, but only after
init-state verification succeeds. Init also never overwrites an existing
operator-edited `.ai/node/isolation.yaml`. See
[Execution Isolation](execution-isolation.md) for the complete accepted policy.
