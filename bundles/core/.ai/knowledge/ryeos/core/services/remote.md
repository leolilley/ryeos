<!-- ryeos:signed:2026-05-22T07:21:24Z:62e0d67ba5ab2118969765b49f5df0efe5382a95fc933da88c5d037a592f54e6:4clIsk3DxtXpX6UxZAwlhoZSj5VOiRguGt8xjIaBaaPxY5IceaQjDio1rJsS81fFa8F3klUDonRM2lDKdQmdBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, remote, pushed-head, transfer, capabilities]
version: "1.1.0"
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
| `remote/configure` | `remote.configure` | `ryeos.execute.service.remote.configure` |
| `remote/list` | `remote.list` | `ryeos.execute.service.remote.list` |
| `remote/status` | `remote.status` | `ryeos.execute.service.remote.status` |
| `remote/push` | `remote.push` | `ryeos.execute.service.remote.push` |
| `remote/pull` | `remote.pull` | `ryeos.execute.service.objects.get` |
| `remote/execute` | `remote.execute` | `ryeos.execute.service.remote.admin` |
| `remote/authorize` | `remote.authorize` | `ryeos.execute.service.remote.admin` |
| `remote/threads` | `remote.threads` | `ryeos.execute.service.remote.admin` |
| `remote/thread-status` | `remote.thread-status` | `ryeos.execute.service.remote.admin` |
| `remote/bundle-install` | `remote.bundle-install` | `ryeos.execute.service.bundle.install` |
| `remote/vault-set` | `remote.vault-set` | `ryeos.execute.service.remote.admin` |
| `remote/vault-list` | `remote.vault-list` | `ryeos.execute.service.remote.admin` |
| `remote/vault-delete` | `remote.vault-delete` | `ryeos.execute.service.remote.admin` |

## Operational invariants

- Outbound remote requests are signed with the local **node key**, not
  the operator user key.
- `remote configure` stores remote identity, vault fingerprint, URL, and
  ingest-ignore config in the local system space under
  `.ai/config/remotes/remotes.yaml`.
- `remote push` and `remote execute` use the target node's ingest-ignore
  rules, not local ignore rules, when building a pushed manifest.
- `remote execute` is synchronous in v1: push, execute, pull, apply.
- `remote bundle-install` is live daemon-side installation; local
  `bundle install/remove` remain offline-only.
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
