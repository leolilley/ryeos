<!-- ryeos:signed:2026-11-18T02:26:14Z:dc52c9b6baf27a85368a4d90fafae7e14a406f9db84dc9babc23dcaac78fdf3b:FkkxhHnh6pV28LUywZGiwiT9AEEVcpexGm5oYOE3IONK6hzzG/ReNKTx4SuxwCpK/2IS1yx/zfQBWUSN5iX0Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, health, identity, status]
version: "1.3.0"
description: Health, identity, and status service reference.
---

# Services: health, identity, status

Invariant: health and public identity surfaces are safe unauthenticated reads; status reports daemon state without mutating execution state.

- `health/status` backs health checks.
- `identity/public_key` returns the node public identity document.
- `node/status` returns daemon/node status, including the immutable isolation
  snapshot's mode, schema version, source path, policy digest, typed backend
  status, selected bundle/implementation, manifest and signer identities,
  adapter digest/build, declared and effective capabilities, and inspected
  payload digests.
- `node/locality` returns this node's own graded locality facts: for each of
  `boot`, `storage` and `account` a digest the node measured, or `unknown` with
  a reason key (`emulated_kernel`, `not_measurable`, `user_namespace`). It never
  returns a boot id, path, mount, device or uid. It needs
  `ryeos.execute.service.node/locality`, so a remote answers only a caller its
  operator granted that capability to. Digests are plain SHA-256 over a
  domain-separated string; `account` and `storage` include the boot digest, so
  one uid or device number on two machines never matches.
- `node/locality/groups` asks each configured remote for `node/locality` and
  compares the answers with this node's own. A remote that refuses is
  `not_granted`; one that cannot be reached or is too slow is `did_not_answer`
  with a reason, which is not the same as `unknown`. A relation holds only when
  both nodes measured the same digest. It needs
  `ryeos.execute.service.node/locality/groups`; `ryeos node locality` prints it.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.

See [Execution Isolation](../node/execution-isolation.md) for snapshot and restart
semantics.
