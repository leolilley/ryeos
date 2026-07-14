<!-- ryeos:signed:2026-07-14T02:11:07Z:3fcc0190530e62ac916182e7bb11097d42fff3c4e56384911660a4b2c1d978de:B+ZgDhvI74EKAgsUMKxYJIp+8ogVs0RrVnBce6yjFNcNEAEwvSmftUb9329+iYCMy5CFzl3p9Yt+oXoI8luvAA==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
# Future: Tool Runtime Authority Model

## Status

Not a new runtime binary yet. The current domain-events branch implements the first safe slice: direct tool executions may receive only exact, self-bundle domain-event callback capabilities derived from a signed bundle manifest.

This document records the full target model so future work does not accidentally turn tool metadata into a self-grant surface.

## Problem

Directive and graph executions already have a callback authority path:

```text
directive/graph item
  -> compose permissions
  -> policy_facts.effective_caps
  -> callback token effective_caps
  -> daemon-enforced runtime callbacks
```

Direct tool execution did not have an equivalent model. A direct subprocess tool received daemon callback environment variables, but its callback token carried empty `effective_caps`, so runtime APIs such as `ryeos_runtime.events.append(...)` correctly denied.

The missing concept is tool callback authority: what daemon APIs may a directly executed tool call, and who decides?

## Non-goals

- Do not let tool YAML declare arbitrary callback authority.
- Do not reuse `required_caps` as callback authority.
- Do not let runtime API callers provide or spoof `bundle_id`.
- Do not grant wildcards for domain events.
- Do not introduce cross-bundle domain-event access in the default direct-tool path.
- Do not use unsigned `manifest.source.yaml` as runtime authority.

## Current branch implementation

For this branch, direct tool domain-event authority is intentionally narrow:

1. The daemon resolves and verifies the direct tool item.
2. The executor derives `effective_bundle_id` from the verified canonical tool ref.
3. The executor locates the resolved tool's containing `.ai` directory.
4. The executor reads signed generated `.ai/manifest.yaml` only.
5. If the signed manifest declares domain events, `manifest.name` must match the derived `effective_bundle_id`.
6. The executor mints only exact caps for declared event kinds and operations:
   - `ryeos.append.domain_events.<bundle>/<event_kind>`
   - `ryeos.scan.domain_events.<bundle>/<event_kind>`
7. The daemon runtime API still derives bundle identity from the callback token and enforces those exact caps.

Missing or empty manifest declarations produce no caps. That is deny-by-default, not backward compatibility authority.

## Manifest shape

Bundle manifests may declare domain-event authority like this:

```yaml
domain_events:
  - event_kind: email_event
    operations: [append, scan]
  - event_kind: suppression_event
    operations: [append, scan]
```

The manifest declares only event kinds and operations. It does not declare the bundle namespace for the cap. The namespace comes from the verified executing tool ref and must match `manifest.name` when declarations are non-empty.

## Why not tool `permissions`?

For directives and graphs, permissions compose through an execution item that controls downstream callbacks. Tools are the executable code itself. A generic tool `permissions:` block would let the code being executed request arbitrary daemon authority:

```yaml
permissions:
  - ryeos.*
```

Unless paired with a separate delegation/narrowing model, that is self-grant. The branch deliberately avoids it.

## Why not `required_caps`?

`required_caps` gates whether a caller may launch a tool. It is a launch authorization requirement, not authority granted to the running subprocess.

Using `required_caps` as callback caps would confuse two directions of permission flow:

```text
caller -> may launch tool     required_caps
tool   -> may call daemon     callback effective_caps
```

Those must stay separate.

## Future full model

The eventual `ToolAuthority` model should separate three concerns:

```text
execution mechanism       = subprocess / native runtime / streaming runtime
authority derivation      = signed manifest + install policy + caller delegation
callback enforcement      = daemon callback token effective_caps
```

A future managed tool runtime may standardize launch behavior, sandboxing, streaming, cancellation, and resumability, but it should not itself be the source of authority. Authority should remain a daemon-side derivation from signed metadata and explicit delegation.

Any future per-tool sandbox profile must only narrow the immutable node-owned
policy. It cannot enable a disabled node boundary, add mounts/network access,
or override node limits.

Per-tool profiles are not a substitute for hosted workload isolation. The
current inner boundary deliberately does not provide CPU/memory/process cgroup
quotas, cross-principal PID/signal isolation, or immutable closure capture for
all transitive imports and assets. Hostile workloads still require a cgroup and
a VM, microVM, or dedicated outer worker together with principal-scoped data,
secrets, network, accounting, and cleanup. The existing shared launch boundary
is what lets those future layers be added once, beneath every tool/runtime path.

## Future triggers

Revisit a full managed tool-runtime authority design when direct tools need any of:

- generic callback APIs beyond self-bundle domain events;
- cross-bundle domain-event access;
- caller-delegated caps narrower than the caller but broader than self-domain-events;
- user-visible approval grants;
- per-tool sandbox profiles tied to authority;
- nested tool execution authority;
- long-lived or resumable tool sessions;
- install-time namespace ownership and revocation.

## Future guardrails

- Tool callback authority is always minted by the daemon, never accepted from runtime input.
- Runtime APIs should continue to reject caller `bundle_id` unless an explicit cross-bundle delegation model exists.
- Signed manifest declarations should be exact and enumerable, not wildcard based.
- Bundle namespace binding must be explicit and auditable.
- Direct/local dev mode may offer test helpers, but production callback authority should come from signed generated manifests.
