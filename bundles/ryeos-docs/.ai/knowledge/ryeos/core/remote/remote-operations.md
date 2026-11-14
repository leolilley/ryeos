<!-- ryeos:signed:2026-11-14T07:17:08Z:eb004e3d1e114d83c9a600606207c429cf562c70f41f1a98707240d5ce505862:LzwRryAQbuOUCdQu1PrTIWYlPS9JnUcaRFqkvmDoD8xEpz2vLpOtg6y9siGtJ57LCQzVdzBTFBKY/EYlAndIAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [remote, operations, trust, security, networking]
version: "3.7.7"
description: >
  Remote execution and bundle synchronization — trust model,
  operator workflows, fail-closed semantics, and security requirements.
  Covers the per-request engine, hybrid binary resolution, project
  pull-back, single-flight cache builds, and live bundle roots.
---

# Remote Operations

Remote operations allow one ryeos node to execute items, push/pull
CAS objects, and synchronize bundles with another node over HTTPS.

See also:

- [Remote Command Reference](remote-command-reference.md) for command syntax, examples, capabilities, scopes, routes, and failure modes.
- [Identity Model](../identity-model.md) for the four trust layers and request authentication flow.
- [Spaces](../spaces.md) for project/system resolution.
- [Execution Provenance](../execution/provenance.md) for local vs pushed-head execution source invariants.
- [Execution Isolation](../node/execution-isolation.md) for the target node's immutable subprocess boundary.
- [HTTP API](../node/http-api.md) and [Services: remote](../services/remote.md) for route/service mappings.

## v1 Trust Boundary

v1 remote execution is for **operator-trusted remotes**, not mutually
untrusted tenants.

