# Render Sandbox early-access lifecycle adapter

## Bounded provider feasibility observations (2026-09-27–28)

With explicit operator approval, two `starter` Sandboxes in the default Oregon
group ran for a total of about 124 seconds. Both used `deny-all` network policy
and were terminated and subsequently observed `terminated`. In
`sbx-18p4gdaseelbbc2fs73etqm50`, `unshare --user --map-root-user --mount
--pid --fork` mounted a private tmpfs and wrote a marker that was absent from
the enclosing mount namespace. A filesystem snapshot
`snp-dasef2ljk5ds739n9hg0` restored that marker into
`sbx-18p4gdasef8rbc2fs73etssh0` with the same SHA-256
`0a7860b394d39d9b2f8b65a0fa4ec9213f5188a385ea382d8ace469f252cfc82`,
`0700` parent, and `0444` file modes. The marker-only snapshot was deleted;
the snapshot list was then empty. An earlier bounded guest
`sbx-18p4gdasecdvlk1mc73bjsasg` showed that a `sleep` process survived
abrupt loss of its CLI run stream and that a private `0700` activation
directory contained an uploaded owner-owned `0600` file. That guest was also
terminated. These observations establish only feasibility of those specific
Render behaviors. They do not qualify an owner executable, signed runtime
product, authenticated supervisor `Ready`, whole-guest writer exclusion, or
RyeOS recovery. No model or Kaggle contact occurred. Exact charges were not
observed.

On 2026-09-28, a further approved `starter`, `deny-all` Sandbox
`sbx-18p4gdat3m2h7lnhs73bhbo30` accepted a user/mount/PID namespace with
`unshare --user --map-root-user --mount --pid --fork`. A descendant scheduled
to write a marker after namespace PID 1 exited did not write it. The Sandbox
was explicitly stopped and then independently observed `terminated` at
2026-09-28T10:04:59Z. This establishes that the relevant Linux namespace
primitive is available in this disposable Render guest. It does **not** attest
that the admitted Lillux owner launched the exact verifier in such a namespace,
that its process tree settled, or that restored/product bytes were frozen and
joined to the retained qualification occurrence. Those remain installed
qualification requirements; the signed activation operation stays closed.

