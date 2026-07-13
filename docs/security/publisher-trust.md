# Publisher trust boundaries

RyeOS treats a `PUBLISHER_TRUST.toml` file as a pointer an operator may choose
to pin, never as authority derived from where the file was found. A bundle,
package, or container cannot make its own signing key trusted merely by
shipping that file beside its content.

`ryeos init` always pins the official publisher from public-key bytes compiled
into the CLI. Additional publishers enter the operator trust store only through
an explicit `--trust-file <PUBLISHER_TRUST.toml>` argument. Bundle preflight
then fails closed when the source signer is not pinned.

Deployment wrappers preserve the same boundary:

- Release containers pass no packaged trust documents to `ryeos init`.
- A development/custom container may opt in with
  `RYEOS_TRUST_BAKED_PUBLISHERS=1`.
- The local source installer may opt in with
  `--trust-source-publishers`. Without it, a non-official source fingerprint is
  rejected only after the document's decoded key and declared fingerprint agree,
  and before the daemon is stopped or the installed layout is changed.
- Test and smoke workflows pass an exact trust file when they intentionally use
  repository development signatures.

These opt-ins trust every publisher document in the named source boundary. Use
them only for source trees or images whose provenance you have independently
verified. Production and packaged release flows should use official-signed
artifacts and the compiled official trust root instead.
