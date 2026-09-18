# RyeOS merged-worktree integration review

Date: 2026-09-17 (Pacific/Auckland)

Reviewed snapshot: `83f32c890bbd6ea23c9dcd39908339c229e363b0` (`next`, merge of hosted runtime qualification; v0.5.92).

Review baseline: `c003e665e`, expanded to include the remote-workflow foundations of the recent landings. The aggregate diff is 796 files, 93,263 insertions and 15,780 deletions, including generated browser assets. This is a risk-directed, cross-layer review, not a claim that every line of this large range was exhaustively audited.

Implementation status: no fixes, source edits, configuration changes, deployments, activation calls, privileged cgroup mutations, or external messages. This report is the requested output. The earlier review ran local tests/builds; those produced normal build artifacts and eventually exhausted disk. This deeper pass avoided Cargo builds and large artifacts.

## Executive assessment

**Do not sign off the merged system yet.** There are substantive backend integration failures, not just browser defects:

- Three independent incompatibilities prevent the new contained OCI hook/bootstrap sequence from reaching a running controller.
- OCI recovery does not retire a dead but non-leaf controller hierarchy itself.
- Installed-bundle activation and pinned-project launch disagree about the identity of the shipped Codex environment consumer.
- A backend session helper returns a descriptor pathname after closing its owning descriptor; separate projection code treats descriptor pathnames as durable project identities.
- A resident resource operation can accept more request attributions than settlement can represent, leaving financial settlement permanently failing after 1,025 requests.
- The previously identified browser transport, overlay, tab and focus defects remain present.

The architecture's intended authority separation is generally recognizable in the code reviewed: signed content does not automatically grant execution, exact process ownership precedes release, financial issue replay is separate from live release authority, recovery retains historical admission, and containment claims are distinguished from external fencing and trusted process groups. The failures are principally at integrations between those individually strict contracts. Adding permissive fallback would be the wrong general remedy.

Evidence labels used below:

- **Reproduced primitive/runtime behavior:** executed the relevant packaged WASM, framing implementation, Linux syscall behavior, or in-memory projection comparison.
- **Source-confirmed:** traced the actual production producer/consumer call chain and incompatible conditions; not a claim of a live installed end-to-end run.
- **Conditional:** requires the stated deployment or lifetime scenario.

P1 means a major supported path is blocked and should be addressed before sign-off. P2 means a concrete failure under a narrower interaction, lifetime or recovery condition. No P0 or confirmed containment escape was established.

| Priority | Finding | Main owner |
|---|---|---|
| P1 | F01: proc-root traversal incompatible with secure directory opener | OCI adapter / Lillux |
| P1 | F02: root validation uses unprivileged delegation-owner invariant | Node bootstrap / Lillux |
| P1 | F03: mount-root delegation sent to child-delegation provisioner | Lillux OCI/native integration |
| P1 | F05: activated environment binding differs from pinned launch binding | External content / executor |
| P1 | F06: returned project path outlives its descriptor | Assistant backend authority |
| P1 | F09: Map-to-JSON mismatch plus failed-append retry loop | Browser transport |
| P1 | F10: absent overlay/notice renderer | Browser shared-model integration |
| P2 | F04: empty non-leaf cgroup recovery cannot release | OCI recovery |
| P2 | F07: ephemeral descriptor path used as stored project identity | Backend projections |
| P2 | F08: attribution admission exceeds settlement representation | Resource accounting / pool |
| P2 | F11/F12: group-tab and shared-focus misrouting | Browser interaction |
| P2, inherited | I01: next request races prior request cleanup | Local-inference protocol |

The OCI startup defects mask one another: `prestart traversal (F01) → privileged binding validation (F02) → controller provisioning (F03)`. Fixing the first observed error is not sufficient to qualify the sequence.

## 1. Scope reconstruction

The review checked actual `.worktrees/` directories, `git worktree list`, branch ancestry, merge history and the `next` reflog. Reflog inspection matters because several landings were fast-forwards and do not appear as simple merge commits on `next`.

| Worktree / stream | Landed reference | Scope reviewed |
|---|---|---|
| `assistant-ui-w0` | `06ee963f8` | Compiled signed UI authority, session/project identity, logical work, attention, approvals, candidate evidence, shared-model/browser integration |
| `external-container-workers` | `836774538`, merged by `f53627989` | External placement readiness, independent cleanup/release requirements, retained historical authority, interaction with local/trusted worker lanes |
| `hosted-runtime-qualification` | `fbd9c329b`, merged by `83f32c890` | Earlier readiness/preflight plus newly landed OCI hook, protected binding, cgroup topology, bootstrap, recovery, image/profile and evidence gates |
| `local-inference-gpu-arc3` | `2db5b1b1d` | Resource admission, exact device observations, durable exclusivity, accounting, occupancy watchdog, persistent workers, model authoring/contracts and runtime qualification boundaries |
| `ui-visual-system` | `bc58d9b70`; signed-entry follow-up `6c18233d8` | Svelte renderer, typed WASM boundary, nested workspaces, interaction parity, asset closure |
| `remote-workflow-cli-ux` | `5b0298ea7` | Signed command parsing and shared admission; caller authority and execution controls |
| `remote-workflow-products` | `f75fc52f0` | Target product selection, bundle/project consumer identity, child launch and retained recovery |
| `remote-workflow-qualification` | `f01dd5d54` | Recovery/materialization changes and the explicit unfinished live qualification ledger |

