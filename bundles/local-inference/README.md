# Local inference bundle

Optional local-model execution content for RyeOS. The bundle owns concrete
worker definitions, their adjacent source, provider routes, activation data,
and acceptance probes. The generic `worker` kind remains part of the platform.

This bundle does not own an isolation backend. Recorded local execution
requires the separately authored Bubblewrap bundle with isolated-network
capability; live execution may use the generic disabled-isolation path.

See `knowledge:local-inference/activation` after installing the bundle.

For a source checkout, build the isolation payload explicitly, then select the
optional install set that publishes and preserves both bundles:

```text
./bundles/sandbox-linux-bubblewrap/build-payload.sh
sudo scripts/pkg/install-local-direct.sh --populate --all \
  --bundle-set full-local-inference --trust-source-publishers
```

Continue selecting `full-local-inference` for later base installs on that node.
It never rebuilds the isolation payload implicitly. The ordinary `full` set
deliberately removes optional bundle registrations.
