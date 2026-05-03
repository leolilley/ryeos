```yaml
id: natural-language-cli
title: "Natural Language CLI — Hybrid Verb Dispatch with NL Fallthrough"
description: Extend the Rye CLI with a natural language catch-all path. Deterministic verb matching (execute, fetch, sign) hits the existing dispatch table first. Everything else routes to the configured model as a substrate-native signed execution — not a bolted-on translation layer.
category: future
tags: [cli, natural-language, dispatch, model-as-tool, signing]
version: "0.1.0"
status: exploratory
```

# Natural Language CLI

> **Status:** Exploratory. Builds on existing CLI primitives, not scheduled for implementation.

## The Idea

The CLI gains a natural language fallthrough. The three primitives — `execute`, `fetch`, `sign` — hit the existing dispatch table directly, zero ambiguity, no model involved. Anything that doesn't match a known verb routes to the configured model endpoint as a natural language prompt. The response streams back to terminal.

```
rye execute my-directive        → deterministic dispatch, existing path
rye fetch some-item             → deterministic dispatch, existing path
rye sign tool:*                 → deterministic dispatch, existing path
rye build me a deploy pipeline  → fallthrough → model execution
rye what's the state of remotes → fallthrough → model execution
rye whatever                   → fallthrough → model execution
```

The verb table is the execution surface. Natural language doesn't bypass it — it enters through it. The model is just another tool in the CAS, signed like everything else.

---

## Dispatch Architecture

```
CLI input
    │
    ▼
Known verb? ──yes──→ Existing dispatch table (execute / fetch / sign)
    │                       │
    no                      └─ signed execution, CAS-stored trace
    │
    ▼
NL fallthrough
    │
    ▼
Resolve model from current identity (attestation.json → model capability)
    │
    ▼
Execute via rye execute (model tool, stdin as prompt, stdout as response)
    │
    ▔─ signed execution, CAS-stored trace, full episode history
```

Two codepaths. Same substrate. The NL path is not special-cased — it's just `execute` with the model tool. Which means it's signed, traceable, CAS-stored. You get a full episode trace of every NL interaction for free.

---

## The Model Is the Tool

This is the structural difference from every existing NL CLI.

The NL response doesn't come from a side channel. It's an execution. The model invocation goes through the same primitive — `rye execute` — with the same signing, the same CAS storage, the same traceability.

```
rye "explain the deployment graph"

resolves to:
  execute(item="model", params={prompt: "explain the deployment graph"})
  → signed request
  → CAS-stored transcript
  → streamed response to terminal
```

The model doesn't suggest a shell command for you to run. The model *runs*. It's a first-class tool executor, same as anything else in the three-tier space system.

---

## Identity-Scoped Configuration

The model endpoint isn't configured in a dotfile. It's declared in the current keypair's `attestation.json` as a capability.

Different keys, different models. Same CLI, different substrate.

```yaml
# attestation.json
capabilities:
  model:
    endpoint: "claude-sonnet"
    provider: "anthropic"
```

Switch identities, switch models. No config files to edit, no environment variables to set. The attestation *is* the config. This is already how RYE works for everything else — the NL path just uses the same mechanism.

---

## NL Responses Can Emit Directives

The model's response doesn't have to be prose. It could return:

- A signed directive — a runnable artifact stored in the CAS
- A tool chain — an execution graph for the CLI to run
- Prose — the default, for when you just want an answer

```
rye "build me a directive that fetches X and executes Y"

resolves to:
  execute(model, {prompt: "build me a directive that fetches X and executes Y"})
  → model generates directive content
  → directive signed and stored in CAS
  → path returned to terminal
```

This is the natural language → deterministic substrate bridge. The model produces a signed artifact, not just text. The artifact is immediately runnable through the same verb table.

---

## Verb Matching Rules

Deterministic verbs use **exact prefix matching**. No fuzzy matching, ever.

```
rye execute     → hits execute
rye fetch       → hits fetch
rye sign        → hits sign
rye exec        → fallthrough (not "execute")
rye fetching    → fallthrough (not "fetch")
rye sign-off    → fallthrough (not "sign")
```

This avoids the ambiguity problem entirely. If the first token is an exact match for a known verb, it's a primitive call. Everything else is NL. There is no middle ground.

---

## Prior Art and Structural Differences

Plenty of NL CLIs exist. None have this architecture.

