```yaml
id: sovereign-inference
title: "Sovereign Inference — Self-Hosted Models Through RYE Execution"
description: Self-hosted LLM inference on your own hardware — tinygrad in-process on GPU execution nodes, a standalone completions server as the external-facing surface, and cluster routing through RYE's execute primitive. The model is a device driver. The cluster is the kernel.
category: future
tags:
  [
    inference,
    self-hosted,
    tinygrad,
    gpu,
    cluster,
    tool-use,
    execution,
    hardware,
  ]
version: "0.1.0"
status: exploratory
```

# Sovereign Inference — Self-Hosted Models Through RYE Execution

> **Status:** Exploratory — architecturally grounded in existing infrastructure. Not scheduled for implementation.

## The Idea

RYE today calls LLM providers over HTTP — Anthropic, OpenAI — via the `http_provider` adapter in `rye/agent/threads/adapters/`. The provider is external. You rent inference.

This proposal replaces the external provider with your own hardware running tinygrad. The model loads directly into the `ryeos-node` process on GPU execution nodes, and the `llm/complete` tool calls `model.generate()` as a Python function call. On top of this, a separate completions server exposes `/v1/chat/completions` — the same interface as Anthropic or OpenAI. Agent threads call it via `http_provider` like any other provider, with no CAS sync overhead.

The interesting part is not just "run tinygrad instead of calling Anthropic". It's that the entire inference stack — from forward pass to tool dispatch to cluster routing — runs through RYE's execution primitives. Chat template formatting and tool call parsing are data-driven — config files resolved via 3-tier, processors and parsers registered in the existing routers. At cluster scale — 30 GPUs across 15 machines — the routing between GPU execution nodes is also a RYE tool. The completions server sits on top as the external-facing surface, while the execution node infrastructure handles distribution underneath.

---

## How Tool Use Works at the Protocol Level

Before getting into RYE's role, the raw mechanics of LLM tool use need to be clear. This is the protocol that any self-hosted inference endpoint must implement.

### The Multi-Turn Loop

Tool use is not the model executing tools. The model declares intent; you execute.

```
1. The caller sends a request with a `tools` array defining available tools
2. The model responds with a tool call block (instead of, or alongside, text)
3. The caller executes the tool locally and sends back a tool result
4. The model continues with the tool output in context
5. Repeat until the model returns a final response (no tool calls)
```

The model never executes tools — it only declares which tools it wants called and with what arguments. RYE's agent thread harness (`rye/agent/threads/`) already manages exactly this loop today: it calls the LLM provider, parses tool calls from the response via the `streaming_tool_parser`, dispatches them through the dynamic tool registration system, appends results, and loops until the model returns a final response. The only change in the sovereign inference model is where the LLM call goes — to a self-hosted completions server instead of Anthropic's API. The completions server runs tinygrad on GPU execution nodes underneath, but the agent thread harness doesn't know or care. The tool dispatch, message management, and loop are existing infrastructure.

### The Response Shape (OpenAI Format)

When the model decides to call a tool, the response body contains a `tool_calls` array:

```json
{
  "choices": [
    {
      "finish_reason": "tool_calls",
      "message": {
        "role": "assistant",
        "tool_calls": [
          {
            "id": "call_abc123",
            "type": "function",
            "function": {
              "name": "get_weather",
              "arguments": "{\"location\": \"Tokyo\"}"
            }
          }
        ]
      }
    }
  ]
}
```

Key details:

- `finish_reason: "tool_calls"` signals the model is waiting for tool results
- `arguments` is a JSON **string**, not an object — you must `JSON.parse()` it
- Multiple tools can be called in a single turn
- The assistant's full message (including tool call blocks) must be echoed back verbatim as the assistant turn in the next request

### Sending Tool Results Back

After executing the tool, you append the assistant message and a tool result message, then call the endpoint again:

```json
{
  "messages": [
    { "role": "user", "content": "What's the weather in Tokyo?" },
    { "role": "assistant", "tool_calls": [{ "id": "call_abc123", ... }] },
    {
      "role": "tool",
      "tool_call_id": "call_abc123",
      "content": "{\"temp\": 22, \"condition\": \"cloudy\"}"
    }
  ]
}
```

The `tool_call_id` in the result must match the `id` from the model's request. If the model called multiple tools in one turn, you send back multiple tool result messages.

### Anthropic vs OpenAI Format

