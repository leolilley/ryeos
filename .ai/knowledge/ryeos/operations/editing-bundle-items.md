---
category: "ryeos/operations"
name: "editing-bundle-items"
description: "How to edit, sign, and deploy items that live inside bundles"
---

# Editing Bundle Items

When you need to change something that lives inside a bundle — a verb, an
alias, a route, a service YAML, a tool, a knowledge doc — the flow is:

## 1. Edit the source

Bundles live in `bundles/<name>/` at the repo root. Edit the file directly.
If the file has a `# ryeos:signed:` header, strip it (delete the first line)
before editing, or the publish pipeline will strip it for you.

**Do NOT edit installed bundles** under `~/.local/share/ryeos/.ai/bundles/`.
Those are copies; your changes will be overwritten on the next `ryeos init`.

## 2. Build the bundle

```bash
target/debug/ryeos-core-tools build bundles/<name>
```

This runs the full publish pipeline:

| Phase | What it does |
|-------|-------------|
| 0 — Clean | Strips all signatures, removes derived CAS artifacts |
| 1 — Bootstrap | Signs kind schemas, parsers, handlers, protocols, runtimes |
| 2 — CAS rebuild | Hashes binaries under `.ai/bin/`, writes manifest sidecars |
| 3 — Sign items | **The engine walks every registered kind's directory and signs every file** |
| 4 — Manifest | Generates `.ai/manifest.yaml` from `.ai/manifest.source.yaml` |
| 5 — Trust doc | Emits `PUBLISHER_TRUST.toml` at the bundle root |

Phase 3 is the important one. The engine iterates all registered kinds
(including `node`, which covers `.ai/node/` recursively). Every file with
a matching extension gets parsed, validated (metadata anchoring), and
signed. This means verbs, aliases, routes, bundle registrations, services,
tools, knowledge, config — **everything** — goes through the same engine
path. There are no special cases.

## 3. Deploy to the system source dir

```bash
sudo rm -rf /usr/share/ryeos/<name>
sudo cp -r bundles/<name> /usr/share/ryeos/<name>
```

## 4. Init + restart

```bash
ryeos init --source /usr/share/ryeos
ryeos stop; sleep 1; ryeos start
```

`ryeos init` copies from `/usr/share/ryeos/` into the installed location
at `~/.local/share/ryeos/.ai/bundles/`, registers them in node config,
and auto-pins trust docs from each bundle root.

## Common edits

### Adding a new verb or alias

1. Create `bundles/core/.ai/node/verbs/<name>.yaml` (or `aliases/`)
2. Add the `category`, `section`, `name`/`tokens`, `description`, and
   `execute` fields
3. Build: `ryeos-core-tools build bundles/core`
4. Deploy + init + restart

### Changing where a verb points

Edit the `execute:` field in the verb YAML. Use `service:` refs for
in-process daemon handlers (registered in `handlers::ALL`) and `tool:`
refs for subprocess tools (defined in `.ai/tools/`).

### Adding a new service handler

This requires Rust code changes — the handler goes in
`crates/services/api/src/handlers/`, gets registered in `handlers::ALL`
in `mod.rs`, and needs a corresponding route YAML in
`bundles/core/.ai/node/routes/`.

## Key files

| File | Purpose |
|------|---------|
| `bundles/core/.ai/node/engine/kinds/node/node.kind-schema.yaml` | Defines the `node` kind (covers `.ai/node/`) |
| `crates/tools/core-tools/src/actions/publish.rs` | Build pipeline phases |
| `crates/tools/core-tools/src/actions/sign_bundle.rs` | Phase 3 engine-based signing |
| `crates/core/node/src/init.rs` | Init flow (install, register, auto-pin trust) |
