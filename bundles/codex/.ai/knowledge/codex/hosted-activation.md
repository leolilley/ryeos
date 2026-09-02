<!-- ryeos:signed:2026-08-29T10:19:43Z:0ccb3b7d9f333102c7476c0b9c4f45ef1bc2bdf1eff131438bfa283bfbbef9db:Y9A0FIN164c8GjXDmfeRDK2OwPhuA1GhjYNpuV0iMz0b2J5cYRzT6d7GZ5vUIFj7lWxiSYDiexycOPBY3CFaDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: codex
tags: [codex, hosted-execution, structured-session, credentials, acceptance]
version: "1.6.0"
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
2. Before starting the hosted daemon, author both node-owned policies outside
   the live node namespace. Managed activation requires no named filesystem
   root: the daemon acquires exact signed bytes into its typed private runtime
   root and feeds them through the existing import and consumer-binding
   authorities. The external-content policy must explicitly grant the one
   HTTPS host and finite acquisition/storage ceilings:

   ```yaml
   schema: 1
   roots: {}
   limits:
     max_depth: 8
     max_entries: 64
     max_file_bytes: 268435456
     max_total_bytes: 536870912
     store_budget_bytes: 1073741824
     minimum_free_bytes: 1073741824
   managed_activation:
     allow_online: true
     allowed_https_hosts: [releases.openai.com]
     max_redirects: 0
     max_archives: 1
     max_compressed_bytes: 134217728
     max_expanded_bytes: 335544320
     max_members: 64
     max_member_bytes: 268435456
     max_concurrent_activations: 1
     cache_budget_bytes: 536870912
     store_budget_bytes: 1073741824
     minimum_free_bytes: 1073741824
     max_attempts: 3
   ```

   ```yaml
   schema: 1
   limits:
     max_pool_groups: 4
     max_total_processes: 4
     max_total_address_space_bytes: 68719476736
     max_total_cpu_seconds: 14400
     max_open_streams: 32
     max_active_streams: 4
     max_active_streams_per_subject: 1
     max_stream_backlog_bytes: 16777216
     max_total_backlog_bytes: 67108864
   ```

   A fresh `hosted-workflow`, `full`, or `full-sandbox` installation selects a
   publisher-signed init profile containing these values and publishes one
   complete node-signed generation under `<system>/.ai/node/policies/`.
   `external_content.yaml` and `persistent_sessions.yaml` are mandatory
   members; bundles never enable acquisition, storage, or worker capacity
   themselves. An operator changing either member later must stop the daemon
   and use `ryeos node policy-apply <section> <source.yaml>`, which validates
   the replacement and atomically republishes the complete generation. Do not
   hand-edit generation files or manufacture prerequisite policy documents for
   an ordinary fresh install.
3. Start the node while the configured operator still has its ordinary local
   grant, then activate the signed recipe:

   ```text
   ryeos external-content activate config:codex/activation online
   ryeos external-content activate config:codex/environment-activation online
   ```

   The generic service downloads or reuses the exact pinned archive, refuses
   redirects, enforces compressed/expanded/member bounds, and verifies every
   selected digest and executable mode. The first recipe imports the five
   worker file realizations, including the package's workload-owned command
   sandbox companion. The second creates only the signed environment's
   descriptor-rooted `bin/{zsh,rg}` tree through Lillux, captures it through
   the existing manifest importer, and binds it to
   `config:codex/environments/default`. Each recipe records one compact node-
   signed receipt. Neither creates a public assembly directory nor accepts a
   caller-authored manifest or mount. Repeat both activations independently on
   every node that may become a placement target. `offline` is accepted only
   when the exact archive is already present in that node's private managed
   cache.
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
   Portable placement is a separate internal transport boundary: admit each
   configured peer node key with only the generic closure-read,
   worker-placement preflight/prepare/adopt/abort, and follow-terminal scopes
   listed in `knowledge:ryeos/core/execution/worker-hosted-execution`. Do not
   add those services to the configured-operator grant. The public handoff is
   owner-authorized; autonomous transfer and recovery then use node-signed
   chain, placement, continuation, and follow testimony. This lets the original
   local operator endpoint receive a return handoff without changing its key's
   semantic class.
5. On every node that may host the session, independently open projectless
   login, call `credential.login.start`, finish the ephemeral
   ceremony, call `credential.account.read`, close it, and confirm the exact
   login epoch/account digest. The attached caller receives the device code;
   the recorded worker-command thread and any source-node `remote.run` thread
   retain only its canonical digest under the signed generic result policy.
   Never copy `auth.json`, tokens, or the remaining private profile home between
   nodes. Handoff proves only that each target-local confirmed account derives
   the same signed credential-subject digest.
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
   client directly to the hosted daemon. Start the worker with the signed
   `config:codex/environments/default` environment (the local CLI spelling is
   `codex session start <profile> --environment
   config:codex/environments/default --async --current-head`; a typed remote
   request carries the same `environment` ref binding). Call `session.start`, then
   `turn.start`, `turn.steer`, and `turn.interrupt`. Every turn is bound to the
   one returned remote thread; cross-thread targeting is rejected.
   Before cross-site handoff, the destination's configured-operator project
   HEAD must already be the source placement's exact base snapshot. Preserve
   that origin HEAD or use `remote reconcile-project-head` with both observed
   HEADs and an explicit content winner while no handoff is in progress. That
   provider-neutral operation creates one two-parent generation and publishes
   remote-first under durable recovery. Handoff deliberately refuses a missing
   or divergent destination HEAD instead of overwriting it.
