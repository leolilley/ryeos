# Lossless named-product acceptance

The independent [unknown-output extension](unknown-output/README.md) adds a
same-verifier/two-manifest/two-root-slot fixture. The fixed-pin acceptance below
remains separate; neither fixture's source checks count as node-backed E2E.

The pure response-assertion regressions run with
`python3 -B tests/e2e/environment-products/test_response_assertions.py`.
Replay acceptance compares the complete accepted-product result, including its
authority, rather than treating equal witness hashes alone as sufficient.

This neutral fixture exercises the admitted producer-to-consumer path without
introducing a build kind, candidate-specific service, provider account, or
database seed. The producer is an ordinary signed Config, Graph and Bash Tool.
The current consumer is a v6 worker environment with one complete two-slot selection:
the exact qualified runtime executable plus its independently selected
distribution tree.

## Two-node portability phase

`run-portability-acceptance.sh` reuses the generic `local-node-v2` admission
and configured-remote import owners; it does not copy CAS files or publish a
parallel export head. Its fresh phase takes exact accepted-result evidence from
a completed fixed producer on node A, admits each witness as a CAS root,
receives both witnesses through node B's already-pinned remote, then imports,
executes the fixed verifier, qualifies, and composes the two received sources
on B.

After stopping A and restarting only B, run it with
`--phase after-restart --resume-from <fresh-evidence>`. This phase makes no
origin call. It authenticates the unchanged receiver identity and exact saved
acceptance hashes, reuses the retained verifier terminal, and requires the same
qualification, D0/D1 identities, and two binding hashes. Both phases require
new evidence directories and explicit node URLs, app roots, binaries, and
project snapshot hashes; the script never starts or stops either node.

The fixed large-content manifests contain ordinary CAS blobs, so admitted-root
transport suffices here. This does not claim transfer support for an actual
`LargeObjectStore` sidecar.

The recipe reserves two non-overlapping workspace output roots:

- `products/distribution`, containing the `distribution` product and its
  nested `runtime` product;
- `products/build-scratch`, captured for continuation/recovery but excluded
  from both product witnesses.

The runtime preserves an executable `bin/program` as portable mode 0755, the
contained relative symlink `current -> bin/program`, and a truly empty child
directory. The producer writes `distribution/NOTICE` as 0640 and scratch state
as 0600 so the source fixture checks its intended inputs; lossless capture
normalizes those non-executable files to portable mode 0644. RyeOS does not
claim to persist arbitrary host owner/group metadata.

The generated distribution manifest is canonical
`1e3ec95d5f220c500598907f7647dd0a08dddf731fdf93da5be1bbc07b36f7b3`.
It contains the normalized 0644 `NOTICE`, empty `licenses` directory, and the
complete nested runtime tree. The nested runtime manifest is canonical
`94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881`.
Its executable is 9688 bytes with blob identity
`3a55c0c7914fdeccc41ac91789116675ca07befdc1c64d8c4b1b77c4b7296d77`.
The signed recipe and fixed-pin verifier both commit that manifest.
The direct opaque verifier returns the raw `ProductQualificationResult` on
stdout, not a managed-runtime completion envelope. The daemon owns its
terminal status and thread coordinates independently.

## What the source contains

`project/` contains the five project definitions to sign in one disposable
project generation:

```text
config:test/two-products
config:test/runtime-consumer
tool:test/produce
graph:test/two-products
graph:test/recorded-producer
```

`graph:test/recorded-producer` is the ordinary Contract E wrapper. It owns one
inline `effects: recorded` action targeting `graph:test/two-products` through
its declared execute capability. It has no product recipe or output partition
of its own, and authors no effect id, controller, service, follow, or detach
mode. The awaited producer returns the typed accepted-product result.

`qualification-bundle-overlay/` contains two explicit unsigned Bundle overlays:

- `standard/.ai/` owns `config:test/runtime-qualification` and its fixed-pin
  `tool:test/verify-runtime`;
