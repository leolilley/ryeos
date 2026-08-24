<!-- ryeos:signed:2026-08-24T15:37:11Z:efa182aeca77d1da29d1da476fe8baf3174a50e282fe081d29c7bc7777c6384b:gagSCE48YLaDwIKsUa6UqUgHOAen14Dwesv9e2RBKtaTiGXMPceFYIEQpMPXv9vpeF7D5d1z6X0SFZ8zDIzqAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
| `remote/configure` | `remote.configure` | `ryeos.execute.service.remote/configure` |
| `remote/list` | `remote.list` | `ryeos.execute.service.remote/list` |
| `remote/status` | `remote.status` | `ryeos.execute.service.remote/status` |
| `remote/doctor` | `remote.doctor` | `ryeos.execute.service.remote/doctor` |
| `remote/admit` | `remote.admit` | `ryeos.execute.service.remote/admit` |
| `remote/push` | `remote.push` | `ryeos.execute.service.remote/push` |
| `remote/pull` | `remote.pull` | `ryeos.execute.service.objects/get` |
| `remote/execute` | `remote.execute` | `ryeos.execute.service.remote/admin` |
| `remote/authorize` | `remote.authorize` | `ryeos.execute.service.remote/admin` |
| `remote/threads` | `remote.threads` | `ryeos.execute.service.remote/admin` |
| `remote/thread-status` | `remote.thread-status` | `ryeos.execute.service.remote/admin` |
| `remote/bundle-install` | `remote.bundle-install` | `ryeos.execute.service.bundle/install` |
| `remote/vault-set` | `remote.vault-set` | `ryeos.execute.service.remote/admin` |
| `remote/vault-list` | `remote.vault-list` | `ryeos.execute.service.remote/admin` |
| `remote/vault-delete` | `remote.vault-delete` | `ryeos.execute.service.remote/admin` |

## Operational invariants

- Outbound remote requests normally use the local **node key**. The explicit
  configured-operator push/run mode uses the exact configured operator only
  after local operator authentication, and the target must bind it to the
  forwarding site with an exact-scope `remote_operator` grant. The source node
  co-signs each exact request, and its independently admitted `remote_node`
  grant must carry `ryeos.attest.request.forwarded-operator`.
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
  uses signed requests checked against target-node grants. Admission and
  online `authorize-key` are create-only and cannot replace/reclassify an
  existing fingerprint.
- `remote doctor` is an operator diagnostic: it combines remote discovery,
  pinned-identity checks, signed authorization probing, project binding
  checks, and next-step commands.
- `remote push` and `remote execute` use the target node's ingest-ignore
  rules, not local ignore rules, when building a pushed manifest.
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
