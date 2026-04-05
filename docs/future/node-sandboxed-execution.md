```yaml
id: node-sandboxed-execution
title: "Node-Level Sandboxed Execution — Cryptographically Attested Hardware"
description: Bind execution to verified hardware at the node level. Lillux handles supply chain security (what ran, who signed it). The node handles runtime security (what hardware, what resources, what isolation). The key authorizes it all.
category: future
tags: [node, sandbox, hardware-attestation, security, execution]
version: "0.1.0"
status: exploratory
```

# Node-Level Sandboxed Execution

> **Status:** Exploratory

## The Security Stack

RYE today has one security layer:

- **Lillux** — supply chain security. You know what ran, who signed it, and that it wasn't altered. Verified code runs with the permissions of the subprocess user. No runtime confinement.

The manifesto is explicit about this: "This isn't OS-level sandboxing." That's intentional at the Lillux level. Lillux answers "what" and "who." It doesn't answer "where" or "within what constraints."

The node answers those questions.

## The Insight

Each node already has its own Ed25519 key, generated on first boot. It already signs execution results. The output carries two signatures: yours on the input, the node's on the output. You already know where something ran.

The step that's missing: the node doesn't attest to what it is. It signs results, but it doesn't sign a description of its own execution environment. What hardware. What isolation. What resource constraints. What capabilities are available and what are restricted.

When the node cryptographically attests to its environment, execution becomes bound to verified hardware. Not just "this signed code ran" but "this signed code ran in this specific verified environment on this specific attested hardware with these specific constraints."

## The Stack

| Layer      | Concern                 | What it does                                                                      |
| ---------- | ----------------------- | --------------------------------------------------------------------------------- |
| **Key**    | Identity                | Who authorized this                                                               |
| **Node**   | Environment attestation | Declares what hardware, what capabilities, what isolation is available             |
| **Lillux** | Enforcement             | Applies constraints at execution time. Will not spawn unless sandbox requirements are met |

The node declares. Lillux enforces. The node says "I have these capabilities and these restrictions." Lillux says "I will not spawn this process unless these constraints are applied." Declaration above, enforcement below.

This is why enforcement can't live at the node level alone. The node is Python. If sandboxing is above Lillux, you're trusting Python to enforce runtime constraints, which is exactly the kind of bypass Lillux exists to prevent. `lillux exec` is where the subprocess actually gets spawned. That's where cgroups, namespaces, seccomp filters, network restrictions get applied. The Rust binary is the enforcement point.

## What Node Attestation Looks Like

On first boot (or on environment change), the node produces a signed attestation:

- Hardware: CPU architecture, GPU models, memory, storage
- Isolation: container runtime, namespace configuration, resource cgroups
- Capabilities: what tool IDs this node can execute (already exists via `/status`)
- Restrictions: what's not available, what's firewalled, what resource limits apply

This attestation is signed by the node's key and published. Callers can verify it before dispatching execution. TOFU pins the node and its attested environment together.

## How This Connects

**Sovereign inference.** GPU nodes already report capabilities via `/status`. Attestation extends this: not just "I have `llm/complete/meta-llama/llama-3-1-8b`" but "I have 4x A100 GPUs, 320GB VRAM, model loaded in isolated GPU memory, no network egress from the inference process."

**Encrypted execution.** When you combine node attestation with encrypted execution, you get verifiable computation on untrusted hardware. The node attests to its environment (even if you don't trust the operator), and execution happens within sealed boundaries. Hardware attestation is the foundation that makes encrypted execution meaningful.

**Capability attenuation.** Directives already scope down permissions through the fork hierarchy. Node-level sandboxing extends this to the physical layer. A directive can declare not just "this thread can only call these tools" but "this thread must run on a node with these hardware constraints and this isolation level."

**Webhook agents.** Always-on agents triggered by external services currently trust the node they're running on. With attestation, the webhook binding can specify environment requirements. The agent only executes on nodes that meet the attested constraints.

## What Doesn't Change

- The execution chain stays the same. Tool → runtime → Execute → `lillux exec`.
- Signing and verification stay the same. Items are signed by your key, results are signed by the node's key.
- CAS sync stays the same. Content-addressed objects flow between nodes regardless of sandboxing.

## What Changes in Lillux

Lillux gains sandbox enforcement as part of Execute. `lillux exec` learns to accept and apply constraint parameters: cgroups, namespaces, seccomp profiles, network restrictions. The four concerns stay the same (Execute, Memory, Identity, Time), but Execute grows to include "execute within these constraints." The constraints are passed down from the node layer, but the Rust binary is what applies them.