- CAS is shared within a node, but reads are scoped by root: `objects/get`,
  `objects/has` and the closure services serve only hashes reachable from a
  typed root the caller is entitled to (`project_head`, `thread_result`,
  `bundle_export`, `placement`, `admitted_subject`, `catalog_publication`),
  and refuse anything else with `object_not_in_scope`. Holding
  `ryeos.execute.service.objects/get` does not by itself reach another
  principal's objects. The unscoped read, `objects/get-any`, is operator-only
  and never part of a recommended grant. See [Services: objects](../services/objects.md#authority-reads-are-scoped-by-root).
- Vault is a single shared store. Capability checks gate the vault services,
  and the target node's signed `sealed_vault_names` policy bounds, per name,
  who may write, list and receive it; a name without an entry is operator-only
  there. The target judges the principal it admitted: the delegated user when
  the caller holds a delegation, otherwise the calling node.
- Remote requests are signed with this node's Ed25519 key and carry the
  calling principal's own signed delegation. The remote node admits the
  principal from its authorized-keys trust store and caps the request to the
  delegation (see [Identity Requirements](#identity-requirements)). A call made
  as the node itself, with no principal behind it, is signed by the node key
  alone.
- Granting the local `remote.admin` capability is operator-level access
  to high-impact remote orchestrators in v1. It does not replace
  target-node authorized-key scopes.
- A remote's pinned identity (`signing_key`, `principal_id`,
  `vault_fingerprint`) comes only from the node-signed pin records under
  `.ai/node/remotes/`. A project cannot supply or override it: see
  [Configuration](#configuration).

## Identity Requirements

A remote run acts as the **principal that asked for it**, not as the node. Each
principal signs, with its own key, a bounded delegation that lets this node act
for it on one remote. The daemon never holds a principal's private key, the
operator's included: the operator is one more principal that signs a
delegation.

### Getting admitted as a principal

A principal that has no grant on the remote yet is admitted with a
`remote_principal` token. Three steps, in order:

1. **Mint the token on the remote (node B).** B's operator mints a token for
   the principal's home site and sends the cleartext to the principal out of
   band.

   ```bash
   ryeos admission-token --class remote_principal --origin-site-id "site:home" \
     --scopes "ryeos.execute.service.remote/run,ryeos.execute.service.vault/list"
   ```

   B must already admit the home node (A) as a `remote_node` of `site:home`
   holding `ryeos.attest.request.forwarded-operator`, because that node
   co-signs every principal claim. A pinned outbound remote for B must exist
   on A (`remote configure`).

2. **Admit from the home node (node A).** The principal runs this against A.
   The CLI signs the claim with the principal's own key; A checks that the
   signature is the caller's, co-signs it as the origin site, and forwards it
   to B. A never holds the principal's key.

   ```bash
   ryeos remote admit --as-principal --remote hosted --token "<one-time-token>" \
     --scopes ryeos.execute.service.remote/run \
     --scopes ryeos.execute.service.vault/list
   ```

   The caller needs `ryeos.execute.service.remote/admit-principal` on A.
   Scopes must be a subset of the token's. The grant on B is a
   `remote_principal` grant bound to A's site, with an expiry bounded by B's
   authorization policy. A remote pinned inbound-only is refused with
   `remote_inbound_only`.

3. **Delegate.** With the grant in place, sign the delegation that lets A act
   for the principal (next section).

### Delegating

```bash
ryeos remote delegate --remote hosted \
  --scopes ryeos.execute.service.remote/run,ryeos.execute.service.remote/push \
  --lifetime-secs 3600
```

The CLI asks the node for the identities of both parties, signs the statement
with the caller's own key, and hands it to the node, which stores it
(node-signed) for that principal and remote and replaces any earlier one. The
signed statement is `{schema, delegator, delegate_site, delegate_node,
audience_site, audience_node, scopes, not_after, nonce}`:

- `delegator` is the signing principal (`fp:<fingerprint>`);
- `delegate_site` and `delegate_node` name this node, which may use it;
- `audience_site` and `audience_node` name the one remote it is addressed to;
- `scopes` are the capabilities the principal lends, a subset of what the
  remote's grant allows;
- `not_after` is the last second it is honoured. The node refuses to store one
  that lasts more than 30 days;
- `nonce` makes every delegation distinct, so each has its own digest.

A delegation is addressed to a remote this node dials, so `remote delegate`
against an inbound-only pin is refused with `remote_inbound_only`. The remote's
own verification, where the origin calls in, does not depend on how the remote
pinned the origin, so an origin pinned inbound-only there still works.

The caller needs the local grant `ryeos.execute.service.remote/delegate`.
`ryeos remote delegation list` shows the stored delegations;
`ryeos remote delegation revoke --remote <name>` revokes one (below).

### What the remote checks

For every request the node signs with its own key, adds its forwarding
co-signature, and sends the delegation in `x-ryeos-delegation`. The remote
verifies, in order:

1. the source node's key is admitted as `remote_node` for the asserted source
   site, with `ryeos.attest.request.forwarded-operator`, and its co-signature
   covers the exact request;
2. the delegation names the remote as its audience and the forwarding node as
   its delegate, has not passed `not_after`, and carries a valid signature by
   the delegator;
3. the delegator holds a `remote_principal` grant on the remote bound to that
   same source site;
4. no tombstone for the delegation's digest is on the remote's ledger.

The request then runs as the delegator with `delegated_via` set to the source
site. Its effective scopes are the delegator's grant **intersected with** the
delegation: a delegation never widens the grant, and the grant is never used
beyond the delegation. The delegation digest is recorded with the forwarding
evidence on the remote and in the source node's durable job records, so a
result names whom it was for.

A node that is not the principal's home cannot forge the principal, and a
compromised source node can only act inside the scopes and time the principal
signed.

### Outbound principal

`remote push`, `remote run`, `remote execute`, target-site forwarding from
`/execute`, the `remote vault-*` commands and the product and testimony
exports make the call as the **caller under its delegation** by default. When
the caller is the node itself, the call is signed by the node key alone.
`outbound_principal` on `remote/push` and `remote/run` selects explicitly:
`delegated` or `node`. Retained-current-HEAD launches are always delegated.

Refusals are typed and name the cure:

| Code | Where | Meaning and cure |
|------|-------|------------------|
| `remote_delegation_missing` | source | no stored delegation: `ryeos remote delegate --remote <name>` |
| `remote_delegation_expired` | source or remote | past `not_after`: sign a new one |
| `remote_delegation_replaced` | source | a durable operation began under another delegation: start it again |
| `remote_delegation_not_transitive` | source or remote | a request that arrived through a delegation cannot delegate onward |
| `remote_delegation_malformed` | remote | not a canonical delegation |
| `remote_delegation_bad_signature` | remote | not signed by the delegator's key |
| `remote_delegation_wrong_audience` | remote | addressed to another node: sign one for this remote |
| `remote_delegation_wrong_delegate` | remote | names another forwarding node |
| `remote_delegation_revoked` | remote | tombstoned on the remote |
| `remote_delegation_principal_not_admitted` | remote | the delegator has no `remote_principal` grant for that source site |

### Revoking

`ryeos remote delegation revoke --remote <name>` asks the remote to record a
tombstone for the delegation's digest and then deletes the stored delegation.
The remote accepts the tombstone from the delegating principal (so the
delegation must lend `ryeos.execute.service.identity/revoke-delegation`, and
the principal's grant must hold it) or from its own operator, who can record
one for any digest with `ryeos delegation tombstone --delegator <fp:...>
--delegation-digest <digest>`. If the remote cannot be asked, the local record
is kept and the refusal names the operator command; `--local-only` deletes only
the local record. A tombstone ends one delegation at once; revoking the
principal's grant ends all of them, and `not_after` bounds the rest.

## Prerequisites

### On the remote node

1. **Bootstrap**: run `ryeos init --node-profile <distribution-profile>
   --host-class <class>` so
   the remote has node identity, vault key material, local operator
   authorization, the exact distribution bundles, and a complete signed node
   policy generation. Packaged full nodes use `full` with class `local`;
   images use the profile mapped by their bundle-set packaging, with the class
   the deployment sets in `RYEOS_INIT_HOST_CLASS` (`hosted` in a hosting
   provider's container, `local` on a host the operator runs).

2. **Authorize the caller node key**: either mint a one-time admission
   token on the target node, or directly authorize the caller node key.
   For direct authorization, the remote operator needs the caller node
   public key in `ed25519:<base64>` form. The caller gets it with:

   ```bash
  ryeos identity
   ```

   Preferred bootstrap is token based: the remote operator runs a local
   offline tool on the target node and sends the cleartext token to the
   caller out of band:

   ```bash
   ryeos admission-token \
     --class remote_node \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
     --ttl-secs 600
   ```

   The caller then configures the remote and claims the grant with a
   self-signed admission request:

   ```bash
   ryeos remote configure --remote production --url https://ryeos.example.com \
     --expect-fingerprint <remote node fingerprint>
   ryeos remote admit \
     --remote production \
     --token "<one-time-token>" \
     --label "dev-machine" \
     --scopes ryeos.execute.service.objects/has \
     --scopes ryeos.execute.service.objects/put \
     --scopes ryeos.execute.service.objects/get \
     --scopes ryeos.execute.service.system/push-head
   ```

   Direct local authorization remains available when the operator has the
   caller's node public key:

   ```bash
   ryeos authorize-key \
     --public-key "ed25519:<caller_node_pubkey_b64>" \
     --label "dev-machine" \
     --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"
   ```

   Every grant expires. Without `--lifetime-secs` it lasts the class maximum
   in the target's signed authorization policy (`remote_node` 90 days,
   `remote_principal` 30 days, `local_client` 365 days in the shipped
   profiles). When it lapses the key holder receives HTTP 401
   `grant_expired`; the principal that granted it (or the target's operator)
   renews it with `ryeos renew-key --fingerprint <fp>`. The grantee cannot
   renew its own grant. A grant is withdrawn early with `ryeos revoke-key`
   (or `ryeos remote revoke`), which leaves a tombstone. See
   [Identity model](../identity-model.md).

3. **Choose scopes by operation**. Do not mix local remote-service caps
   with remote authorized-key scopes. The complete matrix is in
   [Remote Command Reference](remote-command-reference.md#authority-matrix).

For a principal's delegated runs, do not use admission claim or the remote
authorize endpoint: both create ordinary remote-node/client authority. Stop
the target and use its local operator to install the principal's public key as
an exact origin-bound `remote_principal` grant. A fresh target needs no
semantic-conversion flag; supply it only when an incumbent grant for that same
fingerprint is deliberately being reclassified or rebound:

```bash
RYEOS_APP_ROOT=/path/to/target-app-root ryeos authorize-client \
  --public-key "<principal_raw_ed25519_base64>" \
  --label "principal forwarded from source" \
  --origin-site-id "site:<source>" \
  --scopes "<comma-separated exact workflow scopes>"
```

Before that, separately admit the source node key with the exact scope
`ryeos.attest.request.forwarded-operator`. The principal's grant is an
allowed-site constraint, not evidence that a request transited the source. The
source-node co-signature covers the exact primary request and supplies that
proof. Forwarded HEAD and execute bodies also include a caller-signed
required-origin assertion so a missing or wrongly classified grant/proof fails
closed; the assertion only narrows verified authority and can never create it.

One grant exists per key fingerprint, so the target classifies the principal's
key as remote. The target-local operator remains a separate `local_client` and
performs local maintenance without changing that grant. The offline tool holds
the same exclusive state lock as the daemon throughout publication and refuses
if the daemon is live. When an incumbent grant is deliberately reclassified,
`--allow-semantic-conversion` is required and `--merge-scopes` is forbidden.

Admission initially grants only the declared admission scopes. When that same
source node also transports object closures or project heads, the target-local
operator extends its existing `remote_node` grant in place:

```bash
RYEOS_APP_ROOT=/path/to/target-app-root ryeos authorize-client \
  --public-key "<source_node_raw_ed25519_base64>" \
  --remote-node-origin-site-id "site:<source>" \
  --merge-scopes \
  --scopes "<comma-separated exact peer scopes>"
```

Do not use `--origin-site-id` for this key: that flag describes the separate
forwarded `remote_principal` principal.

   Common remote-side scopes:

   | Operation | Remote scopes on target |
   |-----------|-------------------------|
   | `remote push` | `ryeos.execute.service.objects/has`, `ryeos.execute.service.objects/put`, `ryeos.execute.service.system/push-head` |
   | `remote reconcile-project-head` | the delegating operator: push scopes; peer node: `ryeos.execute.service.objects/closure/get` |
   | `remote pull` | `ryeos.execute.service.objects/get` |
   | `remote execute` | push scopes + `ryeos.execute.service.objects/get` + whatever caps the executed item requires; `ryeos.use.vault.operator` only if the policy exposes the target's operator vault (not by default, see below) |
   | `remote authorize` | `ryeos.execute.service.identity/authorize-key` |
   | `remote bundle-install` | `ryeos.execute.service.bundle/export`, `ryeos.execute.service.objects/get` |
   | `remote vault-set/list/delete` | matching `ryeos.execute.service.vault.*` cap |
   | `remote threads/thread-status` | signed auth; no additional thread service cap in v1 |
   | `external-content push-product` | `ryeos.execute.service.objects/stage-chunk`, `ryeos.execute.service.external-content/product-offer`, `ryeos.execute.service.external-content/product-accept` (forwarded `remote_principal` principal) |

   `ryeos.use.vault.operator` lets an execution read the target's
   operator vault (`include_operator_vault: true`, or the `operator` vault
   namespace). `remote execute` and `remote run` send
   `include_operator_vault: false` by default, so no remote scope list needs
   it. Do not grant it to a remote caller node by default: that node could
   then run any item it may execute on the target operator's secrets. Grant
   it only to a principal that should hold operator-level secret access, and
   prefer a target-side project `.env` or provider environment. The source
   also requires its own caller to hold it before forwarding such a policy.

   Short-form scopes like `bundle.install` will never authorize a
   handler — the matcher does not auto-prefix and the daemon's auth
   loader refuses to load TOMLs that contain them.

   `ryeos external-content push-product <remote> <witness-hash> <origin-admission-hash> [maximum-bytes]` is the origin-side counterpart of `receive-product`. It offers the product's witness, admission and closure objects (`product-offer`); the hosted node verifies the admission against the pinned remote key before storing anything. Missing blobs and large objects then go up in bounded chunks (`objects/stage-chunk`, at most 512 KiB each, resumable) into an owned durable stage, and `product-accept` publishes the acceptance under the node identity once the closure and every large object are resident. Large objects live in a separate large-object tier and are read back in bounded chunks through `objects/get` `large_object_chunk`. The byte bound defaults to the product declaration's signed `maximum_total_bytes`, the bound its capture already enforced; an explicit bound may only narrow it (above it the push is refused `product_push_bound_exceeds_declaration`), and neither may exceed this node's import policy (`product_push_bound_exceeds_import_policy`).

### On the caller node

1. **Configure the remote**:

   ```bash
   ryeos remote configure --descriptor ./production.remote.yaml
   # or, with the node fingerprint the remote operator gave you:
   ryeos remote configure --remote production --url https://ryeos.example.com \
     --expect-fingerprint <remote node fingerprint>
   ```

   The remote node key must be pinned by a descriptor or an expected
   fingerprint obtained out of band; configure never trusts the key it is
   shown on first contact. Without either pin it refuses with
   `remote_key_unpinned`, writes nothing, and names the fingerprint the live
   node reported so you can confirm it with the remote operator.

   This discovers the remote's public key, principal id, canonical site ID,
   vault fingerprint, and ingest-ignore rules. A descriptor is a trust pin
   only: it is signed by the remote node key it pins and binds the remote's
   name, URL, site ID, vault fingerprint and key together. Configure verifies
   that signature, then checks the pins against the live `/public-key`
   document before the complete discovered identity is written as a
   node-signed pin record under `.ai/node/remotes/`. Only the configured local
   operator can run `remote configure` and `remote bind-project`.

   The configured endpoint is one canonical, credential-free base URL. It has
   no user information, query, fragment, control/whitespace characters, or
   trailing slash. Direct operator input is normalized before contact and
   persistence; a signed descriptor must already use the canonical form.
   Audience discovery and all signed requests refuse redirects, so configure
   the final target origin directly.

   `remote status` and `remote doctor` compare the live principal, signing
   key/fingerprint, site ID, and vault fingerprint with that configured
   coordinate before making signed probes. A mismatch skips all authenticated
   thread/project contact. `remote admit` applies the same check before it
   releases a one-time token to the target. Remote pins exist only as
   node-signed records on the node: a project never supplies or overrides a
   remote, so a project cannot change a credential destination. A pin record
   that is unsigned, edited or signed by another key is refused with a typed
   `remote_pin_*` code that names the cure.

### Publishing a descriptor

Any node operator can export a descriptor from the running daemon (the signed document is the `yaml` field of the reply):

```bash
ryeos remote-descriptor \
  --name production \
  --url https://ryeos.example.com \
  --capabilities "remote-execute,bundle-install"
```

The exported descriptor (version 2) is signed with the node key it pins. The
signature proves the producer held that key and makes the bound name, URL,
site ID and vault fingerprint tamper-evident; it does not replace checking the
fingerprint with the remote operator out of band. Version 1 descriptors are
unsigned and are refused with `remote_descriptor_unsigned`: ask the remote
operator to re-export. A descriptor signed by a different key is refused with
`remote_descriptor_wrong_signer`, and an edited one with
`remote_descriptor_tampered`.

The descriptor is intentionally generic core data. RyeOS Cloud or another
hosted/provider bundle may distribute it through a web UI later, but the
authority model does not depend on that distribution channel. The caller
still verifies the descriptor against the live node identity during
`remote configure --descriptor`.

A version 2 descriptor always carries the node's site (`site_id`) and vault
fingerprint (`vault_fingerprint`), and may carry its signed ingest-ignore rules
(`ingest_ignore`) and a `url`. All of them are covered by the node signature.
With them a node can pin the remote without contacting it:

```bash
ryeos remote pin origin --descriptor ./origin.remote.yaml
```

`remote pin` verifies the descriptor's node signature and writes a node-signed
pin record under `.ai/node/remotes/` from the descriptor alone (`pin_source:
descriptor_without_contact`). The descriptor is the trust anchor, so it must
reach the pinning node through a channel its operator trusts. A version 1 or
unsigned descriptor is refused with `remote_descriptor_unsigned`. One without
ingest rules pins a remote this node never pushes to: push, execute and project
sync to it refuse with `remote_pinned_without_ingest_rules` until `remote
configure` (or a pin with the rules) replaces the entry.

**Inbound-only peers.** A descriptor without a `url` pins an inbound-only
remote: a node behind NAT that only ever calls in, such as the origin pinned by
a hosted controller. The pin record carries `reachability: inbound_only` and no
URL, and still holds the site, node key and vault fingerprint that inbound
verification and grant origin checks use. Every path that dials a remote (run,
push, pull, execute, sync, status, doctor, configure) refuses an inbound-only pin
with `remote_inbound_only`, naming the remote. Export one with
`ryeos remote-descriptor --name <name>` and no `--url`.

## Wiring a hosted node without a shell

A hosted controller behind an origin's NAT cannot dial the origin, and its
operator has no terminal on it. The origin operator wires it with three signed
offline operations, which the controller's signed profile delegates
(`maintenance.remote_offline_operations` in the `hosted-controller` profile):

| Operation | Input | Effect |
|---|---|---|
| `node.authorize-client` | arguments only | Reconcile one `remote_node` or `remote_principal` grant (public key, exact scopes, label, origin site, `merge_scopes`, optional `lifetime_secs`). Never an offline-operation scope or a wildcard. The grant ends: `lifetime_secs` (default the class maximum of the controller's signed `authorization` policy) above that maximum fails with `grant_lifetime_exceeds_policy`. Its `authorized_by` is the request's signer and the request digest. The configured operator's fingerprint is refused with `grant_is_operator`. |
| `node.remote.pin` | `descriptor` (64 KiB) | Pin a remote from its signed v2 descriptor (a URL-less one pins an inbound-only remote), as `ryeos remote pin`: a node-signed record under `.ai/node/remotes/`. |
| `node.bundle.replace` | `archive` (2 MiB `.tar.gz`) | Install or replace a bundle, verified as `bundle install --replace` is. The archive holds one top-level directory named after the bundle, and only directories and regular files. |

Each is signed against the controller's facts (`authorized_keys`, `remotes`,
`bundle_registrations` respectively), delivered through the origin node, and
applies at the controller's next start.

**Delivery through the origin node.** The origin operator's own node forwards
the operator's calls to the controller under a delegation the operator signed
for it, the same way `remote run` and `remote push` act for the operator. The
origin node's key signs and co-signs each call and the operator's delegation
rides with it, so the controller admits the operator as the principal; the
origin node never holds the operator's key and never signs the request. Input
files (a descriptor, a bundle archive) travel in the same call, so nothing is
written onto the controller's container outside its own inbox.

The controller must already hold two grants, which only its local operator
can make (through the platform channel below, once): the origin node as a
`remote_node` bound to the origin's site with the forwarded-operator
attestation scope, and the origin operator as a `remote_principal` bound to the
same site whose scopes name the offline-operations routes
(`ryeos.execute.service.offline-operations/submit`, `/facts`, `/status`) and
the exact `ryeos.offline.node.*` scopes it may sign. The origin pins the
controller with its URL (the controller is the side the origin can dial).

On the origin, the operator then lends the routes' scopes to its node for the
controller and works from there:

```bash
# once, and again before it expires (default 8 hours, at most 30 days)
ryeos remote delegate --remote controller \
  --scopes ryeos.execute.service.offline-operations/submit,ryeos.execute.service.offline-operations/facts,ryeos.execute.service.offline-operations/status

ryeos node offline-operation facts --remote controller --json > controller-facts.json
ryeos node offline-operation sign --operation node.remote.pin \
  --arguments pin.json --input descriptor=./origin.remote.yaml \
  --facts controller-facts.json --out pin-delivery.json
ryeos node offline-operation submit --remote controller --file pin-delivery.json --stop
ryeos node offline-operation list --remote controller
```

The controller's facts name the caller's own authority on it (`signer`): read
through the origin, that is the operator's current `remote_principal` grant
(its digest and origin site), not the origin node's grant. `sign` takes the
grant digest and origin site from there, so the request is always signed
under the grant as it now stands; after a `node.authorize-client` that changes
the operator's own grant, read the facts again. `sign` refuses facts that name
no signer (`offline_operation_facts_without_signer`), facts read for another
key (`offline_operation_facts_for_another_signer`), and a key the controller
holds no usable grant for (`offline_operation_signer_ungranted`, with the
reason when the grant exists but cannot be verified, such as an expired one).

The controller's effective scopes for the operator are its grant intersected
with the delegation, so both must name a route's scope. The refusals name
their cure:

| Code | Where | Meaning and fix |
|---|---|---|
| `remote_delegation_missing` | origin | No delegation stored for (operator, remote): `ryeos remote delegate --remote <name> --scopes ...`. |
| `remote_delegation_scope_missing` | origin | The stored delegation does not lend the route's scope; the message names it. Delegate again with it. |
| `remote_inbound_only` | origin | The remote is pinned without a URL. An inbound-only remote is never dialed; delivering to one would need it to poll, which no path does. |
| `offline_operation_signer_not_caller` | origin | The request is signed by another key than the caller's; the controller queues a request only from its signer. |
| `remote_grant_scope_missing` | controller | The controller admitted the operator, but its grant does not name the route's scope; the controller's local operator adds it. |
| `remote_delegation_*` (`principal_not_admitted`, `revoked`, `expired`, ...) | controller | The controller refused the delegation; the message names the cure. |
| `remote_offline_operation_refused` | controller | The controller verified the request and refused it; `reason` is its offline-operation refusal (`stale_ledger_head`, `not_delegable`, `grant_changed`, ...). |

The controller records each forwarded delivery as the operator: its reply
names `submitted_by` (the operator, which must also be the request's signer)
and `delegated_via` (the origin's site). The request itself names the signer,
its grant digest and its origin site, and the node-signed receipt the next
start writes covers that request.

**The platform channel.** A controller that cannot start, or one that does not
yet hold the two grants above, is reached through the channel the entrypoint
reads (`RYEOS_OFFLINE_OPERATION_REQUEST` and `RYEOS_OFFLINE_OPERATION_INPUTS`,
see `deploy/composition-images.md`): `sign --stage-dir` writes the files it
takes.

Requests pin the ledger head, so sign the next operation against the facts
the controller reports after the previous one applied, or sign each against
the same head and submit them in order of application. A bundle replacement
needs the daemon's own startup: it composes the node's configuration as
`run-service` does to verify the new bundle, and only the daemon can.

Delegating any of these is close to delegating the whole controller: a grant
or a bundle changes who the node trusts and what it runs. The operator whose
grant names the `ryeos.offline.node.*` scopes is trusted that far. Those
scopes are themselves granted only by the controller's local operator.

## End-to-End Operator Workflow

This is the copy-pasteable two-node bootstrap path. It uses only generic
core primitives; hosted/provider bundles can later wrap the same flow in
UI or provisioning automation.

### Target node / remote operator

Export a descriptor trust pin for the node and mint a one-time admission
token. Save the descriptor `yaml` to a file and deliver it, with the token, to the caller out of band.

```bash
ryeos remote-descriptor \
  --name prod \
  --url https://ryeos.example.com

ryeos admission-token \
  --class remote_node \
  --label "dev-machine" \
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
  --ttl-secs 600
```

### Caller node

Import the descriptor, claim the grant, verify setup, then execute.

```bash
ryeos remote configure --descriptor ./prod.remote.yaml

ryeos remote admit \
  --remote prod \
  --token "<one-time-token>" \
  --label "dev-machine" \
  --scopes ryeos.execute.service.objects/has \
  --scopes ryeos.execute.service.objects/put \
  --scopes ryeos.execute.service.objects/get \
  --scopes ryeos.execute.service.system/push-head

ryeos remote doctor --remote prod

ryeos remote execute --remote prod --item-ref tool:my/heavy-compute
```

Operational notes:

- The descriptor is a trust/discovery pin, not a credential. Importing it
  still verifies the live `/public-key` response before local config is
  written.
- The admission token is one-time bootstrap material. It must be
  delivered out of band, is shown only once by the token tool, and is
  consumed by `/admission/claim`.
- Non-loopback admission should use HTTPS. Request signing authenticates
  claims, but it does not encrypt the token in transit.
- Admission tokens and admission claims must use concrete scopes only;
  wildcard scopes are rejected.
- Runtime authority after admission is the normal target-node-local
  `authorized_keys` grant for the caller node key.

The synchronous `remote execute` command:

1. **Push** — ingests the local project, uploads missing CAS objects
   to the remote, advances the remote's HEAD ref.
2. **Execute** — runs the specified item on the remote using the
   pushed content.
3. **Pull** — fetches the resulting snapshot, diffs against the
   exact project tree that was pushed, and applies changes to the local workspace.

### Clean-Base Guarantee

If the pull detects that any local tracked file has changed since the
push, the entire apply is **aborted** — no partial writes. The local
HEAD ref is rolled back to its pre-push state.

## No Wildcard Delegation

Bootstrap may create `["*"]` keys locally for the node operator.
Remote authorization (`ryeos authorize-key`) and admission bootstrap
reject any requested or token-file scope containing `"*"`. All remote
keys must enumerate their capabilities explicitly.

## Node-Key Rotation

When a node's signing key is compromised or needs rotation, treat it as a
break-glass operation. The daemon no longer has an `--init-only` path and
will not auto-regenerate the node key, because doing so would invalidate
the signer records, which the node key signs.

Safe rotation requires explicit operator action:

1. Stop the daemon.
2. Replace/regenerate the node signing key under
   `<system>/.ai/node/identity/private_key.pem`.
3. Re-pin every signer with `ryeos trust pin` so each signer record is
   signed by the new node key.
4. Re-sign or recreate node-signed local config items as needed.
5. Reissue every remotely granted authorized key.

This intentionally invalidates delegated authority until remotes
authorize the new node key.

### Key rotation on the caller side

1. Rotate the node key on the caller.
2. On the remote: `ryeos authorize-key` with the new node public key.
3. On the caller: `ryeos remote configure` with the caller's existing
   `--descriptor` or `--expect-fingerprint` pin to pick up changes.
4. On the remote: revoke the superseded grant with `ryeos revoke-key
   --fingerprint <old fingerprint> --reason "rotated"` (run `ryeos remote
   revoke` from the caller, or `ryeos node authorization revoke` on the
   stopped remote). Do not delete the file by hand: a revocation leaves a
   node-signed tombstone, so a restored copy of the old grant stays refused,
   and it stops the old key's live work and schedules.

## Remote Ignore Cache

`ryeos remote configure` caches the remote node's ingest-ignore rules
in the local remotes config. Subsequent push/execute operations union those
cached target exclusions with the source node's current signed exclusions to
capture the pushed project snapshot policy and tree. The destination still
validates actual paths against its current policy before accepting a HEAD.

If the remote changes its ignore rules, re-run `ryeos remote configure` with
the same `--descriptor` or `--expect-fingerprint` pin.

If no cached rules are available, the push handler fetches them inline for the
operation without manufacturing or persisting partial remote configuration. If
the inline fetch fails, the push is **aborted** — the handler does not silently
fall back to either an empty or source-only policy.

## HTTPS Requirement

All remote communication uses Ed25519 request signing for authentication
and integrity. However, vault `set` operations send plaintext secret
values under the signing layer — they are not end-to-end encrypted by
the vault protocol itself.

**Production deployments must terminate TLS in front of `ryeosd`.**
HTTP without TLS is only acceptable for loopback addresses.

Remote clients never follow HTTP redirects. This applies both to public
audience discovery and signed requests: a redirect cannot move the configured
origin, audience, authorization header, or request body. Configure the final,
canonical base URL directly. URLs containing credentials, query, or fragment
are rejected rather than retained in config or diagnostics.

## Async Boundaries

`remote execute` is deliberately synchronous: it blocks through
push/execute/pull so project results return under the clean-base guard. Durable
accepted execution is available separately through `remote run` with an exact
caller-retained `launch_id` and retained-current-HEAD policy. That path exposes
launch/thread status and cancellation but does not recursively materialize its
project result back to the source; it is not an asynchronous form of
`remote execute`.

## Bundle Synchronization

### Remote Pull

Fetch typed CAS artifacts from a remote node and materialize them into
one new explicit directory. Operator pulls do not create unrooted local CAS
entries.

```bash
ryeos remote pull --remote production --root '{"kind":"thread_result","thread_id":"T-..."}' --object-hashes abc123 --output-dir /tmp/objects
ryeos remote pull --remote production --root '{"kind":"thread_result","thread_id":"T-..."}' --blob-hashes def456 --output-dir /tmp/blobs
```

**Fail-closed**: aborts if any requested hash is missing. No partial fetches.
Local capability: `ryeos.execute.service.remote/pull`. Remote target scope: `ryeos.execute.service.objects/get`.
`remote pull` names the remote read root with a `root` parameter (a JSON object
such as `{"kind":"thread_result","thread_id":"T-..."}`); hashes outside that
root are refused with `object_not_in_scope`.

### Remote Bundle Install

Install a complete bundle from a remote node via CAS pipeline:

```bash
ryeos remote bundle-install --remote production --bundle-name knowledge
```

**Fail-closed**:

- The export is limited to 10,000 files, 20,000 total tree entries, 64 levels,
  4 KiB paths, 32 MiB per file, and 256 MiB of declared file content.
- Object responses are streamed into bounded buffers and fetched in bounded
  batches; the installer never retains a whole-bundle blob map. The export and
  each object batch have a five-minute total request bound covering both the
  response and bounded body read.
- Missing, duplicate, malformed, wrong-sized, or hash-mismatched blobs abort
  the operation and remove the hidden staging generation.
- Network transfer uses a request-unique hidden generation while no bundle
  mutation lock is held. The installer reacquires that lock only to reconcile,
  run prospective admission, journal, and durably activate the completed tree;
  it rechecks for a concurrent install before activation. A bounded scavenger
  removes only same-owner, real-directory UUID transfer generations that have
  remained stale for at least 24 hours; current transfers are not age-eligible.
- The completed staging tree must pass signed-manifest preflight and the same
  prospective engine/node-config admission used at node boot. A bundle that
  would introduce a registry, protocol, runtime, command, or native-executor
  collision never becomes live.
- No registration is written unless activation succeeds. Transaction recovery
  completes an interrupted committed install and invalidates cached engines.

Local capability: `ryeos.execute.service.bundle/install`.
Remote scopes on the target: `ryeos.execute.service.bundle/export`,
`ryeos.execute.service.objects/get`.

### Bundle Export (server-side)

Walk an installed bundle's tree, ingest files into CAS, return manifest.
Called automatically by `remote bundle-install` — not invoked directly.
Remote target scope: `ryeos.execute.service.bundle/export`

## Additional Remote Commands

```bash
ryeos remote push --remote production
ryeos remote vault-set --remote production --name API_KEY --value "sk-..."
ryeos remote vault-list --remote production
ryeos remote vault-delete --remote production --name API_KEY
ryeos remote threads --remote production
ryeos remote thread-status --remote production --thread-id abc123
```

## Using `--input` for Complex Parameters

When a handler requires arrays, nested objects, or typed parameters
that the CLI flag binder cannot express, use `--input` to pass a JSON
file (or `-` for stdin):

```bash
# From a file
ryeos execute service:identity/authorize-key --input params.json

# From stdin
echo '{"public_key":"ed25519:abc","label":"test","scopes":["cap1","cap2"]}' | \
  ryeos execute service:identity/authorize-key --input -
```

## Per-Request Engine Overlay

When `remote execute` pushes a snapshot, the remote daemon builds a
**per-request engine** that resolves items against the caller's pushed
content rather than the remote operator's own local workspace.

### How it works

1. The caller pushes one typed, full-project snapshot closure: snapshot,
   policy, tree, file descriptors, and blobs.
2. The remote daemon checks the **engine cache** keyed by
   `(system_install_generation, snapshot_hash)`. On a miss, it:
   - Reads the live signed bundle registry with
     `BootstrapLoader::load_bundle_section()`.
   - Builds a fresh `Engine` against registered bundle roots + the
     project checkout.
3. The project checkout is **per-request** (each request gets its own
   directory, cleaned up when the request completes).
4. All dispatch paths — top-level routes, scheduler, callbacks, and
   resume — construct a single type-state `ExecutionProvenance` value
   at the entry point. Its four variants encode the legal combinations
   of role and source: `RootLiveFs`, `RootPushedHead`,
   `BorrowedChildLiveFs`, and `BorrowedChildPushedHead`. Pushed roots
   carry the snapshot hash and a non-optional `Arc<TempDirGuard>`;
   borrowed children carry no snapshot hash, so they cannot own lineage.
   It flows through `DispatchRequest`, `ExecutionParams`,
   `BuildAndLaunchParams`, and `CallbackCapability` unchanged.
   Callback children clone via `clone_for_borrowed_child()` — they
   never reconstruct provenance from other fields, and there is no
   "infer from temp_dir/snapshot_hash" heuristic anywhere in the
   codebase. Provenance is required on every callback token; there is
   no `Option`, no fallback to the daemon engine, and no deploy-window
   migration path.
5. All materialised directories in `resolve_project_context` are
   wrapped in `Arc<TempDirGuard>` immediately. Failures anywhere in
   the build path drop those guards and clean up through the existing
   `Drop` implementation. Cache-owned guards survive as long as the
   cache entry; request-owned guards survive as long as the request's
   execution guard.

### Cache invalidation

- `bundle.install` and `bundle.remove` write persistent registry YAMLs
  at `<system_space_dir>/.ai/node/bundles/<name>.yaml` and bump the
  `system_install_generation` counter. Subsequent per-request engine
  rebuilds call `BootstrapLoader::load_bundle_section()` to read the
  live registry, so the new bundle set takes effect on the next
  `pushed_head` request without a daemon restart.
- Existing cached engines continue using the bundle set they were built
  with until evicted by LRU or the idle threshold.
- The cache uses single-flight builds: concurrent first requests for
  the same snapshot serialise on one `EngineCache::get_or_insert_with`
  build. Late arrivals receive a clone of the first-built
  `Arc<Engine>`; there is no double materialisation and no same-key
  overwrite race.
- LRU eviction with a **strong-count guard**: entries whose engine
  `Arc` is still held by a running request are never evicted, even
  if that means temporarily exceeding capacity.
- Idle entries past the configured threshold (default: 30 minutes) are
  evicted on the next access, subject to the same strong-count guard.

### Isolation guarantee

A `pushed_head` request resolves only against the pushed project
checkout plus the remote's registered bundle roots. There is no
operator user space to fall through to — user-tier resolution does not
exist in the single-app-root model.

Executable tool/runtime launches then pass through the target node's sandbox
snapshot exactly like local launches. Remote content cannot enable or weaken
that policy. The default readable placeholders expose only the verified bundle
roots, resolved pushed item source, node trusted-key directory, and any exact
callback socket required by the managed launch; the request-owned checkout is
the writable project.

### Borrowed workspace for callbacks

Borrowed callback children carry a borrowed variant tag in their
provenance. The runner's lifecycle gates read this directly:
`provenance.is_borrowed_child()` returns true iff the execution is
borrowed. Borrowed children:

- skip `pin_localpath_snapshot` and `post_execution_foldback` (parent
  owns the snapshot lineage);
- do not track the workspace temp dir on their `ExecutionGuard` (the
  parent's `Arc<TempDirGuard>` pins it; the callback token's borrowed
  pushed provenance variant keeps it alive across parent crash or
  token-TTL grace period);
- have no snapshot hash field at all (the parent owns lineage; the
  child has nothing to pin).

Nested callback children (callback within a callback) preserve the
parent's provenance via `clone_for_borrowed_child`, so the grandchild's
variant remains `BorrowedChildPushedHead` and its lifeline Arc still
pins the original Root's temp dir.

Resuming a `pushed_head` thread under the per-request overlay is
tracked separately; the resume path falls back to the daemon engine.
Detached children from callbacks are disallowed at the callback
dispatcher (inline only).

## Result Verification

The remote node signs every remote-execute result it returns. The forwarding
node sends `attest_result: true` on `/execute`; the remote then adds a
`result_attestation` member to its successful response, signed with its node
key over a `ryeos.remote_execute_result.v1` statement that binds:

- `request_digest`: the `ryeos-request-v1` content hash of the exact signed
  request (method, path, body hash, timestamp, nonce, audience), so a result
  cannot be replayed onto another request;
- `item_ref`: the canonical item ref the remote executed;
- `result_digest`: the SHA-256 of the canonical JSON of the whole response
  without `result_attestation`.

Before it reads the thread status, the result snapshot or anything else from
the response, the forwarding node checks that the signer is the pinned remote
principal, that the request digest and item ref are the ones it sent, that
the result digest matches, and that the signature verifies under the key
pinned by `remote configure`. It refuses with:

- `remote_result_unsigned` when the response has no attestation;
- `remote_result_attestation_invalid` when the attestation names another
  signer, request or item, does not match the returned result, or does not
  verify under the pinned key.

A refused result fails the sync job in phase `remote_result_unverified`;
nothing is pulled or applied locally. The check is only as strong as the pin,
which is why `remote configure` requires a descriptor or an expected
fingerprint.

Both nodes must run a version with result attestation. An older remote rejects
the `attest_result` request field, and the forwarding node fails closed.

## Pull-Back

After remote execution, the pull phase applies changes to the local
project: it diffs the exact pushed project tree against the remote's result
snapshot and applies updated/deleted files to the local project
directory.

The JSON response from `remote execute` reports the counts:

```json
{
  "job_id": "remote-execute:<uuid>",
  "pull": {
    "files_updated": 3,
    "files_deleted": 1
  }
}
```

The returned `job_id` is the exact source-local durable operation coordinate.
Inspect it through `service:sync/jobs/inspect` for the canonical operation,
including the item ref, exact `ref_bindings`, target site, and remote project
path, plus retained attempts. The bounded `service:sync/jobs/list` projection
is for discovery and does not expose the canonical operation.

## Hybrid Binary Resolution

When a pushed handler descriptor references a binary that is
not installed on the remote node, the handler is recorded as
`VerifiedHandler::Unresolved`. `build_engine_for_roots` logs a warning
and proceeds.

Boot-time subprocess validation for parser and composer handlers
downgrades `HandlerBinaryMissing` specifically to a warning. Other
validation errors — non-zero exit, spawn failure, protocol violation —
still produce a `BootIssue` and fail boot. Actual **invocation** of an
`Unresolved` handler returns `EngineError::HandlerBinaryMissing` with
the handler id, reason, and structured remediation:

```
handler binary missing: 'bin/x86_64-unknown-linux-gnu/my_tool'
for handler 'handler:my/custom-tool' — binary not found in any bundle.
binary 'bin/...' not installed on this node — install the bundle
containing it or push it as a project-tier item.
```

Other boot issues (schema mismatch, signature mismatch, untrusted
publisher) still cause boot to fail. Only `HandlerBinaryMissing` is
downgraded.

This allows pushed snapshots that reference tools the remote doesn't
have to build successfully and pass boot validation, while still
producing a clear error if the operator actually tries to invoke one.

## Configuration

Each remote is one node-signed record,
`<system_space_dir>/.ai/node/remotes/<name>.yaml` (schema
`ryeos/remote-pin/v1`), written by `ryeos remote configure`:

```yaml
# ryeos:signed:<timestamp>:<hash>:<signature>:<node-fingerprint>
schema: ryeos/remote-pin/v1
pin_source: expected_fingerprint   # descriptor | expected_fingerprint | descriptor_and_expected_fingerprint
pinned_at: 2026-11-13T00:00:00Z
request: <digest of the operator request that made the pin>
remote:
  name: production
  url: https://ryeos.example.com
  principal_id: fp:<remote-signing-key-fingerprint>
  signing_key: ed25519:<base64-remote-public-key>
  site_id: site:production
  vault_fingerprint: sha256:def456...
  ingest_ignore:
    patterns:
      - ".git/"
      - "target/"
      - "*.pyc"
```

The loader verifies the node signature on every read, so editing the pinned
key, URL or a binding in place makes the record unusable until it is pinned
again. A leftover unsigned remotes config file is refused with
`remote_config_file_unsupported`; `ryeos node doctor` reports it in its
`remote_pins` row. Re-pin each remote with `ryeos remote configure`, then
delete the file.
