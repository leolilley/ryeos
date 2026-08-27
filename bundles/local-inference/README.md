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

Local inference does not require Bubblewrap. On the default trusted single-user
node, RyeOS delivers the exact signed source and external realizations through a
daemon-owned private workspace and runs the persistent worker under disabled
OS isolation. An installed isolation backend is optional hardening; RyeOS
records whether confinement and isolated networking were actually enforced.

Installing the bundle does not import or activate model content. Follow
`knowledge:local-inference/activation` to configure node bounds, import and bind
the exact runtime/model realizations, validate the route, and prove first-bank
versus zero-contact replay.

The shipped Qwen3-0.6B CPU route is a bounded recorded-class contract fixture.
It is not a sealed qualification and does not define the future production
model, device, context, trace, or training architecture.
