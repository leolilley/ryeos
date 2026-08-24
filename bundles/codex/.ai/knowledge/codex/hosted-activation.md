<!-- ryeos:signed:2026-08-24T16:46:45Z:b23c45b5040b3324616a463727eb84b3100fe22dcf270ae202ac2eddcd5590cf:noTBtI1C6xuuNi5cPIf+gAFcjU/JOpywDZx+obXr63iTXseYDHhcBemfR/0GeTnpJjcjSmpAaRgTfrPuiIrQBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: codex
tags: [codex, hosted-execution, structured-session, credentials, acceptance]
version: "1.0.0"
description: >
  Activation, credential ceremony, command routes, and release acceptance for
  the pinned Codex structured-session workload.
---

# Hosted Codex activation and acceptance

The Codex bundle hosts the pinned Codex App Server using ChatGPT subscription
authentication managed by Codex. It does not route Codex through RyeOS local
inference. The executable, its same-version code-mode host, its packaged
model-command runtime resources, and App Server schemas are pinned by
activation and source closure.

This is installed operator knowledge shipped by the Codex bundle. It documents
how to activate and accept that optional integration; it is not a RyeOS
repository-development workflow. The provider-neutral authority and lifecycle
contract beneath it is documented by
`knowledge:ryeos/core/execution/worker-hosted-execution` in the Standard
knowledge bundle.

## Activation

1. Publish the `hosted-workflow` set containing `core`, `central-auth`,
   `standard`, `hosted-node`, and `codex`. Generic worker-execution runtime/preparer
   binaries belong to `core`; the generic knowledge kind belongs to
   `standard`; bridge/profile and all Codex-specific data belong to `codex`.
2. Import and bind the exact realization in
   `.ai/config/codex/activation.yaml`.
3. Configure node-owned persistent-session limits. Bundles never enable node
   worker capacity themselves.
4. Provision the same configured-operator identity at the operator endpoint
   and a dedicated hosted node. First admit the source node key on the target
   as `remote_node` with only
   `ryeos.attest.request.forwarded-operator`; this key co-signs the exact
   configured-operator request and proves source-node transit. Then stop the
   hosted daemon and run the supported local command
   `RYEOS_APP_ROOT=<hosted-root> ryeos authorize-client --public-key
   <raw-base64> --origin-site-id site:<source>
   --allow-semantic-conversion --scopes <exact-scopes>` on the hosted node.
   Use the complete exact scope set printed in the Codex bundle README. The
   target-signed `remote_operator` grant constrains which source site may
   forward the operator; it is not transit proof without the separate
   source-node co-signature. A plain `local_client` grant is not acceptable,
   and ordinary remote-node grants remain node principals that cannot own this
   workflow.
5. Open projectless login, call `credential.login.start`, finish the ephemeral
   ceremony, call `credential.account.read`, close it, and confirm the exact
   login epoch/account digest.
6. Establish the configured operator's principal-scoped project HEAD through
   the standard local `commit` or an explicit full-project
   `service:remote/push` with `outbound_principal: configured_operator`. A
   local launch uses `--current-head`; a client with a different absolute path
   uses `service:remote/run`, whose configured project binding supplies the
   destination path, preserves that configured-operator principal, and
   co-signs the request with the admitted source-node key. It returns the
   durable accepted thread ID. Drive projectless credential and session
   services through wait-mode `service:remote/run` with
   `outbound_principal: configured_operator`; do not connect an operator-key
   client directly to the hosted daemon. Call
   `session.start`, then
   `turn.start`, `turn.steer`, and `turn.interrupt`. Every turn is bound to the
   one returned remote thread; cross-thread targeting is rejected.
7. Resolve digest-fenced pending approvals. Command approval displays bounded
   command/cwd. File or permission expansion is deny-only without an exact
   admitted reviewable effect.
8. Complete work, validate the frozen candidate, then publish or discard.

External-content maintenance after activation requires a quiesced class
transition, not another identity. Finish or terminate hosted executions, stop
the daemon, run offline `authorize-client` without `--origin-site-id`, with
`--allow-semantic-conversion`, and with only the required local maintenance
scopes. Start the daemon and perform import/bind/scrub/release; stop it again;
then reinstall the exact `remote_operator` grant with
`--allow-semantic-conversion`. Never use `--merge-scopes` across either
transition. A separate key cannot pass the exact configured-operator check.

