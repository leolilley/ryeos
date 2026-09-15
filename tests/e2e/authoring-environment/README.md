# Authoring environment qualification

## Hosted integration checkpoint — 2026-09-09

`hosted-worker-qualification.json` records partial installed two-node evidence,
not a passing development-loop gate. The disposable target used its dedicated
unprivileged runit service and configured-operator forwarding from a separate
source node. The primary node was not initialized, reset or restarted.

Pinned Codex read/edited the eight-file private fixture. Exact turn observation,
completion-fenced capture, frozen-candidate survival across daemon restart and
owner discard passed. Child formatting did not execute: after correcting the
signed shell environment, the nested command sandbox refused the restricted
broker connection with EPERM. Do not interpret completed turn/candidate facts
as successful project verification, promote these candidates, or claim Railway
or full-repository development acceptance.

The original next step was to resolve the nested CLI connection without general
network access or weakening broker authentication. That prescription is now
superseded by the reviewed structured-session invocation correction: Codex uses
an explicitly admitted protocol callback; the native CLI is qualified separately,
not claimed inside nested Codex. The later checkpoints below describe the new
evidence. Scoped scratch and installed worker/child execution remain open.
Existing operator-driven Cargo evidence does not establish those worker gates.

### Offline pinned transport diagnostic

The original credential-free diagnostic used the exact retained executable
against disposable sockets. To reproduce that historical diagnostic:

```sh
python3 -B tests/e2e/authoring-environment/probe_pinned_codex_broker.py \
  --codex /absolute/path/to/retained/pinned/codex
```

The probe verifies the executable against the authored Codex activation pin.
It deliberately uses the test host's Python/readable runtime, not a production
worker environment; it cannot qualify the complete nested launch. On the pinned
0.147.0 Linux artifact, direct Unix connect is refused, and the exact-allowlisted
HTTP Unix-socket proxy request returns **501, `unix sockets unsupported`**.
An unallowed address request returns 403; the private listener receives nothing.
This was executed on 2026-09-09 without a model call or node mutation.

Therefore an exact socket allowlist alone is **not** the missing implementation.
Do not enable unrestricted networking, allow all loopback services, accept PID 0,
or silently select a different transport. The original requirement to resolve
the CLI transport before changing the profile is superseded by the explicitly
approved callback plan, not silently bypassed. A newer vendor pin is not presumed
to fix CLI connectivity until actually qualified. No proxy or authentication
change is claimed by this historical diagnostic.

## Production and independent probes

Production is owned by the ordinary Tools and adjacent libraries under
`.ai/tools/ryeos/development/authoring-environment-production/`; input contracts
live in `.ai/config/development/ryeos/`. This directory owns independent probes
and source-level regression tests, not an alternative producer or publisher.

Run the focused tests without a Rust build, daemon, credentials or model:

```sh
python3 -B -m unittest discover -s tests/e2e/authoring-environment -p 'test_*.py'
python3 -B -m unittest discover -s bundles/codex -p 'test_authoring_environment.py'
```

The utility-build unit tests are synthetic recipe/boundary tests. Installed
captured/offline support assembly, nested subprocess qualification and a full
fresh utility build have separately passed; their exact execution coordinates
are in `build-support-qualification.json`. Assembly alone does not prove the
upstream configure/Make closure. The ordinary `build-utilities` entry reuses the
qualified recipe; its installed-execution gate is recorded separately below.

`build_support_probe.py` is an E2E Tool fixture. In a disposable copy of the
source project, materialize it beside the existing production runtime as
`build-support-probe.py` and sign that fixture. Capture the project using
`ryeos snapshot create <message>`, independently import/bind its exact Python,
support and Stage0 declarations to the fixture at that snapshot, then run:

```sh
ryeos execute tool:ryeos/development/authoring-environment-production/build-support-probe --current-head --no-operator-vault --async
```

The fixture verifies missing ambient shell/compiler/loader-cache paths, loads
every selected helper, builds a tiny static C program through nested Make and
shell, strips and runs it, and checks exact output. It uses the production
environment constructor and existing runtime bounds, with one Make job. It is
not a worker grant, full fresh utility build or hosted development acceptance.
`build-support-qualification.json` distinguishes these gates and retains exact
observed coordinates; it does not overwrite historical artifact evidence.

The nested probe passed on 2026-09-07 after an operator-approved target-only
open-file policy change from 1024 to 4096; its first failure is also retained.
It produced six selected result files and no retained compiler cache.

After that gate, `utility_build_probe.py` is the separate disposable E2E entry
for the sole `lib/utilities.py` fresh-build recipe. Materialize/sign it as
`utility-build-probe.py` beside that runtime and set its finite deadline using
project execution Config. Bind its four production dependencies at the captured
snapshot: Python, build support, Stage0 and source archives. The fifth exact
authoring-runtime input independently exercises fresh Git's final shell path;
it does not supply the freshly compiled utilities. The fixture extracts/builds
in isolated `/tmp`, retaining only products and bounded per-source logs.
The whole recipe passed on 2026-09-07, including fresh Git's shell alias. The
ordinary `build-utilities` Tool and this E2E entry share `utility_production.py`;
only the E2E entry adds the independent final-runtime probe. The installed Tool
entry also passed in `T-1632d8ec-05a1-793c-595a-339807f6104d`, retaining 80 files
and 92,435,329 bytes without publication. Independent retained-manifest comparison
matched all 41 executable entries, including bytes, modes and paths, against the
successful E2E build. Only the added shared-entry source and updated recipe/evidence
files differ; this is not whole-artifact equality. Historical utility archives and the
worker authoring environment remain separate evidence. This is operator-driven,
not worker/remote-loop qualification.

## Named-product source composition

