<!-- ryeos:signed:2026-11-14T07:05:20Z:50a5b3711d5ad77f1855331648ff89fe8455fd5a6afb17f9dfdfc4672836e0da:Yc2LUmfDlkNRF/pkrfRpI7MCzcfUkBHRYXcdx43xO/LeoDdATLbhnZkcFONW7WorWigEWObIze0PBWBiog6bAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Central Auth Bundle

`central-auth` provides small, reusable web-app auth primitives for RyeOS
projects. It is intentionally separate from node admission:

- node admission answers: which RyeOS key/node may call this daemon?
- central-auth answers: which human/browser session may access this app?

The consuming web backend owns HTTP details such as cookies, CORS, CSRF,
login pages, and route middleware. This bundle owns auth semantics and
node-owned realm state: principals, roles, capabilities, invites, and sessions.

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

Realm state belongs to the node, not to a project or a caller. Each realm is one
namespace of central-auth's bundle state home, which the daemon creates and
mounts for the tool (the manifest declares `runtime_authority.state_home` with
`launch_param` namespaces, and every tool requests the namespace named by its
`realm_id` parameter, read-write). The realm directory is the home itself:

```text
<app_root>/.ai/state/bundle-state/central-auth/<realm_id>/
  policy.json
  principals/
    alice.json
  sessions/
    <sha256(session_token)>.json
  invites/
    <sha256(invite_code)>.json
  failures/
    <sha256("principal:<id>" | "address:<client_address>")>.json
  audit.jsonl
  lock
```

The tools read the directory from `RYEOSD_BUNDLE_STATE_HOME`. A run cannot
choose this location: requests carrying `project_path` or `runtime_state_dir` are
refused with `caller_state_location_refused`, and a launch the daemon gave no
state home is refused with `state_home_unavailable` (check the tool's
`state_home` declaration). Under enforced execution isolation the realm's
directory is mounted at `/run/ryeos/bundle-state` and nothing else of the app
root is visible.

`realm_id` must match `[a-z0-9][a-z0-9._-]{0,127}`. This avoids collisions
when one node hosts multiple apps or environments.

### Realm access

Whoever calls a central-auth tool must hold, in its own grant, the use
capability for that realm's namespace. Grant each app backend its own realm only:

```bash
ryeos authorize-key --public-key <ed25519:...> --label app-backend \
  --scopes ryeos.use.bundle-state.central-auth/<realm_id>
```

A backend granted `app-a` is refused with `bundle_state_namespace_denied` when it
names `app-b`. Back up, move or delete a realm with
`ryeos node bundle-state export|import|remove --bundle central-auth`.

## Tool API

All tools read JSON from stdin and write JSON to stdout. Do not pass
passphrases, invite codes, or session tokens on argv.

Common field:

```json
{
  "realm_id": "example-app"
}
```

### set-policy

Stores the realm policy.

```json
{
  "realm_id": "example-app",
  "policy": {
    "roles": {
      "viewer": { "capabilities": ["example-app.ratings.read"] },
      "analyst": { "capabilities": ["example-app.ratings.read", "example-app.ai.chat"] }
    },
    "allowed_capabilities": [
      "example-app.ratings.read",
      "example-app.ai.chat"
    ],
    "login_throttle": { "max_failures": 5, "window_secs": 900, "lockout_secs": 900 }
  }
}
```

`login_throttle` is required. A policy without it is refused with
`policy_missing_login_throttle`, both by `set-policy` and when a realm stored
before it existed is loaded; rerun `set-policy` with the field to upgrade.

### create-principal

Creates or replaces a disabled=false principal. `bootstrap: true` is only
accepted while the realm has no principals.

```json
{
  "realm_id": "example-app",
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
  "realm_id": "example-app",
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
  "realm_id": "example-app",
  "method": "passphrase",
  "principal_id": "alice",
  "passphrase": "secret123",
  "client_address": "203.0.113.7",
  "ttl_secs": 28800
}
```

Failed passphrase logins are counted per `principal_id`, and per
`client_address` when the backend passes one (the bundle cannot see the
network peer itself). `max_failures` failures within `window_secs` lock that
subject for `lockout_secs`: further attempts return
`{"code": "locked", "retry_after_secs": n}` without checking the passphrase.
Unknown principals are counted and locked the same way, and a successful login
clears the principal's count.

Invite redemption creates a principal and immediately issues a session:

```json
{
  "realm_id": "example-app",
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
  "realm_id": "example-app",
  "session_token": "opaque-token",
  "required_capability": "example-app.ai.chat"
}
```

### revoke-session

```json
{
  "realm_id": "example-app",
  "session_token": "opaque-token"
}
```

### gc

Full sweep for operators: removes expired sessions, spent or expired invites,
and stale login-failure records. Nothing needs to schedule it: every login and
verify-session also purges a bounded random sample of expired entries (and the
expired session it touched), so a realm stays bounded on its own.

## Security defaults

- session tokens and invite codes are 32-byte random values encoded with
  URL-safe base64;
- only SHA-256 token/code hashes are stored;
- passphrases use PBKDF2-HMAC-SHA256 with per-principal salt;
- comparisons use constant-time checks, and a failed passphrase login always
  runs one KDF (against a dummy record when the principal is missing,
  disabled, or has no passphrase), so timing does not reveal which principals
  exist;
- failed logins are throttled per principal and client address;
- directories are created with `0700` and files with `0600`;
- mutating operations take a realm lock and use atomic rename writes;
- login failures return generic `invalid_credentials` responses;
- unknown roles/capabilities fail closed.
