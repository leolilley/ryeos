# Local inference bundle

Optional local-model execution content for RyeOS. The bundle owns concrete
worker definitions, their adjacent source, provider routes, activation data,
and acceptance probes. The generic `worker` kind remains part of the platform.

This bundle does not own an isolation backend. Recorded local execution
requires a separately installed enforced backend with isolated-network
capability; live execution may use the generic disabled-isolation path.

See `knowledge:local-inference/activation` after installing the bundle.
