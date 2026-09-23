<!-- ryeos:signed:2026-09-22T02:52:14Z:8e85a74908bb52f98ea771d2eb3f30d9e906b2a7713a9cb492ef5d5e4cfab98e:9llr8VcWQBQU5a0qwQeI2Z+Oi1Caz6+EE9Ury3LiQyUK0gi29c/VT/6gSRqoVSz5NaxVmpM5D3TrnVH63G3MAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/development
tags: [development, tooling, production, qualification]
version: "1.5.0"
description: Canonical owners, external entry boundaries and completion gates for repository operations.
---

# Development operation ownership

Source ownership is independent of permission to execute. A Tool can be
operator-only. Being a release operation, being invoked before installation,
or not being available to a worker does not exempt reusable production logic
from the ordinary Tool/config/Graph organization.

An external caller may invoke the same canonical implementation before RyeOS
is available. That is explicitly external evidence, not admitted execution.
Do not maintain two implementations, route a Tool to ambient host programs, or
grant a worker installation, acquisition, signing or publication permissions
merely because the operation has acquired a Tool definition.

## Continuation rules from the Stage-0 handoff correction

The active implementation sequence is section 0 of
`.tmp/bundle-release-execution-closure-implementation-plan-20260921.md`.
Finish release-authority prerequisites, calibration/catalog bootstrap, then
data-only and binary-bearing bundle publication/activation on another node.
The full development-worker proof is separate. The source inventory below is
an ownership backlog, not an instruction to complete every row before release.

Read the dated checkpoint in `source-local-bundle-development.md` before
continuing the native release proof. It records the successful corrected
artifact, separates observed results from open gates, and identifies the next
existing authorities to use.

- Put reusable behavior beside its existing RyeOS Tool family, selections in
  Config, orchestration in Graphs, and qualification fixtures in the existing
  test family. Check those owners before creating a script or another runbook.
  Operator-only and bootstrap behavior are not automatic `scripts/release/`
  exceptions.
- The Stage-0 export/verification helper is
  `.ai/tools/ryeos/development/stage0-platform-production/lib/bootstrap.py`.
  Its tests belong to `tests/e2e/development-toolchain-stage0/test-bootstrap.py`;
  its usage belongs to the existing source-local development knowledge. Do not
  recreate a parallel release-script entry or leave a compatibility wrapper.
- That helper is still an **externally invoked auxiliary implementation**, not
  an admitted Tool descriptor. A source move or publisher signature does not
  authorize host Docker, sudo, Python, network or filesystem access inside a
  worker. A future admitted entry requires its actual environment and policy.
- Elevate only the unavoidable host boundary. The ordinary user receives the
  bounded Buildx export and runs verification; do not require root-owned output
  repair or recursive chown. Keep exact saved verification inputs with output.
  Check plugin availability and permissions rather than guessing executable
  names or treating permission-denied discovery as evidence of absence.
- Repair missing dependencies in the owning exact input contract, including
  recursive ELF closure, byte hashes and notices. Do not borrow host libraries,
  add an ambient PATH fallback, manufacture a wrapper, or reuse the previous
  platform manifest after changing its bytes.
- Use RyeOS retained generations, materialization and product evidence for
  source/build identity. Git may serve repository maintenance or external CI,
  but is not a dependency of native snapshot, capture or bundle publication.
- Preserve the simple case: explicitly authorized live-filesystem execution
  remains distinct from captured/isolated release production. Do not impose
  compiler bootstrap on every worker or relax isolated production because an
  ordinary live-filesystem Tool is allowed to use its selected environment.
- Keep development-specific toolchain selections in the project development
  bundle, generic reusable mechanisms in their existing owner, and node policy
  in the node's signed generation. Editing an installation seed does not alter
  live policy; a missing binding is not a reason to widen generic policy.
- A node-local external-content root is signed node policy. Create a durable
  directory, inspect its device and inode, and apply those coordinates to that
  node's live policy generation. Never hard-code one machine's path, device or
  inode into a portable initialization profile, and never treat an empty
  portable `roots` map as authority to import from an arbitrary host path.
- Bind a raw bootstrap closure to its declared bootstrap producer/verifier
  consumers only. Downstream producers consume an independently qualified product via
  named Config relationships. Directly rebinding Stage-0 to Cargo-vendor,
  authoring utilities or routine workers collapses bootstrap provenance into
  ambient runtime state and is not RyeOS-aligned.
