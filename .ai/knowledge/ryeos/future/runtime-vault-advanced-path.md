<!-- ryeos:signed:2026-06-08T01:27:53Z:c75efca37bc9bc0280f67d29392c2523e2542c3801ca4a86bd0e256d16b95ca7:8BGUGDEvzOxVFGnX3ih5kNTIjg7SYDNg4OwLjWNPpc+uTF6CJVpQM+phsOCYfJp20QhZQm71OyxA8pfmtWWBAQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
# Runtime vault advanced path

Status: deferred advanced work.

The runtime vault API should keep the existing RyeOS vault as the single secret
primitive while separating operator-env secrets from runtime-created bundle
credentials.

The current near-term implementation intentionally uses a scoped vault API over
the sealed envelope backend. Runtime bundle entries are stored behind an
internal physical key namespace and exposed to bundle code only through logical
refs such as:

```text
vault://bundle/<bundle-id>/<namespace>/<key>
```

## Deferred improvements

1. First-class scoped storage
   - Replace flat sealed-map physical storage with explicit vault record scopes.
   - Keep at least these scope classes:
     - operator environment secrets;
     - runtime bundle credentials.
   - Ensure `required_secrets` can only read operator-env scope by construction.

2. Versioned updates / compare-and-set
   - Add a version or etag to runtime vault entries.
   - Support token rotation flows that update only when the expected version
     still matches.
   - Use this for OAuth refresh-token rotation and concurrent scheduled workers.

3. Audit metadata
   - Record non-secret metadata for put/delete/rotate operations.
   - Include bundle id, namespace, logical key, actor/tool, thread id, and time.
   - Do not log or event-store secret values.

4. Operator diagnostics
   - Add a safe diagnostic command that shows runtime vault refs and metadata,
     not plaintext values.
   - Keep normal `ryeos vault list` focused on operator-env secrets.

5. Ref lifecycle tooling
   - Detect dangling `vault://bundle/...` refs in bundle event chains.
   - Support revocation/deletion during account disconnect flows.
   - Support migration from temporary envelope-encrypted event payloads.

6. Richer key strategy
   - Keep runtime vault logical keys conservative today: `[A-Za-z0-9_]+`.
   - If downstreams need emails/provider subjects in refs, hash them into stable
     opaque keys instead of exposing identifiers directly.
   - Later, consider structured metadata fields separate from the logical key.

## Non-goals

- Do not create a separate "bundle secrets" storage subsystem.
- Do not store OAuth refresh tokens in bundle events.
- Do not expose runtime-created credentials through `required_secrets` env
  injection.
- Do not make `auth: none` public webhooks safe by relying on runtime vault refs;
  route/provider auth remains a separate concern.
