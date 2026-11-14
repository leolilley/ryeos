<!-- ryeos:signed:2026-11-14T07:47:54Z:fcb1e3fe14a3d7621ee6af0b1e578c1dddc58bc0428c9926217a4c8e25255ccb:nvHpNfsIvjzk40ZRdk4f+AlfqHlQCPIUEg/YNDOBIlcALKvihP6vwTI3w4yQGQjs4O/5jdslsfVuLRvGWsNlCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [identity, trust, keys, security, fundamentals]
version: "2.2.0"
description: >
  The four identity layers in ryEOS: publisher trust, user key, node key,
  and vault key; what each signs and who owns each artifact.
---

# Identity Model

## Identity is separate from executable intent

Keys identify signers. Grants define authority. Executions retain attributable
work. A directive defines model-driven behaviour; neither that definition nor
its prompted role creates a new cryptographic principal.

A directive invocation is an admitted execution, not an enduring persona.
The runtime acts through the authority admitted for that execution. A model
need not hold a signing key, and a node-authored observation does not mean the
model personally signed it.

The earlier shorthand "an agent is a signing key with a body of work" expressed
that accountability should outlive a process or model session. It is not the
current identity schema: the roles below are distinct, and custody, delegation
and key succession cannot be reduced to one persona equalling one key.

Keep principal identity, exact content identity and execution identity separate.
A valid signature is evidence of signing under key-custody assumptions, not
proof of a human identity, semantic correctness or current permission to act.
See [Mental model](mental-model.md) for the relationship to directives and work.

## Identity and trust roles

ryeOS has five distinct identity/trust layers:

| Layer | Purpose | Storage | Created by |
|---|---|---|---|
| Signer registry | Verify signed items; node-signed signer records with role, scope and status | `<system>/.ai/node/trust/signers/<fp>.yaml` | `ryeos init` / `ryeos trust pin` |
| User (CLI) key | Sign local CLI HTTP requests | `<user>/.ai/node/operator/signing/private_key.pem` | `ryeos init` |
| Node (daemon) key | Sign outbound daemon requests and authorized-key TOMLs | `<system>/.ai/node/identity/private_key.pem` | `ryeos init` |
| Authoring key | Sign items the runtime authors (`runtime.author_item`); machine drafts | `<system>/.ai/node/authoring/signing/private_key.pem` | `ryeos init` |
| Vault X25519 | Seal/unseal vault secrets | `<system>/.ai/node/vault/private_key.pem` | `ryeos init`, repaired if missing after init |

## Publisher trust

`ryeos init` pins the compiled official publisher key and any
`--trust-file` entries as signer records (role publisher, space bundle).
Additional publishers are pinned on a stopped node with
`ryeos trust pin --from <PUBLISHER_TRUST.toml> --role publisher`. The node
key is the implicit root and needs no record. Official publisher rotation requires a coordinated
binary release because the public key bytes are compiled into
`ryeos-node` and must hash to the compiled fingerprint.

## User (CLI) key

The user key is the operator identity for signing local daemon requests.
The CLI resolves it from `RYEOS_CLI_KEY_PATH` when set, otherwise from
`<user>/.ai/node/operator/signing/private_key.pem`. It must not fall back
to the node key.

## Authoring key

The authoring key signs the items a runtime authors through the daemon. It is
neither the node key nor the operator key, so authored items can be admitted or
refused as a class without touching either. `ryeos init` creates it and pins it
in the signer registry with role `authoring`, spaces `[project]` and no kinds:
authored items are machine drafts until the operator widens the scope for a kind
(`ryeos trust scope <fp> --kind <kind>`) or reviews and re-signs them. The key
home `.ai/node/authoring` is in the live-execution floor.

## Key lifecycle

Each key has one lifecycle verb, all offline and local-operator-only. A running
node refuses with `node_running_trust_change`; stop it first.

| Key | Rotate | Retire / revoke |
|---|---|---|
| Operator | `ryeos node operator rotate` | after rotation the old key is `retired`; `ryeos trust revoke <fp>` makes it verify nothing |
| Authoring | `ryeos node authoring rotate` | same |
| Publisher | `ryeos trust succeed --from <statement> --old <fp>` | `ryeos trust retire` / `revoke` |
| Node | not rotatable (see below) | not applicable |
| Vault | `ryeos-core-tools vault rewrap` | not applicable |

A rotation generates the new key in a staging path inside the key's home, has
the old key sign a succession statement naming it, and applies one signed
offline operation (`node.operator.rotate`, `node.authoring.rotate`). The
operation records the succession in the registry (the new key becomes active
with the old key's role and scope, the old key is retired and **keeps verifying
what it signed**) and installs the new key last. Installing replaces the old
private key, so no retired private key stays on disk. A rotation that stopped
part way is finished by running the same request again.

