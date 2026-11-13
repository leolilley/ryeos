<!-- ryeos:signed:2026-11-12T23:59:42Z:392a5bee98107fdf07d3e2ad688869ad25d6b367fc363986a62e815138341021:+t2K03yocJSmVPppUU5DPiZFZWAifvV4HosUoXV9ab/8MqwhI6n+lW9ciMv+Zgh7pk768wEyWsGosiUvWAw0Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "dev-host-tools"
title: "Development Host Tools"
description: "Externally invoked host tools for guarded builds, daemon feature checks, init smokes and dev-node drift"
entry_type: reference
version: "1.0.0"
```

# Development Host Tools

These tools run on a developer's machine, outside RyeOS. They read host
processes, disks, installed binaries and app roots, so they are externally
invoked auxiliary implementations (like `stage0-platform-production/lib/bootstrap.py`),
not admitted Tools: no `# ryeos-tool:` header, no worker grant, no RyeOS
authority. Source lives under `.ai/tools/ryeos/development/<tool>/lib/`, the
shared helper under `dev-host/lib/`, tests in `tests/development/dev_host_tools.py`.

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