- `codex/.ai/` owns `worker:fixture/enrollment`, which uses a separately granted literal pin for
  the public credential-enrollment ceremony;
- `worker:fixture/hosted`, which receives the qualified runtime and unqualified
  auxiliary distribution only through one complete
  `config:test/runtime-consumer` selection batch;
- `worker_execution:fixture/login` and
  `worker_execution:fixture/session`.

Both Workers use the existing packaged
`bin:core/ryeos-structured-session-bridge`. That is binary packaging only:
their definitions and worker-executions live alongside that binary in the
disposable Codex Bundle, not in Standard with an undeclared cross-Bundle edge.
the fixture never selects `worker:codex/hosted`, a Codex profile, Codex
credential state, a provider route, or network access. The admitted product
program implements the finite JSON-RPC methods locally and reports
`network_contacted:false`.

The worker source closure is not represented by a hand-written digest.
`prepare-qualification-bundle.sh` computes the canonical
`ryeos.source_closure_manifest` over the copied regular files and substitutes
the exact digest into both unsigned Worker definitions before publisher
signing.

## Offline fixture check

Run:

```text
tests/e2e/environment-products/check-fixture.sh
```

It copies the fixture into a temporary directory, executes only the producer's
literal Bash body, checks the filesystem shape/modes/symlink/empty directory,
checks the exact executable bytes and canonical manifest, populates the
unsigned overlay, checks the Worker source digest and closed route shape, and
checks the recorded Graph wrapper. It does not sign, publish, install, launch a
node, write CAS state, or claim live acceptance.

The producer deliberately declares `filesystem_authority: node_policy`: its
ordinary admitted `local_binary` Bash command invokes literal `/usr/bin`
helpers from the publisher-selected node policy while retaining isolated
network authority. The generated product executable is instead the
checked syscall-only Linux x86-64 ELF in `runtime-program.b64`; it has no
shebang, dynamic loader, libc, PATH, or host executable fallback. Its reviewed
source and exact reproduction command live in `runtime-program.c` and
`rebuild-runtime-program.sh`.

## Populate a disposable enforced node source

The product and verifier use `mount_root: execution_runtime`, and the verifier
uses the admitted `realization_member` command owner. They therefore require
enforced isolation. Of the existing publisher-authored profiles, `development`
is the enforced profile and its exact bundle set is `full`. Do not use the
disabled `hosted-workflow` profile for this run and do not patch a live policy.

First copy and validate the unsigned overlay using the exact observed manifest:

```text
tests/e2e/environment-products/prepare-qualification-bundle.sh \
  94c66e97825a25091abe924ae3de4715d26469332aa59f31cfb39854bdb59881 \
  <new-overlay-directory>
cp -a <new-overlay-directory>/standard/.ai/. \
  <disposable-repository-source>/bundles/standard/.ai/
cp -a <new-overlay-directory>/codex/.ai/. \
  <disposable-repository-source>/bundles/codex/.ai/
```

Then use the repository's ordinary publisher flow. It must sign the complete
source and stage the full existing binary set, including the generic bridge:

When replacing an older disposable fixture source, remove only its obsolete
`bundles/standard/.ai/workers/fixture` and
`bundles/standard/.ai/worker-executions/fixture` copies before repopulation.
The preparation checker refuses those stale duplicates. This changes no
installed node or production Bundle ownership.

```text
(cd <disposable-repository-source> && \
  ./scripts/populate-bundles.sh --key <publisher-key> --owner <test-owner> \
    --bundle-set full --all)
tests/e2e/environment-products/prepare-disposable-node-source.sh \
  <absolute-disposable-repository-source> <new-absolute-node-source>
<matching-ryeos> init --non-interactive \
  --app-root <absolute-disposable-app-root> \
  --source <new-absolute-node-source> --node-profile development \
  [--trust-file <publisher-trust-document>]
```

`prepare-disposable-node-source.sh` verifies that all six overlay definitions
are publisher-signed, copies only the existing closed `full` set and shared
init authority, and performs no signing or node lifecycle operation. Sign the
five project definitions through the new node's ordinary project signing flow;
do not copy signatures from another generation.