An operator rotation also:

- replaces `operator/genesis.json` with a genesis signed by the new key that
  records `predecessor_fingerprint` and `predecessor_created_at`;
- issues the new operator's grant (schema 3, `expires_at = "never"`) with
  `authorized_by` naming the old operator and the rotation request's digest, and
  removes the retired key's grant;
- reseals the init completion under the new key.

Refusals name their cure: `key_rotation_no_current_key`, `key_rotation_not_pinned`
and `key_rotation_not_active` (run `ryeos init`, or inspect the record with
`ryeos trust show`), `key_rotation_staged_key_missing`,
`key_rotation_staged_key_mismatch` and `key_rotation_old_key_mismatch` (run the
rotate command again instead of submitting the operation by hand), and
`key_rotation_signer_not_old_operator` (an operator rotation must be signed by
the key being rotated out).

Native-capacity documents (the issuance, re-issuances, re-anchors and policy
amendments) are signed by the operator and chain to the operator that issued the
capacity account. After an operator rotation the account keeps working: a
document signed by a successor operator verifies when the signer registry holds
a `succeeded` key event, node-signed, that links the issuing operator to the
signer, directly or through earlier rotations. The links are read from the
registry's event log (each `succeeded` event records its `predecessor`), never
from a capacity document, and re-issue, re-anchor and policy amendment are
signed by the current operator. A revoked operator breaks the chain at its
link: it and every operator after it are refused with
`capacity_operator_chain_broken`, naming the revoked link. The same code covers
an issuing operator missing from the registry, and a document signed by an
operator the registry does not link to the issuer. Revocation is terminal; the
cure is a fresh app root.

`ryeos node doctor` reports each key's age in its `key_age` row: the node,
operator, authoring and publisher keys, each with the time it entered service
(from the operator genesis for the operator, from the signing time of the
registry event that pinned the others, from the public identity for the node)
and its age in days. Age is reported, not judged: no signed policy sets a
rotation threshold.

## Node (daemon) key

The node key normally signs outbound remote requests, signs authorized-key
TOMLs, produces the node public identity document, and signs every signer record
in the registry; it is the implicit trust root. Daemon startup never auto-regenerates it, because doing so
would invalidate operator trust and remote authorizations.

Node key rotation is out of scope. The node key is the root of every node
document, so rotating it means re-signing the policy generation, grants, trust
records, remote pins and head refs, and re-pinning the node on every remote that
pinned it. The doctor's `key_age` row shows how old it is. The node key is
trusted for the node space only; it no longer signs project items.

### Who signs node state

Node state verifies against exactly one key, never against every pinned
key. A pinned publisher key is trusted for items, but it cannot sign node
state:

| Node document | Path | Only signer |
|---|---|---|
| Node policy generation (including `native_capacity`) | `<system>/.ai/node/policies/` | node key |
| Schedules | `<system>/.ai/node/schedules/` | node key |
| Bundle registrations | `<system>/.ai/node/bundles/` | node key |
| Any other node config under the app root (routes, commands, external execution, runtime snapshots) | `<system>/.ai/node/<section>/` | node key |
| Authorized-key grants | `<system>/.ai/node/auth/authorized_keys/` | node key |
| Init completion record | `<system>/.ai/config/onboarding/init-completion.json` | the local operator key (`.ai/config/keys/signing/`) |
| Offline operation requests | request envelope | the local operator key, or an operator granted by the node |

Routes and commands shipped inside a bundle root keep their publisher
signatures. A node document signed by any other key is refused with
`node_document_wrong_signer`, naming the file, the signer it found and the
key it expected. The cure is to remove the file: node state is written only
by the node's own commands (`ryeos init`, `ryeos bundle install`,
`ryeos node policy-apply`, schedule registration).

Ordinary remote operations authorize the caller's node key, not the user's
CLI key:

```text
CLI --[user key]--> local daemon --[node key]--> remote daemon
```

## Authorized keys

Authorized-key TOMLs are node-signed local node config. Bootstrap/admin
may grant wildcard `*`; normal remote delegation should enumerate scopes.
Daemon startup may repair the local user's authorized-key entry after
`ryeos init` has created the required keys and signer records, but the daemon
never writes signer records.

### Grant lifetime, provenance and renewal

Every grant is a schema-3 body and carries two fields that older grants lack:

- `expires_at`, required: an RFC 3339 UTC time, or the literal `never`. Only
  the configured operator's own grant (written by `ryeos init` and by
  `ryeos node reset authorization`) is issued as `never`. A grant of any other
  class, or any other `local_client`, always ends.
