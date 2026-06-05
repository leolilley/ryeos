<!-- ryeos:signed:2026-06-05T04:12:08Z:8c73992cd867e749f4a3cb49b59c2f9ff353b1e5a4fdc2a7c58e365e7bddde55:1wAPL4UGu5qjqb97Te4hpSyolt8Jsz2OWGzGSYhQGc/10iO3fTS/N8Axh4elWMOY4SaAyX3ldtZqgX85I6GaAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
| `remote/doctor` | `remote.doctor` | `ryeos.execute.service.remote.doctor` |
| `remote/admit` | `remote.admit` | `ryeos.execute.service.remote.admit` |
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
- `remote configure` may import a remote descriptor. The descriptor is a
  trust pin/discovery record, not a credential; configure still reads the
  live `/public-key` document and refuses to write config if the live node
  key or fingerprint does not match the descriptor.
- Initial remote authorization can use `admission/claim` when the target
  node has a one-time local admission token. Claiming the token creates a
  normal authorized-key grant on the target node; execution traffic still
  uses signed requests checked against target-node grants.
- `remote doctor` is an operator diagnostic: it combines remote discovery,
  pinned-identity checks, signed authorization probing, project binding
  checks, and next-step commands.
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
