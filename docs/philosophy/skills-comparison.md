```yaml
id: skills-comparison
title: "Why RyeOS Is Ahead of Agent Skills"
description: A direct comparison between Anthropic's Skills system and RyeOS's directive/tool/knowledge model, and why the architectural bets made in RyeOS compound over time
category: philosophy
tags: [philosophy, skills, comparison, architecture, design]
version: "1.0.0"
```

# Why RyeOS Doesn't Use Skills

Anthropic launched Agent Skills in 2025 to broad adoption. Box, Rakuten, Canva and others are using them. The industry followed. It is worth being precise about what Skills actually are, what they solve, and why RyeOS is operating on a different level entirely.

## What Skills Actually Are

A skill is a directory with a `SKILL.md` file. The file has YAML frontmatter (name, description, some flags) and markdown instructions below it. That is the entire system.

The mechanics:

- Skill descriptions load at startup so Claude knows what is available
- Full content loads when the skill is invoked (manually via `/skill-name` or automatically when Claude matches the description)
- `!`cmd`` syntax executes shell commands before Claude sees the prompt, injecting their output
- `context: fork` spins a subagent with the skill content as its prompt
- `allowed-tools` restricts which tools the skill can call without approval

Skills are **prompt injection with a directory convention and some YAML**. There is no integrity, no typed I/O, no namespacing beyond filesystem paths, no search, no inheritance, no execution chain. Anyone who can write to your `.claude/skills/` directory owns your agent's behavior without any record of it.

## The Comparison

| Capability                   | Skills                         | RyeOS                                                                           |
| ---------------------------- | ------------------------------ | ------------------------------------------------------------------------------- |
| Integrity / tamper detection | None — just files              | Ed25519 signing on every item, verified on every execute/load                   |
| Structured I/O               | None — markdown instructions   | Typed `<inputs>/<outputs>`, model tier selection, resource limits per directive |
| Namespacing                  | Filesystem paths               | 3-tier (system/user/project) with explicit resolution order                     |
| Search / discovery           | Description-matching heuristic | `rye_search()` across all spaces with scope and namespace filters               |
| Knowledge as a type          | Not distinct from instructions | First-class item type, separate from directives, searchable, composable         |
| Tool execution               | `!`cmd`` shell injection       | Full runtime chain: tool → runtime YAML → primitive → Lillux                    |
| Inheritance                  | Not possible                   | `extends` on directives                                                         |
| Permissions                  | `allowed-tools` coarse list    | `<permissions>` with execute/search/load/sign scopes per directive              |
| Thread model                 | Forked subagent, no provenance | Capability tokens, signed thread metadata, budget propagation                   |
| Portability of identity      | None — files have no identity  | Signatures travel with data, not infrastructure                                 |
| Authorship provenance        | None                           | `\|registry@username` layers into the chain                                     |
| Config resolution            | None                           | 3-tier deep merge or first-match, injected into tool params                     |
| Runtime isolation            | None                           | Interpreter resolution, env setup, anchor system, dep verification              |
| Chain validation             | None                           | Space compatibility, I/O compatibility, semver constraints                      |

## The Architectural Difference

Skills are a context injection convenience. RyeOS is a trust and execution substrate.

The distinction that does not exist anywhere else in a shipped system is this: **identity lives in the data, not in the compute**. Skills are files. If a file changes, Claude runs the changed instructions with no indication anything happened. In RyeOS, every tool, directive, and knowledge entry is signed with an Ed25519 key. `verify_item()` runs on every `execute` and `load` call. Tampered items raise `IntegrityError`. No fallback, no bypass.

The consequence is portability that compounds over time. Swap Claude for Gemini. Move from a local machine to a serverless function. The compute is ephemeral. The signatures on your tools and directives trace back to their authors regardless of where they run or which model executes them. Skills cannot claim this because they have no identity to carry.

The second distinction is the **execution chain**. When a Skills directive runs `!`gh pr diff``, it is a shell command with no oversight. When a RyeOS tool runs, it goes through a chain: tool declares `**executor_id**` pointing to a runtime, runtime points to a primitive, primitive maps to a Lillux class. Each link in the chain is integrity-verified before the chain is walked. The space compatibility rules are enforced (a system tool cannot delegate to a project tool). Semver constraints between chain elements are checked. Config files are resolved across all 3 tiers and merged before the subprocess spawns.

The third distinction is the **three-tier space system**. System items ship with the platform. User items live in `~/.ai/`. Project items live in `.ai/`. Resolution is explicit: project shadows user, user shadows system. Skills have no equivalent — there is no principled resolution order, no way to audit what is in effect, no way to establish which version of a skill an agent actually ran.

## What Skills Optimized For

Skills succeeded at distribution. The bar to write one is genuinely low: write markdown, name a directory, done. That is why enterprise teams adopted it quickly — the authoring cost is near zero.

This is a real tradeoff. RyeOS's model is more powerful across every axis that matters for a system you want to trust and build on, but authoring a directive has higher ceremony than dropping a markdown file.

The question of whether that tradeoff matters depends entirely on what you are building. If you want a quick automation that runs `gh pr diff` and summarizes it, a skill is fine. If you want an agent substrate where the behavior is auditable, tamper-evident, composable across teams, and portable across infrastructure — Skills does not even address the same problem.

## Pre-Execution Context Assembly

Skills have one mechanism worth examining on its own terms: the `!`cmd`` syntax. When a skill file includes `!`gh pr diff``, that shell output is inserted into the prompt before the agent's first turn. Zero turns consumed on context gathering.

RyeOS already does this, and the mechanism is strictly better.

The layered context injection system has three independent layers that compose before the agent loop starts:

**Layer 1 — Tool schema preload.** After capability resolution but before turn 1, every tool the directive is permitted to call has its schema injected into the before-context. The agent starts knowing the exact parameter shapes for every tool it can use.

**Layer 2 — Context hooks.** Hooks on the `thread_started` event can execute any RyeOS item — tool, knowledge, directive — and inject the result at `before` or `after` positions relative to the directive body. The hook context includes the directive's `inputs`, so hook tool calls can be parametrized with what was passed to the directive (`${inputs.pr_number}`, etc.). Directives can declare their own hooks directly in their `<hooks>` block. A review directive that needs the current diff before reasoning starts declares a `thread_started` hook that runs `rye/bash` and injects the output — before the agent loop, before turn 1, with full integrity verification and permission checking.

**Layer 3 — Extends chain context.** The `<context><system>`, `<context><before>`, and `<context><after>` positions load named knowledge or directive items into the system prompt and first message assembly.

All three layers resolve and compose before the agent processes its first turn. Compare this to Skills' `!`cmd``: that is an unverified shell command with no oversight, no record in any transcript, no permission check, limited to what you can express as a shell one-liner. RyeOS context hooks can run any tool in the runtime chain — web fetches, LSP queries, database reads, structured tool outputs — with the same signed execution, integrity verification, and transcript logging as any other tool call.

The authoring surface for this is hook declarations rather than inline `!`backtick`` syntax, which is more verbose. That is a surface tradeoff, not a capability gap. See [layered context injection design](../../.tmp/layered-context-injection-design.md) for the full design.
