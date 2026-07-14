<!-- ryeos:signed:2026-07-14T01:54:46Z:dc3a9788296ffef7ba326e01b29191b09cadf08e2f34915ec24a5a3518e84779:uRPadTdM66oBECGAM2ACsdfSkZQ+nqQenpUqJs8oRJfdRUeLczleMUkoHBa2+3fB3Z63WtGG75ERl/vtOiXABg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
# Future: Shared Engine-Backed Offline Executor

## Status

Not now. The current CLI offline-dispatch rewrite should stay a scoped, CLI-owned implementation:

1. load aliases and verbs from node config;
2. boot the engine with installed bundle roots;
3. call `engine.effective_item(... expected_kind: None ...)`;
4. inspect composed dispatch fields;
5. resolve trusted bundle binaries and run them.

This keeps the immediate `ryeos tui` path small while removing duplicated descriptor parsing/signature/template/binary-resolution logic from the CLI.

## Trigger for revisiting

Create a shared executor only when there is real pressure from more than one caller or from protocol complexity. Good triggers:

- `ryeos execute <item_ref>` needs to share the same local/offline path as verb dispatch.
- The daemon or another local worker needs identical `cli_exec` / subprocess launch semantics.
- More executable item shapes appear beyond the current field patterns.
- Protocol handling grows beyond simple env injection, argv forwarding, stdin/stdout mode, and trusted binary resolution.
- Execution needs centralized tracing, lifecycle control, or cancellation.
- The existing shared immutable sandbox boundary proves insufficient for a new
  offline executable shape; sandboxing alone is no longer a trigger because
  current offline tool/service launches already use the RyeOS strict policy.
- Engine startup cost per CLI invocation becomes a measurable problem and needs a cached local execution service.

## Target shape

The executor should sit below the CLI and above raw process spawning. It should consume engine output, not re-resolve descriptor schemas itself.

```text
CLI / daemon / local worker
        │
        ▼
Engine effective_item(item_ref)
        │
        ▼
Shared offline executor
  - inspect composed dispatch fields
  - apply protocol/env rules
  - resolve bundle binary refs
  - run child process
  - return silent/json/streaming outcome
```

## Design principles

- The executor should not switch on hardcoded kind names as its primary model.
- Prefer composed field contracts over kind-specific Rust structs.
- Keep alias and verb loading outside this executor unless node config itself moves into engine kinds.
- Keep trusted binary resolution delegated to `ryeos_engine::binary_resolver`.
- Treat protocols as the natural place for launch semantics once they become richer than today’s small `cli_exec` subset.
- Keep outcome explicit: TTY-owning clients need a silent outcome; tools/services may produce JSON.

## Likely API sketch

```rust
pub enum LocalExecutionOutcome {
    Silent,
    Json(serde_json::Value),
    // Future: Streaming, Detached, ReplacedProcess, etc.
}

pub struct LocalExecutionRequest<'a> {
    pub engine: &'a ryeos_engine::engine::Engine,
    pub item: ryeos_engine::engine::EffectiveItem,
    pub tail: &'a [String],
    pub params: Option<serde_json::Value>,
    pub project_path: &'a str,
    pub system_space_dir: &'a std::path::Path,
}

pub fn execute_local(request: LocalExecutionRequest<'_>) -> Result<Option<LocalExecutionOutcome>, Error>;
```

Do not add this abstraction until the caller count or protocol complexity justifies it.
