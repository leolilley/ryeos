# Bundle-source operations

Readiness: the implementation now includes measured-authority calibration,
Core/substrate genesis production, exact per-release recipe admission, remote
catalog transport, and stopped-node activation. It is not qualified until the
first complete bundle set and one subsequent single-bundle successor have run
end to end on the actual target; compilation and registration remain
insufficient evidence.

Native bundle publication is initiated from an admitted RyeOS development
project through `service:bundle-release/submit`. That operation invokes the
canonical `graph:ryeos/bundle-release/publish`; GitHub is not an alternate
release implementation. An optional GitHub adapter may provide an immutable
source coordinate, invoke this service, and mirror the resulting coordinates.

The normal topology keeps the release authority and bundle-source node
separate. After the publisher authorizes the exact successor, the release
authority transports its complete bounded CAS closure through a pinned RyeOS
named remote (normally `bundle-source`) and the
authorized-uploader `service:bundle-catalog/upload` boundary on the
bundle-source node. It then requests `service:bundle-catalog/publish` using the
returned durable upload-session identifier and exact expected predecessor.
This is the canonical path even when both nodes run from the same substrate
image. A normal bundle release does not rebuild or republish that image.
The remote client signs each call with the release node identity, which must be
listed in the catalog's `authorized_uploaders` policy and admitted for the
remote service capabilities. That identity is independent of the publisher
signing key. There is no parallel bearer-token catalog publication protocol.

`service:bundle-release/status` returns bounded progress and, on success, only
immutable release coordinates: generation hash, generation-publication
attestation, set hash, and catalog-publication attestation. Callers must not
derive success from logs, mutable channels, or workflow status.

Disaster recovery uses `service:bundle-catalog/export-recovery` to capture the
exact current catalog coordinate, policy binding, and bounded object/blob
closure inventory. Restore first transports that inventory through the same
authorized-uploader `service:bundle-catalog/upload` session used by normal
publication. `service:bundle-catalog/restore-genesis` then accepts the exact
publisher-authored catalog only when the catalog head is absent and the live
policy digests exactly match the export. It never manufactures replacement
publisher authority, overwrites a live head, or infers a successor.

The release-authority node does not load a publisher private key from its app
root or `AppState`. Its constrained publisher is an independently operated
authenticated service configured at process startup with
`RYEOS_BUNDLE_PUBLISHER_URL` and `RYEOS_BUNDLE_PUBLISHER_BEARER` (both or
neither). The endpoint must use HTTPS, except for a loopback development
sidecar, and must expose only the closed operations for exact build/capture
recipe authoring, Core-seed authorization, tree signing, bundle/substrate
generation authorization, and catalog-successor authorization. See
`deploy/release-authority.env.example`; inject the bearer from the deployment
secret store.

The authenticated JSON endpoints are `POST /v1/bundle-recipe/authorize-build`,
`POST /v1/bundle-recipe/authorize-capture`,
`POST /v1/substrate-core/authorize-recipe`,
`POST /v1/bundle-tree/sign`,
`POST /v1/bundle-generation/authorize`,
`POST /v1/substrate-release/authorize`, and
`POST /v1/bundle-catalog/authorize-successor`. The authority must share the
release node's CAS or import every returned coordinate into it before replying;
the client accepts only bounded responses from the publisher fingerprint pinned
by the live catalog policy. HTTP redirects are refused so the bearer cannot be
forwarded to a second origin.

`service:bundle-catalog/stage-local` remains an optional optimization for a
deliberately combined release-authority/bundle-source deployment whose two
roles share one CAS. It performs the same bounded closure walk and creates a
principal-bound durable upload stage over the exact catalog attestation. It is
not used by the default release Graph, is not granted by the default
`release-authority` profile, and is not a fallback when remote transport is
misconfigured. A dedicated combined-node profile may opt into the capability;
an empty upload session is never treated as closure evidence.

The substrate image remains an independently recoverable bootstrap artifact.
Its seed can start the bundle-source service but is not the live catalog and
cannot replace persistent catalog state after redeployment.

The bootstrap Graph intentionally publishes a Core-only sequence-0 genesis.
That is an authority root, not a deployable node set: activating it would remove
every installed non-Core bundle. Build one successor for every remaining member
of the exact target bundle set and expose only the final complete head to
consumers. For the `bundle-source` role this means Core genesis followed by
`central-auth` and `bundle-source`; only then may a consumer select the head.
A later publication replacing just one member is the independent-update proof.

At first initialization the deployment must pass the resolved image digest and
the substrate compatibility protocol explicitly; mutable tags and environment
discovery are not authority:

```sh
ryeos init --non-interactive --node-profile bundle-source \
  --substrate-image-digest sha256:<64-lowercase-hex> \
  --substrate-protocol 1
```

Initialization persists a strict `ryeos/substrate-identity/v1` document and
binds its canonical digest into the operator-signed init-completion record.
Restarts and stopped-node bundle updates load it only through full completion
verification. Reinitialization may omit the two arguments only when that exact
verified record already exists; it cannot replace the substrate identity.

## Deployment cutover

The shared container entrypoint requires `RYEOS_SUBSTRATE_IMAGE` (the exact
running image reference pinned by digest) and `RYEOS_SUBSTRATE_PROTOCOL` on
first boot for **every** image using it, not just the bundle-source image.
Set these in the deployment's runtime environment; an image cannot embed its
own eventual registry digest. `RYEOS_INIT_NODE_PROFILE` remains the image's
explicit seed selector. A restart preserves the installed generation rather
than reconciling it against image contents.

This release accepts only `ryeos/init-completion/v3`, including its signed
substrate-identity binding. Existing volumes with a different completion schema
are not an in-place image-swap deployment target. Keep them stopped and backed
up; do not delete the bundle directory, edit completion JSON, or use policy
reset as a completion-schema migration. No automatic state migration is
implemented. Deploy the bundle-source role to a new volume, initialize with
explicit substrate coordinates, and provision measured publication authority
as described in `deploy/constrained-bundle-publisher.md`. Existing nodes that
must preserve their state require a separately implemented and qualified
cutover before using this release. Retain the previous image and its untouched
volume for rollback; never run the previous image against newly written state.

Before production rollout, prove one complete native-bundle release and one
portable-data release on the actual target, followed by a stopped consumer
update and restart. Source/static checks are not substitutes for this gate.