The older container-hosted work is historical context only. The newly landed OCI design explicitly rejects its daemon-owned external provisioning topology. No uncommitted/historical worktree changes were treated as landed code.

The earlier report stopped at `00de99fae`. This report supersedes that scope limitation: the additional `00de99fae..83f32c890` OCI/release diff (38 files, about 3,083 insertions) was included. The worktree was clean at the start of this deeper pass; the release edits observed late in the earlier pass are now committed in `39ee898ac`.

## 2. Backend findings

### F01 — P1: OCI prestart cannot traverse the process-root paths it supplies

**Evidence:** source-confirmed and Linux syscall behavior reproduced. New OCI landing.

Primary location: `crates/host-adapters/lillux-oci-hook/src/main.rs:301`.

Prestart passes `/proc/{init_pid}/root/data/app` and `/proc/{init_pid}/root/run/ryeos` to `PinnedDirectory::open`. That secure API walks every component with `O_DIRECTORY | O_NOFOLLOW` (`crates/kernel/lillux/src/secure_fs.rs:643`). `/proc/<pid>/root` is a procfs magic symlink, so the walk fails with `ENOTDIR` at `root`, before the application or binding directory is reached. Correct OCI state, root privilege and correct directory ownership do not make this traversal valid.

`install_oci_controller_mount` repeats the incompatible traversal for `/proc/{init_pid}/root/sys/fs/cgroup` at `crates/kernel/lillux/src/process_control/cgroup.rs:332`.

Read-only reproduction of the critical operation:

```python
flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
pid_directory = os.open(f"/proc/{os.getpid()}", flags)
os.open("root", flags, dir_fd=pid_directory)
# NotADirectoryError, errno 20
```

**Impact:** the installed hook cannot complete prestart or publish the protected binding. The contained image cannot start through the advertised path.

**Correction boundary:** provide an explicit exact-process-root pinning primitive in Lillux, then traverse descendants relative to that retained descriptor. Do not relax generic no-follow traversal for arbitrary paths. Test the actual Rust adapter against a real process-root descriptor, including process replacement and root replacement refusal.

### F02 — P1: root bootstrap rejects the correctly delegated non-root cgroup owner

**Evidence:** source-confirmed; independently followed from node bootstrap through Lillux. New OCI landing. This is a second blocker after F01 is corrected.

Primary locations: `crates/kernel/lillux/src/process_control/scope.rs:667` and `crates/daemon/ryeos-node/src/host_runtime.rs:241`.

The hook assigns controller-root R to UID/GID 10001 (`cgroup.rs:167`). Root-only `exec_external_controller` calls `binding.validate()` before dropping privileges. Validation of an OCI binding calls `require_oci_generation`, which opens a `ProcessScopeProvider`, which calls `DelegatedCgroup::open`. That opener checks that the cgroup owner equals the **current effective UID** (`cgroup.rs:724`). At this point the caller is root, while R correctly belongs to 10001.

Privilege dropping is configured only later through `scope.rs:822` and the controller command setup. There is no expected-controller-account override on the validation path.

**Impact:** a correctly prepared binding fails privileged bootstrap even after process-root traversal is fixed.

**Correction boundary:** distinguish administrator-side validation of an exact expected account from unprivileged controller-side provider opening. Preserve the latter's ownership invariant. Test root validation followed by an irreversible transition to the selected account; also test wrong-owner rejection at both stages.

### F03 — P1: OCI mount-root configuration violates the native provisioner's topology assumption

**Evidence:** source-confirmed. New OCI landing. Independent subsequent startup blocker.

Primary location: `crates/kernel/lillux/src/process_control/scope.rs:638`.

The hook emits `LinuxCgroupV2 { parent: "/sys/fs/cgroup" }`, because it mounts the already prepared R at that location inside the container. `exec_controller_inner` unconditionally calls the existing `provision_controller(parent, uid, gid)` (`scope.rs:814`). That provisioner opens `path.parent()` and requires it to be a cgroup-v2 directory (`cgroup.rs:561` through `571`). For this configuration, the enclosing path is `/sys/fs`, which is sysfs, not cgroup2.

**Impact:** even after F02 is fixed, bootstrap still cannot place and launch the controller. The native-host child-delegation API does not accept the mount-root topology produced by the OCI adapter.