On daemon restart, the generic worker-execution runtime reclaims the same root
thread and exact unpublished CoW workspace, starts a fresh pinned App Server
process, then executes the signed `session.resume` and `session.read` routes for
the retained Codex thread. The worker boot epoch changes; the RyeOS root thread,
Codex thread identity, credential generation, and workspace identity do not.
Login executions intentionally disable remote-thread recovery and settle
cancelled if interrupted.

That App Server reattachment applies only while the hosted session itself is
live. If restart occurs after Codex has stopped and RyeOS has frozen the
candidate, RyeOS starts no new Codex process. The generic disposition controller
repairs the root-tested candidate boundary only after the private workspace is
closed, waits for validate/publish/discard, then finalizes the RyeOS root.

The route IDs above are canonical. Inspect a complete leaf such as
`ryeos help codex session command` for its current CLI presentation; every
command must still match the signed command and service contracts.

## Mechanical policy boundary

The signed profile launches Codex with immutable argv containing every
security-critical override; those immutable arguments are the sole
configuration authority. A same-UID process can replace a file in its writable
home, so RyeOS atomically resets the mode-0400 compatibility seed before every
worker generation and never treats workload-authored changes as retained
policy. If the node enables a generic enforced isolation backend, RyeOS
additionally overlays that file read-only, but hosted Codex does not require
RyeOS's optional Bubblewrap isolation bundle or another RyeOS isolation
backend. OpenAI's standalone package does include its own private Bubblewrap,
shell, and `rg` resources; RyeOS pins and materializes those as workload files
so Codex can enforce the narrower model-command permission profile. They do not
become a RyeOS launch backend. Immutable CLI overrides fix login, credential
store, built-in provider, empty MCP map, approval routing, permission profile,
command network, shell environment, and disabled helpers for process life.
Thread start/resume checks supported response fields for effective approval and
sandbox policy.

The child workload also inherits an owner-only creation mask. Codex can
explicitly restore broader bits on non-secret state such as its installation
identifier, so the provider-neutral bridge descriptor-traverses and tightens
the complete private home after initialization and at every IPC boundary. Any
link, special entry, mount crossing, or bounded-resource violation fails the
worker closed before a credential-bearing operation is sent.

For pinned Codex 0.147 the granular approval policy is inherited from immutable
CLI configuration. Request-level `approvalPolicy` is intentionally omitted
because the stable App Server rejects that granular field unless the forbidden
`experimentalApi` capability is enabled.

App Server inherits a cleared minimal environment and no RyeOS control FD.
Model commands receive the signed Codex permission profile and cannot access
profile home, boot/capsule metadata, callback authority, DBus/keyring
coordinates, or direct network through that contract. Without an enforced
node-isolation backend this is not an OS-level hostile-workload containment
claim. Codex's signed `session_resources` override is capped by the generic
worker kind and frozen into the admitted capsule. Its finite `RLIMIT_NPROC` is
shared by the daemon's real UID, not a per-worker process boundary; node policy
and the persistent-session registry separately bound session groups and total
dedicated workers. Stderr drains continuously to a non-retained private sink.

## Release acceptance

Run packaged artifacts in a disposable app/state root, never the developer's
installed node, and prove:

- remote configured-operator acceptance only with the exact admitted
  source-node co-signature, rejection of a missing/wrong-site proof, another
  key, a plain local-client grant, and local-only operator APIs;
- online delegation/admission create-only behavior, explicit stopped-daemon
  class transition in both directions, and the complete maintenance ceremony;
- device login, confirmation, fresh-process continuity, refresh, and restart;
- real turn, pushed events, approval, interruption, and blocked-route cancel;
- daemon restart before/after contact, during approval, and after HEAD contact;
- candidate capture, closure/base validation, publish CAS, discard, and root
  finalization;
- revoke/retry under proved and unproved worker cleanup;
- Codex-absent `standard` and `central-host` publication still stage generic
  core worker-execution binaries; and
- signatures plus clean install/boot inventory resolution.

Environmental inability to run a probe is not passing evidence and does not
justify changing the live local installation.
