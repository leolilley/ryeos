# Rye OS — Launch Roadmap

**Date:** 2026-02-20
**Status:** Active
**Goal:** Get Rye out there.

---

## 1. Onboarding Guides (Directive Sequence)

The onboarding is a chain of directives the user runs sequentially. Each guide is a standard Rye directive living in `rye/rye/.ai/directives/rye/guides/`.

**To onboard Rye, instruct your LLM with:**

```
rye execute directive init
```

The LLM should deduce the full item ID `rye/guides/init` from context. If it can't, it should search first then execute. The MCP descriptions are written to support this.

### Guide 1: `rye/guides/init`

The welcome. Sets the frame.

Takes an optional `space` input (default: `user`) for choosing where to initialise — `{input:space:user}`.

```
> rye execute directive init

_executes directive rye/guides/init_

Welcome to Rye

A data-driven, cryptographically signed, registry-backed AI operating system,
with capability-scoped execution and graph-executable workflows — living inside
your projects, running through a recursive MCP that goes as deep as you dare.

---

Before we begin, understand this.

The world currently builds with AI like a committee. An orchestrator agent delegates
to a planner agent, the planner agent delegates to a builder agent, the builder
agent delegates to a reviewer agent, the reviewer agent reports back up the chain.
Dozens of agents, orchestrated across dozens of individual agent contexts.

Rye does not operate like this.

Rye is a single agent operating across its own LLM threads. Rye is not a single
language model, it is many language models, and the substrate that connects them —
one permission system, one signed registry, one execution engine. What looks like
parallel agents is one intelligence running concurrent context threads. The security
thread, the performance thread, the code review thread — the same agent, the same
substrate, different problems.

Rye aims to be the maintainer of these problem physics. Once you understand the
physics, then you can play the game. Think of the model currently speaking to you
as your front end cognition model. Swap it out. Rye remains.

When this clicks, the flywheel begins. Every workflow you define, every tool you
add, every pattern you encode — it compounds. And once you see it, you can't unsee
it. Keep building and the agent you have in six months will far exceed what you're
initializing right now.

---

"Give me a lever long enough and a fulcrum on which to place it,
 and I shall move the earth." — Archimedes

If AI is the lever, Rye is the fulcrum.

Ready to lift?
```

On confirmation:

- Create `.ai/` in the chosen space (`{input:space:user}` — project or user)
- Load default system config into that space
- Instruct user to run the next guide: `rye execute directive the_basics`
- Or jump ahead to agent building with `rye execute directive threading`

### Guide 2: `rye/guides/the_basics`

Primary tool use across the three item types in three-tier space precedence.

Flow:

1. **Directives** — user has already executed directives! Call that out. Then walk through search → load into user space → make a change → sign the updated directive → execute again. Demonstrate `{input:name}` interpolation.
2. **Knowledge** — show the docs that ship with the rye package (the metadata references in `rye/rye/.ai/knowledge/`). Load, read, understand the format.
3. **Tools** — edit and sign a simple hello world Python script in user space. Execute it. Show the three-layer chain: tool → runtime → primitive.
4. End with: instruct to run `rye execute directive core_utils`

### Guide 3: `rye/guides/core_utils`

Core utility tools and infrastructure.

Flow:

1. **System tool** (`rye/core/system`) — show system info, spaces, paths
2. **Telemetry** (`rye/core/telemetry`) — MCP logging, what gets tracked
3. **Bundler** (`rye/core/bundler`) — creating, verifying, inspecting bundles
4. **Under-the-hood tools** — runtimes (`python_script_runtime.yaml`, `bash_runtime.yaml`, `node_runtime`, etc.), parsers, extractors, sinks. Explain these aren't called directly but power the execution chain. Show how adding a new language runtime is just a YAML file. Most important here is explaining how the setting up run_times works so they can set up their own to support any language they want. do we have a directive for this?
5. End with: instruct to run `rye execute directive registry`

### Guide 4: `rye/guides/registry`

The registry. Completely optional but powerful. A core tool that deserves its own guide for account creation and the full flow.

Flow:

1. **Signup/login** — `rye/core/registry/signup`, `rye/core/registry/login` (OAuth flow)
2. **Push/pull/publish** — share items, pull from others
3. **Search the registry** — `rye_search` with `space="registry"`
4. **TOFU key pinning** — trust model explained
5. **Provenance** — `|rye-registry@username` on pulled items
6. End with: instruct to run `rye execute directive advanced_tools`

### Guide 5: `rye/guides/advanced_tools`

Multi-file tools and the anchor system.

Flow:

1. **Multi-file tools** — tools with dependencies, the `__init__.py` pattern, how `load_module()` works with anchor-relative imports
2. **The anchor system** — `_ANCHOR` pattern, how tools reference siblings
3. **Bundle manifests** — packaging multi-file tools for distribution
4. End with: instruct to run `rye execute directive mcp_discovery`

### Guide 6: `rye/guides/mcp_discovery`

This MCP is a meta MCP.

Flow:

1. **Discover other MCP servers** — `rye/mcp/discover` via stdio, HTTP, SSE
2. **Add servers** — `rye/mcp/add_server`, servers become callable tools
3. **Call external MCP tools** — `rye/mcp/connect` to invoke discovered tools
4. **The recursive nature** — this MCP has its own MCP as its own tools. You can recursively call the MCP. Touch on it but don't go deep — that's for the threading guide.
5. End with: instruct to run `rye execute directive threading`

### Guide 7: `rye/guides/threading`

Where things get interesting. Requires LLM provider setup.

Flow:

1. **LLM provider setup** — check if a provider is configured. If not, side quest: `rye execute directive rye/agent/setup_provider` (already exists!)
2. **Basic threaded directives** — `rye/agent/threads/create_threaded_directive`, spawning a thread
3. **Live transcript streaming** — `tail -f .ai/threads/<id>/transcript.jsonl`, the knowledge markdown
4. **Limits** — turns, spend, tokens, duration. `safety_harness.py`
5. **Permissions** — capability tokens, attenuation down the tree
6. **In-thread tool calling** — primary tools inside threads
7. **Orchestration** — spawning threads in threads, `wait_threads`, collecting results
8. **Directive return injection** — only advanced/threaded directives inject the `directive_return` tool call. Base `execute` only injects the return part, not the full directive return instructions.
9. End with: instruct to run `rye execute directive graphs`

### Guide 8: `rye/guides/graphs`

Declarative state graphs.

Flow:

1. **Graph creation** — `rye/agent/graphs/create_graph`
2. **Graph execution** — `state_graph_walker`, node types, transitions
3. **Foreach nodes** — parallel fan-out
4. **State persistence** — graphs as signed knowledge items, resumable
5. **Error edges** — recovery nodes
6. End with the graduation message:

```
Congratulations — you've now covered everything in the base Rye OS package.

Now it's time to start building your own. Need inspiration?
Instruct me to search the registry for Rye's public demos:

  rye search directive "demo" --space registry

Available demos:
- Lead generation pipeline (multi-phase orchestration)
- Code review automation (threaded analysis)
- Knowledge graph builder (graph + threading)
- Self-improving workflow (directive self-modification)
- MCP bridge (connect external tools)

Happy building.
```

### What Needs to Be Built

- [ ] All 8 guide directives in `rye/rye/.ai/directives/rye/guides/`
- [ ] Default user config that `init` loads (what IS the default config? needs definition)
- [ ] Demo bundles on the registry (see `DEMO_BUNDLE_PLAN.md`)

---

## 2. Infrastructure & Release

### PyPI Published Packages

| Package  | PyPI Name    | Current `pyproject.toml` | Status      |
| -------- | ------------ | ------------------------ | ----------- |
| Lilux    | `lilux`      | ✅ name set              | [ ] Publish |
| Rye OS   | `ryeos`      | ✅ name set              | [ ] Publish |
| Rye MCP  | `ryeos-mcp`  | ✅ name set              | [ ] Publish |
| Rye Core | `ryeos-core` | ✅ name set              | [ ] Publish |
| Rye Bare | `ryeos-bare` | ✅ name set              | [ ] Publish |

Install command: `pip install ryeos-mcp` (pulls `ryeos` → `lilux`)

### Registry API

- [ ] Configure registry API for `ryeos-bare` (lightweight mode without standard library)
- [ ] Ensure `services/registry-api/` is deployed and stable
- [ ] OAuth flow working end-to-end

