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

| Layer      | Concern                 | What it does                                                                              |
| ---------- | ----------------------- | ----------------------------------------------------------------------------------------- |
| **Key**    | Identity                | Who authorized this                                                                       |
| **Node**   | Environment attestation | Declares what hardware, what capabilities, what isolation is available                    |
| **Lillux** | Enforcement             | Applies constraints at execution time. Will not spawn unless sandbox requirements are met |

The node declares. Lillux enforces. The node says "I have these capabilities and these restrictions." Lillux says "I will not spawn this process unless these constraints are applied." Declaration above, enforcement below.

This is why enforcement can't live at the node level alone. The node is Python. If sandboxing is above Lillux, you're trusting Python to enforce runtime constraints, which is exactly the kind of bypass Lillux exists to prevent. `lillux exec` is where the subprocess actually gets spawned. That's where cgroups, namespaces, seccomp filters, network restrictions get applied. The Rust binary is the enforcement point.

## What Node Attestation Looks Like

Node state lives at `~/.ai/node/` — machine-local, never in project space, signed by the node's own Ed25519 key (stored at `~/.ai/node/identity/`). On first boot (or on environment change), the node produces a signed attestation at `~/.ai/node/attestation/`:

- Hardware: CPU architecture, GPU models, memory, storage
- Isolation: container runtime, namespace configuration, resource cgroups
- Capabilities: what tool IDs this node can execute (already exists via `/status`)
- Restrictions: what's not available, what's firewalled, what resource limits apply

This attestation is signed by the node's key (from `~/.ai/node/identity/`) and published. Callers can verify it before dispatching execution. TOFU pins the node and its attested environment together. Authorized callers are tracked in `~/.ai/node/authorized-keys/`.

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

---

## Updated Architecture: Sandbox Engines as Data-Driven Providers

After further analysis, the approach shifts from implementing sandboxing primitives directly in Lillux to integrating with existing sandbox solutions as **registered capabilities**. This aligns with RYE's data-driven philosophy and follows the same pattern as model providers, runtimes, and other external systems.

### Relationship to Runtime Config Processing

The engine's `SubprocessSpec` (see `.tmp/RUNTIME-CONFIG-PROCESSING-v2.md`) is the
composition boundary. Both language runtimes and sandbox wrappers produce the same
struct:

1. **Chain resolution** compiles a runtime descriptor into a `SubprocessSpec`
   (cmd, args, env, stdin_data, cwd, timeout_secs)
2. **Sandbox wrapping** transforms a `SubprocessSpec` → `SubprocessSpec` by
   prepending the sandbox engine command and its arguments
3. **Dispatch** receives the final spec and hands it to Lillux — doesn't know
   whether sandboxing was applied

```
chain resolution → SubprocessSpec → sandbox_wrap(spec, config) → SubprocessSpec → dispatch
```

The sandbox wrapper is **not** part of the executor chain. It's applied after chain
resolution, at the spec level. This is why the engine enforces "one runtime descriptor
per chain" — sandbox composition is a different mechanism that operates on the compiled
output, not on the chain walk.

### The Integration Model

Instead of Lillux learning cgroups, namespaces, and seccomp directly, it delegates to **sandbox engines** — existing, battle-tested solutions like nsjail, Firecracker, bubblewrap, or Docker. The sandbox becomes a configuration object, loaded from YAML and applied as a spec wrapper.

```yaml
# .ai/config/sandbox/nsjail.yaml
engine: nsjail
binary: /usr/bin/nsjail
profiles:
  strict:
    config: /etc/ryeos/nsjail/strict.cfg
    capabilities: [cgroups_v2, seccomp, user_namespaces]
  gpu-isolated:
    config: /etc/ryeos/nsjail/gpu.cfg
    capabilities: [cgroups_v2, seccomp, user_namespaces, gpu_passthrough]
invocation:
  args: ["--config", "{profile_config}", "--", "{cmd}"]
  cwd: "{cwd}"
  env_passthrough: ["RYE_*", "HOME", "PATH"]
```

```yaml
# .ai/config/sandbox/bubblewrap.yaml
engine: bubblewrap
binary: /usr/bin/bwrap
profiles:
  strict:
    args: ["--unshare-all", "--die-with-parent", "--ro-bind", "/", "/"]
    capabilities: [user_namespaces]
```

