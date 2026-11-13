<!-- ryeos:signed:2026-11-13T10:01:44Z:4d3401dca28e4f3dc51c6fca2ed7b2f2d1bad710a4863f728fdf52f5da4e44b4:zAuSFdryUIorIwVgrsD2weSx1liyvnj+vIOxVKna03u6uwzWddC5Mq0ZGtrQcEkJNrqhZzS+YK4HmhCRNyoABg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, external-execution, runtime-snapshot, qualification, recompose, operator]
version: "1.0.0"
description: >
  Operator procedure for runtime snapshot bindings: the three bindings a
  snapshot needs, and how to bring them back in line with the installed
  bundles after a payload change.
---

# Runtime Snapshots

A runtime snapshot is produced and qualified through three node-signed
bindings, each composed on a stopped node (see the
[CLI Reference](cli-reference.md) for every flag):

| Binding | Section | Composed by | Pins |
|---|---|---|---|
| `render-snapshot` | `runtime_snapshot_production` | `snapshot-compose` | lifecycle adapter generation, snapshot production spec, provider scope, credential generation |
| `render-snapshot-qualification` | `runtime_snapshot_qualification` | `snapshot-qualification-compose` | the production binding's id and digest, provider spec, restoration verifier, consumer verifier selections |
| `codex-qualification-session` | `external_execution` | `qualification-session-compose` | lifecycle and provider executables, the snapshot id, the qualified runtime, credential generation |

The ids are the operator's; the names above are the convention these docs use.
Name a binding after what it does, never after a campaign stage.

`ryeos node doctor` checks every pin offline in its `external_bindings` row.

## After a payload change

Installing a new adapter, restoration verifier, consumer verifier or provider
executable (`ryeos init`, `ryeos node reset policy-generation`,
`ryeos bundle install --replace`) leaves the bindings pinning the old ones.
The next produce, qualify or launch through them would be refused. Bring them
back in line in this order:

1. **Settle open work first.** Terminate every open qualification occurrence
   and bootstrap source made under the bindings you are about to recompose,
   and let in-flight snapshot operations finish
   (`ryeos external-content terminate-runtime-snapshot-qualification`,
   `ryeos external-content terminate-bootstrap-runtime-source`). Their
   termination looks up the binding they were made under by digest; once it is
   recomposed, that lookup fails and the occurrence is left to its provider
   timeout. Recompose does not check this for you yet.
2. **Doctor.** With the node stopped, run `ryeos node doctor` and read the
   `external_bindings` findings: each names the binding, the pinned field and
   what is installed.
3. **Derive consumer selections, if any.** When the qualification binding
   selects consumer verifiers, start the node, run
   `ryeos external-content derive-consumer-qualification-context`, save the
   response's `consumer_verifier_selections` map (scenario id to selection) as
   a JSON file, and stop the node again. The scenario source
   digest is only known to a running node.
4. **Recompose.**

   ```bash
   ryeos node external-execution recompose --dry-run
   ryeos node external-execution recompose [--consumer-verifiers selections.json]
   ```

   Production bindings are re-pinned first, then the qualifications that name
   them, then placements. Every operator-stated fact and every credential
   generation is carried, so no secret is provisioned again. A refusal is
   typed and publishes nothing; `recompose_settings_schema_changed` means the
   adapter's settings changed shape and the binding must be composed again
   with `compose`.
5. **Start** the node and run `ryeos node doctor` again: the
   `external_bindings` row is clean.
6. **Produce and qualify** a new snapshot. A snapshot produced under the old
   producer cannot be qualified under the new one; the qualification-session
   binding keeps naming its snapshot until it is composed again with
   `qualification-session-compose --snapshot-id <new>`. Recompose never picks
   a snapshot.

## Credentials

A credential is keyed by the node, backend, account and generation. Recompose
keeps all four, so the secret already provisioned stays valid. A fresh
`snapshot-compose`, `compose` or `qualification-session-compose` names a new
generation, and its secret must be provisioned with
`ryeos node external-execution provision-credential` before the node uses it.
