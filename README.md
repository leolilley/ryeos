# RyeOS

> _In Linux, everything is a file. In RyeOS, everything is data._

RyeOS is portable verified execution. That portability describes signed
execution data moving between compatible nodes; the current production
distribution is Linux x86-64. See the
[platform support matrix](bundles/standard/.ai/knowledge/ryeos/core/platform-support.md).

(_RYE Your Execution_.)

Work in RyeOS — a tool run, a multi-step workflow, a scheduled job — is
data: signed, content-addressed, durable. Durability here means the explicit
[filesystem and recovery contract](docs/architecture/filesystem-durability.md),
not cross-filesystem transactions or identical guarantees on every platform.
Because it is data, it can prove
what it is, who authorized it, and what it actually did. And because it is
data, it can move: to another machine, across a restart, into the future.
A run whose process dies resumes from its own record. Work pushed to
another node carries its trust with it instead of borrowing the machine's.

That is the whole idea. Everything in this repository is that one property
at a different layer:

- **Signed items and bundles** — behavior you can install and trust because
  resolution verifies its signature and content identity. Enforced isolationing
  additionally pins verified entry bytes through execution.
- **Threads** — every execution has an identity and a durable event log.
  The log _is_ the run: tail it live, replay it, resume it, cancel it.
- **Keys** — the only actors. An operator, a node, an agent: each is a
  signing key, and anything that acts, acts by signing. Every piece of
  work traces back to the key that stands behind it, and trust is always
  a decision about a key, never about a machine.
- **Remotes** — push work to another node with signed requests and scoped
  grants. Trust travels as data, never as ambient machine access.
- **Content-addressed state** — history is the source of truth; databases
  and even running processes are rebuildable projections of it.

None of this is AI-specific — remove every LLM runtime and the property
stands. But an execution substrate that doesn't care what the executor is
turns out to be exactly what LLM work needs: directives make an LLM call
into a signed, durable, resumable execution like any other, and an agent
is simply a signing key with a body of signed work — not a process, not a
session.

## Mental model

```text
╭──────────────╮  MCP: one cli tool
│ AI client    │──────╮
╰──────────────╯      ▼
╭──────────────╮  ╭──────────────╮       ╭──────────────╮                 ╭─────────────╮
│ operator     │─▶│ ryeos CLI    │ HTTP  │ ryeosd node  │ signed requests │ other nodes │
╰──────────────╯  │ bundle verbs │──────▶│ execution +  │◀───────────────▶│             │
                  ╰──────────────╯       │ state        │    CAS sync     ╰─────────────╯
                                         ╰──────┬───────╯
                                                │
                      ╭─────────────────────────┴─────────────────────────╮
                      │ signed bundles, CAS objects, refs, threads, vault │
                      ╰───────────────────────────────────────────────────╯
```

The node, `ryeosd`, is where data becomes act: it holds keys, checks
signatures, executes at the frontier, and owns durable state. It is
deliberately the least special part of the system — any node with the right
trust can be the site of an execution, because everything that matters is in
the data.

RyeOS is built around a few primitives:

| Primitive      | Meaning                                                                                                             |
| -------------- | ------------------------------------------------------------------------------------------------------------------- |
| **Item**       | A signed unit of behavior or context: `tool:`, `directive:`, `knowledge:`, graph, service, config, runtime, schema. |
| **Bundle**     | A signed distribution unit containing items, schemas, binaries, CLI descriptors, and publisher trust metadata.      |
| **Node**       | The local daemon and system space. It verifies bundles, executes items, owns state, and exposes HTTP services.      |
| **Thread**     | A tracked execution: event log, lifecycle, lineage, continuation chain, receipts, cancellation, and replay.         |
| **CAS**        | The authoritative append-only state store. Hashes identify events, snapshots, manifests, and project objects.       |
| **Ref**        | A signed mutable pointer into CAS, such as a project head, chain head, or bundle registration.                      |
| **MCP bridge** | A local single-user adapter exposing one `cli` tool that shells out to the `ryeos` binary.                          |

## What you can do with it

### Execute signed behavior

```bash
ryeos execute tool:ryeos/core/identity/public_key
ryeos execute directive:ryeos/examples/continuing_research
```

Tools are executable programs. Directives are LLM-evaluated programs with
permissions, limits, context, and inheritance. Both are resolved from signed
bundle or project data before execution.

### Trace, steer, and replay executions

```bash
ryeos thread list
ryeos thread get <thread-id>
ryeos thread tail <thread-id>        # live event stream
ryeos thread children <thread-id>    # lineage-linked child threads
ryeos thread chain <thread-id>       # continuation chain
ryeos thread cancel <thread-id>
ryeos events replay <thread-id>
```

