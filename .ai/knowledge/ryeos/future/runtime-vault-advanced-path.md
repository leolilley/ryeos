<!-- ryeos:signed:2026-07-14T10:12:37Z:d7f7b69133dd01af735976c2a3e4a03b2a8b32e5392133156f2e9e60042145e7:221MagRK+KR4rfiPCVzlcL/Rd0vNkV5apoq93TK4wWeHRcnV6HTtgpep4DSoCaiRydVafJ18ZK58vtT1ZuCLAw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
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

## Current bounded boundary

The sealed backend remains one global encrypted map. Every operator-vault or
runtime-vault operation opens and validates the complete map, and every
mutation seals the complete map again. Current storage invariants cap it at:

- 1,024 entries;
- 256 bytes per physical key;
- 256 KiB per value;
- 4 MiB serialized/decrypted plaintext; and
- 6 MiB for the sealed envelope on disk.

Runtime-vault logical namespace/key segments remain `[A-Za-z0-9_]+` and at
most 64 characters. Runtime list is lexical cursor pagination with a default
page of 64 keys, a maximum page of 128, and a 64 KiB serialized-response cap.
That pagination bounds callback response materialization only: selecting a
page still requires a whole-envelope decrypt and validation. It must not be
described as narrow per-bundle or per-namespace storage I/O.

## Deferred improvements

1. First-class scoped storage
   - Replace flat sealed-map physical storage with explicit vault record scopes
     or sharded per-scope envelopes.
   - Keep at least these scope classes:
     - operator environment secrets;
     - runtime bundle credentials.
   - Ensure `required_secrets` can only read operator-env scope by construction.
   - Preserve the logical `vault://bundle/...` API while making get/list/update
     touch only the addressed bundle/namespace shard.
   - Add bounded per-scope indexes so cursor pagination also bounds storage I/O,
     rather than only the returned page.

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
