# RyeOS

> _In Linux, everything is a file. In RyeOS, everything is data._

RyeOS is an operating substrate for AI agents: a local node, signed execution
engine, durable state layer, and bundle system that let agents carry tools,
workflows, knowledge, and authority across projects and machines.

It is not another prompt library. It is the layer underneath an agent session:
identity, trust, execution, state, orchestration, scheduling, remotes, and an MCP
bridge over one CLI.

## Why RyeOS exists

Most agent setups are ephemeral. A model is connected to a bag of tools, the
session runs, and the useful state is scattered across chat history, local files,
and one-off scripts. RyeOS gives that work a persistent substrate:

- **Signed items** — tools, directives, knowledge, graphs, and config are
  tamper-evident data.
- **Bundles** — installable collections of signed items, binaries, schemas, and
  CLI verbs.
- **A local node** — `ryeosd` owns execution, state, vault material, threads,
  scheduling, and remote APIs.
- **Content-addressed state** — events, snapshots, manifests, and project state
  are written to CAS first; SQLite is only a rebuildable projection.
- **Threads** — every execution has an ID, event log, lifecycle, cancellation,
  and replay path.
- **MCP integration** — agents call one `cli` tool; RyeOS routes to whatever
  signed verbs the installed bundles provide.

The result is a system where the model can change, the client can change, and
the machine can change, while the signed execution substrate remains.

## Mental model

```text
╭──────────────╮      ╭──────────────╮      ╭──────────────╮
│ AI client    │ MCP  │ ryeos CLI    │ HTTP │ ryeosd node  │
│ Amp/Claude…  │─────▶│ bundle verbs │─────▶│ execution +  │
╰──────────────╯      ╰──────────────╯      │ state        │
                                            ╰──────┬───────╯
                                                   │
                         ╭─────────────────────────┴─────────────────────────╮
                         │ signed bundles, CAS objects, refs, threads, vault │
                         ╰───────────────────────────────────────────────────╯
```

RyeOS is built around a few primitives:

| Primitive      | Meaning                                                                                                             |
| -------------- | ------------------------------------------------------------------------------------------------------------------- |
| **Item**       | A signed unit of behavior or context: `tool:`, `directive:`, `knowledge:`, graph, service, config, runtime, schema. |
| **Bundle**     | A signed distribution unit containing items, schemas, binaries, CLI descriptors, and publisher trust metadata.      |
| **Node**       | The local daemon and system space. It verifies bundles, executes items, owns state, and exposes HTTP services.      |
| **Thread**     | A tracked execution with lifecycle, events, result, parent/child relationships, cancellation, and replay.           |
| **CAS**        | The authoritative append-only state store. Hashes identify events, snapshots, manifests, and project objects.       |
| **Ref**        | A signed mutable pointer into CAS, such as a project head, chain head, or bundle registration.                      |
| **MCP bridge** | A local single-user adapter exposing one `cli` tool that shells out to the `ryeos` binary.                          |

## What you can do with it

### Execute signed behavior

```bash
ryeos execute tool:ryeos/core/identity/public_key
ryeos execute directive:apps/demo/chat --message "Summarize this project"
```

Tools are executable programs. Directives are LLM-facing workflows with
permissions, limits, context, and inheritance. Both are resolved from signed
bundle or project data before execution.

### Inspect and replay work

```bash
ryeos thread list
ryeos thread get <thread-id>
ryeos thread tail <thread-id>
ryeos events replay <thread-id>
```

Every execution runs as a thread. Threads make agent work observable instead of
being trapped inside a chat transcript.

### Run declarative workflows

State graphs describe multi-step workflows as YAML DAGs with conditional edges,
foreach execution, hooks, caching, and persisted state. Graphs run through the
same signed execution and thread machinery as tools and directives.

### Schedule recurring jobs

```bash
ryeos scheduler register <spec>
ryeos scheduler list
ryeos scheduler pause <id>
ryeos scheduler show-fires <id>
```

Schedules fire items on cron or interval rules. Each fire creates a normal
thread with normal history and result inspection.

### Push work to another node

```bash
ryeos remote configure --descriptor ./prod.remote.yaml
ryeos remote admit \
  --remote prod \
  --token "<one-time-token>" \
  --label dev-machine \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head"
ryeos remote doctor --remote prod
ryeos remote execute \
  --remote prod \
  --item-ref tool:my/heavy-compute \
  --project /absolute/path/to/project
```

Remote execution uses node keys, signed requests, scoped grants, and
content-addressed sync. A descriptor is a trust pin, not a credential; runtime
authority lives in the target node's authorized-key store.

## Install

### Arch Linux / AUR

```bash
yay -S ryeos ryeos-mcp
ryeos init
ryeos start
ryeos status
```

`ryeos init` discovers packaged bundles under `/usr/share/ryeos`, installs them
into the system space, creates operator and node keys, initializes trust and
vault material, and writes node configuration. `ryeos start` launches `ryeosd`.

The user lifecycle surface is intentionally small:

```bash
ryeos init
ryeos start
ryeos stop
ryeos status
```

### Docker image

The release workflow publishes a composed daemon image:

```bash
docker pull ghcr.io/leolilley/ryeosd-full:latest
```

The image includes `ryeosd`, `ryeos`, core tools, and signed bundle trees. It
uses `/data/user` and `/data/core` for persistent operator and system state.

### From source