RYE's `streaming_tool_parser` already handles both formats. The differences:

|                 | Anthropic                           | OpenAI                             |
| --------------- | ----------------------------------- | ---------------------------------- |
| Tool call block | `type: "tool_use"` in content array | `tool_calls[].function` on message |
| Arguments       | Object                              | JSON string (parse it)             |
| Result role     | `"user"` with `tool_result` block   | `"tool"` message                   |
| Stop reason     | `"tool_use"`                        | `"tool_calls"`                     |

These format differences are handled by the data-driven chat template and tool call parsing layer described below — not hardcoded into the inference tool.

---

## Architecture: Execution Nodes and the Completions Server

Every node in the cluster is an execution node running `ryeos-node`. The difference is what tools are available — a GPU execution node has `llm/complete` tools that call tinygrad directly, a CPU-only execution node has routing implementations that dispatch to GPU execution nodes.

On top of this sits the completions server — a separate HTTP service exposing `/v1/chat/completions`. It's not an endpoint on `ryeos-node`. It's its own standalone process that runs RYE's execution engine underneath for tool use loops. External-facing — for agent threads, users, and automations calling the model directly. It could run on the same hardware as an execution node, but it's a separate service.

Two paths coexist:

```
Provider path (agent threads → completions server):
  agent thread harness → http_provider adapter → POST /v1/chat/completions
    → completions server: format → tinygrad generate() → parse
    → returns completion (same as calling Anthropic)
  No CAS sync. No _dispatch_remote(). Fast.

Execute path (cluster-internal tool dispatch):
  ExecuteTool.handle() → _dispatch_remote() → CAS sync → /execute on target node
    → target node: PrimitiveExecutor resolves tool chain → executes
  Used for tool execution that needs workspace context.
```

The agent thread harness doesn't know it's talking to tinygrad. It calls the completions server via `http_provider` the same way it calls Anthropic — registered as a provider in `agent.yaml` with the completions server's URL. The completions server internally uses the execution node infrastructure to run forward passes and handle tool use loops.

### What Runs on a GPU Execution Node

The `ryeos-node` process on the GPU node loads the tinygrad model at startup. No separate inference server — the model lives in GPU memory as state held by the process.

tinygrad's `LLaMa` implementation (`examples/llama.py`) loads weights once via `safe_load` (memory-mapped from safetensors, realized to GPU via `.realize()`), keeps them in GPU memory for the lifetime of the process, and uses `TinyJit` to compile GPU kernels on the second forward pass and replay them from the third onward. The KV cache persists across calls as in-place tensor updates.

The `llm/complete/meta-llama/llama-3-1-8b` tool on this node calls `model.generate()` directly — a Python function call, not an HTTP request. The chain is:

```
llm/complete/meta-llama/llama-3-1-8b (tool)
  → format messages + tools via llm/format processor (reads model family config)
  → tokenize
  → model.generate() — tinygrad forward pass, JIT-compiled GPU kernels
  → detokenize
  → parse tool calls via llm/tool-calls parser (reads model family config)
  → return structured response
```

No `http_client` primitive. No subprocess. No server. The model is process state.

### What Changes in RYE Configuration

On a GPU execution node, no provider config needed — the `llm/complete` tool calls tinygrad directly. On a CPU-only execution node, the routing implementation of `llm/complete` queries `/status` on known remotes and dispatches via `_dispatch_remote()`. See [Execution Nodes](execution-nodes.md) for the routing model.

For agent threads, the self-hosted setup registers as a provider in `agent.yaml`:

```yaml
# .ai/config/agent/agent.yaml (project override)
provider:
  default: self-hosted

providers:
  self-hosted:
    url: https://completions.internal/v1/chat/completions
    key_env: COMPLETIONS_API_KEY
```

The agent thread harness calls this endpoint via `http_provider` — same adapter, same path as Anthropic. No CAS sync overhead for LLM calls.

---

## Chat Templates & Tool Call Parsing

Raw tinygrad gives you token generation. Between the messages array and the model's input tokens — and between the model's output tokens and a structured tool call response — sits a formatting and parsing layer. This layer is data-driven, following the same pattern as RYE's existing provider and processor infrastructure.

### Model family config

Each model family has a config file describing its chat format:

```yaml
# .ai/config/llm/models/meta-llama.yaml
family: meta-llama
chat_format:
  bos_token: "<|begin_of_text|>"
  eos_token: "<|eot_id|>"
  role_tokens:
    system: "<|start_header_id|>system<|end_header_id|>"
    user: "<|start_header_id|>user<|end_header_id|>"
    assistant: "<|start_header_id|>assistant<|end_header_id|>"
    tool: "<|start_header_id|>ipython<|end_header_id|>"
  tool_call_format: json
  tool_call_markers:
    start: "<|python_tag|>"
    end: "<|eom_id|>"
  tool_definition_format: |
    When you receive a tool call response, use the output to ...
```

Resolved via 3-tier config (system → user → project). Adding support for a new model family = add a config file. The core bundle ships configs for common families; projects can override or add their own.

### Formatting processor

The `llm/format` processor takes a messages array + tools definition + model family config and produces the token sequence the model expects. Registered in `ProcessorRouter` alongside existing processors like `inputs/validate` and `inputs/interpolate`.

```
messages + tools + model_config
  → llm/format processor
    → reads meta-llama.yaml for token markers and structure
    → produces formatted prompt string
      → tokenizer produces token IDs
```

### Tool call parser

The `llm/tool-calls` parser takes raw model output text and extracts structured tool calls. Registered in `ParserRouter` alongside existing parsers like `markdown/frontmatter` and `markdown/xml`.

```
raw model output text
  → llm/tool-calls parser
    → reads meta-llama.yaml for tool_call_markers
    → detects tool call boundaries
    → extracts function name + arguments JSON
    → returns structured tool_calls array
```

### What this means

Adding a new model family to the inference cluster:

1. Add a config file at `.ai/config/llm/models/{family}.yaml`
2. Deploy the model weights on a GPU node
3. Add the `llm/complete/{family}/{model}` tool to the node

The formatting and parsing logic is shared — the processors and parsers read the config to know how to handle each family. No model-specific code in the tool itself.

---

## Inference as a RYE Tool

The interesting architectural move: making the inference call itself a tool that routes through `PrimitiveExecutor`.

Agent threads call the completions server via `http_provider` — same path as Anthropic. But internally, the completions server and execution nodes treat inference as a tool like any other, resolved through the executor chain:

```
.ai/tools/
  llm/
    complete/
      meta-llama/
        llama-3-1-8b.py      ← on GPU execution node: tinygrad in-process
                              ← on CPU-only execution node: routing to capable remote
```

On a GPU execution node, the tool loads the tinygrad model into the `ryeos-node` process at startup and holds it in GPU memory. The tool's `execute()` calls `model.generate()` directly — no chain to `http_client`, no subprocess. The model is process state, and the `TinyJit` system compiles and replays GPU kernels after the second call.

On a CPU-only execution node, the same tool ID has a different implementation that queries `/status` on known remotes, matches capabilities, and dispatches via `_dispatch_remote()`.

### What This Gives You

The inference call inherits everything that tool execution provides for free:

| Capability              | How It Works                                                           |
| ----------------------- | ---------------------------------------------------------------------- |
| Chain validation        | `ChainValidator` validates the tool chain before execution             |
| Integrity verification  | `verify_item()` checks Ed25519 signatures on the tool file             |
| Lockfile caching        | `LockfileResolver` caches the resolved chain for fast repeat execution |
| Trace and observability | `ExecutionResult` captures duration, chain, metadata, trace events     |
| Data-driven formatting  | `llm/format` processor + model family config handles chat template     |
| Data-driven parsing     | `llm/tool-calls` parser + model family config extracts tool calls      |
| ENV_CONFIG resolution   | `EnvResolver` resolves runtime environment through the chain           |

Neither the agent thread nor the CPU-only execution node knows it's tinygrad. The agent thread calls the completions server via `http_provider`. The completions server calls `ExecuteTool.handle(item_type="tool", item_id="llm/complete/meta-llama/llama-3-1-8b", parameters={...})`. On a GPU execution node, the tool runs a forward pass. On a CPU-only execution node, the tool routes to one that can.

---

## The Tool Use Loop — Two Contexts

The tool use loop runs in two different contexts, each using the appropriate path:

### Agent thread context (provider path)

The agent thread harness manages the loop the same way it does with Anthropic. It calls the completions server via `http_provider`, gets a response, dispatches tool calls locally, appends results, and calls again:

