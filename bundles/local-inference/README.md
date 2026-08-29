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

Installing the bundle does not acquire model/runtime content or grant node
capacity. Follow `knowledge:local-inference/activation` to apply explicit
managed-acquisition and persistent-session policy, then activate the complete
fixture with one generic operation:

```text
ryeos external-content activate config:ryeos-runtime/local-tinygrad-activation online
```

The node downloads exact immutable publisher archives into its private cache,
reproduces the worker's existing manifest pins, publishes ordinary consumer
bindings, and records target-local settlement. There is no installed
assembler, public realization directory, named-root policy, node-side build,
or manual import/bind loop.

The shipped Qwen3-0.6B CPU route is a bounded recorded-class contract fixture.
It is not a sealed qualification and does not define the future production
model, device, context, trace, or training architecture.
