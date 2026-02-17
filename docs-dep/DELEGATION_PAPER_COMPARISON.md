# RYE OS vs. "Intelligent AI Delegation" (Google DeepMind, 2026)

**Paper:** _Intelligent AI Delegation_ — Tomašev, Franklin, Osindero (Google DeepMind, 2026-02-12)  
**System:** RYE OS — Lilley (2025–2026)  
**Date:** 2026-02-18

---

## Overview

The DeepMind paper proposes a theoretical framework for intelligent AI delegation — "a sequence of decisions involving task allocation, that also incorporates transfer of authority, responsibility, accountability, clear specifications regarding roles and boundaries, clarity of intent, and mechanisms for establishing trust." It identifies five pillars: Dynamic Assessment, Adaptive Execution, Structural Transparency, Scalable Market Coordination, and Systemic Resilience. It then maps these to nine technical protocols (§4.1–§4.9).

The paper explicitly notes that existing agent frameworks "rely on simple heuristics, and are not able to dynamically adapt to environmental changes and robustly handle unexpected failures." It proposes protocol extensions to MCP, A2A, AP2, and UCP to close these gaps — positioning them as future work for the field.

RYE OS is a working implementation that addresses the majority of these requirements. What the paper proposes as theoretical protocol extensions, RYE has built as shipped code. This document maps each of the paper's nine protocols to RYE's concrete implementation, identifies where RYE exceeds the paper's proposals, and where genuine gaps remain.

---

## 1. Task Decomposition (§4.1)

### What the paper proposes

- Delegators decompose complex objectives into sub-tasks
- Decomposition should optimise for efficiency and modularity
- "Contract-first decomposition" — delegation is contingent on the outcome having precise verification
- Sub-task granularity should align with available delegatee capabilities
- Hybrid human-AI decomposition with latency/cost awareness

### What RYE implements

**Directives are the decomposition format.** A directive's `<process>` section contains the decomposition — steps that the LLM reads and follows. Orchestrator directives decompose work and spawn child threads for sub-tasks:

```xml
<process>
  <step name="load_state">Load pipeline state from .ai/knowledge</step>
  <step name="spawn_scrapers">
    For each target, spawn a child thread:
    rye_execute(tool="rye/agent/threads/thread_directive",
      directive_name="execution/scrape_website",
      inputs={"url": target.url}, async_exec=true)
  </step>
  <step name="wait">Wait for all child threads to complete</step>
  <step name="aggregate">Collect results and update state</step>
</process>
```

**The LLM performs the decomposition reasoning.** RYE doesn't hardcode decomposition heuristics — it trusts the LLM (the orchestrator model) to decompose based on the directive's instructions. This is a fundamental design difference from what the paper assumes. The paper presumes framework-level decomposition logic. RYE says: the LLM is the decomposition engine; the framework provides the primitives (spawn, wait, cancel, aggregate).

**Contract-first decomposition** is partially present. Directives declare `<outputs>` that child threads must produce, and parent threads match against these keys. But RYE doesn't enforce formal verification contracts — it relies on the parent LLM to evaluate child results.

### Assessment

| Paper's Requirement                     | RYE Status                                           |
| --------------------------------------- | ---------------------------------------------------- |
| Task decomposition into sub-tasks       | ✅ Orchestrator directives + thread spawning         |
| Granularity alignment with capabilities | ✅ LLM-driven, informed by directive structure       |
| Contract-first decomposition            | ⚠️ Partial — outputs declared, not formally verified |
| Hybrid human-AI decomposition           | ❌ No human-in-the-loop primitives                   |

---

## 2. Task Assignment (§4.2)

### What the paper proposes

- Capability matching via registries or decentralized market hubs
- Matching formalized into smart contracts
- Pre-execution negotiation of monitoring cadence, privacy, autonomy level
- Recursive delegation (delegating the act of delegation)

### What RYE implements