```
agent thread harness
  → http_provider → POST /v1/chat/completions { messages, tools }
    → completions server returns completion
      → finish_reason="tool_calls"

  → parse tool_calls from response
  → for each tool_call:
      ExecuteTool.handle("tool", tool_call.name, tool_call.args)
        → PrimitiveExecutor.execute() → chain resolution → Lillux primitive
        → result

  → append assistant message + tool results to messages
  → http_provider → POST /v1/chat/completions { updated_messages, tools }

  → repeat until finish_reason="stop"
```

LLM calls go through the provider path (fast, no CAS overhead). Tool dispatch goes through `execute` (local or routed). The harness doesn't know the provider is self-hosted.

### Completions server context (execute path)

Inside the completions server, when a caller sends a request with tools, the server runs the tool use loop internally via `execute`:

```
POST /v1/chat/completions { messages, tools, model }
  → ExecuteTool.handle("tool", "llm/complete/meta-llama/llama-3-1-8b", { messages, tools })
    → format → tinygrad generate() → parse → structured response
      → finish_reason="tool_calls"

  → for each tool_call:
      ExecuteTool.handle("tool", tool_call.name, tool_call.args)
        → PrimitiveExecutor.execute() → chain resolution → Lillux primitive
        → result

  → append results → call llm/complete again
  → repeat until finish_reason="stop"
  → return standard chat completions response
```

The caller sends one request, gets one response. The completions server ran N inference calls and M tool dispatches internally. Every internal step is `execute`.

---

## Cluster Routing via Execute

With 30 GPUs across 15 machines, `llm/complete` needs to route to the right GPU execution node. This is covered in detail in [Execution Nodes](execution-nodes.md) — the key insight is that routing is not a separate system but a different implementation of the same tool.

### Same tool ID, different implementations per node

On a GPU execution node, `llm/complete/meta-llama/llama-3-1-8b` calls `model.generate()` directly via tinygrad in-process. No HTTP, no server.

On a CPU-only execution node, `llm/complete/meta-llama/llama-3-1-8b` is a different implementation — one that queries `/status` on known remotes, matches capabilities via fnmatch, picks the least loaded node, and dispatches via `_dispatch_remote()`.

Same tool ID. Different behavior. Standard space resolution. No fallback logic in the executor — the routing is intentional, authored into the tool.

### Node capabilities are dynamic

Execution nodes report what they can execute via a `/status` endpoint — a lightweight HTTP wrapper around a local `node/status` tool that introspects the workspace. Capabilities are derived from what tools are actually in the node's `.ai/tools/`, expressed as the same fnmatch strings used for thread permissions (`rye.execute.tool.llm.complete.meta-llama.llama-3-1-8b`).

`remote.yaml` stays minimal — just URLs and auth. Capabilities come from the nodes themselves. See [Execution Nodes](execution-nodes.md) for the full model.

### The request flow

```
POST /execute { directive: "add-show", input: { username: "mrbeast" } }
  → execution node
      ExecuteTool.handle("directive", "add-show", ...)
        → _run_directive() → fork thread
          → agent loop begins
            → needs inference
              → http_provider → POST /v1/chat/completions
                → completions server routes internally:
                  → ExecuteTool.handle("tool", "llm/complete/meta-llama/llama-3-1-8b", ...)
                    → routing: query /status on remotes
                      → fnmatch against capabilities → nodes 3-15 match
                        → node-9 least loaded
                          → _dispatch_remote(target="remote:node-9")
                            → node-9: llm/complete runs locally
                              → format → tinygrad generate() → parse
                                → returns structured completion
            → tool_calls in response?
              → ExecuteTool.handle("tool", tool_call.name, tool_call.args)
                → dispatches locally or routes again
            → loop until done
```

The agent thread calls the completions server via the provider path. The completions server routes internally via `execute`. The cluster is just more tools.

---

## The Completions Server

The completions server is a separate HTTP service from `ryeos-node` — its own standalone process exposing `/v1/chat/completions`. It's not an endpoint on `ryeos-node`. It could run on the same hardware as an execution node, but it's a separate service.

From the outside, it looks like any other LLM endpoint — Anthropic, OpenAI, or any OpenAI-compatible provider. Internally, every request runs through RYE's execution engine:

```
POST /v1/chat/completions { messages, tools, model }
  → completions server
      ExecuteTool.handle("tool", "llm/complete/meta-llama/llama-3-1-8b", { messages, tools })
        → routes to GPU execution node (or runs locally if this node has a GPU)
          → format → tinygrad generate() → parse
      tool_calls in response?
        → ExecuteTool.handle("tool", tool_call.name, tool_call.args)
        → append results
        → ExecuteTool.handle("tool", "llm/complete/meta-llama/llama-3-1-8b", { updated_messages })
      loop until done
  → return standard chat completions response
```

