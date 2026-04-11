```yaml
id: cluster-and-federation
title: "Cluster, Federation, And GPU Infrastructure"
description: Everything beyond single-node and static-config multi-node — fleet enrollment, hardware attestation, federated views, global chain authority, sovereign inference, cluster vault, and remote capability leases. One doc to rule them all.
category: future
tags: [future, cluster, federation, gpu, inference, trust, scale, nodes]
version: "1.0.0"
status: planned
```

# Cluster, Federation, And GPU Infrastructure

> **Status:** Planned — all of this sits on top of the native runtime (`ryeos-engine-v2`). None of it is a prerequisite for the first stable runtime.

> **Prerequisite:** The native runtime plan must be complete and working. Static-config remote execution, manual trust exchange, mirror threads, and delegated forwarding must all be proven before any of this is built.

---

## What This Document Covers

The `ryeos-engine-v2` plan covers local execution, static-config remote execution, and all the contracts needed for sound multi-node forwarding. That plan explicitly defers everything in this document.

This document consolidates every scale-up, federation, GPU, and cluster concern into one place:

1. Fleet enrollment and automated trust
2. Hardware attestation and sandboxed execution
3. Sovereign inference and GPU cluster routing
4. Federated views and global chain authority
5. Cluster vault and secret replication
6. Remote capability leases

These are ordered roughly by dependency — each section builds on the ones before it.

---

## 1. Fleet Enrollment

### Problem

The native runtime uses manual trust exchange: export a node's identity document, copy it to a peer, run `ryeosd auth authorize`. This works for a handful of stable nodes. It does not scale to ephemeral GPU workers, autoscaling pools, or environments where pre-distributing identity documents is impractical.

### Design

Introduce an enrollment authority — a designated registry node or dedicated service that acts as the root of trust for a fleet.

**Join flow:**

1. New node generates its signing key and identity during `init`.
2. New node presents a short-lived join token and its public identity to the enrollment authority.
3. Enrollment authority validates the token, issues a signed attestation.
4. New node persists the attestation.
5. Peer nodes verify each other through attestations, not direct identity exchange.

**Join tokens** are short-lived, scoped, and limited-use:

```json
{
  "token_id": "jt-abc123",
  "issued_by": "fp:registry-fingerprint",
  "expires_at": "2026-04-12T00:00:00Z",
  "max_uses": 10,
  "scope": {
    "roles": ["gpu-worker"],
    "labels": { "pool": "inference-a100" }
  },
  "signature": "ed25519:<base64>"
}
```

**Node attestations** are signed documents proving fleet membership with expiry:

```json
{
  "kind": "attestation/v1",
  "subject": "fp:node-fingerprint",
  "issued_by": "fp:registry-fingerprint",
  "expires_at": "2026-07-11T08:00:00Z",
  "roles": ["gpu-worker"],
  "labels": { "pool": "inference-a100" },
  "signature": "ed25519:<base64>"
}
```

**Revocation** via signed revocation lists published by the enrollment authority. Nodes periodically fetch and enforce.

**Key rotation** for both nodes (re-enroll with new key, old key retained for historical verification) and the enrollment authority itself (chain-of-trust rotation, similar to DNSSEC).

### Ephemeral Workers

Pre-authorized pools with multi-use join tokens scoped to specific roles:

```
ryeosd init --join https://registry.example.com --token-file /run/secrets/pool-token
ryeosd serve
```

Automatic deregistration on graceful shutdown. Heartbeat-based inactive detection for crashes.

### Trigger

When manual identity exchange becomes operationally painful — more than a handful of nodes, or any ephemeral worker pattern.

---

## 2. Hardware Attestation And Sandboxed Execution

### Problem

The native runtime delegates to remote nodes based on identity and trust. It does not verify what kind of environment the remote node provides. A node can claim to have GPUs or TEE enclaves without proof.

### Design

Extend node identity with cryptographic environment attestation:

- Hardware: CPU architecture, GPU models, memory, storage
- Isolation: available sandbox engines, profiles, capabilities
- Restrictions: what's firewalled, what resource limits apply

**Attestation documents** are signed by the node's key and published:

```json
{
  "kind": "attestation/v1",
  "node_id": "fp:a3f8c921e7b04d12",
  "environment": {
    "platform": "linux",
    "architecture": "x86_64",
    "gpu": [{ "model": "A100", "vram_gb": 80, "count": 4 }]
  },
  "isolation": {
    "engines": [
      { "name": "nsjail", "profiles": ["strict", "gpu-isolated"] },
      { "name": "bubblewrap", "profiles": ["strict"] }
    ]
  },
  "signature": "ed25519:<base64>"
}
```

