<!-- ryeos:signed:2026-08-29T15:44:54Z:c2ea79b99182f1bf68d807c26133621a78d2e733908a2d404f5de05cf9e2500f:tp1gBzJaYz0luenZMzEgv4Ac4ReK325Hx9xILYM2aQJpjPHWJhweo4+UTPQwnJZtts9mdzKEaORN5hJg9BidAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: local-inference
tags: [execution, managed-activation, persistent-session, local-model, replay]
version: "1.3.0"
description: >
  Node policy, managed activation, execution evidence, and recovery contract
  for the admitted Qwen3 tinygrad fixture.
---

# Admitted local model workers

RyeOS runs a local model as admitted RyeOS content, not as an ambient HTTP
sidecar. The signed worker owns its executable source and exact external
realization manifest pins. The signed activation declaration says how to
acquire already-final publisher artifacts. Existing manifest and consumer-
binding authorities remain launch authority; the node-local activation receipt
is historical settlement evidence only.

The ordinary full source installation includes the local-inference bundle:

~~~text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
~~~

Installation does not acquire model/runtime bytes and does not silently grant
node acquisition or persistent-process capacity. Bubblewrap is optional node
hardening and is not an activation prerequisite.

The shipped worker:local-inference/local-tinygrad and
provider:local-tinygrad are the bounded Qwen3-0.6B CPU fixture. They are
recorded, not sealed.

## Exact execution boundary

The worker's signed source lives under
.ai/workers/local-inference/lib/local-tinygrad/. Its four locator-free pinned
realizations are:

- the hermetic Python runtime and dynamic loader;
- the exact tinygrad source tree;
- the exact compiler/linker closure; and
- the exact model, tokenizer, configuration, template, and provenance bytes.

config:ryeos-runtime/local-tinygrad-activation uses the generic
ryeos.external_content_activation.v3 whole-archive-tree shape. Each signed
source is an immutable, publisher-produced final tree. Node activation performs
no package installation, stripping, patching, template expansion, generated
metadata, or arbitrary command. It descriptor-safely scans and materializes
only canonical directories, regular files, and non-escaping relative symlinks,
then requires the resulting existing manifest digest to equal the worker's
signed pin.

When isolation is disabled, RyeOS materializes admitted source and realizations
into a daemon-owned private session workspace and launches the worker from
retained exact command authority. It provides bounded private scratch, home,
cache, and temporary directories plus the daemon-owned session channel. The
worker does not execute from a release archive, mutable cache, vendor checkout,
or public assembly directory.

Disabled isolation is trusted signed local execution, not OS confinement.
RyeOS reports it honestly. A separately installed compatible backend may
enforce stronger filesystem/network isolation without changing the recorded
provider contract.

## Node policy

Apply node policy while the daemon is stopped. Managed activation needs no
named filesystem root. The following is the exact minimum shape for this
fixture; operators may grant larger storage reserves deliberately, but must
not reduce the signed acquisition or import bounds:

~~~yaml
schema: 1
roots: {}
limits:
  max_depth: 11
  max_entries: 4480
  max_file_bytes: 1503300328
  max_total_bytes: 1514745854
  store_budget_bytes: 4294967296
  minimum_free_bytes: 8589934592
managed_activation:
  allow_online: true
  allowed_https_hosts:
    - github.com
    - release-assets.githubusercontent.com
  max_redirects: 2
  max_archives: 4
  max_compressed_bytes: 1329438282
  max_expanded_bytes: 1893273600
  max_members: 4836
  max_member_bytes: 1503300328
  max_concurrent_activations: 1
  cache_budget_bytes: 2147483648
  store_budget_bytes: 4294967296
  minimum_free_bytes: 8589934592
  max_attempts: 3
~~~

The two HTTPS hosts are separately node-admitted because an immutable GitHub
release URL redirects to GitHub's release-asset host. RyeOS follows at most the
node-owned redirect ceiling, rechecks canonical HTTPS and the host allowlist on
every hop, and still requires the exact signed archive digest.

The persistent worker also requires a separate node-owned session policy:

~~~yaml
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
~~~

Author both files outside the live node namespace, then atomically validate and
apply them through their registered sections:

~~~text
ryeos node policy-apply external_content /path/to/external-content-policy.yaml
ryeos node policy-apply persistent_sessions /path/to/persistent-session-policy.yaml
~~~

