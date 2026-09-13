<!-- ryeos:signed:2026-09-13T00:19:08Z:78d3ed8d60ba32d866e4bfa8928211f57ca04f91ef3d9e9c2b3f2c7e01111c73:JHx0PwOzaYQgoqe4DT/uP+BFuqFd6lUk3Un8CfYIZPyDWFwZYZ3ph+HhXSmRKxicT1+vcO2psVRKv/OpvGA6BQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: source-local-bundle-development
title: Source-Local Bundle Development
description: Source-local workflow, project-bundle, realization, and confinement contracts
entry_type: reference
version: "1.14.0"
```

# Source-Local Bundle Development

## Status

The original local command, state-root, signing, and bundle-smoke workflow is
implemented. The project-bundle and development-realization sections below
also record source-authored work that still has the explicit artifact,
runtime-root, signing, and qualification gates they name. They must not be read
as evidence that remote build/test execution is already available.

- Command descriptors / local command help: routed without daemon alias
  parsing; project-aware tails auto-detect a cwd ancestor containing `.ai/`;
  `RYEOS_PROJECT_ROOT` / `RYEOS_PROJECT_PATH` are part of the runtime/env
  contract.
- **Runtime state-root override**: `ryeos execute <ref> --state-root /tmp/...`
  runs against the resolved project source while runtime state anchors under
  the override; both roots appear in the response's `execution` diagnostics.
  Live-fs only; a state root inside the project source is rejected.
- **Multi-item signing**: `ryeos sign` accepts a bounded set of changed
  bundle refs/paths in one invocation (`753d758e`), input hardened against
  escape (`c1751747`).
- **Bundle smoke command**: `ryeos bundle smoke` (service:bundle/smoke +
  command descriptor). Bundles declare a `smoke:` list in
  manifest.source.yaml; the service runs bundle preflight, then dispatches
  each entry as a normal synchronous wait-mode thread against the bundle source with state
  isolated under a temporary state root, and reports per-entry status,
  thread ids, and the state root (kept on failure or `keep_state`). See
  `crates/daemon/ryeos-api/src/handlers/bundle_smoke.rs` and
  `ryeos_bundle::manifest::SmokeDecl`.

## Development project versus node initialization

The repository-root `.ai/` is this project's development bundle: it owns
development Tools, environment composition, exact realization declarations and
development knowledge. `bundles/.ai/` is the shared installation source-root
metadata, not another workload bundle. It owns publisher trust and signed
`node/init/profiles/` seeds. Installers copy those seeds into the installation
source; explicit node initialization or policy-generation replacement selects
and node-signs a complete generation. Editing a seed does not change a live
node's authority.

The opt-in `development` profile admits compiler-sized external content.
The ordinary content tier retains its 10,000-entry / 1 MiB manifest bounds;
the large-content tier admits up to 65,536 tree entries while retaining its
8 MiB serialized manifest bound. The tier-neutral realization set independently
bounds the aggregate launch to 65,536 entries. Node policy may narrow these
structural limits, never widen them. The verified Stage-0 platform has about
384 MB of small-file CAS content plus five large-store files; its development
profile therefore permits a 512 MiB aggregate CAS blob closure. Local binding,
verification and launch proof consume the selected node's `object_closure`
policy, not generic control-plane defaults. No policy entry selects a compiler
version or grants an unbound project access to that content.

## Project capture policy

The signed node `ingest_ignore` policy owns the complete conventional pattern
set. There are no engine-provided `.git`, build-directory, `.env`, or project-
specific defaults. RyeOS separately rejects its own identity, auth, vault,
signing-key, state, cache, bundle-registry lock, and pull-transaction paths as
a non-bypassable structural floor.

Repository-specific exclusions are authored in the existing
`.ai/config/execution/project-snapshot.yaml` contract. It excludes local
qualification artifacts, nested worktrees, local nodes and each active source
bundle's generated `bin`, `objects` and `refs` trees. These compose with node
patterns. The root source-local development bundle's generated `.ai/bin`,
`.ai/objects` and `.ai/refs` are excluded by this same project policy, while its
authored Tools, config, knowledge and manifest source remain inputs.
`.gitignore` is not capture authority. RyeOS's current anchored-path
patterns are literal prefixes, so Git-style `bundles/*/...` patterns must not
be pasted into this policy. The focused source-policy test checks every bundle
in the development profile while retaining source, Tools and the public key
fixture. Preflight must still inspect the actual manifest for retired/untracked
directories before a real project is exposed to a worker.

An in-place checkout can contain retired bundle payloads and nested fixture-node
history that a fresh worktree never had. The project policy also excludes the
observed retired bundle directories, local smoke items, editor log, and generated
E2E runs/history. These exclusions preserve the files on disk and retain authored
fixture directives, Tools, knowledge, and reviewed qualification evidence. They
are project input selection, not new engine path rules. Check nested paths as
well as root `.ai/state` when inspecting the actual captured tree; a clean Git
status alone does not prove a clean project snapshot.

`.dev-keys/PUBLISHER_DEV.pem` is deliberately public, Git-tracked development
fixture material and remains part of a `full_project` snapshot. Its signature
does not represent owner, release, deployment, or publication authority.
Actual private operator/node/vault/release/deployment keys remain outside every
project generation.

This release makes `ingest_ignore` schema 2 a clean cut: it replaces the old
schema-1 `additional_patterns` extension with the complete `patterns` set.
An existing node therefore needs an explicit stopped-node generation
replacement during its first install of this version:

```bash
sudo scripts/pkg/install-local-direct.sh \
  --populate --all \
  --trust-source-publishers \
  --reset-node-policy-generation
```

The replacement selects the install's mapped signed profile and preserves
node/operator/vault identities, execution history, project heads, and all
non-policy state. Fresh nodes and nodes already on schema 2 omit the reset
flag. There is no schema-1 decoder or implicit migration.

## Repository project bundle

Node grant reconciliation is not project production. The ordinary
`ryeos authorize-client` command belongs to the configured-local-operator
`identity/authorize-client` service and shares the existing canonical grant
writer with the explicit external bootstrap entry. Do not restore a confined
Tool wrapper that reopens node identity files, mount private node directories
into a Tool, or treat this service as a worker grant. Exact remote origin,
same-class scope reconciliation and stopped-node semantic-conversion fencing
remain enforced by the existing identity owner.

The repository-root `.ai` tree is also the source-local `ryeos` project
bundle. Its signed `.ai/manifest.yaml` is generated from
`.ai/manifest.source.yaml`; both are exact-file project sync surfaces. The
manifest declares the `config`, `knowledge`, `tool`, and `graph` kind dependencies
and no runtime authority.

Project AI surfaces have an explicit shape:

- a `file` surface admits exactly the named regular file and never descendants;
- a `directory` surface admits the named subtree but not a directory entry as
  project-manifest content; and
- apply replaces or deletes every materialized surface under one existing
  rollback window before advancing the deployed project ref.

Development configuration uses the generic
`.ai/config/development/<project-namespace>/` surface. The RyeOS repository
therefore uses `.ai/config/development/ryeos/`; the state/engine path contains
no `ryeos-next` or development-provider branch. Only that namespace, the root
manifests, and `.ai/tools/ryeos/development/` are unignored in this repository.
Unrelated root `.ai` content remains private/ignored unless deliberately added
to the registered project surface and Git contract.

The generated manifest and executable development items are signed by the
public development publisher fixture. That binds reproducible development
identity only. It grants no operator, node, release, deployment, vault,
publication, or remote authority.

### Sign project items without exchanging the checkout

Use `scripts/dev/sign-dev.sh <changed-file> ...` for this repository's public
development publisher. Validate the selected items through normal resolution
and admission; source-wide `bundle-verify` is read-only. Item signatures and
the existing project snapshot commit the selected generation. Signing alone
is not validation or execution authority.

Do not point the whole-directory `build`, `bundle-sign` or binary-manifest
publication transaction at the working repository. That publisher copies and
atomically exchanges its complete input directory; project snapshot exclusions
do not govern its staging. It is appropriate for a detached, exclusively owned
bundle tree, not a live Git checkout with nested worktrees, caches and ongoing
edits. A full project-bundle publication check must use such a detached tree
constructed from an exact admitted source generation. Do not add a second
ignore list to the publisher or treat its staging as project capture.

### Node-owned bundle verification

Run "ryeos bundle verify /absolute/source" as the configured local operator.
The CLI uses the live daemon, or the same service through normal standalone
dispatch when that node is stopped. Source and explicit registry-root paths
must be absolute; the daemon never interprets them relative to its own cwd.

Verification retains the installed generation through preflight and executes
parser handlers under its existing isolation runtime. It is not an offline
Tool that receives node registrations, policies or private keys to bootstrap
another privileged engine. The registry_root and registry_roots parameters are
alternative selectors and cannot both be supplied. The direct
"ryeos-core-tools bundle-verify" maintainer/CI entry remains available in its
explicit host composition context; it is not a workload sandbox escape hatch.

Verification neither signs nor publishes the candidate and its report is not
an installation/admission receipt. For an immutable publication check use an
exact detached source generation.

## Locked registry input acquisition

`scripts/release/fetch-development-registry.py` currently provides the explicit
operator input-acquisition entry. Its reusable selection/checking/assembly now
has one canonical owner beside `tool:ryeos/development/registry-production/assemble`.
The script retains only host transport and input loading; both paths call
`registry-production/lib/registry_inputs.py`. It consumes the selected
`config:development/ryeos/registry-acquisition` and `Cargo.lock`, and obtains
only the locked public registry coordinates. It verifies archive checksums
against both the lock and selected index entries; it never executes Cargo,
unpacks crates, reads host Cargo credentials, or writes node state.

URLs, host selection and byte/time budgets are project-owned acquisition data,
not node egress permissions. The helper consumes operator-selected input files;
it does not verify their RyeOS signatures or turn them into node authority.
Host Python/PyYAML and curl are bootstrap dependencies only. Curl must support
streaming download bounds (version 8.4 or later, checked before acquisition).
User curl configuration, proxies and redirects are disabled. Each request is
independently process-supervised within both request and total time budgets.

The output uses Cargo's documented local-registry format: unchanged verified
crate archives plus plain index entries for selected locked versions. It does
not synthesize Cargo's private sparse-cache encoding. A sibling staging
directory prevents failed downloads from publishing a partial registry. The
operator must exclusively own the destination during acquisition; this helper
is not a concurrent no-replace transaction or a replacement for Lillux's
node-owned filesystem authorities.

The signed `registry-production/assemble` Tool takes the same lock and public
selection Config plus `config:development/ryeos/registry-production` for project
paths. It reuses the exact admitted Python runtime and reads only the bound
registry tree with captured filesystem and isolated network. Missing or changed
input fails; no transport is available. Its output retains verified archive
bytes and selected index rows. The new `registry-inputs.json` distinguishes
`public_https_acquisition` from `admitted_retained_inputs`; it is an audit
receipt, not a manifest, consumer binding or new acquisition claim. Historical
archives/receipts are preserved. Production does not unpack crates or run
Cargo; the existing Cargo Tool still owns vendoring.

Focused production tests live in `tests/e2e/registry-production`; tests of the
retained external curl boundary stay beside that script. Installed reproduction
and full input/output comparison are separate from source tests and from
hosted-worker development qualification. No new worker grant is implied.

Import/bind that input tree through ordinary external-content authority. The
admitted Stage-0 Cargo then owns final vendoring with isolated networking and
explicit source replacement. The finite `tool:ryeos/development/cargo-vendor`
selects that exact Cargo and input manifest, an empty executable search path,
and `--locked --frozen --offline --respect-source-config --versioned-dirs`.
It has no caller-selected arguments or root-worker grant. Its private Cargo
home lives under `/tmp`; the useful result is `products/cargo-vendor` in the
retained project generation. Both the descriptor and resolved project execution
configuration bound it to 300 seconds. Import its retained output through
`external-content import-result`. Source checks and successful acquisition
alone do not prove that the pinned Cargo accepted the format, produced the
required closure, or that repository build/test qualification passed. No
dependency manifest or build/test grant is invented before those gates.

Admitted isolated vendoring and exact retained-result import passed on
2026-09-06. Thread `T-137fee28-f0e1-afe6-5baf-e0acd428e682` completed with
`exit:0`; result snapshot
`8af6287a6eb131a36ef6b59f22aa1d4926dcac71f0b4ef137a7c36e26923907f`
retains `products/cargo-vendor`. Its observed large-content manifest is
`8dac785a06faad238b3210c79ba3ca7bee37dc211fe0e95acf80dff6500a75ac`
(25,804 entries / 575,879,606 bytes). This is a real input-production receipt,
not evidence that repository check/build/test operations have passed.

The upstream contracts are [Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)
and [cargo vendor](https://doc.rust-lang.org/cargo/commands/cargo-vendor.html).

## Development execution confinement

The project-owned `config:development/ryeos/worker-environment` selects the
independently qualified authoring product through its signed relationship and
requests structured-session workload invocation. It does not include a native
CLI realization or add one to executable search. Its finite
request contains `format-check`, `format-file`, `platform-inspect` and the
bounded `cargo-check`, `cargo-build`, `cargo-test` operations. Each
workspace-access assertion matches the corresponding
signed Tool. Compiler content is child-owned, not root-worker content. The
request does not itself grant authority: node policy, initiating principal,
parent delegation and resolved child contracts still intersect at admission.
The Codex bundle's general authoring environment remains client-free.

When one of those child operations declares product slots, its exact
destination-local selection is a root-launch input targeted at that admitted
workload execution. The portable environment continues to name only the
logical child ref and signed product relationship. Worker boot compiles the
operator-selected testimony into the callback grant, and the daemon injects it
as an ordinary root selection immediately before the existing child admission.
The workload wire has no product-selection field: a model or bridge cannot
name witness hashes, omit the retained selection, or substitute another
target. Selectors for a disabled or undeclared child surface fail launch
preparation instead of becoming inert capsule data.

Native CLI ingress remains a separate explicit environment selection and
qualification lane under the same restricted invocation authority. Source
composition checks are not evidence of target-local product selection/binding,
a successful worker call, or qualification of the combined implementation.
Cargo dependency production now has an observed manifest. The finite
`cargo-check`, `cargo-build` and `cargo-test` Tools select that exact vendor
closure and the admitted platform. Their initial package allowlist is only
`lillux` and `ryeos-isolation-protocol`; tests require one exact nonempty test
name. They use offline locked resolution, two jobs, private Cargo/target state
and no caller-selected flags. A test process exiting successfully with zero
matched tests is not positive qualification evidence.

Installed, operator-driven Cargo qualification passed on 2026-09-07 against a
captured clean source checkout: check/build for both selected packages, and
one actual exact test in each. A separate one-file broken snapshot produced
the intended failing test and exit 101. Captures used signed node/project
policy; no host Cargo, network acquisition, operator vault, primary-node reset
or execution-driven HEAD publication was involved. See the exact evidence coordinates
in `tests/e2e/development-cargo/qualification.json`.

The Lillux test exposed an omitted link input in the original recipe. The
platform already contains `libc_nonshared.a`; its `libc.so` is a verified ELF
copy, not the publisher's absolute-path linker script. All three Cargo Tools
and the independent bootstrap probe now explicitly link that admitted archive.
This preserves glibc's nonshared input without a replacement implementation,
host library, new platform pin or executor branch. The corrected Lillux test
links and executes successfully. The strengthened bootstrap fixture is source
coverage; its complete publisher-image reproduction still needs qualification.

These finite operations are now requested by the signed development environment;
new snapshots still require exact target-local bindings and admission. This does
not qualify a worker-to-child invocation: the installed runs were operator-driven.
Worker edit/shared-child/restart/candidate and remote return/apply remain gates.
For exact project preflight, `ryeos validate <ref> --current-head
--no-operator-vault` now uses the existing snapshot/admission path rather than
live-source bindings. It borrows a read-only cache generation, retaining the
requested launch policy without allocating a never-launched CoW workspace.
The focused API check proves live drift cannot replace that generation and
neither a thread nor HEAD publication occurs. Installed worker qualification
remains separate. See `knowledge:ryeos/core/execution/threadless-validation`.
Broader workspace packages and native build dependencies are not implied by
this finite first selection. The approved eight-file worker fixture retains its
earlier three-operation grant until that separate fixture is explicitly updated.

The maintainer `ryeos verify` command also currently fails under this enforced
profile: its offline inspect bootstrap tries to reopen node bundle registrations
inside the restricted process. Signature generation/source-contract checks are
not a claim that this command passed. Complete that remaining preflight path
using retained engine authority, as node-owned bundle verification already does;
do not mount node-private configuration or disable isolation for inspection.

The development node's object-closure policy admits the approximately 576 MB
vendor tree with a 640 MiB aggregate blob ceiling; it still fits the existing
1 GiB response bound. No compiler-specific runtime limit or bypass is added.

Core carries one self-contained `linux-lillux` isolation adapter under the
clean-cut isolation-adapter v10 protocol. The backend is available signed data,
not ambient host setup: bundle membership does not activate it and every
ordinary init profile remains explicitly disabled. The separately selectable
`development` init profile maps to the existing `full` bundle set and enables
the backend plus a finite workload-client node ceiling. It is a policy choice,
not a duplicate bundle distribution. A fresh local development node selects it
with `--bundle-set full --node-profile development`; replacing an existing
policy generation additionally requires the explicit
`--reset-node-policy-generation` decision. Hosted-worker profiles retain their
independently selected network policy.

Policy v3 explicitly supplies the fixed-parent live-project selection and
construction limits. State supplies the existing protected-path floor; Lillux
creates only private connector mounts, preserving authorized source writes
outside those fixed-entry parents. See Standard execution-isolation knowledge
for the exact supported filesystem and symlink boundaries. Policy or runtime
history from the prior authority contract requires explicit scoped retirement,
not a decoder fallback or automatic install-time reset.

The v10 plan carries bounded fixed-parent views, a sorted collection of
daemon-created target channels and explicit PID-namespace and proc-filesystem
choices. The development profile selects fresh PID-local, read-only procfs for
ordinary compiler/executable self lookup, never the host proc mount. Lillux owns namespace, mount,
pivot-root, seccomp, descriptor, pidfd/procfs and process-settle mechanics.
RyeOS layers own only typed authorities, exact adapter identity and signed
policy/data. Do not recreate those OS mechanics in an executor, daemon,
release script, project tool, Python bootstrap, or compiler-specific wrapper.
The same source-to-target descriptor mapping is enforced when isolation is
disabled, and the daemon side remains a typed Lillux byte-stream endpoint.
There is no raw Unix-socket conversion escape hatch.

The scope-backed nested-sandbox cut adds explicit node permission and a
generation-qualified capability, not a workload-specific exemption. Only a
held launch retaining its journaled whole-execution scope can use the broader
`pid_namespace_nested` proc surface and construct child sandboxes. Ordinary
Tools/preparers remain strict-group, read-only task-only proc launches. The
broader mode exposes non-task kernel metadata, but no host processes or scope
controls; native root launch is refused. See Standard execution-isolation
knowledge for this policy/threat boundary. Host supervision must provision an
unprivileged controller with actual delegated scope authority; worker tests
and worker turns must not depend on an operator entering a sudo password.

Prospective bundle initialization validates signed definitions and runs their
ordinary validators under enforcement, but is not process-scope execution
admission. It retains the exact policy without opening/probing the supervised
controller's scope facility from the CLI. Its snapshot grants no scope
capabilities; actual execution still needs a separately qualified generation.
Do not fix this ordering by weakening policy, skipping bundle preflight, or
making ordinary initialization require a privileged supervisor placement.

Writable runtime workspaces use one detached view created and transferred by
the trusted backend before its creator exits. Each child attaches a clone of
that same view in its own isolation namespace; it does not construct another
overlay over the parent's backing directories. The original workspace lifeline
owns the descriptor, and the existing per-launch runtime rows retain exact
borrower membership. Capture quiesces every writer; close/recreation require
proved settlement, including pending dedicated-worker starts. Missing process
attachment is not proof of no contact. The installed v7 authoring graph now
passes actual assembly and independent verification in the same retained view;
the exact evidence is in `tests/e2e/authoring-environment/selection.json`.
Worker-to-child acceptance and restart remain gates before remote use.

Per-child filesystem and network denial are data driven. The signed Tool kind
projects composed `filesystem_authority` and `network_authority` into the
serialized execution plan. Development build/test items author
`captured_execution` and `isolated`; ordinary Tools use signed `node_policy`
defaults. Parent restrictions intersect irreversibly. The effective filesystem
ceiling is supplied before runtime compilation, so host-environment templates
and path mutations cannot capture ambient values before spawn filtering.
Execution-realization testimony records the same retained pair that launch
consumes. Neither restriction is silently accepted when isolation is disabled.
Generic dispatch never recognizes Cargo, Rust, Zig, Codex, or a project name.

Persistent sessions retain their signed protocol document, not merely its ref.
Admission intersects that protocol's workspace/network restrictions into the
plan before hashing. Recovery verifies the retained document against current
node trust and sealed identity; it never adopts a replacement descriptor from
the installed registry. Session testimony must not claim captured/isolated
authority simply because the process is a session.

Every external-content declaration and realized entry names a mandatory
`mount_root` and canonical relative `mount`. `project` targets the admitted
workspace; `execution_runtime` targets a strict child of the shared sandbox
namespace `/ryeos/realizations`. Allowed roots are signed kind/runtime data.
Tool, Worker and Config contracts permit both; Graph and launch-content
dependency contracts currently permit project roots only. Runtime mounts use
the existing descriptor-pinned read-only realization authority, reject mount
and workspace overlaps, and require enforced isolation. They never use the
disabled-mode project-copy path or enter project fold-back exclusions. Exact
realization-member command identity includes the root, path, manifest and
member digest through restart. No host path becomes portable authority.

Signed subprocess descriptors can project schema-validated scalar invocation
parameters through the existing bounded rye-expr/1 runtime template context,
for example one package name into one argv element. The value remains a single
argument; it is not reparsed as a command line. Prefer that ordinary data path
for finite project-tool selectors. Do not add a development argv builder,
shell/JSON shim, compiler-specific dispatcher, or command-multiplexer binary
when the existing Tool schema, config schema, and runtime template already
express the operation.

Tool subprocess protocol selection is likewise existing generic signed data.
Every executable Tool names one protocol admitted by the Tool kind's closed
allowlist. Development compiler/build/test tools select callback-free
`protocol:ryeos/core/opaque`, so their subprocess environment contains no
daemon callback or thread-auth bearer for build scripts to inherit. Tools that
need callbacks select `tool_callback` explicitly; command names never decide.

The native backend intentionally refuses aggregate resource isolation until
Lillux supplies a separately qualified aggregate-quota authority. Whole-execution
scope freeze/termination/recovery does not establish CPU, memory or process-count
ceilings. OS controller mechanics stay in Lillux; neither the presence of a
scope nor per-process rlimits may be represented as aggregate containment.

Initial confined build/test tools use the backend's existing private writable
`/tmp` for Cargo home, target output and compiler temporaries. Their exact
toolchain and dependency trees remain read-only realizations. Do not add a
durable runtime-view/cache authority merely to improve first-run performance;
such reuse requires an explicit lifecycle, recovery, quota and GC contract.

Multiple independent workers may consume the same immutable definitions,
toolchain/dependency identities and base project generation. Each owns its
writable workspace, command history and candidate. Identical content permits
storage reuse; it does not imply that every materializer already deduplicates
physical storage. Within one session, immutable child operations capture a
stable current generation and exclusive operations use its existing workspace
quiescence protocol. That protocol is not concurrent shared-directory editing
authority for unrelated workers. Cargo target directories remain private; there
is no implicit cross-worker writable build cache or automatic candidate merge.

RyeOS-specific environment composition belongs to this repository's development
configuration and Tools. Provider protocol and generic Codex integration remain
in the Codex bundle. Neither may supply node policy, target identity or
credential authority.

## Exact workload-client realization

This artifact serves explicitly selected native CLI ingress. The current
source-local Codex development environment selects structured-session protocol
invocation instead and does not carry this executable as an unused dependency.
The two interfaces share existing dispatch authority; qualifying one does not
qualify the other inside a nested sandbox.

The restricted workload client is an external realization, not a Core bundle
binary and not a file extracted from a runtime image. The official release's
single shared construction solve builds it as a fully static executable from
the exact release source and publishes a separate deterministic archive:

```text
ryeos-workload-client-<version>-x86_64-unknown-linux-gnu.tar.gz
```

The tree contains only `bin/ryeos`, the repository license, and canonical
`RYEOS-BUILD` testimony. The binary embeds the same testimony in a private ELF
section. Packaging and recovery verification require exact equality and bind
the explicit release version, full source revision, UTC build date,
source-date epoch, target, and release profile. An ordinary Cargo build is
marked `development` and cannot pass realization publication. Neither build
path consults Git to invent missing provenance.

The archive is portable content, not target authorization. A project-owned
worker environment or Tool first declares the real manifest digest produced
from this exact tree. Each target then uses the existing external-content
import and pinned-project consumer-binding path. Managed activation remains
restricted to trusted installed-bundle consumers; it is not widened for the
source-local project. Do not author a project environment, toolchain, or
dependency declaration with a placeholder digest. Those signed items land only
after the corresponding real tree has been produced and imported.

The verifier can publish the verified tree directly beneath an already-
admitted named-root directory. It stages beside the requested destination and
renames only the complete verified directory. It never provisions the named
root, changes node policy, or silently selects a fallback location:

```bash
scripts/release/verify-workload-client-realization.sh \
  --version "$version" \
  --source-revision "$source_revision" \
  --build-date "$build_date" \
  --source-date-epoch "$source_date_epoch" \
  --archive "$archive" \
  --checksum "$archive.sha256" \
  --materialize "$named_root/ryeos-workload-client-$version"
```

## Stage-0 platform payload

`.ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml` is the
source-local signed input contract for the first compiler payload. It selects
the exact linux/amd64 publisher-image manifest, immutable dated Rust 1.95.0
component archives, Zig 0.15.2 archive, dated Rust manifest, byte bounds, hashes,
target and source-date epoch. It deliberately contains no output manifest
digest: that digest does not exist until the real tree is produced and
imported.

Stage-0 input schema v3 excludes the live Zig download catalog from acquisition.
That catalog is discovery evidence when selecting a version, not a reproducible
build input: unrelated nightly releases change its bytes. The authored versioned
archive URL, exact size and SHA-256 remain unchanged and mandatory. Never update
a catalog digest to whatever a build happens to download or consult a latest
catalog to replace a pinned archive. The old input schema is rejected, not
silently interpreted under the new contract.

`Dockerfile.development-realizations` runs the transparent publisher without a
package-manager step. The producer downloads only the selected upstream
archives, verifies exact sizes and digests, runs the upstream Rust component
installers into a private tree, extracts Zig into that same platform payload,
records the publisher/input/producer/program coordinates, inventories every
file, normalizes timestamps and emits one deterministic archive. Its signed
`image_member_*` rows additionally name exact canonical members of the pinned
publisher image: source path, destination member, mode, size and SHA-256.
These are explicit authoring sources, not execution-host discovery. They supply
the loader, glibc, libgcc, zlib, native linker support and license notices.
`runtime_alias_*` rows make regular copies of already verified tree members;
they do not introduce symlinks or custom executables. Zig is the native C
compiler/archiver; the upstream GCC driver and Rust LLD provide linking.
The producer explicitly retains the Rust archive-level license/copyright
notices that its component installer does not install. Installer logs,
uninstall scripts and installed-component manifests are excluded: they describe
a mutable installation and embed random authoring-directory paths. The verifier
requires the notices and rejects that bookkeeping in the immutable payload.
The retained input digest covers the canonical flat contract body, not its
replaceable signature header, so re-signing unchanged semantics does not alter
the produced tree.

The official GNU Rust host executables are dynamically linked. The separately
pinned upstream ELF authoring tool rewrites their interpreter and library paths
to `/ryeos/realizations/platform`, with default-library search disabled. The
shared runtime transformation/verifier helper and bootstrap-artifact verifier
now live under
`.ai/tools/ryeos/development/stage0-platform-production/`. Source ownership does
not claim admitted execution: the current combined first-bootstrap producer
still runs in the publisher boundary. Its replacement and admitted Stage1
producer must reuse this canonical behavior, not fork it.
It records every pre/post digest in `RYEOS-ELF-TRANSFORMS`, records selected image
members in `RYEOS-RUNTIME-SOURCES`, and inventories final interpreter/DT_NEEDED
edges in `RYEOS-RUNTIME-DEPENDENCIES`. The verifier resolves every such edge
inside the final tree and refuses undeclared executable scripts. Upstream
GDB/GDBGUI/LLDB launchers are omitted from the finite development operation set.

The selected ELF tool must preserve section order (`--no-sort`). Qualification
found that its ordinary section sorting changed libgcc symbol section
references despite a passing execution smoke test. Every transformed ELF is
therefore also checked for unchanged symbol ownership and function coordinates;
the tool's exit status alone is insufficient. This is an authoring check, not
permission to repair arbitrary worker executables.

The artifact class is `runtime_closed_platform_candidate`, not launch authority.
Its signed `execution_gate` is
`target_local_binding_and_isolated_acceptance_required`. The ordinary target
import/binding and actual Lillux execution proof remain mandatory. Generic
runtime-root support supplies the mount boundary, not implicit bytes. Ambient
host `/lib`, `/lib64`, `/usr` or `/bin`, undeclared image members, and custom
compiler wrappers remain forbidden. Produced build-script/test executables must
use this same loader/library closure through declared compiler arguments;
checking the compiler's own ELF edges alone does not prove its descendants.

The verifier checks the closed archive shape, complete retained tree
inventory, dated Rust-manifest hash, build testimony, executable dependency
evidence and signed bounds before optionally publishing one sibling-staged
directory. It does not clear the execution gate:

```bash
bash .ai/tools/ryeos/development/stage0-platform-production/lib/verify-bootstrap-artifact.sh \
  --inputs .ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml \
  --producer scripts/release/produce-development-toolchain-stage0.sh \
  --archive "$stage0_archive" \
  --checksum "$stage0_archive.sha256" \
  --materialize "$named_root/stage0-toolchain"
```

Qualifying Stage 0 runs the pinned publisher twice into distinct output
directories (and preferably distinct empty caches), then passes both archive /
checksum pairs to
`tests/e2e/development-toolchain-stage0/test-artifact.sh`. The test requires
byte-identical archives and checksums before applying the full verifier once; it
does not build or acquire anything itself. The tracked artifact tests consume
already-built archives, do not compile RyeOS, and never manufacture substitute
binaries. Artifact production and those tests remain explicit qualification
steps rather than release-time fallback logic.

## Import and target-local binding

Archive verification is not RyeOS launch authority. After materialization
beneath a node-policy named root, use the existing operator path to create the
ordinary content manifest:

```bash
ryeos external-content import \
  <named-root-id> <relative-tree> tree content <maximum-bytes>
```

Select `large_content` instead of `content` when the exact tree exceeds the
ordinary tier; the qualified Stage-0 compiler tree uses `large_content`.
The selected node must independently admit that tier and its actual totals.

The result supplies `staging_id`, `request_digest`, and the real
`manifest_hash`. Only then may the project Tool be authored with a locator-free
`mode: pinned` tree declaration naming that exact hash, an explicit permitted
`mount_root`, and a canonical relative `mount`. Sign the Tool, create the exact
project snapshot, and bind the staging
capability to that pinned project consumer:

```bash
ryeos --project <project-path> external-content bind \
  <staging_id> <request_digest> <manifest_hash> \
  tool:ryeos/development/<tool> pinned_project \
  <project_snapshot_hash>
```

The same sequence runs independently on every placement target. Portable
content may move through RyeOS object-closure transfer, but each node creates
its own operator-authorized binding. Managed activation remains restricted to
trusted installed-bundle consumers.

These declarative Tools have no separately executed source tree. Their binding
therefore retains explicit null source-closure evidence plus the mandatory
exact generation, verified publisher and effective-program digest. This does
not relax source admission for Python or worker programs that do execute a
source closure. Binding v3 / head epoch 4 expresses this distinction; use the
existing scoped binding reset for a predecessor installed namespace, not a
history reset, fabricated source closure or generic subprocess source policy.

The Stage-0 dependency realization cannot be authored honestly before the
compiler payload, its exact runtime-root dependencies, and their real RyeOS
manifests are admitted. Its producer is the later signed project Tool that
invokes the exact admitted Cargo with `vendor --locked --versioned-dirs`; its
build descendants use only the resulting exact offline tree with `--locked
--frozen --offline`. A shell process using publisher-image Cargo, an ambient
Cargo home, or a checked-in placeholder manifest would create a competing
bootstrap authority, so none is included here.

For ordinary Tools that need child executable search, use the existing
`env_config.env_paths` owner. Seed `env_config.env.PATH` with an explicit empty
string, then prepend only the declared realization directories. The seed
prevents host inheritance; the path operation supplies the existing typed
`RuntimePathMutation` provenance. Do not put `PATH` in `config.env`, which is
ordinary descriptor data and correctly refuses protected environment names.
Do not weaken the vault/environment contract, append host directories, or add
a parallel Tool environment field. The main command remains an exact
`realization:` command, and its captured-filesystem ceiling still confines
all descendants. Worker environments separately use their already-owned
`executable_search` contract.

The adjacent `authoring-environment-production` development knowledge owns
the producer's exact Python runtime, portable input-mode verification and
strict output receipts. Those operations use the same retained-result and
separate import/binding authorities, not a second environment bootstrap.

## Development operation and script ownership

The source-local Tools currently authored under `tools/ryeos/development/`
include `platform-inspect`, `format-check`, and `format-file`. They select the
real imported platform manifest, captured-filesystem execution and isolated
network. Inspection/checking borrow an immutable current generation;
formatting borrows the exclusive workspace. Target-local bindings and standalone
CLI platform-inspect/format-check have passed on the bounded fixture. The
worker's changed-generation negative check and shared-exclusive format/readback
remain unqualified. The three finite Cargo operations above have operator-driven
installed evidence against the observed vendor manifest, not worker-driven proof.

Resolved operation timeouts use the existing project execution configuration
at `.ai/config/execution/execution.yaml`. Its per-item selections override
bundle-wide defaults. A descriptor's `config.timeout_secs` is lower-precedence
input, not an immutable ceiling; do not infer an executed deadline from that
field alone. Inspect the admitted plan and its configuration provenance. Node
limits and parent workload-client lifetime/authority remain independent limits.

Source ownership and execution authorization are separate decisions.
**Not granted to a hosted worker does not mean belongs in scripts.** Operator-only
production, packaging, signing and verification may still be ordinary signed
Tools. A reusable operation has one canonical implementation beside its Tool;
project selections live in Config; composition lives in Graphs; qualification
fixtures and node/container acceptance live under tests/E2E.

External entrypoints are justified only by a specific pre-RyeOS, host-install,
or external-orchestration requirement. "Release", "bootstrap" and "not
worker-side" are not blanket exemptions. A first-bootstrap entry identifies
exactly which immutable inputs it seeds and calls the canonical implementation.
It must not become a second implementation after admitted execution exists.

The complete family inventory, actual disposition, dependency requirements and
completion criteria are in `development-operation-ownership.md`. The compiler
behavior fixture now lives in `tests/e2e/development-toolchain-stage0/`.
Relocation does not rebuild historical archives or change their recorded
producer hashes. Tests of genuinely external scripts can remain beside them.

The four `repository-validation/` Tools reuse the already admitted sealed-source
Python runtime and select rules from
`config:development/ryeos/repository-validation`. They require no Cargo,
shell/search executable, network, private node mount or additional worker grant.
The canonical checks are shared with explicit pre-install CI invocation;
host CI execution alone is not RyeOS admission evidence. Rule selection names
the current crate layout and refuses absent/empty inputs, rather than suppressing
search errors or treating obsolete roots as a successful check.

No ambient interpreter, shell, curl, Docker, TLS/resolver files or host libraries
may be hidden behind a Tool descriptor. Missing closure or acquisition authority
remains an explicit gate. Keep the verified authoring environment unchanged
while independently qualifying fresh utility build support and compilation.
Completed operator-driven Cargo evidence remains distinct from still-open
worker edit/child/completion-fence/candidate/restart/remote acceptance.
