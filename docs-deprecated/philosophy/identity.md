```yaml
id: identity
title: "Identity on the Substrate"
description: Why Rye signs the data the agent operates within, not the agent itself
category: philosophy
tags: [identity, signing, portability, trust, design]
version: "1.0.0"
```

# Identity on the Substrate

Everyone else is asking "how does the agent prove who it is to external systems?" — that's the enterprise IAM framing.

Rye is asking something different: "who authored and trusts the data that defines what the AI _is_?" The signing isn't authenticating the agent to a service. It's establishing the chain of custody on the tools, directives, and knowledge that form the agent's entire operating context. The identity lives in the substrate, not in the transport layer.

That inversion — identity on the _data_ rather than on the _compute_ — is the part that doesn't exist anywhere else in a shipped, working system.

## The Landscape

Cryptographic identity for AI agents is an active area. Every existing approach frames it as an authentication problem:

| Approach                                  | What it signs             | What it solves                               |
| ----------------------------------------- | ------------------------- | -------------------------------------------- |
| Enterprise IAM (Keyfactor, SPIFFE, X.509) | The workload or container | "This process is allowed to call this API"   |
| Verifiable Credentials (Dock.io, DIDs)    | The agent's claims        | "This agent was issued by this organization" |
| OAuth / mTLS (Curity, standard IAM)       | The transport             | "This request is authenticated"              |
| Research proposals (MSCIKDF)              | Derived session keys      | "This agent can prove its origin"            |

These all answer: _how does the agent prove who it is?_

None answer: _who decided what the agent can do, and has that been tampered with?_

## What Rye Signs

Rye's Ed25519 key signs the substrate the agent operates within. See [Integrity and Signing](../internals/integrity-and-signing.md) for the full technical breakdown.

| Item type       | What it controls                                       | Signed by             |
| --------------- | ------------------------------------------------------ | --------------------- |
| Directives      | Workflows and instructions — _how_ to accomplish tasks | Author's Ed25519 key  |
| Tools           | Executable capabilities the agent can invoke           | Author's Ed25519 key  |
| Knowledge       | Domain context and learnings the agent draws from      | Author's Ed25519 key  |
| Runtimes        | How tools are invoked (interpreter, flags, env)        | Author's Ed25519 key  |
| Transcripts     | The record of what the agent did                       | Checkpoint signatures |
| Thread metadata | Capabilities, limits, and budget for child threads     | JSON field signature  |

`verify_item()` runs on every `execute` and `load` call. Tampered items raise `IntegrityError`. No fallback. No bypass. System items are verified with the same flow as user items — there are [zero exceptions](../internals/integrity-and-signing.md#zero-exceptions).

## Why This Matters

### The model is replaceable

Swap Claude for Gemini. Switch from GPT-4 to whatever ships next year. The model changes. Your signed tools, directives, and knowledge remain. Identity persists because it's bound to the data the model operates within, not to the model itself.

### Portability without loss of trust

Run Rye in a serverless function that spins up, executes, and dies. The compute is ephemeral. The signature on every item traces back to you. Move from AWS to Modal to a Raspberry Pi — the chain of custody is intact because it travels with the data, not with the infrastructure.

This is a consequence of the [three-tier space](../internals/three-tier-spaces.md) design. Your `.ai/` directory is a portable filesystem. The signing key is a portable identity. Together they form a self-contained, verifiable workspace that runs anywhere.

### Authorship compounds

Pull a tool from the [registry](../registry/sharing-items.md). It carries its author's signature with [registry provenance](../internals/integrity-and-signing.md#registry-provenance) (`|registry@username`). You trust the author, not the registry, not the CDN, not the hosting provider. Build on top of that tool and your signature layers into the chain. The [trust store](../internals/integrity-and-signing.md#trust-store) manages this — a set of Ed25519 public keys you've explicitly trusted, resolved through the same three-tier system as everything else.

## The Framing Shift

Traditional identity systems protect the _boundary_ — who can access what, which requests are authorized, which containers are trusted.

Rye protects the _interior_ — what the agent is made of. The tools it can use, the instructions it follows, the knowledge it draws from. Control the substrate and you control the agent's behavior regardless of where it runs, which model powers it, or who hosts the infrastructure.

The signing key is not a security feature bolted onto an agent framework. It _is_ the identity. Everything else — the model, the hosting, the transport — is interchangeable scaffolding around it.
