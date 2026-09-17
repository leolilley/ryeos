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

No live host adapter is currently selected in this repository. Therefore this
driver cannot yet produce installed evidence and its presence is not an
installed-qualification claim.
