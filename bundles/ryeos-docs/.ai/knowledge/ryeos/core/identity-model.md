<!-- ryeos:signed:2026-11-13T16:31:57Z:a9300119335c7f99c17c778dd36312025636fc51ea3955654898cf6f4300c285:woloR/Wyvb95CEvnW3pFYJWx6nS2AdtLL8GWcDjQam5gWRAzJeiFXFQikmDeCLylYcml51zFyDO8kLX0N4CdDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [identity, trust, keys, security, fundamentals]
version: "2.1.1"
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

ryeOS has four distinct identity/trust layers:

| Layer | Purpose | Storage | Created by |
|---|---|---|---|
| Publisher trust | Verify signed bundle items | `<user>/.ai/config/keys/trusted/<fp>.toml` | `ryeos init` / `ryeos trust pin` |
| User (CLI) key | Sign local CLI HTTP requests | `<user>/.ai/config/keys/signing/private_key.pem` | `ryeos init` |
| Node (daemon) key | Sign outbound daemon requests and authorized-key TOMLs | `<system>/.ai/node/identity/private_key.pem` | `ryeos init` |
| Vault X25519 | Seal/unseal vault secrets | `<system>/.ai/node/vault/private_key.pem` | `ryeos init`, repaired if missing after init |

## Publisher trust

`ryeos init` pins the compiled official publisher key and any
`--trust-file` entries. Additional publishers are pinned with
`ryeos trust pin`. Official publisher rotation requires a coordinated
binary release because the public key bytes are compiled into
`ryeos-node` and must hash to the compiled fingerprint.

## User (CLI) key

The user key is the operator identity for signing local daemon requests.
The CLI resolves it from `RYEOS_CLI_KEY_PATH` when set, otherwise from
`<user>/.ai/config/keys/signing/private_key.pem`. It must not fall back
to the node key.

## Node (daemon) key

The node key normally signs outbound remote requests, signs authorized-key
TOMLs, produces the node public identity document, and anchors the node
self-trust doc. Daemon startup never auto-regenerates it, because doing so
would invalidate operator trust and remote authorizations.

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
`ryeos init` has created the required keys and trust docs, but the daemon
never writes user trust.

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

An opt-in `remote_principal` grant is the narrow exception for an
operator-owned workflow forwarded by another RyeOS node. The source request is
still signed by that source node's configured operator key, while the target
retains only its public key in a node-signed grant constrained to one canonical
`origin_site_id` and concrete scopes. The
target separately admits the source node key as `remote_node` with
`ryeos.attest.request.forwarded-operator`; it must co-sign the exact primary
request. Only the two verified grants plus that co-signature create
authenticated remote origin. A caller header cannot create or remove it.
The source operator private key never moves to the target. The target's own
local operator remains a separate `local_client` for local maintenance.
Because grants are keyed by fingerprint, every target-side use of the source
key is remote and fails without the source-node proof.

## Vault X25519

Vault X25519 is separate from the Ed25519 node identity, so node-key
rotation does not brick sealed vault data. Vault rotation is handled by
vault rewrap flows.

## Request authentication

Local CLI requests are signed with the user key and verified against the local
authorized-keys store. Ordinary daemon-to-daemon requests are signed with the
caller node key. Explicit configured-operator forwarding is instead signed by
that operator key and co-signed by the source node key. The target requires an
exact `remote_principal` allowed-site grant and an exact `remote_node`
forwarding-attestation grant. In both cases, authenticated origin comes from
verified key/grant evidence, never an unsigned caller claim.

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
