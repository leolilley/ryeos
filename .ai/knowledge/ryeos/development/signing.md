<!-- ryeos:signed:2026-09-10T20:28:16Z:0571b485b5d6ab3f159d0061550849a6810276476f93cb0288f2887807c254e7:I5lQvB+QoeW49fglIUnelMI1yyy7NePCF8RyNyVDYeMt+ZKhjMYjvIMXcbS7w5rf9D1AGrOkgqG7AcCozduTBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "signing"
title: "Signing Rules"
description: "Short rules for bundle signing, project item signing, keys, and recovery"
entry_type: reference
version: "1.3.0"
```

# Signing Rules

## Bundle rule

Sign bundles as bundles. Do not try to fix one bundle YAML or one staged binary
by hand.

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev \
  --all
```

This builds release binaries, stages them into bundle bin trees, signs all
signable bundle items, rebuilds CAS manifests, and emits `PUBLISHER_TRUST.toml`.
`--all` is required (or `--crates "<crate ...>"` to rebuild a subset, `--jobs N`
to cap parallelism) — populate refuses to rebuild the full set implicitly.

Run it after:

- editing YAML under `bundles/`;
- changing Rust that affects bundled binaries;
- a fresh checkout, because bundle bins/CAS are derived;
- any `hash mismatch` or stale manifest failure.

## Dev key

Use only the checked-in dev publisher key for dev bundles:

```text
.dev-keys/PUBLISHER_DEV.pem
.dev-keys/PUBLISHER_DEV_TRUST.toml
```

Dev fingerprint:

```text
741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
```

Never trust this key in production. Never use old `--seed 42` instructions for
bundle artifacts; that key is not trusted by the engine test harness.

## What invalidates signatures

Any byte change to signed YAML invalidates its `# ryeos:signed:...` header.
The loader strips the signature line, hashes the body, and verifies the Ed25519
signature against the trust store. A stale header is not reusable.

Quick check:

```bash
head -1 bundles/standard/.ai/config/ryeos-runtime/model-providers/zen.yaml
# trailing fingerprint should begin 741a8bc...
```

## Recovery playbook

For stale signatures, stale bundle binaries, stale CAS, or wrong-key signing:

```bash
./scripts/gate.sh --refresh-bundles --no-tests

target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core
target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core
```

Then reinitialize the system space you are actually using:

```bash
target/release/ryeos init \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

If that system space has a running daemon, restart it around init.

## Project item signing

### Repository-local development bundle

This repository's root `.ai/` is the RyeOS development bundle, not an ordinary
operator-authored end-user project. Its authored items use the deliberately
public development publisher fixture, as specified in
`source-local-bundle-development.md`. Sign selected edited items in place:

```bash
scripts/dev/sign-dev.sh .ai/knowledge/ryeos/development/<entry>.md
```

This existing source-authoring entry does not need an installed node, runtime
history reset, operator key or vault. Do not route these publisher-owned items
through an unrelated installed operator merely because they live under a
project `.ai/` directory. Source ownership selects the signer. Validate the
result through normal resolution/admission; the fixture grants no operational
authority. It is not a substitute for private project signing or combined
`bundles/` payload publication. Never publish the entire live checkout through
a whole-directory bundle exchange; use the detached-tree procedure described
by the source-local development knowledge.

### Ordinary operator-authored projects

Project `.ai/` items are different from bundle items. They are signed with the
operator/user key, not the publisher key.

```bash
ryeos sign knowledge:my/entry
ryeos sign 'tool:agent-kiwi/*'
ryeos sign 'node:routes/*'
```

Use this for project knowledge/directive/tool/node files. The `sign` verb takes
canonical refs, and it accepts `*`/`?` globs in the bare-id part for batch
project signing. Quote globs so the shell does not expand them. Use
`populate-bundles.sh` for `bundles/`.

A file **path** under the project `.ai/` also works — `sign` resolves it to its
single canonical ref and signs that, so you can sign the file you just edited
without retyping the ref:

```bash
ryeos sign .ai/graphs/foo/bar.yaml      # == ryeos sign graph:foo/bar
```

Signing a graph-shaped file as a `tool:` (e.g. one under `.ai/tools/**/graphs/`,
which resolves as `tool:` not `graph:`) now emits a warning pointing at the
canonical `.ai/graphs/` location.

## Authorized-key scopes

`authorize-client` writes one authorized-key file per fingerprint and, by
default, **replaces** its scope set (the result reports dropped scopes). To add scopes
without losing existing ones, pass `--merge-scopes` (it unions with the file's
current scopes):

```bash
ryeos authorize-client --public-key <b64> --scopes <a,b> --merge-scopes
```

Normal CLI use dispatches to the node-owned `identity/authorize-client`
service, online or standalone against existing node state. It requires the
exact configured local operator and reuses the canonical locked grant writer
with the node's retained identity and configured authorization directory.
There is no worker Tool for this operation and no key mount into a subprocess.
The external core-tools entry remains only for explicit host bootstrap; it
shares request validation and the same writer, not a second grant protocol.
Remote-operator grants require an exact origin site and reject wildcards.
Existing remote-node grants are maintained with
`--remote-node-origin-site-id`; this preserves the admitted node's principal
class while reconciling its exact scopes through the same locked writer.
Class/origin conversion still needs explicit stopped-node authority; ordinary
same-class scope merging does not convert or copy any identity.

## Do not do these

- Do not manually copy `target/release/ryeos-core-tools` or another binary into
  `bundles/*/.ai/bin/<triple>/` as a fix.
- Do not edit signed YAML and leave the old signature header.
- Do not add hardcoded trust bypasses, raw YAML fallbacks, or registry shortcuts
  to avoid signing errors.
- Do not verify source bundles without `--registry-root`; installed
  registrations may point at stale bundle copies.