The event log is the execution, so tailing a thread is watching the execution
object grow at its frontier, and replay is reading it back. Steering and
cancellation act on the same control plane the node uses internally.

### Run declarative workflows

State graphs describe multi-step programs as YAML DAGs with conditional edges,
foreach execution, hooks, caching, and persisted state. Graphs run through the
same signed execution and thread machinery as tools and directives: long runs
continue across segment cuts as chained threads, and work fans out into
detached, lineage-linked child threads.

### Schedule recurring work

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
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"
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

The supported production target is Linux 6.9 or newer on x86-64 with glibc.
The kernel floor supplies the pidfd process-group and authenticated Unix-peer
primitives used for durable cancellation and lifecycle control. Official
container images are currently `linux/amd64`, and packaged bundle executables target
`x86_64-unknown-linux-gnu`. Other targets are tracked in the
[platform support matrix](bundles/standard/.ai/knowledge/ryeos/core/platform-support.md) and must not silently bypass
the isolation or durability contracts.

### Arch Linux / AUR

AUR packages (`ryeos`, `ryeos-mcp`) are coming soon. Once published:

```bash
yay -S ryeos ryeos-mcp
ryeos init
ryeos start
ryeos node status
```

`ryeos init` discovers packaged bundles under `/usr/share/ryeos`, installs them
into the system space, creates operator and node keys, initializes trust and
vault material, and writes node configuration. The signed
`sandbox-linux-bubblewrap` bundle contains its adapter and launcher; RyeOS has
no host Bubblewrap package dependency. The isolation policy defaults to
`mode: disabled`. `ryeos start`
launches `ryeosd`. See the
[execution isolation contract](bundles/standard/.ai/knowledge/ryeos/core/node/execution-isolation.md) before
enabling or tightening the node-owned policy.

The user lifecycle surface is intentionally small:

```bash
ryeos init          # bootstrap operator keys, trust, and bundles
ryeos start         # bring the local node online
ryeos stop          # stop it
ryeos node status   # local node lifecycle status
ryeos node doctor   # offline "why won't it start" checklist
```

### Docker image

The release workflow publishes a composed daemon image:

```bash
docker pull ghcr.io/leolilley/ryeos-standard:latest
```

The image includes `ryeosd`, `ryeos`, core tools, and signed bundle trees. The
entrypoint runs `ryeos init` on every boot (idempotent) before starting
`ryeosd`; the app root lives at `/data/app` on the persistent `/data` volume,
so keys, trust, and runtime state survive redeploys. Release containers rely
only on the official publisher key compiled into `ryeos`; the entrypoint does
not infer trust from files baked into the image. The initialized isolation policy
defaults to disabled, so the normal container profile needs no extra namespace
capabilities. Bubblewrap remains installed in the image for operators who opt
in to enforcement. Keep `/data` on a named volume:

```bash
docker volume create ryeos-data
docker run -d --name ryeos \
  -p 8000:8000 \
  -v ryeos-data:/data \
  ghcr.io/leolilley/ryeos-standard:latest
docker exec ryeos ryeos node status
```

To opt in to enforced Bubblewrap isolation, change the node-owned policy,
recreate the container with the required namespace/AppArmor profile, and
verify both the running daemon's immutable snapshot and the backend check:

```bash
docker exec ryeos sed -i 's/^mode: disabled$/mode: enforce/' \
  /data/app/.ai/node/isolation.yaml
docker rm -f ryeos
docker run -d --name ryeos \
  --cap-add SYS_ADMIN \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  -p 8000:8000 \
  -v ryeos-data:/data \
  ghcr.io/leolilley/ryeos-standard:latest
docker exec ryeos ryeos daemon status
docker exec ryeos ryeos node doctor --json
```

A locally built image signed by a development or custom publisher requires an
explicit trust acknowledgement at startup:

```bash
docker run -e RYEOS_TRUST_BAKED_PUBLISHERS=1 ryeosd-full:dev
```

