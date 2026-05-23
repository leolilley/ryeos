<!-- ryeos:signed:2026-05-23T07:18:13Z:511866ec3cf3814dbe9a4cb31d3c32383478039bd675767f9c80436046b6ca03:Kpyh+sgaLBReM3STQpLOeRucnzLKjvBxHiZzqhIYprqN+YUALXoBVJF7Jw0Z1l0b2PbXR+JfVV1Q17thJrOZDA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard
tags: [cli, quickstart, reference, llm, execute, remote, threads, offline]
version: "1.1.0"
description: >
  LLM-facing quickstart for using the ryeos CLI from initialization through
  local execution, project execution, thread inspection, and remote execution.
---

# Rye CLI Basics for Agents

This page is the fast operational reference to give an LLM when it needs
to use Rye OS from a shell. It covers the normal path from a fresh install
through running local and remote work.

Terminology:

- **Project root**: a directory containing `.ai/` for project-specific
  directives, tools, graphs, knowledge, and config.
- **System space**: the local Rye installation state, usually
  `~/.local/share/ryeos`. It contains installed bundles, node identity,
  config, vault state, and daemon state.
- **User space**: user-level `.ai/` content and signing keys.
- **Bundle**: an installed `.ai/` tree such as `core` or `standard`.
- **Canonical ref**: `kind:path`, for example
  `tool:apps/demo/echo`, `directive:apps/demo/chat`,
  `graph:jobs/report`, `knowledge:apps/demo/notes`.

## 1. Initialize Rye

Run init once per machine or whenever installed bundles need refreshing.
`init` is daemon-independent.

Packaged install:

```bash
ryeos init
```

Development checkout:

```bash
ryeos init \
  --source /path/to/ryeos/bundles \
  --trust-file /path/to/ryeos/bundles/core/PUBLISHER_TRUST.toml \
  --trust-file /path/to/ryeos/bundles/standard/PUBLISHER_TRUST.toml
```

Custom system space:

```bash
ryeos init --system-space-dir /tmp/ryeos-state --source /path/to/bundles
```

The init result reports the system-space path, user key fingerprint,
node key fingerprint, vault fingerprint, and installed bundle names.

## 2. Start, stop, and inspect the local daemon

Lifecycle verbs (`init`, `start`, `stop`, `status`) and
`identity public-key` are the only hardcoded CLI commands. Everything
else is descriptor-driven from installed bundles.

Start the daemon after `init`:

```bash
ryeos start
ryeos status
ryeos status --json
ryeos stop
```

If the daemon is managed outside `ryeos start`, run it directly:

```bash
ryeosd --system-space-dir ~/.local/share/ryeos --bind 127.0.0.1:7400
```

Useful rules for agents:

- If a command says it cannot contact the daemon, run `ryeos status`.
- If aliases seem stale after installing bundles, restart the daemon.
- `ryeos identity public-key` and `ryeos init` are useful before the
  daemon is running.
- `ryeos sign`, `ryeos verify`, and `ryeos fetch` work offline without a
  daemon (see Section 6).

## 3. Offline vs daemon commands

Commands come from signed bundle descriptors. Each service descriptor
declares an `availability` field:

- **`availability: offline`** — runs in the CLI process, no daemon required.
  These are source-tree authoring operations: `sign`, `verify`, `fetch`.
- **No `availability` field** (or `availability: daemon`) — requires a
  running daemon. Most runtime commands fall here: `execute`, `thread`,
  `remote`, `events`, `scheduler`.

The CLI reads descriptors from installed bundles on disk and dispatches
offline-capable commands in-process. If a command is not offline-capable
and the daemon is not running, you get a clear error.

Do not run `ryeos start` or restart the daemon to use `sign`, `verify`,
or `fetch`. They work without it.

## 4. Ask for help and discover commands

Use help first when unsure about a command shape:

```bash
ryeos --help
ryeos fetch --help
ryeos execute --help
ryeos remote doctor --help
```

The top-level help shows lifecycle verbs and all commands discovered from
installed bundle descriptors. Use `ryeos help <verb>` for verb-specific
usage and field schema.

## 5. Always set the project root for project work

