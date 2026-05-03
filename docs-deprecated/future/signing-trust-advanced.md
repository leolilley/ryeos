# Signing & Trust — Advanced Path (deferred)

> **Status:** explicitly deferred. The current basic-path implementation (see `.tmp/SIGNING-INIT-FLOW-IMPLEMENTATION.md`) handles every signing scenario we have today. This document captures the items oracle put in the "advanced path" along with the trigger conditions that would make each worth picking up.
>
> Until one of the trigger conditions becomes real, **do not build any of this**. Self-signed trusted-key docs + the three-role split (author / user / node) is sufficient.

---

## 1. Multi-publisher trust delegation

**What it is.** Today there is one bundle-author key (Leo's) shipping inside every bundle the platform produces. Multi-publisher means: third-party authors produce their own bundles, signed with their own keys, and the operator opts in to trusting each author independently.

**Trigger.** Anyone other than the platform maintainers ships an installable bundle that an operator wants to install.

**Sketch when picked up.**
- Each bundle ships `<bundle-root>/.ai/config/keys/trusted/{publisher_fp}.toml` declaring the publisher's verifying key (already supported architecturally — basic path keeps this slot open).
- `bundle.install` reads the bundle's declared publisher fingerprint, looks up `~/.ai/config/keys/trusted/{fp}.toml`, refuses install with a clear "untrusted publisher" error if absent.
- Operator opts in by writing a trust doc for the publisher's key (manual step; no auto-trust).
- Optional UI: `rye trust add <fingerprint> --label <publisher-name>` verb that fetches the publisher's verifying key from a known-good source and writes the trust doc.

**What stays the same.** The three-role split (author / user / node), the per-item signing flow, the per-bundle trust doc shipping inside the bundle.

---

## 2. Online bundle registry / install + update

**What it is.** Today bundles install from a local source path via `service:bundle/install --source_path <dir>`. Online registry means: bundles published to a server, discoverable via a name + version, fetched + verified + installed.

**Trigger.** Any of:
- More than 3 bundles need to be tracked across operator machines.
- Operators ask for "list available updates" / "update standard to v1.4.0".
- A registry hosting service exists (could be a static HTTP file server or an OCI-style artifact registry).

**Sketch when picked up.**
- New service: `service:bundle/fetch` — given `{name, version, registry_url}`, downloads the bundle archive, verifies its publisher signature, lays it down in a quarantine directory.
- `service:bundle/install` extended to accept `--from-registry <url> --name <bundle-name> --version <semver>` as an alternative to `--source_path`. Internally chains through `service:bundle/fetch`.
- Registry response format: bundle archive + manifest signed by publisher. No new signing semantics required (publisher is the same author key).
- `service:bundle/update` — given `{name}`, looks up the bundle's recorded registry URL + current version, fetches the latest, verifies, installs over the existing entry.

**What's tricky.** Network failure modes, partial-state recovery if fetch succeeds but install fails. Atomic install via shadow directory + atomic rename.

---

## 3. Per-bundle author keys

**What it is.** Today the basic-path architecture supports one author key per bundle (each bundle's `.ai/config/keys/trusted/{fp}.toml` is independent). In practice today, all platform bundles share Leo's single key. Per-bundle author keys means platform bundles ship with DIFFERENT author keys (e.g. `core` signed by `core-maintainer`, `standard` signed by `standard-maintainer`, individual sub-bundles by their domain owners).

**Trigger.** The platform team grows past one person, and ownership of bundles diverges.

**Sketch when picked up.** No code changes needed — already supported. Just the maintainer process changes:
- Each bundle's `.ai/config/keys/trusted/{fp}.toml` ships the relevant maintainer's verifying key.
- `rye-bundle-tool sign-items --source <bundle> --key <maintainer.pem>` — each maintainer uses their own key for their bundles.
- Operator's trust store ends up with multiple author trust docs (one per platform bundle maintainer), each one entered manually or via a "platform setup" script that bundles the verifying keys for the standard set.

---

## 4. Bundle metadata with explicit publisher identity

**What it is.** A new field in the bundle MANIFEST or `<bundle>/.ai/config/bundle.yaml` that names the publisher in addition to the signature fingerprint. Cosmetic clarity for error messages and UI.

**Trigger.** Multiple publishers exist AND error messages are confusing operators ("signed by `8f4c00…`" → "signed by `acme-corp`").

**Sketch when picked up.**
- Add `publisher: <human-readable-name>` to the trust doc TOML (already half-present via `owner` field).
- Surface `publisher` in `service:bundle/list` output and `bundle.install` errors.
- No security-relevant change — fingerprint remains the trust anchor.

**Caution.** Don't conflate the publisher NAME (display string) with the trust anchor (fingerprint). The fingerprint is what's verified; the name is decoration.

---

## 5. Signed install receipts / provenance chains

**What it is.** When `bundle.install` succeeds, write a signed receipt (`<state_dir>/.ai/bundles/<name>/install-receipt.yaml`) capturing: bundle name + version + publisher fingerprint + install timestamp + node fingerprint of the daemon that installed it. Receipts chain (each new install/update references the previous receipt's hash).

**Trigger.** Audit / compliance requirement — "prove that this machine installed this bundle from this publisher at this time."

**Sketch when picked up.**
- New artifact type: `kind: install-receipt` (added to the kind registry, like any other kind).
- `bundle.install` writes the receipt as the last step, signed by the node key.
- `bundle.update` chains the new receipt to the old one's content hash.
- `service:bundle/audit` — verifies the receipt chain is intact and references valid publisher signatures.

**Cost.** New kind, new service, new tests, audit logic. Heavy — only worth it under a compliance driver.

---

## 6. Cross-signed author certificates / registry-pinned publisher keys

**What it is.** Beyond simple self-signed trusted-key docs: introduce a third-party "trust authority" that cross-signs publisher keys. Operators trust the authority, and via that, all publishers that the authority has cross-signed.

Or: a registry pins specific publisher fingerprints, and the operator trusts the registry; install verifies against the registry's pinned set.

**Trigger.** Either:
- The platform becomes a true marketplace with arbitrary publishers (need delegated trust to scale operator opt-in).
- A regulatory or organizational requirement demands a single point of trust admission rather than per-publisher trust docs.

**Sketch when picked up.**
- Trust-authority model: introduce `kind: cross-cert` — a doc signed by the authority key, declaring `{publisher_fp, capabilities, expiry, label}`. Operator trusts the authority's verifying key; trust over each publisher derives from the cross-cert + authority sig.
- Registry-pinning model: registry serves a signed list of `(publisher_fp, allowed_bundles)`. Install verifies the bundle's publisher fingerprint is pinned by the registry the operator opted into.
- Either way: existing per-bundle trust docs continue to work (basic path is not deprecated — it becomes the "no authority, direct trust" mode).

**This is real engineering** (new kind, new verb to manage authority/registry trust, cert revocation semantics, expiry handling). Defer until there's a concrete deployment scenario that needs it.

---

## 7. What is NOT advanced — already in the basic plan

For clarity, these were considered and explicitly pulled into the basic-path implementation plan rather than deferred:

- **Per-bundle author trust doc shipping inside the bundle** — already in `.ai/config/keys/trusted/{fp}.toml`.
- **`bundle.install` preflight verification** (validate every signed item against current trust + kind registry before mutating filesystem) — basic-path step 3.
- **Three-key role separation** (author / user / node) — basic-path step 1.
- **Self-signed trusted-key docs at init time** — basic-path step 2.
- **Maintainer signer with explicit `--registry-root` and `--key`** (no ambient discovery) — basic-path step 5.

If any of these slipped out of the basic-path implementation, that's a regression — they're not "advanced", they're table stakes.

---

## 8. When to revisit this document

Re-read this doc when any of the following becomes true:

- A non-platform-team author wants to ship an installable bundle. → Sections 1, 4.
- Bundles need to be discovered / fetched / updated over the network. → Section 2.
- The platform team grows beyond one person and bundle ownership diverges. → Section 3.
- An audit / compliance requirement appears. → Section 5.
- The system becomes a true marketplace with delegated trust. → Section 6.

Until then, the basic path stands.
