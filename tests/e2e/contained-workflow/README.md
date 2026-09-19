# Contained-workflow installed qualification

This directory is an evidence-validation driver, not a deployment harness. It
does not create OCI containers, cgroups, accounts, volumes, protected bindings,
or credentials. Those operations belong to a separately installed and reviewed
Lillux host adapter selected by the administrator.

After the adapter has completed the disposable-host matrix and written its
bounded JSON evidence file, the checked-in refusal/shape driver can inspect it
with:

```text
tests/e2e/contained-workflow/qualify.py /absolute/path/installed-evidence.json
```

The checked-in command intentionally exits nonzero for installed evidence even
when every field is present, because the source config names no installed
attestor. Structural image smoke and source-contract tests run separately.
Installed success requires an administrator-signed replacement config and
verifier that name a concrete attestor and authenticate the canonical evidence
payload.

The repository now supplies an explicitly selected Docker runtime adapter and
administrator setup commands. See the
[adapter runbook](../../../crates/host-adapters/lillux-oci-hook/README.md).
They connect Docker lifecycle callbacks to the existing hook and initialize a
private contained node; they do not grant this evidence driver an installed
attestor. The lifecycle matrix and authenticated candidate-return observations
must still be collected and authenticated for the selected host.

Local development builds use `docker-bake.contained-dev.hcl` with explicit
`VERSION`, `VCS_REF`, `BUILD_DATE`, and `SOURCE_DATE_EPOCH` coordinates. Set
`CONTAINED_SOURCE` to a frozen checkout. This produces a local contained image
and hook archive in one solve using the repository's public development key;
it does not produce an official bundle archive or push registry images.

When preparing a disposable node from that immutable image ID, explicitly pass
`--trust-source-publisher` to `scripts/pkg/setup-contained-docker-node.py`.
This records consent to trust the pinned image's source publisher and supplies
its public trust document to initialization. The option is off by default and
must remain consistent on retries. It does not change the runtime adapter,
signed containment profile, or installed qualification requirements. Never use
the public development publisher for production nodes.