- Keep D0 literal bindings and selected D1 bindings distinct. A bundle verifier
  may have an installed-bundle binding for its fixed bootstrap runtime, but
  resolving a project-owned product relationship makes the selected effective
  consumer generation-scoped. Re-stage the already verified literal binding
  and publish it through the selected-binding contract with the exact product
  selections and admitted owner; do not bypass this with a host path.
- A retained dynamically linked bootstrap executable is not directly execable
  merely because its file exists. Isolated Tools must enter it through its
  retained loader and library path. An `ENOENT` at exec for a present file is
  evidence to inspect the ELF interpreter, not authority to expose ambient
  host libraries or weaken isolation.
- Separate milestones in every status report: source implemented, locally
  tested, artifact verified, imported, bound, independently qualified,
  calibrated, published, transferred and activated. Cached export is not
  independent reproduction. Passing one does not imply the next.
- Review prerequisite and release process environments together before the next
  signing/install cycle. One-shot verifiers use the existing private sandbox
  scratch and dedicated temporary children for Cargo state or a required home;
  they neither inherit host HOME nor set HOME to the entire `/tmp` root.
  Persistent-session runtime views are not required for a version probe.
- Before introducing another prerequisite, name the exact release operation
  requiring it and its declared closure. Worker-environment utility production,
  Stage1 reproduction, self-hosted CI and persistent-cache work do not become
  release gates simply because they share development tooling.

The verified Stage-0 artifact bootstraps a missing development prerequisite.
Routine non-core bundle updates must reuse admitted environment products;
they must not repeat compiler acquisition or rebuild the serving host image.
Do not narrow the agreed end-to-end proof merely to avoid its current gates.

## Complete source inventory and disposition

This inventory covers every file in the 59-file pre-cleanup `scripts/` tree.
Paths in this table are relative to `scripts/` unless stated otherwise.
"Final owner" describes the required end state; an open gate is not completed
by moving source into a different directory.

