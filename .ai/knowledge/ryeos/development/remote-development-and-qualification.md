<!-- ryeos:signed:2026-09-01T22:25:21Z:2cb77de884287c30e7dd9427111a8c5198199dda5b85c660f14d33113e05932e:YA90kh3bhegTKh2gZfj7UGwwRa3Q/ttgrZQ/RFXbPquqF60tvBOVTXcpbOPSgmDOnOQIUeKO6frQcnSktnEKAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "remote-development-and-qualification"
title: "Remote Development and Qualification Runbook"
description: "Use an operator-controlled stronger host and an ordinary configured RyeOS remote without adding a deployment or scheduling substrate"
entry_type: implementation_guide
version: "1.0.4"
```

# Remote Development and Qualification Runbook

This is the generic workflow for using a stronger machine to build and test
RyeOS and then qualify a workload through RyeOS's existing remote boundary. It
deliberately separates host administration from authenticated RyeOS runtime
operations.

The source workstation owns the source commit, review, and final integration.
The stronger target owns its checkout, build cache, disposable qualification
roots, and retained non-secret evidence. Each app root is one ordinary RyeOS
node. A target is selected by the operator as one named configured remote; it
is not discovered by a registry or scheduler.

## The boundary

```text
source operator                       stronger target operator
  chooses exact commit  ----------->    creates an ordinary checkout
  reviews returned commit/artifacts     builds and runs focused tests
                                         installs/starts a disposable node

source RyeOS node                     target RyeOS node
  configured remote + pinned ID  --->   exact node/operator grants
  full-project push/execute/pull        target-local execution and caches
  source-local job transcript      <---   result; signed receipt when supplied
