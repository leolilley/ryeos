<!-- ryeos:signed:2026-11-13T06:27:19Z:2b4a67e2482b4be85ff26d380c9dec62bf954532bf1214cce035545345497b27:I1R5xShC5ASh6k0FX1z175mbpRR6Vpav9RxNqGH1wp1EVaSyiKN1BTgUGiKiI1rwLwVIBcHd3GT7y/UMF8jRDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "dev-host-tools"
title: "Development Host Tools"
description: "Host tools for guarded builds, daemon feature checks, init smokes and dev-node drift: external helpers and their host-access Tool entries"
entry_type: reference
version: "1.1.0"
```

# Development Host Tools

These tools read host processes, disks, installed binaries and app roots. Each
helper's implementation is signed source under
`.ai/tools/ryeos/development/<tool>/lib/` (which Tool resolution excludes),
with the shared helper under `dev-host/lib/` and tests in
`tests/development/dev_host_tools.py`. A helper runs two ways:

- **Externally**, before a node exists or on any machine: `python3 <lib file>`
  as below. That is external evidence, not admitted execution, and grants no
  RyeOS authority.
- **As a Tool under the node.** A thin entry beside each helper
  (`<tool>/run.py`, or `build-support-inputs/execute.py` and
  `contained-oci-provider/container.py`) declares `host_authority: required`.
  It runs on the host, unconfined, only under an exact operator grant (`ryeos
  node host-access grant`), on a lane that offers host access (the
  `development` profile's local lanes), when the configured local operator
  runs it. The entry maps a schema-validated request and the Tool's resolved
  Config onto the helper's arguments through `dev-host/lib/node_entry.py`; it
  passes an absolute `worktree` parameter, because the node runs the entry from
  its retained source closure, not the checkout. See
  `ryeos/core/node/host-access` in the ryeos-docs bundle.

Three checks need no host access and are ordinary sandboxed Tools:
`build-support-inputs/plan` (the dry run), `contained-oci-provider/report`
and `render-provider-shape-check/run` (its key through `required_secrets`).
They read the project at the runtime's project path, so run them under a
pinned project policy. `maintainer/lib/sign-dev.sh`,
`interactive-host/lib/dev-tui.sh` and `interactive-host/lib/dev-ui-assets.sh`
stay external: the first would hand a node child the publisher key, and the
interactive pair needs the operator's terminal or display.

A grant pins the exact bytes of the entry's source closure, which for these
Python Tools is the whole `.ai/tools/ryeos/` tree: any edit there needs a
re-grant (`ryeos node host-access list --project <worktree>` shows which).

Site values come from signed Configs under `.ai/config/development/ryeos/`.
The committed ones hold portable defaults. Keep a site copy outside the repo
and pass it with `--config-file`; values expand `${NAME}` and
`${NAME:-default}` from `WORKTREE`, `TARGET_DIR` (guarded-cargo) and the
environment.

| Tool | Config | Use |
|---|---|---|
| `guarded-cargo/lib/guarded_cargo.py` | `guarded-cargo` | One cargo per machine: shared flock, waits for configured live-step process patterns and unguarded cargo/rustc, refuses under the disk floor, kills cargo's process group under the lower floor, touches sources when a shared target changes worktree, prunes afterwards. `--status`, `--prune-only`. |
| `featured-build-check/lib/featured_build_check.py` | `dev-node` | Compares `ryeosd build-info --json` `compiled_features` with what the profile's bundle set selects (`scripts/pkg/bundle-features.py`). Exit 1 names the missing features. |
| `bundle-set-init-smoke/lib/bundle_set_init_smoke.py` | `dev-node` | `ryeos init` of a throwaway app root from the worktree's bundles; checks the profile's whole set admits. `--start` also waits for `/_ryeos/ready`. |
| `dev-node-drift-check/lib/dev_node_drift_check.py` | `dev-node` | Installed host binaries vs the worktree build, the installed daemon's revision vs HEAD (source files changed since), installed bundle files vs `bundles/`. |
| `codex-worker-handshake-check/lib/codex_worker_handshake_check.py` | none | Starts a Codex binary as the signed Worker profile does, empty home, no credential; holds startup replies to the profile's schemas and predicates. Needs `jsonschema`. |
| `render-provider-shape-check/lib/render_provider_shape_check.py` | `render-provider-shape` | One free GET of the newest terminated Render sandbox, held to the shape read from the adapter's own `RenderSandbox` source. `--response FILE` checks offline. |

```bash
python3 .ai/tools/ryeos/development/guarded-cargo/lib/guarded_cargo.py \
  --config-file ~/site/guarded-cargo.yaml --worktree "$PWD" --log /tmp/cargo.log \
  -- test -p ryeos-engine
python3 .ai/tools/ryeos/development/featured-build-check/lib/featured_build_check.py --built
python3 .ai/tools/ryeos/development/dev-node-drift-check/lib/dev_node_drift_check.py \
  --config-file ~/site/dev-node.yaml
```

Exit codes: 0 pass, 1 a finding, 2 cannot check (guarded-cargo returns
cargo's status, 3 for a disk refusal and 4 for a watchdog kill).
