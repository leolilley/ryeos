# Digital Sovereignty

> Sovereignty not as a feature someone grants you. As a property the mathematics enforce.

---

## The Premise

Every layer of RYE converges on one conclusion: the agent is the signing key, and the signing key is sovereign. The substrate is commodity. The intelligence is borrowed. The identity is yours. Follow every thread to its end and you arrive at a computing model where no platform can revoke your agent, no company can shut it down, no terms of service can take it away. The cryptography guarantees it.

This document traces the threads.

---

## Post-Platform Computing

Every agent framework today is a product. OpenAI's agents are OpenAI's software with your config applied. Anthropic's agents are Anthropic's software with your prompts. Switch products, lose the agent. The agent IS the product. That's the trap.

RYE has no product to trap you in. The "platform" is the protocol: canonical ref format + signing scheme + CAS. Anyone can run a node. Anyone can publish to a registry. The network effect comes from the shared addressing scheme, not from a company's servers. You can't get locked in because there's nothing to lock into. The key is yours. The substrate is commodity.

The internet was a network of documents. Then applications. Then platforms captured the applications and the network effects that came with them. RYE is a network of agents that can't be captured, because the identity is the key and the key is sovereign. There is no platform to capture. The protocol is open. The substrate is commodity. The only scarce thing is your identity, and that's yours by mathematics.

---

## Agents as Economic Actors

If every action is signed and attributable, and the CAS provides an immutable audit trail, agents can participate in systems that require accountability.

An agent that signs a directive and executes it produces cryptographic proof of what was agreed to and what was done. That's not just technical auditing. That's the foundation for agents operating in contexts where trust matters: contracts, compliance, financial operations, anything where "who did this and can you prove it" is a real question.

The verifiable history is portable. Move machines, switch providers, the history follows the key. Your agent's history is as real as your git history but for everything, not just code. Not a log file. A cryptographic chain of custody across every tool call, every directive, every item, every node, every thread.

---

## Composable Trust

Trust in RYE is not asserted. It's proven.

You trust Alice's tools because her key signed them and you pinned her key. Alice trusts Bob's knowledge because she pinned his key. You can pull Bob's knowledge through Alice's registry and verify the chain. The thing being trusted is the computation itself, not a platform's promise.

Capability refs extend this further. Alice can mint a signed token that lets your agent execute exactly one of her tools with exactly these params, once, before a deadline. That's not an API key. That's a cryptographically scoped delegation of a specific computation. Trust composes without anyone sharing credentials, without any central authority, without any platform mediating the relationship.

---

## Autonomous Agents as First-Class Identities

An agent is a signing key. A parent agent can generate a child key, scope its capabilities via attenuation, and set it loose. The child operates independently but its actions trace back through the trust chain to the parent's key.

This is not hypothetical. The capability attenuation model already does this for threads. Extending it to persistent identity means agents spawning agents, each with their own provable history, each traceable to a root key. The child can only do less than the parent, never more. Permissions attenuate through the hierarchy. The recursion is safe because the trust model makes it safe.

What emerges: a tree of agents, each genuinely distinct, each cryptographically accountable, each traceable to the human who holds the root key.

---

## Intelligence as a Utility

Sovereign inference is not "run your own model." It's intelligence becoming infrastructure.

Your agent submits a canonical ref to a model endpoint. The endpoint is a tool. The tool resolves through the chain. Whether it runs on your GPU, a cloud provider, or hardware on the other side of the world is a routing decision, not an architectural one. The canonical ref abstracts over the source of intelligence the same way it abstracts over the source of any computation.

Models become commodities. You shop for intelligence the way you shop for compute. The agent selects the best available model for each task. Model providers compete on price, performance, and capability. Your agent isn't locked to any provider because the agent was never the model.

When `llm/complete` is just another tool that resolves through the chain, the model is a device driver. `execute` is the syscall. The agent is the user. The operating system abstracts the hardware. All hardware, including the hardware that thinks.

---

## The Full Stack

Follow the layers:

- **Identity**: Ed25519 signing key. Yours by mathematics.
- **Data**: CAS. Content-addressed, signed, immutable. Yours by possession.
- **Compute**: Nodes running ryeosd. Same daemon everywhere. Yours by deployment.
- **Intelligence**: Models as swappable tools. Yours by selection.
- **Execution**: Lillux microkernel. Process isolation, nothing more.
- **Trust**: Signatures all the way down. No platform, no authority, no revocation.

Every layer is owned by the key holder. The entire stack from identity to intelligence is bound to a key only you hold.

---

## The Conclusion

Digital sovereignty as a mathematical property, not a marketing claim.

Your agent, your compute, your intelligence, your data. All bound to a key. All verifiable. All portable. All yours in a way that no hosted platform can replicate, because the mathematics don't care who runs the node or who made the model. The key is the agent. The agent is sovereign. The rest is substrate.