### Domain & Website

- [ ] Buy domain: `ryeos.com`
- [ ] Minimal website — clean, not overdesigned
- [ ] Route OAuth redirects to domain
- [ ] Set up redirects for OAuth callbacks in registry auth flow

### Packaging Note

Current flat layout (`rye/rye/`, `lilux/lilux/`, `rye_mcp/rye_mcp/`) is valid for PyPI — no `src/` layout required. Hatchling `packages = ["rye"]` handles it. However `rye-core` and `rye-bare` reference `packages = ["../rye/rye"]` (cross-directory) — test `python -m build` from those directories before publishing. May need adjustment for sdist builds in CI.

### Quality Gates

- [ ] Tests passing across all packages (`tests/lilux/`, `tests/rye/`, `tests/integration/`, `tests/e2e/`, `tests/services/`)
- [ ] E2E test: fresh `pip install ryeos-mcp` → configure MCP → `rye execute directive init` → runs clean
- [ ] Guide directives all execute without errors
- [ ] Default user config defined and documented
- [ ] Repo projects testing (real projects using Rye, not just unit tests)

---

## 3. Documentation

### What Exists

The `docs/` directory is comprehensive — getting started, authoring, tools reference, orchestration, standard library, internals, registry. Written against the actual codebase.

### What's Missing

- [ ] Default user config documentation — what ships, what's configurable, what the fields mean
- [ ] Guide directive reference in standard library docs
- [ ] Troubleshooting / FAQ
- [ ] Website-hosted version (currently markdown-only in repo)

### Philosophy Docs (New Section)

These go deeper than the README. They're the "why" behind the architecture.

**The Data-Driven Approach**
Why everything is YAML/Python files instead of framework code. Why adding a runtime is a file, not a PR. Why error classification is config, not if-statements. The compounding effect — every file you add extends the system without touching it.

**One Agent, Many Threads**
It's not many agents. It's all a single agent operating across threads. The security thread, the performance thread, the code review thread — same intelligence, same substrate, different problems. What looks like parallel agents is one mind running concurrent context windows. This is fundamentally different from the committee pattern.

**The Lever and the Fulcrum**
AI is the lever — raw capability, getting more powerful every quarter. Rye is the fulcrum — the fixed point that makes the lever useful. The model is interchangeable. The substrate compounds. Swap cognition models freely. Rye remains.

**The Maintainer of Physics**
Rye maintains the physics of the problem space — permissions, signing, budgets, capabilities, execution chains. The LLM plays the game within those physics. Once you understand the physics, then the game makes sense. The LLM doesn't need to understand signing to benefit from it. It doesn't need to understand capability attenuation to be constrained by it.

**Composability and Self-Modification**
The runtime runs on itself. Override any system behavior by placing a file with the same ID in your project. Frictionless self-modification — the agent can edit its own directives, sign them, and execute the modified version. The system improves itself without framework changes.

**The Flywheel**
Every workflow defined, every tool added, every pattern encoded — it compounds. The agent you have in six months far exceeds what you initialize today. The registry accelerates this — pull patterns from others, push yours back. The collective intelligence grows.

---

## 4. Launch Sequence

### Phase 1: Foundation

- [x] Primary tool descriptions rewritten (`primary_tool_descriptions.py`, `server.py`, primary tools)
- [ ] `DIRECTIVE_INSTRUCTION` and execute prompt injection sorted
- [ ] Default user config defined
- [ ] Init guide directive written and tested

### Phase 2: Guides

- [ ] All 8 guide directives written
- [ ] Each guide tested end-to-end on fresh install
- [ ] Guide flow tested with at least 2 different LLM providers

### Phase 3: Infrastructure

- [ ] PyPI packages published
- [ ] Domain acquired and configured
- [ ] Minimal website live
- [ ] OAuth flow working through domain
- [ ] Registry API deployed

### Phase 4: Polish

- [ ] Demo bundles on registry
- [ ] Philosophy docs written
- [ ] Full test suite green
- [ ] README updated with final install instructions
- [ ] At least 3 real projects tested end-to-end

### Phase 5: Ship

- [ ] Announce