**Task assignment is directive-to-thread mapping.** The orchestrator directive specifies which child directive to run, with what inputs and limits. There's no marketplace bidding — the orchestrator knows its available directives and selects directly.

**Capability matching is implicit.** Directives declare their `<permissions>` (what tools they can use) and `<limits>` (budget, turns, time). The SafetyHarness enforces these at runtime. The orchestrator LLM selects which directive to assign based on the task description. There's no formal capability advertisement protocol.

**Recursive delegation is native.** `thread_directive` can spawn children that spawn grandchildren, with depth tracking, capability attenuation, and budget cascade at every level.

### Assessment

| Paper's Requirement          | RYE Status                                                                                               |
| ---------------------------- | -------------------------------------------------------------------------------------------------------- |
| Capability registries        | ⚠️ Local registry (search tool), remote registry (registry API) — but no formal capability advertisement |
| Smart contract formalization | ❌ No smart contracts — trust is cryptographic (signing) not contractual                                 |
| Pre-execution negotiation    | ❌ No negotiation — orchestrator selects directives directly                                             |
| Recursive delegation         | ✅ Native — thread_directive is recursive with depth limits                                              |

---

## 3. Multi-objective Optimization (§4.3)

### What the paper proposes

- Delegators balance cost, speed, quality, privacy, uncertainty
- Pareto optimality across competing objectives
- Continuous re-optimization based on monitoring signals
- Delegation overhead floor (bypass delegation for trivial tasks)

### What RYE implements

**Cost-speed-quality tradeoffs are expressed in directive metadata:**

```xml
<metadata>
  <model tier="orchestrator" />  <!-- expensive, capable -->
  <limits>
    <turns>30</turns>
    <spend>3.00</spend>
    <duration_seconds>1800</duration_seconds>
  </limits>
</metadata>
```

Orchestrator directives use expensive models (sonnet-tier) with high budgets. Leaf execution directives use cheap models (haiku-tier) with tight budgets. The human author makes the Pareto tradeoff at design time; the framework enforces it at runtime.

**Budget cascade provides continuous cost tracking.** Spend is tracked per-thread, reserved from parent budgets, and cascaded upward. The limit checker runs pre-turn and post-turn. But there's no dynamic re-optimization — if a child is burning budget, the parent doesn't automatically intervene.

### Assessment

| Paper's Requirement        | RYE Status                                                            |
| -------------------------- | --------------------------------------------------------------------- |
| Multi-objective tradeoffs  | ✅ Model tier + limits + budget — but static at directive design time |
| Continuous re-optimization | ❌ No dynamic re-optimization based on monitoring signals             |
| Delegation overhead floor  | ✅ Implicit — simple tasks run as single tool calls, not threads      |

---

## 4. Adaptive Coordination (§4.4)

### What the paper proposes

- Runtime re-allocation when performance degrades
- External triggers: task cancellation, resource outages, priority preemption
- Internal triggers: performance degradation, budget overrun, verification failure, unresponsive delegatee
- Checkpoint artifacts for task resumption
- Centralized vs. decentralized orchestration

### What RYE implements

**Error classification and retry are fully data-driven:**

```yaml
# error_classification.yaml
patterns:
  - id: "http_429"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "error.message"
          op: "regex"
          value: "rate limit|too many requests"
    retry_policy:
      type: "use_header"
      header: "retry-after"
      fallback:
        type: "exponential"
        base: 2.0
        max: 60.0
```

The error_loader classifies errors by pattern matching, determines retryability, and calculates delay. The hook system (error event) decides whether to retry, abort, or escalate. All configurable via YAML — no code changes needed.

**External triggers are supported:**

| Trigger             | RYE Mechanism                                                                          |
| ------------------- | -------------------------------------------------------------------------------------- |
| Task cancellation   | `orchestrator.cancel_thread(thread_id)` — sets cancelled flag, harness checks pre-turn |
| Kill unresponsive   | `orchestrator.kill_thread(thread_id)` — sends SIGKILL                                  |
| Priority preemption | Not directly supported                                                                 |
| Resource outage     | Error classification catches connection errors, timeouts                               |

