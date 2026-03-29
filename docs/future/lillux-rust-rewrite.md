```yaml
id: lillux-rust-rewrite
title: "Lillux Rust Rewrite — The Kernel Goes Native"
description: Full rewrite of the Lillux microkernel in Rust. Three primitives — Execute, Memory, Identity — plus a thin Time layer. Zero Python. One binary. The process boundary is the ring transition.
category: future
tags: [lillux, rust, kernel, microkernel, architecture, security, capability]
version: "0.1.0"
status: design-proposal
```

# Lillux Rust Rewrite

> **Status:** Design Proposal — architectural direction is decided, not scheduled for implementation.

## Executive Summary

Lillux is the microkernel of Rye OS. Today it's a Python package (`lillux/kernel/`) wrapping two Rust binaries (`lillux-proc`, `lillux-watch`) plus a set of Python primitives (`subprocess`, `http_client`, `signing`, `integrity`, `cas`, `lockfile`). This grew organically from solving real problems.

The Rust rewrite replaces the entire Python kernel with a single Rust binary. Three primitives — **Execute**, **Memory**, **Identity** — plus a thin **Time** layer. Zero Python in Lillux. The process boundary between Rye (Python, userspace) and Lillux (Rust, kernel) becomes the ring transition — the strongest isolation boundary the operating system provides.

This isn't a performance decision. It's a security architecture decision. The thing that holds your private keys, verifies content integrity, and executes untrusted code should be a compiled binary with no interpreter, no garbage collector, and a minimal attack surface.

---

## Why Now (And Why Not Before)

The architecture already proved itself. `lillux-proc` has been a Rust binary from the start — Python's `SubprocessPrimitive` is just a wrapper that calls it via `asyncio.create_subprocess_exec`. The pattern works: JSON in, JSON out, process boundary isolation. The rewrite completes what `lillux-proc` already started.

The primitives accumulated organically while solving real problems:

| Primitive     | Came From                          |
| ------------- | ---------------------------------- |
| `subprocess`  | Needed to run tools                |
| `http_client` | Needed to call APIs                |
| `signing`     | Needed to trust items              |
| `integrity`   | Needed to verify content           |
| `cas`         | Needed content-addressable storage |
| `lockfile`    | Needed to pin resolved chains      |

Six Python primitives plus two Rust binaries. The rewrite asks: what _must_ be in the kernel, and what was always userspace wearing a kernel hat?

---

## The Fetch-Execute Parallel

RYE's three agent-facing primitives — **Fetch**, **Execute**, **Sign** — mirror the CPU's fetch-execute cycle. This parallel isn't a borrowed metaphor; it emerged from solving the same problem at a different abstraction level.

The CPU cycle is the most reduced description of computation: get a thing, run the thing, repeat. Everything else — scheduling, memory management, interrupts — is infrastructure built around that loop.

RYE makes the same claim for AI agent computation:

| CPU Cycle                                                   | RYE Agent Layer                        | Lillux Kernel Layer                                     |
| ----------------------------------------------------------- | -------------------------------------- | ------------------------------------------------------- |
| **Fetch** — read instruction from memory at program counter | **Fetch** — pull item from CAS by hash | **Memory** — CAS read/write with integrity verification |
| **Execute** — carry out the operation                       | **Execute** — run the tool/directive   | **Execute** — subprocess dispatch                       |
| _(implicit trust — CPU trusts its address space)_           | **Sign** — cryptographic attestation   | **Identity** — Ed25519 signing and verification         |

The CPU cycle has no Sign equivalent because the CPU implicitly trusts everything in its address space. That trust assumption is exactly what makes traditional software insecure. **Sign is the primitive you add when your "instructions" come from untrusted, distributed, model-generated sources rather than a compiler you control.** It's the delta between a CPU OS and an AI OS.

The same structure at two abstraction levels — agent layer and kernel layer — with Execute as the invariant at both. Fractals tend to be right.

---

## Three Primitives + Time

### What Must Be Kernel

The microkernel question: what is the minimum that _must_ be trusted code?

**Execute** — can't be userspace, it _is_ the kernel's reason to exist. The primitive that runs a unit of computation and returns a result. Subprocess today, but the abstraction generalises to WASM sandbox, remote dispatch, GPU offload. The kernel doesn't care what the instruction means, it just runs it.

**Memory** — CAS integrity verification must be trusted. If you can't trust that a hash matches its content, the entire content-addressed architecture collapses. The kernel owns the CAS _interface_ — hash, verify, store, retrieve — but storage backends (filesystem, S3, remote nodes) are userspace drivers that implement a trait.

