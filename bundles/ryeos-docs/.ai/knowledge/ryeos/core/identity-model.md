<!-- ryeos:signed:2026-11-13T14:12:01Z:33459ac860c9e6cdbbb48d9a4d78d75ee7a96d2c3ba8d9b81b4c6f9985ebd905:R3XhtokHB+H2GJJOr1xq2uF17sB6MAZo05mPOIjXbRuY6pMNkKtgn3ACYkEjQLHm2tYLuHSqgKLaMKgKIi/tDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
| Signer registry | Verify signed items; node-signed signer records with role, scope and status | `<system>/.ai/node/trust/signers/<fp>.yaml` | `ryeos init` / `ryeos trust pin` |
| User (CLI) key | Sign local CLI HTTP requests | `<user>/.ai/node/operator/signing/private_key.pem` | `ryeos init` |
| Node (daemon) key | Sign outbound daemon requests and authorized-key TOMLs | `<system>/.ai/node/identity/private_key.pem` | `ryeos init` |
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

## Node (daemon) key

The node key normally signs outbound remote requests, signs authorized-key
TOMLs, produces the node public identity document, and signs every signer record
in the registry; it is the implicit trust root. Daemon startup never auto-regenerates it, because doing so
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
`ryeos init` has created the required keys and signer records, but the daemon
never writes signer records.

An opt-in `remote_operator` grant is the narrow exception for an
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
exact `remote_operator` allowed-site grant and an exact `remote_node`
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
