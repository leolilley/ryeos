# Authoring environment qualification

## External execution routing checkpoint — 2026-09-19

`probe_pinned_codex_external_execution.py` drives the exact pinned Codex App
Server with a credential-free scripted Responses endpoint and an actual pinned
exec-server. Two separate bwrap filesystem views give the same `/workspace`
path different contents. This is an external diagnostic, not an installed RyeOS
worker, Render deployment, authenticated channel or Lillux qualification.

```sh
python3 -B tests/e2e/authoring-environment/probe_pinned_codex_external_execution.py \
  --package /absolute/path/to/extracted/authored-codex-package
python3 -B tests/e2e/authoring-environment/probe_pinned_codex_external_execution.py \
  --package /absolute/path/to/extracted/authored-codex-package --selected-skills
python3 -B tests/e2e/authoring-environment/probe_pinned_codex_external_execution.py \
  --package /absolute/path/to/extracted/authored-codex-package --managed-mcp
python3 -B -m unittest discover -s tests/e2e/authoring-environment \
  -p test_external_execution_probe.py
```

The probe verifies all five executable artifacts against the existing activation
declaration, uses fresh empty profiles, mounts no host home or credentials, and
starts no paid model or RyeOS daemon. It needs Linux user/mount/PID namespaces
and loopback sockets. The host's explicitly mounted utilities are fixture
dependencies, not an admitted production runtime. The namespace views share
the host network for the loopback endpoints: **this does not qualify egress
restriction**. Files, processes and listeners are disposed on exit; diagnostic
stdout and protocol input have finite bounds and protocol waits have deadlines.

`pinned-codex-external-routing.json` records the successful checkpoint:

- Actual shell, patch, image, symlink reads and interactive stdin used the
  external view; the controller workspace stayed unchanged.
- A synthetic secret in the controller's profile did not reach tool output.
- Read-only configuration overlays refused direct write and unlink attempts;
  the exact environment inventory overrode a conflicting ambient execution URL.
- Every scripted request retained the same namespace-qualified tool inventory
  and complete definition digest; later schema/namespace changes are refused.
- Actual authored immutable feature/notification arguments overrode a hostile
  fixture profile. Candidate-local configuration did not add controller tools.
- The optional selected-skills diagnostic discovered and read an exact executor
  package, then refused an existing, independently readable file outside it.
  This diagnostic is not permission to enable selected roots in RyeOS.
- Explicit `environment_id: local` was refused when local was not configured.
- Killing the executor produced an explicit failure without local execution.
  The pinned client first attempted recovery for 25 seconds. The fixture's
  40-second observation deadline accommodates that existing protocol behavior;
  it neither adds production retries nor grants permission to replay a command.

The fixture uses `gpt-5.5` **embedded tool metadata**, not a real model call.
The custom loopback provider is diagnostic-only and does not change production
subscription authentication. Official App Server/configuration documentation
guided the fixture; exact protocol behavior is tested against pinned 0.147.0.

This establishes the narrow tool-routing mechanism, not full tool/authority
closure. Candidate-controlled discovery/hooks, all
registered tool paths, enforced egress, occurrence authentication, authoritative
writer exclusion, frozen export and durable RyeOS restart remain open. Do not
activate the current authoring profile by simply adding an execution URL. A
mutable `environments.toml` can override that URL; the production binding and
effective configuration must be owned and protected by the runtime.

### Pinned controller-extension audit

The 0.147.0 source review found independently enabled controller paths:
`plugins` defaults on despite `remote_plugin=false`; notification commands are
independent of the hooks feature; skill MCP dependency installation defaults on.
Both authored profiles now explicitly close those paths in baseline and immutable
argv, disable orchestrator skills/MCP, and refuse caller-supplied
`selectedCapabilityRoots`. Selected executor roots can add MCP independently
of the plugins feature. The separately admitted RyeOS dynamic-tool injector is
unchanged; caller-supplied dynamic tools remain forbidden in the authoring route.

The baseline routing diagnostic advertises six tools; the optional skills
diagnostic adds `skills.list` and `skills.read`. The latter uses exact observed
package/resource identities, not reconstructed paths. Startup host discovery,
enterprise-managed inputs and all other authority surfaces still need qualification.

**Ordinary `mcp_servers={}` is not a deny-all policy.** Pinned configuration
merges tables, preserving lower-layer server entries. `--managed-mcp` supplies
a diagnostic read-only `/etc/codex/requirements.toml` containing an empty managed
MCP allowlist. A configured controller subprocess canary remains in the merged
ordinary configuration but is not launched during the completed turn. No host
system file is changed. This proves that pinned mechanism only in the fixture's
empty enterprise/cloud-policy context—not an unconditional production denial.

Production still requires qualification of the admitted runtime-configuration
mount and control of higher-priority managed policy before MCP startup. A profile-home
`requirements.toml` would not be read as system requirements. Do not repurpose
node networking files or enable the external profile to bypass this gate.

Fixture HTTP handling is serialized, request retention is bounded before append,
and accepted sockets have a timeout before header parsing. A live incomplete-header
probe waits for exact acceptance and verifies bounded teardown. These fixture
properties are not RyeOS transport/recovery evidence. Independent reviews found
and corrected the previous missing-file escape assertion, namespace-hash omission,
post-retention request limit, and pre-handler header timeout gap.

### Production configuration work in progress

The implementation branch retains required `auxiliary_configs` in structured
session profile v8 and retained persistent-session capsule v13. Each entry names
one signed source file and one flat destination in the existing locked profile
home. Destinations are sorted/unique, cannot replace the primary baseline, and
are limited to 16 files of at most 64 KiB each. Primary baseline admission now
uses that same existing launch-time byte limit.

Admission and recovery refuse nonempty auxiliary inventories without enforced
read-only isolation. The executor uses pinned source descriptors for overlays;
the bridge verifies exact bytes before starting the provider. Where portable
state is supported, exact forbidden selectors reserve configuration names and
potential portable-session overlap is rejected. Empty inventories remain
explicit in every current bundle profile; no external authoring profile is
activated by these changes.

The same profile/capsule now also requires `runtime_configs`: at most 16 exact
source files (nonempty, at most 64 KiB each), with sorted, unique absolute
namespace destinations. The executor seals captured source bytes in descriptors;
the isolation planner mounts them read-only and refuses collisions with existing
authority, writable views and reserved roots (including private `/tmp`). These
mounts cannot supply executable authority, and preparation never writes their
destinations on the host. The bridge checks exact mounted bytes and read-only
ancestors before provider startup. This is generic configuration delivery, not
an executor-owned Codex path. Current bundle inventories are empty. Native Lillux
mount/ancestor checks pass across realized/sealed executables and nested-sandbox
mode, including descendant exec. Installed bridge admission and effective
provider-policy qualification remain gates before enabling a profile.

Cargo is now authorized. Rebuilt state framing/export/transcript (8) and capsule
(10) groups pass. The native terminal-export group (3), absolute-configuration
exec qualification (1), and ordinary Lillux suite (235 passed, 18 ignored) pass.
Engine/executor/structured-session test targets pass `cargo check --tests`;
type checking is not test execution. Rebuilt allocation/channel (14), engine
profile (14) and isolation (41) tests also pass. A prior build was explicitly interrupted
for disk space; targeted builds have resumed without relocating or deleting
other work. Full bundle refresh and installed admission remain required. See
[external execution qualification](../external-execution/README.md) for the
exact checkpoint, operator tests and missing lifecycle/channel/export work.

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
