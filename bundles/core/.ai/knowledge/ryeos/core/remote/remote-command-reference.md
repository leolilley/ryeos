<!-- ryeos:signed:2026-05-31T04:22:26Z:7ea675d5fe58247894804aabb89240d3ba7629793ff02a5096fdff562affc561:t4Q3QZrmGhEebmOLF4yb93/YQFgKDs1rH3e4OTYtXldflRG5la5YQFJx7F25B6yjtTDitoTMBZ5tElot+RrBBA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/core/remote
tags: [remote, cli, reference, manpage, capabilities]
version: "1.0.0"
description: >
  Manpage-style reference for ryeos remote commands, including local
  capabilities, remote authorized-key scopes, routes, examples, and
  failure modes.
---

# Remote Command Reference

Remote commands are local daemon services that call another ryeos daemon
through signed HTTP requests. Read this page together with
[Remote Operations](remote-operations.md), [HTTP API](../node/http-api.md),
and [Services: remote](../services/remote.md).

A remote command can require authority in two places:

1. **Local service capability** — the caller must be allowed to invoke
   the local `service:remote/...` orchestrator.
2. **Remote authorized-key scopes** — the target daemon must authorize
   the caller node key for the remote routes touched by the operation.

Remote HTTP requests are signed with the caller **node key**, not the
operator user key. Share `ryeos identity` output with the
remote operator when requesting access.

## Authority Matrix

| Command | Local service cap | Remote scopes on target | Remote routes used |
|---|---|---|---|
| `ryeos remote configure` | `ryeos.execute.service.remote.configure` | none | `GET /public-key`, `GET /ingest-ignore` |
| `ryeos admission-token` | local tool execution | none | none |
| `ryeos remote admit` | `ryeos.execute.service.remote.admit` | none before claim; claim creates requested grant | `GET /public-key`, `POST /admission/claim` |
| `ryeos remote list` | `ryeos.execute.service.remote.list` | none | none |
| `ryeos remote status` | `ryeos.execute.service.remote.status` | none | `GET /health`, `GET /public-key` |
| `ryeos remote doctor` | `ryeos.execute.service.remote.doctor` | signed auth probe; project status if `--project` is supplied | `GET /health`, `GET /public-key`, `GET /threads?limit=1`, optionally `POST /project/status` |
| `ryeos remote authorize` | `ryeos.execute.service.remote.admin` | `ryeos.execute.service.authorize.key` | `POST /authorize-key` |
| `ryeos remote push` | `ryeos.execute.service.remote.push` | `ryeos.execute.service.objects.has`, `ryeos.execute.service.objects.put`, `ryeos.execute.service.push.head` | `GET /ingest-ignore`, `POST /objects/has`, `POST /objects/put`, `POST /push-head` |
| `ryeos remote pull` | `ryeos.execute.service.objects.get` | `ryeos.execute.service.objects.get` | `POST /objects/get` |
| `ryeos remote execute` | `ryeos.execute.service.remote.admin` | push scopes + `ryeos.execute.service.objects.get` + caps required by the executed item | `GET /ingest-ignore`, `POST /objects/has`, `POST /objects/put`, `POST /push-head`, `POST /execute`, `POST /objects/get` |
| `ryeos remote run` | `ryeos.execute.service.remote.admin` | caps required by the executed item | `POST /execute` |
| `ryeos remote threads` | `ryeos.execute.service.remote.admin` | signed auth; no extra thread service cap in v1 | `GET /threads?limit=N` |
| `ryeos remote thread-status` | `ryeos.execute.service.remote.admin` | signed auth; no extra thread service cap in v1 | `GET /threads/{thread_id}` |
| `ryeos remote bundle-install` | `ryeos.execute.service.bundle.install` | `ryeos.execute.service.bundle.export`, `ryeos.execute.service.objects.get` | `POST /bundle/export`, `POST /objects/get` |
| `ryeos remote vault-set` | `ryeos.execute.service.remote.admin` | `ryeos.execute.service.vault.set` | `POST /vault/set` |
| `ryeos remote vault-list` | `ryeos.execute.service.remote.admin` | `ryeos.execute.service.vault.list` | `GET /vault/list` |
| `ryeos remote vault-delete` | `ryeos.execute.service.remote.admin` | `ryeos.execute.service.vault.delete` | `POST /vault/delete` |