7. Resolve digest-fenced pending approvals. This release exposes bounded
   command/cwd for review but makes command-execution, file, and permission
   requests deny-only. Accepting an upstream sandbox-escalation request could
   widen the immutable permission ceiling and is therefore not admitted.
8. Complete work, validate the frozen candidate, then publish or discard.
   `terminate` accepts only `reason: completed` or `reason: cancelled`.
   `completed` freezes a project session and exposes its candidate;
   `cancelled` terminalizes without a checkpointable placement. A portable
   checkpoint is therefore captured only after `completed`, and `resume`
   conditionally restores that manifest into a fresh placement before its
   worker is released.

External-content maintenance after activation requires a quiesced class
transition, not another identity. Finish or terminate hosted executions, stop
the daemon, run offline `authorize-client` without `--origin-site-id`, with
`--allow-semantic-conversion`, and with only the required local maintenance
scopes (`external-content/activate` plus `release` or `scrub` only when that
operation is actually required). Start the daemon and perform exact managed
activation or cleanup; stop it again; then reinstall the exact `remote_operator` grant with
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
backend. OpenAI's standalone package requires its own private
`codex-resources/bwrap` companion for restricted Linux command execution, so
the activation selects that exact file with the Codex executable, code-mode
host, Zsh, and `rg`. This is workload-owned pinned content: it does not select
RyeOS's Bubblewrap isolation backend, discover a host `bwrap`, or acquire
BusyBox. Immutable CLI overrides fix login, credential
store, built-in provider, empty MCP map, approval routing, permission profile,
command network, shell environment, and disabled helpers for process life.
Thread start/resume checks supported response fields for effective approval and
sandbox policy.

The child workload also inherits an owner-only creation mask. Codex can
explicitly restore broader bits on non-secret state such as its installation
identifier, but those nested modes remain behind the mode-0700 profile root.
Before attachment, the daemon strictly descriptor-traverses the stopped home,
counts but never follows Codex-owned links, and rejects special entries, mount
crossings, multiply-linked regular files, entries owned outside the pinned
home's owner, and bounded-resource violations. Descendant mode bits remain
opaque workload state behind the exact mode-0700 root. While App Server is live its rollout and database
namespaces are legitimately concurrent; the provider-neutral bridge therefore
reasserts owner-only access on the exact pinned root at every IPC boundary
instead of claiming a stable subtree snapshot. RyeOS-owned paths such as the
compatibility seed still require exact non-link types and atomic reset before a
credential-bearing process generation is released.

For pinned Codex 0.147 the `on-request` approval policy is inherited from
immutable CLI configuration. Request-level `approvalPolicy` is intentionally
omitted because the stable App Server rejects a granular field unless the
forbidden `experimentalApi` capability is enabled, while the pinned exec
boundary rejects explicit sandbox escalation when the immutable policy itself
uses the granular variant. Supported approval requests can therefore become a
durable RyeOS approval request. Every retained App Server approval class is
nevertheless `deny_only`:
RyeOS can deliver decline/cancel, but an accept decision is refused before
upstream contact. Supporting accept later requires an upstream request class
whose accepted effect is proven to remain inside the identical frozen
permission profile.

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

Use two deliberately separate qualification environments:

- package, clean-install, schema-cutover, and adversarial security tests use a
  disposable node so destructive setup and fault injection cannot damage
  lasting operator state;
- the real remote activation qualification uses the durable dedicated hosted
  target that will retain its node identity, credential profile, private home,
  external-content bindings, and session state across ordinary daemon
  restarts.

Neither tier should casually mutate the developer's primary interactive node.
Run the following matrix in the tier appropriate to the behavior under test,
and prove:

- remote configured-operator acceptance only with the exact admitted
  source-node co-signature, rejection of a missing/wrong-site proof, another
  key, a plain local-client grant, and local-only operator APIs;
- bidirectional handoff peers use exact remote-node placement/closure/follow
  scopes, never configured-operator transport for autonomous internal jobs;
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

The 2026-08-29 epoch-17 reference qualification exercised two independently
activated dedicated nodes and independently authenticated matching credential
subjects. A real configured-operator Codex placement moved from the hosted
target back to the source and then to the hosted target again, retained the
same upstream workload thread, survived daemon restarts, rejected stale
placement control, and completed candidate validation plus explicit
publication. A separate graph-followed Codex child moved cross-site, explicitly
discarded its candidate, recovered terminal delivery across target and parent
restarts, and appended exactly one delivery, graph completion, and parent
completion. No token/profile home or absolute realization path crossed nodes.
This is reference evidence for those exact paths, not a substitute for running
the remaining fault-injection matrix on a new release target.

Environmental inability to run a probe is not passing evidence and does not
justify changing the live local installation.
