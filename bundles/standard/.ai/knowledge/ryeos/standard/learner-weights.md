<!-- ryeos:signed:2026-07-14T10:12:30Z:137c15ba76134a39c430473ce407f661f42bdeef46762195e95859a570594793:xfC04SAHabhFis24q3uS9Yi5IYhoe3GQcHyMo2aYL7pIVMaCWHwbfEjOMKgUI2bjMKNCcvdX5JOg/4yhOAzzAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [learner, weights, vault, bundle-events, durable, agents, learning]
version: "1.0.0"
description: >
  The durable lifecycle for learned actor parameters (weights/policy
  state) on RyeOS: load latest → update from outcomes → persist an
  immutable checkpoint → advance the latest pointer → append an audit
  event. Uses the runtime vault as the source of truth and bundle-events
  as the append-only lineage log.
---

# Durable learner weights

An agent with a *learned* actor needs parameters that survive across runs:
loaded at the start of a run, updated from episode outcomes, and persisted
for the next run. RyeOS already ships the storage primitives — this doc
defines the standard pattern that ties them together.

## Where weights live

| Concern | Primitive | Why |
|---|---|---|
| **Latest durable weights** (source of truth) | **runtime vault** | A mutable, namespaced key the agent overwrites each run. Authoritative "current weights." |
| **Immutable checkpoints** | **runtime vault** (content-addressed key) | Each version stored under its own key so history is recoverable and identical weights dedupe. |
| **Audit / lineage / metrics** | **bundle-events** | Append-only, hash-chained log of every weights update — training outcomes, parent→child digest lineage. |
| **UI / debug inspection** | thread artifacts | A small metadata-only record for a run's output. **Not** the source of truth — artifacts are per-thread output records, not cross-run mutable state. |

Do **not** use thread artifacts as the weights store: they are thread
output records, not a cross-run "latest weights" authority.

## The weights envelope

Vault values are strings, so store canonical JSON:

```json
{
  "schema_version": 1,
  "kind": "learner_weights",
  "model_id": "arc_actor_v1",
  "format": "json",
  "created_at": "2026-06-21T00:00:00Z",
  "parent_digest": "sha256:...",
  "weights_digest": "sha256:...",
  "metrics": { "games_seen": 110, "success_rate": 0.37 },
  "payload": { "weights": [], "feature_schema": "arc_actor_features_v1" }
}
```

`weights_digest` is the sha256 of the canonical `payload`; `parent_digest`
links to the checkpoint this was trained from (the lineage spine).

## Key scheme (read this — the obvious scheme is invalid)

Runtime-vault **namespaces and keys must match `[A-Za-z0-9_]+` and be ≤ 64
characters** (`validate_runtime_vault_segment`). That rules out dots,
slashes, and hyphens — so keys like `arc_actor_v1.latest` or
`arc_actor_v1/checkpoints/...` are rejected, and a full 64-hex sha256 in
the key overflows the 64-char limit once you add any prefix.

Use underscore-delimited segments, and put the **full** digest in the
value, not the key:

| Purpose | Key | Notes |
|---|---|---|
| Namespace | `learner_weights` | declared in the manifest |
| Latest pointer | `<model_id>_latest` | e.g. `arc_actor_v1_latest` — overwritten each run |
| Immutable checkpoint | `<model_id>_cp_<digest16>` | e.g. `arc_actor_v1_cp_a1b2c3d4e5f60718`; `digest16` = first 16 hex (64 bits) of `weights_digest` |

The 16-hex (64-bit) prefix is **low collision risk**, not collision-proof.
Because a checkpoint key is content-derived, a prefix collision between two
*different* weight sets would overwrite the earlier checkpoint. So on
checkpoint write, **read any existing value at that key first and compare
its full `weights_digest`**: if it differs, the weights are not identical —
fall back to a longer suffix (e.g. `<digest24>`) or rely on the
append-only bundle-event for the full digest. Identical full digest = same
weights, safe to treat as a no-op.

