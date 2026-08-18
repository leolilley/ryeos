<!-- ryeos:signed:2026-08-18T22:04:53Z:7d4c3c398d84d6a5cf94c0339549c68826a018045b30c0e499a8a57811084b15:1FplciaF2T+1l5CqiNGebIjSq0cuH4FcrKBxjeDnGUHnBL7UV8dZdi9VR3TQTwv3zylJaY9XdJYzMhFCk+4rAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [cli, quickstart, reference, llm, execute, remote, threads, offline]
version: "1.3.0"
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
  `~/.local/share/ryeos` (overridable via `RYEOS_APP_ROOT`). It contains
  installed bundles, node identity, operator signing keys and trust,
  config, vault state, and daemon state.
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

Custom app root:

```bash
ryeos init --app-root /tmp/ryeos-state --source /path/to/bundles
```

The init result reports the app-root path, operator key fingerprint,
node key fingerprint, vault fingerprint, and installed bundle names.

## 2. Start, stop, and inspect the local daemon

Lifecycle verbs (`init`, `start`, `stop`, `status`) and
`identity` are the only hardcoded CLI commands. Everything
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
ryeosd --app-root ~/.local/share/ryeos --bind 127.0.0.1:7400
```

Useful rules for agents:

- If a command says it cannot contact the daemon, run `ryeos status`.
- If aliases seem stale after installing bundles, restart the daemon.
- `ryeos identity` and `ryeos init` are useful before the
  daemon is running.
- `ryeos sign` uses the daemon when it is live and the same authoring service
  standalone when the node is stopped. `ryeos verify` and `ryeos fetch` are
  local inspections that may run alongside the daemon.

## 3. Command ownership modes

Commands come from signed bundle descriptors. Each service descriptor
declares an `availability` field:

- **`availability: local`** — runs as a local operation; the daemon may remain
  live. Examples include `verify`, `fetch`, `bundle verify`, and `bundle publish`.
- **`availability: stopped_node`** — requires exclusive stopped-node state,
  such as bundle replacement or projection rebuild.
- **`availability: both`** — prefers the live daemon and uses the same service
  standalone only when the node is stopped. Project `sign`, content pinning,
  and maintenance GC use this shape.
- **No `availability` field** (or daemon-only availability) — requires a
  running daemon. Most runtime commands fall here: `execute`, `thread`,
  `remote`, `events`, `scheduler`.

The CLI reads signed descriptors from the installed generation and applies the
declared ownership mode before dispatch. It never guesses that an unavailable
daemon means stopped-node authority.

Do not stop or restart the daemon to use `sign`, `verify`, or `fetch`.

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

## 6. Read, verify, sign, and fetch items locally

`verify` and `fetch` are local descriptor-driven commands and do not require a
daemon. `sign` is dual-mode: it uses the authenticated non-threaded daemon
service while the node is healthy and the same service under exclusive
stopped-node authority otherwise. You do not stop a healthy node merely to
sign a project item.

Inspect an item without running it:

```bash
ryeos --project /abs/project fetch --item-ref knowledge:ryeos/standard/cli-basics
ryeos --project /abs/project fetch --item-ref tool:apps/demo/echo --with-content
ryeos --project /abs/project fetch --item-ref directive:apps/demo/chat --verify
```

Verify signature and trust status:

```bash
ryeos --project /abs/project verify --item-ref knowledge:ryeos/standard/cli-basics
ryeos --project /abs/project verify --item-ref tool:apps/demo/echo
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

## 7. Bundle verify and publish locally

Bundle release commands are local and do not require a running daemon. They do
not take exclusive node-state ownership.

### Verify a bundle before publishing

Validate all bundle items, signatures, metadata anchoring, and manifest:

```bash
ryeos bundle verify --source bundles/standard
```

This is read-only — it never rewrites any files. Run it before
publishing to catch issues early.

### Publish a bundle (release pipeline)

Full release pipeline: bootstrap-sign, rebuild CAS, sign items, generate
manifest, emit trust doc:

```bash
ryeos bundle publish --source bundles/core
ryeos bundle publish --source bundles/standard --registry-root bundles/core --owner myname
ryeos bundle publish --source bundles/standard --no-trust-doc
```

Publish is **incremental and idempotent**. On a no-op run (no source
content changes), the second run produces no git diff. Only files that
actually changed are re-signed.

A doc-only edit does NOT require `bundle publish`. Just sign the
edited doc:

```bash
$EDITOR bundles/standard/.ai/knowledge/ryeos/standard/cli-basics.md
ryeos sign knowledge:ryeos/standard/cli-basics --project bundles/standard
git diff
git commit
```

## 8. Run tools, directives, and graphs locally (daemon-backed)

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

## 9. Understand execution output and threads

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
ryeos events replay T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

If a command returns a thread id and then fails later, inspect the thread
rather than rerunning blindly.

## 10. Vault and secrets

Do not pass secrets in normal parameters unless the item specifically
requires it. Put secrets in the node vault and let config reference them.

```bash
ryeos vault set API_KEY "$API_KEY"
ryeos vault list
ryeos vault delete API_KEY
```

For local operator maintenance outside the daemon, `ryeos-core-tools`
also supports stdin-based vault writes:

```bash
printf '%s' "$API_KEY" | ryeos-core-tools vault put --name API_KEY --value-stdin
```

Remote vault commands exist too, but require remote authorization:

```bash
ryeos remote vault-set prod API_KEY "$API_KEY"
ryeos remote vault-list prod
```

## 11. Remote setup and diagnostics

Remote commands are local daemon services that call another Rye daemon
with signed HTTP requests. They use the caller's **node key**.

Show the local node identity to a remote operator:

```bash
ryeos identity
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

## 12. Remote project workflows

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

## 13. Remote thread inspection

After remote execution, inspect remote threads directly:

```bash
ryeos remote threads prod --limit 20
ryeos remote thread-status prod --thread-id T-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```

These commands are useful when a remote run starts a thread but the local
CLI output is incomplete or a runtime fails on the remote node.

## 14. Troubleshooting checklist

Start here when something fails:

1. `ryeos status --json` — is the local daemon running?
2. `ryeos identity` — does local identity exist?
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

## 15. Command patterns to copy

Local project execution:

```bash
ryeos -p /abs/project execute tool:namespace/name --input params.json
```

Local directive chat-style execution:

```bash
ryeos -p /abs/project execute directive:apps/my-app/chat --message "Hello"
```

Local authoring and inspection:

```bash
ryeos --project /abs/project fetch --item-ref directive:apps/my-app/chat --with-content
ryeos --project /abs/project verify --item-ref directive:apps/my-app/chat
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
