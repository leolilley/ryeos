# Local inference bundle

Local-model execution content for RyeOS. The bundle owns concrete worker
programs, their adjacent source, provider routes, activation fixtures,
acceptance probes, and model-domain knowledge. The generic `worker` kind,
persistent-session protocol, content stores, effect records, and execution
identity remain platform-owned.

The ordinary full source installation includes this bundle:

```text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
```

Source publication validates the signed YAML contract with Python 3 and
PyYAML (`python3-yaml` on Debian/Ubuntu). Realization release authoring also
requires `zstd` to extract the exact python-build-standalone licensing metadata
artifact; release and container builders install their required dependencies
explicitly.

The release pipeline keeps third-party runtime/model execution in read-only,
no-secret jobs. Fresh write-authorized jobs accept only the exact regular-file
asset set whose bytes match the source-owned contract, and never execute those
assets. Disabled-isolation activation and replay are functional/recovery
conformance evidence over bytes the RyeOS publisher has already chosen to
trust; same-UID observations are not evidence that adversarial code is safe.

Local inference does not require Bubblewrap. On the default trusted single-user
node, RyeOS delivers the exact signed source and external realizations through a
daemon-owned private workspace and runs the persistent worker under disabled
OS isolation. An installed isolation backend is optional hardening; RyeOS
records whether confinement and isolated networking were actually enforced.

Installing bundle bytes does not acquire model/runtime content. A fresh
`full` or `full-sandbox` initialization explicitly publishes the bounded
publisher-authored acquisition and persistent-session baseline needed by this
fixture, so it can be activated with one generic operation:

```text
ryeos external-content activate config:ryeos-runtime/local-tinygrad-activation online
```

Activation submission returns a durable sync-job coordinate promptly. Observe
it with `service:sync/jobs/inspect`; once complete, repeating the command
returns the verified node-local receipt idempotently. Acquisition/import is
daemon-owned and restart-recoverable rather than tied to the CLI connection.

The node downloads exact immutable publisher archives into its private cache,
reproduces the worker's existing manifest pins, publishes ordinary consumer
bindings, and records target-local settlement. The online path needs no
installed assembler, public realization directory, named-root policy,
node-side build, or manual import/bind loop.

Reinstalling an existing node preserves its current node-owned policy
generation. Operators use stopped-node `ryeos node policy-apply` only when
deliberately changing that policy, adding an offline archive root, or repairing
an older/missing policy generation; an ordinary fresh full initialization does
not require hand-authored prerequisite policy files.

For a network-independent node, the same operation accepts an explicitly
selected node-policy archive root:

```text
ryeos external-content activate \
  config:ryeos-runtime/local-tinygrad-activation \
  offline local-inference-archives
```

That root is removable/offline acquisition authority only. RyeOS verifies the
same signed archives and then uses its private digest-keyed cache; it is not a
lasting workload root, assembly directory, or manual binding path.

The shipped Qwen3-0.6B CPU route is a bounded recorded-class contract fixture.
It is not a sealed qualification and does not define the future production
model, device, context, trace, or training architecture.
