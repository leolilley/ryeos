<!-- ryeos:signed:2026-08-09T06:00:54Z:4516a926eacd9cbd78da460513a027eceaf961e318186bd02cfca8dccc1f56ef:aKIqu4RhCcXjnCI6YIPSitVkU8nABrH3kBiUiXebrUxHDodQS2VrNs6J4J4sbQGyZ4mXOLJL80kB8ikjKcWXDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, external-content, persistent-session, local-model, replay]
version: "1.0.0"
description: >
  Operator activation and recovery contract for admitted local model workers.
---

# Admitted local model workers

RyeOS runs a local model as admitted RyeOS content, not as an ambient HTTP
sidecar. The signed worker item identifies the program and declares its exact
external realizations. The node captures those realizations into durable
content-addressed storage, binds them to the exact trusted worker, and launches
the worker through the generic persistent-session contract.

This does not remove live-filesystem execution. Project and bundle authoring can
still resolve current live files. Durable recorded execution additionally
captures the external bytes that affected one admitted worker so restart and
replay never need to reopen their original paths.

The shipped `worker:standard/local-tinygrad` route is deliberately
`recorded`. It is not `sealed`: no node qualification currently proves a
pre-admitted deterministic compiled-artifact set.

## Node policy

Persistent sessions are disabled when their node-owned policy is absent.
External import is also disabled without a node-owned named-root policy. Both
files live in system space and must be signed by the configured operator:

- `<system>/.ai/node/persistent_sessions/policy.yaml`
- `<system>/.ai/node/external_content/policy.yaml`

The external root identity is not a path string alone. Open the intended
assembly directory and record its current device and root inode (`stat -c
'%d %i' <assembly-root>`). A missing or changed value refuses import.

```yaml
schema: 1
roots:
  local-model-assembly:
    path: /absolute/operator-owned/assembly
    containing_device: 66306
    root_inode: 1745406
limits:
  max_depth: 64
  max_entries: 10000
  max_file_bytes: 2147483648
  max_total_bytes: 2147483648
  store_budget_bytes: 4294967296
  minimum_free_bytes: 4294967296
```

The numbers above are examples except for the bounds required by the shipped
Qwen fixture. The device and inode must be measured from the actual root; they
must never be copied from this document.

```yaml
schema: 1
limits:
  max_pool_groups: 4
  max_total_processes: 1
  max_total_address_space_bytes: 17179869184
  max_total_cpu_seconds: 3600
  max_open_streams: 8
  max_active_streams: 1
  max_active_streams_per_subject: 1
  max_stream_backlog_bytes: 16777216
  max_total_backlog_bytes: 16777216
```

Those values are the machine-checked one-worker acceptance policy in the
activation fixture. In particular, the aggregate address-space and CPU
ceilings must each cover the worker lifecycle reservation; a smaller pool is
refused before spawn.

Sign both files with `ryeos sign <path>` while the daemon is stopped, then
start the node and confirm the policies loaded. Absence is a refusal, not a
request for compiled defaults.

## Exact acceptance realization

The worker-digest-pinned
`bundles/standard/.ai/workers/lib/tinygrad_qwen/activation-fixture.yaml` is the
durable machine-readable fixture. Its four paths are relative to one
operator-admitted assembly root. Each import is an ordinary signed service
execution:

```text
ryeos execute service:external-content/import --no-stream \
  '{"root":"local-model-assembly","path":"runtime","shape":"tree","storage":"content","maximum_bytes":104857600}'
```

Repeat using the fixture's path, storage, and bound. Compare every returned
`manifest_hash` with `expected_manifest_hash`; any difference stops activation.
Tree imports intentionally have no author-supplied expected file digest—the
canonical manifest hash is the whole-tree comparison.

Each response returns `staging_id`, `request_digest`, and `manifest_hash`.
Bind that exact tuple before the staging authority expires:

```text
ryeos execute service:external-content/bind --no-stream \
  '{"staging_id":"<from-import>","request_digest":"<from-import>","manifest_hash":"<from-import>","consumer_ref":"worker:standard/local-tinygrad"}'
```

The bind must report the installed standard-bundle publisher and the exact
consumer ref. Knowing a manifest hash is not mount authority. Project content,
another publisher, or another consumer cannot reuse this binding.

## Clean-cut activation

The effect and runtime authority contracts use clean-cut epochs. An installed
node with predecessor state refuses startup rather than translating it. With
the daemon stopped, inspect first and then perform only the reset the refusal
names:

```text
ryeos node gc --discard-thread-history --dry-run
ryeos node gc --discard-thread-history --confirm-discard-thread-history
ryeos node replay-reset --confirm-discard-replay-indexes
```

The thread-history command is destructive: it retires all thread/recovery
history while preserving identity, trust, installed bundles, vault data,
project heads, and durable provider evidence roots. The replay reset discards
only predecessor graph/provider replay indexes. These commands are a cutover
operation, not normal startup procedure.

## Bank and replay acceptance

After installing the current core and standard bundles, execute:

```text
ryeos execute directive:ryeos/examples/local_tinygrad_smoke --no-stream '{}'
```

The first run must complete through `provider:local-tinygrad`, settle its
ExplicitlyFree attempt at exactly zero, retain the daemon-observed terminal,
publish the provider-call record, and confirm its publication proof before the
turn succeeds. Stop and restart the daemon, then execute the identical
directive again. The second run must load the retained record with no worker
contact and no new reservation.

Changing the worker, runtime, Tinygrad tree, toolchain, model, provider config,
request, sampler, execution realization, or admitted authority moves the
coordinate. Missing or contradictory retained evidence fails closed; it never
falls back to a live model call.
