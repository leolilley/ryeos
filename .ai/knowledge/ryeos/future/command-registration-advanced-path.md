<!-- ryeos:signed:2026-06-04T06:46:50Z:b1a754551351b2b42e287807abe6c1e9fe3f3561cf0b8e84842d56795eb48849:dv+BUZ18oQaRyBzQUeyVVFDD3QP4CG+cjZCDrfHSyQenyRRsGI9az+MQZpMotq8YJmWRUFJYfnNgYOOJtWlgCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Future: Command Registration Advanced Path

## Status

Deferred. The current command registration implementation should stay simple:

```text
descriptor structure → derived claims → node-owned policy → source grants → admitted commands
```

This note records the advanced path if RyeOS later needs richer policy delegation, signer roles, dynamic grants, or lifecycle command migration. Do not implement these pieces until a concrete product or operator requirement appears.

## Current baseline

The current model is intentionally narrow:

- `node/commands` owns CLI syntax and dispatch intent.
- Rust derives structural registration claims from command descriptors.
- A single node-owned `command_registration` policy maps claims to required registration caps.
- Signed node-owned bundle registrations grant `command_registration_caps` to commands loaded from that bundle root.
- `ryeos init` verifies publisher-signed source seed data, then materializes node-owned policy signed by the node key.
- Runtime loading requires the installed command registration policy to be signed by the node identity.
- Normal bundles may not ship `.ai/node/command_registration`.
- Local lifecycle verbs remain a bootstrap carve-out: they are the minimum needed to start, stop, and initialize RyeOS before descriptor dispatch is available.

This is enough for the current security boundary. The advanced path should not be used to reintroduce `node/verbs`, bundle-name branching, descriptor self-authorization, or legacy compatibility refs.

## When to revisit

Reach for the advanced path only when at least one of these becomes real:

1. Operators need to grant or revoke protected command registration caps after init without hand-editing node config.
2. Multiple trusted publishers or organizations need distinct roles such as platform publisher, bundle publisher, node operator, and node identity.
3. A bundle wants to request protected command registration rights, but approval must remain operator/node-owned.
4. Command registration policy must update without re-running init or restarting the daemon.
5. Source-local command descriptors become first-class and need explicit trust/admission UX.
6. Lifecycle/local commands need to become discoverable, auditable, and governed by the same descriptor policy.
7. Audit/security workflows need a non-CLI operation taxonomy separate from command syntax and executable item refs.

## Advanced design direction

### 1. Signer roles in trust metadata

Today trust is mostly “trusted signer or not.” If multiple authority domains matter, introduce explicit signer roles in trust entries:

```yaml
fingerprint: <fp>
owner: ryeos-platform
roles:
  - platform_publisher
  - seed_publisher
```

Possible roles:

| Role | Purpose |
| --- | --- |
| `node_identity` | Signs materialized node-owned config and runtime state. |
| `node_operator` | Approves local policy/grant changes. |
| `platform_publisher` | Publishes official bundles and source seed data. |
| `bundle_publisher` | Publishes ordinary bundles. |
| `seed_publisher` | Publishes install-time seed policy and grant intent. |

Each node-config section can then declare the signer role it accepts. For example, `command_registration` should accept only node identity at runtime, while source seed files can accept platform/seed publishers during init.

Do not encode these roles as Rust privilege classes such as `PlatformCore`. Roles belong in trust/policy data and generic verifier logic.

### 2. Seed intent versus materialized node config

Keep the two-stage boundary:

```text
publisher-signed seed intent
        │ verified by init
        ▼
node-signed materialized node config
        │ loaded by runtime
        ▼
effective command registry
```

Seed files should express desired initial policy/grants. Runtime should consume node-owned config, not publisher seed directly.

If the seed format grows, keep it clearly separate from normal bundle content:

```text
<source-root>/.ai/node/command_registration/default.yaml
<source-root>/.ai/node/bundle_registration_grants/default.yaml
```

Normal bundles should continue to be unable to ship effective command registration policy.

### 3. Operator-managed grant commands

If post-init grant changes become necessary, add explicit node-admin commands/services that edit node-owned bundle registration records:

