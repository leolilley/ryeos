# Contained-workflow installed qualification

This directory is an evidence-validation driver, not a deployment harness. It
does not create OCI containers, cgroups, accounts, volumes, protected bindings,
or credentials. Those operations belong to a separately installed and reviewed
Lillux host adapter selected by the administrator.

After the adapter has completed the disposable-host matrix and written its
bounded JSON evidence file, validate it with:

```text
tests/e2e/contained-workflow/qualify.py /absolute/path/installed-evidence.json
```

The command succeeds only for the `ryeos-contained-workflow` image and
`contained-workflow` profile contract when every required capability, refusal,
and named host observation is present. Structural image smoke or source tests
must use their own claim class and will intentionally return a nonzero status.

No live host adapter is currently selected in this repository. Therefore this
driver cannot yet produce installed evidence and its presence is not an
installed-qualification claim.