```bash
git clone https://github.com/leolilley/ryeos.git
cd ryeos
cargo build
./scripts/dev-up.sh
```

`scripts/dev-up.sh` populates and signs bundles with the development publisher
key, initializes a repo-local `.local/ryeos` system space, and starts a daemon
against that isolated state.

## Using RyeOS from an AI client

The MCP adapter is deliberately thin. It exposes one tool, `cli`, which invokes
the `ryeos` binary. The available commands come from the installed signed
bundles, so adding a bundle can add CLI verbs without redeploying the MCP
server.

Example MCP configuration:

```json
{
  "mcpServers": {
    "ryeos": {
      "command": "ryeosd-mcp"
    }
  }
}
```

The MCP tool accepts argv for `ryeos`:

```json
{
  "tool": "cli",
  "args": ["execute", "tool:ryeos/core/identity/public_key"],
  "project_path": "/path/to/project"
}
```

The MCP server is for local single-user stdio use. Do not expose it directly on
the network without a separate authentication boundary.

## Bundles and trust

RyeOS behavior is shipped as bundles. A bundle may contain:

- item YAML and Markdown;
- schemas and composer rules;
- runtime and handler binaries;
- CLI command descriptors;
- knowledge docs;
- publisher trust metadata;
- content-addressed manifests and refs.

Installed bundles are verified before use. Bundle-owned binaries live inside the
signed bundle tree and are resolved by hash; they are not arbitrary programs
copied onto `PATH`.

The repository currently includes bundles such as:

| Bundle         | Purpose                                                                                     |
| -------------- | ------------------------------------------------------------------------------------------- |
| `core`         | Node, trust, identity, signing, state, service, and bundle primitives.                      |
| `standard`     | Agent-facing workflows: directives, tools, graphs, threads, scheduler, and common runtimes. |
| `web`          | Web-oriented tools and runtimes.                                                            |
| `studio`       | UI/operator-facing bundle assets.                                                           |
| `hosted-node`  | Policy for exposing a node as a hosted remote target.                                       |
| `central-auth` | Reusable app-level auth primitives for RyeOS-backed projects.                               |

## State model

RyeOS state follows a three-tier truth model:

| Tier              | Mutable? | Rebuildable? | Purpose                                                          |
| ----------------- | -------: | -----------: | ---------------------------------------------------------------- |
| CAS objects       |       No |          N/A | Authoritative events, snapshots, manifests, and project objects. |
| Signed refs       |      Yes |           No | Entry points into the CAS graph.                                 |
| SQLite projection |      Yes |          Yes | Query performance only.                                          |

Writes are CAS-first. If a projection update fails after the CAS write succeeds,
the daemon can rebuild the projection later by walking signed heads through CAS.
That makes the event graph the source of truth, not an incidental database file.

## Repository map

| Path                           | Purpose                                                                             |
| ------------------------------ | ----------------------------------------------------------------------------------- |
| `crates/kernel/lillux`         | Low-level signing, hashing, atomic IO, process, and primitive execution support.    |
| `crates/engine/ryeos-engine`   | Item resolution, composition, policy facts, and execution planning.                 |
| `crates/engine/ryeos-executor` | Execution dispatch and runtime integration.                                         |
| `crates/daemon/ryeos-app`      | The daemon application: HTTP services, state, remotes, lifecycle, and node runtime. |
| `crates/bin/cli`               | `ryeos`, the operator CLI and MCP target.                                           |
| `crates/bin/daemon`            | `ryeosd`, the local node daemon.                                                    |
| `crates/runtimes/*`            | Directive, graph, and knowledge runtimes.                                           |
| `crates/state/*`               | Durable state, scheduler, and vault crates.                                         |
| `crates/tools/*`               | Bundle-owned tool binaries and handler protocols.                                   |
| `bundles/*`                    | Signed bundle source trees.                                                         |
| `integrations/mcp/ryeosd`      | Python MCP stdio adapter exposing the `cli` tool.                                   |
| `scripts/`                     | Bundle population, validation, local install, and development workflows.            |
| `deploy/`                      | Container entrypoints and package metadata.                                         |

## Development

Use the repository scripts rather than hand-editing derived bundle state.

```bash
./scripts/gate.sh                 # populate/sign bundles, then run nextest
./scripts/gate.sh --no-tests      # refresh bundle bin/CAS/signatures only
./scripts/dev-up.sh               # fresh repo-local daemon in .local/ryeos
./scripts/pkg/install-local-direct.sh
```

Common loops:

| Change type                                | Recommended loop                                         |
| ------------------------------------------ | -------------------------------------------------------- |
| Rust-only compile feedback                 | `cargo build` or targeted `cargo test -p <crate>`        |
| Rust affecting bundled binaries            | `./scripts/gate.sh --no-tests`, then targeted tests.     |
| Bundle YAML, schemas, tools, or runtimes   | `./scripts/gate.sh` unless intentionally skipping tests. |
| Daemon/CLI behavior with installed bundles | `./scripts/dev-up.sh`.                                   |
| Packaged layout repair                     | `./scripts/pkg/install-local-direct.sh`.                 |

Hard rules for contributors and agents:

- Do not manually copy bundle-owned binaries into `/usr/bin` as a fix.
- Do not edit signed bundle YAML and leave stale signatures.
- Do not add hardcoded fallbacks for stale bundle state; regenerate bundles.
- Restart a running daemon after reinitializing bundles so in-memory registries
  match disk.

## License

MIT.
