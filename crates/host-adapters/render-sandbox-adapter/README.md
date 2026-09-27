# Render Sandbox early-access lifecycle adapter

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
- Activation and activation reconciliation always return `supervisor_pending`.
  The pinned CLI source exposes a connect-token POST for a run, returning an
  execution ID, expiry, method, proxy URI, and short-lived bearer token. The CLI
  then sends a command to that URI using the returned method and parses
  `output`, `exit`, and `error` SSE events. It also exposes exact execution GET
  and list operations; execution records contain command metadata,
  operation/type, token-mint `startedAt`, optional `stoppedAt`, and optional
  client-reported `exitCode`, but no supervisor readiness proof. The CLI's SSE
  reader has no event-size ceiling; a RyeOS implementation would need bounded
  per-operation SSE limits. If token minting's response is lost, the adapter
  also lacks the returned execution ID needed for exact GET. A list result
  cannot safely bind that uncertain activation to this RyeOS operation. The
  dynamic proxy URI additionally needs a reviewed origin/path policy and
  transport policy. W3 therefore does not mint a token, invoke a proxy URI, or
  infer activation from an exit status.
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

`src/activation_contact.rs` now contains the disabled one-shot contact sequence:
mint a bounded token and upload the signed import, mint and stream the exact
inherited package inode, then mint and send the fixed owner run command. It
uses no retry, validates each returned bearer against the exact Sandbox proxy
route, and preserves only a `Pending` interpretation of the run response. A
focused failure-order test proves it stops after the first uncertain stage.
An additional request-construction test checks the exact Sandbox proxy URL,
method, body budget, TLS roots and redacted bearer header before contact.
Provider-spec schema 2 now declares the exact upload-token and run-token API
routes. The contact code requires its constructed URL to match that signed
route as well as the fixed adapter operation; neither declaration authorizes
contact while activation remains `unsupported_pending`.
The signed provider spec still declares activation `unsupported_pending`, so
none of this code is reachable as a live mutation. Enabling it requires the
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
| Bootstrap and readiness | W2's activation request identity remains authoritative; this adapter does not create a Render execution token. | No bootstrap/readiness proof is emitted. Activation and its reconciliation remain pending. | The pinned CLI's token-mint response would carry the execution ID and operation-scoped proxy URI; a lost response leaves no exact ID to query. This adapter does not mint, retry, invoke, or guess from a list. |
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
fixed operation kinds, route segments, typed field sources, one reviewed
snapshot precondition, and code-defined proof-profile identifiers. It rejects
unknown fields, methods, origins, arbitrary expressions, capability claims,
and proof values. The interpreter builds the create body and exact route from
the signed data; Rust validates create binding and terminal observations before
deriving effective capabilities. Create remains
one POST with no retry or listing-based reconciliation. Termination remains
one POST followed by exact-ID GET. Activation and its reconciliation remain
pending.

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

The feature branch's focused offline serial package test passed all six
remaining tests on 2026-09-26. The removed seventh test only recognized a
snapshot-error shape; it never proved that a transmitted create had not made a
Sandbox. No release build, signing, Render API call, provisioning, or installed
qualification followed this correction. Before use, the integration must
establish provider-authoritative lifecycle evidence, proxy validation, RyeOS
bootstrap and network-route semantics, and joined cancellation behavior. Any
authenticated Render call requires separate approval.
