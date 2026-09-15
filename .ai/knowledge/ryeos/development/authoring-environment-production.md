<!-- ryeos:signed:2026-09-15T01:10:03Z:4cfb6b4e0ce0e9848a9bb11453a37891197063763b58f2411086be7f71305d18:MiJNhgW3Lu7VfCpYkAeAvcJb4PbU4wzZmVnmkjT14EVdq951Q1XpPKNK6gajm+2eBKlD0u7WS62tH11w5SfSCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/development
tags: [development, authoring, external-content, production]
version: "1.4.1"
description: Finite production and qualification of the shared command environment.
---

# Authoring-environment production

## Bootstrap realization

The production and qualification tools use the exact pinned
`producer-python` realization as their sealed bootstrap interpreter. A fresh
development node can acquire and retain those exact bytes through the trusted
standard-bundle recipe:

```sh
ryeos external-content activate \
  config:development/ryeos/producer-python-activation \
  online
```

This recipe binds the retained manifest to the installed native-authoring
verifier named by the recipe. Before launching a project production Graph, the
target's local operator must use `external-content import-binding` and
`external-content bind` to bind that same retained manifest to each exact Tool
and project snapshot that consumes it. Managed activation and general binding
remain local-operator authority; a configured remote operator does not acquire
those capabilities merely because it owns the later producer products.

Do not copy a realization directory from another node or add an ad hoc locator:
activation verifies the signed archive recipe, the node's acquisition policy,
and the final pinned tree manifest before retaining the witness. This recipe
does not provision the separately pinned authoring source, build-support,
platform, registry, or GNU Python inputs. Those inputs must come from their own
publisher-authored acquisition artifacts and exact target-local bindings; their
absence remains a closed preflight failure.

The source-local namespace
`tools/ryeos/development/authoring-environment-production/` owns finite,
ordinary environment Tool operations, including `prepare`, `assemble` and `verify`.
The separate `assemble-build-support` Tool produces compiler helper inputs;
`build-utilities` compiles the finite utility selection from exact sources using
those helpers and Stage0. Neither operation publishes the worker environment.
The adjacent `runtime`
executes their sealed Python source using an exact admitted interpreter.
It is not the runtime supplied to the final worker.

The exact Python executable discovers its own admitted standard library. Its
prefix is checked before loading verified source, and UTF-8 is selected through
argv. The runtime does not override protected `PYTHONHOME`, `PYTHONPATH` or
locale names. Missing runtime content fails qualification; it never permits
host path inheritance or a weaker node environment policy.

The selected `config:development/ryeos/authoring-environment-inputs` enumerates
every input's hash, bytes and mode, selected output members, exact relocation
targets and upstream provenance. Literal bootstrap inputs remain exact pins.
Produced inputs are selected by named product witnesses through the signed
relationship Configs. Graph-owned shared inputs reach their inline leaves
through ordinary admitted realization inheritance; leaf-owned interpreter and
source inputs still require their own exact bindings. Large source trees require
explicit large-content authority. The production Tools perform no acquisition,
network, signing, binding or publication operation.

Input modes describe portable manifest identity, matching Lillux: an ordinary
regular file normalizes to `0755` if any executable bit is set, otherwise
`0644`. Immutable cache materializations may expose those bytes as `0555` or
`0444`. Preparation, assembly and utility-support checks use that input-only
projection without chmodding shared content. Exact hashes, sizes, complete
inventories and regular-file/bounds checks still apply. Newly produced trees
and receipts instead verify their exact physical permission bits.

## Flow through existing owners

Production and product selection retain one exact admitted operator throughout.
That may be the node's configured local operator or a configured remote operator
authenticated by the source node and an origin-bound target grant. A remote
producer, its product witnesses, independent qualification and consuming worker
must have the same owner; target-local products do not become interchangeable
with another operator's products. No private signing key moves between nodes.