The caller sends one request, gets one response. The completions server ran N inference calls and M tool dispatches in between. Transparent to the caller.

This is the surface that agent threads call via `http_provider` — registered as a provider in `agent.yaml` with the server's URL. The harness calls it the same way it calls Anthropic. No CAS sync overhead, no `_dispatch_remote()` on the agent side.

The completions server is not directive-driven. It receives standard chat completions requests and runs them through the execution engine for tool dispatch. No directives, no threads, no agent — just the mechanical part of running the tool use loop through `execute`.

---

## What This Gives You

**Uniformity.** Every operation — inference, tool execution, cluster routing — is addressed through `ExecuteTool.handle()`. You don't have a different mental model for "calling the LLM" vs "running a browser tool" vs "scheduling work across machines". It's all `execute` resolving a chain to a Lillux primitive.

**Observability for free.** Because everything goes through `PrimitiveExecutor`, you get `ExecutionResult` with duration, chain trace, and metadata for every operation. You don't instrument individual tools — the executor instruments `execute` once.

**Swappability.** `llm/complete` is just a tool ID. Swap tinygrad for a different backend by changing the tool implementation. The execution loop doesn't change. Add a new model family by adding a config file — the formatting and parsing are data-driven.

**Fault handling in one place.** Node down, GPU OOM, tool timeout — all surface as failed `ExecutionResult`s from `PrimitiveExecutor`. The caller sees a failed execute and the chain trace shows exactly where it broke.

**The cluster is just more tools.** The same execution model that runs a single directive scales to coordinating 15 machines — because routing and placement are tools resolved through `execute` like everything else. You scale the substrate, not the framework on top.

---

## The Logical Conclusion

An OS abstracts hardware. You don't write to a disk — you call a filesystem API. You don't talk to a NIC — you open a socket. The hardware is invisible behind a uniform interface.

RYE does the same thing, but the hardware is compute clusters, models, and tools. You don't call a GPU, you don't manage a process, you don't route a network request. You call `execute`. Everything else is invisible.

**The model is a device driver.** Tinygrad, Anthropic API, a fine-tuned specialist model — these are drivers. `llm/complete` is the syscall. You swap drivers without changing anything above the tool interface. The execution environment doesn't know or care what's serving inference. And this isn't just philosophical — it's mechanical. `agent.yaml` says `provider: self-hosted` instead of `provider: anthropic` and the entire system keeps running. The "borrowed intelligence" from the [manifesto](../manifesto.md) isn't just borrowed from a lab anymore. It's borrowed from your own GPUs, and the swap is a one-line config change. The manifesto's thesis — the model is swappable, the agent is the key — is proven at the infrastructure level.

**The LLM runtime is data, like every other runtime.** The Python runtime is a YAML file that describes how to invoke the interpreter. The model family config at `.ai/config/llm/models/meta-llama.yaml` is the same pattern — it describes how to format the prompt, what tokens to use, how to parse tool calls. Adding a new model family is adding a config file, not writing code. The manifesto says "everything is data." Sovereign inference extends that to inference itself. The forward pass is described, configured, and executed through the same data-driven pipeline as a bash script or a Python tool.

**The cluster is the kernel.** Lillux manages primitives on a single machine. At cluster scale, the same abstraction extends — `execute` routes work to wherever the resources are. Node 7 or node 12 is like a CPU core. The routing tool picks it; you never address it directly.

**Any program becomes an agent.** Any system that can POST to `/v1/chat/completions` is running inside RYE's execution environment without knowing it. It thinks it's talking to an LLM. It's talking to a completions server backed by an OS that happens to speak OpenAI. Tools, routing, observability — all available, all transparent.

**The network is the machine.** 15 machines, 30 GPUs, unified `execute` surface behind the completions server. There's no meaningful distinction between local and remote execution — it's all just `execute` resolving to wherever the resource lives. The physical topology is an implementation detail managed by `cas/remote.yaml` and node-reported capabilities.

### No control plane

Traditional infrastructure has a hard split: the control plane (Kubernetes, Terraform, Nomad) manages the cluster, the data plane runs the workload. Different systems, different APIs, different operators. A human configures the control plane. The workload runs on the data plane. They never mix.

