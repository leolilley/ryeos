<!-- ryeos:signed:2026-11-13T21:32:28Z:ae2b1d6eba1f68e2be2ff39d2a514a2e5a9002010a4921c3243dc3a50a1edae3:yUNwG0vOskPl6YY1X6ZtvwWPHZZjwhQHn5TwRuN4eh/uav4A4ZlL3jY6UX+1u/BK2veLoDMrWvHlw77UWQa6DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, remote, pushed-head, transfer, capabilities]
version: "1.1.2"
description: Remote service reference.
---

# Services: remote

Remote services are **local daemon-only orchestrators** for cross-node
configuration, transfer, execution, thread inspection, bundle install,
and vault proxy operations.

Do not confuse the local service capability with target-node authority:
remote commands often require one capability to start the local
orchestrator and different authorized-key scopes on the remote daemon.
The authoritative matrix is in
[Remote Command Reference](../remote/remote-command-reference.md).

## Local services

| Service | Endpoint | Local capability |
|---|---|---|
| `remote/configure` | `remote.configure` | `ryeos.execute.service.remote/configure` |
| `remote/list` | `remote.list` | `ryeos.execute.service.remote/list` |
| `remote/status` | `remote.status` | `ryeos.execute.service.remote/status` |
| `remote/doctor` | `remote.doctor` | `ryeos.execute.service.remote/doctor` |
| `remote/admit` | `remote.admit` | `ryeos.execute.service.remote/admit` |
| `remote/push` | `remote.push` | `ryeos.execute.service.remote/push` |
| `remote/reconcile-project-head` | `remote.reconcile-project-head` | `ryeos.execute.service.remote/reconcile-project-head` |
| `remote/pull` | `remote.pull` | `ryeos.execute.service.remote/pull` |
| `remote/execute` | `remote.execute` | `ryeos.execute.service.remote/admin` |
| `remote/authorize` | `remote.authorize` | `ryeos.execute.service.remote/admin` |
| `remote/threads` | `remote.threads` | `ryeos.execute.service.remote/admin` |
| `remote/thread-status` | `remote.thread-status` | `ryeos.execute.service.remote/admin` |
| `remote/bundle-install` | `remote.bundle-install` | `ryeos.execute.service.bundle/install` |
| `remote/vault-set` | `remote.vault-set` | `ryeos.execute.service.remote/admin` |
| `remote/vault-list` | `remote.vault-list` | `ryeos.execute.service.remote/admin` |
| `remote/vault-delete` | `remote.vault-delete` | `ryeos.execute.service.remote/admin` |
| `remote/delegate` | `remote.delegate` | `ryeos.execute.service.remote/delegate` |
| `remote/delegation-list` | `remote.delegation-list` | `ryeos.execute.service.remote/delegation-list` |
| `remote/delegation-revoke` | `remote.delegation-revoke` | `ryeos.execute.service.remote/delegation-revoke` |
| `remote/offline-operations-facts` | `remote.offline-operations-facts` | `ryeos.execute.service.remote/offline-operations-facts` |
| `remote/offline-operations-submit` | `remote.offline-operations-submit` | `ryeos.execute.service.remote/offline-operations-submit` |
| `remote/offline-operations-status` | `remote.offline-operations-status` | `ryeos.execute.service.remote/offline-operations-status` |

## Operational invariants

- Outbound remote requests are signed by the local **node key**. A request made
  for a principal also carries the principal's own signed delegation
  (`x-ryeos-delegation`); the daemon never loads a principal's private key,
  the operator's included. The target binds the principal to the forwarding
  site with an exact-scope `remote_principal` grant and caps the request to the
  delegation. The source node co-signs each exact request, and its
  independently admitted `remote_node` grant must carry
  `ryeos.attest.request.forwarded-operator`.
- `remote configure` (configured local operator only) stores remote identity,
  vault fingerprint, URL, ingest-ignore config and the pin source as a
  node-signed record in the local system space under
  `.ai/node/remotes/<name>.yaml`. Projects never contribute remotes.
- `remote configure` requires a pin for the remote node key: a remote
  descriptor or an `expect_fingerprint` (64 lowercase hex). Without either it
  refuses with `remote_key_unpinned` and names the fingerprint seen; there is
  no trust-on-first-use. The descriptor is a trust pin/discovery record, not a
  credential; configure still reads the live `/public-key` document and
  refuses to write config if the live node key or fingerprint does not match
  the descriptor or the expected fingerprint (`remote_key_pin_mismatch`).
- Initial remote authorization can use `admission/claim` when the target
  node has a one-time local admission token. Claiming the token creates a
  normal authorized-key grant on the target node; execution traffic still
  uses signed requests checked against target-node grants. Admission and
  online `authorize-key` are create-only and cannot replace/reclassify an
  existing fingerprint.
- `remote doctor` is an operator diagnostic: it combines remote discovery,
  pinned-identity checks, signed authorization probing, project binding
  checks, and next-step commands.
- `remote push` and `remote execute` build a transfer policy from the union of
  the source and target nodes' signed ingest-ignore exclusions. This keeps
  source-forbidden content out of local CAS/upload and target-forbidden content
  out of the generation. The receiving node rechecks its current policy before
  HEAD publication or reconciliation.
- `remote reconcile-project-head` is the explicit full-project DAG convergence
  operation. It requires exact expected local and remote configured-operator
  HEADs plus an explicit `local` or `remote` content winner. It creates one
  two-parent generation, publishes remote-first, and advances the local HEAD
  through a durable recovery job. It never silently rebases a handoff.
- `remote execute` is synchronous in v1: push, execute, pull, apply.
- `remote bundle-install` is live daemon-side installation; local
  `bundle install/remove` require stopped-node authority.
- `remote vault-*` proxies to the target node vault. In v1 the vault is
  a node-level capability-gated store, not per-principal isolated.

## Failure model

Remote services fail closed:

- missing CAS blobs abort transfer/install
- failed preflight removes partial bundle installs
- stale remote identity causes signed-request/audience failures until
  `remote configure` refreshes local config
- clean-base conflicts during `remote execute` pull-back abort local
  apply without partial writes