Do not hand-edit or separately sign files under .ai/node. Policy absence or
invalid bounds are refusals.

## One-command activation

Start the node, then run as its configured local operator:

~~~text
ryeos external-content activate config:ryeos-runtime/local-tinygrad-activation online
~~~

The generic durable job:

1. resolves the exact trusted signed declaration and worker;
2. admits URLs, redirects, archive counts, bytes, entries, and storage against
   current node policy;
3. downloads or reuses the four exact archives in the node-private cache;
4. scans and stages them outside state-store mutation locks;
5. imports ordinary content/large-content manifests;
6. proves all four manifest hashes equal the worker declarations;
7. publishes bindings owned by that exact worker; and
8. publishes one compact node-signed completion receipt only after the whole
   set settles.

The operation is idempotent and restart-recoverable. A completed retry verifies
the current receipt, manifest objects, and binding heads. A failed or
interrupted attempt retains its canonical operation and can only resume within
the admitted retry ceiling.

Use the offline spelling only when the exact archive digests are already in
that node's managed cache from a successful prior online activation or a later
supported offline-export import:

~~~text
ryeos external-content activate config:ryeos-runtime/local-tinygrad-activation offline
~~~

Offline mode never downloads and refuses a missing or wrong cache entry. It is
not permission to copy files into an ambient assembly directory or revive the
retired named-root import ceremony.

## Validation, bank, restart, and replay

Validate the signed fixture threadlessly, then execute:

~~~text
ryeos validate directive:local-inference/examples/tinygrad_smoke \
  --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
  '{}'

ryeos execute directive:local-inference/examples/tinygrad_smoke \
  --ref-binding model=directive:local-inference/examples/tinygrad_smoke \
  --no-stream '{}'
~~~

The directive runtime requires its signed model declaration as the `model`
launch binding. This binding selects the same trusted directive; it is not a
caller-selected provider, model name, worker, or realization.

The first run must:

- resolve provider:local-tinygrad and the exact admitted worker;
- launch from the daemon-owned private view under the reported isolation mode;
- settle an ExplicitlyFree attempt at exactly zero;
- retain the daemon-owned terminal observation; and
- publish and confirm one complete provider-call record.

Stop and restart the node, then execute the identical directive. The second run
must replay from the durable provider-call effect record with no worker lease,
process, model, device contact, or new reservation. Its projected provider
observation reports `source: replay` and `publication: not_applicable`; the
effect record remains the replay authority rather than an observation-source
label.

Changing worker source, runtime, tinygrad, toolchain, model/tokenizer/template,
provider config, request, sampler, target execution identity, or relevant
authority moves the coordinate. Missing, corrupt, or contradictory indexed
evidence fails closed; it never silently contacts the model under the old
identity.

## Publisher boundary

Node operators never run the realization authoring utility. Release maintainers
use scripts/release/author-local-inference-realizations.py in a reviewed
publisher workflow. It verifies exact upstream inputs, authors canonical final
archives in a sibling staging directory, reproduces the existing signed worker
manifest pins, compares every result with
scripts/release/local-inference-qwen3-0.6b-v1.json, and atomically publishes
the completed local artifact directory. The release workflow never overwrites
an existing asset.

Jobs that extract or execute third-party runtime/model bytes have read-only
repository authority, no release/package/OIDC authority, and no persisted Git
credential. Fresh write-authorized jobs check out the exact immutable tag,
revalidate the complete regular-file asset set and every source-owned digest,
and only then publish or promote; they never execute the subject. The online
disabled-isolation run is functional/recovery conformance over exact bytes the
publisher contract already trusts. Its same-UID database and process
observations do not qualify malicious code or claim an OS security boundary.

## Current scope and future profiles

The fixture uses CPU, Qwen3-0.6B, 2,048 tokens of context, 256 output tokens,
one active stream, and disabled mutable compiler caches. It proves model
mapping, tokenizer/template behavior, sampling, streaming, cancellation,
persistent lifecycle, recording, and replay.

A serious model/device route uses a separate signed profile that binds exact
model/runtime/compiler content, backend/device requirements, numerics,
quantization, context, resources, sampler, and trace policy. It never mutates
this fixture or auto-selects ambient hardware.