In RYE, there's no split. An agent running a directive and an agent managing the cluster are doing the same thing — calling `execute`. An agent that hits the capacity limit during a long-running job can execute a `cluster/provision` directive that spins up a new GPU execution node, adds it to `remote.yaml`, waits for `/status` to report healthy, then continues its work with more inference capacity. When it's done, it executes `cluster/deprovision`. No human in the loop. No separate auto-scaler. The boundary between "doing work" and "managing the infrastructure the work runs on" doesn't exist — it's `execute` all the way down.

Kubernetes can auto-scale, but the scaling policy is configured by a human and runs as a separate controller that doesn't know anything about the workload's actual needs. In RYE, the agent doing the work IS the operator. It knows it's about to process 50 shows, it knows each show needs 3-5 LLM calls, it can calculate that it needs more capacity before it hits the wall. And the operational procedures — how to provision a node, how to recover from a failure, how to rebalance load — those are directives. They live in `.ai/directives/` alongside the application directives. Runbooks become executable. Incident response becomes a directive an agent follows.

### AI as operator, workload, and client

Most infrastructure was designed with humans as the operator and AI as the workload. RYE inverts this. AI is the operator, the workload, and the client — simultaneously, through the same interface.

The agent thread runs a directive (AI as workload). It calls the completions server for reasoning (AI as client of the inference cluster). The completions server routes internally across GPU execution nodes (infrastructure managed by tools an AI can execute). An AI agent can provision, monitor, and decommission the nodes its own inference runs on — because those operations are just more tool executions.

The manifesto says: "The OS persists. Processes are transient. The kernel doesn't care what runs through it." With sovereign inference, the kernel is distributed across 15 machines, and it still doesn't care what runs through it. The agent doesn't address machines. It calls tools. The cluster topology is invisible. And the agent can reshape that topology at runtime — not by escaping to a control plane, but by executing more tools through the same `execute` primitive it uses for everything else.

This is what "OS for AI" actually means. Not "an OS that's friendly to AI." An OS where AI operates the infrastructure, runs the workload, and consumes the results — all through `execute`, all signed, all verified, all observable.

This extends the principle from the [Residual Stream](Residual%20stream%20and%20native%20model%20family.md) doc: "the substrate and the models together." The completions server is the syscall interface. The execution nodes are the kernel. The `llm/complete` tools are device drivers. And the programs are agents that don't know any of this exists — they just call a provider endpoint and get intelligence back.

---

## What's New vs What Exists

### Proposed New Tools

| Tool                            | Proposed Location         | Purpose                                                                                                          |
| ------------------------------- | ------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `llm/complete/{family}/{model}` | `.ai/tools/llm/complete/` | Inference call as a tool — on GPU nodes calls tinygrad in-process, on execution nodes routes to a capable remote |

### Proposed New Processors & Parsers

| Item             | Type      | Purpose                                                                                           |
| ---------------- | --------- | ------------------------------------------------------------------------------------------------- |
| `llm/format`     | Processor | Format messages + tools into model-family-specific token sequence, reads from model family config |
| `llm/tool-calls` | Parser    | Extract structured tool calls from raw model output, reads from model family config               |

### Proposed New Config

| Config               | Proposed Location                     | Purpose                                                                                       |
| -------------------- | ------------------------------------- | --------------------------------------------------------------------------------------------- |
| Model family configs | `.ai/config/llm/models/{family}.yaml` | Chat format specification — special tokens, role markers, tool call markers. 3-tier resolved. |

Cluster routing config (`cluster/topology.yaml`) and node capability reporting are covered in [Execution Nodes](execution-nodes.md).

### What's Unchanged

| Component                                              | Status                                                                            |
| ------------------------------------------------------ | --------------------------------------------------------------------------------- |
| `ExecuteTool.handle()` — unified entry point           | Unchanged — inference tools are new tool IDs, same dispatch path                  |
| `PrimitiveExecutor.execute()` — chain resolution       | Unchanged — inference tools resolve through the same chain system                 |
| `ProcessorRouter`                                      | Unchanged — `llm/format` registers alongside existing processors                  |
| `ParserRouter`                                         | Unchanged — `llm/tool-calls` registers alongside existing parsers                 |
| `_dispatch_remote()` — remote execution                | Unchanged — routing implementations use existing `remote:<name>` targeting        |
| `cas/remote.yaml` — named remotes                      | Unchanged — nodes are named remotes with the same config shape                    |
| `ChainValidator`, `LockfileResolver` — chain integrity | Unchanged — inference tool chains validated and lockfiled like any other          |
| Ed25519 signing                                        | Unchanged — inference tool files are signed items                                 |
| 3-tier space resolution (project → user → system)      | Unchanged — same tool ID, different implementations per node via space resolution |
| fnmatch pattern matching                               | Unchanged — same matching used for node capability routing                        |

