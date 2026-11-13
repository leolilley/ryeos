<!-- ryeos:signed:2026-11-13T00:50:49Z:a339cd20786b0352c2771084b87ff65cdb839a57fa55d65170e78a5d763acf3f:yYchrKyPbjrNIXHY8cFW6dIpNJyzJC9y6nXeuklfKToBSxvQ/D2pa/W0RCNJJvE4V4I8Dc5sAcKzWssFS8eZDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, objects, cas]
version: "1.0.0"
description: CAS object service reference.
---

# Services: objects

Invariant: object services expose explicit CAS get/put/has operations and fail closed when requested hashes are absent.

- `objects/has` checks object and blob namespaces independently. Requests and
  responses preserve the required CAS kind even when both namespaces contain
  the same digest.
- `objects/get` returns object bytes/metadata for a hash.
- `objects/put` writes supplied content into CAS.

Remote transfer and pushed-head execution depend on these services for content-addressed synchronization.

## Authority: a hash plus the grant fetches the object

`objects/get` treats knowledge of a hash as authority. A caller holding
`ryeos.execute.service.objects/get` can fetch **any** object or blob in the
node's CAS whose hash it knows. There is no owner, principal, project or
reachability check: CAS is shared within a node, and the handler serves the
requested hash to any holder of the grant. A missing hash is reported as
`missing_object` / `missing_blob`, so the grant also answers whether a hash is
present.

Treat the grant as read access to the whole CAS of that node, including other
principals' pushed project snapshots, for anyone who learns their hashes.
Grant it only to callers you would let read all content on the node, and keep
it out of delegated grants by default. Owner-scoped object reads are not
implemented.

Who holds `objects/get` in what ships:

| Holder | Where | Notes |
|---|---|---|
| Configured operator (bootstrap grant `*`) | node bootstrap; reissued by the offline authorization reset | The only shipped grant that covers `objects/get`. |
| Init profiles (`bundles/.ai/node/init/profiles/*.yaml`) | none | Profiles grant no capabilities. |
| Shipped bundle items | none | No bundle item declares `objects/get`. The hosted Codex scopes deliberately exclude it. |
| UI surfaces (`ryeos-ui`) | `capabilities: ryeos.execute.service.*` | A ceiling on what a surface may call, not a grant; the viewer's own grant still decides. |

Grants operators are told to make (each one gives the grantee the read access
above on the granting node):

| Recommended grantee | Where it is recommended | Why it needs `objects/get` |
|---|---|---|
| A caller node that runs `remote execute` against this node | `remote status` and `remote doctor` suggested `authorize-client` command; admission-token, `remote admit` and `authorize-key` examples in [Remote Operations](../remote/remote-operations.md) and the README | Pull-back reads the result snapshot from the remote by hash. |
| A caller node that runs `remote pull` or `remote bundle-install` against this node | [Remote Command Reference](../remote/remote-command-reference.md) authority matrix | Fetches the requested objects or the exported bundle tree. |
| A bidirectional worker-placement peer | [Worker-hosted execution](../execution/worker-hosted-execution.md) node-grant ceiling | Reads placement closures by hash. |

Locally, the `remote.pull` service is also gated by `objects/get`, so a local
holder of the grant can pull objects from a configured remote.
