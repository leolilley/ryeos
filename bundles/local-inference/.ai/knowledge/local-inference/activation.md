<!-- ryeos:signed:2026-08-18T22:05:09Z:e9eff69814a2c620fac0ad57239430ca456a91a59a78bfa9a3217ba5ee652bca:KttCsyvKN4L9g2feZOpc2lDXWJXu+fNvqZTQmcxKexfOvKjqk+p5tUFjp0XVnbQYHcLGCLgI92bzOcOGL7W5Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: local-inference
tags: [execution, external-content, persistent-session, local-model, replay]
version: "1.1.0"
description: >
  Operator activation and recovery contract for admitted local model workers.
---

# Admitted local model workers

The concrete worker/provider content ships in the ordinary full source-checkout
installation:

```text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
```

That installs no isolation backend and does not activate local inference.
Building, installing, and selecting an isolation implementation remains a
separate operator decision required only before recorded local execution.

RyeOS runs a local model as admitted RyeOS content, not as an ambient HTTP
sidecar. The signed worker item identifies the program and declares its exact
external realizations. The node captures those realizations into durable
content-addressed storage, binds them to the exact trusted worker, and launches
the worker through the generic persistent-session contract.

This does not remove live-filesystem execution. Project and bundle authoring can
still resolve current live files. Durable recorded execution additionally
captures the external bytes that affected one admitted worker so restart and
replay never need to reopen their original paths.

The worker's own publisher-authored source is not one of those external
inputs. It lives beside the descriptor under
`.ai/workers/local-inference/lib/local-tinygrad/`; the descriptor atomically
signs its root, entry, and aggregate source-manifest digest. RyeOS admits that source
closure directly from the exact installed bundle generation. Only the runtime,
Tinygrad, toolchain, and model are operator-imported external realizations.

The shipped `worker:local-inference/local-tinygrad` route is deliberately
`recorded`. It is not `sealed`: no node qualification currently proves a
pre-admitted deterministic compiled-artifact set.

## Node policy

Persistent sessions are disabled when their node-owned policy is absent.
External import is also disabled without a node-owned named-root policy. Both
files live in system space and are node-signed only after their typed section
validator accepts an operator-authored source:

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

Author each policy outside the live `.ai/node` namespace, then apply it while
the daemon is stopped:

```text
ryeos node policy-apply external_content /path/to/external-content-policy.yaml
ryeos node policy-apply persistent_sessions /path/to/persistent-session-policy.yaml
```

The command validates through the registered node-config section and
atomically publishes a node-signed `policy.yaml`; `ryeos sign` intentionally
remains a project-item authoring tool. Do not start the node yet; the isolation
backend, isolation policy, clean-cut resets, and doctor check below are part of
the same stopped-node activation. Absence is a refusal, not a request for
compiled defaults.

## Required isolation backend

Recorded local workers require enforced process isolation and a backend capable
of isolated networking. The captured-worker launch narrows networking to
isolated even when the node ceiling permits host networking for unrelated
admitted tools. Persistent-session and external-content policy alone do not
activate the route. RyeOS installs no backend by default.

Build the current `sandbox-linux-bubblewrap` payload explicitly before running
the `full-sandbox` populate/install command. The set adds that exact payload to
the ordinary full installation; it never downloads or builds it implicitly:

```text
./bundles/sandbox-linux-bubblewrap/build-payload.sh
sudo scripts/pkg/install-local-direct.sh --populate --all \
  --bundle-set full-sandbox --trust-source-publishers
```

The node policy is a ceiling shared by ordinary parser, handler, tool, and
worker launches. It therefore admits the standard node surfaces they may
request. The persistent-session launch independently selects the captured
filesystem ceiling: only `{verified_code}`, daemon-owned `{project}` scratch,
and its admitted realization mounts survive, and networking is forced to
isolated. It does not receive live bundle roots, node trust configuration, node
identity, or the daemon socket even though the node ceiling permits those
surfaces for other executions:

```yaml
version: 1
mode: enforce
backend:
  bundle: sandbox-linux-bubblewrap
  implementation: linux-bubblewrap
filesystem:
  readable:
    - "{node_public_identity}"
    - "{daemon_socket}"
    - "{bundle_roots}"
    - "{node_trusted_keys}"
    - "{verified_code}"
  writable:
    - "{project}"
    - "{checkpoint_dir}"
network:
  mode: host
environment:
  allow:
    - CACHELEVEL
    - CCACHE
    - DEV
    - HOME
    - HTTPS_PROXY
    - HTTP_PROXY
    - LANG
    - LC_ALL
    - LC_CTYPE
    - NO_PROXY
    - PATH
    - PYTHONHASHSEED
    - PYTHONHOME
    - PYTHONNOUSERSITE
    - PYTHONSAFEPATH
    - PYTHONDONTWRITEBYTECODE
    - PYTHONUNBUFFERED
    - RUST_BACKTRACE
    - RUST_LOG
    - RYEOSD_CALLBACK_TOKEN
    - RYEOSD_PROJECT_STATE_SCOPE
    - RYEOSD_SOCKET_PATH
    - RYEOSD_THREAD_AUTH_TOKEN
    - RYEOSD_THREAD_ID
    - RYEOS_APP_ROOT
    - RYEOS_CHAIN_ROOT_ID
    - RYEOS_CHECKPOINT_DIR
    - RYEOS_ADMITTED_SOURCE
    - RYEOS_EXTERNAL_REALIZATIONS
    - RYEOS_ITEM_KIND
    - RYEOS_ITEM_PATH
    - RYEOS_ITEM_REF
    - RYEOS_NATIVE_ASYNC
    - RYEOS_ORIGIN_SITE_ID
    - RYEOS_PROJECT_PATH
    - RYEOS_PROJECT_ROOT
    - RYEOS_PYTHON
    - RYEOS_RESUME
    - RYEOS_SESSION_FD
    - RYEOS_SITE_ID
    - RYEOS_THREAD_ID
    - SSL_CERT_DIR
    - SSL_CERT_FILE
    - TZ
    - http_proxy
    - https_proxy
    - no_proxy
limits:
  open_files: 1024
  stdout_bytes: 8388608
  stderr_bytes: 8388608
  verified_artifact_file_bytes: 67108864
  verified_artifact_total_bytes: 268435456
  verified_artifact_files: 4096
```

