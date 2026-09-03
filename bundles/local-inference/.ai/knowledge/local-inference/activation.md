<!-- ryeos:signed:2026-09-02T13:27:39Z:87bfc0dcd963b50f49fe1d57f1c7b676ab8b14bf667e1919310b296939b383bc:G08nne15FJZLJ7axpNo3w+br+GfXcR3uWa5rkDhYU60Z8oKzqXbSLn/DULWuHKEOMdWb3mgeZDPslWNeJ8Q2CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: local-inference
tags: [execution, managed-activation, persistent-session, local-model, replay]
version: "1.4.0"
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

Installation does not acquire model/runtime bytes. A fresh explicit `full` or
`full-sandbox` initialization publishes a bounded publisher-authored
acquisition and persistent-process baseline; reinstalling an existing node
preserves that node's current policy generation. Bubblewrap is optional node
hardening and is not an activation prerequisite.

The bundle ships two exact recorded Qwen3-0.6B CPU profiles:

- worker:local-inference/qwen3-0.6b-cpu-4096 through
  provider:qwen3-0.6b-cpu-4096; and
- worker:local-inference/qwen3-0.6b-cpu-2048 through
  provider:qwen3-0.6b-cpu-2048.

They share the same implementation source and four immutable realization
pins. Their signed real-UID process ceilings differ, so the worker contributor,
effective program, persistent-session pool identity, restart authority, and
provider coordinate differ. Neither profile is sealed.

## Exact execution boundary

The worker's signed source lives under
.ai/workers/local-inference/lib/local-tinygrad/. Its four locator-free pinned
realizations are:

- the hermetic Python runtime and dynamic loader;
- the exact tinygrad source tree;
- the exact compiler/linker closure; and
- the exact model, tokenizer, configuration, template, and provenance bytes.

The two profile-specific activation declarations use the generic
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

Apply a node-policy replacement only while the daemon is stopped. Online or
cache-only managed activation needs no named filesystem root. The following is
the current `full`/`full-sandbox` installed-bundle union with a recommended
higher residual reserve. When applying this change, start from the node's
current complete `external_content` member and change only the two
`minimum_free_bytes` values; do not remove capacity or hosts required by other
installed workloads. The current complete shape is:

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
  enabled: true
  limits:
    allow_online: true
    allowed_https_hosts:
      - github.com
      - release-assets.githubusercontent.com
      - releases.openai.com
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

The three HTTPS hosts are the exact installed-bundle union: immutable GitHub
release URLs may redirect to GitHub's release-asset host, while the installed
Codex workload uses releases.openai.com. RyeOS follows at most the node-owned
redirect ceiling, rechecks canonical HTTPS and the host allowlist on every hop,
and still requires each exact signed archive digest.

The 8 GiB residual free-space floor above is an operator recommendation, not
workload identity. The publisher-authored `full` and `full-sandbox` baseline
uses a reviewed 2 GiB floor so fresh source qualification can run on the
storage-constrained hosted runner after deleting build targets. A higher local
override changes admission testimony, never archive, manifest, program, or
replay identity.

The persistent worker also requires a separate node-owned session policy:

`max_real_uid_process_limit` is the process's real-UID-wide `RLIMIT_NPROC`
ceiling, not a per-worker descendant count. RyeOS separately bounds pool groups
and total resident worker processes.

~~~yaml
schema: 1
enabled: true
limits:
  max_pool_groups: 4
  max_total_processes: 1
  max_total_address_space_bytes: 17179869184
  max_total_cpu_seconds: 3600
  max_real_uid_process_limit: 4096
  max_open_streams: 8
  max_active_streams: 1
  max_active_streams_per_subject: 1
  max_stream_backlog_bytes: 16777216
  max_total_backlog_bytes: 16777216
~~~

A fresh `full` or `full-sandbox` installation selects a publisher-signed init
profile whose capacities cover the exact acquisition and worker requirements
above, with the reviewed 2 GiB residual free-space baseline, and publishes one
complete node-signed generation under `.ai/node/policies/`. `external_content.yaml` and
`persistent_sessions.yaml` are mandatory members. An operator changing either
member later must stop the daemon and use
`ryeos node policy-apply <section> <source.yaml>`; that command validates the
replacement and atomically republishes the complete generation. Do not
hand-edit generation files or manufacture prerequisite policy documents for
an ordinary fresh install. Policy absence or invalid bounds are refusals.

## One-command activation

Start the node, then run both profile-specific activations as its configured
local operator. The second operation reuses the verified four manifests but
publishes distinct consumer bindings:

~~~text
ryeos external-content activate config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation online
ryeos external-content activate config:ryeos-runtime/qwen3-0.6b-cpu-2048-activation online
~~~

The command first returns a durable coordinate such as
`{"job_id":"external-activation:...","state":"running","idempotent":false}`.
It does not keep the invoking service thread open for a multi-gigabyte
download, extraction, or import. Inspect the exact coordinate through the
ordinary sync-job service:

~~~text
ryeos execute service:sync/jobs/inspect --no-project --no-stream \
  --input '{"job_id":"external-activation:..."}'
~~~

Once that job is `completed`, repeating the activation command returns the
verified completion result with its `receipt_hash` and `idempotent: true`.

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
the admitted retry ceiling. The durable running attempt is also the exclusive
execution lease: concurrent submissions cannot launch duplicate acquisition,
and daemon startup reconciles an interrupted lease before recovery claims the
next attempt.

Offline activation has two explicit forms. Cache-only activation requires every
exact digest to have settled previously:

~~~text
ryeos external-content activate config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation offline
~~~

An offline artifact set may instead be supplied through one node-owned
external-content root, for example `local-inference-archives`, after the
operator applies its exact path/device/inode policy while the node is stopped:

~~~text
ryeos external-content activate \
  config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation \
  offline local-inference-archives
~~~

This is the generic offline acquisition boundary, not the retired realization
assembly/import/bind ceremony. The durable operation binds the selected root's
node-policy authority digest. RyeOS opens only the signed URL basename as a
no-follow regular file, verifies its signed byte ceiling and digest, and
atomically publishes the digest-keyed private cache entry before using the
normal managed extractor. It never scans for alternatives, transforms bytes,
or falls back online. A missing, linked, oversized, or wrong archive refuses.

## Validation, bank, restart, and replay

Validate both signed profiles threadlessly, then execute them sequentially:

~~~text
ryeos validate directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
  --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
  --no-project --input '{}'

ryeos execute directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
  --ref-binding model=directive:local-inference/examples/qwen3_0_6b_cpu_4096_smoke \
  --no-project --no-stream --input '{}'
~~~

The directive runtime requires its signed model declaration as the `model`
launch binding. This binding selects the same trusted directive; it is not a
caller-selected provider, model name, worker, or realization. Static validation
invokes the same signed bounded launch preparer used by execution and reports
the selected worker's canonical ref, as-launched resolution digest, source and
four exact content/binding readiness records, and session-policy eligibility.
It never launches the workload, creates a session or lease, or publishes
authority.

The first run must:

- resolve the profile's exact provider and admitted worker;
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
