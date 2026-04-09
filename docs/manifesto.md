I built a cryptographically signed AI execution system that inverts how agents work.

Before I get into this, a quick note. I started building this before OpenClaw blew up. But I think if we keep building on the architecture it lays its foundations on, we're collectively heading toward a mistake. There is a more secure, more principled way to build agents. RYE OS is my attempt at showing what that actually looks like. I'm not a researcher. I don't work for a lab. I'm just a developer who wanted a better MCP for my own workflows and kept pulling the thread until the full picture came clear.

---

**What is RYE OS?**

RYE OS is a cryptographically signed, graph-executable AI execution system — data-driven, capability-scoped, and registry-backed through content-addressed execution. It lives inside your projects, runs through a recursive MCP, and executes remotely or locally on nodes you control — going as deep as you dare.

Yes that's a lot of jargon but it's truly the TL;DR of what this is. There is a lot to absorb here but keep reading and I can show you how it all comes together.

---

**Action primitives**

After working with AI over these past few years I've come to realise that to really turn an LLM into an agent, you first need to understand the core primitives of agentic operation.

Strip away the abstractions and every agent does two things: it retrieves data, and it acts on it. Every framework, every harness, every workflow is some variation of retrieve and act.

If you're trying to derive the right primitives for agents, you don't need to reinvent it. Computing already answered it at every layer. The CPU's fetch-execute cycle arrived at the same reduction operation, you point at a thing and you get it back. Whether you're pointing by address, by URL, by query, or by content hash doesn't matter. How you point at it is a parameter, not a different operation.

Retrieval reduces to Fetch. The "act" is the obvious half. You run the thing. Execute. In RYE that means resolving a tool through its execution chain down to a primitive that can run. Every agent framework you've seen is already doing some version of these two. When your harness comes prebuilt with filesystem or web search tools, you've pre-configured Fetch. When Claude Code finds a skill and runs it, that's Fetch, Execute. When OpenClaw routes a task to a sub-agent, same thing.

None of them have the final action primitive. Sign. That's what makes the other 2 secure. Other frameworks treat agent security as a runtime problem, watch what the agent does and intervene when it steps out of bounds. In RYE permissions aren't enforced after the fact. They're declared, signed, and verified before execution ever starts. But to understand what gets signed, and why it changes everything, you need to understand what RYE actually works with.

---

**Item primitives**

The three actions tell you what an agent does. The next question is what it acts on. An agent needs three things: instructions that tell it how to behave, context that tells it what it's working with, and capabilities that let it do things. Those reduce to three item types. Directives, the HOW. Knowledge, the KNOW. Tools, the DO.

Directives act as just that, direction. They reflect your intent inflicted on the LLM to adhere to. Instructions with teeth. Steps to follow, how to respond, what tools to call, what to evaluate. On the surface this looks like what other frameworks call a skill. But a skill only describes what to do. A directive also enforces the bounds: token limits, turn limits, cost budgets, what the LLM is permitted to fetch, execute or sign, what context gets injected, what hooks fire. That's not just instruction. That's scope.

Knowledge is persistent context without intent. Best practices, something discovered during execution, transcripts, domain references. Anything worth remembering that isn't an instruction and isn't executable. Knowledge items are first-class, searchable and fetchable on demand. Not loose files in your project. Structured data the agent can pull when it needs it.

Tools are capabilities described as data. A tool doesn't just say "run this executable." It describes its own execution: what interpreter runs it, what environment it needs, what parameters it accepts, what schema validates its input. This is how RYE supports every language natively. Python, JS, Bash, all supported through a toolchain that describes its own execution. The execution is defined in the item itself, not hardcoded somewhere in the framework.

All three item types live in a workspace inside your project. RYE organises items across three spaces: system, user, and project. System is the immutable package install level, core runtimes, system knowledge, and the directives that govern how other items are created. User is your personal configuration accessible from any project, your trusted keys, your git workflow tools, your preferred web search setup, things you've built once and want everywhere. Project is specific to that codebase, the directives that define how this agent behaves, the knowledge scoped to this domain, the tools this project needs. Items resolve project first, then user, then system. Projects override defaults as needed. Your personal tools follow you into every project. Two agents working in the same codebase share the same ground truth.

In Linux, everything is a file. In RYE OS, everything is data.

And that's the insight that closes the loop on Sign. Because all three item types are data, all three can be hashed. And because they can be hashed, they can be signed and cryptographically verified.

When you set up RYE, it generates an Ed25519 signing key. This is your identity in the system. Not an account, not a login, a cryptographic key that only you hold. Every item you create can only be acted on if it is signed by a key you trust. And it can only be signed if it passes schema validation. The shape of the item is verified before the signature is ever applied.

Your directives carry your signature. Your tools carry your signature. Your knowledge carries your signature. RYE only acts on items signed by a key you trust. Hash validation, schema validation, signature verification, at every step. And because a directive defines not just what to do but what the LLM is permitted to do, how far it can go, and what it can spend, a signed directive isn't just an instruction. It's a contract.

---

**Lillux**