**Checkpoint and resumption:**

- Thread state is persisted to `thread.json` (limits, capabilities, cost, status)
- Full transcript saved per-thread
- `resume_thread` operation appends a new message and continues
- `handoff_thread` compresses context via summary directive + trailing turns, then resumes under a token ceiling

**Resilience configuration:**

```yaml
# resilience.yaml
child_policy:
  on_parent_cancel: "cascade_cancel"
  on_parent_complete: "allow"
  on_parent_error: "cascade_cancel"

concurrency:
  max_concurrent_children: 5
  max_total_threads: 20
```

### Assessment

| Paper's Requirement                          | RYE Status                                                       |
| -------------------------------------------- | ---------------------------------------------------------------- |
| Error detection and retry                    | ✅ Declarative error classification, configurable retry policies |
| Task cancellation                            | ✅ cancel_thread, kill_thread                                    |
| Budget overrun detection                     | ✅ Pre-turn limit checking, budget ledger                        |
| Checkpoint artifacts                         | ✅ thread.json + transcript persistence                          |
| Task resumption                              | ✅ resume_thread, handoff_thread with context compression        |
| Cascade policies                             | ✅ Configurable child_policy (cascade_cancel, allow)             |
| Dynamic re-allocation to different delegatee | ❌ No mid-task delegatee switching                               |

---

## 5. Monitoring (§4.5)

### What the paper proposes

Five axes:

1. **Target** — outcome-level vs. process-level
2. **Observability** — direct (API polling) vs. indirect (environment observation)
3. **Transparency** — black-box vs. white-box (reasoning traces)
4. **Privacy** — full transparency vs. cryptographic (ZK proofs)
5. **Topology** — direct (1-to-1) vs. transitive (signed attestations)

### What RYE implements

**Process-level monitoring is built in.** The EventEmitter emits structured events throughout the LLM loop:

| Event              | Data                                 |
| ------------------ | ------------------------------------ |
| `cognition_in`     | User/system message text             |
| `cognition_out`    | LLM response text, model name        |
| `tool_call_start`  | Tool name, call ID, input parameters |
| `tool_call_result` | Call ID, output data                 |

These are persisted to the Transcript (per-thread) and emitted to configurable sinks (file, WebSocket, null).

**Direct observability via orchestrator:**

| Operation           | Purpose                                     |
| ------------------- | ------------------------------------------- |
| `get_status`        | Read thread.json for current status/cost    |
| `list_active`       | All running threads                         |
| `aggregate_results` | Collect results from multiple children      |
| `read_transcript`   | Full execution trace with tail support      |
| `get_chain`         | Parent→child chain for any thread           |
| `chain_search`      | Regex/text search across thread transcripts |

**White-box transparency.** Parent threads can read child transcripts — full reasoning traces, tool calls, and results. This is white-box monitoring by the paper's taxonomy.

**Transitive monitoring via chain operations.** `get_chain` resolves the full delegation chain. `chain_search` searches across all threads in a tree. Budget cascades upward with per-thread accounting.

**Privacy axis.** Not addressed — no ZK proofs, no encrypted monitoring. All monitoring is full-transparency within the thread hierarchy.

### Assessment

| Paper's Axis                  | RYE Status                                     |
| ----------------------------- | ---------------------------------------------- |
| Outcome-level monitoring      | ✅ Thread status, result aggregation           |
| Process-level monitoring      | ✅ Full event stream, transcript persistence   |
| Direct observability          | ✅ Status polling, transcript reading          |
| White-box transparency        | ✅ Full reasoning traces accessible to parents |
| Privacy-preserving monitoring | ❌ No ZK proofs or encrypted monitoring        |
| Transitive monitoring         | ✅ Chain resolution, cross-transcript search   |

---