**Correction boundary:** explicitly support an already prepared, identity-verified mount-root delegation, or change the adapter's topology coherently. Do not bypass the physical ancestry/generation checks. The test needs to run hook output through the real bootstrap consumer, not validate either side separately.

### F04 — P2: dead OCI trees containing empty child cgroups cannot be retired by recovery

**Evidence:** source-confirmed, conditional recovery failure. New OCI landing; downstream of the startup blockers.

Primary location: `crates/kernel/lillux/src/process_control/cgroup.rs:311`.

`retire_ended_oci_controller` proves exact init death and recursive `populated=false`, then removes R with one `unlinkat(..., AT_REMOVEDIR)`. It does not retire child cgroup directories. Normal bootstrap creates `R/controller` (`cgroup.rs:613`); interrupted execution can leave other empty child scopes. Killing processes empties these cgroups but does not remove their directories. The production recovery path has no bottom-up retirement of those children.

**Trigger:** the runtime leaves the dead hierarchy present and the administrator invokes the documented hook recovery. R is empty of processes but not a leaf directory.

**Impact:** removal fails, the lifecycle record cannot reach `Released`, and volume reuse stays blocked despite proved process death. If the external runtime already recursively removed C, this condition does not arise. This is a fail-closed recovery/liveness defect, not an unsafe reuse acceptance.

**Correction boundary:** exact, bottom-up retirement of verified empty descendants, or an explicit enforced external teardown prerequisite. Test daemon crash, retained controller leaf, empty execution descendants, repeated poststop/recover, and unrelated-sibling preservation.

### F05 — P1: activation and pinned launch disagree on the default Codex environment consumer

**Evidence:** source-confirmed cross-stream failure, not a live daemon reproduction. Introduced by `34a6b4dab` in the reviewed range.

Primary location: `crates/daemon/ryeos-app/src/external_content_admission.rs:83`.

The new bundle-consumer branch chooses `PinnedProject` whenever the outer subject has an operational generation. Its comment describes consumers deliberately composing pinned-project relationships, but the condition applies to every bundled consumer under that subject, including fixed-pin consumers with no such relationships.

Concrete shipped path:

1. `bundles/codex/.ai/config/codex/environments/default.yaml` has `external_product_slots: []` and a fixed locator-free `command-tools` tree pin.
2. Managed environment activation publishes an `InstalledBundle` binding, with no project snapshot (`crates/daemon/ryeos-api/src/handlers/external_content_activate.rs:949`).
3. The documented hosted flow then starts a session using the default environment and `--current-head` (`bundles/codex/.ai/knowledge/codex/hosted-activation.md:286`). That is a pinned project launch.
4. Session content-dependency admission receives the outer subject authority unchanged (`crates/engine/ryeos-executor/src/execution/persistent_session.rs:947`, `:1521`). `consumer_authority` therefore requests a `PinnedProject` consumer for the environment.
5. `active_binding_from_store` derives its exact head key from that consumer (`crates/daemon/ryeos-app/src/operator_external_content.rs:1504`). It cannot find the different installed-bundle binding produced by activation.

The worker's own admission deliberately uses a projectless subject, so this finding specifically concerns the environment/content-dependency path, not a blanket claim that all bundled workers fail.

**Impact:** successful prescribed activation does not make the documented pinned Codex session launch ready. Repeating the same managed activation cannot create the missing generation-specific head.

**Correction boundary:** determine consumer identity from the actual admitted composition/relationship closure, while preserving generation scoping for consumers that really depend on project products. Do not add an unconditional installed-bundle fallback for project-dependent consumers. Add one test for default fixed-pin environment activation followed by `--current-head`, and one negative test for substitution of an installed binding into a genuinely project-dependent composed consumer.

### F06 — P1: session project helper returns a path to an already closed descriptor

**Evidence:** source-confirmed and descriptor lifetime behavior reproduced. Assistant authority landing.

Primary location: `crates/daemon/ryeos-ui/src/seat_auth.rs:53`.

`project_path()` calls `project_directory()`, which clones the retained `PinnedDirectory`, then consumes that clone in `.map(|authority| authority.descriptor_path())`. `descriptor_path()` merely formats `/proc/self/fd/<fd>` (`crates/kernel/lillux/src/secure_fs.rs:2111`); it does not transfer or retain ownership. The closure drops the cloned directory before the pathname is returned. The original session descriptor remains open, but has a different FD number.

Backend consumers include item list/inspection (`handlers/ui_items.rs:165`, `:323`) and field/work/thread sources. A read-only `os.dup`/close reproduction confirmed that the returned cloned-FD pathname exists before close and does not exist afterward.

**Impact:** project-aware resolution can fail or omit project content. FD reuse can make the pathname refer to a different subsequently opened descriptor; this is a broken authority/lifetime contract, not merely a display issue. No cross-project exploit was demonstrated.