| Source family | Final owner / external boundary | Completion criterion |
| --- | --- | --- |
| `lint-dependency-layers.py`, `lint-naming.sh`, `lint-cli-presentation.sh`, `lint-no-content-wrap.sh` | Ordinary `tool:ryeos/development/repository-validation/*`; canonical adjacent Python helper; signed `development/ryeos/repository-validation` Config. CI calls that same source before a node is available. No worker grant. | Captured/offline installed execution with the existing exact Python binding; positive, negative, missing-input and bound checks; remove old behavioral implementations and update CI. |
| `release/fixtures/development-toolchain-stage0/{Cargo.toml,README.md,probe.sh,app/Cargo.toml,app/build.rs,app/native.c,app/src/main.rs,app/tests/child.rs,macro/Cargo.toml,macro/src/lib.rs}` | Moved to `tests/e2e/development-toolchain-stage0/`. Test input, not production, runtime adapter or worker dispatcher. | Caller/path/source-contract checks; preserve historical artifact receipts. A new publisher or admitted run must supply its own reproduction evidence. |
| `release/qualify-local-inference-node.sh` | Moved to `tests/e2e/local-inference-node/qualify.sh`; disposable-node E2E owns activation/restart/recovery testing. | Workflow and source-qualification path tests; live local-inference qualification remains its own explicitly selected campaign. |
| `release/qualify-container-image.sh`, `release/container-mock-chat-provider.py` | Moved to `tests/e2e/container-image/`; external test harness owns Docker and mock-provider topology. | Shell/path checks and real image qualification at release, not an added worker capability or mandatory build-time test. |
| `dev/qualify-configured-remote.sh`, `dev/test-qualify-configured-remote.sh` | Moved to `tests/e2e/configured-remote/`; exact installed CLI drives existing configured remote authority. | Focused fake-client contract suite plus separately recorded real remote campaign; do not conflate them. |
| `smoke-execute-stream.sh`, `smoke-installed-resume.sh` | Moved to `tests/e2e/execute-stream/smoke.sh` and `tests/e2e/installed-resume/smoke.sh`. External acceptance, not worker production. | Update terminal-library paths and CI/container callers; live smoke requires explicitly selected node credentials/lifecycle scope. |
| `release/fetch-development-registry.py`, `release/test-development-registry.py` | Selection/verification/assembly now share `registry-production/lib/registry_inputs.py` with the signed offline `assemble` Tool. Project paths and acquisition selection are Config. The external entry retains only first-bootstrap transport/input loading; its curl tests stay there, production tests moved to E2E. Stage0 Cargo remains sole vendor producer. | Confined offline input construction and full byte comparison must pass separately from existing admitted vendoring; no host Cargo/cache/credential import. Network bootstrap is not an admitted transport. |
| `release/acquire-development-toolchain-stage0.sh` | The remaining pre-RyeOS entry owns only exact HTTPS transport, cache verification and capture of Config-selected publisher-image members. It atomically emits `ryeos.development.stage0-acquisition.v1`; it does not produce or verify the compiler platform. | Place the result at `stage0-acquisition` in the selected source snapshot and run `graph:ryeos/development/stage0-acquisition-capture`. Existing retained-project product authority captures the exact tree; network and publisher-image access never enter the graph or offline producer. |
| `config:development/ryeos/stage0-acquisition-products`, `graph:ryeos/development/stage0-acquisition-capture` | Existing retained-project product authority records the exact closed acquisition tree without copying it or relabelling it as compiler output. The graph has no action node, network authority or consumer relationship. | A retained product witness exists for the exact source snapshot. Consumer linkage remains absent until the admitted Stage1 runtime is complete. |
| `stage0-platform-production/lib/{produce.sh,contract.sh,runtime.sh}` | One canonical offline compiler producer and shared Config/runtime implementation. Bootstrap and future Stage1 call this auxiliary source with an exact acquisition directory. | Admit an exact Stage1 runtime product containing the complete Bash/archive/helper closure, then run a fresh reproduction and independently compare its archive/tree testimony. Current build support lacks `tar` and `gzip`; host PATH and substitute toolboxes are refused. |
| `tests/e2e/development-toolchain-stage0/test-artifact.sh`, `tests/e2e/development-toolchain-stage0/test-runtime.sh` | Focused qualification tests beside the existing compiler behavior fixture. They call the canonical Tool-adjacent runtime/artifact verification source. | Exact retained archive inputs, corrupt/member/ELF refusal and independent equality. No archive mutation or relabeling of bootstrap provenance. |
| `release/author-local-inference-realizations.py`, `release/local-inference-qwen3-0.6b-v1.json`, `release/verify-local-inference-release.py` | Local-inference realization production/verification Tools, input selection under project development Config, composition in Graph. Current host entry is not the final owner. | Pin the actual Python/archive/ELF/LLVM/acquisition closure; retain manifests/licenses; eliminate hard-coded source selections in production code. Preserve published artifacts and independent local-inference acceptance. Not a hosted-Codex dependency. |
| `release/package-workload-client-realization.sh`, `release/verify-workload-client-realization.sh`, `release/test-workload-client-realization.sh` | Signed release-artifact packaging/verification operations; external release job supplies the already built static executable and immutable release coordinates. Tests stay with the external script until the replacement is qualified. | Exact archive/ELF/provenance support; one packager/verifier implementation; refuse development testimony and reproduce deterministic content. No Docker-image mining or worker client installation. |
| `release/package-bundle-artifact.sh`, `release/verify-bundle-artifact.sh`, `release/test-package-bundle-artifact.sh` | Reusable package/verify behavior belongs beside release Tools. A thin CI distribution entry can remain external before node installation. | Preserve signed bundle closure and exact version/target/build-profile checks with admitted archive/signature tools; same canonical implementation in CI. |
| `release/prepare-aur.sh`, `release/test-prepare-aur.sh` | Deterministic AUR metadata production Tool with release coordinates/templates as Config; external job alone owns push/distribution credentials. | Identical generated metadata from exact inputs, output refusal tests and no implicit publication. |
| `release/resolve-version.sh`, `release/test-resolve-version.sh`, `release/official-publisher-fingerprint.sh` | Small external workflow-input adapters may remain for GitHub event/ref/key handoff; reuse existing version/signature owners. Reusable parsing belongs beside release operations if expanded. | Closed input/ref validation and exact official fingerprint; no second version source or private-key exposure. |
| `pkg/install-local-direct.sh`, `pkg/bundle-sets.sh`, `pkg/test-bundle-sets.sh` | Necessary host package/filesystem/PATH/user/lifecycle entrypoint and its pure installation-data helper. Bundle membership remains signed source data. | Existing install/init authorities own policy generation; explicit reset only. Do not reimplement runtime policy or bundle admission in shell. |
| `populate-bundles.sh` | Necessary pre-installed-RyeOS build/staging entry; calls existing bundle publisher. Build selections and artifact production ultimately compose project-owned operations; seed only binaries/registrations needed to boot that path. | Remove independent signature-envelope behavior in favor of the canonical signer; preserve explicit focused builds and source signing, no full gate during release. |
| `lib/ryeos-terminal.sh`, `pkg/test-ryeos-terminal.sh` | Shared external-shell presentation library and its tests; no standalone Tool or duplicated runtime renderer. | Keep source/install/build status consumers aligned; focused renderer checks. |
| `ci/install-static-linters.sh`, `ci/lint-workflows-and-shell.sh` | Pre-install CI acquisition/orchestration entry; exact actionlint/ShellCheck are reusable validation Tool inputs, not ambient worker dependencies. | Preserve checksum-pinned acquisition; signed validation operation must bind exact binaries and complete child PATH before replacing CI behavior. |
| `ci/test-daemon-image-init-policy.sh`, `ci/test-publisher-trust-policy.sh` | Focused tests of genuinely external Docker/init/install/publisher integration; remain with that owner. | Run relevant contract tests when those scripts change; no automatic live node mutation. |
| `gate.sh` | Explicit broad CI/maintainer test orchestration, not default worker operation. Runtime test behaviors remain Cargo/project-owned Tools. | Keep full test/rebuild opt-in and crash-feature isolation; do not hide unrelated tests in release packaging. |
| `browser-assets-production/publish.sh`, `check-ui-wasm-fresh.sh`, `dev-ui-assets.sh`, `dev-tui.sh` | Complete contract/WASM/Svelte assembly and atomic publication has one canonical signed source under `.ai/tools/ryeos/development/browser-assets-production/`. The historical checker is a thin check-only entry; host file-watcher and interactive TUI/browser launch remain thin host entrypoints. | The operation builds one closed six-file generation, rejects stale bytes and atomically replaces `pkg/`; exact Node/npm/wasm-bindgen and offline package authority must come from the admitted development environment. Watching/process lifecycle remains separate. No host PATH inside worker children. |
| `dev/sign-dev.sh` | Existing RyeOS/Lillux signing is canonical; the public development fixture selects a key, not another signature algorithm. Only the necessary explicit pre-install key-selection entry may remain. | Expose/reuse safe per-item publisher signing without whole-checkout exchange or node identity impersonation; remove duplicate envelope/hash/signature implementation after canonical entry qualification. Current script remains an open consolidation item. |
| `dev/revert-sig-churn.sh` | Maintainer-only Git working-tree operation; Git remains the authority for local review/commits. Reusable filtering should live beside its signed maintainer Tool if retained. | Exact path selection, dry-run, reject mixed edits, never staged-change loss; no blanket worker Git/signing grant. |
| `dev/free-build-space.py` | Host build-cache housekeeping, not project production or node GC. Existing Cargo cache and RyeOS GC owners must remain separate. | Remove personal default path and silent deletion errors, require exact exclusively owned target and explicit destructive intent. Do not run it against active builds or add node/host mounts to a Tool. |