- `authorized_by { principal, request_digest }`: the `fp:<64 hex>` of the key
  that asked for the grant and the `ryeos-request-v1` content hash of the
  signed request that carried the ask. `granted_by` stays the granting
  authority (the asking principal, or `admission:<token>` for an admitted
  node); `authorized_by` is the evidence.

| Issuance path | `authorized_by.principal` | `authorized_by.request_digest` |
|---|---|---|
| `authorize-key` (online) | the authenticated caller | hash of the caller's signed request |
| `authorize-client` through the daemon | the configured local operator | hash of the operator's signed request |
| `ryeos-core-tools authorize-client` on the volume | the configured operator | absent: no signed request exists |
| `admission/claim` | the admitted key | hash of the admitted key's signed claim |
| `renew-key` | the renewing caller | hash of the renewing request |
| `ryeos init` and daemon start repair | the configured operator | absent: init is not a signed request |
| `node reset authorization` | the signer of the offline request | the offline request digest |

The node-policy `authorization` section sets the longest lifetime per class
(`local_client`, `remote_node`, `remote_principal`) and `warn_before_secs`, in
signed data. The shipped profiles allow 365, 90 and 30 days. A request that
names no lifetime receives the class maximum; one above it is refused with
`grant_lifetime_exceeds_policy` (HTTP 400), which names the maximum.