**Correction boundary:** retain the descriptor owner across the entire operation, preferably passing the pinned directory rather than returning a bare descriptor pathname. Tests must use the returned authority after the helper returns and under FD churn, not only while the original closure is executing.

### F07 — P2: current-project projections compare descriptor paths with stored canonical paths

**Evidence:** source-confirmed; exact SQL comparison reproduced in memory. Separate from the closed-FD problem, so retaining the descriptor alone will not fix it.

Primary location: `crates/daemon/ryeos-ui/src/handlers/ui_work.rs:70`.

Work, attention and approval-history sources pass `caller.project_path()` into `ThreadListFilter.project_root`. The state query compares it directly with the stored `project_root` string (`crates/state/ryeos-state/src/queries.rs:668`). `/proc/self/fd/N` is not the durable canonical path recorded on the thread. Work listing then repeats a literal path comparison against continuation placements at `ui_work.rs:134`.

The same pattern affects `ui_threads.rs:113` for `project: current` and `ui_field_runs.rs:65`. Shipped Home/review/history views request the current-project filters.

**Impact:** a project-bound session can see empty current-project work, approvals and history even when matching records exist. This can masquerade as no pending work or no required approval.

**Correction boundary:** separate validated stable project identity used for projection lookup from the descriptor-rooted handle used for filesystem access. Maintain path-binding checks; do not reopen a display path as authority. Test a real project session against rows stored under its canonical identity, plus replacement refusal.

### F08 — P2: 1,025 resident requests make complete resource settlement fail indefinitely

**Evidence:** source-confirmed and independently corroborated by a second reviewer. New provider-neutral resource accounting (`33b42d045`). No Rust execution claimed.

Primary locations: `crates/daemon/ryeos-app/src/accounting_db.rs:2594` and `crates/engine/ryeos-accounting/src/resource.rs:708`.

Every completed resource-bearing resident request appends one attribution per process-level resource operation (`executor/src/execution/persistent_session.rs:319`; `ryeos-app/src/persistent_session.rs:1625`). The process can return to the pool and reuse the same operation IDs. Attribution insertion validates issued state and non-overlap, but has no cumulative cardinality limit or rollover. The single and batch APIs both allow the retained set to grow.

`ResourceUsagePartition::derive` rejects more than `MAX_ATTRIBUTIONS = 1_024`. Complete cleanup reads all retained attributions and calls this derivation (`accounting_db.rs:1764`). Both advisory and bounded settlement unconditionally reach it (`:4783`). No pruning or lifetime-request rollover was found. `active_request_count` is a concurrent lease counter, not a cumulative-request bound; occupancy duration also does not bound request count.

**Trigger:** one resident resource operation completes at least 1,025 sequential, disjoint requests before normal complete cleanup.

**Impact:** partition derivation fails inside the financial settlement transaction, rolling back its financial state/hold updates. The immutable attribution rows remain, so retry/restart repeats the same failure. Bounded reservations remain unreconciled; pool owner cleanup can remain retained or poison admission even though process death was separately proved. Advisory settlement also fails.

**Correction boundary:** enforce a compatible rollover/admission invariant before exceeding the settlement representation, or implement bounded aggregation/pagination preserving exact time and money conservation. An analytical partition cap must not unexpectedly prevent financial reconciliation. Test 1,024/1,025 requests, bounded/advisory authority, multiple resources, clean stop, failed-settlement retry and restart recovery.

## 3. Browser findings retained from the first pass

These remain important, but are not the whole review. They are separated here so backend findings and validation receive their own treatment.

### F09 — P1: WASM maps are not converted into the JSON objects expected by transport

**Evidence:** actual packaged WASM reproduction. The map serialization mismatch predates parts of the landing; the new adapter retains it and adds an unbounded failure-retry behavior.

`crates/clients/web/src/wasm.rs:18` uses a serde-WASM serializer that emits maps as JavaScript `Map`. A seat event was observed as:

```text
Map { 'seq' => 0n, 'event_type' => 'seat.facet',
      'payload' => { key: 'input.route', value: Map { 'thread' => 'review-thread' } } }
```

`browser/runtime/session.ts:135` reads `event.event_type`, `event.seq`, and `event.payload` as plain properties. Its outgoing event becomes `{"payload":{}}`, missing the required event type. On append refusal, line 144 immediately retries the same unsynced batch with no backoff/terminal-error gate.

Nested map-valued invocation parameters are also serialized as `{}` by `browser/runtime/effects.ts:59`. Static authored parameters may be reconstructed by the daemon, but dynamic facet/filter/selection values cannot be recovered this way. BigInt is an additional JSON boundary consideration once the map conversion is fixed.

