I built a cryptographically signed AI operating system that inverts how agents work.

Before I get into this, a quick note. I started building this before OpenClaw blew up. I give it credit for getting people excited about agentic AI, genuinely. But I think if we keep building on the architecture it lays its foundations on, we're collectively heading toward a mistake. There is a more secure, more principled way to build agents. RYE OS is my attempt at showing what that actually looks like. I'm not a researcher. I don't work for a lab. I'm just a developer who wanted a better MCP for my own workflows and kept pulling the thread until the full picture came clear.

---

**What is RYE OS?**

RYE OS is a cryptographically signed, graph-executable AI operating system — data-driven, capability-scoped, and registry-backed through content-addressed execution. It lives inside your projects, runs through a recursive MCP, and executes remotely or locally on infrastructure you control. It goes as deep as you dare.

Yes that's a lot of jargon but it's truly the TL;DR of what this is. There is a lot to absorb here but keep reading and I can show you how it all comes together.

---

**Action primitives**

After working with AI over these past few years I've come to realise that to really turn an LLM into an agent, you first need to understand the core primitives of agentic operation.

I reason you can reduce a secure agent to 4 action primitives: Search, Load, Execute, and Sign. That's it. Across any harness, any workflow, any prompt, this is all you're actually getting the LLM to do. When your harness comes prebuilt with filesystem or web search tools, you've pre-configured the Search and Load steps. When Claude Code finds a skill and runs it, that's Search, Load, Execute. When OpenClaw routes a task to a sub-agent, same thing. Every agent framework you've seen is doing some version of this, they just haven't named it.

Other frameworks have approached agent security as a runtime problem. Watch what the agent does and intervene when it steps out of bounds. In RYE permissions aren't enforced after the fact. What people have missed is the Sign step. That's what makes the other 3 operations secure. They're declared, signed, and verified before execution ever starts. But to understand what gets signed, and why it changes everything, you need to understand what RYE actually works with."

---

**Item primitives**

I reason that the primitives of what an LLM actually acts on can be reduced to 3 item types. Directives, the HOW. Knowledge, the KNOW. And tools, the DO.

I started building this before the community collectively decided that skills were the way to go. The reason I haven't adopted that framing is simple. Skills describe what to do. They don't describe what an agent is allowed to do, or how far it can go.

Directives act as just that, direction. They reflect your intent inflicted on the LLM to adhere to. Steps to follow, how to respond, what tools to call, what to evaluate. On the surface this feels like what a skill is, but directives go deeper. They also enforce the bounds of the LLM's limits, tokens, turns, cost. Its permissions, what it can search, load, execute or sign. Its context, system prompt, context injection, hooks. That's not just instruction. That's scope.

Knowledge is persistent context without intent. Best practices, something discovered during execution, transcripts, domain references. Anything worth remembering that isn't an instruction and isn't executable. But it's not just loose files in your project. Knowledge items are first-class, searchable and loadable on demand.

Then tools. You're already familiar with these. But in RYE they go further. A tool isn't just an executable to call, it also describes its own execution as data. What interpreter runs it, what environment it needs, what parameters it accepts, what schema validates its input. This is how RYE supports every language natively. Python, JS, Bash, etc, all supported through a toolchain that describes its own execution. The execution is defined in the item itself, not hardcoded somewhere in the framework.

All three item types live in a workspace inside your project. RYE organises items across three spaces, system, user, and project. The system level is the immutable package install level, core runtimes, system knowledge docs, and the directives that instruct how tools, knowledge, and other directives are created. The user space is your personal configuration accessible from any project, your trusted keys, response style preferences, your git workflow tools, your preferred web search setup, things you've built once and want everywhere. The project space is specific to that codebase, the directives that define how this agent behaves, the knowledge items scoped to this domain, the tools this project needs. Items resolve in that order, system first, then user, then project, so defaults live higher up and projects override them as needed.

Your personal tools follow you into every project. Two agents working in the same codebase share the same ground truth.

In Linux, everything is a file. In RYE OS, everything is data.

