# Bubblewrap isolation backend bundle

This is the independently installed Linux implementation of RyeOS's
data-driven isolation protocol. It remains outside every default bundle set,
ordinary local install, container image, release artifact, and RyeOS build. The
explicit `full-local-inference` source-checkout set preserves and publishes an
operator-built payload; it never builds or downloads one implicitly. The
admitted local-worker acceptance route uses it only after an operator explicitly
builds, installs, and selects it.

RyeOS runs normally with isolation disabled and no selected backend. Nothing
downloads, builds, installs, or probes Bubblewrap unless an operator explicitly
authors and installs this bundle.

To author the backend, install its build prerequisites (`meson`, `ninja`,
`make`, a C compiler, `pkg-config`, `readelf`, and `xz`), then run:

```bash
./bundles/sandbox-linux-bubblewrap/build-payload.sh
```

That bundle-local helper builds the adapter and the pinned Bubblewrap payload
under this bundle's `.ai/bin/<triple>/` directory. Publish and install the
result with the ordinary bundle tooling, then select its declared backend in
the node's `isolation.yaml` before changing the policy to `mode: enforce`.