An expired grant authenticates nothing. The holder of its key, once its
signature verifies, receives HTTP 401 `grant_expired` naming the cure; anyone
else still receives the plain 401. The cure is `ryeos renew-key --fingerprint
<fp> [--lifetime-secs <n>]`, which re-signs the grant with a new expiry and
keeps its key, class, origin, scopes, label and granting authority. Only the
principal that granted the key (while it still holds the grant's scopes) or the
configured operator may renew: the grantee cannot extend its own grant. A
refused renewal is `grant_renewal_not_permitted`, `grant_renewal_scopes_not_covered`,
`grant_not_found` or `grant_never_expires` (the operator grant is not renewed). The operator's grant is managed only by
`ryeos init` and `ryeos node reset authorization`: `authorize-key` and
`authorize-client` aimed at the operator's fingerprint are refused with
`grant_is_operator` (HTTP 409) rather than rewriting it as a bounded grant.

Grants written before schema 3 are refused, not read: `grant_schema_stale`
names the cure (re-issue the key with `ryeos authorize-client`, or run
`ryeos node reset authorization` and re-issue every key). A grant of the
retired `remote_operator` class is refused as `grant_class_stale`: re-admit the
key as `remote_principal`. There is no upgrade or reclassify operation.
`ryeos node doctor` lists expired, soon-expiring, stale and unreadable grants
in its `authorized_keys` row.

### Naming principals across nodes

A principal is its key fingerprint, `fp:<64 lowercase hex>`, on every node:
request authentication, grants, thread records and per-principal storage keys
all use that one spelling, and a handler that takes a principal from input
accepts nothing else (`principal_storage_key` refuses any other form). A site
is `site:<name>`. A principal that arrives from another site is shown as
`fp:<hex>@site:<origin>` when its grant records an origin; that string is for
display and is never parsed back into authority.

A grant's `label` is free text for people. Two grants may share a label and
stay distinct, and code never matches on a label. The configured operator's
grant is labelled `operator`.

`ryeos node doctor` lists every readable grant in its `authorized_keys` row:
the principal as above, class, origin site, label, scope count and
`authorized_by`. The row warns when any key other than the configured operator
holds a wildcard scope.

### Revocation

Revoking a grant is an operator-signed action that leaves evidence. `ryeos
revoke-key --fingerprint <fp> --reason <text> [--leave-running] [--keep-delegates]`
writes a node-signed tombstone `.ai/node/auth/revoked/<grant digest>.toml`
(`fingerprint`, `grant_digest`, `revoked_at`, `revoked_by`, `reason`) and only
then removes the grant file. The grant digest is the sha256 of the exact signed
grant file, so a saved copy put back is refused with `grant_revoked` (the
tombstone is looked up on every read of the grant, including by the scheduler);
authorizing the key again writes a new grant with a new digest, which the
ledger does not block. Re-granting is an explicit act.

Only the principal that granted the key or the configured operator may revoke
it (`grant_revocation_not_permitted`, HTTP 403, otherwise). The configured
operator's own grant is revoked only offline, with `ryeos node authorization
revoke --fingerprint <fp> --reason <text>` on the stopped node (`grant_is_operator`,
HTTP 409, online). The offline operation converges on a rerun and refuses an
unknown fingerprint with `grant_not_found`.

Revocation is the compromise response, so by default it also:

- revokes every grant the revoked key granted, transitively, because
  delegation was bounded by the granter (`--keep-delegates` leaves them; the
  receipt lists the delegates it left);
- pauses every schedule pinned to a revoked grant with `authority_revoked`;
- cancels every live thread whose covering authority is a revoked grant, by
  the thread cancel path without its ownership check (`--leave-running` opts
  out). A thread that cannot be cancelled is listed with its error; the
  revocation itself stands.

The receipt lists every revoked grant (digest, time, reason, what it cascaded
from), the paused schedules and the cancelled threads. Expiry is different: a
lapsed grant leaves running work alone, and the scheduler pauses its schedules
at their next fire with `authority_expired`. `ryeos remote revoke` calls the
same service on a pinned remote. The doctor's `authorized_keys` row verifies
the ledger: an unsigned or tampered tombstone fails it, and a tombstoned grant
file still on disk warns.

A `remote_principal` grant admits a principal whose home is another RyeOS
node. The principal signs, with its own key, a bounded delegation that lets its
home node act for it (see [Remote operations](remote/remote-operations.md#identity-requirements)).
The target retains only the principal's public key in a node-signed grant
constrained to one canonical `origin_site_id` and concrete scopes. It
separately admits the home node's key as `remote_node` with
`ryeos.attest.request.forwarded-operator`; that key signs and co-signs each
forwarded request. The target verifies both grants, the co-signature and the
delegation, and runs the request as the principal with scopes equal to the
grant intersected with the delegation. A caller header cannot create or remove
authenticated origin, and the principal's private key never leaves its owner.
The target's own local operator remains a separate `local_client` for local
maintenance. Because grants are keyed by fingerprint, every target-side use of
the principal's key is remote and fails without the source-node proof. The
target keeps a ledger of revoked delegation digests (`.ai/node/auth/
delegation_revocations/`); a tombstoned delegation is refused at once.

## Vault X25519

Vault X25519 is separate from the Ed25519 node identity, so node-key
rotation does not brick sealed vault data. Vault rotation is handled by
vault rewrap flows.

## Request authentication

Local CLI requests are signed with the user key and verified against the local
authorized-keys store. Daemon-to-daemon requests are signed with the caller
node key; a request made for a principal also carries that principal's signed
delegation and the source node's forwarding co-signature. The target requires
an exact `remote_principal` allowed-site grant for the principal and an exact
`remote_node` forwarding-attestation grant for the node. In every case,
authenticated origin comes from verified key/grant evidence, never an unsigned
caller claim.

## Browser sessions

A browser has no key. It holds a session cookie that a key-holding principal
minted through a signed `ui.launch.mint` request (the web launcher does this
from the operator's machine). The session is always minted by a principal that
holds a key; a human without a key on the node machine has no other login.

What a session may do is bounded by the node's signed `ui_browser_sessions`
policy (schema 5), never by the cookie flags alone:

- **Authority ceiling.** `session_scopes` is the most any browser session on
  this node may hold. The session's grant is the minter's scopes intersected
  with that list, and the mint request may narrow it further with an optional
  `scopes` list. A minter holding `*` gets exactly the ceiling. The intersection
  is capability-aware (grant patterns, not string prefixes) and fails closed
  where overlap cannot be proven. The mint response returns the effective
  `granted_caps`. Every later admission, event stream, project operation and
  binding compile reads that grant; binding entries the grant cannot reach are
  listed as attenuated at mint.
  The shipped default lists no admin capability: it holds the toolset of
  `directive:ryeos/ops/base` (fetch, verify, doctor, knowledge compose, snapshot
  status/show/log) plus the named docs namespaces `ryeos/core/*`,
  `ryeos/standard/*` and `ryeos/ui/*`, never a knowledge wildcard.
- **Timing.** `session_ttl_secs` (at most 24 hours) ends a session regardless of
  use, `session_idle_secs` ends one that is not used, and `launch_token_ttl_secs`
  (at most 300) bounds the single-use activation link. None has a code default;
  a policy without them is refused.
- **End.** A session ends itself with `POST /ui/api/session/end`
  (`ui.session.end`); the response clears the cookie.
- **List and revoke.** The configured local operator lists live sessions with
  `GET /ui/api/sessions/list` (`ui.sessions.list`) and revokes one with
  `POST /ui/api/sessions/revoke {session_id}` (`ui.sessions.revoke`). A revoked
  session's next request answers `401` with `error_code: browser_session_revoked`.
  Listings carry session ids, principals, grants and ages, never cookie values
  or launch tokens.

Sessions live in daemon memory; a restart ends them all. A node whose policy
generation still carries `ui_browser_sessions` at schema 4 refuses to start
until the section is re-issued at schema 5.