And that's the insight that closes the loop on the Sign step. Because all three item types are data, all three can be hashed. And because they can be hashed, they can be signed and cryptographically verified.

When you set up RYE, it generates you an Ed25519 signing key. This is your identity in the system. Not an account, not a login, a cryptographic key that only you hold.

When you create any of the 3 items it can only be acted on if it is signed by a key you trust. And it can only be signed if it passes schema validation. The shape of the item is verified before the signature is ever applied.

With that, all your directives carry your signature. Your tools carry your signature. Your knowledge carries your signature. RYE only acts on items signed by a key you trust. Hash validation, schema validation, signature verification, at every step. And because a directive defines not just what to do but what the LLM is permitted to do, how far it can go, and what it can spend, a signed directive isn't just an instruction. It's a contract.

---

**Tool primitives**

A tool in RYE doesn't just say "run this file." It describes the full execution as data. What primitive it derives to, what runtime handles it, what environment it needs, what parameters it accepts. And every tool, regardless of what it does, derives to one of two execution primitives. Subprocess for anything with logic, parsing, or state. HTTP for tools that are just a request, a stateless API call with no interpreter in between. The Python runtime is a YAML file that describes how to invoke the interpreter, what flags to pass, how to resolve dependencies. Adding a new language isn't a code change. It's a YAML file.

Lillux manages execution as a Rust subprocess boundary. Tampered or unsigned items are rejected before they ever reach it. Secrets are resolved at this level, below RYE entirely, so the LLM and item layer never see them. This isn't OS-level sandboxing, verified code runs with the permissions of the subprocess user. The security guarantee is supply chain security, not runtime confinement. You know exactly what ran, who signed it, and that it wasn't altered. No fallback. No bypass.

---

**MCP, harness, and recursive execution**

Everything described so far, the four actions, the three item types, signing, Lillux, all of it is accessible through MCP. Four primitive actions as MCP tools. Search, Load, Execute, Sign. You point your existing harness at RYE MCP and you're using it. Claude Code, Cursor, Codex, whatever you're already running. That's the entry point.

Through MCP, RYE enforces hash validation, schema validation, and signature verification on every item. That's real security you get immediately. But there's a ceiling when running RYE through MCP in your native harness. A directive defines permissions, token budgets, turn limits, what the LLM can and can't touch. But if your harness is the one running the LLM, your harness is the one that would need to enforce those constraints. And it doesn't know how. It's not built for that.

So RYE ships its own harness as just another tool to execute. When you execute a directive you can choose to run it in your harness as a verified instruction, or fork it to RYE's harness. When the LLM runs inside RYE's harness, the directive-as-contract becomes fully enforceable. Permissions are real. Budgets are real. The directive isn't just describing limits, it's enforcing them.

This is also where the recursive MCP comes in. When RYE forks a directive to its own harness, that harness itself has full access to RYE MCP. Which means a directive can spawn further directives, which can spawn their own. What looks like sub-agents in other frameworks is just the same agent running a narrower thread. Each child thread inherits the execution guarantees of its parent but can only be scoped down, never up. Permissions, token budgets, turn limits, all of it attenuates through the hierarchy. This is capability attenuation, and it's what makes the recursion safe rather than just deep. The recursion is a natural consequence of the architecture, not a feature added on top of it. Personally I build with Amp and think of in-harness execution as my front end thread driving directives that execute on managed threads underneath.

Every execution in RYE runs on a thread managed by Lillux. This gives you cross-platform thread management regardless of OS, spawn, poll, kill, and inspect any thread on any platform. A tool call has a thread. A directive has a thread with one key difference. A directive is LLM execution, the harness itself is just another tool call underneath, exposed through the fork parameter. That's one directive, one thread. From here graph execution follows naturally.

---

**Graph execution**

A graph in RYE is a YAML file that describes a workflow as a series of nodes. Each node is Search, Load, Execute, Sign. You describe the shape of the work and RYE walks it. Like any other tool, a graph is defined in YAML and runs on the RYE graph runtime, just another runtime in the toolchain.