```

RyeOS remote execution is not arbitrary host-shell access. It can execute
admitted signed RyeOS items against an exact pushed project generation, but it
must not be used to install packages, clone RyeOS, invoke unrestricted shell
commands, replace node binaries, or restart the host. Use an existing
operator-controlled transport for that first layer: a pinned CI runner, SSH,
or a cloud provider's console/agent. That transport and its credentials are an
external deployment decision, not a new RyeOS runtime capability.

Do not make the workstation's primary node the experiment. Use fresh source
and target app roots when qualifying lifecycle behavior.

## Existing RyeOS authorities to reuse

No daemon API, node-instance registry, `nodes/` hierarchy, shared filesystem,
scheduler, migration layer, or workload-specific app root is needed.

| Need | Existing authority |
|---|---|
| Select and pin a target | `ryeos remote configure`; stored named remote pins URL, node principal/key, canonical site ID, vault fingerprint, and ingest-ignore policy |
| Bootstrap trust | target-local `admission-token` plus source `remote admit`, or target-local exact `authorize-key`/`authorize-client` |
| Preserve a durable operator principal and origin | explicit configured-operator forwarding for `service:remote/push` and retained-current-HEAD `service:remote/run`, with target `remote_operator` grant and source-node co-signature |
| Transfer a complete project | a `full_project` binding and the typed snapshot/CAS closure used by `remote execute` |
| Execute on target compute | target-local admitted item execution through the configured remote |
| Return project changes | clean-base guarded pull/apply from the result snapshot |
| Install target capabilities | operator-controlled target package/image or stopped-app-root bundle activation from an exact signed artifact |
| Observe recovery | authorized source-local sync job list/inspect and exact remote thread/launch status surfaces |
| Reuse expensive content | host-operator Cargo caches plus target RyeOS CAS, per-snapshot engine, bundle, and managed-realization caches; these are separate authorities |

Ordinary `remote execute` authenticates as the source node and is the generic
synchronous full-project push/execute/pull operation. Configured-operator
continuity is a narrower, explicit mode for durable push/run workflows; it is
not selected implicitly and should not be simulated by sharing keys or editing
grants. Choose the mode from the ownership contract of the workload.

There are two supported execution shapes:

- use synchronous `remote execute` for bounded builds, focused tests, and
  model probes whose project results must be pulled back automatically;
- use configured-operator push plus an accepted, retained-current-HEAD
  `remote run` for long jobs where durable target-local thread/result evidence
  is enough.

The second shape does not currently provide recursive retained-project-result
materialization back to the source. `remote pull` can fetch caller-known typed
object/blob hashes into a new directory, but it is not a `pull-result` command.
Do not promise automatic artifact download for accepted jobs or bypass this
gap with an implicit shared filesystem.

`remote bundle-install` is remote-to-caller import: the caller fetches an
installed bundle from its named remote into the caller's live node. It does
not publish or activate a candidate on the stronger target. Target candidate
activation stays in the target operator's stopped-node package/image/bundle
cutover.

## Reproducible host preparation

Before creating a real cloud machine, decide all external inputs:

- provider/host and spending limit;
- machine architecture, CPU/GPU/device profile, RAM, disk, and network egress;
- base image and access mechanism;
- operator key custody and target node policy;
- model/artifact URL, hash, license, and redistribution permission when a real
  model workload is in scope;
- evidence retention location and deletion period.

Stop before provisioning if any choice or credential is missing.

On an already-provisioned stronger host, use an ordinary checkout outside the
target app root and pin the source commit:

```bash
git clone <operator-approved-ryeos-origin> ryeos
cd ryeos
git fetch --tags <operator-approved-ryeos-origin>
git checkout --detach <exact-commit>
test "$(git rev-parse HEAD)" = "<exact-commit>"
test -z "$(git status --porcelain=v1)"
```

Keep caches target-local and explicitly bounded. They are performance state,
not execution identity or transfer authority:

```bash
export CARGO_TARGET_DIR=/var/tmp/ryeos-qualification/cargo-target
export CARGO_HOME=/var/tmp/ryeos-qualification/cargo-home
export GATE_BUILD_JOBS=<bounded-jobs>
export GATE_TEST_THREADS=<bounded-threads>
```

These variables describe the host-controlled build/test layer. A pushed
project does not carry `target/`, and a default RyeOS execution sandbox does
not gain access to `/var/tmp` merely because the variable names it. If a build
is itself run as an admitted RyeOS item, its node-local cache path must be an
explicit target deployment/isolation contract; otherwise use a disposable
project-local build and make no persistent-cache claim.

Record toolchain versions and every command's exit status in an evidence
directory outside the checkout and both app roots. A typical build and focused
test sequence is:

```bash
rustc --version --verbose
cargo --version --verbose
cargo build --release -p ryeos-cli -p ryeosd
cargo nextest run -p ryeos-state project_sync
cargo nextest run -p ryeos-api --test remote_descriptor_admission_e2e
cargo nextest run -p ryeos-api --test remote_import_e2e
```

Use `./scripts/gate.sh` for the final source gate. Use
`./scripts/gate.sh --refresh-bundles` only when signed/bundle-owned content was
changed. Preserve the exact resulting `ryeos` and `ryeosd` SHA-256 hashes and,
when applicable, the signed bundle artifact checksum. Do not copy a random
binary into an existing target node or reinstall the developer's primary node.

## Disposable node lifecycle

For local two-node qualification, create independent target-local directories:

```text
qualification/
  source-home/       source-node/       source.sock
  target-home/       target-node/       target.sock
  source-project/    target-projects/
  evidence/
```

`source-project`, `target-projects`, and `evidence` must be outside both app
roots and synthetic homes. Start each ordinary node with its own `HOME`, app
root, UDS path, and loopback listener. Use `ryeos node status --json` to record
the actual bind address and stable identity. Never point either lifecycle
command at the primary node's app root.

On a real remote host, install the already-qualified candidate through the
host operator's normal package/image mechanism, initialize exactly one target
app root, and run it under the normal supervisor. Candidate installation and
host restart remain operator actions outside the RyeOS remote API.

## Authentication and site identity

Configure the live target from the source, preferably from a separately
delivered descriptor trust pin:

```bash
ryeos remote configure --descriptor ./stronger.remote.yaml
ryeos remote status stronger
ryeos remote doctor stronger
```

The descriptor itself pins only the node signing key and its fingerprint.
Descriptor import verifies those pins against the live `/public-key` response,
then `remote configure` records the discovered principal, signing key,
fingerprint, canonical site ID, and vault fingerprint as one configured
coordinate. Non-loopback traffic requires HTTPS. Run `remote configure`
immediately before qualification and review any identity change out of band.
The stored endpoint is one canonical credential-free base URL; it contains no
query, fragment, user information, control/whitespace characters, or trailing
slash. Public audience discovery and signed requests refuse redirects, so use
the target's final origin directly.
`remote status`, `remote doctor`, and admission validate that complete tuple
before authenticated contact; a mismatch skips every signed status/project
probe, refuses to release an admission token, and fails the helper.

For an ordinary node-principal workflow, the target operator mints a short
lived one-time admission token with only the required scopes, or authorizes
the source node key locally. A full-project remote execution generally needs:

```text
ryeos.execute.service.objects/has
ryeos.execute.service.objects/put
ryeos.execute.service.system/push-head
ryeos.execute.service.objects/get
<the exact caps required by the executed item and its children>
```

Never grant wildcards. If the target must fetch a bundle from a source node,
authorize that target node on the publisher node for exactly:

```text
ryeos.execute.service.bundle/export
ryeos.execute.service.objects/get
```

For configured-operator continuity, stop the target and use the documented
offline semantic conversion. The target grant must bind the configured
operator key to the source's canonical `site_id` and exact workflow scopes;
the source node key separately receives only
`ryeos.attest.request.forwarded-operator`. The source node then co-signs each
exact operator request. Do not use this mode for `remote execute`, admission
claim, or arbitrary delegated callers.

## Project and workload qualification

Bind one clean source project to one absolute target-local project identity as
an explicit operator action:

```bash
ryeos --project "$PROJECT" remote bind-project stronger \
  --remote-project /srv/ryeos/projects/qualification \
  --sync-scope full_project
