<!-- ryeos:signed:2026-05-23T04:53:02Z:44f5e8e5a7c83f65a1a8809004834e6483c3c1b8997420a160a462dc3b9e5b62:6QP5MW6s6yjKehvD2GL0oBK+CNas7oF6z4RiT0ZqDO0xFZbEDOTQN8ivTOrtpltfecgU9lzqNpRMViYVRmBjCw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
- `remote doctor` is an operator diagnostic: it combines remote discovery,
  signed authorization probing, project binding checks, and next-step commands.
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