**Identity** — private key operations must be in the kernel. This is the strongest argument for Rust: the thing that signs blobs should never share a process with interpreted code. A compiled binary with no runtime is the smallest possible trust surface for cryptographic operations.

**Time** — a thin layer, barely a primitive. Monotonic clock and timer callbacks — "what time is it" and "call me back after N milliseconds." ~30 lines of Rust. The kernel exposes the clock; userspace (Rye) decides policy — TTLs, expiry, scheduling.

### What Moves to Userspace

**`http_client`** — networking is I/O, not a kernel operation. seL4, L4, Mach — none put networking in the kernel. HTTP becomes a userspace concern: Rye keeps `httpx`, or a small Rust binary handles it, but either way it's not kernel. Execute doesn't know or care that something makes HTTP calls. If the thing it runs happens to hit a network endpoint, that's userspace business.

**`lockfile`** — lockfile I/O is a Rye concern. It's about tool resolution metadata — which version, which chain, which integrity hash. The kernel just does CAS. If you want to persist a lockfile, you `lillux cas store` the blob.

**`env_resolver`** — resolves what environment to set up, then passes it to `lillux exec` via the process environment. Policy, not mechanism. Always belonged in Rye.

**`auth`** — authentication configuration and credential resolution. Rye-level policy that feeds into kernel-level Identity operations.

**`embedding`** (proposed) — computing an embedding is Execute (run a model). Storing and querying embeddings is Memory (a backend). The "embedding primitive" proposed in other future docs is two operations that already have homes. It doesn't need its own kernel slot.

---

## The Binary

One Rust binary. Subcommands for each primitive.

```
lillux exec <cmd> [args]           # Execute: run and wait
lillux exec --spawn <cmd> [args]   # Execute: detached process
lillux exec --kill <pid>           # Execute: terminate process
lillux exec --status <pid>         # Execute: check process

lillux cas store <path>            # Memory: hash, shard, store → returns hash
lillux cas fetch <hash>            # Memory: retrieve by hash
lillux cas verify <hash>           # Memory: verify integrity

lillux sign <hash>                 # Identity: Ed25519 sign → returns signature
lillux verify <hash> <sig>         # Identity: verify signature
lillux keypair generate            # Identity: generate new keypair
lillux keypair fingerprint         # Identity: compute key fingerprint

lillux time now                    # Time: monotonic clock
lillux time after <ms>             # Time: timer callback
```

`lillux-proc` and `lillux-watch` get absorbed as subcommands. `lillux exec` is what `lillux-proc exec` is today. `lillux watch` (if kept) becomes another subcommand rather than a separate binary.

### Interface

JSON on stdin, JSON on stdout. Same pattern as `lillux-proc` today. The interface is:

```
$ echo '{"command":"python","args":["worker.py"],"timeout":300}' | lillux exec
{"success":true,"stdout":"...","stderr":"","return_code":0,"duration_ms":1234}
```

Process boundary isolation. No PyO3, no FFI, no shared memory. Rye (Python) calls `lillux` via `asyncio.create_subprocess_exec` — exactly like it calls `lillux-proc` today. The ring transition is a `fork+exec`, the same ring transition a real OS uses.

---

## Security Architecture

### The seL4 Insight

seL4 is a microkernel that has been formally verified — mathematically proven correct. ~10,000 lines of C. The philosophy: the kernel should be so small that verification is conceivable.

Lillux will never need formal verification. But the principle is right: a kernel you can read in an afternoon is a different security posture than a 50,000 line framework.

Target: **~500 lines of Rust.**

| Module        | Estimated Lines | Responsibility                                         |
| ------------- | --------------- | ------------------------------------------------------ |
| `main.rs`     | ~50             | Subcommand dispatch                                    |
| `exec.rs`     | ~150            | Process lifecycle (absorbs `lillux-proc`)              |
| `cas.rs`      | ~100            | CAS interface, integrity verification, sharded storage |
| `identity.rs` | ~170            | Ed25519 keypair, signing, verification                 |
| `time.rs`     | ~30             | Monotonic clock, timer                                 |
| **Total**     | **~500**        |                                                        |

Memory backends (filesystem, S3, remote) live outside the kernel as userspace drivers implementing a trait. The kernel owns the interface and the integrity verification — the part that must be trusted.

### Capabilities as Mechanism

Traditional security checks permissions at the syscall boundary — "is this caller allowed to do this?" seL4-style capability security makes the capability the _mechanism_ — you can't call the syscall without holding an unforgeable token.

Rust's type system enables this at zero cost:

