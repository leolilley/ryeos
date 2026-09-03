<!-- ryeos:signed:2026-08-17T23:06:01Z:5bcd5dc9a93c16cb4b9e68dc947237dd361dfa8c8e15e4f8725673d202017d91:FNzhBRG51oMi1Dakz7l84Nsrw9YJg7Jlf8+P+9Lm2lklm9K+FDuH5n2i6gzMSCLPy71KUas3i9GCqtBMAIbADA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, sign, signatures, authoring]
version: "1.3.0"
description: Core signing and content-pin authoring reference.
---

# Sign and content-pin authoring

Invariant: the sign tool signs project/user items with the operator key; system bundle items are signed by publishers during bundle publish.

Sign availability: **both**. With a healthy local daemon, the command uses its
non-threaded local-operator service. With the node stopped, the same service is
constructed standalone under exclusive node-state ownership. Ordinary signing
does not require a stop/start cycle.

```bash
ryeos sign <canonical-ref> --project <dir>
ryeos sign "tool:ryeos/core/*" --project <dir>
```

It supports canonical refs and glob batches in project space. Node/operator
state and system bundles are separate authoring authorities; bundle items are
signed by publishers.

`service:sign` owns both modes. Its daemon path authenticates the exact
configured local operator and performs no runtime launch or thread recording.
Its standalone path uses the same handler only after proving the daemon does
not own node state. `tool:ryeos/core/sign` remains implementation vocabulary;
it is not a separate ambient signing authority.

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

Complete and sign the declaration atomically, whether the daemon is healthy or
stopped:

```bash
ryeos --project . content pin graph:training/run --id dataset
```

The command is the dual-mode, non-threaded `service:content/pin`, restricted to
the configured local operator. A live daemon supplies its exact registered
parser, kind, trust, and ingest-policy generation; a stopped node constructs
that same registered state under the exclusive node lock. It inserts the
observed digest, validates the completed item, and publishes one signed
replacement. It does not launch the item or publish CAS, thread, scheduler, or
runtime state. If a complete pin already names different bytes, pass `--update`
explicitly. Use `--all` to select every locator-backed declaration in one
transaction.

`mode: captured` has different meaning: it authorizes each admitted execution
to capture the then-current bytes. It is not a temporary spelling for pinned
intent and does not need a promotion run.