## Live acceptance

Once that disposable node is already running with a configured local operator,
run the bounded harness with one new explicit profile id:

```text
tests/e2e/environment-products/run-live-acceptance.sh \
  --ryeos-bin <absolute-matching-ryeos-binary> \
  --app-root <absolute-disposable-app-root> \
  --daemon-url <explicit-disposable-daemon-url> \
  --project <absolute-signed-fixture-project> \
  --consumer-project-snapshot-hash <exact-signed-project-snapshot> \
  --credential-profile-id environment-product-fixture \
  --evidence-dir <new-absolute-evidence-directory>
```

The script never initializes, starts/stops, signs, populates, installs, or
discovers a global latest object. Every response is retained. Every subsequent
operation uses an exact id returned by the preceding invocation, the explicit
consumer snapshot, or the explicit profile id.

This fixture intentionally signs the producer and project Config consumer in
one generation. `--current-head` admits the producer from that generation, and
the harness requires its accepted result to name the same explicit snapshot
passed as the composition context; it does not claim cross-generation recipe
or consumer compatibility. Composition uses the current complete batch DTO.
The consumer declares two non-overlapping slots from the same D0:
`distribution` at project mount `fixture-distribution`, with no qualification
policy, and `runtime` at execution-runtime mount `runtime`, with its independent
qualification. The single selected D1 and both ordinary manifest bindings must
all succeed before the harness proceeds; one ready slot is never reported as
partial readiness.

The finite sequence is:

1. Run the recorded producer twice and require the same ordered distribution
   and runtime witnesses for the equivalent action coordinate. The replay receipt must say
   `source: effect_record` and `publication: not_applicable`, name the first
   immutable record, and the exact replay wrapper chain must contain no child
   thread or edge.
2. Import a fresh stage, bind the exact runtime manifest to the installed verifier,
   execute its realization member, publish qualification from its exact
   terminal coordinates, and compose the exact consumer generation with the
   ordered `{distribution,runtime}` selector batch. Require two distinct
   ordinary bindings with the exact distribution and runtime manifests.
3. Import another fresh stage and bind the same manifest to the independent
   enrollment Worker. A witness is not reused as a consumer grant.
4. Use only public credential services to create the profile, start the
   offline enrollment, and observe the exact finite account projection. Read
   each exact root/placement/sequence through the command-observation service
   and require an authoritatively settled non-turn command with no completion
   fence. Cancel and reap the enrollment session without inventing a turn,
   then confirm that exact login epoch/account digest. No row is preseeded.
5. Launch the session WorkerExecution with the exact `environment` ref binding
   and both `{declaration_id,witness_hash,witness_source,qualification_hash}` selections. Its generic
   bridge opens `program` from the sealed admitted runtime realization. The one
   `session.run` command must return the exact offline marker. Its exact
   command observation must prove the completed turn and supply the completion
   fence used to finish the retained-COW session; its candidate snapshot is
   then read by root id and explicitly discarded.

The qualifier request contains only witness, relationship and verifier
coordinates. It does not accept caller claims, manifests, paths, policies, or
evidence. The session selector contains only the declaration id and exact
published witnesses. No live project fallback or fixed `/ryeos` string is
passed by the caller.

## Acceptance boundary

Do not report full E2E success unless the node-backed harness actually ran.
The fixture is expected to fail closed for wrong owner/node/current head,
changed terminal capture or partition, failed/continued producer, altered
manifest bytes/modes/symlink, wrong policy/claim/verifier terminal, wrong
consumer generation, or selector drift.

The fixed known manifest is test-specific. General build output remains
unknown before capture; this first qualification lane deliberately uses a
literal fixed-pin, installed-Bundle verifier and one exact required claim. It
does not demonstrate dynamically built verifier selection, qualification
policy migration, or cross-publisher qualification. The witness is durable
testimony, not a consumer grant or reusable build-cache answer. Fresh import
stages and existing binding authority remain required for each verifier/worker
consumer.
