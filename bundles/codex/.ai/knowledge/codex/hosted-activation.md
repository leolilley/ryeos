<!-- ryeos:signed:2026-09-17T04:06:11Z:9c8ffce90f0bb1a86d10dca1165b6af80c3c48bccf924e6fec75c32702cce5bb:ymegtqeU5P9vIIAVJPmIt7XqLfDhpXAXemefBVfcsJ5v68xRU4Crbj6W/Gy0nMSljcmD+983wPknSlsH7V+CDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: codex
tags: [codex, hosted-execution, structured-session, credentials, acceptance]
version: "1.10.2"
description: >
  Activation, credential ceremony, command routes, and release acceptance for
  the pinned Codex structured-session workload.
---

# Hosted Codex activation and acceptance

The pinned `thread/settings/updated` notification is explicitly schema-validated
and correlated to the bound upstream session. Its durable event retains only
the bounded thread/model/provider identifiers and a settings digest, not raw
paths or instructions. It is workload testimony, not a change to RyeOS grants
or lifecycle authority. Turn-level model/effort selection can emit this event
before the start response; it must not be mistaken for an unknown protocol
message. Other undeclared notifications remain fail-closed.

The worker-environment v6 sources explicitly set `external_product_slots: []`.
These installed bundle environments retain literal pins; dynamic product slots
are limited to separately admitted pinned-project environment Configs.

The Codex bundle hosts the pinned Codex App Server using ChatGPT subscription
authentication managed by Codex. It does not route Codex through RyeOS local
inference. The executable, its same-version code-mode host, its packaged
model-command runtime resources, and App Server schemas are pinned by
activation and source closure.

The signed Codex worker profiles do not claim a finite RyeOS `spend_usd`
allowance. The worker-execution runtime has no provider financial authority,
and Codex does not expose authoritative ChatGPT-subscription charges to this
contract. Unattended execution can still use the shared whole-tree duration,
logical-worker, and bounded hosted-turn contact ceilings; none of those is
presented as subscription billing evidence.

This is installed operator knowledge shipped by the Codex bundle. It documents
how to activate and accept that optional integration; it is not a RyeOS
repository-development workflow. The provider-neutral authority and lifecycle
contract beneath it is documented by
`knowledge:ryeos/core/execution/worker-hosted-execution` in the Standard
knowledge bundle.

## Activation

Before creating another login, discover the existing profiles on the selected
node through the CLI:

```sh
ryeos codex profile list
ryeos codex profile get <profile-id>
```

The list is restricted to the authenticated operator and contains profile IDs,
lifecycle state, credential generation, whether the profile is in use, and
creation/update timestamps. It does not read private homes or expose account,
login, or token contents. An empty `profiles` array means this owner has no
registered profiles on that node; it says nothing about another node or the
ordinary Codex CLI login. Deleted profiles are omitted. The default page size
is 50 (maximum 200); pass `--limit` and then `--after <next_cursor>` for further
pages until `next_cursor` is null.

This is the generic owner-authorized `service:credential-profiles/list` surface.
A configured remote operator must have the explicit
`ryeos.execute.service.credential-profiles/list` capability and use the normal
configured-operator forwarding route to inspect profiles on a remote node.

1. Publish the `hosted-workflow` set containing `core`, `central-auth`,
   `standard`, `hosted-node`, and `codex`. Generic worker-execution runtime/preparer
   binaries belong to `core`; the generic knowledge kind belongs to
   `standard`; bridge/profile and all Codex-specific data belong to `codex`.