## Acquisition and production are different authorities

The current registry fetch helper requires host Python/PyYAML, curl >= 8.4,
resolver files, trusted TLS roots and public network access. Its URL/byte/time
Config is an input selection, not a node egress grant. The web page-fetch Tool
is not a binary package transport; it has text conversion/output limits.
Managed activation is for trusted installed-bundle consumers, not arbitrary
project consumers, and ordinary external-content import/bind already owns
project content. None should be widened for this conversion.

The split now implemented in source is: acquire public inputs through the
explicit external bootstrap entry; import/bind them; have the signed offline
registry-input Tool validate the lock/index/archive relationship and construct
the documented local-registry tree; let the existing exact Cargo Tool produce
the vendor tree. The same canonical compiler serves bootstrap and offline
production, with explicit provenance in each new receipt. A source move does
not reproduce historical archives. Installed production/equality evidence is
recorded separately under `tests/e2e/registry-production`.
The installed offline Tool passed for 406 locked packages on 2026-09-07;
independent retained-manifest comparison matched all 1,108 payload entries,
including 771 archive/index files and their modes. Only the new receipt differs.
This does not relabel earlier Cargo or acquisition evidence as a new run.
Where acquisition must run outside RyeOS to seed the first interpreter/content,
the entry remains explicit and calls the canonical selection/checking helper.
It neither executes Cargo nor establishes consumer bindings. Until TLS/resolver,
network admission and exact transport are supplied, do not sign an ambient
curl/Python wrapper and call acquisition converted.