## 6. Trust and Reputation (§4.6)

### What the paper proposes

- Reputation as aggregated verifiable history
- Trust as private, context-dependent threshold
- Immutable performance ledger
- Web of Trust with Verifiable Credentials
- Behavioral/explainability metrics
- Graduated authority based on trust level

### What RYE implements

**Cryptographic trust, not reputation-based trust.** RYE's trust model is binary and cryptographic:

1. **Ed25519 signing** — Every item (directive, tool, knowledge) carries a signature
2. **Trust Store** — Public keys stored by fingerprint in `~/.ai/trusted_keys/`
3. **TOFU pinning** — Registry public key pinned on first pull
4. **Integrity verification** — 4-check pipeline (signature exists, hash matches, signature valid, key trusted) runs before every tool execution

There is no reputation scoring, no performance history aggregation, no graduated authority based on past behavior. Trust is binary: your key is in the trust store, or it isn't. Your signature is valid, or it isn't.

**Where RYE's approach differs philosophically:** The paper envisions trust as a continuous signal in an open marketplace. RYE envisions trust as a cryptographic property of artifacts — you trust the item because you trust the signer, verified by Ed25519. This is closer to how package managers (npm, pip) work than how marketplaces work.

### Assessment

| Paper's Requirement                     | RYE Status                                                    |
| --------------------------------------- | ------------------------------------------------------------- |
| Cryptographic verification of artifacts | ✅ Ed25519 signing, integrity verification on every execution |
| Trust store with key management         | ✅ TOFU pinning, manual key trust                             |
| Reputation scoring                      | ❌ Not implemented — binary trust model                       |
| Performance history ledger              | ❌ Budget ledger exists but not used for trust decisions      |
| Graduated authority                     | ❌ Capabilities are static per-directive, not trust-adaptive  |

---

## 7. Permission Handling (§4.7)

### What the paper proposes

- Privilege attenuation in delegation chains
- Just-in-time, scoped permissions for high-stakes tasks
- Semantic constraints (not just binary access)
- Meta-permissions (governing which permissions can be granted)
- Continuous validation and automated revocation
- **Delegation Capability Tokens (DCT)** — proposed as a future protocol extension based on Macaroons/Biscuits

### What RYE implements

**This is where RYE most directly implements what the paper proposes as future work.**

**Capability attenuation is native:**

```python
# SafetyHarness.__init__
if permissions:  # From directive XML
    self._capabilities = [p["content"].replace("/", ".") for p in permissions]
elif parent_capabilities:  # Inherited from parent
    self._capabilities = parent_capabilities
```

Children inherit parent capabilities or declare a subset. A child cannot escalate beyond its parent's capabilities.

**Semantic constraints via fnmatch wildcards:**

```
rye.execute.tool.rye.file-system.*     → Can execute any file-system tool
rye.execute.tool.rye.file-system.read  → Can only execute read
rye.search.tool                        → Can search for tools
```

This is not binary access — it's operation-specific, namespace-scoped, with wildcard patterns. The paper's example ("read-only access to specific rows, or execute-only access to a specific function") maps directly to RYE's capability format.

**Fail-closed enforcement:**

```python
def check_permission(self, primary, item_type, item_id):
    if not self._capabilities:
        return {"error": "Permission denied: no capabilities declared"}
    # ... fnmatch checking ...
```

No capabilities = all actions denied. Every tool call checked before dispatch.

**Ed25519-signed capability tokens:**

The paper proposes "Delegation Capability Tokens" as a future extension. RYE's `capability_tokens.py` (683 lines) implements:

| Feature                            | Paper's DCT Proposal                               | RYE's Implementation                               |
| ---------------------------------- | -------------------------------------------------- | -------------------------------------------------- |
| Cryptographic signing              | ✓ Proposed (Macaroons/Biscuits)                    | ✅ Ed25519 signed                                  |
| Attenuation / restriction chaining | ✓ Proposed                                         | ✅ Parent token ID tracking, capability subsetting |
| Audience binding                   | Not mentioned                                      | ✅ `aud` field prevents cross-service replay       |
| Expiry                             | ✓ Proposed                                         | ✅ UTC-based expiry                                |
| Scope restriction                  | ✓ Proposed ("ONLY for folder X AND ONLY for READ") | ✅ fnmatch patterns on capability strings          |

**What RYE lacks from the paper's vision:**

- No continuous validation / automated revocation based on trust metrics
- No meta-permissions (governing who can grant what)
- No just-in-time permission escalation with human approval gates

### Assessment

| Paper's Requirement          | RYE Status                                                 |
| ---------------------------- | ---------------------------------------------------------- |
| Privilege attenuation        | ✅ Native — capabilities narrow down delegation chains     |
| Semantic constraints         | ✅ fnmatch patterns with namespace/operation granularity   |
| Fail-closed default          | ✅ No capabilities = all denied                            |
| Delegation Capability Tokens | ✅ Implemented — Ed25519 signed, audience-bound, expiring  |
| Restriction chaining         | ✅ Parent capabilities inherited, children can only subset |
| Automated revocation         | ❌ Not implemented                                         |
| Meta-permissions             | ❌ Not implemented                                         |
| Human-gated escalation       | ❌ Not implemented                                         |

---

## 8. Verifiable Task Completion (§4.8)

### What the paper proposes

- Direct outcome inspection for high-verifiability tasks
- Third-party auditing
- Cryptographic proofs (ZK-SNARKs)
- Game-theoretic consensus
- Signed verifiable credentials on completion
- Recursive verification in delegation chains
- Dispute resolution with escrow

### What RYE implements

**Outcome verification is delegated to the parent LLM.** When a child thread completes, its result is returned to the parent thread's LLM, which evaluates it. This is "direct outcome inspection" by the paper's taxonomy, performed by the orchestrator model rather than formal verification.

**Cryptographic artifact integrity exists but is about provenance, not task completion.** Ed25519 signatures verify that a tool or directive hasn't been tampered with — they don't attest that a task was completed correctly.

**Thread results are persisted and auditable:**

- `thread.json` records status, cost, model, limits, capabilities
- Transcripts record full conversation including tool calls and results
- `aggregate_results` collects results from multiple children
- `chain_search` enables post-hoc auditing across threads

**No formal verification, no ZK proofs, no game-theoretic consensus, no escrow.**

### Assessment

| Paper's Requirement               | RYE Status                                                        |
| --------------------------------- | ----------------------------------------------------------------- |
| Direct outcome inspection         | ✅ Parent LLM evaluates child results                             |
| Audit trail                       | ✅ Transcripts, thread.json, chain operations                     |
| Cryptographic proof of completion | ❌ No ZK proofs or signed completion attestations                 |
| Third-party auditing              | ❌ No third-party verification primitives                         |
| Dispute resolution                | ❌ No escrow or dispute mechanisms                                |
| Recursive verification in chains  | ⚠️ Parent evaluates children, but no formal chain-of-attestations |

---

## 9. Security (§4.9)

### What the paper proposes

Threat categories:

- Malicious delegatee (data exfiltration, poisoning, verification subversion, resource exhaustion, backdoors)
- Malicious delegator (harmful task delegation, prompt injection, model extraction)
- Ecosystem-level (Sybil attacks, collusion, agent traps, agentic viruses)

Defenses: trusted execution environments, least privilege, prompt sanitization, decentralized identifiers, mutual TLS.

### What RYE implements

**Supply-chain integrity:**

- Every tool and directive is Ed25519 signed
- Every element in the execution chain is integrity-verified before running
- Lockfiles pin specific versions with hash verification
- Bundle manifests provide per-file integrity for multi-file tools
- Symlink escape detection in dependency verification

**Permission isolation (advisory):**

