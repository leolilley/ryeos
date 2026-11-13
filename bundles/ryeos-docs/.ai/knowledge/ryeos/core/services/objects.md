<!-- ryeos:signed:2026-11-13T20:05:09Z:a28c8875a3c441c69c1a1bd93e333433da6a86549e12861cf47c5cf3c0d2c806:L90fxY9o+mmqNaqYWV5LgE+woC7W46uvWsLL4umRtkrV1FfnYSUzt/ss3gpj2CjaAimwlN4uieEcjQyChir1CQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, objects, cas]
version: "1.0.0"
description: CAS object service reference.
---

# Services: objects

Invariant: object services expose explicit CAS get/put/has operations and fail closed when requested hashes are absent.

- `objects/has` checks object and blob namespaces independently, under a
  named read root. Requests and responses preserve the required CAS kind even
  when both namespaces contain the same digest.
- `objects/get` returns object bytes/metadata for hashes reachable from a
  named read root.
- `objects/get-any` is the operator-only unscoped read.
- `objects/closure/get` and `objects/closure/describe` walk the closure of
  closure roots that are themselves reachable from a named read root.
- `objects/put` writes supplied content into CAS, as the caller's own
  project head upload; it never answers presence.

Remote transfer and pushed-head execution depend on these services for content-addressed synchronization.

## Authority: reads are scoped by root

Knowing a hash is not authority to read it. Every `objects/get`,
`objects/has`, `objects/closure/get` and `objects/closure/describe` request
carries a required `root` field naming a typed root the caller is entitled to.
The node resolves the root to the object hashes it is anchored at, walks the
schema-defined closure of those hashes (cached per root hash for complete
closures) and serves only hashes inside it.

| `root.kind` | Fields | Who may name it | Anchors |
|---|---|---|---|
| `project_head` | `project_path` | the caller, for its own head namespace only | the caller's head of that project, with its history |
| `thread_result` | `thread_id` | the thread's owner | the thread's result project snapshot and admitted launch capsule |
| `bundle_export` | `bundle_name` | a caller holding `ryeos.execute.service.bundle/export` | the file blobs of the installed bundle's export |
| `placement` | `placement_id` | the owner of a dedicated worker session, or the configured peer of a retained handoff or follow-delivery operation | the session's candidate and evidence hashes, or every hash the retained operation record and its terminal testimony name |
| `admitted_subject` | `subject_hash` | a caller holding `ryeos.execute.service.admission/attestations-for-subject` | a subject this node holds an accepted admission attestation for |
| `catalog_publication` | `catalog_namespace` | a caller holding `ryeos.execute.service.bundle-catalog/resolve` | the signed head of that bundle catalog and its history |

`admitted_subject` and `catalog_publication` cover content the node has
deliberately published: the admission or catalog head is what announces it, and
the capability that lists those attestations is the capability that reads what
they cover.

Refusals are typed. A request with no root, a root the caller may not see, or a
hash that is not reachable from the named root is refused with the structured
code `object_not_in_scope` (HTTP 403). The refusal does not distinguish a hash
that exists elsewhere on the node from one that does not, so a read is never a
presence oracle. A hash inside the root whose bytes are absent is reported as
`missing_object` / `missing_blob`, which reveals nothing the caller could not
infer from the root it is entitled to.

`objects/has` answers present only for hashes reachable from the named root and
absent for everything else, whether or not the node holds them. A project push
asks under the `project_head` root of the project it is pushing: objects it
uploads for a head that does not exist yet are by construction not reachable
from the current head, so they are answered absent and uploaded, and
`objects/put` deduplicates. Re-sending content already present elsewhere on the
node is the price of not revealing that it exists.

`objects/get-any` takes no root. It requires the configured node operator and
the capability `ryeos.execute.service.objects/get-any`, which is never part of
a recommended remote grant. The CLI's artifact export uses it because the
operator reads their own node.

Who holds `objects/get` in what ships:

| Holder | Where | Notes |
|---|---|---|
| Configured operator (bootstrap grant `*`) | node bootstrap; reissued by the offline authorization reset | The only shipped grant that covers `objects/get` and `objects/get-any`. |
| Init profiles (`bundles/.ai/node/init/profiles/*.yaml`) | none | Profiles grant no capabilities. |
| Shipped bundle items | none | No bundle item declares `objects/get`. The hosted Codex scopes deliberately exclude it. |
| UI surfaces (`ryeos-ui`) | `capabilities: ryeos.execute.service.*` | A ceiling on what a surface may call, not a grant; the viewer's own grant still decides. |

Grants operators are told to make. Each one now gives the grantee read access
only to what its named roots reach, not to the node's whole CAS:

| Recommended grantee | Where it is recommended | Root it reads under |
|---|---|---|
| A caller node that runs `remote execute` against this node | `remote status` and `remote doctor` suggested `authorize-client` command; admission-token, `remote admit` and `authorize-key` examples in [Remote Operations](../remote/remote-operations.md) and the README | `thread_result` of the execution (pull-back), `project_head` while pushing |
| A caller node that runs `remote pull` or `remote bundle-install` against this node | [Remote Command Reference](../remote/remote-command-reference.md) authority matrix | the root `remote pull` names; `bundle_export` for bundle install |
| A bidirectional worker-placement peer | [Worker-hosted execution](../execution/worker-hosted-execution.md) node-grant ceiling | `placement` |

Locally, the `remote.pull` service is gated by
`ryeos.execute.service.remote/pull` and takes the `root` to read on the remote.
Both nodes must run the scoped version: an older caller's rootless request is
refused with `object_not_in_scope`.