**Impact:** seat persistence fails, append requests repeat, and invocation data can be silently discarded or refused. Test populated effects and seat events through actual HTTP encoding, not just empty envelopes. Preserve exact integer semantics explicitly.

### F10 — P1: overlays and notices exist in the shared model but are never rendered

**Evidence:** packaged WASM produces overlay state; source inspection finds no corresponding Svelte renderer.

`browser/app/RyeOs.svelte:27` renders chrome, navigation and workspace but never consumes `view_model.overlays` or `view_model.notices`; the component tree has no alternate consumer. Shared launcher/command/help shortcuts still create overlays and route input into that state. Refusal and draft warnings also remain in shared notices.

**Impact:** invisible modal interactions and invisible operational feedback. Restore rendering/accessibility/focus behavior for shared overlays and notices; do not add browser-owned substitute state. Test launcher open/filter/select/close, command refusals and draft-retarget warnings against the real shared core.

### F11 — P2: group tabs invoke workspace switching

**Evidence:** reproduced using packaged WASM with two workspaces.

`browser/layout/TileFrame.svelte:16` emits `switch_tab` using the group's local tab index. The reducer maps that intent to `switch_workspace_tab` (`crates/clients/base/src/ui/reducer/mod.rs:835`; `tiles.rs:289`). The group tab already supplies its `tile_id`, but the handler ignores it.

**Impact:** clicking the second view in workspace One selects workspace Two; with only one workspace the click is a no-op. Use the intended group/tile selection event, with a multi-workspace/multi-group interaction test.

### F12 — P2: pointer activation does not synchronize shared focus and cursor

**Evidence:** source-confirmed missing event wiring.

`browser/layout/TileFrame.svelte:12` and `DockSlot.svelte:8` have no shared-focus handlers. Row/table activation in `browser/views/ViewRenderer.svelte:21` and `:32` invokes the action without `focus_changed` or `set_tile_cursor`; records without an activation intent are disabled instead of remaining selectable. Input composer focus has separate addressed handling, but ordinary views do not.

**Impact:** shared keyboard/contextual actions can continue targeting the previously focused tile or selected row after pointer interaction elsewhere. Wire pointer/focus/cursor events into the existing shared authority and test mixed pointer/keyboard sequences across tiles and docks.

## 4. Inherited defect relevant to the expanded inference path

### I01 — P2: final-frame publication races immediate resident-worker reuse

This defect is present before the review baseline. It is not attributed to the new ARC3 merge, but is relevant to relying on the expanded persistent-worker path.

`bundles/local-inference/.ai/workers/local-inference/lib/local-tinygrad/session.py:606` sends the final response before clearing `RequestInbox._current` at line 616. The reader rejects a new request while `_current` is set at line 259. The daemon returns immediately on `Final` (`crates/daemon/ryeos-app/src/persistent_session.rs:3220`), so it can legally reuse the process in that interval.

A no-model socketpair test extracted the actual framing functions and `RequestInbox` from the Python source using AST, paused at the scheduling boundary, and observed:

```text
peer received: final
next request outcome: ValueError persistent-session process received concurrent requests
```

**Impact:** a valid immediate next request can terminate the reader/worker. Add a scheduling-barrier regression that sends the next request immediately on receipt of final. Keep this separate from the new accounting-cardinality failure.

## 5. Architecture and cross-stream coverage

The following are specific reviewed safeguards, not claims that all possible bugs in these domains were excluded.