- Fail-closed capability enforcement prevents unauthorized tool access
- Capability attenuation prevents privilege escalation
- Depth limits prevent infinite spawn loops
- Spawn limits prevent fork bombs
- Budget limits prevent resource exhaustion

**What's advisory, not enforced:**

- Permissions are Python-level, not OS-level sandboxing
- No trusted execution environments (TEE)
- No container/VM isolation
- A malicious tool with valid signature could still exfiltrate data through side channels
- No prompt injection defense at the framework level
- No Sybil attack protection in the registry

### Assessment

| Paper's Threat           | RYE's Defense                                               |
| ------------------------ | ----------------------------------------------------------- |
| Tampered artifacts       | ✅ Ed25519 chain verification                               |
| Unauthorized tool access | ✅ Capability enforcement (advisory)                        |
| Resource exhaustion      | ✅ Budget, turn, spawn, depth, duration limits              |
| Privilege escalation     | ✅ Attenuation prevents escalation                          |
| Data exfiltration        | ❌ No TEE or sandboxing                                     |
| Prompt injection         | ❌ No sanitization framework                                |
| Sybil attacks            | ❌ No decentralized identity                                |
| Supply-chain poisoning   | ⚠️ Signing prevents tampering but not malicious-from-origin |

---

## 10. The Paper's MCP Critique and RYE's Response

The paper's assessment of MCP (§6) identifies specific limitations:

> "MCP defines capabilities but lacks the policy layer to govern usage permissions or support deep delegation chains. It provides binary access — granting callers full tool utility — without native support for semantic attenuation."

> "MCP is stateless regarding internal reasoning, exposing only results rather than intent or traces."

> "The protocol is agnostic to liability and lacks native mechanisms for reputation or trust."

RYE is built as a layer on top of MCP that specifically addresses these critiques:

| MCP Limitation (per paper)       | RYE's Addition                                                   |
| -------------------------------- | ---------------------------------------------------------------- |
| No policy layer for permissions  | SafetyHarness with capability enforcement                        |
| Binary access (no attenuation)   | fnmatch capability strings with semantic granularity             |
| No deep delegation chain support | Recursive thread spawning with depth tracking and budget cascade |
| Stateless (no reasoning traces)  | Full transcript persistence, EventEmitter, chain_search          |
| No trust mechanisms              | Ed25519 signing, trust store, TOFU pinning                       |

The paper proposes extending MCP with "Delegation Capability Tokens" and "monitoring streams" as future protocol work. RYE has implemented both.

---

## 11. What the Paper Proposes That RYE Doesn't Address

These are genuine gaps — areas where the paper's vision goes beyond what RYE targets:

| Paper's Proposal                                                                 | Why RYE Doesn't Address It                                                         |
| -------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| **Decentralized marketplace** with bidding, auctions, and competitive assignment | RYE assumes a single-operator model. Directives are authored, not bid on.          |
| **Smart contracts** with escrow, dispute resolution, and automated penalties     | RYE uses cryptographic trust (signing), not contractual trust (blockchain).        |
| **ZK proofs** for privacy-preserving verification                                | Different threat model — RYE assumes trusted operator, not adversarial delegatees. |
| **Reputation scoring** with performance history and graduated authority          | Binary trust model (key is trusted or not) instead of continuous reputation.       |
| **Human-in-the-loop** approval gates, cognitive friction, and escalation         | No human intervention primitives — threads run autonomously within limits.         |
| **De-skilling mitigation** and curriculum-aware task routing                     | Out of scope — RYE is infrastructure, not workforce management.                    |
| **Game-theoretic verification** (consensus games, Schelling points)              | No multi-agent verification — parent LLM evaluates child results.                  |

These gaps are largely by design. RYE targets a **single-operator deployment model** (one team, one `.ai/` directory, one trust boundary) rather than the paper's **open agentic marketplace** model. The paper's marketplace-oriented features (bidding, escrow, reputation, dispute resolution) would become relevant if RYE's registry evolves into a live marketplace where unknown agents execute directives from unknown authors.

