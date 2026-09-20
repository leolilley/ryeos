# Constrained bundle publisher

The release-authority node calls a separately composed publisher over five
purpose-owned HTTP operations. The publisher owns the private publisher key;
the release-authority daemon never does.

The server router is `ryeos_api::publisher_server::router`. Compose it with a
`LocalConstrainedPublisherAuthority`, an explicit publisher signer loaded from
a secret path outside the application and project roots, the shared/imported
publication CAS, and the current closed-mutation and release-evidence proof
authorities. There is deliberately no generic signing route.

## Required process configuration

- Read the publisher signing key from an explicit secret-mounted path. Do not
  generate a key at startup and do not fall back to the node or operator key.
- Read a high-entropy bearer from an explicit secret-mounted file and pass its
  contents to `PublisherServerState::new`. Do not put it in a URL.
- Bind plain HTTP only to a loopback address. For a remote release-authority,
  terminate HTTPS with authenticated infrastructure in front of the loopback
  listener and expose no other route from this process.
- Give the process access only to the publication CAS and proof inputs needed
  by `LocalConstrainedPublisherAuthority`. The release-authority must either
  share that CAS or explicitly import the referenced immutable objects before
  requesting authorization.
- Configure the release-authority daemon with a base URL ending in `/` through
  `RYEOS_BUNDLE_PUBLISHER_URL` and the matching bearer through
  `RYEOS_BUNDLE_PUBLISHER_BEARER`. Its node policy pins the expected publisher
  fingerprint independently.

The only accepted paths are:

- `POST /v1/bundle-recipe/authorize-build`
- `POST /v1/bundle-recipe/authorize-capture`
- `POST /v1/bundle-tree/sign`
- `POST /v1/bundle-generation/authorize`
- `POST /v1/bundle-catalog/authorize-successor`

Every request must use `Authorization: Bearer …`, is limited to 256 KiB, is
decoded through a current strict request schema, and is validated before the
authority runs. Unknown paths are not routed. Publisher errors do not create a
second signing surface: callers receive an unsuccessful response and must not
infer or manufacture an attestation.

The recipe operation accepts only `catalog_namespace`,
`bundle_publication_policy_section_digest`, `trust_epoch`, and the closed
`release_input` plan. It rejects Core, unsupported inputs and mismatched policy
coordinates. The publisher constructs a fixed Config template with exact build
parameters and bounded product allowances; callers cannot supply Config bytes,
graph refs, output paths, capabilities or arbitrary signing payloads. The returned
signed Config and its exact blob/body hashes are not execution admission or proof
that a source snapshot exists. Normal signature/trust, retained-source, payload
ownership and product admission checks remain mandatory.

This endpoint is implemented independently of release-graph wiring. The recipe
must be installed at its fixed identity in a retained per-release execution
workspace, not written into the clean source checkout or substituted through an
unchecked ref override. That workspace/source admission and post-sign capture
integration are still pending; this endpoint alone does not make releases runnable.

The `ryeos-bundle-publisher` binary is the explicit deployment entrypoint. It
accepts `--publisher-key`, `--bearer-file`, `--cas-root`, and `--policy`, and
defaults to `127.0.0.1:7411`. The strict JSON policy pins the catalog namespace,
bundle-publication policy digest and trust epoch; qualification signer public
key, policy, verifier definition and artifact identities, and required claims;
and publisher-tool definition and artifact identities. Unknown fields are
rejected. The process constructs `LocalConstrainedPublisherAuthority` and binds
only after every input validates.

The policy schema is `ryeos.standalone_bundle_publisher_policy.v1`. Public keys
are JSON arrays of exactly 32 Ed25519 bytes. The qualification policy source
and verifier artifact identity use their existing strict current wire forms;
operators should export those admitted objects rather than reconstructing
them. The CAS directory is descriptor-pinned before the key-bearing listener
starts.

## Provision measured authority before publishing

All shipped initialization profiles start with `bundle_publication.catalogs: []`.
This explicitly authorizes no catalogs. There is no default development key,
placeholder digest, automatic trust-on-first-use, or inferred verifier identity.
Installing the release-authority profile alone does not enable publication.

Before initialization, obtain the publisher's public trust document through an
authenticated operator channel and verify its fingerprint out of band. Pin it
on every release, bundle-source and consumer node using the existing
`ryeos init --trust-file /secure/PUBLISHER_TRUST.toml` option alongside that
node's source, profile and substrate arguments. A fingerprint in the publication
policy does not install or authorize a public key; the key must independently
exist in the node TrustStore. Do not use the repository development key for a
production publisher or copy its private key into any node.

Prepare an operator-owned YAML section containing `schema: 1` and the exact
catalog entries defined by `BundleCatalogPolicy` in
`crates/daemon/ryeos-app/src/node_policy/sections/bundle_publication.rs`.
Obtain qualification policy source, verifier definition and launch-artifact
identities from the admitted release environment, not from source-file hashes.
Pin the actual qualification signer public key/fingerprint, publisher fingerprint,
publisher-tool definition/artifact identity, required claims and nonzero trust
epoch. These values are deployment-specific and cannot be supplied by the source
repository. A managed-runtime inference artifact is not a native verifier.

Each catalog must also specify `authorized_uploaders`: a sorted, unique list of
the release **node** public-key fingerprints permitted to upload and publish
already-signed catalog content. An empty list permits no writes. The publisher
fingerprint is not implicitly an uploader. Keep the publisher's private key
exclusively in the constrained publisher; do not install it as a release-node
identity. Authorize each uploader separately for the bundle-source service
capabilities it needs. Both service-capability admission and catalog-specific
uploader policy are required. A release submission's caller needs its own
release-service capabilities, not the publisher key or the release node key.

Upload sessions remain owned by the actual authenticated uploader. A different
uploader cannot resume that session, even when both are allowed by catalog policy.
Catalog/set/generation signatures are always checked against the independent
publisher identity; transport permission never grants signing authority.

Use the same complete section on the release, bundle-source and consumer nodes:
the section digest binds the evidence, so independently different catalog lists
are not interchangeable. With each node stopped, install it using the existing
operator-authorized policy boundary:

```sh
ryeos node policy-apply bundle_publication /secure/bundle-publication.yaml --app-root /data/app
```

Derive the standalone publisher's policy from those same bytes using its
read-only preparation mode (no signing key, CAS access or listener required):

```sh
ryeos-bundle-publisher --prepare-policy /secure/bundle-publication.yaml --catalog-namespace official
```

Capture that JSON as the publisher's `--policy` input. Preparation validates
the typed section and derives its canonical digest; it does not establish that
the operator's measurements are trustworthy. Normal publication still verifies
the actual evidence against every pin. Test a complete release in the admitted
environment before enabling production consumers. Rotate measured authority
through explicit stopped-node policy changes and an updated publisher policy.

For development, run the publisher on a dedicated loopback port under the
same operator account as its CAS, with the key and bearer mounted read-only.
Production deployment should put key custody in a dedicated service account
or hardware-backed signer while keeping this exact typed API.
