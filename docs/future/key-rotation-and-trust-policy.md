# Key Rotation & Managed Trust Policy (deferred)

> **Status:** explicitly deferred. The current basic-path
> implementation (`docs/future/signing-trust-advanced.md`,
> `.tmp/POST-KINDS-FLIP-PLAN.md` step 6) covers every signing scenario
> we have today — single platform-author key, hand-pinned third-party
> publishers, manual operator opt-in. This doc captures the *next*
> tier: lifecycle operations (rotation, revocation), better trust-doc
> tooling, and admission policy for fleets.
>
> **Trigger:** none of this is worth building until at least one of
> the conditions below becomes real. Re-read this when that happens.

This doc is the spillover from the oracle review of step 6 — the
"advanced path" the reviewer flagged but explicitly told us not to
build before commit. Together with `signing-trust-advanced.md` (which
covers multi-publisher, registry, cross-cert, install receipts) it
forms the long-tail of trust work.

---

## 1. Compile-time platform-author fingerprint check

**What it is.** Today the platform-author key + fingerprint live as
two `const`s in [`ryeos-tools/src/actions/init.rs`](../../ryeos-tools/src/actions/init.rs)
(`PLATFORM_AUTHOR_PUBKEY: [u8; 32]` and `PLATFORM_AUTHOR_FP: &str`).
A unit test asserts `sha256(PUBKEY) == FP`. Drift would only be caught
at `cargo test` or at `rye init` runtime via
`decode_platform_author_pubkey()`.

A `build.rs` script could **fail the build itself** if the constants
ever drift, removing the entire class of "wrong fingerprint shipped
in a release binary" footguns.

**Trigger.** A near-miss release where the two constants drift and
slip past unit tests (e.g. someone edits one and not the other on a
branch that doesn't run the test, then merges). Until that happens,
the runtime check + unit test are sufficient.

**Sketch.**
- Add `ryeos-tools/build.rs` that recomputes
  `sha256_hex(PLATFORM_AUTHOR_PUBKEY)` and `panic!`s if it differs
  from `PLATFORM_AUTHOR_FP`.
- Either `include!()` the constants from a shared file the build
  script can also read, or use a small proc-macro / declarative-macro
  to single-source them.
- Cost: small. Mostly a build-time refactor of how the constants are
  declared.

**Don't bother yet.** The runtime check is fail-closed (`rye init`
refuses to proceed) and is exercised in CI via the unit test.

---

## 2. Publisher key rotation as a first-class operation

**What it is.** Today there is no explicit rotation flow. If the
platform-author key is compromised:

1. Mint a new keypair.
2. Re-sign every bundle.
3. Ship a new `rye` binary with new `PLATFORM_AUTHOR_PUBKEY` /
   `PLATFORM_AUTHOR_FP` constants.
4. Operators run the new `rye init` against an existing state dir.
   Their old self-pin doc for the *old* platform key sticks around;
   the new pin is added; both work until the operator manually
   removes the old one.
5. Existing on-disk bundles signed by the old key still verify
   against the old pin until manually purged.

This works but is awkward. Real rotation should:

- Mark the old key as superseded by the new key (signed delegation).
- Re-verify every signable item under the new pin.
- Fail-closed if any item is still signed by the old key after the
  rotation completes.

The same applies to user and node keys, with different blast radii.

**Trigger.** Any of:
- A real key compromise or strong suspicion thereof.
- Regulated environments that mandate periodic rotation.
- Multiple publishers in the wild (compounds the rotation problem).

**Sketch.**
- New kind: `kind: key-rotation` — a signed doc declaring
  `{old_fp, new_fp, effective_at, reason}`, signed by either the old
  key (planned rotation) or the new key with a separately published
  out-of-band proof (compromise rotation).
- New verb: `rye trust rotate <old_fp> <new_pubkey_file>` — writes
  the rotation doc, replaces the old pin, and triggers a re-verify
  sweep over every installed bundle.
- Sweep refuses to proceed if items signed only by the old key remain
  unsignable by the new key (i.e. operator needs new bundles before
  rotation can finalize).
- For `--force-node-key` and (future) `--force-user-key`: the same
  rotation doc semantics apply, scoped to the relevant tier. Today
  `--force-node-key` is "regenerate, don't revoke" — explicitly NOT
  rotation in the security sense.

**Cost.** Medium. New kind, new verb, sweep logic, and a careful
think about partial-rotation states. Don't start without a real
trigger.

---

## 3. Trust-doc inspection / verification verbs

**What it is.** Today operators can:

- `rye trust pin <fp> --pubkey-file <file>` — write a pin.
- (Implicitly) `rye init` — pin platform author + self-pin user/node.

There's no first-class verb to:

- List currently-pinned keys with metadata (`rye trust list`).
- Verify an arbitrary trust doc on disk parses, fingerprints
  correctly, and (if signed) verifies (`rye trust verify-doc <path>`).
