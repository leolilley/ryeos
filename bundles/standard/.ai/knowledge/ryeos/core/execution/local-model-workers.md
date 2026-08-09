<!-- ryeos:signed:2026-08-09T11:09:57Z:b598b043246ae0b60e67f6a11433fd22abcdd0171e4884ae490df0268f48cf6a:P+c7KfWeMRPnADcCylXgrQovvvGU4CLyMw7IfeC5wxZ5rtfAcJ47kTZayh5tDgTGmaJLj6vtWn+5WC7M75JgDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Sign both files with `ryeos sign <path>` while the daemon is stopped. Do not
start the node yet; the isolation backend, isolation policy, clean-cut resets,
and doctor check below are part of the same stopped-node activation. Absence is
a refusal, not a request for compiled defaults.

## Required isolation backend

Recorded local workers require enforced process isolation and isolated
networking. Persistent-session and external-content policy alone do not
activate the route. RyeOS installs no backend by default.

Build and publish the current `sandbox-linux-bubblewrap` bundle with the
ordinary bundle-authoring tools and install it while the daemon is stopped:

```text
./bundles/sandbox-linux-bubblewrap/build-payload.sh
ryeos bundle publish bundles/sandbox-linux-bubblewrap
ryeos bundle install sandbox-linux-bubblewrap bundles/sandbox-linux-bubblewrap
```

The activation policy grants this worker only `{verified_code}` in
`filesystem.readable` and `{project}` in `filesystem.writable`; the worker uses
the former for its content-addressed command and the latter for daemon-owned
ephemeral scratch. Its persistent-session launch does not receive live bundle
roots, node trust configuration, node identity, or the daemon socket. It must
select the backend and isolate networking:

```yaml
version: 1
mode: enforce
backend:
  bundle: sandbox-linux-bubblewrap
  implementation: linux-bubblewrap
filesystem:
  readable:
    - "{verified_code}"
  writable:
    - "{project}"
network:
  mode: isolated
environment:
  allow:
    - CACHELEVEL
    - CCACHE
    - DEV
    - LANG
    - LC_ALL
    - PATH
    - PYTHONHASHSEED
    - PYTHONHOME
    - PYTHONNOUSERSITE
    - PYTHONSAFEPATH
    - PYTHONDONTWRITEBYTECODE
    - PYTHONUNBUFFERED
    - RYEOS_EXTERNAL_REALIZATIONS
    - RYEOS_SESSION_FD
limits:
  open_files: 1024
  stdout_bytes: 8388608
  stderr_bytes: 8388608
  verified_artifact_file_bytes: 67108864
  verified_artifact_total_bytes: 268435456
  verified_artifact_files: 4096
```

Sign `<system>/.ai/node/isolation.yaml` with `ryeos sign <path>` while the
daemon is stopped. The shipped worker additionally declares `linux` and
`x86_64` as its supported node substrate; admission checks that constraint
before capturing content or contacting a process.

## Exact acceptance realization

Construct the four operator-owned inputs from exact upstream archives into a
new assembly directory. The helper is authoring tooling, not worker content;
it verifies every download and refuses an existing output path:

```text
python3 scripts/assemble-local-tinygrad.py \
  --cache /absolute/operator-owned/download-cache \
  --output /absolute/operator-owned/assembly
```

`--offline` forbids downloads when the exact cache is already populated. The
script deliberately does not mint RyeOS identity. The import service below
builds each canonical manifest, and the signed fixture is the only authority
for its expected hash.

The publisher-signed config item
`config:ryeos-runtime/local-tinygrad-activation` is the durable
machine-readable fixture. Its four paths are relative to one operator-admitted
assembly root. Each import is an ordinary signed service execution authenticated
as the configured local operator. A remote-origin client, a different local
signer, or project content is refused even when it can name the service:

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
ryeos node external-content-reset --dry-run
ryeos node external-content-reset --confirm-discard-external-content-bindings
```

The thread-history command is destructive: it retires all thread/recovery
history, including references to persistent-session capsule schema 1; the
path-free retained-resolution wire is capsule schema 2. Identity, trust,
installed bundles, vault data, project heads, and durable provider evidence
roots are preserved. No predecessor capsule is translated. The replay reset
discards only predecessor graph/provider replay
indexes. These commands are a cutover operation, not normal startup procedure.
The external-content reset retires every predecessor binding head because
current tree manifests use the
`ryeos.external_content.tree.v2` and
`ryeos.external_content.large.v2` schemas. It does not translate or preserve
old bindings: every required realization must be imported and rebound after
startup. Old CAS bytes become ordinary unrooted content eligible for later GC.

Perform activation in this order so doctor and the first launch observe one
coherent contract generation:

1. Assemble the external inputs and verify that the assembly helper completed.
2. Stop the daemon. Install the current RyeOS build and the freshly published
   current `core` and `standard` bundles. The installed engine must understand
   `persistent_session.target_path` and `ipc.target_unix_stream` before doctor
   inspects the new declarations.
3. Build, publish, and install the current `sandbox-linux-bubblewrap` bundle.
4. Author and sign `external_content/policy.yaml`,
   `persistent_sessions/policy.yaml`, and `isolation.yaml`. Keep isolation in
   `mode: enforce`, networking `isolated`, `{verified_code}` as the only
   readable namespace, and `{project}` as the only writable namespace.
5. Run only the clean-cut reset commands named by the stopped node's refusal.
6. Run `ryeos node doctor` and require the selected isolation backend to report
   `available` with the expected capabilities.
7. Start the daemon once. Confirm all three signed policies loaded, then import
   and bind the four exact realizations before running the smoke directive.

## Bank and replay acceptance

After completing the ordered activation above, execute:

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