Notes:

- `GET /health`, `GET /public-key`, and `GET /ingest-ignore` are
  intentionally unauthenticated discovery endpoints.
- `remote execute` must also satisfy the capabilities required by the
  executed item once the remote daemon dispatches `/execute`.
- `remote.admin` is a **local** umbrella capability for high-impact
  remote orchestration commands. It is not automatically sent to the
  target daemon and does not replace remote authorized-key scopes.

## `ryeos remote configure`

Add or update a named remote in local node config.

```bash
ryeos remote configure --remote prod --url https://ryeos.example.com
ryeos remote configure --descriptor ./prod.remote.yaml
```

The command fetches the remote node public key, principal id, vault
fingerprint, and ingest-ignore rules. Results are stored in
`<system_space_dir>/.ai/config/remotes/remotes.yaml`.

When `--descriptor` is supplied, the descriptor is a trust/discovery pin,
not a credential. `remote configure` still fetches the live `/public-key`
document and refuses to write config if the live node key or fingerprint
does not match the descriptor.

Failure modes:

- fails if the remote cannot be reached
- fails if `/public-key` or `/ingest-ignore` returns invalid data
- fails if a descriptor pins a different node key/fingerprint than the
  live remote reports
- later signed requests fail if the remote's node key changes and the
  remote config is not refreshed

## `ryeos admission-token`

Mint a one-time, node-local bootstrap token file on the target node.

```bash
ryeos admission-token \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head" \
  --ttl-secs 600
```

This is an offline/local operator tool. It writes a token hash file under
the target system space at `.ai/node/admission/tokens/<sha256>.toml` and
prints the cleartext token once. The token is not a bearer credential for
runtime requests; it can only be consumed by `/admission/claim`, where
the claimant must also sign the claim with the node key being admitted.

## `ryeos remote admit`

Consume a one-time admission token to authorize this caller node on a
configured remote.

```bash
ryeos remote admit \
  --remote prod \
  --token "<one-time-token>" \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects.has,ryeos.execute.service.objects.put,ryeos.execute.service.objects.get,ryeos.execute.service.push.head"
```

Before sending the token, the command fetches the live remote identity
and verifies it still matches the locally pinned remote config. The
target consumes the token and writes a normal node-signed authorized-key
grant for the caller node key.

## `ryeos remote list`

List locally configured remotes.

```bash
ryeos remote list
```

This is local-only and does not contact remote daemons.

## `ryeos remote status`

Check a remote's public identity and health.

```bash
ryeos remote status --remote prod
```

Uses unauthenticated discovery routes and a non-fatal signed probe. It
is useful for confirming URL reachability, key material, local node
identity, current authorization status, project bindings, and the
bootstrap `authorize-client` command to run on the remote host.

## `ryeos remote authorize`

Ask a remote node to authorize a public key.

```bash
ryeos remote authorize \
  --remote prod \
  --public-key "ed25519:<base64_pubkey>" \
  --label "ci-runner" \
  --scopes "ryeos.execute.service.objects.get"
```

This is a remote administrative operation. Your caller node must already
be authorized on the target for `ryeos.execute.service.authorize.key`.
For initial bootstrap, the remote operator can run `ryeos authorize-key`
locally on the remote node instead.

Remote authorization rejects wildcard scope delegation. Enumerate every
scope explicitly.

## `ryeos remote push`

Push the current project snapshot to a remote node.

```bash
ryeos remote push --remote prod --project /absolute/path/to/project
```

The push pipeline ingests local project content, uploads missing CAS
objects, and writes a principal-scoped remote pushed HEAD via
`/push-head`. It uses the remote's cached ingest-ignore rules; if the
cache is missing the handler fetches `/ingest-ignore` inline and aborts
if that fetch fails.

Failure modes:

- project path must be absolute and canonicalizable
- missing remote ignore rules fail closed if they cannot be fetched
- object upload or pushed-head write errors abort the push

## `ryeos remote pull`

Fetch CAS objects from a remote by hash.

```bash
ryeos remote pull --remote prod --hashes abc123 def456
ryeos remote pull --remote prod --hashes abc123 --output-dir /tmp/objects
```

Objects are stored in the local CAS. With `--output-dir`, blobs are
written as `<hash>` and JSON objects as `<hash>.json`.

