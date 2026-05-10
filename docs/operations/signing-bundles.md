# Signing Bundle YAML — Dev Guide

> **TL;DR for impatient agents:** to re-sign one or more YAML files inside `ryeos-bundles/{core,standard}/`, run:
>
> ```bash
> ./scripts/populate-bundles.sh \
>   --key   .dev-keys/PUBLISHER_DEV.pem \
>   --owner ryeos-dev
> ```
>
> That re-builds binaries (idempotent) and re-signs **every** signable item in both bundles. For files under `ryeos-bundles/{core,standard}`, do **not** use per-item signing — use `populate-bundles.sh` / `ryeos publish` at bundle granularity. `ryeos sign` exists for other item trees (project items, user-space directives) but is **not** the right tool for the bundled dev tree. `ryeos publish <bundle-dir> ...` is the underlying primitive if you only want one bundle.

This doc exists because LLMs and humans repeatedly get stuck on "how do I sign this YAML I just edited?" The answer is short and unsatisfying: **sign at the bundle granularity, not the file granularity**, and use the dev publisher key in `.dev-keys/`.

---

## 1. The mental model

### What "signed" means here

Every YAML file under `ryeos-bundles/<bundle>/.ai/` that is loaded through `VerifiedLoader` carries an inline `# ryeos:signed:...` header. The header is an Ed25519 signature over the file's content hash, plus a publisher fingerprint. When the runtime loads the file, it:

1. Strips the signature line.
2. Hashes the remaining content.
3. Verifies the signature against the fingerprint's pubkey from the publisher trust store.
4. Either accepts (signer is trusted), warns (signer is unknown but signature is valid), or fails (signature is invalid or absent depending on policy).

You **cannot** edit a signed YAML file and re-use the old signature line. Any byte change invalidates it. You must re-sign.

### What "publisher" means

A publisher is whoever owns the private key. The private key signs items; the corresponding **publisher trust doc** (`PUBLISHER_TRUST.toml`) declares the matching pubkey + fingerprint + owner label. Operators "pin" trust docs to decide which publishers their daemon trusts.

For development the publisher is `ryeos-dev` and the keypair lives in `.dev-keys/`. The private key is intentionally checked into version control — these keys must NEVER be trusted in production.

### What `ryeos publish` actually does

`ryeos publish <bundle-dir> --key <pem> --owner <label>` does five things:

1. Walks every signable item under `<bundle-dir>/.ai/` (directives, tools, configs, knowledge).
2. Re-hashes each, builds a fresh `# ryeos:signed:...` header, and rewrites the file in place.
3. Rebuilds the bundle's CAS manifest (objects + refs under `.ai/objects/` and `.ai/refs/`).
4. Emits `<bundle-dir>/PUBLISHER_TRUST.toml` declaring the pubkey + fingerprint + owner label (unless `--no-trust-doc`).
5. Prints a JSON report of what it touched.

**You do not sign individual files.** `ryeos publish` operates on the whole bundle.

---

## 2. The one command you actually run

```bash
./scripts/populate-bundles.sh \
  --key   .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

This wraps `cargo build --release` for the workspace binaries, stages them into `ryeos-bundles/{core,standard}/.ai/bin/<triple>/`, then runs `ryeos publish` on both bundles.

**When to use it:**
- After editing any YAML under `ryeos-bundles/`.
- After a fresh checkout (binaries are `.gitignored`; tests need them).
- Before running tests that read the signed bundle tree (`tests/test_support.rs`, golden-wire-real, etc).
- Before `docker build` if you want to skip the in-image cargo step.

It is idempotent. Safe to re-run.

---

## 3. When you don't need `populate-bundles.sh`

If you only edited YAML in **one** bundle and don't need to rebuild binaries, you can call `ryeos publish` directly:

```bash
# Make sure the ryeos CLI binary is built once:
cargo build --release -p ryeos-cli

# Re-sign just the standard bundle:
./target/release/ryeos publish ryeos-bundles/standard \
  --registry-root ryeos-bundles/core \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

