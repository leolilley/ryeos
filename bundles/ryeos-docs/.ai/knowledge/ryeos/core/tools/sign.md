<!-- ryeos:signed:2026-11-14T11:51:10Z:a8d0327eed80500fc6d96f65fe1783788d40ab143da4b8a13cdd42fc532d761b:505KlNmlmCWvxWapnPge6fereBzhMR+UhK1iZ9ERxS0e6e0XdZCLaM7Hzd57sqAOkL3EuGt07j9zMR5ypJhPDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, sign, signatures, authoring]
version: "1.3.0"
description: Core signing and content-pin authoring reference.
---

# Sign and content-pin authoring

Invariant: the sign tool signs project/user items with the operator key; system bundle items are signed by publishers during bundle publish.

Signing runs in the CLI, in the operator's own process, whether the daemon is
healthy or stopped. The daemon holds no operator private key and exposes no
signing service; the CLI reads the operator key from its own key directory and
verifies items against the node's registered kind, parser and trust generation.
Ordinary signing does not require a stop/start cycle.

```bash
ryeos sign <canonical-ref> --project <dir>
ryeos sign "tool:ryeos/core/*" --project <dir>
```

It supports canonical refs and glob batches in project space. Node/operator
state and system bundles are separate authoring authorities; bundle items are
signed by publishers.

The command is a local CLI handler. It performs no runtime launch or thread
recording and takes only the registered-generation read lock.
`tool:ryeos/core/sign` remains implementation vocabulary; it is not a separate
ambient signing authority.

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
    mount_root: project
    mount: data/training
```

Complete and sign the declaration atomically, in the CLI, whether the daemon is
healthy or stopped:

```bash
ryeos --project . content pin graph:training/run --id dataset
```

The command is a local CLI handler that signs with the operator key in its own
process. It constructs the node's registered parser, kind and trust generation
under the registered-generation read lock and applies the conservative
ingest-policy superset. It inserts the
observed digest, validates the completed item, and publishes one signed
replacement. It does not launch the item or publish CAS, thread, scheduler, or
runtime state. If a complete pin already names different bytes, pass `--update`
explicitly. Use `--all` to select every locator-backed declaration in one
transaction.

`mode: captured` has different meaning: it authorizes each admitted execution
to capture the then-current bytes. It is not a temporary spelling for pinned
intent and does not need a promotion run.