```rust
/// A capability token. Non-Copy, non-Clone.
/// Consumed on use — you can't reuse or duplicate it.
/// The compiler enforces privilege boundaries.
pub struct Capability<T: Primitive> {
    scope: FnMatchPattern,
    _phantom: PhantomData<T>,
}

/// Execute literally cannot be called without a valid capability.
/// This isn't a runtime check — it's a compile-time guarantee.
pub fn execute(cap: Capability<Exec>, config: ExecConfig) -> ExecResult {
    // cap is consumed here — cannot be used again
    // ...
}
```

A `Capability<Exec>` that's `!Copy + !Clone`, consumed on use, scoped by lifetime. The compiler enforces what seL4 enforces at runtime. Code that escalates privilege won't compile. That's not a metaphor for security-by-construction — it _is_ security-by-construction.

### Cryptographic MMU

In a traditional OS, the MMU (memory management unit) enforces memory isolation in hardware — a process literally cannot address memory outside its page table. The boundary is physical.

In distributed Rye, the boundary is cryptographic. Signed blobs are kernel-space, unsigned are user-space. The Ed25519 signing chain enforces isolation across machines in a way ring protection never could. The signing chain is the page table — it defines what's addressable within a given trust boundary.

---

## The Ring Transition

```
┌─────────────────────────────────────────┐
│  Rye (Python) — Userspace               │
│                                          │
│  Space resolution, tool metadata,        │
│  directive parsing, MCP server,          │
│  httpx, env_resolver, auth,              │
│  lockfile logic, orchestration           │
│                                          │
├──────────────────────────────────────────┤
│  Process Boundary (fork+exec)            │
│  JSON on stdin/stdout                    │
│  = Ring Transition                       │
├──────────────────────────────────────────┤
│  Lillux (Rust) — Kernel                  │
│                                          │
│  Execute: subprocess dispatch            │
│  Memory: CAS store/fetch/verify          │
│  Identity: sign/verify/keypair           │
│  Time: clock/timer                       │
│                                          │
│  ~500 lines. No interpreter. No GC.      │
│  Private keys never leave this boundary. │
└──────────────────────────────────────────┘
```

The PyO3/FFI alternative was considered and rejected. A process boundary is stronger than a language boundary:

- Separate address space — Rye cannot read Lillux memory
- OS-enforced isolation — not library-enforced
- Crash isolation — Lillux crash doesn't corrupt Rye state
- Auditable interface — JSON in, JSON out, nothing implicit

This is the same isolation model that `lillux-proc` already uses successfully.

---

## What Exists Today vs What Changes

| Component                                        | Today                                | After Rewrite                         |
| ------------------------------------------------ | ------------------------------------ | ------------------------------------- |
| `lillux/kernel/lillux/primitives/subprocess.py`  | Python wrapper calling `lillux-proc` | Absorbed into `lillux exec`           |
| `lillux/kernel/lillux/primitives/signing.py`     | Python using `cryptography` lib      | Absorbed into `lillux sign/verify`    |
| `lillux/kernel/lillux/primitives/integrity.py`   | Python SHA256 hashing                | Absorbed into `lillux cas verify`     |
| `lillux/kernel/lillux/primitives/cas.py`         | Python CAS operations                | Absorbed into `lillux cas`            |
| `lillux/kernel/lillux/primitives/http_client.py` | Python using `httpx`                 | Moved to `rye/runtime/http_client.py` (done) |
| `lillux/kernel/lillux/primitives/lockfile.py`    | Python lockfile I/O                  | Moved to `rye/runtime/lockfile.py` (done)    |
| `lillux/kernel/lillux/runtime/env_resolver.py`   | Python env resolution                | Moved to `rye/runtime/env_resolver.py` (done) |
| `lillux/kernel/lillux/runtime/auth.py`           | Python auth primitives               | Moved to `rye/runtime/auth.py` (done)        |
| `lillux/proc/`                                   | Standalone Rust binary               | Absorbed as `lillux exec` subcommand  |
| `lillux/watch/`                                  | Standalone Rust binary               | Absorbed as `lillux watch` subcommand |

After the rewrite, `lillux/` is a pure Rust workspace:

```
lillux/
  Cargo.toml
  src/
    main.rs
    exec.rs
    cas.rs
    identity.rs
    time.rs
```

Zero Python. The `lillux/kernel/` directory ceases to exist.

---

## Distributed Implications

The three-primitive kernel has properties that matter for decentralised execution:

**CAS eliminates write conflicts.** Two nodes writing the same content produce the same hash. The conflict problem that plagues distributed databases dissolves. Leaderless replication (Dynamo-style) becomes trivial — any node can accept a write, consistency is structural rather than eventual.

