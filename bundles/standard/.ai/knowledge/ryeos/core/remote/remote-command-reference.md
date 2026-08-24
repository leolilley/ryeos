<!-- ryeos:signed:2026-08-24T16:23:36Z:391101d150fe4cd7f1f5f0ceaad30e84b860da5535fa41007f7e7a4886e2625c:Ah5Oz21xr6x1JxWZ20sUhFG556yJrjF/dn/PAjJ6r2tZ5xAl/yxrFn2kNeYBm+fFfmf5kbyAaMeY/6p5QuqfBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Remote HTTP requests are normally signed with the caller **node key**. Share
`ryeos identity` output with the remote operator when requesting ordinary
remote access. The generic `remote/push` and `remote/run` services additionally
support an explicit configured-operator continuity mode for durable workflows:
the local request must already be signed by the configured operator, and the
target must separately authorize that same operator key as `remote_operator`,
bound by the target node to the source's canonical `site:` ID and exact scopes.
The target must also admit the source node key as `remote_node` with
`ryeos.attest.request.forwarded-operator`. Each forwarded operator request is
co-signed by that source-node key over the exact operator authorization. The
operator grant is therefore an allowed-source constraint; the co-signature is
the transit proof. This mode is never selected implicitly and is not available
to delegated callers.

## Authority Matrix

| Command | Local service cap | Remote scopes on target | Remote routes used |
|---|---|---|---|
| `ryeos remote configure` | `ryeos.execute.service.remote/configure` | none | `GET /public-key`, `GET /ingest-ignore` |
| `ryeos remote-descriptor` | local tool execution | none | none |
| `ryeos admission-token` | local tool execution | none | none |
| `ryeos remote admit` | `ryeos.execute.service.remote/admit` | none before claim; claim creates requested grant | `GET /public-key`, `POST /admission/claim` |
| `ryeos remote list` | `ryeos.execute.service.remote/list` | none | none |
| `ryeos remote status` | `ryeos.execute.service.remote/status` | none | `GET /health`, `GET /public-key` |
| `ryeos remote doctor` | `ryeos.execute.service.remote/doctor` | signed auth probe; project status if `--project` is supplied | `GET /health`, `GET /public-key`, `GET /threads?limit=1`, optionally `POST /project/status` |
| `ryeos remote authorize` | `ryeos.execute.service.remote/admin` | `ryeos.execute.service.identity/authorize-key` | `POST /authorize-key` |
| `ryeos remote push` / `service:remote/push` | `ryeos.execute.service.remote/push` | `ryeos.execute.service.objects/has`, `ryeos.execute.service.objects/put`, `ryeos.execute.service.system/push-head` | `GET /ingest-ignore`, `POST /objects/has`, `POST /objects/put`, `POST /push-head` |
| `ryeos remote pull` | `ryeos.execute.service.objects/get` | `ryeos.execute.service.objects/get` | `POST /objects/get` |
| `ryeos remote execute` | `ryeos.execute.service.remote/admin` | push scopes + `ryeos.execute.service.objects/get` + caps required by the executed item | `GET /ingest-ignore`, `POST /objects/has`, `POST /objects/put`, `POST /push-head`, `POST /execute`, `POST /objects/get` |
| `ryeos remote run` | `ryeos.execute.service.remote/admin` | caps required by the executed item | `POST /execute` |
| `ryeos remote threads` | `ryeos.execute.service.remote/admin` | signed auth; no extra thread service cap in v1 | `GET /threads?limit=N` |
| `ryeos remote thread-status` | `ryeos.execute.service.remote/admin` | signed auth; no extra thread service cap in v1 | `GET /threads/{thread_id}` |
| `ryeos remote bundle-install` | `ryeos.execute.service.bundle/install` | `ryeos.execute.service.bundle/export`, `ryeos.execute.service.objects/get` | `POST /bundle/export`, `POST /objects/get` |
| `ryeos remote vault-set` | `ryeos.execute.service.remote/admin` | `ryeos.execute.service.vault/set` | `POST /vault/set` |
| `ryeos remote vault-list` | `ryeos.execute.service.remote/admin` | `ryeos.execute.service.vault/list` | `GET /vault/list` |
| `ryeos remote vault-delete` | `ryeos.execute.service.remote/admin` | `ryeos.execute.service.vault/delete` | `POST /vault/delete` |

Notes:

- `GET /health`, `GET /public-key`, and `GET /ingest-ignore` are
  intentionally unauthenticated discovery endpoints.
- `remote execute` must also satisfy the capabilities required by the
  executed item once the remote daemon dispatches `/execute`.
- `remote.admin` is a **local** umbrella capability for high-impact
  remote orchestration commands. It is not automatically sent to the
  target daemon and does not replace remote authorized-key scopes.
- `outbound_principal: configured_operator` on `service:remote/push`, and a
  retained-current-HEAD accepted launch through `service:remote/run`, require
  the exact configured operator locally and preserve that principal remotely.
  The target grant and independently admitted source-node co-signature preserve
  remote origin, so these calls do not satisfy local-only operator checks.
  Their remote HEAD and session ownership do not interoperate with a HEAD
  previously pushed under the default node principal.

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