Author that YAML outside the live node namespace and apply it while the daemon
is stopped:

```text
ryeos node isolation-apply /path/to/isolation.yaml
```

Isolation is the separate fixed bootstrap policy rather than a signed
node-config section. The command strictly validates and atomically publishes
it; backend resolution and artifact inspection remain doctor/startup admission
steps. The environment list is the current generic RyeOS base, engine, and
protocol vocabulary plus this worker's signed bindings. Add an
application-specific authorized secret name deliberately when that application
needs one; do not replace the ceiling with ambient passthrough. The captured
persistent-session plan is narrower again and contains only its sealed authored
and daemon-owned bindings. The shipped worker additionally declares `linux`
and `x86_64` as its supported node substrate; admission checks that constraint
before capturing content or contacting a process.

## Exact acceptance realization

Construct the four operator-owned inputs from exact upstream archives into a
new assembly directory. The helper is authoring tooling, not worker content;
it verifies every download and refuses an existing output path:

```text
python3 bundles/local-inference/assemble.py \
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
  '{"staging_id":"<from-import>","request_digest":"<from-import>","manifest_hash":"<from-import>","consumer_ref":"worker:local-inference/local-tinygrad"}'
```

The bind must report the installed local-inference-bundle publisher and the exact
consumer ref. Knowing a manifest hash is not mount authority. Project content,
another publisher, or another consumer cannot reuse this binding.

## Clean-cut activation

The effect, source, and runtime authority contracts use clean-cut epochs. This
source-closure cut moves admitted launch capsules to schema 13,
persistent-session capsules to schema 3, and the runtime operator epoch to 4.
An installed node with predecessor state refuses startup rather than
translating it. With the daemon stopped, inspect first and then perform only
the reset the refusal names:

```text
ryeos node reset execution-history --dry-run
ryeos node reset execution-history --confirm
ryeos node reset replay-indexes --confirm
ryeos node reset external-content-bindings --dry-run
ryeos node reset external-content-bindings --confirm
```

The thread-history command is destructive: it retires all predecessor
thread/recovery history, including launch capsules before schema 13 and
persistent-session capsules before schema 3. Schema 3 retains the admitted
source binding alongside the path-free retained resolution. Identity, trust,
installed bundles, vault data, project heads, and durable provider evidence
roots are preserved. No predecessor capsule is translated. The replay reset
discards only predecessor dispatch-effect indexes and preserves provider-call
evidence. The runtime database
reset named by a startup refusal establishes operator epoch 4; it never
rewrites predecessor rows. These commands are a cutover operation, not normal
startup procedure.
The external-content reset retires every predecessor binding head because
current tree manifests use the
`ryeos.external_content.tree.v2` and
`ryeos.external_content.large.v2` schemas. It does not translate or preserve
old bindings: every required realization must be imported and rebound after
startup. Old CAS bytes become ordinary unrooted content eligible for later GC.

Perform activation in this order so doctor and the first launch observe one
coherent contract generation:

1. Assemble the external inputs and verify that the assembly helper completed.
2. Build the Bubblewrap payload and verify that both static executables pass
   the bundle helper's inspection. This is the only step that builds the
   independently authored isolation implementation:

   ```text
   ./bundles/sandbox-linux-bubblewrap/build-payload.sh
   ```

3. Stop the daemon. Publish and install the current RyeOS build and the
   separately built sandbox as one coherent set:

   ```text
   sudo scripts/pkg/install-local-direct.sh --populate --all \
     --bundle-set full-sandbox --trust-source-publishers
   ```

   The set publishes `core` and `standard` first, then closes the already-built
   sandbox payload and `local-inference` against those dependencies. The
   installed engine therefore understands `persistent_session.target_path`
   and `ipc.target_unix_stream` before doctor inspects the new declarations.
4. Apply the external-content and persistent-session sources through `ryeos
   node policy-apply`, then apply isolation through `ryeos node
   isolation-apply`. Keep isolation in `mode: enforce`; the node ceiling may
   retain host networking and standard node surfaces, while the captured
   worker ceiling independently removes them and requires backend support for
   isolated networking.
5. Run only the clean-cut reset commands named by the stopped node's refusal.
6. Run `ryeos node doctor` and require the selected isolation backend to report
   `available` with the expected capabilities.
7. Start the daemon once. Confirm all three signed policies loaded, then import
   and bind the four exact realizations before running the smoke directive.

## Bank and replay acceptance

After completing the ordered activation above, execute:

```text
ryeos execute directive:local-inference/examples/tinygrad_smoke --no-stream '{}'
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
