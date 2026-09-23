<!-- ryeos:signed:2026-09-23T08:16:34Z:40a863927bc164c3f7c2cce63b0df2cb62f01b35008b8690c716da3b12680572:qnCwmuMJuZfDcPQusTy/R0EjB3d+N4JrnEO71lDiFpWOEzuFPRCHzIb8FVwvFp7NjZOv48POJn/qog+pmKi4Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: source-local-bundle-development
title: Source-Local Bundle Development
description: Source-local workflow, project-bundle, realization, and confinement contracts
entry_type: reference
version: "1.24.0"
```

# Source-Local Bundle Development

## Status

The original local command, state-root, signing, and bundle-smoke workflow is
implemented. The project-bundle and development-realization sections below
also record source-authored work that still has the explicit artifact,
runtime-root, signing, and qualification gates they name. They must not be read
as evidence that remote build/test execution is already available.

For continuation of the native release proof, start with the dated checkpoint
below and `development-operation-ownership.md`. Historical successful compiler
or binding evidence elsewhere in this document is not evidence for a newly
produced platform identity.

### Native release proof checkpoint — 2026-09-23

The four release-environment prerequisites (Python, platform, Cargo-vendor and
static-link inputs) have retained qualifications. The child-owned execution
binding correction is installed. Calibration at RyeOS ProjectSnapshot
`ec9f7354dfa822947d446f0895f30a52bf1ac73a66f6f34a41c96c6312685df1`
launched portable Tool `T-1f4208a6-f276-33bb-381a-4f4a0243102c`, which
failed because `${tool_dir}` named an invisible installed-host path. A later
retry at clean snapshot
`103476e7b4939ce1c5a85e79f3862a166a1bdbfaa310e5812d1c704d57734479`
launched Tool `T-40964241-4393-f52d-cbd4-a68b6e785678`; its relative
`.ai/tools/ryeos/bundle-release/lib/native-build.py` argument resolved inside
the pinned workspace, but the source was not mounted there. Investigation of
`resolve_executor_source_policy` showed the exact cause: `source_scope` is
selected from verified executor-chain intermediates, not from the root Tool.
Root-only declarations therefore provided no source admission. The eleven
release Tools now point to one signed `tool:ryeos/bundle-release/runtime`
intermediate. That intermediate owns the narrower `item_directory` scope;
each root retains its own exact command, interpreter, product slots and
workspace-relative script argument. The focused asset test covers all eleven.
This corrected chain is source-signed but not yet installed or live-proven.
No successful calibration, catalog bootstrap or consumer bundle activation is
established. Continue from the controlling plan below; do not reproduce
qualified prerequisites, clear Cargo caches, or treat a host file's existence
as sandbox visibility.

Audit the whole lane before retrying a live build: each child Tool needs its own
signed execution declarations, explicit product selections and exact-consumer
composition. A Graph's sealed input bytes are not authority for the child to
execute them. Build inputs are stage-specific: compiler/vendor/static inputs
belong to native build; capture and qualification take Python and their subject.
Producer relationship parameters must equal the producer's admitted parameters,
including for calibration. An empty parameter object is not a wildcard.
Qualification policy, selected subject consumer and retained verifier thread
must name the same actual verifier. Do not substitute its wrapper Graph's
thread. Preserve the producer's actual failure before asking for product
acceptance, so downstream missing-evidence messages do not hide launch failures.

The source correction supplies child-owned declarations and exact selections
through the signed Graph/Tool handoff; calibration retains a trusted fixed
template identity and signs only exact invocation parameters. The constrained
substrate receipt recipe is publisher-signed, and direct qualifier Tools own
their subject and verifier identity. Focused API, recipe, Core, calibration,
composition and release-asset checks pass. These are source checks only. Build
of the coupled daemon/CLI/publisher/core-tools artifacts and population of the
signed 11-bundle release-authority set completed on 2026-09-23. The live node
still runs the prior generation. Install the coupled generation with the
release-authority policy reset, take its exact source snapshot and rebind the
retained prerequisites before retrying live calibration; only the subsequent
catalog and distinct-consumer update can close the release proof.

#### Retained prerequisite evidence and ownership

The objective is independently published bundle generations served by a stable
bundle-source node, with consumer verification and activation without another
host-image build/publication. The development-toolchain campaign supplies the
release authority's prerequisite evidence; it is not itself the bundle-transfer
proof and must not become work repeated for every bundle update.

The controlling implementation continuation is section 0 of
`.tmp/bundle-release-execution-closure-implementation-plan-20260921.md`.
This knowledge checkpoint owns durable evidence and operating boundaries.
The current stage is release-authority prerequisite qualification. The finish
line requires both a data-only and a binary-bearing non-Core bundle successor
published and activated on another node with unchanged substrate identity,
including failure/retry, pre-commit recovery and explicit refusal of
post-execution rollback in v1. The original publication acceptance also retains
a second consumer on the prior selection, restarts source/consumers and restores
a clean source from exported closure; a single successful update is insufficient.
The full development-worker proof, Stage1 reproduction, self-hosted CI cutover
and persistent build-cache work are separate campaigns. Add authoring utilities
only when an exact remaining release operation declares that dependency.

#### Retained capture and later consumer authority

The capture recipe and consumer relationship are separate signed authorities.
The witness permanently identifies the admitted producer, recipe binding,
parameters, product declaration and captured bytes. A consumer relationship may
be authored later in another trusted Config; selection records that Config's
exact ref, digest and relationship, then checks that its producer coordinate and
bounded product requirement match the witness. Adding a release consumer does
not require recapturing or requalifying unchanged product bytes.

Do not require a consumer slot's relationship Config to be the historical
capture recipe Config. That collapses production testimony and consumption
authority, prevents legitimate retained-product reuse, and would make every new
bundle builder repeat prerequisite production. The current signed consumer
relationship still owns the exact consumer and slot, producer coordinate,
product bounds, qualification policy and claims. The authenticated witness still
owns the exact product bytes and capture provenance. Either side changing must
fail its own comparison; neither side may overwrite the other's identity.

The operator completed the Stage-0 bootstrap export and canonical artifact
verification. The second export reused the completed BuildKit solve and finished
in approximately four seconds. This was cache reuse, not a second independent
production. The local files are user-owned and their archive digest was also
checked against the checksum file when recording this checkpoint:

| Coordinate | Recorded value |
| --- | --- |
| Output directory | `/tmp/ryeos-development-stage0-verified` |
| Archive | `ryeos-development-toolchain-stage0-rust-1.95.0-zig-0.15.2-x86_64-unknown-linux-gnu.tar.gz` |
| Archive bytes | `304969653` |
| Archive SHA-256 | `29a19fb73c84078ca19c013e62476a075d991b90bed5e65a986f0e1cf4fa0910` |
| Verification inputs | The output directory's retained `source/` tree |
| Established | Canonical bootstrap verification of a runtime-closed platform candidate |
| Not established by this run | RyeOS import, target binding, independent reproduction, isolated acceptance, release calibration, consumer bundle activation |

These are local checkpoint coordinates, not portable Config values or durable
CAS retention. `/tmp` may be cleared. Confirm the files and hashes still exist
before proceeding; preserve this verified result through the existing retained
content path, not by assuming a temporary path is a permanent artifact store.
The saved `source/` is the input copy used by the bootstrap helper, not a RyeOS
project snapshot or signed execution receipt.

Historical bootstrap sequence, completed through platform production/capture:
the coordinates below supersede its initial missing-input observations. Do not
repeat these steps as the next action.

1. Use the retained `source/` verifier, input Config and producer together to
   materialize this exact archive under an existing node-policy named root.
   Inspect that node's current admission and bounds before selecting a path.
   Do not verify historical output against whichever producer happens to be
   in the current checkout, rerun population, or rebuild Stage 0 unnecessarily.
2. Import the actual tree through existing external-content authority. Measure
   its real entries/bytes and use the admitted content tier. The archive SHA-256
   above is **not** its RyeOS tree-manifest hash. Record the returned staging,
   request and manifest coordinates; never invent them.
3. Reconcile project-owned platform declarations against that actual manifest.
   `platform-production/assemble.py` initially named the historical
   `98bceddd5b4024d5963eeac8c579e6d4e79c24577980fa9f88bce9ae3151d316`
   tree. Do not assume the corrected Stage-0 closure has the same identity.
   Inspect dependent Config/Tool declarations, sign exact changed items, retain
   the intended project snapshot and bind its exact consumers. Do not silently
   reuse a binding or qualification from another manifest or generation.
4. Complete platform and Cargo-vendor production, capture, independent
   qualification and composition using existing project relationships and
   retained generations. Inspect existing Python evidence for reuse at its
   exact coordinate; do not restart completed prerequisite work by default.
5. Complete release-authority calibration, then exercise signed publication,
   authenticated closure transfer and target-local verification/activation on
   another node. Record before/after substrate identity and installed bundle
   generation to demonstrate the update did not rebuild or publish a host image.
   Preserve failure/retry/recovery evidence required by that transaction.

The continuation has now completed the materialization, import and initial
consumer-binding part of that sequence. These are the exact live coordinates;
they are operational evidence for this release-authority node, not portable
values to copy into another node's policy:

| Coordinate | Recorded value |
| --- | --- |
| Materialized tree | `/tmp/ryeos-development-stage0-materialized` |
| Manifest entries | `19551` |
| Materialized bytes | `1026378565` |
| Release-authority import root | `/home/leo/.local/share/ryeos-release-authority-imports/development-platform-stage0` |
| Imported manifest hash | `b74b15f0877a2c4941e8bb5660b9f827091aeeb2b2a50b00311101c903895c00` |
| Import request hash | `470a49b2ce78aa66e58245d48a098f61dc074571df48ac4c85b2079f83fc7416` |
| Import staging id | `upload-18d77c7d2ee49a57-5930-1` |
| Pinned project snapshot | `4d6d1bdb083857747e79b6c5e644b0edced6631f058813eba1d1f02d58a43987` |
| Stage-0 assembler binding | `ff4005eac6dd891576a6ea36448d91371a034a5b3d076a95a4da2635ac369b59` |
| Reused Python manifest | `800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf` |
| Python assembler binding | `c4ff6391725f4b3f4cd21c1472f2b5a45791ffa4a27ecd0180dace79c3679f88` |
| First expected failed run | `T-ce4ceab9-28bf-c9af-a915-6d55deabfcbb` — no active Python binding for the new snapshot |
| Corrected platform run | `T-e0b70c8f-a456-7581-3cb5-f4cdb5e5ab05` — Graph completed; terminal retained-result capture was still pending at handoff |

That corrected run subsequently reached durable `thread_completed` success.
Its result workspace capture is
`063977fadfe44ba13f7f55da7b21451eaaca6dd000533485d3a04b25a2bc1fc2`.
Product capture returned witness
`1dbfc122307ad9741ae776898b376f73ae6c05e5d1b7b7118f007cc38e5042fb`
and coordinate
`958fa30ff5af461437db25a28b3d1c3ba77932f37b39546c4de2b0ab52902e95`;
the captured product reproduces the imported `b74b15...` manifest exactly.

The independent verifier's unqualified `subject` slot was composed at the same
project snapshot, producing binding
`50433b61304c67f7c3908a840913359fd0396c9c67d907e4672cbc97ace7d5b4`.
The verifier's literal bootstrap-Python pin required a separate selected-D1
binding after composition. Its ordinary installed-bundle D0 binding is
`25db278f629368378fa85391b983aaf35f610fb324648056d1335c32bfd41c53`;
the selected-D1 pinned-generation binding is
`3a294155f142a06edff65ed9f70545e30365ae532b4be9d55453cd6f4f32b7d9`.
This distinction is intentional: product composition does not silently clone
literal external-content authority, and an installed bundle with a project
relationship becomes generation-scoped after selection.

Verifier thread `T-10beede8-be0e-abb7-b0cd-7baad87eb90c` then proved admission
and isolation reached exec, but failed with exit 125 because the verifier tried
to execute the dynamically linked bootstrap Python directly. The file existed;
the isolated namespace correctly lacked its ambient musl interpreter. The
platform and Cargo-vendor verifier Tools were corrected to enter Python through
the retained `lib/ld-musl-x86_64.so.1`, matching the established authoring
runtime contract. Subsequent attempts passed that startup boundary; this is
historical diagnosis, not a pending instruction to reinstall that correction.

Later verifier thread `T-31b3151b-9ce7-b396-a81c-ce8ca44bb39f` failed at the
Cargo identity probe with insufficient diagnostics. Thread
`T-bb23222c-4ad9-cdff-04c7-f98ce7161a24` exposed Cargo's missing-home error.
The attempted v1.0.6 source correction set `HOME=/tmp` and added an assertion
for that value. It was signed and passed focused host-side checks, but was not
installed or independently qualified at the pause. It is an unaccepted patch,
not evidence that the platform or complete build environment is qualified.

The existing enforced sandbox supplies a fresh private `/tmp` and controls
`TMPDIR`. A one-shot verifier can create a scoped temporary directory there,
with a dedicated Cargo state directory and, if needed, an explicit private
home child for that subprocess. It must not inherit host HOME or use the whole
scratch root as HOME. Persistent-session runtime views have a separate durable
lifecycle and are unnecessary for this identity probe. Keep the identity and
closure checks; correct their environment instead of removing the failing gate.

The node's signed `external_content` policy originally contained no named
roots. The operator added the `release-inputs` root to the live signed policy,
using the inspected device and inode for the durable directory above, through
the supported stop / policy-apply / start lifecycle. Do not commit those
machine-local coordinates to the release-authority seed profile. A new node
must create and inspect its own durable import root and apply its own signed
policy generation before importing content.

The active source declarations now name the imported `b74b15...` manifest only
for `tool:ryeos/development/platform-production/assemble`. The old
`98bced...` value remains valid only where it is explicitly historical test or
qualification evidence. Cargo-vendor production and authoring-utility
production receive the independently qualified platform product through the
`platform_to_cargo_vendor_production` and
`platform_to_authoring_utility_production` relationships. They must never bind
the raw bootstrap manifest as their own ambient platform dependency.

The consolidated environment review has now produced source corrections:
platform verifier v1.0.7 owns separate temporary Cargo/home state; vendor
verifier v1.0.2 checks the actual admitted workspace and lock instead of
inventing an all-packages dependency graph; release GNU Python slots now match
its embedded `python-gnu` runtime mount. Eleven prerequisite, four vendor and
12 release closure tests pass. These are source checks, not installed proof.

That same review found unresolved native/Core builder closure: host build
scripts/proc macros need different execution linkage from shipped binaries,
the chosen GCC lacks its C frontend, and the current static recipe lacks
required libc/startup inputs. The implementation plan's section 0 audit records
the exact remaining gate. Resolve and qualify that finite build recipe before
another installation/calibration attempt. Do not guess new mounts, wrappers,
toolchain identities or a Stage-0 rebuild from these findings.

The next bounded diagnostic rejected the proposed final Cargo loader override:
the dependency-free build-script/proc-macro/C fixture compiled in an isolated
retained-platform namespace, but its ELF still named the platform interpreter
despite a trailing substrate-loader argument. Running it with an explicitly
selected loader did not prove the delivered ABI. Exact owned-bin selection is
now corrected in both native and Core builders (12 release closure tests pass).
Both release builders now normalize only copied unsigned final ELF products
before capture and record the transformation hashes. Shared recipe-owned GCC
specs supply host linkage with explicit Cargo target selection; retained Zig
supplies C compilation. The bounded dynamic fixture passed direct execution
without the build-platform mount, using retained DSOs at substrate ABI paths.
Static-link input closure remains unresolved; this is not installed-substrate
or Lillux proof. Preserve this distinction: compiler success, ABI validity,
Lillux qualification and publication acceptance are separate evidence gates.

Ownership remains explicit: `bundle-release` owns payload selection, build
recipes, output normalization and publication. The development bundle owns
toolchain/vendor input production. Missing static inputs must enter through a
declared development product, never acquisition embedded in release tooling.
Reusing a tiny development test fixture does not start the broader development
worker qualification campaign or require reproduction of unchanged Stage-0.

The supplementary static inputs are now measured against the same pinned
publisher image in `.ai/config/development/ryeos/static-link-inputs.yaml`.
The upstream `libm.a` is a linker script referring to `libm-2.41.a` and
`libmvec.a` by absolute distribution paths; retain its bytes and layout, and
prove lookup inside the supplementary sysroot rather than exposing host
`/usr/lib`. Eleven files (9,989,612 bytes) include the measured glibc notice.
These pins are not an imported or qualified product identity. Export,
development-owned production, admission and static compile/run evidence
remain outstanding; do not treat this inventory as completion.

Follow-up: verified supplementary inputs were exported to
`/tmp/ryeos-static-link-inputs`. The small dynamic/static release diagnostic
now passes with unchanged upstream archive/script bytes: explicit linker
`--sysroot=%R` resolves the absolute math-script members within the declared
supplementary tree. Its static executable runs without loader/library mounts.
Export and finite static recipe evidence are therefore complete, but retained
development-product production, import, qualification and release consumer
binding remain outstanding. The installed node and actual Core payloads have
not been proved by this diagnostic.

Current-node import now succeeded after disk capacity was restored. Verified
files reside under the existing admitted `release-inputs` root at
`static-link-inputs`; input manifest is
`e16652fd10ff718182fe2b09fd8a4c913fc8c994baacf833e3e6a29a69750ebf`
(`external_large_content_manifest`, 17 entries, 9,989,612 bytes).
The subsequent signed producer ran successfully at project snapshot
`d28b446c6c2f2c7490c41ce3edcbbbc0e1c4c56be77c4ec9046dfe812d1fa11f`.
Graph chain and terminal thread are both
`T-42b7ed0a-23b8-2faa-664a-f5a55c94f4a5`; product capture returned witness
`7f5f8e375ced4160dbf907b38d88b373c99c163e4806c2c3743b417f72b50a7e`
and coordinate
`6125d74e07597e6646fdf5516b3b4333e83b6d1f6867673ab933d4529c44b588`.
The product reproduces the imported manifest exactly. Bootstrap Python was
reused through its active retained binding, without acquisition. This proves
production and capture, not independent qualification or release admission.
The new Standard verifier still needs installation and exact selected bindings.
Do not rerun production merely because the verifier generation changes.

Qualification revealed that this first static witness retained only the
unqualified verifier relationship. A qualification policy must be present in
the captured producer recipe; a later consumer declaration cannot add it to
old testimony. Recipe v1.0.1 adds the exact policy-bearing native/core release
relationships. The corrected product is witness
`562606efca5076fa7ed77a03ccc20a888e156653e3ad481ad5b3aeefacfae93b`,
coordinate `7b6eb131aae8acda868bb03adc91997ee825cb912beebd9cb80fb7d19cc718ff`,
from graph `T-47e5fb4d-a93d-0380-c98a-e0e1a3166f99` at snapshot
`84d3ab11026bcd931b91a7844af5b7f6b087a1cc5e909e59eda6969aa9d746d5`.
It reproduces the same `e16652...` manifest. Use this witness for qualification;
the earlier witness is historical evidence only. This recapture was required
by changed recipe authority, not changed input bytes.

Live platform verifier `T-157f276c-6a5c-cfe5-0e4e-c50cac8af6dc` passed the
Cargo probe but failed because `collect2 --version` delegates to an absent
`ld`. Source v1.0.8 selects retained LLD explicitly with `-fuse-ld=lld` and
probe-local COMPILER_PATH, leaving PATH empty. Its isolated retained-platform
probe and 12 focused tests pass; install and repeat live qualification before
claiming platform acceptance. No compiler alias or Stage-0 rebuild is needed.

The v1.0.8 live run `T-a66eb988-da21-aa57-a620-ad102bb29e93` passed
collect2 but rejected GCC's invocation-dependent banner. Verifier v1.0.9
expects `gcc (Debian 14.2.0-19) 14.2.0`, matching retained `native/bin/gcc`,
not the publisher image's original executable basename. All seven identity
probes and the complete isolated diagnostic passed using retained bootstrap
Python and Stage-0 (19,551 entries, 1,026,378,565 bytes, 47 ELF closures).
Thirteen focused tests pass. This diagnostic is not authority-issued platform
qualification; install v1.0.9 and rerun the live verifier next.

After v1.0.9 installation, live thread
`T-5ea90cfa-19e8-f696-3d05-556bd6f58bb8` passed with both ABI and target
claims and 47 ELF closures. Issuing qualification exposed an output contract
error: those two claims were emitted target-first, while the authority requires
a sorted unique set. Verifier v1.0.10 emits the policy's ABI-first order;
the focused test checks it, all 13 tests pass, and both files are signed.
This correction requires source installation, a refreshed bootstrap Python
binding for the new consumer generation, and another live verifier execution
before qualification can be issued. Retain Stage-0 and its original witness.

That v1.0.10 installation and rerun are complete. Live thread
`T-9ca081eb-f134-ea3e-45a2-7b40b4081c52` passed with sorted ABI/target
claims and all 47 ELF closures. Authority issued qualification
`53857da6e4663bf2417ac807ed92957bcbd61efbf8a5da96a41cb3a674c80d31`
for the unchanged Stage-0 witness
`1dbfc122307ad9741ae776898b376f73ae6c05e5d1b7b7118f007cc38e5042fb`
under `platform_to_cargo_vendor_production`. This is current-node platform
qualification, not a Cargo-vendor or release-build qualification.

Static-input live verifier `T-8e99201c-2088-074e-2197-50b2a9dbbc27` completed
with exit zero. Qualification
`3b895459903f116ba7cf60fb20dba33ee7c7ef1ce506ded9d70df63e65e37e4e`
was issued for corrected witness `562606...`; both native and Core seed
relationship requests resolve idempotently to that evidence. It proves the
17-entry, 11-file, 9,989,612-byte checksum closure, not compiler execution.
The prior concurrent launch failed on fork-sensitive CAS guard quiescence;
its bound thread was confirmed terminal before this sequential retry. Keep
prerequisite admissions sequential; do not weaken descriptor safety or blindly
retry a delivery-uncertain launch.

The subsequent finite cc-rs diagnostic caught a separate release-recipe defect:
cc-rs appends the Rust target spelling, which Zig rejects. The shared recipe
now supplies final `CFLAGS` and `CXXFLAGS` with Zig's `x86_64-linux-gnu` spelling,
preserving ordinary cc-rs defaults without a compiler wrapper. Exact locked
cc/shlex/find-msvc-tools archives were checksum-verified and used as nonparallel
scratch path dependencies. Both dynamic and static C-linkage fixtures passed,
including retained GNU AR and host/target separation. This does not qualify
the vendor product, C++ ABI, actual Core payloads or installed substrate.

Platform production, capture and qualification are now durable. Changing the
verifier did not invalidate unchanged subject bytes. Historical binding
coordinates above must still be checked against the selected current identity.

At the earlier checkpoint, the release-authority node had neither the historical
`e8f5ded8d3327ed0ce91798f8f0264ac7bf4f120a85936d3a3dc675be7ad2dbe`
registry-input manifest nor a local-capture registry product witness. The
2026-09-07 registry qualification belongs to another node and an older source
generation; it is evidence, not transferable current authority. Cargo-vendor
continuation must therefore use the canonical bounded registry acquisition and
offline registry-production path to create a fresh retained product on this
node, then compose that exact unqualified registry product alongside the
qualified platform product. A fresh acquisition records full upstream index
response hashes, so unrelated registry changes may change its receipt and tree
manifest even with the same lock and archives. Measure and bind the actual
acquired tree, updating its signed declaration if needed; do not assume it
reproduces the historical `e8f5...` identity. This missing prerequisite does
not require reacquiring or rebuilding Stage 0.

The current-node registry path is now complete through production/capture.
The bounded acquisition imported manifest
`9de036779fbc33f8e33bc5068a286ac27813e80586d1b766ee7db99a1fa7a2e8`;
`graph:ryeos/development/registry-production` completed as thread
`T-dc0bb4c3-7444-bfe9-8dc1-2f81cb38f354` with 414 packages and no network
acquisition. Capture issued witness
`932a12d3f5eb733ddb2354e333f270d81652147966dce2b2f4e1a0526c52f207`
for output manifest
`c73499d571ab14a599d4a9f32d0d29942c079632d6c1762fc280ec63da460227`.
This registry witness is an exact unqualified input; it does not inherit the
older node's qualification.

Cargo-vendor composition selects qualified `platform` and exact
`registry-inputs`. The child Cargo Tool independently declares its pinned
platform; admission now deduplicates it only when the full retained identity
equals the inherited Graph realization, while rejecting same-ID disagreement
and mount overlap. Installed-node graph
`T-364d9357-8873-099f-0c7a-ad94793d0f4e` completed, including production
and lock retention. The retained `Cargo.lock` is 113,004 bytes with SHA-256
`9e8e1a93918f8e229cdbb8a037aa1a4fbbccbe5efe1257519396bb8fc3103f09`;
the producer reported `source_mutated: false`. Capture issued witness
`a77eb108d4f1667dbbba3f5c1b995ba0a11c348288ac5cfdb6f7750402d09323`,
manifest `eb30513b80548dbd9a28f97ec21078e87f673c642373f2678c351f9cf753843c`,
26,265 entries and 581,661,813 bytes. These coordinates preserve diagnostic
evidence, **not a qualified vendor product**.

The independent verifier `T-339c1724-f647-ea7c-8af2-25ad6fb36d30`
rejected that witness: `cc-1.2.60/.cargo-checksum.json` names four
`src/target/*.rs` files absent from the captured tree. A read-only retained
manifest scan found no other package mismatch. The release-authority policy's
component-wide `target/` ignore also applied to workspace-output capture,
silently stripping nested vendored source. Its signed source profile now uses
root-anchored `/target/`, which still excludes the project build directory but
retains nested package source; focused matcher and policy tests pass. Install
the corrected policy generation, take a new project snapshot under it, bind
the exact retained inputs to that generation, then reproduce/capture/verify a
new vendor product. Do not qualify or reuse the incomplete witness. This
policy correction does not require rebuilding Stage-0, registry inputs or the
daemon. The finalizer has a dedicated signed Tool entrypoint; it does not own
public-registry acquisition or Cargo vendoring.

Under the installed root-only policy, snapshot
`1633788e59e2ac266504f4fc7777c1bfc51f5984818bcfd50b0540acb0f96e43`
rebound the unchanged platform, registry and bootstrap Python identities.
Cargo-vendor thread `T-c7590324-c8e8-f4d7-b9e6-392a5fddd9e4` completed and
capture issued witness
`16967633e22f697993faf66ede6c93d2bd299497f4741cddc825861a6f09c005`,
manifest `ede513268c395bb79d21933e02fc317ccc9b56fbf18e00ae7d14b3efd10565c7`,
26,270 entries and 581,717,585 bytes. Its manifest includes all four
formerly missing `cc-1.2.60/src/target/*.rs` files. Independent verifier
`T-709a88d7-6320-08de-a4ac-631469656691` passed the lock-closure and
offline-checksum claims with 414 registry packages, 21,623 verified files,
579,399,937 verified bytes, no Git dependencies and no network contact.

Qualification issuance first refused an object closure of 671,156,525 bytes
against the release-authority policy's 640 MiB aggregate blob ceiling. A 768
MiB wire-transfer trial failed init because base64 and envelope overhead would
exceed the hard 1 GiB response maximum. The 704 MiB response-bounded policy
was installed, but retrying qualification refused at 742,904,407 bytes. These
failures report the first threshold crossing, not the complete closure.
Read-only inspection of the verifier's retained Python, vendor and platform
manifests found 40,537 distinct CAS blobs totaling 1,007,471,595 bytes.
This is local verification of one aggregate execution realization, not a
single remote transfer. The correction is a separately signed local
verification aggregate of 65,536 blobs and 1 GiB, while remote admission and
its response bound remain unchanged. Focused tests and compilation passed. An
initial daemon-only population left `ryeos init` and core-tools on the prior
policy schema. Policy-reader population now couples `ryeos-cli`, `ryeosd` and
`ryeos-core-tools`, with focused regression coverage. The exact host generation
is installed and the upgrade journal is clear. Qualification then issued
coordinate `beed07545f5b09fa99348e8831deb6feaac1621f64c29eea1870ac2ade822bc9`
and qualification
`5f37c3e8f795a65d8bdc32213d498279941744c6f7a707c76d4f63f4cc10b518`
from the retained witness and successful verifier. Do not split aggregate
verification into per-manifest passes or reproduce this qualified product.
Do not mistake successful verifier execution for issued product qualification,
and do not reproduce Stage-0, registry inputs or vendor output unless an exact
generation check requires it.

Current calibration is defined by
`crates/daemon/ryeos-app/src/bundle_publication/calibration.rs`:
`CalibrationEnvironmentSelection` requires Python, platform and Cargo-vendor
product selections. The portable build Graph's Python-only runtime does not
erase those authority requirements. Do not weaken calibration to obtain a
green portable-bundle test, or claim that source tests prove live activation.

Calibration and catalog bootstrap establish the initial authority. Routine
updates reuse its measurements while the selected identities and signed policy
remain admissible; invalidating changes require recalibration. A data-only
bundle uses the Python-only portable build graph. A binary-bearing bundle uses
its selected targets and qualified platform/vendor inputs; shared dependencies
may compile. Neither recurring path requires a new Stage-0 export, host-image
build or daemon build just because a bundle changed.

The previously installed release-authority app root is
`/home/leo/.local/share/ryeos-release-authority`. Recheck its status, installed
generation and selected policies before live work; its earlier successful
startup is not a claim about its current health. Consult installed command help
and current handlers before issuing import/binding commands. Keep checks focused;
no full Cargo/image rebuild or broad host-service cleanup is implied by this
checkpoint. Report the exact remaining gate instead of sending another guessed
sudo command.

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
structural limits, never widen them. A previously verified Stage-0 platform had
about 384 MB of small-file CAS content plus five large-store files; its development
profile therefore permits a 512 MiB aggregate CAS blob closure. Local binding,
verification and launch proof consume the selected node's `object_closure`
policy, not generic control-plane defaults. No policy entry selects a compiler
version or grants an unbound project access to that content.
Those historical measurements do not establish the corrected artifact's import
totals; measure the actual manifest before evaluating policy admission.

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

Current release-authority acquisition used this entry and the current Cargo.lock
(414 locked registry entries). The finished tree is retained at
`/home/leo/.local/share/ryeos-release-authority-imports/development-registry-inputs`.
Node import under the admitted `release-inputs` root measured 1,129 entries,
77,004,545 bytes and manifest
`9de036779fbc33f8e33bc5068a286ac27813e80586d1b766ee7db99a1fa7a2e8`.
The staging request is `5e5c94d92b13cb477496ae72ed1423852ab0181c1d3f6b7623bef87f851d88d8`
at `upload-18d78ff65d54bb95-2d30-1`. The signed registry-production Tool now
pins this measured input; it was bound, executed and captured as recorded in
the current-node checkpoint above. Its captured product remains unqualified.

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
package-manager step. The external acquisition entry downloads only the selected
upstream archives and captures exact selected publisher-image members. The
offline producer verifies that complete input directory, runs the upstream Rust
component installers into a private tree, extracts Zig into that same platform
payload, records the publisher/input/producer/program coordinates, inventories
every file, normalizes timestamps and emits one deterministic archive. Its signed
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
Config parser, offline producer, runtime transformation/verifier helper and
bootstrap-artifact verifier now live under
`.ai/tools/ryeos/development/stage0-platform-production/`. The external
`scripts/release/acquire-development-toolchain-stage0.sh` boundary only obtains
the exact selected archives and publisher-image members, then atomically emits
an acquisition directory. Bootstrap passes that directory to the canonical
offline producer; a future admitted Stage1 run must call the same producer.
Source ownership does not itself claim admitted execution.

The Tool-owned `stage0-platform-production/lib/bootstrap.py` helper coordinates
the external publisher export and canonical artifact verification. Run it as
the ordinary user, elevating only the Buildx subprocess when required:

```sh
python3 .ai/tools/ryeos/development/stage0-platform-production/lib/bootstrap.py \
  --output /tmp/ryeos-development-stage0-verified \
  -- sudo /home/leo/.docker/cli-plugins/docker-buildx
```

The command prefix after `--` is explicit host bootstrap configuration; omit it
when the default `docker buildx` can access the Docker socket. The output
directory must not exist and its parent must exist. The helper saves the
selected source files, builds and verifies against those same bytes, and
accepts only the bounded archive/checksum pair from the Buildx tar export.
Only Buildx is elevated: output remains user-owned without a recursive ownership
change. Existing exports and the publisher's BuildKit cache are left intact.
Failure removes only the newly reserved output directory; success retains the
archive, checksum and exact `source/` inputs. This is seed-toolchain bootstrap,
not a command to repeat for each bundle update.

Verified export is not import, consumer binding, independent reproduction or
isolation qualification. Derive and import the actual external-content manifest
through RyeOS; never reuse a historical platform manifest after changing the
Stage-0 closure. Consumer binding remains scoped to the intended pinned project
snapshot. Production, capture, independent qualification and composition then
use the existing graphs. Current release-authority calibration requires
qualified Python, platform and Cargo-vendor evidence; the portable bundle
graph's smaller runtime requirements do not remove those calibration gates.
The bootstrap helper neither changes policy nor proves consumer activation.

For durable input evidence, place the complete acquired directory at
`stage0-acquisition` in the selected project generation and execute
`graph:ryeos/development/stage0-acquisition-capture`. Its
`config:development/ryeos/stage0-acquisition-products` recipe uses the existing
`retained_project` source. The graph performs no action and authors no consumer
relationship: it records the exact retained tree but does not call acquisition,
produce a compiler, or grant later execution.
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
bash "$stage0_source/.ai/tools/ryeos/development/stage0-platform-production/lib/verify-bootstrap-artifact.sh" \
  --inputs "$stage0_source/.ai/config/development/ryeos/stage0-platform-x86_64-linux.yaml" \
  --producer "$stage0_source/.ai/tools/ryeos/development/stage0-platform-production/lib/produce.sh" \
  --archive "$stage0_archive" \
  --checksum "$stage0_archive.sha256" \
  --materialize "$named_root/stage0-toolchain"
```

Here `stage0_source` is the verified export's retained `source/` directory,
`stage0_archive` is its exact archive, and `named_root` is an inspected,
operator-admitted import root. These are deliberately selected coordinates,
not permission to choose a host path or use the current checkout implicitly.

Independent Stage-0 archive-reproducibility qualification runs the pinned publisher twice into distinct output
directories (and preferably distinct empty caches), then passes both archive /
checksum pairs to
`tests/e2e/development-toolchain-stage0/test-artifact.sh`. The test requires
byte-identical archives and checksums before applying the full verifier once; it
does not build or acquire anything itself. The tracked artifact tests consume
already-built archives, do not compile RyeOS, and never manufacture substitute
binaries. Artifact production and those tests remain explicit qualification
steps rather than release-time fallback logic.

This separate reproducibility campaign is outside the current bundle-update
completion sequence. The current gate is independent live qualification of the
already retained platform product. It neither requires repeating this Docker
export sequence nor establishes two independent archive builds. Keep both
claims separate in evidence and status reports.

The offline producer requires an exact Bash/archive/helper process runtime.
Current admitted authoring build support supplies Bash and most helpers but not
`tar` or `gzip`. Until an existing environment-product owner supplies and
qualifies those programs and their complete closure, do not expose a Stage1
Tool/Graph, inherit host PATH, install packages during production, or substitute
a general-purpose toolbox. The acquisition tree can already be retained by its
return-only product Graph; the producer relationship remains intentionally
absent until this finite dependency is present. No new executor or content
authority is required.

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