## `ryeos remote-descriptor`

Export a local descriptor trust pin for this node.

```bash
ryeos remote-descriptor \
  --name prod \
  --url https://ryeos.example.com \
  --capabilities "remote-execute,bundle-install" \
  --output ./prod.remote.yaml
```

This is local operator tooling. It reads the node's
`.ai/node/identity/public-identity.json`, emits descriptor YAML with the
node public key and fingerprint, and optionally writes it to disk. The
descriptor can be hosted, emailed, checked into infra, or served by any
provider-specific layer. It is still only a trust/discovery pin: callers
must import it with `remote configure --descriptor`, which verifies the
pin against the live `/public-key` response.

## `ryeos admission-token`

Mint a one-time, node-local bootstrap token file on the target node.

```bash
ryeos admission-token \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
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
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"
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
  --scopes "ryeos.execute.service.objects/get"
```

This is a remote administrative operation. Your caller node must already
be authorized on the target for `ryeos.execute.service.identity/authorize-key`.
For initial bootstrap, the remote operator can run `ryeos authorize-key`
locally on the remote node instead.

Remote authorization rejects wildcard scope delegation. Enumerate every
scope explicitly. It is create-only: `/authorize-key` cannot replace or
reclassify any existing local-client, remote-node, remote-operator, bootstrap,
or configured-operator grant. Admission claims are likewise create-only.

Configured-operator continuity is provisioned only by an offline target-node
action, not by remote delegation. With the target daemon stopped, the target
operator replaces that key's grant with an origin-bound, exact-scope grant:

```bash
RYEOS_APP_ROOT=/path/to/target-app-root ryeos authorize-client \
  --public-key "<configured_operator_raw_ed25519_base64>" \
  --label "operator forwarded from source" \
  --origin-site-id "site:<source>" \
  --allow-semantic-conversion \
  --scopes "<comma-separated exact scopes>"
```

Before that conversion, admit the source node key with the exact
`ryeos.attest.request.forwarded-operator` scope. The source site ID is the
canonical `site_id` reported by its RyeOS identity. The target-signed operator
grant constrains the permitted forwarding site; callers cannot create origin
authority in request data. The separately admitted source node co-signs the
method, path, body hash, timestamp, nonce, target audience, primary operator
key/signature, forwarding key, and site. Forwarded HEAD and execute requests
also carry a primary-signed `required_origin_site_id` assertion, which must
match the verified proof. Missing, partial, wrong-class, wrong-site, or invalid
proof fails authentication.

Authorized-key files are keyed by fingerprint, so conversion makes that exact
configured-operator key remote-only. Local maintenance uses the same key after
quiescing workflows and stopping the daemon: rerun the offline tool without
`--origin-site-id`, with only local maintenance scopes and
`--allow-semantic-conversion`; start the daemon and maintain; stop it; then
reinstall the exact remote grant with the same explicit flag. A different key
cannot pass an exact configured-operator check. Scope merging is forbidden
across class or origin changes, and the offline tool reports previous and new
class/origin values. The conversion flag also makes the tool acquire and retain
the node's exclusive daemon state lock through publication; a live daemon
therefore causes a mechanical refusal rather than relying on operator
procedure.

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

The ordinary command is node-owned. A durable workflow that must retain the
configured operator across later remote control uses the explicit service
form:

```bash
ryeos execute service:remote/push --input - <<'JSON'
{
  "remote": "prod",
  "project": "/absolute/path/to/project",
  "outbound_principal": "configured_operator"
}
JSON
```

This does not reuse a node-owned remote HEAD; it creates/advances the distinct
configured-operator principal-scoped HEAD.

Failure modes:

- project path must be absolute and canonicalizable
- missing remote ignore rules fail closed if they cannot be fetched
- object upload or pushed-head write errors abort the push

## `ryeos remote pull`

Fetch explicitly typed CAS artifacts from a remote into one new directory.

```bash
ryeos remote pull --remote prod --object-hashes abc123 --output-dir /tmp/objects
ryeos remote pull --remote prod --blob-hashes def456 --output-dir /tmp/blobs
```

Blobs are written as `<hash>` and JSON objects as `<hash>.json`. The output
directory is required, must be absolute, and must not already exist. Nothing is
inserted into local CAS by this operator export command.

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

1. Push a typed project snapshot, or an empty project snapshot in `--no-project`
   mode.
2. Call remote `/execute` using the pushed project source.
3. Pull result objects and apply changed project files locally.

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

The signed `service:remote/run` contract is broader than the project-bound
terminal presentation. It accepts `project: {kind: projectless}` with no
`project` field for remote control-plane service execution, and accepts
`outbound_principal: configured_operator` when owner continuity is required.
That path signs with the configured operator and co-signs with the source node;
the target `/execute` boundary retains both the closed authorized-key class and
the verified origin into any in-process service handler. Wait mode is used for
unary service calls. Accepted mode additionally requires a caller-retained
`launch_id` and is used for durable projectless or project-backed workers.
Project-backed live/current-HEAD modes still require a configured project
binding.

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

This differs from local `bundle install/remove`, which require stopped-node
authority.
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