```

Then exercise a generic admitted item. The repository helper refuses a missing,
different, or non-`full_project` binding; it does not silently overwrite
operator configuration. It performs the full-project execution, pull-back,
fail-closed best-effort job correlation, exact expected-file transition checks,
and compact evidence retention.

The helper requires Bash, Python 3, Git, `realpath`, and GNU
coreutils/findutils. Before contacting the target it resolves every executable
used after pull-back to a regular path outside the synchronized project,
starts through absolute privileged-mode `/bin/bash` so inherited `BASH_ENV`
and shell functions cannot execute before preflight, clears loader and language
startup injection variables, closes `PATH`, and runs
isolated Python from the evidence directory. A project-local tool shadow is a
hard refusal, not a fallback. It performs no Git operation after the
target-controlled pull-back. Its preflight Git probes explicitly disable the
repository's `core.fsmonitor`, so a prior target-controlled pull cannot turn a
later retry into source-host execution. Qualification also requires an explicit
`RYEOS_APP_ROOT`: the helper canonicalizes and freezes that source node root,
proves its node/operator configuration and signing key remain outside the
synchronized project, and freezes the resolved source daemon URL before the
first RyeOS request. A locally discovered daemon is re-proved through the exact
app-root lifecycle authority before every client request; an explicit URL must
use HTTPS except for loopback. CLI audience discovery and every signed request
refuse redirects, so neither origin nor transport can move after this check.
`source-client-authority.json` retains only SHA-256 identities of the frozen
root and endpoint, never their literal values; the helper also strips remote
config paths from its retained list projection. Other operational and failure
artifacts remain raw and can contain endpoint details or absolute paths, so the
complete evidence directory is confidential. The helper never reproduces
RyeOS's platform-dependent default app-root discovery. The evidence parent must already exist
and be operator-selected outside both the checkout and source app root; the
helper creates only the new final evidence directory after those checks.
Invoke it as:

```bash
RYEOS_APP_ROOT=/absolute/source-node-root \
scripts/dev/qualify-configured-remote.sh \
  --remote stronger \
  --project "$PROJECT" \
  --remote-project /srv/ryeos/projects/qualification \
  --item-ref tool:qualification/run \
  --input /path/to/non-secret-input.json \
  --ref-binding model=worker:models/qualified \
  --expect-file "result.json=<sha256>" \
  --evidence-dir /var/tmp/ryeos-qualification/evidence/run-001