2. Use the installed profile's complete node-owned policy generation. Only
   when changing its values, prepare replacement members outside the live node
   namespace and apply them with the stopped-node command below. Managed
   activation requires no named filesystem
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
     enabled: true
     limits:
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
   enabled: true
   limits:
     max_pool_groups: 4
     max_total_processes: 4
     max_total_address_space_bytes: 68719476736
     max_total_cpu_seconds: 14400
     max_real_uid_process_limit: 4096
     max_open_streams: 32
     max_active_streams: 4
     max_active_streams_per_subject: 1
     max_stream_backlog_bytes: 16777216
     max_total_backlog_bytes: 67108864
   ```

   A fresh `hosted-workflow` or `full` installation selects a
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

   `config:codex/environments/default` uses the closed
   `ryeos.worker_environment.v6` contract. Its `executable_search` contributes
   only the exact activated command-tool tree to `PATH`. Its independent
   `process_environment` map may contribute bounded literals, paths inside an
   explicitly named activated realization, or directories below the
   session-owned `.ai/cache/ryeos-runtime` view. RyeOS freezes those values in
   the persistent-session capsule and resolves target-local paths only at
   placement. The complete retained map is limited to 32 entries and 4096
   serialized bytes. Values are never inherited from the daemon, read from the Codex
   credential home, or encoded as authored absolute paths. The pinned base
   environment currently leaves `process_environment` empty and its required-
   nullable `workload_client` field is `null`. A separately promoted
   development environment selects structured-session invocation and its finite
   execution request; no unused restricted CLI realization is required.
   Toolchain/dependency content
   remains owned by each requested child tool rather than being borrowed from
   the container image or added to the outer Codex process.

   The authoring profile uses pinned App Server `dynamicTools` registration and
   `item/tool/call` delivery. Its signed initialization explicitly enables the
   pinned `experimentalApi` capability; this is a vendor protocol requirement,
   not a separate RyeOS bundle or inference backend. The exact 0.147.0 binary's
   credential-free registration probe passed; that alone is not model execution
   or installed development-loop evidence. `ryeos_execute` accepts the existing
   bounded execution request; the daemon retains and revalidates authority.

   The minimal hosted profile disables Codex's Code Mode host. The authoring
   profile enables the exact pinned same-version host for Codex's ordinary
   read/edit/shell tools, while model-authored JavaScript Code Mode remains
   disabled. This host is not a RyeOS execution authority: finite child RyeOS
   operations are still registered as direct dynamic tools and revalidated by
   the daemon. Model tool selection remains an acceptance property of the exact
   Codex/model/profile generation; a completed turn without an edit or child
   operation is not successful development evidence.

   Ordinary shell/editing commands remain Codex commands. The Codex profiles
   mount their executable resources and command tools in the existing immutable
   execution-runtime namespace, not under the editable project. The minimal
   hosted profile admits only those inputs through its nested sandbox. The
   unattended authoring profile instead selects `danger-full-access`; a
   hard-contained node relies on its qualified Lillux process/filesystem
   boundary, while a trusted disposable node explicitly accepts the broader
   container boundary. Runtime mountpoint setup must not become source edits in
   a frozen candidate; inspect the complete candidate diff independently of the
   task-specific evaluator.
   Moving these mounts changes the admitted worker program and capsule, not
   installed-bundle literal binding identity (consumer ref and publisher).
   Revalidate exact retained manifests, bindings and current grants under the
   new signed source; do not infer that unchanged bytes need acquisition or
   that an old running capsule silently adopts the new program.

   The Codex profiles no longer expose `/tmp/.ryeos-wc` or its endpoint
   variable. Native CLI ingress remains a separate RyeOS interface. Neither a
   minimal-profile sandbox probe nor an authoring command from a trusted
   placement establishes additional RyeOS authority.

   `turn.start` selects `turn/started` as request-correlated early progress.
   Its signed `/message/params/threadId` correlation must match the bound
   thread before the bridge emits progress; another thread cannot establish
   the active command's turn authority.
   The generic wire-v2 progress acknowledgement precedes dispatch of a dynamic
   tool request and publication of later turn notifications. The tool facade
   has the exact name `ryeos_execute` and an absent/null namespace; another
   namespace is refused. Registration uses the pinned experimental schema's
   `type: function`. Vendor registration at fresh start is proven separately
   from its restoration and actual callback execution after resume.
   The authoring environment supplies `TMPDIR` through the existing typed
   runtime-view directory contract. Its profile requires that variable before
   workload launch and directs ordinary temporary output to that scratch
   directory. This is deterministic environment selection, not a filesystem
   confinement claim for `danger-full-access` authoring commands.

   Both signed Codex profiles pass only PATH, selected locale/terminal
   variables, authored GIT_CONFIG_NOSYSTEM/GIT_CONFIG_GLOBAL/GIT_PAGER settings,
   and authoring's declared `TMPDIR` into shell
   children. They filter the bridge's admitted environment with a finite
   include list. HOME, CODEX_HOME, credentials, proxy configuration and
   other RYEOS variables are not child environment. Protocol invocation exposes
   neither an operator CLI endpoint nor a new credential. See the
   [Codex shell-environment semantics](https://learn.chatgpt.com/docs/config-file/config-advanced#shell-environment-policy).
4. Keep the source operator private key at its operator endpoint and the
   hosted node's independent local operator private key at the hosted node.
   First admit the source node key on the target
   as `remote_node` with only
   `ryeos.attest.request.forwarded-operator`; this key co-signs the exact
   configured-operator request and proves source-node transit. Then stop the
   hosted daemon and use its local operator to run the supported command
   `RYEOS_APP_ROOT=<hosted-root> ryeos authorize-client --public-key
   <source-operator-raw-base64> --origin-site-id site:<source>
   --scopes <exact-scopes>` on the hosted node. A fresh grant needs no
   semantic-conversion flag; an intentional reclassification or origin change
   of an incumbent grant does.
   Use the complete exact scope set printed in the Codex bundle README. The
   target-signed `remote_operator` grant constrains which source site may
   forward the operator; it is not transit proof without the separate
   source-node co-signature. A plain `local_client` grant is not acceptable,
   and ordinary remote-node grants remain node principals that cannot own this
   workflow.
   Handoff preflight captures that exact grant's class, origin, signed body
   digest, and canonical scopes as `AdmittedOperatorAuthority`. Preparation
   proves it covers the target capsule's effective and parent-delegation
   capability ceilings and seals it into the target capsule and placement.
   The target revalidates the same grant before publication and private-state
   installation, including recovery. Revocation, scope change, or equivalent
   re-authoring after preflight fences the handoff; only the exact original
   signed grant can resume runnable recovery after an already committed source
   cut. Once a target-signed terminal settlement exists, replay of that exact
   immutable receipt remains source-node-authenticated but is independent of
   later grant changes; it cannot launch a worker or reopen credential-private
   authority.
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
   Once target preparation reserves the credential generation, the same
   durable reservation fences the exact configured-operator project HEAD.
   Snapshot, push, reconciliation, fold-back, and compact-GC writers cannot
   change it before adoption. Abort releases both reservation authorities;
   adoption releases the HEAD fence only after the target branch is
   authoritative, so restart cannot expose private state against a substituted
   project generation.
7. Resolve digest-fenced pending approvals. This release exposes bounded
   command/cwd for review but makes command-execution, file, and permission
   requests deny-only. Accepting an upstream sandbox-escalation request could
   widen the immutable permission ceiling and is therefore not admitted.
8. Complete work, validate the frozen candidate, then publish or discard.
   `terminate` accepts only `reason: completed` or `reason: cancelled`.
   `completed` freezes a project session and exposes its candidate;
   `cancelled` terminalizes without a checkpointable placement. A portable
   checkpoint is therefore captured only after `completed`. For a cross-site
   move, wait for `frozen`, publish that checkpoint, and only then run
   `handoff-preflight` and `handoff`; the preflight is bound to the resulting
   immutable source chain head. `resume` conditionally restores that manifest
   into a fresh placement before its worker is released.

External-content maintenance after activation uses the hosted node's own local
operator, not the forwarded source operator. Finish or terminate hosted
executions and invoke only the required local maintenance scopes
(`external-content/activate` plus `release` or `scrub` only when that operation
is actually required). Same-class scope replacement is an atomic node-signed
grant update and is hot-reloaded; an actual class or origin transition requires
explicit stopped-node semantic-conversion authority. The source
`remote_operator` grant and source private key are untouched. Never use
`--merge-scopes` across an actual class or origin transition.

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
After `turn.start`, retain the returned placement-local `command_sequence` and
`placement_thread_id`. `ryeos codex session command-observation` resolves that
exact coordinate after the transient session status has returned to idle and
returns the immutable turn-completion fence. Passing that fence as
`completion` to the generic terminate service prevents a later command from
being mistaken for the turn the caller intended to complete. A daemon-owned
reattach may advance the worker boot epoch after restart, but it does not
replace the owner-route frontier proved by that fence; placement and admitted
capsule must still match exactly.

`worker_execution:codex/bounded-turn` packages that same generic protocol for
one followed, unattended turn. Its signed routes and retry ceiling are fixed by
the profile; invocation input supplies the credential profile, the two typed
route payloads, and any separately admitted read-only evidence attachments.
Only a root-proved uncontacted command can move to the next attempt. Successful
completion freezes the private CoW result and terminalizes it as
`retained_for_review`, so a graph follow receives a reviewable candidate and
the exact command/turn/fence evidence without polling session status. This is
additional to `codex/session`, which remains the interactive owner-directed
session surface.

The daemon applies the same bounded admission checks to runtime callbacks and
owner commands: each route payload must match the sealed goal, attempts must
stay within the signed ceiling, and retries require exact uncontacted
predecessor testimony. New contact requires an approval-free idle session with
no unresolved observation projection. Approval ingestion and the durable
contact commit share a short transition gate; approvals accepted after that
commit are post-contact events, not permission to start another turn.

The creation-anchored worker lifetime and any aggregate execution deadline
survive restart and also bound queued worker writes. A pending command or an
already-reserved contact allowance cannot extend either deadline. Exact settled
replay remains readable after expiry; a possible-contact boundary without a
proven result remains unknown and is never resent.

## Mechanical policy boundary

The signed profile launches Codex with immutable argv containing every
security-critical override; those immutable arguments are the sole
configuration authority. A same-UID process can replace a file in its writable
home, so RyeOS atomically resets the mode-0400 compatibility seed before every
worker generation and never treats workload-authored changes as retained
policy. If the node enables a generic enforced isolation backend, RyeOS
additionally overlays that file read-only, but hosted Codex does not require
the node to enable RyeOS isolation. OpenAI's standalone package requires its own private
`codex-resources/bwrap` companion for restricted Linux command execution, so
the activation selects that exact file with the Codex executable, code-mode
host, Zsh, and `rg`. This is workload-owned pinned content: it does not select
RyeOS's isolation backend, discover a host `bwrap`, or acquire
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

For pinned Codex 0.147 the minimal hosted profile inherits immutable
`on-request` and the granular `ryeos-workspace-only` permission profile.
Supported approval requests can become durable RyeOS approval requests, but
every retained class remains `deny_only`: RyeOS can deliver decline/cancel and
refuses accept before upstream contact. The authoring profile instead pins
`approval_policy=never` and `danger-full-access`; request-level
`approvalPolicy` remains forbidden, so callers cannot weaken or replace either
baseline. Its signed `experimentalApi` capability enables dynamic tools only.

App Server inherits a cleared minimal environment and no RyeOS control FD.
When the selected signed environment contains a process-environment
contribution, the bridge deliberately resolves and installs only that
capsule-bound map after clearing inheritance; the structured-session protocol
authorizes the sealed relay by name. Codex cannot add variables or redirect a
realization/runtime-view path through request payloads.
Minimal-profile model commands receive the signed granular permission profile.
Authoring commands do not: `danger-full-access` can reach resources visible in
their enclosing execution environment and can use its network. A qualified
local-process-scope placement supplies the hard boundary outside Codex. A
trusted disposable placement makes no containment or credential-secrecy claim,
must contain no project signing, publication, submission, Kaggle, or deployment
credential, and must be recycled after its retained candidate is imported.
Codex's signed `session_resources` override is capped by the generic
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
  replacement for any real class/origin transition, and target-local
  maintenance without changing the forwarded source grant;
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