- Import a `PUBLISHER_TRUST.toml` pointer with a single command
  (`rye trust import-pointer <bundle>` — convenience wrapper around
  `rye trust pin --pubkey-file <bundle>/PUBLISHER_TRUST.toml`).
- Remove a pin (`rye trust remove <fp>`) with a confirmation flow.

**Trigger.** Operator workflow friction shows up. The publish→pin
loop today already requires the operator to pass
`--pubkey-file <bundle>/PUBLISHER_TRUST.toml`; the import-pointer
wrapper is a quality-of-life win, not a correctness requirement.

**Sketch.** Each is a small CLI verb in
[`ryeos-cli/src/local_verbs.rs`](../../ryeos-cli/src/local_verbs.rs)
backed by a function in `ryeos-tools/src/actions/trust.rs`. They all
operate on the same operator-tier directory
(`<user>/.ai/config/keys/trusted/`) and share the existing
`pin_key` / `validate_existing_pin` plumbing in
[`ryeos-engine/src/trust.rs`](../../ryeos-engine/src/trust.rs).

**Cost.** Small per verb. Add when one of them shows up in a real
support ticket / operator feedback session.

---

## 4. Managed admission policy for fleets

**What it is.** Bundle installation today is interactive: a human
operator runs `rye trust pin` once per publisher, then
`rye bundle install` per machine. At fleet scale (dozens or hundreds
of nodes) this is impractical.

Managed admission means: a single declarative policy says "trust
publishers `{A, B, C}`; allow installation of bundles
`{X, Y}` from those publishers; refuse anything else", and every
node enforces it on `bundle.install` without per-node operator
interaction.

**Trigger.** Either:
- Non-interactive bundle installation across more than 5–10 nodes.
- Organizational requirement for centralized trust admission (audit
  trail, separation of duties).

**Sketch.**
- New kind: `kind: trust-policy` — a signed doc declaring
  `{allowed_publishers: [<fp>], allowed_bundles: [<name@version>],
  default: deny|allow, expiry, signed_by: <admin_fp>}`.
- Operator pins the *admin* key only. Everything else is implicit
  via the trust-policy doc.
- `bundle.install` reads the active policy and enforces both
  publisher-fp ∈ allowed AND bundle-name ∈ allowed.
- Distribution: `service:bundle/install` accepts a policy URL or
  fetches the latest signed policy from a known location.
- Revocation: a newer-timestamp policy supersedes the old one;
  expired policies fail-closed.

**Caution — overlap with existing doc.** This is essentially the
"registry-pinned publisher keys" model from
`signing-trust-advanced.md` §6 specialized to fleet management.
When picking either up, unify them under one design.

**Cost.** Large. New kind, new policy-eval logic, distribution
mechanism, expiry handling, admin-key bootstrap UX. Real engineering
project — only worth it for a real fleet driver.

---

## 5. Eliminate fingerprint-drift class entirely

**What it is.** Today the trust doc declares both a fingerprint and
a public key, and we check the fingerprint matches the key at every
load. This is defense in depth — but it also means every place that
edits a trust doc must keep them in sync.

Long-term, the fingerprint should be derivable on demand from the
public key everywhere it's used; the on-disk doc keeps the public
key as the source of truth, and the filename is the only place the
fingerprint appears (computed at write, checked at read).

The `~/.ai/config/keys/signing/public_key.pem` sidecar is the same
class of redundancy — it's just a cache of `derive(private_key)` and
can drift. Removing it eliminates a whole bucket of "why does my
fingerprint not match?" support questions.

**Trigger.** A drift bug actually bites a user. Or just operator
patience runs out for the existing fingerprint-mismatch support
churn.

**Sketch.**
- Drop the `fingerprint = "..."` line from new trust docs (kept for
  backward compat in load).
- Drop the `public_key.pem` sidecar; have all readers derive from
  `private_key.pem`.
- Migration: one-time `rye trust normalize` that rewrites old docs
  in the canonical compact form.

**Cost.** Small once we accept the migration step. Defer until
either drift bites or a clean breaking-change window opens.

---

## 6. When to revisit

Re-read this doc when any of the following becomes true:

- A platform release ships with drifting `PLATFORM_AUTHOR_*`
  constants. → §1.
- A real key compromise or rotation requirement appears. → §2.
- Operators ask for `rye trust list` / `verify-doc` /
  `import-pointer` in support / docs feedback. → §3.
- More than ~5 nodes need to be configured to install the same
  bundles non-interactively. → §4 (and §6 of
  `signing-trust-advanced.md`).
- A fingerprint-drift bug bites a user, or we have a clean break
  window for the public-key-sidecar removal. → §5.

Until then, the current pin-on-init + pin-by-hand model is enough.