### Sandbox as SubprocessSpec Wrapper

The sandbox engine config compiles into a wrapper function over the inner spec:

```rust
/// Wrap an inner SubprocessSpec with sandbox invocation.
/// Same struct in, same struct out.
fn sandbox_wrap(
    inner: SubprocessSpec,
    sandbox: &SandboxConfig,
    profile: &SandboxProfile,
) -> Result<SubprocessSpec, EngineError> {
    let mut args = sandbox.invocation_args(profile)?;

    // Replace {cmd} with inner.cmd, append inner.args
    // Replace {cwd} with inner.cwd
    args = expand_sandbox_templates(&args, &inner)?;

    Ok(SubprocessSpec {
        cmd: sandbox.binary.clone(),
        args,
        cwd: inner.cwd,  // sandbox may override
        env: filter_env(&inner.env, &sandbox.env_passthrough),
        stdin_data: inner.stdin_data,
        timeout_secs: inner.timeout_secs,
    })
}
```

The invocation template tokens (`{cmd}`, `{cwd}`, `{profile_config}`) are sandbox-specific
and separate from the runtime config template vocabulary (`{tool_path}`, `{rye_python}`, etc.).
Both use the same closed-vocabulary, fail-on-unknown approach.

### Node Attestation with Sandbox Engines

The node's attestation document declares which sandbox engines are available and with what profiles:

```json
{
  "kind": "attestation/v1",
  "node_id": "fp:a3f8c921e7b04d12",
  "environment": {
    "platform": "linux",
    "architecture": "x86_64"
  },
  "isolation": {
    "engines": [
      {
        "name": "nsjail",
        "version": "3.4",
        "profiles": ["strict", "gpu-isolated", "network-restricted"],
        "capabilities": ["cgroups_v2", "seccomp", "user_namespaces"]
      },
      {
        "name": "bubblewrap",
        "version": "0.8.0",
        "profiles": ["strict"],
        "capabilities": ["user_namespaces"]
      }
    ]
  }
}
```

### Directive Sandbox Requirements

Directives declare sandbox requirements using the same engine/profile model:

```yaml
metadata:
  sandbox:
    engine: "nsjail"
    profile: "strict"
    requires:
      capabilities: ["cgroups_v2", "seccomp"]
```

### Lillux Integration

Lillux gains a `--sandbox` flag that accepts engine configuration:

```rust
#[derive(Subcommand)]
pub enum ExecAction {
    Run {
        // ... existing fields
        #[arg(long)]
        sandbox: Option<String>,  // JSON: {"engine": "nsjail", "profile": "strict"}
    }
}
```

When `--sandbox` is provided, Lillux:

1. Loads the engine configuration from `.ai/config/sandbox/{engine}.yaml`
2. Resolves the profile and constructs the invocation
3. Executes the sandbox engine instead of the raw command
4. The sandbox engine handles all isolation primitives

This keeps Lillux focused on its core primitives while delegating complex kernel-level sandboxing to specialized tools.

### Cross-Platform Considerations

Different platforms support different sandbox engines:

| Platform    | Available Engines                       | Notes                     |
| ----------- | --------------------------------------- | ------------------------- |
| **Linux**   | nsjail, bubblewrap, Firecracker, Docker | Full ecosystem            |
| **FreeBSD** | jails, bhyve                            | Native OS-level isolation |
| **OpenBSD** | pledge/unveil integration               | Syscall-level sandboxing  |
| **macOS**   | sandbox-exec, Docker                    | Limited native options    |
| **Windows** | Docker, Windows Sandbox                 | Container-based           |

The attestation honestly declares what's available. A macOS node might only offer `{"engines": [{"name": "docker", "profiles": ["basic"]}]}` while a Linux node offers the full range.

### BSD and Enhanced Security Model

FreeBSD and OpenBSD provide unique advantages for RYE's security model:

#### FreeBSD with Jails

- **Native OS-level isolation**: Jails provide clean process and filesystem isolation without complex userspace configuration
- **ZFS integration**: Content-addressed storage gets deduplication, snapshotting, and send/receive for free
- **Coherent base system**: Single-team development of kernel + userland reduces attack surface
- **GPU passthrough**: PCIe device passthrough to jails enables clean GPU isolation