The target's local operator provisions the literal bootstrap input bindings.
An admitted remote operator can then execute the finite producer operations and
capture, qualify and compose its own retained products under separately granted
service scopes. These are operator operations, not additional worker grants.
Named-root filesystem import, general binding and managed activation remain
local-operator operations. Current binding authorizer grant digests remain
revocable; changing a grant invalidates bindings signed against its old digest.
Establish the complete finite operator grant before production and binding.
When a literal dependency is bound to an exact selected D1, the local binding
request also names the canonical `product_owner_principal`. The target verifies
that current admitted operator grant and checks every selected witness against
that owner; the field separates product ownership from local binding authority
and does not delegate the bind to the remote operator.

The received-witness API remains a separate same-operator transfer contract.
Do not relabel historical foreign-owner witnesses as fresh local captures or
assume that source-node trust delegates another operator's products.

1. Acquire and bind exact literal bootstrap inputs through existing content
   owners. Acquisition is separate from offline production; there is no
   turn-time package installer or automatically learned replacement pin.
2. Run the prepared-input and build-support recorded Graph wrappers. Their
   awaited producers own declared output partitions in private retained CoW
   workspaces. The daemon accepts named product witnesses from successful exact
   terminals, not from caller-authored inventory or mutable workspace reads.
3. Compose the build-support witness into the utilities producer's declared
   root slot. Run its recorded wrapper with that explicit selection. Compose
   the resulting utilities and prepared-input witnesses into final assembly.
4. Run `authoring-environment-production-recorded` with both selections. Its
   producer runs `assemble` then `verify` in one shared retained workspace and
   returns named distribution, runtime and shell products. Build scratch is not
   promoted into a runtime product.
5. Select the runtime and prepared-input products into the independent Bundle
   qualification Tool. Qualify using its exact successful root/thread, then
   compose the resulting witness and qualification into the Worker Config.
   Artifact possession, producer testimony and checksums are not authorization.
6. Separately qualify actual Worker command execution, durable completion,
   fenced candidate retention and independent candidate validation. A product
   qualification does not qualify a future worker-edited candidate.

### Run the retained production graph

Push the signed source generation containing the Graphs, Configs and Tools into
the selected principal's project HEAD. Bind literal inputs to their exact
consumers **at that same source snapshot** before launch.
Project consumer bindings include the snapshot hash: existing content manifests
can be reused, but bindings to an older source generation cannot authorize the
new one. Use `ryeos external-content import-binding <exact-active-binding-hash>
<maximum-bytes>` for a fresh receipt, then bind that receipt to the new exact
Tool/snapshot. The source binding must still be active under the local
operator's current authority; a completed receipt cannot simply be reused for
another consumer. Product slots instead use `external-content compose-product`
with an explicit consumer context and complete selection list. A Graph declares
only inputs it owns; it does not guess or redeclare every child's environment.

From that project, using the selected node's ordinary CLI connection, run:

```sh
ryeos execute graph:ryeos/development/authoring-prepared-inputs-production-recorded --current-head --async --no-stream --input '{}'
```

Each recorded wrapper awaits an independent producer root. Within the final
assembly producer, `assemble` and `verify` borrow that producer's retained CoW
workspace and original pinned subject generation. The second sees the first's
products without making them new source-definition or consumer-binding
authority. Those opaque leaves receive no callback bearer; their producer root
owns terminal retention. The execution Config allows both bounded leaves plus
producer and wrapper overhead.
Failure stops the producer before later actions. Its leaf actions are live;
the outer ordinary recorded action can reuse the accepted retained products
without invoking the builder again. Reuse is not independent reproduction.
Selected-input wrappers take their typed selections from `inputs`, not a
separate parameter namespace, and forward only the authored root slots.