When a tool executes, it resolves through a chain. Tool points to a runtime, runtime points to Execute, Execute calls Lillux. Every chain, regardless of language or tool, terminates at the same place.

Lillux is the microkernel at the bottom. A single Rust binary with no runtime dependencies. Four concerns and nothing more: Execute, Memory, Identity, and Time. Every tool call in RYE eventually bottoms out here. The reason it's Rust is the reason it exists at all. This is the trust boundary. Tampered or unsigned items are rejected before they ever reach it. Secrets are decrypted here, below RYE entirely, so the LLM and item layer never see them. This isn't OS-level sandboxing, verified code runs with the permissions of the user calling it. The security guarantee is supply chain security, not runtime confinement. You know exactly what ran, who signed it, and that it wasn't altered. No fallback. No bypass.

---

**The daemon**

Everything described so far, the three actions, the three item types, signing, Lillux, runs on a single daemon called ryeosd. One binary. Same code everywhere. On your laptop it binds loopback. On a remote server it binds the network. The difference between local and remote is deployment config, not architecture.

Every entry point is a thin client over the same daemon API. MCP, the terminal, the CLI, webhooks, cron. You point your existing harness at RYE MCP and you're using it. Claude Code, Cursor, Codex, Amp, whatever you're already running. Three primitive actions as MCP tools. Fetch, Execute, Sign. That's the entry point.

Through MCP, RYE enforces hash validation, schema validation, and signature verification on every item. That's real security you get immediately. But there's a ceiling when running RYE through MCP in your native harness. A directive defines permissions, token budgets, turn limits, what the LLM can and can't touch. But if your harness is the one running the LLM, your harness is the one that would need to enforce those constraints. And it doesn't know how. It's not built for that.

So RYE ships its own harness as just another tool to execute. When you execute a directive you can choose to run it in your harness as a verified instruction, or fork it to RYE's harness. When the LLM runs inside RYE's harness, the directive-as-contract becomes fully enforceable. Permissions are real. Budgets are real. The directive isn't just describing limits, it's enforcing them.

This is also where the recursive MCP comes in. When RYE forks a directive to its own harness, that harness itself has full access to RYE MCP. Which means a directive can spawn further directives, which can spawn their own. What looks like sub-agents in other frameworks is just the same agent running a narrower thread. Each child thread inherits the execution guarantees of its parent but can only be scoped down, never up. Permissions, token budgets, turn limits, all of it attenuates through the hierarchy. This is capability attenuation, and it's what makes the recursion safe rather than just deep. The recursion is a natural consequence of the architecture, not a feature added on top of it. Personally I build with Amp and think of in-harness execution as my front end thread driving directives that execute on managed threads underneath.

Every execution in RYE runs on a thread managed by Lillux. This gives you cross-platform thread management regardless of OS, spawn, poll, kill, and inspect any thread on any platform. A tool call has a thread. A directive has a thread with LLM execution, the harness itself is just another tool call underneath, exposed through the fork parameter. That's one directive, one thread. From here you can naturally extend this to graph execution.

---

**Graph execution**

A graph in RYE is a YAML file that describes a workflow as a series of nodes. Each node is Fetch, Execute, Sign. You describe the shape of the work and RYE walks it. Like any other tool, a graph is defined in YAML and runs on the RYE graph runtime, just another runtime in the toolchain.

What graphs give you that directives alone don't is clean composition of deterministic and non-deterministic steps in the same execution layer. Run a bash script, have an LLM evaluate the output, branch on its judgment, run another tool. Fixed logic and reasoning mixed in a single verified workflow. Every node verified before it runs. Every transition attributable. And because the whole graph is data, the whole graph is hashable and trust verified.

---

**CAS + registry — write once, run anywhere**

At the same time I've been describing a content-addressed system without naming it. Every item you sign gets hashed. Every tool describes its execution as data. Every graph node is verified before it runs. CAS is just the name for what the architecture already produces.

As you execute graphs, RYE persists the results as SHA256 hashed objects in the workspace, sharded by their first four characters. State checkpoints, node receipts, execution snapshots. Each one is immutable and addressed by its content. Any graph execution can be replayed, inspected, and verified after the fact. You don't just know what ran. You can prove it. As AI moves deeper into real systems that audit trail stops being a nice-to-have.

CAS also lets you bundle entire packages as a signed manifest over a collection of objects. Every file listed with its hash. RYE OS ships this way. Code bundles, web bundles, tool collections, any shareable unit of work can be packaged, signed, and distributed with full provenance intact. If you know Nix, this is familiar. The manifest is the declaration, the hashes are the guarantee, the signature is the provenance.

With that, a registry follows naturally. The registry isn't a separate centralised service. It's a feature of ryeosd, the same daemon that runs everything else. Storage is CAS-native. When you push an item it's hashed and signed locally first. The registry validates and stores it, but doesn't re-sign. Your signature is the sole provenance. When someone pulls your item their client verifies four things: signature exists, content hash matches, Ed25519 signature is valid, signing key is in their trust store. First pull uses TOFU, same model as SSH. Anyone can pull a tool, verify it against the author's key, and know they're running exactly what was published. That's not a platform guarantee. It's a cryptographic one.