What graphs give you that directives alone don't is clean composition of deterministic and non-deterministic steps in the same execution layer. Run a bash script, have an LLM evaluate the output, branch on its judgment, run another tool. Fixed logic and reasoning mixed in a single verified workflow. Every node verified before it runs. Every transition attributable. And because the whole graph is data, the whole graph is hashable and trust verified.

---

**CAS + registry — write once, run anywhere**

At the same time I've been describing a content-addressed system without naming it. Every item you sign gets hashed. Every tool describes its execution as data. Every graph node is verified before it runs. CAS is just the name for what the architecture already produces.

As you execute graphs, RYE persists the results as SHA256 hashed objects in the workspace, sharded by their first four characters. State checkpoints, node receipts, execution snapshots. Each one is immutable and addressed by its content. Any graph execution can be replayed, inspected, and verified after the fact. You don't just know what ran. You can prove it. As AI moves deeper into real systems that audit trail stops being a nice-to-have.

CAS also lets you bundle entire packages as a signed manifest over a collection of objects. Every file listed with its hash. RYE OS ships this way. Code bundles, web bundles, tool collections, any shareable unit of work can be packaged, signed, and distributed through the registry with full provenance intact. If you know Nix, this is familiar. The manifest is the declaration, the hashes are the guarantee, the signature is the provenance.

With that, a registry comes to life. When you push an item it's hashed and signed locally first. The registry re-validates and adds its own provenance marker. When someone pulls your item their client verifies four things: signature exists, content hash matches, Ed25519 signature is valid, signing key is in their trust store. First pull uses TOFU, same model as SSH. The result is a public ecosystem of signed, reproducible work. Anyone can pull a tool, verify it against the author's key, and know they're running exactly what was published. That's not a platform guarantee. It's a cryptographic one.

Write once run anywhere becomes a structural property. Tool execution described as data runs identically anywhere Lillux runs. Reproducibility isn't a promise. It's what falls out of content-addressing everything from the start.

Once everything is content-addressed, signed, and packaged, you can move it. And when you can move it, you get fully verified remote execution.

---

**Verified remote execution**

RYE defines named remotes in a simple YAML config. A URL and an environment variable for your registry API key. A sync protocol kicks in automatically. Three operations: has, put, get. The client sends a list of content hashes. The remote says which it has and which are missing. The client uploads the rest. The remote materializes a full workspace snapshot from those objects and your project files, verified against the signed manifest before anything runs. No file diffs, no git pushes, no deploy scripts. Content-addressed objects in, verified execution out.

The remote has its own Ed25519 key, generated on first boot. You pin it on first connect, same as SSH. When the remote produces results it signs them with its own key. The output carries two signatures, yours on the input, the remote's on the output. Chain of custody across machines. You know what ran, where it ran, and who attests to the result.

The remote exposes the same four primitive actions as HTTP endpoints. It's the same interface whether local or remote. Secrets are never stored in CAS, injected at execution time at the Lillux level, below RYE entirely. On a named remote, environment variable injection currently routes through the centralised registry, a bootstrap convenience not a structural dependency.

This is what enables always-on webhook agents. External services trigger remote execution via signed webhook bindings. The binding locks down exactly what directive or tool can execute and against which project. The caller can only provide parameters. An agent that lives outside your machine entirely, triggered by the world, executing verified work on infrastructure you control.

Today I'm hosting the RYE remote executor on Modal. But the architecture doesn't require that. A remote is already a self-identifying node with its own key, its own execution capability, a sync protocol that only needs content hashes to operate. You can host it yourself. The step from named remote to self-certifying node advertising capabilities to any peer is not an architectural change. The sync doesn't change. The execution doesn't change. The CAS doesn't change. Just the auth.

And that's where the decentralised architecture begins to unlock.

---

**Encrypted execution on untrusted nodes — the future**