For producer-to-Tool actions, keep the default inherited child policy; do not
add `--retain-child-results`. These leaves use inline actions rather than
detached or follow children: assembly needs one uninterrupted shared-workspace
sequence. This does not turn the wrapper-to-producer boundary into an inline
borrower. Two separate `--current-head`
Tool invocations also do not compose: retaining a result does not advance HEAD,
so `verify` would not see `assemble`'s output. No live filesystem copy or project
apply-snapshot operation is needed.

After wrapper completion, consume the exact `ryeos.product_build_accepted_result.v1`
return and select witnesses by product name. Its producer snapshot remains the
original admitted generation even when a later consumer uses another snapshot.
Output bytes live in terminal workspace-output captures and named content
manifests paired with the source-only result snapshot, not inside that source
snapshot.
The inner Tool inventories are reproduction evidence, not witness publication
authority. Admitted execution, independent product qualification, worker
acceptance and restart recovery remain distinct checks; source tests do not
establish any of them.

Product selections explicitly name their witness source: `local_capture`, or
`received` with the exact local acceptance hash. Receipt uses the existing
configured-remote admission transport; it does not transfer a consumer binding
or turn an origin qualification into local qualification. A receiving consumer
must satisfy its own admitted verifier and current policy. Full receipt proof
is retained for authorization, recovery and storage ownership, but the receipt
coordinate alone does not change semantic program identity. The immutable
product witness, selected content, relationship and qualification still do.
Current receipt transports ordinary CAS blobs; an actual large-object sidecar
is refused, not treated as transferred merely because its manifest arrived.

`prepare` consumes the pinned raw tree at
`/ryeos/realizations/authoring-source-inputs` and resolves both the assembly-input
and `config:development/ryeos/authoring-utility-sources` contracts. Raw layout:
`bootstrap/` holds the exact binary/source archive pair; `workload/selected-package.tar.gz`
holds the selected resource package; `upstreams/` holds the named source/notice
files; `elf/` and the remaining `notices/` hold exact previously selected members.
Archive members are bounded, regular, exact-name selections; unselected entries
are not extracted. The operation verifies the complete selected inventory's hashes, sizes and physical modes
before advertising completion. It does not execute the acquired binaries,
contact a container daemon, use host PATH, acquire packages or manufacture pins.

`assemble` refuses existing output and staging paths. It checks all selected
inputs before running admitted upstream readelf/patchelf through their supplied
loader. Only declared dynamic members are relocated; static PIE remains
unchanged. Interpreter/library closure, executable modes and symbol ownership/
function coordinates are verified. Upstream ELF programs own interpretation;
there is no custom binary patcher.

`verify` independently reassembles into a separate private directory and compares
the complete file/mode/hash inventory. Both operations return compact output
coordinates and an inventory checksum, not embedded artifact bytes, fake CAS
receipts or publication authority. Their enclosing execution owns process-group
and resource limits. Failed staging is preserved for diagnosis and never bound.

## Artifact boundary

`environment/` contains 43 ordinary commands, a closed runtime for the selected
shell, and notices. `corresponding-sources/`, provenance and inventory accompany
the distributable output. Runtime binaries resolve at
`/ryeos/realizations/authoring-tools`. That fixed root requires enforced
isolation; it does not change ordinary live/filesystem execution or the node's
default backend policy.

The runtime does not include child compilers, project dependencies or a privileged
RyeOS client. No new package kind, solver, environment registry or manifest store
is introduced. The existing content manifest and target-local binding remain
the materialization/authority owners.

Artifact-only reproduction and an offline empty-root probe have passed.
Admitted preparation and single-root assembly also passed on 2026-09-06, with
no operator vault or network access. Exact completed-thread result imports
matched all four selected manifests: prepared inputs, runtime environment,
complete production output and shell file. Project HEAD remained unchanged.
`tests/e2e/authoring-environment/selection.json` retains the exact thread,
capsule, result and import coordinates, separately from the expected pins.

