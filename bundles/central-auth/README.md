# Central Auth Bundle

`central-auth` provides small, reusable web-app auth primitives for RyeOS
projects. It is intentionally separate from the hosted-node bundle:

- hosted-node auth answers: which RyeOS key/node may call this daemon?
- central-auth answers: which human/browser session may access this app?

The consuming web backend owns HTTP details such as cookies, CORS, CSRF,
login pages, and route middleware. This bundle owns auth semantics and
project-local state: principals, roles, capabilities, invites, and sessions.

## Status

Version `0.1.0` is deliberately minimal:

- exact capability strings only;
- no OAuth/OIDC;
- no browser UI;
- no daemon-native `service:` handlers;
- no shared multi-instance storage guarantees.

Apps call the bundle tools through RyeOS execution or trusted local CLI
integration. Browsers should not call RyeOS daemon/tool endpoints directly.

## State layout

Each app uses a realm-scoped state root:

```text
<project_path>/.ai/state/central-auth/realms/<realm_id>/
  policy.json
  principals/
    alice.json
  sessions/
    <sha256(session_token)>.json
  invites/
    <sha256(invite_code)>.json
  audit.jsonl
  lock
```

`realm_id` must match `[a-z0-9][a-z0-9._-]{0,127}`. This avoids collisions
when one project hosts multiple apps or environments.

## Tool API

All tools read JSON from stdin and write JSON to stdout. Do not pass
passphrases, invite codes, or session tokens on argv.

Common fields:

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker"
}
```

`state_root` may be supplied instead of `project_path` for tests or custom
deployments.

### set-policy

Stores the realm policy.

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "policy": {
    "roles": {
      "viewer": { "capabilities": ["tv-tracker.ratings.read"] },
      "analyst": { "capabilities": ["tv-tracker.ratings.read", "tv-tracker.ai.chat"] }
    },
    "allowed_capabilities": [
      "tv-tracker.ratings.read",
      "tv-tracker.ai.chat"
    ]
  }
}
```

### create-principal

Creates or replaces a disabled=false principal. `bootstrap: true` is only
accepted while the realm has no principals.

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "principal_id": "alice",
  "display_name": "Alice",
  "roles": ["analyst"],
  "capabilities": [],
  "passphrase": "secret123",
  "bootstrap": true
}
```

### create-invite

Creates a high-entropy invite code and stores only its hash. The code is
returned once.

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "roles": ["analyst"],
  "capabilities": [],
  "ttl_secs": 86400,
  "max_uses": 1
}
```

### login

Passphrase login:

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "method": "passphrase",
  "principal_id": "alice",
  "passphrase": "secret123",
  "ttl_secs": 28800
}
```

Invite redemption creates a principal and immediately issues a session:

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "method": "invite",
  "invite_code": "returned-once",
  "principal_id": "alice",
  "display_name": "Alice",
  "passphrase": "secret123",
  "ttl_secs": 28800
}
```

### verify-session

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "session_token": "opaque-token",
  "required_capability": "tv-tracker.ai.chat"
}
```

### revoke-session

```json
{
  "project_path": "/abs/project",
  "realm_id": "tv-tracker",
  "session_token": "opaque-token"
}
```

### gc

Removes expired sessions and invites.

## Security defaults

- session tokens and invite codes are 32-byte random values encoded with
  URL-safe base64;
- only SHA-256 token/code hashes are stored;
- passphrases use PBKDF2-HMAC-SHA256 with per-principal salt;
- comparisons use constant-time checks where applicable;
- directories are created with `0700` and files with `0600`;
- mutating operations take a realm lock and use atomic rename writes;
- login failures return generic `invalid_credentials` responses;
- unknown roles/capabilities fail closed.
