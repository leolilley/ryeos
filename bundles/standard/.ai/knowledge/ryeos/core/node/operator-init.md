<!-- ryeos:signed:2026-09-03T11:56:15Z:71d087ced1d9406a6ab36981cc83710088ecaa531c7ac38872f2c0a6da68cfce:OiJz8wfVy2RRSqzoAEgiKtGzP4wx/USG+DCyF/AZTYmX6whnrTETgvrY6xaGZfrVoMtwqiB+yhZuxt7oOXe9Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, init, setup, bundles, trust, publisher, ryeos-node]
version: "1.4.1"
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
ryeos init [--source <dir>] [--app-root <dir>] [--trust-file <file>...] [--node-profile <name>]
```

Defaults are `/usr/share/ryeos` for source and the XDG data dir
(`~/.local/share/ryeos`) for the app root, overridable via `--app-root`
or `RYEOS_APP_ROOT`. Every fresh initialization requires
`--node-profile <name>` to select a complete publisher-signed init profile from
`<source>/.ai/node/init/profiles/<name>.yaml`. Packaged full installs use
`--node-profile full`; each lean distribution uses its exact same-named
profile. Selection is never inferred from the bundles present in the source,
and absence fails rather than manufacturing an implicit default.

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
10. If a node init profile was selected, verify its publisher signature, use
    its sorted `exact_bundles` as the prospective complete installed inventory,
    require every selected bundle to exist in the discovered source, validate
    every typed policy section, and refuse a conflict with an existing
    nonempty policy generation unless the explicit schema-cut flags were
    supplied.
11. Compose and admit the exact prospective source generation. An existing
    enforced policy must resolve and inspect its selected backend here even
    when ordinary test preflight is skipped.
12. Preflight-verify source bundles unless explicitly skipped by tests.
13. Install/replace selected bundles and remove registrations outside the
    prospective exact inventory under the same locked init. Installed bundle
    roots live beneath `<system>/.ai/bundles/<name>/`; signed registrations
    live beneath `<system>/.ai/node/bundles/`.
    Before selected-backend activation, resolve and inspect its exact completed
    staging tree.
14. Create/load the vault X25519 keypair.
15. When no generation exists, require the selected node init profile and
    publish all of its policies beneath
    `<system>/.ai/node/policies/`, signed by the node key. With an existing
    generation, an omitted selection preserves it and an explicit selection
    must be identical unless explicit replacement is selected. Init authors no
    implicit isolation, ingest-ignore, or other policy fallback. Materialize
    only the read-only sync view derived from the admitted generation.
16. Write the signed init-completion fence only after the intended bundle
    registration inventory and policy generation have completed. The prior
    fence is durably absent throughout every bundle/policy mutation. Daemon
    startup re-verifies it after the state lock and any interrupted per-bundle
    reconciliation, refusing partial crash results.
17. Reload trust and verify official publisher, user key, and node key
    are trusted.

The init report includes the app root, operator/node key fingerprints,
official publisher fingerprint, vault public-key fingerprint, and
installed bundle names. It also names the explicitly selected node init profile,
when present.

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

Persisted container nodes preserve their complete signed policy generation by
default. When a release intentionally cuts that policy schema or changes the
image's exact init-profile bundle inventory, the operator may set
`RYEOS_RESET_NODE_POLICY_GENERATION=1` for one stopped-container boot. The
entrypoint asks one locked init to replace that generation from the trusted
baked profile, align the prospective exact bundle inventory, and publish its
completion fence before starting the daemon. All other node state is
preserved; remove the variable after the successful cut.

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
init-state verification succeeds. Init never authors an implicit policy
fallback or overwrites a different existing node-signed generation. Stopped
node policy changes use the typed `ryeos node policy-apply` boundary. See
[Execution Isolation](execution-isolation.md) for the isolation section's
complete accepted body.

When the current registry adds or removes a mandatory section, the predecessor
generation is intentionally undecodable and `policy-apply` cannot treat it as
current authority. Use the explicit stopped-node
`ryeos node reset policy-generation --node-profile <name> --confirm` cutover
against the trusted packaged source. There is no implicit migration or
fallback.