The shell and runtime now have exact installed Worker/Config bindings on the
disposable target. The complete output has a separate project Config binding,
`config:development/ryeos/authoring-distribution`, which retains corresponding
sources and notices as well as runtime bytes. It is a non-executable consumer,
not a second worker environment or an input to independent reproduction.

Independent admitted verification passed in the retained assemble/verify graph
on 2026-09-07. `selection.json` records both leaves, their common retained
workspace, the exact root capsule and matching inventory. The installed
integration uses one original retained view with exact transitive borrower
freeze/cleanup fencing; it does not remount the same overlay separately for
each inline child. No project HEAD or binding was published by the graph.
Restart and real hosted candidate qualification remain gates. A successful
production graph is not worker/child acceptance, and its artifact reproduction
check is not independent qualification of a worker's edited candidate.

## Finite GNU CPython runtime

The adjacent GNU CPython producer consumes exact pinned upstream install and
metadata archives plus the existing prepared ELF support product. It writes
directly into its sole declared private-workspace output root,
`products/gnu-python-distribution`. There is no sibling scratch output or
second publication transaction: only a successful terminal can yield named
products, while an unsuccessful terminal may retain its in-partition files for
diagnosis. The retained product keeps the `python/` member because its admitted
mount is `/ryeos/realizations/python-gnu` and its relocated runtime root is
`/ryeos/realizations/python-gnu/python`.

The finite production Config declares every dependency edit. The producer uses
the admitted `patchelf` to replace the one observed slash-bearing Python edge
and to add the exact `libgcc_s.so.1` edge required by the inventoried native
extension contract. It refuses missing, unused, already-present or additional
edits, proves the exact before/after `DT_NEEDED` difference, and then applies
the ordinary relocation owner. Recursive resolution stays within `python/lib`,
with the exact loader, RUNPATH and `NODEFLIB` contract. A provider with a
`DT_SONAME` must match the requested dependency name. Absence of `DT_SONAME` is
valid when the exact admitted `python/lib/<DT_NEEDED>` filename resolves the
edge; the pinned Tcl libraries exercise this ordinary ELF case. Absence is not
an alias for a present mismatch.

The producer retains a bounded canonical ELF inventory and its digest as
reproduction evidence. Neither that inventory nor the relocation receipt is
independent qualification testimony. The Standard Bundle qualifier instead
runs the exact selected interpreter, rejects ambient loader overrides, reads
its actual bounded process mappings, and requires the mapped
`libgcc_s.so.1` and `libpthread.so.0` objects to be the exact product members.
It opens both each absolute mapped path and its normal dependency name with
`RTLD_NOLOAD`, requires the handles to identify the same loaded object, and
uses `dlvsym` to probe the finite required `GCC_*` and `GLIBC_*` symbol
versions. The built-in zlib claim remains separate and checks the retained
build topology and observed runtime behavior.

This proves only the stated runtime and startup-provider claims for the exact
selected product. It does not qualify every native extension, every future
dependency, universal GNU ABI compatibility, or the provenance of copied
libraries. Exact image-member identities and retained notices remain
production inputs; a notice is not represented as corresponding source when
the matching source archive is absent. A real GNU production completed on
2026-09-09, independently of qualification publication or downstream
consumption.

## Utility-build dependency gate

`lib/utilities.py` owns the finite static-utility build recipe. It reuses the
existing Stage0 realization at `/ryeos/realizations/platform` for Zig rather
than creating another compiler platform. The source configuration's image
coordinate is historical provenance, not permission to access Docker or its
filesystem. The source archive's Zig copy supplies corresponding-source/license
evidence, not a second compiler installation.

Stage0 does not supply Make, a configure shell or their complete helper closure.
The build recipe therefore also requires a separate exact build-support input.
`assemble-build-support` consumes the finite signed
`config:development/ryeos/authoring-build-support-inputs` selection. Its adjacent
`lib/build_support.py` reuses the existing ELF relocation/closure verifier;
upstream readelf/patchelf still own ELF interpretation and transformation.
It assembles selected shell, Make, helper, loader, library and notice files at
the existing utility consumer's `/ryeos/realizations/authoring-build-support`
mount. No image lookup, host search, acquisition, binding or publication occurs
inside the operation. This is production of exact content, not a new compiler,
package manager or environment authority.