When running project items, pass the project root explicitly. Do not rely
on the daemon's current directory.

Global project flag:

```bash
ryeos --project /absolute/path/to/project fetch tool:apps/demo/echo
ryeos --project /absolute/path/to/project execute tool:apps/demo/echo
```

Short global flag:

```bash
ryeos -p /absolute/path/to/project execute directive:apps/demo/chat --message "hello"
```

Some project-aware aliases also accept `--project` after the verb:

```bash
ryeos remote doctor prod --project /absolute/path/to/project
ryeos remote run prod tool:apps/demo/echo --project /absolute/path/to/project
```

Prefer absolute paths. If the command has `--project` and `--no-project`,
choose exactly one.

## 6. Read, verify, sign, and fetch items (offline)

`sign`, `verify`, and `fetch` are offline descriptor-driven commands.
They do not require a running daemon. They work by reading descriptors
from installed bundles and running in-process.

Inspect an item without running it:

```bash
ryeos fetch --item-ref knowledge:ryeos/standard/cli-basics --project-path /abs/project
ryeos fetch --item-ref tool:apps/demo/echo --project-path /abs/project --with-content
ryeos fetch --item-ref directive:apps/demo/chat --project-path /abs/project --verify
```

Verify signature and trust status:

```bash
ryeos verify --item-ref knowledge:ryeos/standard/cli-basics --project-path /abs/project
ryeos verify --item-ref tool:apps/demo/echo --project-path /abs/project
```

After editing a signed Rye item, sign it:

```bash
ryeos sign knowledge:ryeos/standard/cli-basics --project /abs/project
ryeos sign directive:apps/demo/chat --project /abs/project
```

Sign supports glob patterns for batch operations:

```bash
ryeos sign "tool:ryeos/core/*" --project /abs/project
```

These commands are safe to use during bundle authoring. A full bundle
publish is not needed for doc-only edits.

## 7. Run tools, directives, and graphs locally (daemon-backed)

Execute by canonical ref. This requires a running daemon:

```bash
ryeos -p /path/to/project execute tool:apps/demo/echo --name Alice
ryeos -p /path/to/project execute directive:apps/demo/chat --message "Summarize this"
ryeos -p /path/to/project execute graph:jobs/report --date 2026-05-23
```

Simple parameters can be flags or key-value tokens:

```bash
ryeos execute tool:demo/echo --name Alice --verbose
ryeos execute tool:demo/echo name=Alice verbose=true
```

For nested JSON, arrays, numbers, booleans, or exact types, use
`--input` with a JSON object:

```bash
cat > /tmp/params.json <<'JSON'
{
  "message": "What changed this week?",
  "history": "",
  "options": { "limit": 5, "include_sources": true }
}
JSON

ryeos -p /path/to/project execute directive:apps/demo/chat --input /tmp/params.json
```

Stdin form:

```bash
echo '{"name":"Alice","count":3}' | \
  ryeos -p /path/to/project execute tool:apps/demo/echo --input -
```

## 8. Understand execution output and threads

Executions normally return JSON containing thread metadata and result
data. Important fields:

- `thread.thread_id`: durable thread id for inspection.
- `thread.status`: `completed`, `running`, or `failed`.
- `result`: the item-specific output.
- `error`: failure details if the runtime failed.

Inspect thread history with standard thread verbs:

