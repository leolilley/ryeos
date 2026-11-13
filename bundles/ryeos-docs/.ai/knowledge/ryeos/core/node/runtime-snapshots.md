<!-- ryeos:signed:2026-11-13T21:39:36Z:4292514865b17ed910b493a3c8d951266e552fb6cfabc0a9b71f387021e4dd40:2L7YaBrsJuxYpC2opT9m8lWpHvqYKIkNC3ZkEFn9Aq20C3jOCxw7ATelTp8VQEWx7iHkOUuBu0aCOOR+i05ICg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, external-execution, runtime-snapshot, qualification, recompose, operator]
version: "1.1.0"
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

1. **Doctor.** With the node stopped, run `ryeos node doctor` and read the
   `external_bindings` findings: each names the binding, the pinned field and
   what is installed.
2. **Derive consumer selections, if any.** When the qualification binding
   selects consumer verifiers, start the node, run
   `ryeos external-content derive-consumer-qualification-context`, save the
   response's `consumer_verifier_selections` map (scenario id to selection) as
   a JSON file, and stop the node again. The scenario source
   digest is only known to a running node.
3. **Recompose.**

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
4. **Start** the node and run `ryeos node doctor` again: the
   `external_bindings` row is clean. Work opened before the recompose keeps
   the generation it was made under (see below) and shows in the
   `external_capacity` row until it settles.
5. **Produce and qualify** a new snapshot. A snapshot produced under the old
   producer cannot be qualified under the new one; the qualification-session
   binding keeps naming its snapshot until it is composed again with
   `qualification-session-compose --snapshot-id <new>`. Recompose never picks
   a snapshot.

## Open work keeps its generation

Recompose never waits on open work. Every qualification occurrence and
bootstrap source retains, from the moment it is reserved, the exact generation
it was made under: the signed production (and qualification) binding and a
protected copy of that producer's lifecycle artifacts (adapter, supervisor,
launcher, restoration verifier, provider and snapshot specs). Terminating
and cleaning up that work resolve through the retained generation,
never through what node config or the installed bundles pin now, so an
occurrence started before a recompose is still terminated with the adapter
that created it (`ryeos external-content terminate-runtime-snapshot-qualification`,
and the daemon's recovery pass for bootstrap sources). External placements
keep their own retained binding generation the same way and are cleaned up
by the exact adapter generation, or by the one installed successor that reads
the same settings schema.

A retained generation is released when nothing open references it:

- a qualification once its termination is terminal, or, if it never bound an
  occurrence, once its attempt deadline plus its maximum lifetime has passed;
- a bootstrap source once its termination is terminal and no staged snapshot
  is still being created from it.

The release happens as the work settles, in the daemon's normal path: the
artifact copy stops being a CAS root, and the next scheduled maintenance GC
reclaims the bytes no other open operation still uses. The signed journal rows
themselves stay, like every other runtime journal row. Nothing needs an
operator command, and a restart changes nothing: what is retained is derived
from the journal each time.

While a superseded generation is retained, `ryeos node doctor` (running node)
lists it in the `external_capacity` row under `retained_generations`, with
its binding, the retained and current digests, and each operation still
referencing it. A running qualification occurrence makes the row fail until
it is terminated, since nothing on the node ends it otherwise; a bootstrap
source warns while recovery settles it.

## Credentials

A credential is keyed by the node, backend, account and generation. Recompose
keeps all four, so the secret already provisioned stays valid. A fresh
`snapshot-compose`, `compose` or `qualification-session-compose` names a new
generation, and its secret must be provisioned with
`ryeos node external-execution provision-credential` before the node uses it.