The first installed assembly passed on 2026-09-07: 88 files, 21,394,807 bytes,
with exact retained-result import and unchanged source HEAD. The resulting
`config:development/ryeos/authoring-build-support` records the consumer's existing
input/command/notice contract. Its inventory checks are derived assertions;
the existing content manifest and target-local consumer binding authorize use.
The E2E fixture `tests/e2e/authoring-environment/build_support_probe.py` separately
checks executable loading, nested Make/shell execution, static compile/link,
strip and output identity. Materialize/sign that fixture only in a disposable
qualification project; it is not a production operation or a worker grant.

Assembly or a small compiler probe does not qualify all upstream configure and
Make descendants. Full fresh utility compilation passed separately on 2026-09-07,
including fresh Git executing its final runtime shell. The original archives remain preserved;
new retained builds carry their own upstream sources, recipe and execution
evidence. `build-support-qualification.json` records actual gates separately.

The first installed subprocess probe loaded all 37 helpers, then failed during
static musl compilation with `ProcessFdQuotaExceeded` at the target's signed
`isolation.policy.limits.open_files: 1024`. The pinned Zig `cc` driver rejects
`-j1`; the similarly named `build-exe` option is not a valid compiler-driver
substitute for ordinary configure/Make. A larger finite development budget
was selected through the existing node-policy owner with operator approval:
only the disposable target's open-file limit changed to 4096. The rerun passed
all 37 loading checks and nested static compilation, stripping and execution.
Six selected product files were retained; no compiler cache entered the result.
Do not raise a child rlimit in Python/shell, add an engine/compiler exception,
or switch to host libraries to bypass this gate. The probe's corrected fixture
keeps caches in private isolated `/tmp`, retaining only selected outputs and
bounded diagnostics. No new snapshot exclusions are needed for its scratch.

`tests/e2e/authoring-environment/utility_build_probe.py` then exercises the sole
`lib/utilities.py` production recipe against exact admitted source archives,
support and Stage0. It is a separately signed disposable E2E entry, with its
finite deadline in the existing project execution Config. It keeps extracted
sources/caches in isolated scratch and retains selected products and bounded
build logs. The ordinary `build-utilities` Tool now shares its request, selection
and retained-output boundary through `lib/utility_production.py`; it does not
duplicate the compiler recipe. Its installed execution passed on 2026-09-07:
80 files / 92,435,329 bytes retained, with no binding publication. The evidence
index records this separately from the E2E pass. Independent retained-manifest
comparison matched all 41 executable entries between these two fresh builds;
the updated recipe/evidence files are deliberately different. Source HEAD stayed
unchanged. Neither entry is a worker grant.

The full recipe's first run also exposed two build/runtime distinctions.
Autoconf requires explicit `LD` even when `CC` is Zig: use the existing exact
Stage0 `native/bin/ld.lld`, without extending host PATH. Zlib's configure needs
`tee`; the revised support selection adds the already-pinned static helper,
including historical archive/source provenance. Its installed assembly and
38-helper nested probe passed separately from the initial 37-helper artifact.

Git's upstream `SHELL_PATH` controls build generators as well as the path baked
into the delivered program. The recipe selects its admitted build shell there
and uses upstream's separate C-quoted setting for the Config-selected final
authoring shell (`runtime_shell`). Do not bake a build-support/scratch path or
ambient `/bin/sh` into the artifact. The E2E fixture binds the already-qualified
authoring runtime and exercises an actual fresh-Git shell alias, not merely a
string check. That extra fixture input is not a root-worker grant or a new
utility source. The build and final runtime closures must both be qualified.
