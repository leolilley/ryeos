# Bubblewrap isolation example bundle

This is an optional reference implementation of RyeOS's data-driven isolation
protocol. It is not part of any default bundle set, local install, container
image, release artifact, or RyeOS build.

RyeOS runs normally with isolation disabled and no selected backend. Nothing
downloads, builds, installs, or probes Bubblewrap unless an operator explicitly
authors and installs this bundle.

To experiment with the example, install Bubblewrap's authoring prerequisites
(`meson`, `ninja`, `libcap`, `readelf`, and `xz`), then run:

```bash
./bundles/sandbox-linux-bubblewrap/build-payload.sh
```

That bundle-local helper builds the adapter and the pinned Bubblewrap payload
under this bundle's `.ai/bin/<triple>/` directory. Publish and install the
result with the ordinary bundle tooling, then select its declared backend in
the node's `isolation.yaml` before changing the policy to `mode: enforce`.