```text
ryeos command-registration grant <bundle> <cap>
ryeos command-registration revoke <bundle> <cap>
ryeos command-registration policy show
ryeos command-registration grants list
```

These commands should:

1. resolve the target installed bundle registration;
2. validate requested caps against current policy vocabulary;
3. write a node-signed bundle registration update;
4. trigger safe reload/restart if dynamic registry reload exists;
5. audit who/what made the grant change.

The bundle may provide a non-authoritative request document later, but the grant must be operator/node-owned:

```yaml
# bundle-authored request only, not authority
requested_command_registration_caps:
  - ryeos.register.command.root.example
```

The node-admin approval flow decides whether to materialize those caps into signed node config.

### 4. Policy layering only if needed

The current implementation intentionally uses one effective command registration policy. If policy composition becomes necessary, use explicit layering with deterministic merge rules:

```text
base platform policy
        +
node operator policy overlay
        +
temporary emergency deny overlay
        =
effective policy
```

Prefer deny-by-default and explicit conflict handling. Do not allow bundles to contribute authority layers.

Possible future record:

```yaml
section: command_registration
name: operator-overlay
layer: operator
priority: 100
claim_rules:
  - claim:
      kind: command.root
      value: remote-admin
    required_caps:
      - ryeos.register.command.root.remote-admin
```

Do not add layering until a single policy file is insufficient.

### 5. Dynamic reload and revocation

If command registration changes at runtime, registry reload must be safe and observable:

- reject invalid policy/grant changes before swapping the active registry;
- build the new registry off-thread or in a staged transaction;
- atomically swap the effective registry;
- keep old registry active on failure;
- emit audit and telemetry events;
- make revocation semantics explicit for already-running commands.

Until this exists, restart/re-init is acceptable for policy changes.

### 6. Source-local command admission

`CommandOrigin::SourceLocal` exists for diagnostics/future extension, not as privilege. If source-local commands become real, add an explicit admission model:

- project/user trust controls which source-local descriptors are even considered;
- node policy maps source-local claims to required caps;
- source-local grants come from operator-owned config, not from project files;
- UX clearly explains when local source can shadow or extend CLI commands.

Do not let project-local descriptors self-grant protected roots or dispatch kinds.

### 7. Descriptor-backed local handlers

Local lifecycle verbs are currently accepted as a bootstrap carve-out. If they need to become governed by command policy, migrate them carefully:

1. define `dispatch.kind: local_handler` descriptors for lifecycle commands;
2. keep a tiny pre-init bootstrap path for `init` if needed;
3. make local handler implementations keyed by descriptor handler names, not root-token matching;
4. protect `local_handler` dispatch with command registration policy;
5. remove lifecycle root matching only after installed descriptors can reliably boot the system.

This is a separate project. Do not mix it into command registration policy unless the hardcoded lifecycle carve-out becomes a real problem.

### 8. Optional `node/operations`

If RyeOS later needs non-CLI audit/security taxonomy, introduce `.ai/node/operations` deliberately. Operations should not replace commands and should not resurrect verbs.

Use operations for stable audit labels or policy vocabulary only when item refs and command descriptors are insufficient:

```text
commands   = user-facing syntax and dispatch intent
operations = optional audit/security taxonomy
items      = execution authorization target
```

See `node-operations` future notes for details.

## Non-goals

- Do not reintroduce `node/verbs`.
- Do not add backwards compatibility refs for removed command surfaces.
- Do not branch on bundle names in code.
- Do not add `PlatformCore`, `EmbeddedCore`, or similar privilege enums.
- Do not let command descriptors author their own admission requirements.
- Do not let bundle manifests grant command registration authority.
- Do not add dynamic policy layers before a single node-owned policy file is insufficient.

## Implementation guardrails for later

Any future implementation should preserve these invariants:

1. Rust may derive structural facts; data decides policy.
2. Runtime consumes node-owned effective policy.
3. Publisher-authored seed/request data is verified input, not runtime authority.
4. Bundle-authored data may request capabilities but must not grant them.
5. Grants attach to sources through node-owned registrations.
6. Execution authorization remains based on final executable item refs and existing capability checks.
7. Bootstrap local lifecycle commands are a deliberate exception until descriptor-backed local handlers are worth implementing.