That switch pins the image's `PUBLISHER_TRUST.toml` files before preflight. Do
not use it for release images. See the
[official publisher trust contract](bundles/standard/.ai/knowledge/ryeos/core/node/operator-init.md#official-publisher-trust)
for the complete operator contract.

The release gate exercises two distinct profiles: default-disabled startup and
signed execution without extra capabilities, then explicitly enforced startup,
verification of the running daemon's isolation snapshot, backend diagnostics, and
signed execution with the namespace/AppArmor capability profile. Back up the
`ryeos-data` volume before upgrades; it contains node identity, trust, vault,
and durable execution state.

### From source

Source installs stage the selected isolation implementation as a signed bundle.
No host process-confinement package is required in either policy mode.

```bash
git clone https://github.com/leolilley/ryeos.git
cd ryeos
cargo build
./scripts/pkg/install-local-direct.sh --trust-source-publishers
```

`scripts/pkg/install-local-direct.sh` installs the current built artifacts into
the local packaged layout and initializes the user system space. It does not
refresh bundle artifacts by default. Checkout bundles are normally signed by
the development publisher, so the example makes that trust decision explicit.
Without `--trust-source-publishers`, the installer accepts only the official
publisher compiled into `ryeos` and rejects any source-supplied publisher
document whose decoded key is non-official before changing the installed node. Use
`scripts/pkg/install-local-direct.sh --populate --trust-source-publishers` only
when bundle-owned binaries, CAS manifests, or signed bundle outputs actually
need to be regenerated.

## Five-minute first run

After installation, initialize the local system space and start the node:

```bash
ryeos init
ryeos start
ryeos node status
```

Open either operator surface from a project directory:

```bash
cd /path/to/project
ryeos tui
# or
ryeos web
```

Run a signed example and inspect its durable thread:

```bash
ryeos execute directive:ryeos/examples/continuing_research
ryeos thread list
ryeos thread tail <thread-id>
```

To exercise the core recovery guarantee, stop and restart the node while the
example is active, then inspect the same thread and its continuation chain:

```bash
ryeos stop
ryeos start
ryeos thread get <thread-id>
ryeos thread chain <thread-id>
```

If startup fails, `ryeos node doctor` performs the offline lifecycle and state
checks. The terminal and browser clients are projections over the same durable
threads; closing either client does not cancel the underlying work.

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

RyeOS behavior is shipped as bundles — installable signed `.ai/` trees. A
bundle may contain:

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

| Bundle         | Purpose                                                                                  |
| -------------- | ---------------------------------------------------------------------------------------- |
| `core`         | Node, trust, identity, signing, state, service, and bundle primitives.                   |
| `standard`     | Execution-facing workflows: directives, tools, graphs, threads, scheduler, and runtimes. |
| `web`          | Web-oriented tools and runtimes.                                                         |
| `browser`      | Browser automation tools.                                                                |
| `ryeos-ui`       | UI/operator-facing bundle assets.                                                        |
| `hosted-node`  | Policy for exposing a node as a hosted remote target.                                    |
| `central-auth` | Reusable app-level auth primitives for RyeOS-backed projects.                            |

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
The same tiering applies one level up: a running process is a rebuildable
projection of its thread's durable state.

## Repository map

| Path                           | Purpose                                                                             |
| ------------------------------ | ----------------------------------------------------------------------------------- |
| `crates/kernel/lillux`         | Low-level signing, hashing, atomic IO, process, and primitive execution support; see the [durability matrix](docs/architecture/filesystem-durability.md). |
| `crates/engine/ryeos-engine`   | Item resolution, composition, policy facts, and execution planning.                 |
| `crates/engine/ryeos-executor` | Execution dispatch and runtime integration.                                         |
| `crates/daemon/*`              | Daemon crates: app core, HTTP API, bundle install, node lifecycle, and UI assets.   |
| `crates/bin/cli`               | `ryeos`, the operator CLI and MCP target.                                           |
| `crates/bin/daemon`            | `ryeosd`, the local node daemon.                                                    |
| `crates/clients/*`             | Client surfaces: shared ryeos-ui base, terminal, and web.                             |
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
./scripts/gate.sh                         # run workspace tests without refreshing bundles
./scripts/gate.sh --refresh-bundles       # explicit expensive bundle refresh, then tests
./scripts/pkg/install-local-direct.sh --trust-source-publishers  # install dev-signed artifacts
./scripts/pkg/install-local-direct.sh --populate --trust-source-publishers  # refresh, then install
```

Common loops:

| Change type                                | Recommended loop                                                                                      |
| ------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| Rust-only compile feedback                 | `cargo build` or targeted `cargo test -p <crate>`                                                     |
| Rust affecting bundled binaries            | Targeted `cargo build --release -p <owner>`, then explicit bundle refresh only if needed.             |
| Bundle YAML, schemas, tools, or runtimes   | Targeted signing/publish flow; use `./scripts/gate.sh --refresh-bundles` only for release validation. |
| Browser UI assets                          | `./scripts/dev-ui-assets.sh --background --open`; no bundle refresh.                                  |
| Daemon/CLI behavior with installed bundles | `./scripts/pkg/install-local-direct.sh --trust-source-publishers` after building touched binaries.    |
| Packaged layout repair                     | Add `--populate --trust-source-publishers` only when artifacts must be regenerated.                   |

Hard rules for contributors and agents:

- Do not manually copy bundle-owned binaries into `/usr/bin` as a fix.
- Do not edit signed bundle YAML and leave stale signatures.
- Do not add hardcoded fallbacks for stale bundle state; regenerate bundles.
- Restart a running daemon after reinitializing bundles so in-memory registries
  match disk.

## License

MIT.