This crate is an authored, unqualified adapter for the early-access Sandbox
surface described by the pinned Render CLI source at
`/tmp/render-oss-cli` (`de62fd1e2762ac25ad4ae11d086377c49fd4b299`). Render's
published [API reference](https://api-docs.render.com/reference/introduction)
and its [OpenAPI specification](https://api-docs.render.com/openapi/render-public-api-1.json)
currently do not list Sandbox lifecycle paths. The public OpenAPI error-code
enum does include `snapshot_not_found`, `snapshot_not_available`, and
`snapshot_plan_mismatch`; that corroborates these names, but does not publish
the Sandbox create schema, response codes, or the authority of those errors as
proof that a create had no effect. The lifecycle route and response shapes
below therefore remain grounded in the pinned CLI source and are unqualified.
This source and its fixture responses are not a claim of public/stable API
support, signing, installation, or provider qualification.

The current controller source also has a signed operator snapshot-production
service and a durable one-attempt journal. A complete create response binds a
locator, never runtime contents. The credential-free runtime probe now
requires that locator in its schema-2 evidence; controller admission compares
it with the retained bound attempt and product witness before this adapter
compares its snapshot ID with the signed placement. This offline join is
tested, but it is not a live restored-Sandbox verifier. The availability
response parser exists without a provider-contacting readiness operation. The
durable locator now retains the bounded, adapter-validated creation projection
(`requestedAt` and `expiresAt`) under the same operation identity so a later
readiness GET can preserve the parser's exact continuity check. No admitted
verifier yet measures the exact restored owner tree. The adapter constructs the
readiness GET route solely from the signed snapshot profile and retained
locator, and checks availability against the retained creation projection.
The adapter now has a separate sealed `observe-snapshot-readiness` entry that
can make this one bounded read-only GET. RyeOS has a typed observation, an
operator-owned service and a durable one-time readiness record. This only
establishes the provider's `available` report; the restored tree still lacks
independent measurement and activation remains closed. The source-local
service and command definitions are signed; their presence does not prove
admission on the installed controller generation. Installed admission and the
restored-content qualification still need verification before this path can
authorize worker activation.
The schema-5 snapshot probe carries only the retained snapshot locator,
restored-verifier observation, provider terminal observation, and exact
product/placement coordinates. It rejects the older unjoined hashes for
lost-stream survival, authenticated Ready, and writer exclusion. Those are
separate execution/settlement claims, not properties established by a
point-in-time restored-content measurement or a provider terminal status.
Therefore the Render activation gate remains closed.

## Implemented control-plane behavior

- Creates a Sandbox through `POST /v1/sandboxes`, placing `ownerId` in the JSON
  body as required by the pinned generated client. It also takes plan,
  region, immutable `snapshotId`, the reserved maximum lifetime, and `deny-all`
  network policy from the admitted settings/request inputs. `snapshotId`
  selects a reusable Render runtime image, while the reservation's distinct
  RyeOS base snapshot B is transferred and verified during guest activation.
  Both identities are included separately in the allocation observation
  digest. Render's create response does not echo a snapshot ID or content
  digest: this records only the configured runtime image selection, not an
  independent provider attestation of its contents or the guest base B.
- A valid `201 application/json` response must match the pinned Sandbox shape,
  including the requested plan, region, lifetime and network policy, before
  its provider ID is returned as an occurrence. Render's current
  [Python SDK reference](https://render.com/docs/sandboxes-sdk-python) says
  early-access `plan` and `region` request parameters have no effect (all
  Sandboxes currently run in Oregon with fixed resources). That does not tell
  us whether the create response echoes the requested fields or reports the
  effective fields. The exact equality check is fail-closed, but this profile
  may refuse a real successful create and leave its occurrence unknown. An
  installed, bounded observation must settle the actual response semantics
  before this adapter is admitted for production allocation; do not relax the
  match based on the documentation alone or retry an uncertain create.
- `allocation_no_occurrence` is returned only when W1 classifies the create as
  `NoRequestSent`, which establishes locally that the request did not reach
  Render. Every provider response other than a complete, valid `201` remains
  `allocation_pending`, including the pinned CLI source's 404 `snapshot_not_found`
  and 409 `snapshot_not_available` / `snapshot_plan_mismatch` shapes. Those
  errors still need provider-authoritative evidence before they can prove no
  occurrence. The adapter never retries create or scans the broad paginated
  Sandbox list to guess an occurrence. A request that may have reached Render
  can leave a Sandbox and billable spend with no safely bound occurrence; that
  state remains unknown and requires operator quarantine or review.
- Termination uses `POST /v1/sandboxes/{id}/terminate?ownerId=...` once, then
  requires an exact `GET /v1/sandboxes/{id}?ownerId=...` response with matching
  ID, the protected plan and region, `deny-all` network policy,
  `status: terminated`, and a valid `terminatedAt` before returning terminal
  evidence. The termination request does not carry the allocation lifetime,
  so this GET cannot independently recheck `timeoutSeconds` against it. The evidence
  digest binds the operation, binding, allocation request, occurrence,
  termination request, terminal state, timestamp, and provider response hash.
  Terminate acknowledgement, 404, timeout, or malformed response alone is
  pending.
- Allocation reconciliation without a retained provider ID is always pending.
  The adapter does not treat list absence or a GET 404 as proof of no
  occurrence.
- Activation and activation reconciliation always return `supervisor_pending`;
  even a successful proxy run is not authenticated supervisor `Ready`. The
  currently signed provider spec advertises no
  `supervisor_activation` lifecycle capability. RyeOS requires that capability
  at offline placement admission, before Sandbox allocation or other provider
  contact; this incomplete adapter cannot strand a paid occurrence merely to
  discover that startup is unavailable. The capability must be derived from
  a genuinely implemented signed activation operation with authenticated
  readiness and settlement, not added to the declaration as a claim. Selecting
  the dormant one-shot delivery profile is insufficient: it still returns
  `supervisor_pending`, so it advertises neither `supervisor_activation` nor
  `independent_guest_runtime_admission` and cannot pass allocation preflight.
  The pinned CLI source exposes a connect-token POST for a run, returning an
  execution ID, expiry, method, proxy URI, and short-lived bearer token. The CLI
  then sends a command to that URI using the returned method and parses
  `output`, `exit`, and `error` SSE events. It also exposes exact execution GET
  and list operations; execution records contain command metadata,
  operation/type, token-mint `startedAt`, optional `stoppedAt`, and optional
  client-reported `exitCode`, but no supervisor readiness proof. The CLI's SSE
  reader has no event-size ceiling; RyeOS bounds the response before reading
  it. If token minting's response is lost, the adapter lacks the returned
  execution ID needed for exact GET. A list result cannot safely bind that
  uncertain activation to this RyeOS operation. The adapter's narrow proxy
  origin/path and transport checks do not turn a run response into `Ready`.
  The currently signed profile still prevents token mint and proxy contact.
  These behaviors are visible in the pinned CLI's `pkg/sandbox/repo.go`,
  `pkg/sandbox/sse.go`, `pkg/client/client_gen.go`, and
  `pkg/client/sandboxes/sandboxes_gen.go`.

`src/proxy_route.rs` now supplies a dormant fail-closed destination check for
the CLI's example proxy routes. It accepts only the exact bound Sandbox ID and
region under `<sandbox-id>.<region>.sandbox.onrender.com`, `PUT /files/upload`
with the exact reserved path, or `POST /runs/stream`. It rejects arbitrary
returned origins, methods, queries, fragments, credentials, ports and alternate
encodings before a future bearer token could be forwarded. This is local
policy/test evidence, **not** evidence that Render always returns that shape or
that a proxy operation has been safely implemented.
It also constructs only the pinned CLI's exact API token-mint routes for the
same Sandbox and owner: file upload/download with a validated path, or run
stream. The constructor does not mint a token. A lost mint response must not
cause a second mint or upload/run attempt under the same activation claim.
The same dormant boundary now parses a bounded connect response directly into
a zeroizing bearer, rejects duplicates, unknown fields and trailing data,
checks expiry and execution identity, and binds the returned method and URI to
the exact occurrence route. A generic JSON value is deliberately not used:
it would leave a non-zeroized copy of the token. Raw response bytes must also
remain in a zeroizing buffer. This preflight does not mint a token or change
`supervisor_pending` activation behavior.

The intended first activation is one-shot. The controller durably commits its
activation intent before any adapter contact. A single bounded adapter call
may upload and invoke the run proxy at most once; after a crash or ambiguous
mint/upload/run result, reconciliation must not mint, upload or run again.
It remains pending and the exact bound Sandbox is quarantined and terminated.
The existing channel owner can accept a separately authenticated supervisor
`Ready` while provider activation is still pending, so no provider run exit is
treated as startup proof. This trades retry/liveness for a smaller, exact
first-candidate path; it does not require a parallel worker or lifecycle
checkpoint system. Before enabling it, the guest must verify every transferred
input byte against the retained projection and bind that verification to
`Ready`; Render must also be shown to keep the supervisor alive independently
of the run-proxy stream. Provider terminal status alone is not guest-writer
exclusion or hard-isolation qualification.

`src/activation_contact.rs` contains the one-shot contact sequence:
mint a bounded token and upload the signed import, mint and stream the exact
inherited package inode, then mint and send the fixed owner run command. It
uses no retry, validates each returned bearer against the exact Sandbox proxy
route, and preserves only a `Pending` interpretation of the run response. A
focused failure-order test proves it stops after the first uncertain stage.
An additional request-construction test checks the exact Sandbox proxy URL,
method, body budget, TLS roots and redacted bearer header before contact.
Provider-spec schema 2 declares the exact upload-token and run-token API
routes. The contact code requires its constructed URL to match that signed
route as well as the fixed adapter operation. The parser also recognizes only
an explicit `upload_then_run_once_pending` activation profile with a qualified
guest-runtime precondition; reconciliation accepts only `unsupported_pending`
and cannot repeat contact. The signed provider spec still declares activation
`unsupported_pending`, so the one-shot branch is unreachable as a live
mutation. Enabling it requires the
installed snapshot, upload mode, process survival after stream loss, and
authenticated supervisor `Ready` qualification described above.

The source tree now contains a dedicated guest occurrence-owner executable
that composes the one-shot import through held native supervisor launch. It
is not yet packaged into a qualified Render runtime snapshot, and this adapter
does not launch it. The remaining installed ingress and lifecycle gates below
still apply before activation can change from `SupervisorPending`.
The controller now authors a node-root-signed occurrence assignment from the
original bound allocation and seals it into the first activation contact with
the separately signed import authorization and exact package. This adapter
checks canonical shape, coordinate alignment, and the import signature against
the assigned occurrence key. It also adopts the inherited package descriptor
and checks its current owner, `0400` mode, exact length, and payload digest
before any provider contact. This is a point check, not exclusion of concurrent
writers; the eventual upload must stream the same registered inode under
Lillux's stable-reader check while the producer retains private-generation
custody. The adapter cannot authenticate the controller root or establish
guest trust. A qualified guest runtime must independently pin the controller
root and its own runtime manifest hash, verify the assignment first, and then
verify the import under the delegated occurrence-owner key. The final
assignment cannot be placed in Sandbox create-time environment: its occurrence
ID and activation digest do not exist until after allocation. A future
one-shot owner launch must carry it as bounded post-allocation input, and an
ambiguous launch response must not cause another launch.
The generic guest verifier can now observe the exact installed runtime tree,
read its pinned controller-root file, and recheck the full tree immediately
before import admission. This is a point measurement only: the Render snapshot
must still be independently qualified against that manifest/root, and the
owner must exclude writers through descriptor adoption and execution.
`ryeos-external-execution::guest_staging::stage_uploaded_guest_package` already
imports an exact pinned regular inode into a private generation and verifies
the package, base CAS, limits and manifest. `guest_content` rechecks the
realized product/source contents. The supervisor's shipped entrypoint instead
starts **after** its fixed descriptors have been installed; it cannot safely
serve as the importer. The missing generic guest owner must receive the
occurrence-bound expected identity independently of the package, pin and
verify the uploaded inode (or first seal it if the provider can still write
it), stage and recheck the contents, install the fixed descriptor map, launch
the exact packaged supervisor, retain the staged generation for its whole
lifetime, and discard it only after scope and writer settlement. This owner
belongs in RyeOS guest execution, not in the Render API adapter or Farm.
The first activation carries the retained import ticket and exact package;
reconciliation carries neither a fresh ticket nor another package. The guest
owner must durably bind that ticket, the stage inode, installed
runtime/state/private directory inodes, and supervisor executable before
recording a one-way pre-spawn intent. After that intent, recovery may observe
and settle only the original occurrence; it must not restage or start a
replacement supervisor. This is placement ownership below the existing
bounded-turn Worker, not a second session or candidate lifecycle. The
supervisor's journal continues to own its one-shot native candidate launch.
The ticket-checked import now has a distinct `TicketedGuestImport` result;
that type establishes only the supplied ticket-to-bytes/content join. Its
caller must source the ticket and context from retained occurrence authority;
the type does not establish that provenance or exclusive custody. It is
not an installed generation or a settlement witness. Its
`recheck_for_adoption` method repeats the retained-context, base-transfer,
bootstrap/executable and realized-input checks before descriptor binding, but
its caller must exclude concurrent writers across that binding. The
`install_base_into` operation can install a rechecked ticketed base into an
exact empty private runtime while retaining the stage. The provider-neutral
`guest_installation` path now reserves a fixed, create-only occurrence owner
before upload; that owner can stage once and writes a create-only, exact-inode
base-install intent and stage-owner marker before invoking the copy. Recovery
point-reads the same occurrence, owner, stage, fixed candidate-runtime child
and intent, with no restage or reinstall API. An ambiguous or partial copy
must be inspected and settled,
not retried. The live owner can now recheck that retained intent, the staged
input and installed base CAS, empty refs/recovery roots, and the original
runtime child/lock inodes immediately before a future descriptor handoff. Its
serialized observation is a point coordinate, not writer exclusion or a
launch permission. A subsequent one-shot preparation now retains the exact
opened product/source/raw-file descriptors after checking those handles against
the retained input projection; private-scratch slots remain unfilled. Those
handles are still mutable point observations until the outer owner excludes
writers and commits its launch. This is not yet a full installed generation,
supervisor launch intent, or immutable custody proof. The synthetic
fixture currently copies/moves staged inputs into its occurrence and discards the
staging directory before launching the supervisor. Its terminal path checks
the supervisor/server process identities and exits, not an enclosing process
scope or workspace-writer exclusion. Neither behavior is a qualification
template for the Render owner. The fixture later reopens artifacts by name,
so the original import check cannot attest their bytes at supervisor adoption.
A pinned descriptor also does not prevent a writer from changing its inode:
Lillux must enforce immutable/sealed source custody or exclude untrusted
writers across final verification and descriptor handoff. The owner must
retain its exact installed generation through a Lillux-proved enclosing-scope
settlement and separately verify the supervisor's signed candidate
writer-exclusion/export evidence;
provider `terminated` and parent process exit cannot substitute for either.
`GuestStageIdentity` provides a serializable name/inode/manifest coordinate
for a retained private generation. The occurrence owner journals that
coordinate before base installation and resolves it only under its exact
private parent on recovery. It must still bind and verify the coordinate at
supervisor launch under writer exclusion. The stage retains a bounded
canonical manifest sidecar; recovery reads that sidecar by pinned descriptor,
checks it against the separately retained ticket, and rechecks the selected
base, executable and input content. It never replays an upload or authorizes a
new supervisor launch. The coordinate itself is not a recoverable launch or
cleanup authority. An imported stage now retains an exclusive Lillux directory
lock, and exact recovery refuses while another cooperative importer owns that
inode. This serializes import and recovery owners, but the advisory lock does
not exclude an untrusted guest writer or prove descriptor adoption. In
particular, the import-only, roughly 500,000-entry cleanup budget cannot
retire a generation after the base CAS has been duplicated into the candidate
runtime or candidate writers have run.
The manifest sidecar also consumes disk beyond `regular_bytes`:
`GuestImportTicket::minimum_upload_and_stage_bytes` accounts for the exact
coexisting upload and staged regular bytes. Before activation, the guest
owner/provider profile must reserve at least that amount plus filesystem
overhead and the separately bounded installed runtime. The current adapter
does not make or prove that reservation and remains fail-closed.
Until that joined handoff is implemented and tested, the adapter must continue
returning `supervisor_pending` even if token minting and upload work locally.

The native Lillux private-source probe also ruled out a tempting shortcut:
ordinary same-namespace fork/exec from a non-dumpable source owner reset the
exec child's dumpability to `1` on the tested host. The child refused before
reading even synthetic source bytes. A read-only source mount does not close
the resulting transient `/proc` exposure, descriptor-custody or writable-alias
questions. Supervisor activation therefore needs a separately protected
launcher with the exact descriptor map and read-only realization applied
before untrusted code can observe the source; it cannot use a direct
`Command`-style exec within the source owner's namespace. The native probe
is negative architecture evidence, not installed Render qualification.

## Provider operation and uncertainty matrix

| Operation | Stable identity and pre-contact record | Positive evidence and reconciliation | Lost response, negative evidence, and spend |
| --- | --- | --- | --- |
| Create Sandbox | W2 supplies the retained operation ID, binding hash, allocation request digest, and reservation before invoking this adapter. The provider request has no create-correlation or idempotency token. | Only this request's complete, valid `201` response can bind its returned Sandbox ID after the configured fields match. | A lost/malformed response or any failure after request transmission stays pending. No retry or list search is allowed. `NoRequestSent` is locally authoritative. The three typed snapshot rejection shapes remain pending until provider-authoritative semantics are established. An uncertain create may exist and consume capacity/spend, so its original reservation remains quarantined. |
| Allocation observation | The original allocation identity remains the coordinate; no Sandbox ID is invented. | Allocation reconciliation is unsupported without a retained provider ID. This adapter does not list or guess. | A list miss or `404` is not a negative proof. Unresolved occurrence and spend remain unknown under the original reservation. |
| Bootstrap and readiness | W2's activation request identity remains authoritative; the current signed profile refuses the one-shot activation branch. | An explicitly signed, qualified-runtime one-shot profile may mint/upload/run once, but still emits only pending; authenticated supervisor `Ready` comes from the existing channel, never the proxy response. Reconciliation never repeats contact. | A lost token-mint or run-stream response leaves the original activation pending. The adapter does not retry, infer success from an execution list, or mint during reconciliation. |
| Terminate | W2 supplies the retained operation ID, bound occurrence ID, and termination request digest before invocation. | One termination `POST` is followed by a `GET` for that exact ID. Only a matching ID, protected plan/region, `deny-all` policy, `status: terminated`, and a valid `terminatedAt` are terminal observation; allocation lifetime parity is not rechecked. Recovery is GET-only. | A lost POST response is reconciled by exact-ID GET; the POST is not repeated. A timeout, `404`, malformed response, or missing terminal fields remains pending. Until exact terminal observation, remaining capacity/spend is unknown. |
| Provider Sandbox death and writer exclusion | Bound to the exact Sandbox occurrence, but distinct from the termination request coordinate. | The current adapter does not establish guest descendant settlement, writer exclusion, or a frozen export. | Provider `terminated` status is not installed qualification or proof of RyeOS guest-writer death. No candidate execution or export is enabled by this adapter. |

The dormant proxy URL check still needs a live provider-shape qualification;
the mapping from RyeOS captured snapshot/input hashes to Render Sandbox
bootstrap content remains unresolved.
The adapter therefore cannot start RyeOS's supervisor in a Sandbox and is not
a usable external execution backend yet.
The configured runtime `snapshotId` is not yet joined to an independently
qualified, exact importer/build identity. Allocation fixtures do not establish
that trust anchor; supervisor activation stays pending until that binding and
the guest import path are qualified.

## Guest-owner source and snapshot authority cut

The signed placement binding's `guest_runtime_manifest_hash` is an expected
content identity; Render settings' `snapshot_id` is an opaque provider locator.
Neither is evidence that the provider restored those bytes. These two
coordinates and the controller public root must meet in one independently
verified qualification before a new Sandbox can be allocated for RyeOS work.

The source product should be authored through an ordinary retained-output
recipe. Its producer needs the exact owner executable as a **data input**, the
node's public assignment-verification root, and a signed owner profile. It
writes a fresh credential-free tree; RyeOS captures that tree as a product
and checks its manifest against the guest's `ObservedGuestRuntime` algorithm.
The signed Tool now declares `bundle_binary: bin:ryeos-external-guest-occurrence-owner`
under the narrow `bundle_binary:own` kind grant. Admission captures the
declaring Bundle's signature-verified executable into a sealed descriptor and
then into an exact retained file realization; the producer never opens the
installed `.ai/bin` path. The signed production graph reads the current node's
public identity through `service:identity/public_key` and passes it to the
producer. No credential or submission authority enters that workspace.
The separate signed `graph:codex/guest-owner-runtime-production-for-controller`
requires an explicit target controller public key. It retains the same product
recipe and captured-execution Tool, so an enforced-isolation authoring node
can produce bytes for a distinct Render controller without pretending that
the Render controller itself can run the captured producer. The target
controller must still import the exact witnessed product and independently
verify that its own public root is present in the manifest; accepting a
caller key or graph result alone would not do so. A disposable
`hosted-workflow` node on 2026-09-28 admitted the seven-bundle set but refused
the local producer at the correct boundary:
`captured execution requires enforced isolation before admission`. It made
no Render or model contact. This is a topology constraint, not a reason to
weaken the Tool's filesystem authority.
Independent qualification must still join the resulting owner digest and
public root to the executable and root actually installed in the Render
snapshot. Similar names or a successful production graph do not establish
that join. The owner runtime product uses ordinary `content` storage: its
captured manifest coordinate must equal the tree manifest the guest observes.
The large-content manifest format is a distinct hash even for equivalent
files and cannot be substituted in this qualification. The signed owner
Tool/graph has completed once on an isolated disposable development node at
source revision `50e6ba975`: graph thread
`T-a159bd74-018f-a497-4b77-c6d014407f61` produced manifest
`7d897e8ccc37ac621bef3decd7c5ff85e01c397ca5ff439117bfd968b1331c88`,
and ordinary product capture retained witness
`45ce16beec6f684dfa46ddf4c8a3cea382c2c325bfb867d371e8364c6eae1e47`.
The exact owner executable `c5f67f106e17c5a1f894fd0b04626c5047f92c72181253c0923fb770182ff395`
also ran a bounded fail-closed argument probe in a disposable Render Sandbox.
These are source-production and native-execution observations, not an installed
Render snapshot or runtime qualification. The product witness must be carried
through an admitted product-to-provider staging path; the artifact-blob export
command does not export external-content manifests.
`ryeos-executor::execution::external_guest_runtime_product` supplies the
controller-side source half of that path: it authenticates the current witness
and import bounds, loads the exact ordinary manifest under CAS authority,
privately stages it, and re-observes the tree against the node public root.
The staged authority can emit one bounded, deterministic, sealed plain-tar
directory-upload body from pinned descriptors; it rechecks the tree after
packaging. A separate closed snapshot-production profile fixture describes the
reviewed Render upload-token, create and exact-status routes, filesystem kind,
plain-tar content type and response states. Its parser rejects unsupported
substitutions; the fixture is not yet a signed or admitted provider authority.
The adapter now also has a separate `produce-snapshot` invocation. It consumes
an exact sealed attempt, digest-bound profile bytes, settings, credential, network
inputs and owner-product tar descriptor; it checks the running denied-network
source, mints one scoped upload token, uploads the exact plain-tar body, then
requests a filesystem snapshot. A complete create response yields only an
operation-bound locator. The daemon has an operator-owned one-shot production
service and journal, but the profile/definitions have not completed installed
admission. Lost output must be quarantined under the original attempt without a
second upload or snapshot-create sequence. The Render adapter parses a
complete `202` filesystem snapshot response against the exact source Sandbox,
group and plan, retaining the
original product witness/root in a locator observation. A separate bounded
readiness parser accepts only a complete `200` response that reports the same
filesystem snapshot, source, group, plan and request timing as `available`;
this remains provider readiness, not a restored-content proof. RyeOS also
retains a provider-neutral snapshot intent and one-attempt journal. The intent
binds a signed production profile, not the later placement binding whose
snapshot ID does not exist yet. An exact
reservation may start the bounded provider sequence once; an uncertain
attempt must reconcile, never blindly repeat the sequence. The source-side
parser and journal are joined by that operator action, but neither is evidence
of the bytes restored from the snapshot.
This does not export a CAS path, contact Render, publish a snapshot, or attest
restored guest bytes. Durable provider transfer and independent restore
observation must consume that exact staged authority before qualification.

The provider-installed qualification must then bind, at minimum, the exact
product witness and manifest, Bundle/source generation, node public root,
Render owner/account, snapshot ID and observed `filesystem` snapshot kind,
effective plan and region, and the exact
signed binding generation. Its independently admitted verifier must observe
the restored tree and root *inside that snapshot*, upload modes, owner
survival after losing the run stream, authenticated supervisor `Ready`, and
whole-guest termination plus writer exclusion. The generic product
qualification machinery can retain a signed verifier result and bounded
probe evidence. A Render-specific typed interpretation now exists, but its
authenticated join to the placement binding is still missing. A self-described `qualified` field, a
matching create response, or a signed expectation alone must not grant
startup capability.

`src/snapshot_qualification.rs` now defines the bounded Render-specific probe
shape and exact comparison against those expected coordinates. It rejects
unknown fields, changed snapshot/root/manifest, weak or noncanonical controller
keys, changed file modes, and missing provider-terminal evidence identities.
Placement admission now rejoins both the independently admitted verifier's
retained observation and the exact restored occurrence's daemon-retained
provider-terminal observation to the same qualification operation. The old
probe-provided terminal hash is no longer accepted as free-standing evidence.
This remains only a provider-terminal join: the verifier and termination have
not yet run as an installed Render qualification, and a provider `terminated`
response does not prove guest-writer exclusion. The signed provider spec still
refuses activation.

The separate `ryeos-external-guest-restoration-verifier` now measures the
restored owner tree under a fresh challenge. This adapter has an offline,
bounded SSE interpreter for its run output: it requires canonical measurement
bytes, no stderr or unknown event, a complete zero exit, and the exact joined
content/readiness coordinates. This parser is not an authenticated Render run.
The verifier is now an exact static payload in a dedicated `render-sandbox`
bundle alongside the adapter, supervisor, and launcher. Its source-publisher
signed manifest captures the finite provider specs and all four executables
from one generation; the hosted-workflow payload inventory requires them.
That closes source packaging only. The controller must still admit this bundle
and run the matching verifier in the restored qualification occurrence.
A distinct, durable qualification occurrence is now provisioned from the bound
snapshot, with one-shot verifier and provider-termination journals. Installed
qualification must still execute that path, retain the token/run and independent
guest-writer settlement evidence, and submit those facts to the controller
join. Worker allocation cannot be borrowed for that purpose because it itself
requires prior runtime qualification. The existing durable
`ExternalAllocationOwner` variants are only `DedicatedSession` and
`DirectThread`; neither denotes a product-qualification occurrence. A
qualification allocation therefore needs its own bounded intent and journal,
while reusing the signed lifecycle profile and transport primitives. Adding a
fake Worker owner or bypassing current runtime qualification to use that journal
would create an authority cycle.

Binding schema 14 now carries a required-nullable `runtime_qualification`
object with the exact attestation hash, product-owner principal, policy ref,
and required claims. The node Config signer is not assumed to be the product
owner. A non-null object is only an exact coordinate:
fresh placement refuses it until the published witness, current
policy, independent execution evidence, and Render probe can all be joined.
This is distinct from the Codex candidate-runtime product selection retained
in the Worker capsule. Cleanup of an already contacted occurrence does not
reapply this startup gate.

The fresh read-only check now resolves the named owner through its current
node-signed operator grant, loads the exact published qualification and
product witness under CAS guard, and rechecks current policy, verifier
definition/artifact, required claims, and execution evidence. It runs before
credential access on fresh placement paths. That authenticates the product
proof but does not yet compare its provider-specific probe; the backend registry
still refuses a non-null qualification for new contact. The exact proof and
product owner are now CAS-owned by the schema-18 persistent-session capsule,
separately from the Codex executable selection. No synthetic verified request
context is constructed.

The lifecycle backend now has a credential-free, provider-neutral runtime-probe
interpretation hook. Fresh admission and retained placement call it only after
RyeOS authenticates the corresponding proof. The hook defaults to refusal; the
exact Render adapter now implements a sealed offline invocation with no
credential or network descriptors. It compares source-derived owner, tree and
controller-root identities with the bounded independent probe and returns a
request-digest-bound response. The separate allocation gate still refuses
qualified bindings: neither this interpretation nor a successful probe parse
alone is an activation claim.

The remaining provider-probe join has two separate owners. RyeOS authenticates the binding's exact
qualification attestation from current published CAS, its product witness,
current signed policy, admitted verifier definition, and execution evidence;
the session capsule retains that result with the exact signed binding generation
before first contact, and start/channel recovery authenticates it without
selecting a new product head.
The Render adapter interprets the bounded `probe_evidence` against the same
binding's owner/account/snapshot/plan/region, controller public root, owner
executable and runtime manifest. Neither a caller-provided JSON probe nor a
synthetic `HandlerContext` may stand in for RyeOS's authenticated witness.
The exact product-witness hash is the source-lineage join: its authenticated
capture evidence already retains the producer and root-producer admissions,
including effective definition, project snapshot and launch authority. The
removed `bundle_generation_hash` probe field had no independent installed
observation or signed-binding source; copying it from `probe_evidence` into its
own expectation would have proved nothing.
RyeOS now re-verifies the retained product witness and its complete content
closure, then derives the expected owner executable digest and controller-root
file digest from the exact ordinary product manifest under the node's current public
key. The large-content tier has a different manifest identity and is rejected. The
credential-free adapter hook receives these source-derived values; the Render
executable's sealed offline invocation now compares them with the probe. It
still cannot authorize allocation while the signed provider spec declares
activation `unsupported_pending` and joined installed qualification is absent.
The proof must be rechecked at the fresh reservation/contact cut, while
recovery of an already contacted occurrence uses its retained exact proof and
does not select a newer witness. The existing `guest-runtime` Worker slot
remains the distinct Codex executable product, not this guest-owner snapshot.

The required implementation order is: publish and pin the exact owner input;
sign the bounded producer Tool and capture its runtime product; run and retain the installed independent
snapshot qualification; join its exact evidence to the current placement
binding before allocation; then enable the signed activation operation and
prove one-shot contact, ambiguous-response quarantine, authenticated `Ready`,
and exact cleanup. Pre-contact tests must reject missing, stale, different-
snapshot, different-root, and different-manifest evidence without invoking
the adapter. Retained cleanup must remain available even if a later startup
qualification is absent. A read-only Render inventory on 2026-09-27 returned
no snapshots (`[]`), so none of the installed claims above has been made.

The declared capabilities are exactly `authoritative_no_occurrence` and
`exact_terminal_observation`. The first capability is limited to W1's
`NoRequestSent` result; provider response errors do not currently establish it.
The adapter does not declare `idempotent_termination` because it follows the
one-Terminate policy.

## Explicit settings

`fixtures/settings.schema.json` defines the sealed settings payload. It carries
an owner ID, plan, region and immutable runtime snapshot ID,
plus explicit DER roots used only for `api.render.com`. W2's lifecycle runner
supplies resolver and hosts as sealed descriptors and supplies SHA-256 values
for both byte strings plus the signed network-policy digest. W2 validates the
captures against that signed policy; W3 verifies both byte digests, checks the
policy digest encoding, and parses captured bytes through Lillux. It never
reads DNS or hosts from ambient files. The Render API key arrives through the
runner's sealed credential descriptor and is attached only to the fixed API
origin. The remaining-time environment value starts one monotonic deadline
for the whole operation; allocation also clamps it to its request contact
deadline. SIGINT and SIGTERM cancel the operation's shared W1 network
cancellation token.

The provider specification is a separate signed bundle artifact, not a
settings field. The settings schema digest inside `fixtures/provider-spec.json`
binds the supported provider profile to this exact settings schema. The
proposed closed data shape is documented in
`fixtures/provider-spec.schema.json`; the Rust parser remains the runtime
authority and additionally enforces the reviewed operation/profile pairings.

## Signed provider specification

The bundle fragment uses lifecycle protocol v2 and binds the non-executable
`fixtures/provider-spec.json` file by path and SHA-256. W2 captures it from the
same admitted bundle generation as the adapter, enforces the 256 KiB limit,
checks its signed digest, and passes a sealed descriptor and digest to both
inspection and each operation. Inspection reports the digest it actually
observed.

W3 adopts and reads the inspection descriptor through Lillux, checks its
declared byte count and digest, parses the closed schema, and reports
`observed_provider_spec_sha256`. At operation startup it reads the W2-supplied
sealed FD and digest environment variables, then performs the same checks
before settings or network work. The settings-schema digest in the spec must
match the exact settings schema carried by this adapter.

`src/provider_spec.rs` is the first narrow interpreter profile. It accepts
fixed operation kinds, route segments, typed field sources, exact configured
and independently qualified runtime preconditions, and code-defined
proof-profile identifiers. It rejects
unknown fields, methods, origins, arbitrary expressions, capability claims,
and proof values. The interpreter builds the create body and exact route from
the signed data; Rust validates create binding and terminal observations before
deriving effective capabilities. Create remains
one POST with no retry or listing-based reconciliation. Termination remains
one POST followed by exact-ID GET. Activation always reports pending, and the
currently signed provider spec does not permit its one-shot contact branch.

This is the Render reference profile for the data-driven lifecycle direction,
not evidence that a multi-provider shared runtime is complete. The interpreter
is still crate-local and Render-specific; extracting common parsing and
execution logic into the shared host runtime requires a reviewed schema/runtime
boundary before another provider is added. The integrated v2 seam carries the
signed, digest-checked provider spec over a sealed descriptor. The Render
profile cannot introduce
an origin, credential placement, HTTP method, retry rule, or proof behavior.
The Sandbox proxy stays disabled until its observed URL shape, token handling,
bounded transfer, and RyeOS bootstrap mapping are qualified.

## Build and qualification gates

The adapter is a separate host-adapter package registered in this feature
workspace. A source build command is:

```sh
cargo build --release --manifest-path crates/host-adapters/render-sandbox-adapter/Cargo.toml
```

The feature branch's focused offline serial package test passed all 21 tests
on 2026-09-27. A read-only Render snapshot inventory returned `[]`; no release
build, signing, provisioning, paid Sandbox contact, or installed qualification
followed this correction. Before use, the integration must
establish provider-authoritative lifecycle evidence, proxy validation, RyeOS
bootstrap and network-route semantics, and joined cancellation behavior. Any
authenticated Render call requires separate approval.