The entire centralised layer is a bootstrap convenience, not a dependency. The signed objects, the content hashes, the Ed25519 keys, none of that requires a central server to be true. Any peer that can store and serve CAS objects can be a registry. Auth migrates to per-request signatures, same model the remote executor already uses. Encrypted execution on untrusted nodes, where your data remains encrypted through execution on hardware you don't control, is the horizon. The architecture composes directly from the primitives already in place.

---

**What actually is an agent**

Consider how every computing platform we've ever built actually works. The OS persists. Processes are transient. The kernel doesn't care what runs through it. It's not a Python OS or a bash OS. Programs come and go. The environment remains.

Agents got this completely backwards. The model sits at the centre. Memory, tools, files orbit around it. Everything exists to serve the model. OpenClaw was the first to make this architecture complete enough for people to actually use, richer environment management, better tool orchestration, persistent memory across sessions. It's genuinely the most complete version of this architecture. The state persists. But ownership doesn't. There's no cryptographic chain between the work and the person who produced it. Move machines, switch tools, and you have files with no provenance, no verified identity, no way to prove they're yours or that they haven't been altered. Continuity without ownership. A session pretending to be an entity.

The pieces have existed separately. Reproducible builds, content-addressed storage, cryptographic signing. Nobody has combined them into an execution substrate where the agent itself, its tools, its history, its permissions, are all cryptographically tied to a single portable identity. Because until agents, nothing needed all of that at once.

And none of this requires an LLM at all. The four primitives, the execution chain, the CAS, the registry, deterministic graphs, verified tool pipelines, reproducible data workflows, all of it runs without ever calling a model. The LLM is just one kind of runtime that can inhabit the substrate. A powerful one, but one among many.

Which brings us to the actual answer. The agent is the signing key. The workspace. The accumulated directives, knowledge, execution history. The model is just the current best available reasoning engine passing through it. Swap Claude for GPT for Gemini for whatever comes next. The agent remains. Because the agent was never the model. It's the data it consumes and produces, cryptographically tied to the key that signs it.

Your key is unique to you. Not configured to be, not assigned to be. Cryptographically guaranteed to be. Which means your agent is irreducibly yours in a way no hosted platform can replicate. Not a product you're using. Not an instance of something running on someone else's infrastructure with your settings applied. Something that could only have come from your key.

Now imagine that at scale. Millions of signing keys. A global network of verified execution nodes. Public registries of reproducible work anyone can pull and trust. Every agent a genuinely distinct entity, not a million instances of the same product with different configurations. Persistent, portable, and provably yours.

The intelligence is borrowed. The identity is yours.

---

Whew, okay we got there. If you've been following along through the whole thing I thank you. You're a real one. There's more to get into, hooks, harness internals, context injection, graph runtime internals, and I've deliberately kept concrete examples out of this. The architecture is the argument. If you want to see it in practice, the repo has you covered. The onboarding experience isn't fully there yet. Remote execution, CAS and the registry need more battle testing. But after watching Jensen put OpenClaw on the GTC stage as the AI operating system of the future, I couldn't hold it back any longer.

On his recent Lex Fridman episode he argued that to keep agents secure you give them two of three capabilities at any time: sensitive data access, code execution, external communication. I understand why. But that is a guardrail bolted onto an architecture that was never designed with trust in mind. When every action is signed, scoped to a directive, and verified before it executes, you don't need to withhold capabilities. You just need to know exactly what ran, who authorized it, and that it wasn't altered. That is what RYE gives you.

If we keep building AI agents on a paradigm that hasn't properly thought this through, we're all in for a real headache as AI integrates deeper into society.

This is why I need you to try it. Not because it's finished. But because the architecture is right and the direction matters. This project is for those interested in real digital ownership. Those dissatisfied with what OpenClaw and other agent SDKs offer. And if it resonates, I'd love your help building it. This is a big problem for one person to solve alone, and the network only becomes real when people use it. Contributors, testers, skeptics, all welcome.

Give it a go.

```
pip install ryeos-mcp
```

Hook it up to your harness, then prompt your agent:

```
rye execute directive init
```

Let me know what you think.
