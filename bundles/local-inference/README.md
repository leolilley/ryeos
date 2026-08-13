# Local inference bundle

Local-model execution content for RyeOS. The bundle owns concrete
worker definitions, their adjacent source, provider routes, activation data,
and acceptance probes. The generic `worker` kind remains part of the platform.

This bundle does not own an isolation backend. Recorded local execution
requires the separately authored Bubblewrap bundle with isolated-network
capability; live execution may use the generic disabled-isolation path.

See `knowledge:local-inference/activation` after installing the bundle.

The ordinary full source-checkout install publishes this bundle without an
isolation backend:

```text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
```

If recorded local execution is activated later, build and install a compatible
isolation backend separately. Installing this bundle does not select or require
Bubblewrap.