Write once run anywhere becomes a structural property. Tool execution described as data runs identically anywhere Lillux runs. Reproducibility isn't a promise. It's what falls out of content-addressing everything from the start.

---

**Nodes**

Once everything is content-addressed, signed, and packaged, you can move it. And because every ryeosd instance is the same daemon, moving work between them becomes syncing objects.

A node is a ryeosd instance you've connected to and pinned via TOFU. Your laptop runs one. A server you deploy to runs one. They're the same binary, the same API, the same auth model. The only difference is where the daemon binds.

CAS sync connects them. Three operations: has, put, get. The client sends a list of content hashes. The node says which it has and which are missing. The client uploads the rest. The node materializes a full workspace snapshot from those objects and your project files, verified against the signed manifest before anything runs. No file diffs, no git pushes, no deploy scripts. Content-addressed objects in, verified execution out.

Each node has its own Ed25519 key, generated on first boot. You pin it on first connect, same as SSH. When a node produces results it signs them with its own key. The output carries two signatures, yours on the input, the node's on the output. Chain of custody across machines. You know what ran, where it ran, and who attests to the result.

Secrets are never stored in CAS. They're sealed as HPKE-encrypted envelopes against the target node's public key and decrypted at execution time at the Lillux level, below RYE entirely. No centralised secret store. The secret only exists in cleartext inside the execution boundary.

This is what enables always-on webhook agents. External services trigger execution via signed webhook bindings. The binding locks down exactly what directive or tool can execute and against which project. The caller can only provide parameters. An agent that lives outside your machine entirely, triggered by the world, executing verified work on nodes you control.

---

**What actually is an AI agent**

Every agent framework today puts the model at the centre. Memory, tools, files orbit around it. Everything exists to serve the model. The state persists. But ownership doesn't. There's no cryptographic chain between the work and the person who produced it. Move machines, switch tools, and you have files with no provenance, no verified identity, no way to prove they're yours or that they haven't been altered. Continuity without ownership. A session pretending to be an entity.

RYE inverts this. The substrate is inert. Nodes sit idle. Objects sit hashed. Nothing executes until a key shows up.

In RYE, the agent is the signing key.

The cryptographically unique key that signs every item into existence. It authenticates to nodes. It makes every action, every thread, every tool call attributable to a single identity. The workspace, the directives, the execution history, all of it flows through the key and becomes provably yours. The substrate comes alive because a key activates it. Which means the agent can open multiple threads, fan out across nodes, run tasks in parallel, and every one of them traces back to the same fingerprint. There is no planner agent and builder agent and orchestrator agent negotiating with each other. No subagents. No trust asserted between roles. One key, one agent, operating across multiple execution contexts simultaneously. The model coordinating in thread A and the model executing in thread B are different inference calls. The agent is the same. Because the agent was never the model.

An Ed25519 keypair is mathematically unique. Not an account a platform assigned you. Not a username you configured. Its uniqueness is guaranteed by the mathematics. Not identity by convention. Identity by proof. Swap Claude for GPT for Gemini for whatever comes next. The agent remains. Something that could only have come from your key.

Now imagine that at scale. Millions of signing keys activating a shared substrate. A global mesh of nodes running the same daemon. Public registries of reproducible work anyone can pull and trust. Every agent a genuinely distinct entity, not a million instances of the same product with different configurations. Persistent, portable, and provably yours.

The intelligence is borrowed. The identity is yours.

---

**The future**

RYE is already an operating system. Lillux is the microkernel, process isolation and nothing more. The engine is the kernel, resolution, trust, chain building. ryeosd is the OS layer, threads, events, scheduling, networking. Canonical refs are the syscall interface, the stable contract between your agent and the system. The instruction set is open-ended. Add a new kind of computation without touching the kernel. Every instruction carries its own trust proof. That is not analogy. That is the architecture.

Two things extend where this OS can run.

Encrypted execution. Today the substrate runs on nodes you control. When execution itself is encrypted, when your data remains sealed through computation on hardware you don't control, the substrate extends to any node anywhere. You stop needing to trust the hardware.

Sovereign inference. Today RYE calls external LLM providers. You rent intelligence. When the model runs on your own hardware, `llm/complete` becomes just another tool that resolves through the same chain as everything else. The model is a device driver. `execute` is the syscall. At cluster scale, routing between GPU nodes is a tool, provisioning a new node is a directive, and the agent doing the work and the agent managing the cluster are the same agent. There's no control plane. Runbooks become executable. The agent is the operator, the workload, and the client.

---

Whew, okay we got there. If you've been following along through the whole thing I thank you. You're a real one. There's more to get into, hooks, harness internals, context injection, graph runtime internals, and I've deliberately kept concrete examples out of this. The architecture is the argument. If you want to see it in practice, the repo has you covered. The onboarding experience isn't fully there yet, and the sovereign inference vision is still ahead. But after watching Jensen put OpenClaw on the GTC stage as the AI operating system of the future, I couldn't hold it back any longer.

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