The source declarations now express three exact producer relationships without
changing the live leaf operations: prepared inputs are selected by the retained
environment Graph, build support is selected by the retained utility Graph, and
the resulting built utility distribution is selected by the environment Graph.
Graph-root selection supplies normalized admitted realizations to the inline
Tools at the existing `execution_runtime` mounts. The literal producer Python,
Stage0 platform and source-container pins remain fixed bootstrap inputs.

Final assembly uses a closed, disjoint map: `rg`, `zsh`, loader, libraries,
notices and source delivery come from prepared inputs, while the exact source-built
command set comes from the selected `authoring-built-utilities` product. The
product manifest authenticates those command bytes; the recipe additionally
checks its bounded source/build evidence and rejects undeclared executable
members. Environment access alone never grants a product or mount.

These edited Configs, Graphs and Tools are intentionally unsigned until the
operator performs canonical signing and activation. Their relationships require
no qualification policy because they are intermediate build inputs; this is not
a claim that the final environment is qualified. Acceptance still requires an
independent policy/verifier run over the resulting exact witness, including the
final loader, recursive dynamic-library closure, ABI/symbol requirements, command
execution and absence of host-library fallback under enforced isolation.

## Independent artifact probe

### Pinned App Server registration diagnostic

`probe_pinned_codex_tools.py --codex <retained-pinned-executable>` checks the
executable against the signed Codex activation digest, generates both schema
surfaces, and probes registration in fresh credential-free homes. The vendor
experimental capability is toggled only in these disposable diagnostic processes;
no installed profile, node or model turn is involved.

`pinned-codex-tools-capability.json` records the 2026-09-09 result: the stable
surface rejects dynamic-tool registration and the opted-in surface accepts it.
This is not actual tool emission, child execution or hosted-loop qualification.
The user approved adoption after this probe; the signed authoring profile now
opts in. Installed callback/child execution and vendor restart restoration
remain separate qualification gates. The old
Unix-proxy-first prescription in `hosted-worker-qualification.json` is historical;
the corrected invocation plan retains both interfaces with separate topology
qualification. None of its prior failed attempts or produced artifacts changed.

### Native invocation transport checkpoint — 2026-09-09

The bridge's ignored `workload_client_broker::tests::native_dual_ingress` test
passed in a fresh unprivileged Linux PID/mount namespace with private `/tmp`.
It exercised real Lillux peer checks, an idle CLI listener sharing the sole slot
with protocol ingress, and distinct normalized IDs for identical caller IDs.
Repeating it with `RYEOS_TEST_DUAL_CLIENT_BINARY` set to the newly built
`ryeos-workload-client` also passed, including nonzero exit for a deliberately
failed daemon result. No sudo, installed node, credentials or model was used.
The daemon response is a test fixture: this proves transport/client behavior,
not a signed installed child execution or connectivity inside nested Codex.

`invocation-qualification-20260909.json` retains the subsequent real v15/v16
callback attempts separately from historical evidence. V16 admitted a real
immutable-input child but both it and the independent evaluator failed before
process start on missing read-only project mount visibility. Neither turn
completion nor frozen candidate capture is a successful development-loop claim.

```sh
cargo test -p ryeos-structured-session --bin ryeos-structured-session-bridge \
  workload_client_broker::tests::native_dual_ingress -- --ignored --exact --nocapture
```

### Independent candidate assertion

The independent frozen-candidate assertion is the ordinary signed
`tool:qualification/format-candidate` fixture under this directory's `.ai/tools`.
Install it in the fixture **before** capturing the worker's immutable base;
bind its declared exact authoring-tools realization independently and do not
add it to the worker's execution grant. It reads only the fixed sample and
reports the existing candidate-evaluation result with exact base/candidate
hashes. An unchanged, unformatted, extended or symlink sample is refused.
It is a narrow E2E assertion, not a general repository/security evaluator.
The separate owner must still inspect the full candidate delta before adoption.

`test_candidate_evaluator.py` checks assertion semantics using the external test
host's explicitly selected programs. Passing it does not qualify the installed
artifact, base-resolution authority, candidate view or publication path.

### Assembled artifact probes

After producing an assembled output, run:

```sh
python3 tests/e2e/authoring-environment/qualify.py \
  --production <assembled-output> \
  --inventory-sha256 <independently-selected-inventory-checksum> \
  --output <new-probe-directory>
```

This checks the complete inventory, constructs a FROM-scratch image and invokes
the actual patched shell with descriptor-based PATH. `probe.sh` exercises read,
sed editing, search, diff/patch and scratch Git in private tmpfs with no network
or host libraries. The exact edited-file hash is required, not just exit zero.
Docker belongs to this independent probe only; no production Tool contacts it.

`selection.json` preserves the independently reproduced artifact selection,
empty-root evidence and separately identified admitted-production observations.
Expected manifests are computed from selected bytes; observed imports instead
come from exact completed-thread retained results through the normal CLI.
Admitted preparation, single-root assembly and all four manifest imports passed
on 2026-09-06. On 2026-09-07 the complete authoring graph executed assembly and
independent verification in the native backend's one retained shared view.
Both returned the selected inventory digest; the exact graph/capsule/result
coordinates are recorded in `selection.json`. The source HEAD did not advance
and no binding was published by the graph. `ryeos_production` is now qualified.

The owner/borrower integration uses one detached view, not separate overlays
over the same upper/work directories. The real graph result qualifies this
assembly/verification path, not the still-pending hosted worker/child loop,
restart, whole-repository Cargo operations or a remote development campaign.

The complete source/input trees require large-content authority. The original
utility binary and corresponding-source archives remain exact historical inputs;
moving the production recipe does not claim that those archives were rebuilt.
