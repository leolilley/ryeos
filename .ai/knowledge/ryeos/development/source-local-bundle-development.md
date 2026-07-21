<!-- ryeos:signed:2026-07-21T00:24:55Z:f287957a02ed70a646dfa63bfea5af1f15a6298f8db634db46a75e30b0808324:yITVpQVqqjkmHrKLxwjdJq9r3JvWFoINH6I+I6YTqTM+fuZ5ZDit+hkMDACH5dyE2bUaw3HngES4L6gQAFhMAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: source-local-bundle-development
title: Source-Local Bundle Development
description: Implemented source-local command, state-root, signing, and bundle-smoke workflow
entry_type: reference
version: "1.0.0"
```

# Source-Local Bundle Development

## Status

This workflow is implemented. The entries below identify its supported
developer-facing contracts and their owning code.

- Command descriptors / local command help: routed without daemon alias
  parsing; project-aware tails auto-detect a cwd ancestor containing `.ai/`;
  `RYEOS_PROJECT_ROOT` / `RYEOS_PROJECT_PATH` are part of the runtime/env
  contract.
- **Runtime state-root override**: `ryeos execute <ref> --state-root /tmp/...`
  runs against the resolved project source while runtime state anchors under
  the override; both roots appear in the response's `execution` diagnostics.
  Live-fs only; a state root inside the project source is rejected.
- **Multi-item signing**: `ryeos sign` accepts a bounded set of changed
  bundle refs/paths in one invocation (`753d758e`), input hardened against
  escape (`c1751747`).
- **Bundle smoke command**: `ryeos bundle smoke` (service:bundle/smoke +
  command descriptor). Bundles declare a `smoke:` list in
  manifest.source.yaml; the service runs bundle preflight, then dispatches
  each entry as a normal synchronous wait-mode thread against the bundle source with state
  isolated under a temporary state root, and reports per-entry status,
  thread ids, and the state root (kept on failure or `keep_state`). See
  `crates/daemon/ryeos-api/src/handlers/bundle_smoke.rs` and
  `ryeos_bundle::manifest::SmokeDecl`.