Failure mode: fail-closed if any requested hash is missing on the
remote; the error reports all missing hashes.

## `ryeos remote execute`

Synchronously push local/user state, execute an item on the remote, pull
resulting snapshots, and apply changes back locally.

```bash
ryeos remote execute \
  --remote prod \
  --item-ref tool:my/heavy-compute \
  --project /absolute/path/to/project

ryeos remote execute \
  --remote prod \
  --item-ref tool:my/user-tool \
  --no-project
```

`--project` and `--no-project` are mutually exclusive. The daemon does
not discover a project from its own working directory; the CLI must send
an absolute project path or the explicit no-project sentinel.

Execution phases:

1. Push project state, or an empty project plus user-space manifest in
   `--no-project` mode.
2. Call remote `/execute` using the pushed project source.
3. Pull result objects and apply changed project/user files locally.

Clean-base guard: if local tracked files changed since the push, the
pull-back apply aborts without partial writes.

## `ryeos remote run`

Execute an item against a configured remote project without pushing or
pulling project state.

```bash
ryeos -p /absolute/path/to/project remote run \
  --remote prod \
  --item-ref directive:my/deployed-chat
```

Use this after `remote bind-project` and, for AI-managed deployments,
`remote sync-project-ai`. The command resolves the local project binding
and executes against the bound remote project path using the remote
daemon's live filesystem project. It is the preferred path for
`ai_only` bindings where the operator wants to run the deployed project,
not perform a full push/execute/pull cycle.

Project-aware remote commands accept `--project` either globally before
the verb or as the command's service field after the verb:

```bash
ryeos -p /absolute/path remote run prod tool:my/task
ryeos remote run prod tool:my/task --project /absolute/path
```

## `ryeos remote doctor`

Diagnose the remote operator setup path in one command.

```bash
ryeos remote doctor --remote prod
ryeos remote doctor prod --project /absolute/path/to/project
```

The report includes local node identity, remote configuration, remote
health/identity discovery, a signed authorization probe, project binding
status when `--project` is supplied, and next-step commands for bootstrap
authorization, binding, `sync-project-ai`, and `remote run`.

## `ryeos remote threads`

List threads on a remote node.

```bash
ryeos remote threads --remote prod --limit 50
```

Remote thread list/get services are authenticated, but v1 thread query
services do not require a dedicated service capability beyond signed
authorization. The local command still requires `remote.admin`.

## `ryeos remote thread-status`

Get one remote thread's detail.

```bash
ryeos remote thread-status --remote prod --thread-id T-abc123
```

Returns the same kind of thread detail as the remote's `/threads/{id}`
route: status, result, artifacts, and facets where available.

## `ryeos remote bundle-install`

Install a bundle from a remote node into the local system space.

```bash
ryeos remote bundle-install --remote prod --bundle-name standard
```

The remote exports bundle files as CAS object hashes. The caller fetches
all required blobs, materializes the local bundle directory, runs
preflight verification, writes a signed node-config bundle registration,
and bumps the local engine-cache generation.

Failure modes:

- local target bundle already exists
- remote bundle export is empty or unauthorized
- any required blob is missing
- preflight verification fails; partial materialization is cleaned up

This differs from local `bundle install/remove`, which are offline-only.
Remote bundle install is daemon-only and updates live local node state.

## `ryeos remote vault-set`

Set a secret in the remote node vault.

```bash
ryeos remote vault-set --remote prod --name API_KEY --value "sk-..."
```

The value is sent in the signed HTTP request body. Production deployments
must use TLS; request signing authenticates and protects integrity but is
not transport encryption.

## `ryeos remote vault-list`

List secret names in the remote node vault.

```bash
ryeos remote vault-list --remote prod
```

## `ryeos remote vault-delete`

Delete a secret from the remote node vault.

```bash
ryeos remote vault-delete --remote prod --name API_KEY
```

## Troubleshooting

- **401/403 from target daemon**: the caller node key is not authorized
  on the remote, or the authorized-key scopes do not include the route's
  required capability.
- **signature/audience errors**: re-run `ryeos remote configure`; the
  target node key or principal id may have changed.
- **push uses unexpected ignore rules**: re-run `ryeos remote configure`
  to refresh the cached remote ingest-ignore config.
- **remote execute applies no files**: check the JSON response's
  `project_applied` and `user_applied` sections and inspect clean-base
  guard errors.
