# Unknown-output subject extension

This is an additional, unsigned fixture. It does not replace the parent
fixed-pin runtime/Worker acceptance. Nothing here has been installed or run
against RyeOS merely by adding these files.

The same installed Bundle verifier and policy qualify two different subject
manifests without changing a verifier digest, passing a manifest as a probe
parameter, or editing/re-signing the verifier between attempts. Each verifier
launch selects two Root declarations in one complete batch. The primary and
auxiliary files must form the bounded pair checked by the verifier; its result
computes the primary manifest from actual admitted file bytes using the current
ordinary File manifest representation.

## Exact input and authority boundaries

The producer recipe has two output-backed File products, no expected output
hash, and a finite subject/auxiliary inspection allowance. Both producer-to-
verifier relationships require no qualification. A separate subject-to-data-
consumer relationship requires the independent `bounded_payload_pair` claim.
That consumer is deliberately a data Config, not a claim that this tiny file is
an executable worker environment.

The two projects have byte-identical signed definitions and different regular
`variant.txt` inputs: `alpha\n` and `beta\n`. Each input must be captured in
its explicit pinned project snapshot. This is not relying on arbitrary live
filesystem state: `runner.rs::dispatch_subject_components_from_capsule` uses
`product_build_subject_digest` for an output-bearing producer, and that
existing Contract E identity commits the entire original
`producer_project_snapshot_hash` as well as its parameters, definition and
output partition. The harness requires distinct explicit snapshots and checks
each accepted result against the requested generation. It does not weaken
ordinary tool replay identity or add another input/cache lane.

The Bundle verifier retains InstalledBundle authority while its relationship
Config is resolved from the explicitly supplied pinned project context. Its
two slots are `auxiliary` and `subject`, each File-shaped and project-mounted.
Composition explicitly imports and binds both distinct manifests before
launch. The invocation carries only their exact witness coordinates and null
qualification hashes. The subsequent qualification request carries only the
existing witness/relationship/exact verifier root and terminal coordinates.

The producer alone uses the existing Bash local-binary lane and
`filesystem_authority: node_policy`; its literal `/usr/bin/mkdir` and `chmod`
remain explicit disposable-node prerequisites. The verifier and the Graph's
ordinary probe both select the same packaged
`bin:fixture-dynamic-product-verifier`, require `captured_execution`, and use
no host interpreter or utility. The executable is a fixture-only Rust program
using `sha2` and `serde`; it validates the bounded relationship and derives
both canonical File manifest hashes from the actual admitted bytes. Bundle
executor identity commits its exact executable bytes and publisher manifest.
Network authority is isolated. No model, provider, credential profile or
external network is involved.

## Preparation (separate authorized operation)

1. Generate and review the standalone verifier lockfile offline (once), then
   build one static artifact. Ordinary workspace builds do not include this
   fixture crate:

   ```text
   cargo generate-lockfile --manifest-path tests/e2e/environment-products/unknown-output/verifier-program/Cargo.toml --offline
   tests/e2e/environment-products/unknown-output/build-verifier-program.sh <new-absolute-binary>
   ```

   The build refuses a missing lockfile and uses `--locked --frozen --offline`.
   Its ELF check rejects `PT_INTERP` and `DT_NEEDED`.
2. Stage that one binary and the definitions into a new disposable Bundle
   overlay:

   ```text
   tests/e2e/environment-products/unknown-output/prepare-bundle-overlay.sh \
     <absolute-verifier-binary> <new-absolute-bundle-overlay>
   ```

   Merge that prepared overlay into the disposable publisher's `standard`
   Bundle source. Publish/sign its Tools, Graph and policies with the existing
   full Bundle population flow, then initialize/install that disposable node
   through the existing owner. The staging step never signs or installs
   anything.
3. Copy `project/` to a new disposable project A. Sign all its definitions
   using the ordinary project authoring flow and include them in its manifest.