| Domain | Paths/contracts traced | Assessment |
|---|---|---|
| Dependency layering | Architecture map, dependency constitution, actual workspace manifest dependency checker | No forbidden edges/cycles reported across 39 checked manifests. Lillux retains OS mechanics; host adapter owns OCI integration; app owns domain authority. |
| CLI and command admission | Signed descriptor grammar, control-vs-target parameters, project/current-head controls, `commands_dispatch`, `command_invocation`, `admit_execution` | Dispatch re-enters shared admission with caller context; resolving a command does not grant its target capabilities. No additional confirmed defect. |
| Remote workflow recovery | `remote_worker_workflows`, retained operation snapshots/digests, launch acceptance, service-root settlement evidence, target/outer product split | Accepted IDs and exact evidence are retained; recovery does not simply relaunch from current mutable content. Historical authority family is preserved. F05 exposes a composition/activation integration failure. |
| Activation and operator authority | Managed activation, refreshed grants, portable identity vs binding realization, retained import/bind, local vs remote operator checks | Current grant and operator lanes remain distinguished; local import ownership is not silently replaced by a remote caller. No separate confirmed grant-refresh authorization bypass. |
| Externally fenced workers | Readiness before remote launch/push, source import/admission prerequisites, independent cleanup/release receipts | Unsupported external placement remains intentionally refused before contact. Do not interpret the OCI merge as filling these external-provider contracts. |
| Trusted process groups | Explicit protocol and node-policy opt-in, separate local-scope and external-placement lanes | Trust-mode fallback is not silently used as hard containment. No blanket containment claim should be made from process readiness. |
| OCI lifecycle | Full hook journal/lease/reconciliation, exact init birth and boot identity, physical C/R ancestry, protected binding, inherited descriptor, bootstrap and poststop/recover | Correct fail-closed intent, but F01–F04 prevent the claimed startup/recovery integration from working. |
| Execution resources | Node policy, character-device observations, exact resource identity, admission cardinality, deployment-visible vs execution-restricted modes | Device visibility is not treated as containment; unqualified semantic facts are refused; selection and ownership remain exact. No additional confirmed selection bypass. |
| Resource exclusivity | Durable reservation, runtime DB scope/owner attachment, stable-resource conflicts, pre-contact recovery | Reservation precedes process release; recovery uses retained ownership. No additional confirmed exclusivity defect. |
| Financial release and recovery | Exact owner gates, reserve/issue/live-release separation, complete/partial cleanup, late evidence, outbox ordering and replay | Historical financial issue is not a fresh live release capability. Unknown usage is not silently zeroed. F08 breaks settlement liveness at the attribution limit. |
| Occupancy enforcement | Lillux BOOTTIME deadline, pidfd, cgroup kill, watchdog cancellation pipe, cleanup order | Watchdog retains its own keepalive across daemon death; normal cancellation follows cleanup proof. Not live crash-kill qualified in this review. |
| Inference implementation | Exact model/profile selection, BF16 shard/index/tensor checks, backend/libc coordinates, fresh Transformer state, session framing | Model/resource semantics remain bundle-owned. I01 affects worker reuse; admitted GPU execution remains unqualified. |
| Assistant backend authority | Compiled binding digest, node/engine generation checks, revalidation of effective target, coordinate/payload matching, source-safe lane, task-local session authority | Arbitrary browser target refs/capability claims are not accepted. F06/F07 break project authority consumption, despite sound intended separation. |
| Work/candidate/approval evidence | Principal filtering, continuation lineage, exact-thread authorization, candidate workspace base check, bounded changes, redacted approval history | Candidate completion is not automatically promoted to accepted/evaluated/published work. Project-bound list behavior is broken by F07; no separate candidate-write authority was inferred from read services. |
| State projections | Authoritative signed thread subject reads, resource transition identity/fingerprint/sequence validation, projection schema additions | CAS/events remain the authoritative evidence; projections validate coordinates. No additional confirmed transition-replay defect. |
| Packaging/distribution | Release OCI image/hook target, bundle inventories, fixed account, protected entrypoint, signed profile, signed browser asset registry | Structural checks pass. Contained targets remain qualification-only and outside official promotion; that is appropriate, but does not fix the runtime blockers. |

### Design alignment concern

The Svelte shell always renders an Explorer sidebar and reserves its grid column (`browser/app/RyeOs.svelte:34`, `browser/styles/shell.css:18`). The signed UI design contract explicitly rejects reducing RyeOS to a sidebar/page shell and requires the transient launcher to remain available. Alongside F10, this is a material design-contract gap. It is reported separately from security findings rather than presented as a containment or authority defect.

### Important intentional refusals, not bugs

- Missing independent external-placement cleanup/admission evidence must remain a refusal. OCI local lifecycle evidence does not substitute for a provider's external release receipt.
- Trusted disposable process-group execution is a distinct explicit posture, not proof of scoped hard containment.
- The source verifier refusing an installed qualification claim is correct while no authenticated installed attestor is configured.
- Missing model/runtime activation resources and the documented musl/glibc incompatibility cannot be repaired by treating numeric conformance as runtime acceptance.
- Retained recovery rejecting contradictory current or historical identity is preferable to silently adopting mutable replacement authority.

### Remote-workflow failure/recovery audit detail

The remote review was not limited to reading its happy-path entrypoint. The following production paths and failure boundaries were traced in `remote_worker_workflows.rs`, the shared command admission layer, activation/retained-binding owners, and product recovery:

1. **Start/resume authority:** configured operator, retained service-root identity, owner principal, admitted parameter digest and pinned project subject must agree. Config/graph references and the bounded task body are not used as arbitrary destination authority.
2. **Current route before contact:** retained source site, operator grant digest, target site/principal/key, URL and remote project path are compared before further work. A named binding owner is not itself permission to import or bind locally.
3. **Compilation and selected products:** the exact project-space signed workflow config is loaded from the retained snapshot; graph trust, bounded template evaluation, compiled digest and target request digest are retained. Outer controller products and target child products are checked separately.
4. **Readiness before transfer/launch:** protocol/session-policy requirements are checked before target contact that could execute work. External-placement cleanup deliberately refuses without independent source-side lifecycle evidence; ordinary node status is not promoted into proof of future death.
5. **Lost launch acknowledgement:** status for the exact retained launch coordinate is inspected and an already accepted launch is adopted rather than issuing a fresh execute. Planning and terminal states remain distinct.
6. **Acceptance facts:** returned ownership, capsule identity, item, parameter digest, reference bindings, lifecycle and pinned copy-on-write subject are checked. A projection row alone does not prove acceptance.
7. **Concurrent/restarted drivers:** retained attempts exclude a simultaneous driver. Startup reconciliation makes interrupted attempts retryable without inferring that no contact happened. Admission and settlement facts are reconciled against operation/request identity.
8. **Completion and candidate return:** bounded exact continuation identities, graph status/effective-definition digest and target-signed candidate testimony are checked against the pinned target key, source site and owner. Candidate base/path/product selections must match the admitted child capsule. Workflow completion is not automatically source import, publication or external placement release.
9. **Completed replay:** retained authoritative settlement is returned before opening another remote client or initiating lifecycle/model contact.
10. **Historical evidence:** old cleanup-free families receive a narrow local interpretation. Current and historical families cannot be mixed to manufacture additional authority; resumed serialization preserves the original evidence family.
11. **Grant refresh and activation retries:** portable activation identity is separated from grant-bound realization. Current policy/grant is checked before reconciliation/acquisition; refreshed bindings CAS-advance receipt heads. Already-current bindings can close an exhausted failed job without reacquisition, while cancellation/contradiction remain terminal. This does not repair F05's different consumer namespace.
12. **Transfer replay:** small-blob batching remains bounded; large blobs retain chunked transfer. Exact completed-blob acknowledgements can be replayed, while wrong/ambiguous hashes are rejected. Republishing the current project head is idempotent rather than requiring a snapshot to parent itself.

Inspected Rust tests cover lost-ack adoption, driver exclusion, historical acceptance/settlement, request/capsule divergence, target-capability denial, absent-project controls, retained launch IDs, remote-operator context and typed parameter normalization. They were inspected, **not rerun** in this disk-constrained pass. F05 is the confirmed remote/backend defect; absence of another finding is not a claim that these distributed paths were live qualified.

### Resource/accounting crash-boundary audit detail

The resource review followed reservation → held process attachment → financial issue → current live-owner release → request attribution → proved cleanup → settlement → audit publication. In particular:

- Exclusive intent is durable before process ownership is attached; fresh ownership is compared with exact retained resource/reservation identities.
- Replayed financial issue is historical evidence, not permission to release another process. Live release consults the current owner gate and account/deadline eligibility.
- Cleanup proof and financial settlement are distinct facts. Unknown/partial occupancy is handled conservatively rather than fabricated as zero; a proved dead process does not erase unresolved financial liability.
- The audit outbox retains ordering and exact identity/fingerprint semantics across append-before-ack recovery. A changed payload cannot masquerade as replay of the same transition.
- The watchdog uses BOOTTIME and an exact process handle, and keeps its cancellation channel alive independently of the daemon. This is a source-level assessment, not proof of installed behavior under daemon death.
- F08 arises precisely because attribution admission has a different lifetime bound from complete settlement. Restart correctly retains the evidence, but therefore also retains the un-settleable state; ordinary retries cannot repair it.

These seams need real process/DB crash-cut tests in addition to the focused regression tests specified in F08.

## 6. Validation evidence and limits

### Executed during this deeper pass at `83f32c890`

| Check | Result | What it does not establish |
|---|---|---|
| Repository dependency-layer checker | PASS, 39 manifests, no findings | Runtime ownership/authority correctness |
| `tests/development/contained_workflow_packaging.py` | 7 passed | Actual image boot or OCI hook invocation |
| `tests/development/hosted_oci_runtime_verifier.py` | 8 passed | Authenticated installed-host evidence |
| `tests/development/oci_hook_contract.py` | 7 passed | Rust hook/bootstrap integration; much coverage is source strings and a separate Python lease model |
| `tests/development/contained_workflow_products.py` | 4 passed | Installed contained workflow execution |
| `scripts/pkg/test-bundle-sets.sh` | PASS | Runtime realization of each product |
| `scripts/ci/test-daemon-image-init-policy.sh` | PASS | Container lifecycle qualification |
| Standard development-runtime authoring tests | 6 passed | Live authoring execution on admitted runtime |
| Development authoring-contract checks | 21 passed | Remote end-to-end campaign |
| Signed UI asset registry `--check` | PASS | Browser interaction parity |
| Exact proc-root open flags | Reproduced `ENOTDIR` | Full privileged OCI execution was not attempted |
| Descriptor clone/drop and in-memory project SQL comparison | Reproduced dead returned path and zero canonical-path matches | Full authenticated UI daemon fixture was not run |
| Actual local-worker framing/inbox socketpair | Reproduced I01 | Model inference not loaded or exercised |