---

## 12. What RYE Has That the Paper Doesn't Propose

Several RYE capabilities go beyond the paper's framework:

| RYE Capability                                                                                                                              | Paper's Coverage                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| **Self-hosting runtime** — the agent system runs as items managed by itself                                                                 | Not discussed. The paper assumes framework code is separate from the artifacts it manages.       |
| **Data-driven runtimes** — new language runtimes are YAML configs                                                                           | Not discussed. The paper assumes fixed execution infrastructure.                                 |
| **Portable artifact system** — `.ai/` directory with 3-tier space precedence                                                                | Not discussed. The paper focuses on protocol-level interoperability, not artifact portability.   |
| **Declarative error classification** — YAML-driven error patterns with condition DSL                                                        | The paper discusses error handling abstractly; RYE has a concrete, data-driven implementation.   |
| **Bundle manifests** — signed manifests covering non-signable assets                                                                        | Not discussed. The paper's trust model doesn't address artifact integrity at this level.         |
| **Context-aware thread resumption** — summary directive + trailing turns + token ceiling                                                    | The paper mentions "checkpoint artifacts" but doesn't address context window management.         |
| **Configurable everything via YAML override** — error patterns, retry policies, resilience, coordination, provider configs, hook conditions | The paper discusses adaptability conceptually; RYE makes it concrete through the loader pattern. |

---

## 13. Summary

The DeepMind paper is a rigorous theoretical framework that correctly identifies the gaps in current agent delegation systems. Its nine protocols (task decomposition, assignment, multi-objective optimization, adaptive coordination, monitoring, trust/reputation, permission handling, verifiable completion, security) form a comprehensive specification for what intelligent delegation should look like.

RYE OS implements the majority of these protocols as working code:

| Protocol                     | Coverage                                                               |
| ---------------------------- | ---------------------------------------------------------------------- |
| Task Decomposition           | ✅ Implemented (LLM-driven via directives)                             |
| Task Assignment              | ⚠️ Partial (direct selection, no marketplace)                          |
| Multi-objective Optimization | ⚠️ Partial (static at design time, not dynamic)                        |
| Adaptive Coordination        | ✅ Implemented (error classification, retry, resume, cancel, cascade)  |
| Monitoring                   | ✅ Implemented (process-level, white-box, transitive)                  |
| Trust & Reputation           | ⚠️ Partial (cryptographic trust, no reputation scoring)                |
| Permission Handling          | ✅ Implemented (attenuation, semantic constraints, capability tokens)  |
| Verifiable Task Completion   | ⚠️ Partial (LLM-based evaluation, audit trail, no formal verification) |
| Security                     | ⚠️ Partial (supply-chain integrity, advisory permissions, no TEE)      |

**The key divergence is philosophical.** The paper envisions an open, adversarial, marketplace-oriented agentic web where unknown agents transact with unknown agents. RYE envisions a trust-bounded, single-operator system where a team authors, signs, and executes their own workflows with cryptographic integrity guarantees. These are complementary visions targeting different deployment models.

What's notable is that RYE's concrete implementation of permission handling — particularly Ed25519-signed capability tokens with attenuation, audience binding, and expiry — directly implements what the DeepMind paper proposes as a theoretical "Delegation Capability Token" extension. The paper cites Macaroons and Biscuits as inspiration; RYE built it independently using Ed25519.

---

## References

- Tomašev, N., Franklin, M., and Osindero, S. (2026). _Intelligent AI Delegation_. Google DeepMind. arXiv:2602.11865v1.
- RYE OS source: `rye/rye/.ai/tools/rye/agent/threads/` (orchestration), `rye/rye/utils/trust_store.py` (trust), `rye/rye/.ai/tools/rye/agent/permissions/capability_tokens/` (capability tokens), `rye/rye/.ai/tools/rye/agent/threads/config/` (data-driven configs).