#### OpenBSD with pledge/unveil

- **Syscall-level capability enforcement**: pledge() restricts available syscalls, unveil() restricts filesystem access
- **Auditable security**: Every component is designed for human auditability
- **Minimal attack surface**: Features removed if they increase complexity
- **Natural alignment**: OpenBSD's capability model mirrors RYE's declared permissions

### tinygrad and Hardware Independence

tinygrad's architecture enables GPU inference on BSD platforms through multiple paths:

#### Hardware Command Queue (HCQ) Model

tinygrad's HCQ bypasses vendor runtimes (CUDA, ROCm, HIP) and talks directly to hardware via command queues. This eliminates dependencies on proprietary runtime stacks.

#### AMD RDNA3/RDNA4 Support

The AM driver is a complete userspace GPU driver for AMD RDNA3/RDNA4:

- No kernel driver required for compute workloads
- Direct PCIe device access
- Memory management and compute queue binding in Python
- Works with FreeBSD jails via PCIe passthrough

#### Nvidia Open Source Path

With Nvidia's open-source kernel modules (mandatory for Blackwell, available for older generations):

- FreeBSD developers are porting nvidia-drm-kmod via linuxkpi
- tinygrad's NV backend can bypass CUDA runtime
- Direct PCI interface eliminates proprietary userspace dependencies

### The Complete Stack on BSD

For a RYE node on FreeBSD with GPU inference:

```
FreeBSD kernel
├── Jail isolation (OS-level)
├── PCIe device passthrough
├── ryeosd process
    ├── Lillux execution boundary
    ├── tinygrad AM driver (userspace)
    └── GPU hardware (RDNA3/RDNA4)
```

Every layer is auditable. The signing boundary, jail boundary, and hardware boundary coincide. There's no opaque kernel blob in the execution path.

### Use Cases for BSD Deployment

#### Regulated Data Handling

Legal, medical, or financial firms processing privileged documents need fully auditable execution paths. BSD + RYE provides cryptographic proof of capability boundaries.

#### Government/Defense Contracts

Supply chain integrity requirements demand auditable software stacks. A RYE node on FreeBSD with signed task chains satisfies these requirements.

#### Multi-tenant Inference

Shared hardware serving multiple clients requires cryptographic isolation guarantees. Jail isolation + signed execution boundaries make this enforceable rather than policy-based.

#### High-value Autonomous Agents

Agents with real-world consequences (trading, infrastructure, communications) need audit trails with cryptographic integrity. The signed directive chain becomes legally admissible evidence.

### Implementation Phases

#### Phase 1: Attestation Foundation (Cross-platform)

- Environment probing (hardware, OS, available isolation)
- Signed attestation documents
- `/attestation` endpoint
- Constraint matching before dispatch

#### Phase 2: Sandbox Engine Integration (Linux-first)

- YAML-based engine configuration
- `--sandbox` flag in Lillux
- nsjail and bubblewrap integration
- Directive sandbox requirements

#### Phase 3: BSD Native Support

- FreeBSD jail integration
- OpenBSD pledge/unveil integration
- tinygrad AM driver on FreeBSD
- PCIe passthrough for GPU isolation

#### Phase 4: Hardware Attestation (Future)

- TPM-backed attestation
- SEV-SNP encrypted execution
- Hardware-verified isolation claims

### What This Achieves

**Data-driven consistency**: Sandbox engines follow the same YAML configuration pattern as model providers and runtimes. Adding support for a new sandbox = dropping a config file.

**Battle-tested security**: Instead of implementing sandboxing primitives, RYE leverages decades of hardening in nsjail, jails, and pledge/unveil.

**Cross-platform honesty**: Each platform's attestation honestly declares available isolation. No false security claims.

**Verifiable architecture**: On BSD, every layer from directive to hardware is auditable. The security model has no footnotes.

**Practical deployment**: Linux nodes handle throughput-sensitive workloads, BSD nodes handle trust-sensitive workloads. The architecture supports both.

The result is a sandboxing model that's more principled than custom implementation, more honest than container-based solutions, and more verifiable than proprietary stacks — while remaining practical for production deployment.