**Delegation envelope extension:** the origin node can require specific hardware or environment properties as part of the forwarded request. The executor must present attestation matching those requirements.

**Sandbox engines** are integrated as data-driven providers — YAML-configured, invoked through Lillux's `--sandbox` flag. nsjail, bubblewrap, Firecracker, FreeBSD jails, OpenBSD pledge/unveil are all valid engines depending on platform.

**Directives** can declare sandbox requirements:

```yaml
metadata:
  sandbox:
    engine: "nsjail"
    profile: "strict"
    requires:
      capabilities: ["cgroups_v2", "seccomp"]
```

### Cross-Platform

| Platform | Available Engines                       | Notes                                      |
| -------- | --------------------------------------- | ------------------------------------------ |
| Linux    | nsjail, bubblewrap, Firecracker, Docker | Full ecosystem                             |
| FreeBSD  | jails, bhyve                            | Native OS-level isolation, ZFS integration |
| OpenBSD  | pledge/unveil                           | Syscall-level sandboxing                   |
| macOS    | sandbox-exec, Docker                    | Limited native options                     |

Attestation honestly declares what's available. No false claims.

### Trigger

When self-reported node labels are insufficient — regulated workloads, multi-tenant inference, or any scenario where "trust the node" isn't good enough.

---

## 3. Sovereign Inference And GPU Cluster Routing

### Problem

RYE today calls external LLM providers (Anthropic, OpenAI) over HTTP. Sovereign inference replaces external providers with your own GPU hardware running tinygrad, with cluster routing across multiple GPU nodes.

### Architecture

Two execution paths coexist:

**Provider path** (agent threads → completions server):

```
agent thread → http_provider → POST /v1/chat/completions
  → completions server: format → tinygrad generate() → parse
  → returns completion (same interface as Anthropic)
```

**Execute path** (cluster-internal tool dispatch):

```
ExecuteTool.handle() → dispatch to GPU node → tinygrad generate()
  → used for cluster-internal routing
```

### GPU Execution Nodes

`ryeosd` on a GPU node loads the tinygrad model at startup. The model lives in GPU memory as process state. `llm/complete/{family}/{model}` calls `model.generate()` directly — a Python function call, not an HTTP request.

### Completions Server

A separate HTTP service exposing `/v1/chat/completions`. It's not an endpoint on `ryeosd`. It runs the tool use loop internally via `execute`, handling N inference calls and M tool dispatches per request. To the caller, it looks like any other OpenAI-compatible endpoint.

Agent threads call it via `http_provider` — registered in `agent.yaml` with the server's URL. No CAS sync overhead. The swap from Anthropic to self-hosted is a one-line config change.

### Cluster Routing

With multiple GPU nodes, `llm/complete` routes to the right node:

- On a GPU node: calls tinygrad directly (same tool ID, local implementation)
- On a non-GPU node: queries `/status` on known remotes, matches capabilities, dispatches to the least-loaded capable node

Same tool ID, different implementations per node. Standard space resolution. Routing is a tool, not a separate system.

### Chat Templates And Tool Call Parsing

Data-driven, not hardcoded:

- Model family configs at `.ai/config/llm/models/{family}.yaml` describe token markers, role tokens, tool call format
- `llm/format` processor: messages + tools → model-specific token sequence
- `llm/tool-calls` parser: raw model output → structured tool calls

Adding a new model family = adding a config file. No model-specific code.

### Depends On

- Fleet enrollment (section 1) for managing ephemeral GPU workers
- Hardware attestation (section 2) for verifying GPU capabilities
- Static-config remote execution (from `ryeos-engine-v2`) as the baseline forwarding mechanism

### Trigger

When renting inference from external providers becomes a cost, sovereignty, or latency concern.

---

## 4. Federated Views And Global Chain Authority

### Problem

The native runtime provides mirror threads for cross-node visibility, but each node only sees its own mirrors. There is no unified view across the fleet, and there are no global event ordering guarantees.

### Federated Views

- **Aggregated thread views** — query threads across all fleet nodes from a single surface
- **Global resource discovery** — find available execution capacity, registered items, and node capabilities across the fleet
- **Federated event streams** — subscribe to events from multiple nodes through a single connection
- **Chain correlation at scale** — unified chain views spanning multiple executing nodes

The native runtime's mirror thread design is compatible with federation but does not require it. Federation is a layer on top.

### Global Chain Authority

The native runtime keeps per-node event ordering and correlates chains through shared metadata. It does not provide global cross-node sequence guarantees.