Representative commands:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 .ai/tools/ryeos/development/repository-validation/dependency-layers.py --project-path /home/leo/projects/ryeos-next --config-file .ai/config/development/ryeos/repository-validation.yaml
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/contained_workflow_packaging.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/hosted_oci_runtime_verifier.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/oci_hook_contract.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/development/contained_workflow_products.py
./scripts/pkg/test-bundle-sets.sh
./scripts/ci/test-daemon-image-init-policy.sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s bundles/standard/tests/authoring -p 'test*.py' -q
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/e2e/authoring-environment -p test_development_contract.py -q
PYTHONDONTWRITEBYTECODE=1 python3 crates/daemon/ryeos-ui/generate_asset_registry.py --check
```

### Carried forward from the earlier pass at `00de99fae`

These are recorded separately, not falsely relabeled as a complete v0.5.92 rerun.

- `cargo test --offline -j2 -p ryeos-client-base --lib`: **428 passed, 6 failed** out of 434.
- Browser Node contract tests: **2 test files passed**; packaged-WASM contract smoke test passed. That WASM smoke fixture has an empty surface and no effects, so it misses F09–F12.
- Local-inference authoring: **18 passed**; model-contract tests: **7 passed**.
- Codex bundle checks: **30 passed**.
- Inference session/worker tests requiring `RYEOS_LOCAL_WORKER_TEST_WORKSPACE`: unavailable; no admitted fixture was supplied.
- `cargo test --offline -j2 -p ryeos-app --lib --quiet`: **did not reach tests**. Compilation failed with `No space left on device` while compiling `ryeos-scheduler`. It is neither a pass nor a product test failure.

Shared-client failing tests:

1. `ui::content::tests::inputs_plural_list_form_is_rejected`
2. `ui::content::tests::project_section_null_collection_yields_no_rows_not_a_dump`
3. `ui::reducer::affordances::tests::sections_view_without_a_loaded_source_shows_an_empty_group`
4. `ui::field::tests::identical_cross_source_facts_converge_despite_distinct_provenance`
5. `ui::reducer::input::tests::feeds_input_drives_its_source_param`
6. `ui::reducer::tests::duplicate_cancel_is_rejected_while_pending`

Several fixtures still use rejected schema fields or pre-binding assumptions. For example, the plural-input test unwraps deserialization even though `deny_unknown_fields` correctly rejects `inputs`; the feeds test expects `view:test/items` while its seeded binding names `view:test/filter`. This does not justify ignoring the failures: the suite needs corrected fixtures and renewed behavioral assertions, not merely weakened schema validation. The field convergence failure was not fully root-caused.

### Unavailable qualification

- No fresh full Rust workspace/API/executor integration run; disk remains severely constrained (about 1.1 GiB available at the start of this pass).
- No pinned-toolchain full Svelte/Playwright interaction run: browser dependencies were absent and the host Node version differs from the pinned browser toolchain.
- No installed OCI lifecycle run, namespace/cgroup mutation, daemon crash/replacement campaign, actual volume-reuse test, or unrelated-process safety test.
- No actual isolated GPU/device grant, watchdog crash-kill, admitted model execution or live numerical oracle rerun.
- No live source/target remote-development campaign, operator grant changes, provisioning, activation or external state mutation.

The remote-development completion ledger explicitly keeps fresh-node authority reconstruction and downstream gates unfinished. Its historic witnesses do not automatically qualify the new v0.5.92 binaries. The Qwen evidence is explicitly numeric conformance from a glibc Modal process; it is not proof of execution in the admitted musl runtime, whose driver load failed on `gnu_get_libc_version`.

## 7. Recommended closure sequence — not implemented

1. Resolve F01–F03 together with a real hook-to-bootstrap integration test. Unit-checking only one layer will leave the next blocker masked.
2. Add exact dead-tree retirement/recovery tests for F04 before any volume-reuse qualification.
3. Exercise the shipped managed-activation → default environment → pinned session path for F05, alongside a genuinely project-composed product case.
4. Fix the descriptor lifetime and stable project identity contracts separately (F06/F07); test browser backend services after the helper returns and against canonical project rows.
5. Make attribution admission and complete settlement share a lifetime bound/rollover contract (F08), with retry/restart and monetary conservation tests.
6. Repair the browser integration (F09–F12), restore launcher/notices, and run populated real-WASM browser scenarios—not only source-contract assertions.
7. Correct stale shared-client fixtures and investigate the remaining field failure; rerun affected Rust integration suites once sufficient build space is deliberately available.
8. Address or explicitly track inherited worker reuse race I01 before treating resident inference as reliable under immediate reuse.
9. Only then run named disposable-host installed qualification, retain authenticated exact image/policy/node/lifetime evidence, and renew the remote-development/GPU acceptance claims separately.

This sequence is advice for subsequent implementation and qualification. The review itself does not authorize or perform those changes.
