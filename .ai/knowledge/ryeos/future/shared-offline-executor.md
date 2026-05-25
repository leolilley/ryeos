<!-- ryeos:signed:2026-05-25T06:08:13Z:d548309ac2de35dea54c6882848591859e569ccc34a350652b0ce7de93a7f65f:qTMU3aXR7iAcX7S1taAo2xUnd70xw5A3fOZSEBF6a5JwiuFs0YR2Sb1egt+X4oRgrZCo/A3etlXXTX2b80foDA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
- Execution needs centralized tracing, policy, sandboxing, lifecycle control, or cancellation.
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