`--registry-root` points at whichever bundle supplies the kind schemas + parsers (`ryeos-bundles/core` for everything else). When publishing `core` itself, the flag can be omitted; the bundle source IS the registry root.

---

## 4. Common mistakes & how to recover

### "I edited one YAML file, can I sign just that file?"
**No.** There is no per-file signing CLI. Run `populate-bundles.sh` (or `ryeos publish` on the bundle that contains the file). Re-signing the whole bundle is fast and safe.

### "I see a stale comment that says `cargo run --example resign_yaml -p ryeos-engine`"
That example was deleted in V5.2-CLOSEOUT. The comment in [`ryeosd/src/bootstrap.rs`](file:///home/leo/projects/ryeos-cas-as-truth/ryeosd/src/bootstrap.rs#L291) is wrong. Use `ryeos publish` or `populate-bundles.sh`. (If you're cleaning, fix the comment while you're there.)

### "The runtime says my signature is invalid"
You probably edited the file after signing. Re-run the publish step.

### "The runtime says signed by unknown signer"
The trust store doesn't contain the publisher fingerprint. Two options:
1. Pin the dev trust doc:
   ```bash
   ./target/release/ryeos trust pin --from .dev-keys/PUBLISHER_DEV_TRUST.toml
   ```
2. Or run the daemon in dev mode where unknown-signer accepts with a warning (current behavior).

### "I get a hash mismatch only inside Docker / inside CI"
Almost always means the bundle wasn't re-published after the edit. CI doesn't re-sign for you. Either commit the re-signed YAML, or run `populate-bundles.sh` as part of the build step.

### "I want to sign with my own production key, not the dev key"
Use the same commands but point `--key` at your real PEM and `--owner` at your label. **Never** check the production private key into version control.

---

## 5. Verification: did the signing actually work?

After re-signing, confirm the YAML headers updated:

```bash
head -1 ryeos-bundles/standard/.ai/config/ryeos-runtime/model-providers/zen.yaml
# Expect: # ryeos:signed:<timestamp>:<hash>:<sig>:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
```

The trailing fingerprint must be `741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea` (the dev publisher). If it isn't, you signed with the wrong key.

To prove the runtime accepts the file, run any test that loads it through `VerifiedLoader`:

```bash
cargo test -p ryeos-runtime verified_loader 2>&1 | tail
```

Or run a smoke test against the daemon:

```bash
./scripts/smoke-execute-stream.sh
```

---

## 6. Reference: file layout

```
.dev-keys/
  PUBLISHER_DEV.pem          # private Ed25519 (PKCS#8 PEM) — dev only
  PUBLISHER_DEV_TRUST.toml   # pubkey + fingerprint + owner label

ryeos-bundles/
  core/
    .ai/
      config/        # signed YAML
      tools/         # signed YAML + scripts
      directives/    # signed Markdown with YAML frontmatter
      knowledge/     # signed Markdown
      bin/<triple>/  # binaries (gitignored, rebuilt by populate-bundles.sh)
      objects/       # CAS objects (regenerated by ryeos publish)
      refs/          # CAS refs (regenerated by ryeos publish)
    PUBLISHER_TRUST.toml  # emitted by ryeos publish
  standard/
    .ai/                  # same shape as core/
    PUBLISHER_TRUST.toml

scripts/
  populate-bundles.sh    # the canonical "build + sign everything" script
```

---

## 7. For agents handed a "re-sign these N YAML files" task

Do **not**:
- look for a `--file` or `--single` flag on `ryeos publish` (there isn't one)
- try to manually compute the signature header
- search for `resign_yaml` examples (deleted)
- write a one-off Python/Rust script to call into Ed25519 directly
- ask the user "which signing tool should I use?" — the answer is always `populate-bundles.sh`

Do:
1. Run `./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev`.
2. Verify the touched files have a fresh signature header (§5).
3. Move on.

If the script fails because the daemon isn't running or `cargo build` errors out, surface that to the user — those are real blockers. Signature mechanics are not.