Compiler Stage0 similarly has two scopes: first exact input acquisition,
then reusable offline install/relocation/inventory/archive verification. The
external acquisition entry now emits only exact archives and exact selected
publisher-image members. Bootstrap calls the same offline `produce.sh` beside
`stage0-platform-production` that Stage1 must call. `contract.sh`, `runtime.sh`
and the independent bootstrap-artifact verifier remain in that owner; focused
tests live in E2E. Changing the canonical producer creates new production
testimony; preserved historical archives keep their original input/producer
hashes. Source moves, passing fixture tests and archive verification are not
fresh reproduction. The return-only
`graph:ryeos/development/stage0-acquisition-capture` captures a
`stage0-acquisition` directory from the exact retained source snapshot as an
ordinary product. It deliberately has no consumer relationship and makes no
compiler-production claim. Admitted Stage1 remains gated on exact `tar`/`gzip` plus the
complete Bash/helper runtime closure; neither host PATH nor a substitute
toolbox may satisfy it.

## Utility-build support remains an independent deliverable

The merged `authoring-environment-production/lib/utilities.py` remains the sole
fresh utility recipe. Reuse the existing Stage0 compiler, not another Zig
platform. Select real shell, Make, helpers, ELF tooling, runtime libraries and
licenses into an exact build-support realization. Configuration owns selected
inputs and inventory; existing import/binding owns target admission.

`assemble-build-support` now implements that exact assembly as a signed Tool,
using the existing Python and shared upstream ELF transformation/verification
helper. Its first installed retained output/import passed: 88 files and
21,394,807 bytes. The corresponding support-input Config selects raw bytes;
the consumer support Config records the existing input/command/notice contract.
Neither is a second manifest or resource-policy authority.

Qualification must exercise configure, generated scripts, Make recipes and
compiler/linker descendants in captured filesystem with isolated networking.
No `/bin`, `/usr`, resolver/TLS or host PATH fallback is allowed. Do not expose
utility compilation until those nested paths actually work. Historical static
utility assembly and the already verified worker environment are preserved
and are not fresh utility-build evidence.

The first real nested probe loaded 37 helpers and reached static compilation,
then hit the node-owned 1,024-open-file ceiling. The selected compiler's `cc`
driver does not accept `-j1`. The operator approved a target-only change to
4096 through the existing stopped-node policy application. The rerun passed
all helpers and nested static compile/run; only six selected products were
retained. This is not full utility compilation. Exact failed and successful
thread/capsule/result coordinates are retained in
`tests/e2e/authoring-environment/build-support-qualification.json`.

Full fresh utility compilation subsequently passed in a separate execution,
including fresh Git invoking the final authoring shell. The revised support
adds exact `tee` bytes with historical provenance; a separate 38-helper probe
also passed. Stage0's existing linker and the distinction between Git's build
and final runtime shell are explicit, with no host PATH or library fallback.
The ordinary `build-utilities` Tool and independent E2E fixture now use the same
`utility_production.py` entry boundary and `utilities.py` compiler recipe.
The E2E's fifth final-runtime input is not a production build dependency.
Installed execution of the ordinary Tool also passed: 80 retained files,
92,435,329 bytes, no binding publication. Its own execution/capsule/result
coordinate is recorded in the evidence index; none of these operations is
added to a hosted worker's grant. Compiler Stage0 production/transformation
and the remaining release-artifact families still have the open conversion
criteria in the inventory above; this pass does not qualify those operations.

## Evidence and unfinished gates

`tests/e2e/development-cargo/{README.md,qualification.json}` are already tracked
since `53a1776ad`; exact ignore exceptions do not admit generated builds or
private node state. Their evidence is operator-driven check/build/test for two
finite packages plus the expected broken-source failure and completed-result
retention across normal restart.

Repository validation has separate focused source and installed-operation
evidence in `tests/e2e/repository-validation/`. Neither that evidence nor moved
E2E harness contract tests close worker edit/current-child/shared-child,
completion-fence/candidate, live restart recovery, return/apply or real remote
acceptance. No permission, policy, lifecycle or credential boundary is widened
by this ownership correction.