---

## Open Design Questions

### Tool Use Loop Ownership — Resolved

Both options coexist, serving different contexts:

- **Agent thread context:** The agent thread harness owns the loop (status quo). It calls the completions server via `http_provider` — same path as Anthropic. The thread manages tool dispatch locally. The completions server is just another provider endpoint.
- **Completions server context:** The completions server owns the loop internally. When an external caller sends a request with tools, the server runs the full tool use cycle via repeated `execute` calls and returns the final response. This is for external callers who want a standard OpenAI-compatible interface with transparent tool execution.

### Streaming

tinygrad's generate loop produces tokens one at a time. Streaming to the caller (e.g. for the OpenAI-compatible surface) would require yielding tokens as they're generated rather than waiting for the full completion. The `llm/complete` tool could support a streaming mode that emits partial results — but the tool call detection and parsing can only happen once the full output is available (tool call markers need to be complete to parse). Streaming text output and detecting tool call boundaries are in tension.

### Model Loading Lifecycle

The tinygrad model loads into GPU memory when `ryeos-node` starts and stays resident. Questions: should the model load lazily on first request? Should there be a mechanism to unload/swap models without restarting the process? For a single-model node this doesn't matter much, but a node serving multiple models would need model lifecycle management.

### Model Parallelism vs Replica Pool

With 30 GPUs:

- **Model parallelism**: Large model (70B+) sharded across multiple GPUs via tinygrad's `.shard_(device, axis=...)`. Single inference process spans N GPUs. Higher quality, lower throughput.
- **Replica pool**: Small model (8B–13B) replicated across all GPUs. 30 independent `ryeos-node` processes each holding a model. Routing tool load-balances. Higher throughput, lower latency per request.
- **Tiered**: 4 GPUs → large reasoning model (70B) for hard tasks. 26 GPUs → fast small model (8B) for bulk extraction. The agent selects the model by calling the appropriate `llm/complete/{family}/{model}` tool ID. The routing tool finds a node that can execute it.

---

## Things to Watch For

Distributed systems problems that will surface as this moves from design to implementation.

### Partial failure mid-execution

A GPU execution node accepts a request, starts a forward pass, then OOMs or crashes halfway through. The `_dispatch_remote()` call already happened. The routing saw the node as healthy. Now you need: timeout semantics (how long before the caller gives up), retry logic (is it safe to retry — inference calls are stateless, so yes), and error propagation (the `ExecutionResult` failure needs to surface clearly through the completions server back to the agent thread). The uniform `execute` model helps here — failure handling is in one place, not per-subsystem — but the implementation still needs to handle every edge case.

### Completions server availability

Every agent thread LLM call routes through the completions server. If it goes down, all agent threads stall. For production use, the completions server needs to be stateless and horizontally scalable — multiple replicas behind a load balancer. Since it holds no state (it just runs the execution engine per-request), this should be straightforward, but it needs to be designed for from the start.

### Thundering herd on routing

Two concurrent requests both query `/status`, both see node-9 as least loaded, both dispatch there. Now node-9 is over-provisioned while other nodes sit idle. This is the classic stale-cache routing problem. TTL-based `/status` caching makes it worse under high concurrency. Mitigations: add jitter to node selection among equally-loaded candidates, use shorter TTLs under high load, or move to push-based status updates where nodes stream state changes as they happen (eliminates the polling round-trip entirely).

### GPU execution node CAS sync is a non-issue

Worth noting: the CAS sync overhead on the execute path to GPU execution nodes is negligible in practice. GPU execution nodes are pre-provisioned with their `llm/complete` tools, model family configs, and tinygrad — all installed locally in `.ai/`. The `has/put` exchange on each request just confirms "I already have everything" and moves on. The actual inference payload (messages + tools) travels in the `/execute` request body, not through CAS. CAS sync only gets expensive when deploying new tools or configs to a node — that's the setup step, not the hot path.