The full `weights_digest` always lives in the envelope value and in
bundle-events (no length limit there), so the short key is only an index,
never the authority.

**Length budget** (segments are `[A-Za-z0-9_]+`, ≤ 64 chars):
- latest key `<model_id>_latest` → `model_id` ≤ **57** chars,
- checkpoint key `<model_id>_cp_<digest16>` → `model_id` ≤ **44** chars.

Pick `model_id` to satisfy the checkpoint budget (the tighter one).

## Manifest capabilities

The agent bundle declares access to the vault namespace and the event
chain it will use:

```yaml
runtime_authority:
  runtime_vault:
    - namespace: learner_weights
      operations: [get, put, list]

  bundle_events:
    - event_kind: learner_weights
      operations: [append, scan]
```

These map to derived capabilities the daemon enforces at the callback
boundary (`ryeos.put.vault.<bundle_id>/learner_weights`,
`ryeos.append.bundle-events.<bundle_id>/learner_weights`).

## Vault callback payloads

The runtime-vault callbacks are **not symmetric** — `put` and `list` take
`namespace`/`key`, but `get` and `delete` take a fully-qualified `ref`:

| Op | Payload |
|---|---|
| put | `{ "namespace": "learner_weights", "key": "<key>", "value": "<json>" }` |
| get | `{ "ref": "vault://bundle/<bundle_id>/learner_weights/<key>" }` |
| delete | `{ "ref": "vault://bundle/<bundle_id>/learner_weights/<key>" }` |
| list | `{ "namespace": "learner_weights", "cursor": null, "limit": 64 }` |

`<bundle_id>` is the agent's effective bundle id. `list` returns
`{"namespace", "keys", "next_cursor"}` in lexical key order. The cursor is
exclusive; pass a non-null `next_cursor` into the next request until the
response returns `null`. `limit` defaults to 64 and may not exceed 128; the
serialized response is capped at 64 KiB.

Pagination bounds the returned page and response size, but it is not narrow
storage I/O with the current backend. Each page still opens and validates the
complete bounded sealed envelope before filtering this namespace.

## Lifecycle

1. **Load latest** —
   `runtime_vault.get({ref: "vault://bundle/<bundle_id>/learner_weights/<model_id>_latest"})`.
   If absent, initialize default weights.
2. **Update in memory** — train/update from episode outcomes; recompute
   `weights_digest` over the new canonical payload.
3. **Persist an immutable checkpoint** —
   `runtime_vault.put({namespace: "learner_weights", key: "<model_id>_cp_<digest16>", value: <envelope>})`.
   First `get` that key and compare full `weights_digest` (see key scheme):
   identical → no-op; different → collision, use a longer suffix.
4. **Advance latest** —
   `runtime_vault.put({namespace: "learner_weights", key: "<model_id>_latest", value: <envelope>})`.
5. **Append an audit event** —
   `bundle_events.append(event_kind="learner_weights",
   chain_id="<model_id>", event_type="weights_updated",
   payload={weights_digest, parent_digest, metrics})`. The hash-chained
   event log is the durable lineage; vault holds only the latest + recent
   checkpoints.
6. **(Optional) UI artifact** — publish a small metadata-only thread
   artifact (digest + metrics) for inspection. Never the source of truth.

Order matters: write the **checkpoint before** advancing `latest`, so a
crash between steps leaves `latest` pointing at a checkpoint that exists.

## Scale limit

The runtime vault suits compact JSON weights or small policy nets. A single
value is capped at 256 KiB. The shared sealed envelope is capped at 1,024
entries, 4 MiB of plaintext, and 6 MiB on disk; these limits include operator
secrets and other bundles' internally represented runtime-vault entries.

Store larger weights blobs in CAS / object storage and keep only a
digest/pointer (plus metrics) in the vault value. A future first-class scoped
or sharded backend can make bundle/namespace access narrow without changing
the logical `vault://bundle/...` refs.