| System                 | Shape                                          | Gap                                                                                             |
| ---------------------- | ---------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| **ai-shell**           | NL → shell command suggestion                  | Translation layer, not execution. No signing, no traceability. Model is a helper, not a tool.  |
| **Warp / JetBrains AI** | NL → shell command with terminal context       | Same. Model suggests, human confirms, shell executes. No provenance chain.                     |
| **Kiro CLI**           | Separate `translate` and `chat` modes          | Two distinct paths, not unified dispatch. Full agentic IDE-in-terminal, not a thin shell.      |
| **nl-sh**              | NL → shell command sequence, verify then run   | Closest hybrid shape, but generic and unsigned. No substrate primitives beneath the NL layer.   |
| **Fig / Amazon Q CLI**  | Declarative completion specs + NL translation  | Had the schema for verb dispatch, used it for autocomplete not execution. NL was a side channel |

What none of them have:

- **Deterministic verb table with NL fallthrough** — primitives intercept first, NL is the catch-all, not a separate mode
- **Signed NL executions** — the model invocation produces a CAS-stored, cryptographically attested trace
- **Model as tool executor** — the model runs through `execute`, not as a bolted-on translation layer
- **Identity-scoped model config** — which model answers depends on which key you're running as

The Fig lineage is instructive: they had declarative completion specs describing subcommands, flags, and args — structurally similar to RYE's verb table — but used them for autocomplete hints rather than execution dispatch. The NL came in as a completely separate codepath (Codex translation) rather than a fallthrough through the same dispatch. Fig started as a completion tool and ended as an AI company. The verb table needs to stay the identity of the CLI, not become a legacy fast-path buried under a chat interface.

---

## Design Decisions

### Auto-Execute vs Prompt

When the NL handler returns a directive or shell command, does the CLI auto-execute or prompt for confirmation?

- **Auto-execute** makes `rye whatever` a true agentic loop but is risky without sandboxing
- **Prompt-every-time** is safer but kills the "thin shell" feel

Likely answer: trust tiers. Signed directives from your own attestation run immediately. Unsigned or third-party content prompts. This aligns with RYE's existing trust model — signature verification is the gate, not human confirmation.

### Streaming

The NL response should stream to terminal in real-time, same as any model interaction. The execution trace is written to CAS after completion. You see the response as it generates, not after.

### Context

Each NL invocation is a fresh execution by default — no conversation history carried between calls. The model has access to the project context (three-tier space resolution) because it's running through `execute`, but it doesn't accumulate state across invocations.

Persistent conversation context could be added later via thread resumption — the same mechanism used for directive thread continuation. But it's not required for v1.

---

## What It's Not

**Not a chat interface.** There's no conversation loop, no history, no `/clear` command. Each invocation is a single execution. If you want conversation, use an existing entry point — Amp, Claude, the web UI. The CLI is for single-shot interactions.

**Not a shell replacement.** It doesn't wrap `ls`, `cd`, `grep`. Deterministic verbs are RYE primitives, not POSIX commands. NL input goes to the model, not to bash.

**Not a new system.** The CLI gains a fallthrough path. The three primitives are untouched. The model invocation uses `execute`. Nothing new is built except the routing logic.

---

## Implementation Path

### Phase 1: Fallthrough Routing

Add NL routing to the CLI dispatch. First token checked against known verbs (`execute`, `fetch`, `sign`). Match → existing path. No match → resolve model from attestation, invoke via `execute`, stream response.

### Phase 2: Artifact Emission

Model responses that produce directives or tool chains are signed and stored in CAS. CLI detects artifact responses and offers the path to the stored object. Optional auto-execute for trusted sources.

### Phase 3: Trust-Tiered Execution

Signed artifacts from your own attestation auto-execute. Unsigned content prompts. Third-party signed content prompts until explicitly trusted. Extends the existing signature verification chain.

### Phase 4: Context Carrying

Optional thread resumption for multi-turn NL sessions. `rye --continue` picks up the last NL execution's transcript as context. Uses the same continuation mechanism as directive thread handoff.

---

## Relationship to Existing Infrastructure

| Existing Component     | What It Does Today                          | How NL CLI Uses It                                          |
| ---------------------- | ------------------------------------------- | ----------------------------------------------------------- |
| CLI verb dispatch      | Routes `execute`, `fetch`, `sign`           | Unchanged. NL is routing around it, not through it.        |
| `execute` primitive    | Runs directives and tools                   | NL invocation uses it to call the model                     |
| CAS                    | Stores execution traces and artifacts        | NL traces stored here, same as any execution                |
| Signing (Ed25519)      | Attests item provenance                     | NL execution trace is signed, same as any execution         |
| `attestation.json`     | Declares identity and capabilities          | Model endpoint resolved from capabilities                   |
| Thread continuation    | Resumes directive threads across boundaries | Future: carries NL context across invocations               |
| Three-tier spaces      | Resolves items from project → user → system | Model has project context via `execute`'s existing resolution |