**Identity enables Byzantine fault tolerance.** AI agents in a multi-principal environment are inherently Byzantine — a model output is a potentially adversarial message. Sign is the BFT mechanism. An unsigned result is treated as Byzantine regardless of apparent content. The kernel enforces this by construction.

**A RYE node is any machine that can Fetch, Execute, and Sign.** That's the full node spec. No central registry, no enrollment process, no trust authority. You trust a node because you can verify its signatures. The OS becomes a network protocol.

**The log is the system.** Every CAS operation is an immutable append. The current state of a workspace is a projection over the content-addressed log. Entire execution history is auditable and replayable.

---

## Computing Parallels

This architecture draws from fundamental computing concepts. These aren't borrowed metaphors — they emerged from solving the same structural problems at the AI agent layer.

| Concept                | Traditional Computing                                           | Lillux/RYE Equivalent                                                                                      |
| ---------------------- | --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Fetch-execute cycle    | CPU reads instruction, runs it, repeat                          | Fetch item from CAS, Execute it, repeat                                                                    |
| Sign (the addition)    | CPU implicitly trusts its address space                         | Agents operate in adversarial multi-principal environments — Sign is the delta                             |
| Microcode              | Below machine code, primitive ops implementing each instruction | Lillux is microcode — everything collapses to subprocess or CAS regardless of higher-level primitive       |
| Program counter        | Advances through instruction stream, redirected by jumps        | Directive chain — each directive points to the next fetch target, model advances the PC                    |
| Memory hierarchy       | L1/L2/L3 cache → main memory → disk                             | Thread summary (register) → thread chain (L1) → workspace CAS (L2) → shared RAG (main memory)              |
| Ring protection        | Hardware-enforced privilege levels, syscall gates               | Process boundary (fork+exec), JSON interface, signing chain                                                |
| MMU                    | Hardware enforces memory isolation via page tables              | Ed25519 signing chain defines addressable trust boundary                                                   |
| Capability security    | Unforgeable tokens grant access to resources (seL4)             | Rust `!Copy + !Clone` capability types consumed on use — compiler-enforced                                 |
| Stored-program insight | Instructions are data in the same memory                        | Directives are CAS blobs — agents can fetch, modify, sign, and store them back                             |
| Interrupts             | Suspend execution, handle async event, resume                   | Model fires when reasoning is needed — substrate runs deterministic loop, model is the interrupt handler   |
| Out-of-order execution | CPU executes in dependency order, not program order             | Graph execution finds parallelism where dependencies allow — CAS hashes make dependency analysis tractable |
| Von Neumann bottleneck | Single bus between CPU and memory                               | Contention for CAS storage or execution capacity across nodes                                              |
| Superscalar            | Multiple functional units (integer ALU, FPU, memory)            | Different executor types (subprocess, HTTP, GPU) are different functional units                            |

---

## Relationship to Other Future Proposals

| Proposal                                                                | Impact                                                                                                                  |
| ----------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| [Memory & Intent Resolution](memory-and-intent-resolution.md)           | Embedding _computation_ is Execute. Embedding _storage_ is a Memory backend. No new kernel primitive needed.            |
| [Sovereign Inference](sovereign-inference.md)                           | GPU inference is an Execute dispatch target. The kernel doesn't know about models — it runs processes.                  |
| [Execution Nodes](execution-nodes.md)                                   | A node is a machine running the `lillux` binary. Three primitives define the full node spec.                            |
| [Encrypted Shared Intelligence](encrypted-shared-intelligence.md)       | HPKE decryption at the Lillux boundary becomes an Identity operation. Private keys never leave Rust.                    |
| [Dynamic Personality](dynamic-personality.md)                           | Trait retrieval uses Memory (CAS + embedding backend). No kernel changes.                                               |
| [Residual Stream](Residual%20stream%20and%20native%20model%20family.md) | Residual artifacts are CAS blobs. The proposed `residual_stream` primitive is a Memory backend, not a kernel primitive. |

---

## Open Questions

1. **`lillux watch` — keep or cut?** File watching is I/O observation, not computation or storage. It might belong in userspace. But it's already Rust and already works. Pragmatic answer: keep it as a subcommand, revisit later.

2. **Backend trait design.** Memory backends implement a Rust trait. What's the trait surface? Minimum: `store(bytes) → hash`, `fetch(hash) → bytes`, `verify(hash) → bool`. Sharding strategy lives in the trait implementation, not the kernel.

3. **Capability representation over the process boundary.** The `!Copy + !Clone` type works within Rust. But the Rye→Lillux interface is JSON over subprocess. How does a capability token cross the process boundary? Probably a signed, single-use token in the JSON payload — the kernel verifies and invalidates it. The compile-time guarantee becomes a runtime cryptographic guarantee at the boundary.

