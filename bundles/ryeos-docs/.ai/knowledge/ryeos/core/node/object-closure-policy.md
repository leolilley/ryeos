<!-- ryeos:signed:2026-09-03T11:56:16Z:7cfa0d64a9c260848ac10705cb24e998ba2d9b00fa19728619e32ebe1da55004:prkUEEp9KZQuPVJWpdJ4DhGXR+X/v61ImGi7Rp8v7KJH524PtBbfff4RNicwPiQd7p9jfQP8nbpv9bSkBaHFAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [node, policy, object-closure, cas, remote, admission]
version: "1.0.0"
description: >
  Node-owned resource policy for bounded local and remote object-closure
  admission, serving, and staging.
---

# Object Closure Policy

RyeOS compiles one mandatory node-signed `object_closure` policy from the
complete generation under `.ai/node/policies/`. It is the node's resource
authority for operator-facing closure admission and remote CAS closure
transfer.

The policy selects exact ceilings for object and blob counts, individual and
aggregate byte sizes, response bytes, and links per object. These limits apply
on both sides of remote closure transfer:

- a serving node intersects peer-supplied upper bounds with its local policy;
- a receiving node admits its requested limits before making the call;
- a local API caller may request a smaller limit but cannot widen its own node
  policy;
- omitted request limits resolve to the local node policy, not Rust defaults;
- staged handoff payloads derive their aggregate byte ceiling from the same
  policy.

The signed workload, handoff protocol, or remote caller never selects node
capacity. Object schemas and the closure wire protocol retain separate
code-owned absolute safety maxima. Canonical-object size rules, attestation
envelopes, recovery retry semantics, and peer liveness deadlines are not
operator-tunable resource policy.

The publisher-authored initialization profiles provide an explicit baseline:

```yaml
schema: 1
max_roots: 256
max_objects: 32768
max_blobs: 32768
max_object_bytes: 33554432
max_total_object_bytes: 67108864
max_blob_bytes: 134217728
max_total_blob_bytes: 134217728
max_response_bytes: 268435456
max_links_per_object: 100000
```

These values cover the qualified hosted-worker project candidate without
giving every transfer the protocol's maximum allocation. A node expected to
transfer larger ordinary CAS closures must explicitly publish a wider policy
within the protocol maxima. Large-object content such as model weights remains
under the distinct large-object transport and residency authority; a closure
may carry only its typed requirement.

Change this policy only while the daemon is stopped, through the complete
node-policy publication boundary. There is no fallback policy and no
workload-name exception.
