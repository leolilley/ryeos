<!-- ryeos:signed:2026-11-19T05:15:46Z:e435b1f354f73f6ab839a61dcc661fea0afa8c74041f076e7843e3715264281d:7bt/4ueeGP1aLhHK2iMVaEc+4koYhHcCPMYmsec/kpz7uxhHwlOmC19D0emrTy4N7Kdn0L0oiFcf1uKWXrZnBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- `node/locality` returns this node's own graded locality facts: its `site_id`,
  the time it measured (`as_of`), and for each of `boot`, `storage` and
  `account` a digest the node measured, or `unknown` with a reason key
  (`emulated_kernel`, `not_measurable`, `user_namespace`). It never returns a
  boot id, path, mount, device or uid. It needs
  `ryeos.execute.service.node/locality`, so a remote answers only a caller its
  operator granted that capability to. Digests are plain SHA-256 over a
  domain-separated string; `account` and `storage` include the raw boot id
  (never served), so one uid or device number on two machines never matches.
  The schema is `ryeos.node.locality/v2`. A reader ignores fields it does not
  know, and reads a grade or fact it does not recognise as `unknown` with the
  reason `unrecognised`, for that relation only; one new relation never makes a
  node's whole answer unreadable.
  What the relations mean: `boot` is the kernel's per-boot id, one run of one
  kernel. It is not a machine id: the same machine reports a new value after a
  reboot. `storage` is one filesystem instance, the kernel's device number for
  the superblock that holds the app root. Equal means the same live filesystem;
  different does not mean different disks, because two overlay mounts over one
  disk, or two btrfs subvolumes, differ. "Not shared" says nothing about the
  hardware beneath. `account` is the effective uid within one boot.
- `node/locality/groups` asks each configured remote for `node/locality` and
  compares the answers with this node's own. A remote that refuses is
  `not_granted`; one that cannot be reached or is too slow is `did_not_answer`
  with a reason, which is not the same as `unknown`. A remote that answers as a
  different site than the one pinned is `did_not_answer` with `site_mismatch`. A
  relation holds only when both nodes measured the same digest. Each group says
  whether it holds this node (`this_computer`); each member fact lists the
  other members it shares a measured digest with (`shared_with`); each member
  says whether it `disagrees` with a declared group. Every `as_of` is RFC 3339
  in millisecond `Z` form. It needs `ryeos.execute.service.node/locality/groups`;
  `ryeos node locality` prints it. The schema is
  `ryeos.node.locality.groups/v2`.
  Member ids carry a prefix naming the identity: `site:<fingerprint>` for this
  node and every pinned remote, `installed:<service name>` for a node installed
  as a host service that this node has no remote for (listed, never asked), and
  `pin:<name>` for a remote pin that failed verification.
- `node/machine/declare`, `node/machine/list` and `node/machine/remove`
  (`ryeos node machine declare <label> --members <site-id> [--members <site-id>]...`,
  `ryeos node machine list`, `ryeos node machine remove <label>`) keep the
  operator's declared machines as node-signed records,
  `.ai/node/machines/<label>.yaml` (schema `ryeos/machine-declaration/v1`), written
  and verified like remote pins. Declaring and removing need the configured
  local operator. A declaration is the operator's word, graded `declared`: no
  node admits anything on it. Members are site ids only; an
  installed-not-connected node cannot be named, and a named node that is not
  seen shows as missing. A node is on one machine. A record that is unsigned,
  edited, signed by another key or malformed, and every record that names a node
  another record names, is reported under `invalid_declarations` (and
  `invalid_machines` in the list) with a stable code
  (`machine_declaration_unsigned`, `_wrong_signer`, `_tampered`, `_invalid`,
  `_conflict`) and left out of the comparison. It never fails the view.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.

See [Execution Isolation](../node/execution-isolation.md) for snapshot and restart
semantics.