4. **Migration path.** The Python primitives work today. The rewrite can be incremental — replace one primitive at a time as subcommands, with Python wrappers falling through to the `lillux` binary. `lillux-proc` already proved this path.

---

## Implementation Status

The unified `lillux` binary exists at `lillux/lillux/`. **669 lines of Rust. 1.3MB release binary. All primitives functional and tested.**

### What's done

- `lillux exec run|spawn|kill|status` — full port of `lillux-proc`, identical JSON output format
- `lillux cas store|fetch|verify|has` — CAS with 2-level sharding, Python-compatible canonical JSON (`ensure_ascii=True` matching via surrogate pairs), atomic writes with PID-scoped tmpfiles, hash input validation
- `lillux sign|verify` — Ed25519 compatible with Python `cryptography` lib (both sign hex string bytes)
- `lillux keypair generate|fingerprint` — PKCS8 PEM, secure file creation (`create_new` + `mode(0o600)` on Unix, refuses to overwrite)
- `lillux time now|after` — wall clock and sleep timer
- CAS hash parity verified for: ASCII, non-ASCII BMP (é), non-BMP emoji (😀), control chars, nested key sorting

### Known issues — deferred

These are all inherited from `lillux-proc` and the existing Python primitives. None are regressions. They should be addressed but don't block migration.

| Issue | Severity | Module | Detail |
|---|---|---|---|
| stdin deadlock on large payloads | medium | `exec.rs` | `write_stdin` blocks on main thread before stdout/stderr drain threads spawn. If child fills pipe buffer (~64KB) before consuming stdin, both block. Fix: spawn stdin write in a background thread. |
| EPERM false-dead on Unix | medium | `exec.rs` | `kill(pid, 0)` returns EPERM for alive processes owned by other users. Current code treats any non-zero as "dead." Fix: check `errno == EPERM` to correctly identify alive-but-unpermitted. |
| Access denied false-dead on Windows | medium | `exec.rs` | `OpenProcess` returns 0 on access denied (system processes). Reported as dead. Fix: check `GetLastError()` for `ERROR_ACCESS_DENIED`. |
| Windows grace period is dead time | low | `exec.rs` | `kill_process` waits the grace period but never signals the process first. Windows has no generic SIGTERM. Fix: send `CTRL_BREAK_EVENT` or skip the wait. |
| Binary stdin silently dropped | low | `exec.rs` | `resolve_stdin` uses `read_to_string` — invalid UTF-8 fails silently. Fix: use `Vec<u8>` and `read_to_end`. |
| Process tree not killed on timeout | low | `exec.rs` | Timeout kills only the direct child, not descendants that inherited stdout/stderr. Reader threads can hang if grandchild keeps pipe open. Fix: use process groups (Unix) or Job Objects (Windows). |
| Dir permissions race | low | `identity.rs` | `create_dir_all` uses default umask, then `set_perms(0o700)` tightens. Brief window. Fix: use `DirBuilder::new().mode(0o700)`. |
| Fingerprint PEM vs DER | low | `identity.rs` | Fingerprint hashes PEM text bytes — CRLF conversion across platforms changes the hash. Fix: hash raw DER public key bytes instead. Deferred because changing this would break existing fingerprints and must match Python's current `hashlib.sha256(public_key_pem)` behavior. |

### Migration strategy

The Python `lillux.primitives` module stays as a thin shim with identical API. Internals change to shell out to the `lillux` binary. Callers don't change.

**Step 1 — `subprocess.py`:** Change `shutil.which("lillux-proc")` → `shutil.which("lillux")`, change `lillux-proc exec` → `lillux exec run`, `lillux-proc spawn` → `lillux exec spawn`, etc. Smallest diff, ~5 callers.

**Step 2 — `signing.py`:** Rewrite internals to call `lillux sign`, `lillux verify`, `lillux keypair generate`, `lillux keypair fingerprint`. Drop the `cryptography` dependency. ~60 callers, zero API changes.

**Step 3 — `cas.py`:** Rewrite to call `lillux cas store|fetch|has|verify`. ~25 callers.

**Step 4 — `integrity.py`:** Keep as pure Python (10 lines of `json.dumps` + `hashlib.sha256`). Called in hot paths — subprocess overhead for every hash would be a regression. Rust CAS handles its own hashing internally. Both produce identical output.

Things that stay Python (userspace, now in `rye/runtime/` and `rye/schemas/`): `http_client.py`, `lockfile.py`, `env_resolver.py`, `auth.py`, `errors.py`, `schema_validator.py`.
