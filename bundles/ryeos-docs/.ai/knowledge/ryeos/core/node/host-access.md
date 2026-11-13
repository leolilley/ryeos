<!-- ryeos:signed:2026-11-13T06:27:19Z:220acb6bcb702b0cd7ad5e9cca26d30bc7bbfb217ac2b710debbbb34465bb0cc:5KhNraa6xlKxnzdCEaBbS/H1FxAuttaFEqjp6LDQ/H54gaxrVKUxUVDnuDksS3w2l8oTWOfBKJOtWjtsXpyHBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, isolation, security, host-access, node-policy, operator]
version: "1.0.0"
description: >
  The host-direct execution posture: a tool that declares host authority runs
  on the host, unconfined, only when the node lane offers it, the operator
  granted that exact item, and the configured local operator asks.
---

# Host Access

Some development operations need the host itself: host PIDs and file locks,
`statvfs`, installed binaries, `docker`, `cargo` and its shared target dir.
A sandboxed run cannot be honest about them. The host-direct posture lets such
a tool run on the host as an admitted, resolvable, provenance-recorded RyeOS
item, under authority the operator grants explicitly.

**Host access is operator-equivalent authority.** A host-direct process runs
as the daemon user. It can read the operator's signing key, the node key, the
vault files and every bundle root. Grant it only to items you would run as the
operator yourself. The operator vault gate is not a boundary for such a
process: it can read the vault directly.

## Three owners, three facts

1. **The item declares it.** The tool kind projects `host_authority` from the
   item, beside `network_authority` and `filesystem_authority`. An item that
   authors `host_authority: required` asks for the host-direct posture. The
   declaration is frozen into the plan as `execution_posture: host_direct`.
   Declaring it grants nothing.
2. **The publisher's lane offers it.** Each isolation host class says
   `host_direct: unavailable | operator_granted`. Only the `development`
   profile's `local` and `local_direct` classes offer it; every other profile
   and class, `standard` included, says `unavailable`. See
   `ryeos/core/node/execution-isolation`.
3. **The operator grants the exact item** in the operator-owned `host_access`
   node-policy section, changed only by the operator-signed
   `node.policy-apply` operation (`ryeos node host-access grant|revoke`).

A launch runs host-direct only when all three agree **and** the caller is the
configured local operator.

## What the item may not also claim

The tool kind refuses `host_authority: required` together with any projection
that cannot be true on the host: `network_authority`, `filesystem_authority`,
`workspace_access`, `execution_endpoint`, `supported_target`,
`resource_authority`, `project_lane`, `external_content` and
`external_product_slots`. Its effect class may only be `live` (absent is
live), so no graph node can record it and replay never stands in for it. Its
protocol must be `protocol:ryeos/core/opaque`: no callback token, no child
launches, no structured callback surface.

## A grant

```yaml
host_access:
  schema: 1
  grants:
    - id: ryeos-development-guarded-cargo-run
      item: tool:ryeos/development/guarded-cargo/run
      signer: 741a8bc6...                 # 64 hex
      source_closure: 3f0c...             # 64 hex content-manifest digest, or null
      runtime:
        ref: tool:ryeos/core/runtimes/python/script
        content_hash: 9b1e...             # 64 hex
      origins: [direct, graph_child]      # direct | graph_child | schedule
      host_env: [CARGO_HOME, HOME, PATH, RUSTUP_HOME, XDG_RUNTIME_DIR]
      containment: scope_required         # scope_required | group_allowed
```

- The grant pins the exact bytes: signer, effective source-closure manifest
  and runtime. The development publisher key is public, so a grant keyed on
  the signer alone would admit anything anyone signs with it. Any edit to the
  item, anything in its source closure (for a Python tool, its whole
  `.ai/tools/<namespace>/` tree) or its runtime makes the grant stale: grant
  again, then restart.
