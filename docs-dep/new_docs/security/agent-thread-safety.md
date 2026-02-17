# Agent Thread Safety

The `SafetyHarness` wraps every directive execution with resource limits and permission enforcement. It is the runtime guard that prevents runaway threads and unauthorized tool calls.

Implemented in [`rye/rye/.ai/tools/rye/agent/threads/safety_harness.py`](rye/rye/.ai/tools/rye/agent/threads/safety_harness.py).

## Execution Flow

When a directive thread starts ([`thread_directive.py`](rye/rye/.ai/tools/rye/agent/threads/thread_directive.py)):

1. **Load directive** — `_load_directive()` resolves and parses the directive file
2. **Verify signature** — `verify_item()` checks the Ed25519 signature (blocking — thread cannot start if verification fails)
3. **Mint capability token** — `_mint_token_from_permissions()` creates a `CapabilityToken` from declared permissions
4. **Create safety harness** — `SafetyHarness` initialized with limits, hooks, and the capability token
5. **Run tool loop** — `_run_tool_use_loop()` iterates LLM calls and tool executions, each validated by the harness

## Integrity Verification Points

Signatures are verified at two distinct points:

| Point          | Module                                  | What's Verified                           |
| -------------- | --------------------------------------- | ----------------------------------------- |
| Directive load | `thread_directive.py` → `verify_item()` | The directive file itself                 |
| Tool execution | `PrimitiveExecutor` → `verify_item()`   | Every file in the tool's resolution chain |

Both are blocking. A failed verification aborts the operation.

## Limits

Directives declare resource limits in their `<metadata>` block:

```xml
<metadata>
  <limits turns="20" tokens="50000" spawns="3" spend="0.50" duration="300" />
</metadata>
```

| Limit      | Tracks                             | Behavior on Exceed  |
| ---------- | ---------------------------------- | ------------------- |
| `turns`    | LLM round-trips                    | Stops tool loop     |
| `tokens`   | Total token usage (input + output) | Stops tool loop     |
| `spawns`   | Child thread count                 | Prevents new spawns |
| `spend`    | Estimated cost in USD              | Stops tool loop     |
| `duration` | Wall-clock seconds                 | Stops tool loop     |

The `CostTracker` dataclass accumulates these values during execution:

```python
@dataclass
class CostTracker:
    turns: int = 0
    tokens: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    spawns: int = 0
    spend: float = 0.0
    start_time: float = field(default_factory=time.time)
```

`check_limits()` is called before each tool invocation. When a limit is exceeded, it returns an event dict:

```python
{"name": "limit", "code": "turns_exceeded", "current": 21, "max": 20}
```

## HarnessAction

When limits are exceeded or hooks fire, the harness returns a `HarnessResult` with one of these actions:

| Action     | Effect                              |
| ---------- | ----------------------------------- |
| `CONTINUE` | Proceed normally                    |
| `RETRY`    | Re-attempt the current operation    |
| `SKIP`     | Skip the current step               |
| `FAIL`     | Fail the current step with an error |
| `ABORT`    | Abort the entire thread             |

## Permission Enforcement

The harness validates tool calls against the capability token before execution. If the token lacks the required capability:

```python
{
    "name": "error",
    "code": "permission_denied",
    "detail": {
        "missing": ["rye.execute.tool.rye.file-system.fs_write"],
        "granted": ["rye.execute.tool.rye.file-system.fs_read"],
        "required": ["rye.execute.tool.rye.file-system.*"],
    }
}
```

## Child Thread Attenuation

When a thread spawns a child, `_attenuate_token_for_child()` computes the capability intersection — the child only gets capabilities that both the parent has and the child declares. See [Capability Tokens](capability-tokens.md) for details.
