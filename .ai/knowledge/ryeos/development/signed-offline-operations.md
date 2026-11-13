<!-- ryeos:signed:2026-11-13T16:31:52Z:b86cf8badb16d416d1d27e45dd27b3656ca6a2598e2aabd05744b31889ab6a14:aWKuSWcy7TbgKP3RiCBT5yQ9sym4s+6m0jvP5SWQOalEoSCUFWF2Kwbs296uqzhRq2CWIBQ8Jx/OFSyIoJXjAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "signed-offline-operations"
title: "Signed Offline Operations: Design Rationale"
description: "Why stopped-node changes are signed requests applied at the top of ryeosd startup, what they protect against, and the invariants changes to that code must keep"
entry_type: decision
version: "1.0.0"
```

# Signed Offline Operations: Design Rationale

Some node changes need no daemon holding the state lock: policy replacement,
`policy-apply`, external-execution compose and its snapshot and
qualification-session forms, capacity reissue and reanchor, `allow-restart`,
the resets and the authorization reset. A signed offline operation carries
each of these as a signed request that `ryeosd` applies itself, before any
other startup check. Usage is in `deploy/composition-images.md` and the root
`README.md`. This entry records why the mechanism is shaped this way.

Code: `crates/daemon/ryeos-node/src/offline_operation/` (contract, envelope,
table, apply, ledger, deliver), the startup hook in
`crates/bin/daemon/src/main.rs`, and the CLI in
`crates/bin/cli/src/lifecycle_offline_operation.rs`.

## Why not a shell

Each of these changes used to be a CLI lifecycle command that assumed an
operator with a shell inside the node's filesystem. On a hosted node that
shell disappears exactly when it is needed. A held container runs no daemon,
fails the platform health check, and is unhealthy; a platform SSH gateway may
refuse sessions to an unhealthy instance and may replace it. The other routes
were worse: a pre-entrypoint command override runs as root, and the
entrypoint changed state from unsigned environment variables. None of that
was audited.

## What it protects against

Attackers fall into three classes:

| Class | Can | Signed offline operations |
|---|---|---|
| A. Environment only | set env vars and platform secrets, choose the image | refuse anything without a valid signature from an authorized signer |
| B. Inbox only | drop files into the node's inbox | refuse unsigned, tampered, replayed or stale requests |
| C. Volume writer or root | write the persistent volume, run commands as root before start | do not stop. The node key, operator key and grants all live on that volume. |

On a hosted node the platform is trusted, and this design does not change
that. It delivers four things instead: no shell on any platform, a signed
request and a node-signed receipt for every applied change, refusal of
classes A and B, and the retirement of the unsigned environment routes. The
ledger's chain tip makes a later rewrite of history by class C detectable
against an earlier tip. It does not prevent one. Keeping class C out needs a
key the volume cannot reach, which is later work.

A remote signer only authorizes. Artifacts the node trusts are still signed
by the keys on the volume: the local operator key where an operator signs,
the node key where the node signs. Receipts are signed with the node key.

## The request

A request (`ryeos/offline-operation-request/v1`) names the target node, the
signer and the digest of its grant, one operation id, exact arguments, the
digests of its inputs, the expected result and the precondition facts it was
signed against.

- **No ambient inputs.** A handler reads only its arguments, its
  digest-verified inputs and the node's own state. It is given no ledger
  path and no way to read the process environment (`HandlerContext`).
  Operations that produce bytes pin the result, and the handler refuses
  before committing when the result differs.
- **Policy replacement uses the image seed only.** The request names the
  measured seed digest, node profile, host class and substrate image digest,
  so a request signed for one image cannot apply on another.
- **Every request pins `init_completion` and `ledger_head`.** Pinning the
  ledger head totally orders requests: of two requests signed against the
  same head, at most one applies.
- **Replay** is refused by the request's own digest in the ledger. There is
  no separate nonce.
- **Lifetime** defaults to one hour and may not exceed 24 hours. `issued_at`
  may lead the node's clock by at most two minutes.

## Who may sign

- The node's configured local operator.
- A `remote_principal` grant. Its signature is checked against the grant's key,
  the grant is re-read for every request and must still have the digest the
  request names, and the request must name the grant's origin site. This is
  a separate path from the online `require_admitted_operator`, which needs a
  forwarding proof an offline document cannot carry.
- Never a `remote_node` grant.

Each operation has an exact scope, `ryeos.offline.<operation>`. Wildcard
grants never satisfy it, so a broad grant made for product pushes carries no
offline power. `authorize-client` refuses wildcard patterns under that prefix,
and only the local operator may grant an exact offline scope. At startup the
node reports any grant that would have matched under wildcard rules.

Some operations change what the node trusts or runs: policy replacement,
`policy-apply`, the compose forms and the authorization reset. Granting one
is equivalent to granting all. A remote signer may request one only when the
node's signed maintenance policy lists it under `remote_offline_operations`.
That is profile data: the `hosted-controller` profile delegates exactly
`node.external-execution.compose`, every other shipped profile delegates
nothing. The authorization reset is local-signer only whatever the policy
says.

## Why the table is compiled in

Application runs before node configuration can load, and configuration may be
exactly what is broken. So the operation table is compiled into the binary
(`table.rs`): id, argument check, input slots with byte bounds, precondition
and result facts, the authority-bearing and local-only flags, the handler and
its recovery rule. The handlers are the existing stopped-node cores, a closed
and mechanical set. Which operations a remote signer may request stays
policy data. No new engine kind was added; `kind: service` descriptors with
`availability: stopped_node` remain the command registry for services.

Every handler declares how a rerun is safe after a crash between its effect
and its receipt: it either finds its recorded request digest and returns, or
it converges over the effect. A handler without a recovery rule is not
registered.

## Why it runs first

`apply_pending_offline_operations` is called at the very top of `ryeosd`
startup, before `require_initialized`, the state lock, the restart-inhibition
check and bundle-set recovery. Each of those can refuse, and several are what
these operations repair. Application takes the state lock itself and releases
it before normal startup continues. The CLI applies its own requests through
the same code (`apply_local`).

## Application rules

- Bounds first: a request is at most 64 KiB and the inbox holds at most 16
  requests. Reads do not follow links.
- Envelope checks in order: node fingerprint, signature, grant and its
  digest, exact scope, the authority-bearing rule, expiry, issue time.
- Each input is opened once and hashed from the open descriptor; the handler
  gets those bytes and never re-reads a path.
- After an authority-bearing operation applies, application stops and
  startup proceeds with the new state. Later requests wait for the next
  startup and are checked against the state that now exists.
- The authorization reset is applied alone in its startup. Its receipt lists
  every discarded grant digest and is written before the swap.
- **Refusal never crash-loops.** A refused request moves to `refused/` with a
  node-signed refusal receipt; `refused/` is bounded by count and bytes. A
  request whose handler fails three startups running is refused as stuck.
  Application is time-bounded (15 minutes) and stops at a request boundary.
  An error in application itself is logged and the ordinary startup checks
  run next.
- Every startup prints the node's precondition facts to standard error, so a
  hosted node in a crash loop publishes what a request must be signed
  against in its platform log.

## Delivery

Two channels feed one inbox under `.ai/state/offline-operations/inbox/`:

1. A running node takes `POST /offline-operations`, checks the envelope and
   the preconditions it can measure cheaply, and stores the request. It
   stops only when asked (`--stop`), as a plain operator stop with no restart
   inhibition. The supervisor or platform starts the next incarnation.
2. A node that cannot start gets the request from the platform. The
   entrypoint copies `RYEOS_OFFLINE_OPERATION_REQUEST` and
   `RYEOS_OFFLINE_OPERATION_INPUTS` into the inbox as root, verifies nothing,
   and never blocks the start on a copy failure.

## The ledger

Receipts form a hash chain under `.ai/state/offline-operations/ledger/`. No
handler and no reset may write there. The ledger rotates by count with a
checkpoint that carries the chain tip forward. Routes and the CLI return
digests and receipts, never input bytes.

## What this retired

- `RYEOS_RESET_NODE_POLICY_GENERATION` and
  `RYEOS_EXECUTION_HISTORY_SCHEMA_CUT` now refuse the start when set; the
  signed `node.reset.policy-generation` and `node.reset.execution-history`
  replace them.
- First-boot re-seed refuses when the volume already holds a node identity
  or an init completion, so a deleted bundle directory cannot silently
  re-initialize a node under the platform's profile.
- `RYEOS_OPERATOR_HOLD` stays as break-glass for nodes whose operator has a
  shell. No procedure depends on it.
- Hosted health checks use `/_ryeos/health` (liveness) instead of
  `/_ryeos/ready`. No HTTP is served while application runs, because it runs
  before the listener binds.

## Not covered

Credential provisioning, bundle-set update and the stopped-node bundle
services are not offline operations. Credential inputs would need sealing to
the node's vault key so that no secret sits in the inbox or ledger.