- `origins` lists the launch roots that may reach the grant: `direct` (the
  operator runs it), `graph_child` (a node of a graph the operator rooted;
  the parent thread's profile must be one the tool kind lists) and `schedule`
  (an operator schedule fire). `schedule` is never implied.
- `host_env` names come from the **daemon's own boot environment**, not the
  caller's shell. Nothing else is inherited. Site Configs should use absolute
  paths. Secrets still arrive only through `required_secrets`.
- `scope_required` places the run in a cgroup process scope, so cancel and
  recovery reach every descendant; the lane must have `process_scopes:
  required`. `group_allowed` contains only the process group: a descendant
  that calls `setsid` survives cancel.
- The signer must be pinned in the node trust store and must not be the node
  key (runtime-authored items are node-signed and never gain host access).
- Every profile ships `{schema: 1, grants: []}`: nothing runs on the host.

A generation refuses grants on a lane that says `unavailable`, and
`scope_required` on a lane without process scopes.

## Who may run it

Only the configured local operator: a verified `local_client` request from
the operator key, with no remote target, no remote origin and no forwarding.
Whatever capabilities a caller holds (`ryeos.execute.tool.*`, `*`), every
other caller is refused: another principal, a browser session, a remote
operator or node, workload-client delegation, and any child launched from a
directive, worker or tool thread.

## What runs

No isolation adapter is applied, whatever the node's mode: the launch takes
the disabled-isolation path through a per-launch runtime view, and every other
launch on the node keeps the lane's isolation. The node-owned controls stay:
exact-digest delivery of the interpreter and source closure, the pre-exec
attachment hold, stdout and stderr caps, the timeout and subprocess limits.

A host process can write anything the daemon user can, so the only project
policies that can be true for it are a read-write live project
(`ryeos.write.project.live`) or projectless execution. A pinned or read-only
policy is refused (`host_access_project_refused`). The thread's live project
authority records `unconfined_host`.

The execute route checks before anything runs, and the executor checks again
right after launch trust, with the resolved runtime. A host-direct run is never
reconstructed or recovered: after a restart it refuses rather than running
again. Offline `ryeos` dispatch refuses host-authority tools.

## Provenance

Launch provenance gains `execution_posture`, omitted for confined launches so
their provenance stays byte-identical:

```yaml
execution_posture:
  host_direct:
    policy_generation: <node policy generation digest>
    grant_id: ryeos-development-guarded-cargo-run
    item: tool:ryeos/development/guarded-cargo/run
    signer: <64 hex>
    source_closure: <64 hex or null>
    runtime_ref: tool:ryeos/core/runtimes/python/script
    runtime_content_hash: <64 hex>
    origin: direct
    containment: scope
    host_env: [HOME, PATH]
```

## Commands

```bash
# Stop the node first; grants take effect at the next start.
ryeos node host-access grant tool:ryeos/development/guarded-cargo/run \
  --project ~/src/ryeos --origin direct --origin graph_child \
  --env HOME --env PATH --env CARGO_HOME --env RUSTUP_HOME --env XDG_RUNTIME_DIR \
  --containment scope_required
ryeos node host-access list --project ~/src/ryeos   # current or stale per grant
ryeos node host-access revoke ryeos-development-guarded-cargo-run
```

`grant` resolves the item in the project, prints the signer and digests it
pins, and installs the edited section through `node.policy-apply`.

`ryeos node doctor` shows `host_direct` in the `isolation` row and a
`host_access` row listing every grant. The row warns when a grant allows
group containment, and fails when a grant names an unpinned signer or the
node key.

## Refusals

Every refusal is a launch-policy error (HTTP 403) with a stable code:

| Code | When |
|---|---|
| `host_access_lane_unavailable` | The node's host class says `host_direct: unavailable` |
| `host_access_not_granted` | No grant names the item |
| `host_access_grant_stale` | A grant names the item, but its signer, source closure or runtime differs (both values are named) |
| `host_access_principal_refused` | The acting principal is not the configured local operator, or the request is not a verified local_client one |
| `host_access_delegation_refused` | A remote target, origin or forwarding, or a parent thread that is not an operator-rooted graph |
| `host_access_origin_not_granted` | The launch's origin is not in the grant's `origins` |
| `host_access_containment_unavailable` | `scope_required`, but the lane has no qualified scope provider |
| `host_access_signer_refused` | The item is unsigned or node-signed, or the grant's signer is not pinned |
| `host_access_project_refused` | A pinned or read-only project policy |

## Existing nodes

The `host_access` section, the lane field and isolation policy version 11 are
a policy schema cut. With the node stopped:

```bash
ryeos node reset policy-generation --node-profile <profile> --host-class <class> \
  --reset-section isolation --reset-section host_access --confirm
```

`host_access` arrives from the profile, empty. A version-10 `isolation` body
no longer validates, so it is taken from the profile too; an operator who had
applied their own isolation policy re-applies it in the version-11 shape
afterwards with `ryeos node policy-apply isolation <file>`. Nothing changes
silently.
