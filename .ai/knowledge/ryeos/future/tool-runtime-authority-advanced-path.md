<!-- ryeos:signed:2026-07-14T10:12:37Z:3af1d2388d4d85b663d571f87883d86ebdffb9189fe7efb3a31c1e54f778e7f8:sW8cI6bR2FMFMrrIlAbwzc7gicpWnR+ePQIqi6QB1GJR38d3VbdjIVAGA2MtFd8UiANrFhun78qXAVLF+/hMAQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
# Future: Tool Runtime Authority Advanced Path

## Status

Deferred. The current implementation intentionally provides only the narrow
first slice: direct tool executions may receive exact self-bundle callback
capabilities derived from signed generated bundle manifests.

The signed `tool_callback` descriptor makes callback transport explicit;
authority remains separately derived and deny-by-default. This advanced path is
for the broader question: what should a first-class tool runtime authority model
look like once tools need more than current self-bundle authority?

## Why this is deferred

The immediate problem was specific and bounded:

- Tools needed bundle-event, runtime-vault, and item-authoring callbacks.
- The daemon already enforced those callbacks by callback-token capabilities.
- Direct tool subprocesses had no callback caps.
- Granting generic tool `permissions` or reusing `required_caps` would create a self-grant footgun.

So the safe solution was narrow manifest-derived caps, not a complete tool
authority system.

## Trigger for revisiting

Reopen this design when direct tools need one or more of:

- callback access to daemon APIs beyond current self-bundle authority;
- cross-bundle bundle-event or vault delegation;
- caller-delegated capabilities narrower than the caller but broader than self-bundle authority;
- user-visible approval grants for tool authority;
- install-time namespace ownership and revocation;
- per-tool sandbox profiles tied to authority;
- nested tool execution authority;
- long-lived, resumable, or cancellable tool runtime sessions;
- a real managed tool runtime binary for launch standardization.

## Core principle

Do not conflate execution mechanism with authority.

```text
execution mechanism       = subprocess / native runtime / streaming runtime
authority derivation      = signed manifest + install policy + caller delegation
callback enforcement      = daemon callback token effective_caps
```

A managed tool runtime can standardize process behavior, but it should not be the source of permission. Authority should be minted by the daemon from signed metadata and explicit delegation.

Likewise, any future per-tool sandbox profile is an intersecting restriction
beneath the immutable node policy, never item-authored authority to enable or
broaden mounts, network, environment, or limits.

The profile should name typed requirements rather than an OS backend. The node
selects and proves the backend using the contract in
`ryeos/future/data-driven-execution-isolation-backends`; an unavailable required
boundary fails closed instead of falling back to direct execution.

That profile remains an inner application-policy layer. It must not be used as
the claim that hostile multi-tenancy is complete: CPU, memory, and process-count
budgets belong to an outer cgroup/worker controller; hostile workloads require a
VM, microVM, or dedicated worker boundary; and principal-scoped storage,
secrets, networking, audit, and cleanup remain hosted-node concerns. The
node-owned sandbox provides the stable launch handoff where those later layers
can be attached. See `ryeos/future/hosted-node-trust-boundaries` for the layered
completion model.

## Authority model sketch

The eventual model should introduce a daemon-side `ToolAuthority` derivation step:

```text
verified root tool ref
        │
        ▼
signed bundle manifest / install policy
        │
        ▼
caller/session delegation constraints
        │
        ▼
ToolAuthority grant set
        │
        ▼
callback token effective_caps
```

The grant set should be explicit and auditable. It should never be accepted from subprocess input.

## Possible signed manifest shape

Future manifests may grow from the current typed `runtime_authority` blocks:

```yaml
runtime_authority:
  bundle_events:
    - event_kind: email_event
      operations: [append, scan]

tool_authority:
  callbacks:
    - api: artifacts.publish
      operations: [create]
    - api: commands.claim
      queue: ryeos-email-send
```

Important: this should still describe bounded resource authority, not arbitrary capability strings. The daemon should translate structured declarations into exact capabilities.

## Delegation and narrowing

When caller delegation is added, grants should be the intersection of:

1. signed bundle/tool declarations;
2. install policy for that bundle namespace;
3. caller/session capabilities;
4. launch-mode restrictions;
5. optional user approval grants.

No layer should be able to broaden authority alone.

## Direct tools vs managed tool runtime

The advanced model should support both direct subprocess tools and a future managed tool runtime:

```text
direct subprocess tool
  -> daemon derives ToolAuthority
  -> daemon mints callback token
  -> daemon spawns subprocess

managed tool runtime
  -> daemon derives ToolAuthority
  -> daemon mints callback token
  -> runtime supervises/streams/sandboxes actual tool execution
```

The same authority derivation should feed both paths.

## Guardrails

- Do not make tool YAML `permissions` a generic self-grant surface.
- Do not treat `required_caps` as runtime callback authority.
- Do not accept caller-provided `effective_caps` or `bundle_id`.
- Do not grant wildcard callback caps from manifest declarations.
- Do not use unsigned `manifest.source.yaml` as production runtime authority.
- Do not silently allow cross-bundle access; model it as explicit delegation.
- Keep daemon runtime APIs deriving identity from verified callback context.

## Migration path

1. Keep the current self-bundle manifest-authority slice as the baseline.
2. Add structured manifest declarations only when a new callback API needs them.
3. Add install-policy checks before any cross-bundle or new callback authority.
4. Add caller/session delegation as an intersecting constraint, not a replacement for signed declarations.
5. Only then introduce a managed tool runtime if execution behavior, not authority, needs centralization.