```bash
ryeos thread list
ryeos thread get T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
ryeos thread tail T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
ryeos events replay --thread-id T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

If a command returns a thread id and then fails later, inspect the thread
rather than rerunning blindly.

## 9. Vault and secrets

Do not pass secrets in normal parameters unless the item specifically
requires it. Put secrets in the node vault and let config reference them.

```bash
ryeos vault set --name API_KEY --value "$API_KEY"
ryeos vault list
ryeos vault delete --name API_KEY
```

For local operator maintenance outside the daemon, `ryeos-core-tools`
also supports stdin-based vault writes:

```bash
printf '%s' "$API_KEY" | ryeos-core-tools vault put --name API_KEY --value-stdin
```

Remote vault commands exist too, but require remote authorization:

```bash
ryeos remote vault-set prod --name API_KEY --value "$API_KEY"
ryeos remote vault-list prod
```

## 10. Remote setup and diagnostics

Remote commands are local daemon services that call another Rye daemon
with signed HTTP requests. They use the caller's **node key**.

Show the local node identity to a remote operator:

```bash
ryeos identity public-key
```

Configure a named remote:

```bash
ryeos remote configure prod --url https://ryeos.example.com
ryeos remote list
ryeos remote status prod
```

Diagnose the full setup:

```bash
ryeos remote doctor prod
ryeos remote doctor prod --project /path/to/project
```

`remote doctor` checks remote config, health, identity, signed
authorization, project binding, and deployed project status when a
project is supplied. It also prints next-step commands.

If authorization fails, the remote operator must authorize your node key
on the remote host with scopes for the requested operation.

## 11. Remote project workflows

There are two common remote execution modes.

### Push, execute, and pull back

Use `remote execute` when the current local project state should be sent
to the remote for this run and results should be pulled back:

```bash
ryeos remote execute prod tool:apps/demo/compute --project /path/to/project
```

This performs a push, remote `/execute`, and pull/apply. It needs object
upload/download scopes plus the capability required by the executed item.

### Run against an already deployed remote project

Use `remote run` when the remote has a bound project path and you want to
execute against the remote's live filesystem, not push local state:

```bash
ryeos remote bind-project prod \
  --project /local/project \
  --remote-project /data/projects/my-app \
  --sync-scope ai_only

ryeos remote sync-project-ai prod --project /local/project
ryeos remote run prod directive:apps/demo/chat --project /local/project
```

`remote run` is the preferred flow for deployed app agents where `.ai/`
content has already been synchronized to the node.

For complex item parameters through `remote run`, call the service escape
hatch with `--input`:

```bash
cat <<'JSON' | ryeos execute service:remote/run --input -
{
  "remote": "prod",
  "item_ref": "directive:apps/demo/chat",
  "project": "/local/project",
  "parameters": {
    "message": "Run the deployed analysis"
  }
}
JSON
```

## 12. Remote thread inspection

After remote execution, inspect remote threads directly:

```bash
ryeos remote threads prod --limit 20
ryeos remote thread-status prod --thread-id T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

These commands are useful when a remote run starts a thread but the local
CLI output is incomplete or a runtime fails on the remote node.

## 13. Troubleshooting checklist

Start here when something fails:

1. `ryeos status --json` — is the local daemon running?
2. `ryeos identity public-key` — does local identity exist?
3. `ryeos -p /project fetch <ref> --with-content` — does the item resolve?
4. `ryeos -p /project verify <ref>` — is it signed and trusted?
5. `ryeos -p /project execute <ref> --input params.json` — can it run locally?
6. `ryeos remote status <name>` — is the remote reachable?
7. `ryeos remote doctor <name> --project /project` — is auth and project binding correct?
8. `ryeos remote threads <name>` — did the remote create a thread?

Common fixes:

- Re-run `ryeos init` after installing new bundles.
- Restart the daemon after bundle or route changes.
- Use an absolute `--project` path.
- Use `--input` for non-string parameters.
- Sign edited items before running them.
- Ask the remote operator to grant the exact missing capability shown in
  a `403 Forbidden` error.

## 14. Command patterns to copy

Local project execution:

```bash
ryeos -p /abs/project execute tool:namespace/name --input params.json
```

Local directive chat-style execution:

```bash
ryeos -p /abs/project execute directive:apps/my-app/chat --message "Hello"
```

Offline authoring (no daemon required):

```bash
ryeos fetch --item-ref directive:apps/my-app/chat --project-path /abs/project --with-content
ryeos verify --item-ref directive:apps/my-app/chat --project-path /abs/project
ryeos sign directive:apps/my-app/chat --project /abs/project
```

Remote diagnostics:

```bash
ryeos remote doctor prod --project /abs/project
```

Remote deployed execution:

```bash
ryeos remote run prod directive:apps/my-app/chat --project /abs/project
```

Remote execution with structured parameters:

```bash
cat params.json | ryeos execute service:remote/run --input -
```

Thread inspection:

```bash
ryeos thread get T-...
ryeos remote thread-status prod --thread-id T-...
```
