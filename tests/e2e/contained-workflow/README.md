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