4. Copy the signed authored project into a separate disposable project B.
   Change only B's regular `variant.txt` input to `beta` plus one newline.
   Do not carry runtime/state directories into this source copy.
5. Establish each project's exact pinned head through the ordinary snapshot
   owner. Record both distinct hashes; neither may omit its `variant.txt`.
   All authored definitions must remain byte-identical across the copies.

The fixture source lives under tests and is not admitted by default. Preparation
requires a disposable publisher/node and signed project generations; this
harness never performs those operations itself.

## Checks

The local body check is:

```text
python3 tests/e2e/environment-products/unknown-output/check-fixture.py \
  --verifier-binary <absolute-static-binary>
```

It executes the producer's extracted Bash body and the exact supplied verifier
binary in temporary paths, checks both computed manifests and a mismatched
auxiliary refusal, then proves the executable and Tool sources did not change.
It also rejects a dynamic ELF or undeclared verifier host utility, checks that
the ordinary Graph probe has no authored selectors, and checks the workflow's
exact-coordinate source shape. This is not Graph expression validation,
RyeOS admission or live E2E.

Once the explicit disposable node and source generations are ready:

```text
python3 tests/e2e/environment-products/unknown-output/run-live-acceptance.py \
  --ryeos-bin <absolute-matching-binary> \
  --app-root <absolute-disposable-app-root> \
  --daemon-url <explicit-disposable-node-url> \
  --project-a <absolute-signed-project-A> --snapshot-a <exact-hash-A> \
  --project-b <absolute-signed-project-B> --snapshot-b <exact-hash-B> \
  --evidence-dir <new-absolute-directory>
```

Each attempt runs the recorded producer, explicitly composes the complete
two-slot verifier batch, launches the unchanged verifier with Root selectors,
publishes qualification from its exact terminal, and composes the separate
qualified data consumer. All command requests/responses are retained. Polling
is bounded and follows only the root returned by that one launch. Any failed
request, terminal failure, unexpected continuation or timeout stops the run;
there is no relaunch, global lookup, daemon lifecycle action or partial pass.

Success requires input-independent verifier D0 to remain equal, while both
subject manifests, selected D1 and qualification witnesses differ. No live
success is claimed until the harness produces its complete acceptance file.

## Three independent harness modes

The default `--verifier-kind tool` keeps the direct Tool acceptance above.
Use a separate new evidence directory for each additional mode:

- `--verifier-kind graph`: composes the installed Graph's two root slots,
  then launches that selected Graph. Its ordinary probe Tool has no slots and
  receives only the parent's admitted normalized realizations. The Graph
  action intentionally has no raw product selectors. Qualification refers to
  the Graph's exact root and terminal, not the probe's placement.
- `--verifier-kind workflow`: launches one signed project Graph per input
  variant. That Graph performs produce → explicit verifier composition →
  action-selected Tool verification → qualification → consumer composition.
  It reads its own exact thread for the admitted pinned context. It uses
  daemon-authored `dispatch.child_thread_id` after verification, then exact
  `service:threads/get` for the child chain root and capsule. It never infers
  root equals child, accepts a caller host path, or scans receipts/threads.
  The harness only launches and reads the enclosing Graph's exact root.

The workflow uses the existing strict sorted accepted-product names
(`auxiliary`, `subject`), explicit finite capability declarations, and no
authored retry. A missing child coordinate, binding refusal, malformed result,
or unavailable exact read fails the workflow; there is no CLI workaround.
Successful mutation services remain authority-checked by their existing
configured-operator owners; listing a selector does not grant a binding.

Each mode must independently produce its full acceptance file before claiming
live coverage. Adding source fixtures does not establish any live pass.
These are **two root slots**, not two declarations grouped under one worker
Config content-dependency binding. That case still needs separate acceptance.
This extension also does not replace the parent fixture's actual worker,
credential, completion-fence or fixed-pin coverage.