The advanced path adds:

- **Global sequence assignment** — a designated chain authority or consensus mechanism assigns globally ordered sequence numbers
- **Cross-node causal ordering** — events with causal dependencies across nodes are ordered correctly without relying on wall-clock time
- **Deterministic cross-node replay** — replay a multi-node execution in the exact order it occurred

### Trigger

When audit, compliance, or debugging require a single authoritative timeline across nodes, or when mirror-thread-per-node visibility is insufficient for operational needs.

---

## 5. Cluster Vault And Secret Replication

### Problem

The native runtime's remote resource namespace rule prevents forwarded execution from accessing executor-local vault secrets without explicit delegation. This is correct for security, but it means every remote execution must carry secrets explicitly or pre-provision them on every node.

### Design

- **Replication policies** — declare which secrets sync to which nodes. A production credential replicates to all worker nodes; a staging key stays on the staging node.
- **Centralized management** — single vault authority with satellite distribution. Secrets flow outward, never inward.
- **Group-scoped secrets** — secrets available to all nodes with a specific attestation role.
- **External backend integration** — HashiCorp Vault, AWS Secrets Manager as storage backends behind the same `/vault/*` API surface.

### Depends On

Fleet enrollment (section 1). Without attestation-based trust, there is no safe basis for replicating secrets across nodes.

See [Node Vault](node-vault.md) for the V1/V2 single-node vault design that this builds on.

### Trigger

When pre-provisioning secrets on individual nodes becomes operationally painful, or when multiple nodes need the same credentials.

---

## 6. Remote Capability Leases

### Problem

The `ryeos-engine-v2` plan supports capability refs (`@cap:` suffixes) for local execution and budget leases for remote execution. But capability-bound items cannot be forwarded to remote nodes in v1 — the origin rejects them.

### Design

Extend the delegation envelope to carry capability leases alongside budget leases:

- Origin node issues a `CapabilityLease` with the delegated principal envelope
- Remote executor consumes uses against the lease
- Usage reports and final settlement follow the same idempotent pattern as budget settlement
- The `ForwardedExecuteRequest` gains a `capability_lease_ids` field

The capability lease lifecycle is already defined in `17-lifecycle-state-machines.md`. The missing piece is threading leases through the forwarding contract.

### Depends On

- Local capability lifecycle must be proven working first
- Budget lease forwarding (from `ryeos-engine-v2`) as the pattern to follow

### Trigger

When remote execution needs to consume capability-bound items — e.g., a remote GPU node needs to call a rate-limited external API on behalf of the origin caller.

---

## Dependency Order

```
1. Fleet Enrollment
   └── 2. Hardware Attestation (needs attestation trust model)
       └── 3. Sovereign Inference (needs fleet enrollment + attestation for GPU routing)
   └── 5. Cluster Vault (needs attestation for trust basis)
4. Federated Views (independent, but more useful with fleet enrollment)
6. Remote Capability Leases (independent, but blocked on local capability lifecycle)
```

Fleet enrollment is the foundation. Most other extensions depend on it.

---

## Relationship To Other Docs

This document consolidates and supersedes the cluster/federation/GPU content from:

| Document                                                        | Relationship                                                                         |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| [Advanced Native Runtime Path](advanced-native-runtime-path.md) | Superseded — content folded into this doc                                            |
| [Cluster Bootstrap](cluster-bootstrap.md)                       | Detailed design for section 1 — retained as the reference for enrollment mechanics   |
| [Sovereign Inference](sovereign-inference.md)                   | Detailed design for section 3 — retained as the reference for inference architecture |
| [Node Sandboxed Execution](node-sandboxed-execution.md)         | Detailed design for section 2 — retained as the reference for sandbox engines        |
| [Node Vault](node-vault.md)                                     | V1/V2 design retained; V3 cluster vault content summarized in section 5              |

---

## Evaluation Criteria

Before building any of this:

1. Is `ryeos-engine-v2` complete and working? If not, finish that first.
2. Is manual trust exchange actually painful? If not, defer fleet enrollment.
3. Are there real GPU workloads that need cluster routing? If not, defer sovereign inference.
4. Are there workloads where self-reported labels are insufficient? If not, defer attestation.
5. Does anyone need global event ordering? If not, defer chain authority.
6. Is pre-provisioning secrets per-node painful? If not, defer cluster vault.
7. Are there remote workloads that need capability-bound items? If not, defer remote capability leases.

The native runtime contract is designed to be compatible with all of these extensions without requiring contract changes. That is the payoff of the remote-correct-from-day-one design.