```

The helper assumes fresh configuration, an exact pre-existing binding,
authorization, node lifecycle, bundle
activation, and model realization are already complete. It intentionally does
not provision, install, start, stop, authorize, activate, or tear down nodes.
Its item and binding arguments are generic; provider-, model-, Codex-, and
local-inference-specific policy remains in the admitted project/bundle
contract rather than generic runtime or orchestration code.

The helper proves a functional source-node-principal round trip and the
integrity of its source-local operational transcript. The transcript is not a
target-signed qualification receipt. A stronger release/workload claim must
also retain and verify the workload's target-signed receipt, chain head, or
other exact authority defined by that workload. Do not weaken owner-scoped
chain APIs to manufacture that evidence for ordinary `remote execute`.

For a long-running accepted workload, use the configured-operator push/run
policy documented by the remote command reference, retain the returned launch
and thread IDs, and inspect only that exact thread with `remote thread-status`.
Treat the retained remote result/log/facets as the phase-one artifact surface.
If recursive project artifacts must return automatically, that requires a
separate generic owner-bound `remote pull-result` design and is not implemented
by this workflow.

For a model qualification, retain the exact signed worker/model refs,
realization receipt/artifact hash, device profile, deterministic prompt/input,
result, target thread ID, and settled tool/effect evidence. For an offline
replay claim, the target operator must disable acquisition/egress and restart
the disposable target between initial realization and replay; do not replace
that fault boundary with a process-local cache check.

## Evidence and recovery

Retain at least:

- source commit and clean status;
- host/toolchain/device profile and build/test logs;
- `ryeos`/`ryeosd` and bundle artifact hashes;
- source and target public identity/status documents and descriptor hash;
- the exact non-secret grant scope lists (never private keys, tokens, vault
  values, authorization files, app roots, or complete model caches);
- project binding and input hash;
- push, result, and pull snapshot hashes;
- source-side `remote_execute` job and attempt IDs plus terminal inspection;
- exact output file hashes and workload-specific signed receipts.

On success, `remote execute` returns the durable source-local `job_id`; the
helper inspects exactly that coordinate through
`service:sync/jobs/inspect`. Exact inspection, not list projection, owns the
canonical operation (`item_ref`, exact `ref_bindings`, target site, and target
project path) and the complete retained-attempt evidence.

The helper records a bounded `service:sync/jobs/list` view before execution
only so a lost command response can be investigated without guessing or
retrying. After an ambiguous transport failure it takes one bounded after
view, selects only newly visible `remote_execute` job IDs, and inspects each
candidate to compare its exact operation. The compact list deliberately does
not disclose canonical operations and is never an exact authority. Turnover
can make failure correlation incomplete, and multiple exact candidates remain
ambiguous. The helper fails closed in both cases and never reads SQLite.

These services require the local
`ryeos.execute.service.sync/jobs/list` and
`ryeos.execute.service.sync/jobs/inspect` capabilities.

Evidence is created with restrictive permissions, but raw status, doctor,
job, workload result, and error responses may contain confidential model
output, endpoint details, public keys, and absolute paths. Store it only in an
operator-approved location. `evidence.sha256` is an integrity checksum, not an
authentic signature or a substitute for target-signed workload evidence.

Failure handling is phase-specific:

| Failure | Recovery |
|---|---|
| Build/test interruption | keep logs; rerun from the same clean commit with the same bounded target-local cache |
| Remote identity mismatch | stop; verify the expected rotation out of band, then explicitly reconfigure/re-authorize |
| Synchronous push/execute transport failure | retain the before/after source job views and inspect a unique new job when one is visible; the last source phase may be knowable but remote acceptance/completion may remain ambiguous, so do not retry automatically |
| Accepted `remote run` transport failure | use only the caller-retained launch ID and exact returned thread coordinate for status/cancel/recovery; never guess ownership |
| Pull clean-base conflict | preserve both snapshots and evidence; restore or commit the source worktree deliberately, then rerun from a clean base |
| Bundle install/cutover interruption | follow bundle transaction reconciliation and the normal stopped-app-root/supervisor restart or rollback procedure |
| Managed external-content activation interruption | inspect and recover its durable activation sync job; do not reinstall around it |
| Target loss | keep source checkout and retained external evidence; rebuild an ordinary target from the pinned commit/artifacts and reissue exact grants |

## Teardown

Teardown is an operator-owned lifecycle operation after evidence has been
copied and verified:

1. stop only the disposable source/target nodes by their explicit app roots;
2. revoke/remove target grants and one-time tokens through the supported local
   operator procedure;
3. remove the named remote/project binding if it was temporary;
4. delete disposable app roots, project materializations, and caches according
   to the declared retention policy;
5. terminate an external machine only through the chosen provider after cost
   and evidence checks.

Never recursively target a home directory, repository root, primary app root,
or unresolved environment variable.

## Relationship to portable worker placement

Ordinary named-remote build/test qualification and synchronous full-project
`remote execute` remain separate from hosted-worker placement. Cross-site
worker continuation already has its own signed transfer/adoption contracts,
source fencing, successor chain-writer authority, recovery state machines, and
crash matrix. This development helper consumes the same configured-remote and
site identity boundaries but must not duplicate, reinterpret, or bypass those
placement authorities.
