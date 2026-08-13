<!-- ryeos:signed:2026-08-13T07:32:15Z:005786380653e2cdc553eac4d9b12de86177198511b37a2d926ba88501f17dc6:uI9Jd6Xgk5aMoLGdGj5YJ/xw+NRklgvXzRokPHaA7dAHkkBs2VrQ9hVeIUgJBFVNo7H+sIKMK58kYrjjn1G/Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, sign, signatures, offline]
version: "1.2.0"
description: Core signing and content-pin authoring reference.
---

# Tool and Service: sign

Invariant: the sign tool signs project/user items with the operator key; system bundle items are signed by publishers during bundle publish.

Availability: **offline**. Stop the daemon before an authoring transaction so
the operator key cannot be reached through a live runtime dispatch.

```bash
ryeos sign <canonical-ref> --project <dir>
ryeos sign <canonical-ref> --project <dir> --source project
ryeos sign "tool:ryeos/core/*" --project <dir>
```

It calls `ryeos-core-tools sign` and supports the same canonical-ref and glob semantics as the CLI. The `--source` flag accepts only `project` (the default). Node/operator state and system bundles are separate authoring authorities; bundle items are signed by publishers.

Sign is both a tool (subprocess, `tool:ryeos/core/sign`) and a service (`service:sign`). The CLI dispatches it as an offline service descriptor.

## Completing an external-content pin

Write pinned intent directly in an unsigned project-item draft. A missing
digest is accepted only by the dedicated authoring command; ordinary signing,
validation, and execution refuse it.

```yaml
external_content:
  - id: dataset
    kind: tree
    locator:
      root: project_files
      path: data/training
    mode: pinned
    mount: data/training
```

With the daemon stopped, complete and sign the declaration atomically:

```bash
ryeos --project . content pin graph:training/run --id dataset
```

The command uses the same capture policy, bounds, and manifest construction as
admission, inserts the observed digest into the item, validates the completed
item through its registered kind and parser, and publishes one signed
replacement. It does not publish CAS content or create a thread. If a complete
pin already names different bytes, pass `--update` explicitly. Use `--all` to
select every locator-backed declaration in one transaction.

`mode: captured` has different meaning: it authorizes each admitted execution
to capture the then-current bytes. It is not a temporary spelling for pinned
intent and does not need a promotion run.
