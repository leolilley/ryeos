<!-- ryeos:signed:2026-07-14T01:54:46Z:d196e6ddbbb1db6ab05b3598cd356655eb14ab3639f131a9fb162822de76313c:TZ+2D3erOhtrJ6C1kdNjB7+Jfqt3Wkr95uilmBFdMSa5eBb6znNxQCkuLo4XA9LH3+PbrkuqzdbuRCs/yhHVCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, init, setup, bundles, trust, publisher, ryeos-node]
version: "1.1.0"
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
5. Write self-trust docs for both keys into the operator trust store.
6. Pin the official publisher key from hardcoded public key bytes.
7. Pin any additional `--trust-file` publisher docs.
8. Discover bundles in the source directory.
9. Build the bundle plan and verify manifest dependency policy.
10. Preflight-verify source bundles unless explicitly skipped by tests.
11. Install/replace bundles under `<system>/.ai/bundles/<name>/` and
    write signed registrations under `<system>/.ai/node/bundles/`.
12. Create/load the vault X25519 keypair.
13. Write create-once node policy files if missing, including the disabled strict
    subprocess sandbox policy and ingest-ignore config.
14. Reload trust and verify official publisher, user key, and node key
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
operator-edited `.ai/node/sandbox.yaml`. See
[Execution Sandbox](execution-sandbox.md) for the complete accepted policy.
