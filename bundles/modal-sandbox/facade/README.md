# RyeOS Modal lease facade

`ryeos_lease_facade.py` is a small Modal app that turns Modal sandboxes into
RyeOS leases behind eight fixed HTTPS routes. Modal's own client protocol is
gRPC over HTTP/2; the facade lets the RyeOS lifecycle adapter drive leases
over plain HTTP/1.1 JSON instead. It holds no state of its own: every lease
fact lives on the Modal sandbox (its name in the lease app, its tags, and
claim directories inside it).

## Auth

The routes are served by one `modal.asgi_app` (label `ryeos-lease`) with
`requires_proxy_auth=True`. Modal rejects a request before it reaches the
facade unless it carries a workspace proxy auth token in the `Modal-Key` and
`Modal-Secret` headers. The only header the facade reads is `Content-Length`.
It attaches no Modal secret to any function or sandbox and never copies a
provider exception message into a response; provider failures are reported by
exception class name only.

## Routes

All routes are `POST` with a JSON object body, at
`https://<workspace>--ryeos-lease.modal.run<path>`.

| Path | Body | Result |
|---|---|---|
| `/v1/create` | `name`, `lease_set`, `timeout_seconds`, `network` | `created: true` with `name`, `sandbox_id`, `state`, `policy` and `policy_sha256`; or `created: false, name_taken: true` (with `name`) when a lease of that name already runs with exactly the same policy and name tag |
| `/v1/get` | exactly one of `sandbox_id`, `name` | `found`, `sandbox_id`, `state` (`running`/`terminated`), `returncode`, `tagged`, `name_verified`, `tags` |
| `/v1/terminate` | exactly one of `sandbox_id`, `name` | terminates, then polls until a 60 s monotonic-clock deadline and returns the observed terminal state; `already_terminal` when it had ended before the call |
| `/v1/upload` | `sandbox_id`, `content_b64`, `sha256` | writes the one guest file to `/ryeos/guest/entrypoint`, mode 0755 |
| `/v1/run-once` | `sandbox_id`, `stdin_b64` | writes `/ryeos/run.stdin` and releases the guest; `released: true` with the stdin digest |
| `/v1/stage` | `sandbox_id`, `slot`, `offset`, `content_b64`, `chunk_sha256` | appends one chunk to the opaque slot file `/ryeos/stage/<slot>`; `slot_bytes` is the size after the append |
| `/v1/stage-seal` | `sandbox_id`, `slot`, `total_bytes`, `sha256` | digests the slot file inside the sandbox, then seals the slot; `sealed: true`, `already_sealed` on a repeat with the same digest |
| `/v1/list` | `lease_set` | `sandbox_ids` of running leases of that lease set, `untagged_sandbox_ids` of every untagged sandbox in the lease app, `truncated` past 256 entries |

`get` and `terminate` report `found: false` when Modal has no such sandbox.
On every other route a missing sandbox is a provider error.

Every response the facade itself produces, success or refusal, is a JSON
object with:

- `schema`: `ryeos.modal_lease_facade.response.v1`
- `facade_sha256`: SHA-256 of the exact bytes of `ryeos_lease_facade.py` as
  loaded by the running function (once the file is signed, the signed bytes,
  signature line included)
- `facade_app`: `ryeos-lease-facade`
- `guest_image`: the declared guest image identity,
  `modal.debian_slim(python_version=3.12)+mkdir -p /ryeos/guest`
- `modal_client`: the Modal client version in the facade's container
- `operation`: `create`, `get`, `terminate`, `upload`, `stage`, `stage_seal`, `run_once`, `list`, or
  `unknown` for an unknown path
- `ok`: `true` for observations, `false` for refusals and provider failures

Request bodies are bounded per route before they are read: 64 KiB for
`create`, `get`, `terminate`, `stage-seal` and `list`; the encoded 32 MiB
chunk plus 4 KiB for `stage`; the encoded 64 KiB stdin plus 4 KiB
for `run-once`; the encoded 64 MiB upload plus 4 KiB for `upload`. The
received bytes must equal `Content-Length`. Before parsing, the body may hold
at most 32 `{` and `[` bytes in total (a full create needs 7; base64 holds
none), which also bounds JSON nesting depth. The receive buffers are released
before parsing, and the joined body before the handler decodes anything.

Status codes: 200 observation; 400 `invalid_json` / `request_aborted` /
`body_length_mismatch` / `too_many_structures`; 404 `unknown_route`; 405
`method_not_allowed`; 411 `content_length_required` (exactly one decimal
`Content-Length` is required); 413 `body_too_large` (checked from
`Content-Length` before the body is read, and again while reading); 422 a policy refusal (`refusal` names the rule, `detail` is fixed
text); 502 a provider failure (`provider_error`). Responses that do not come
from the facade carry no envelope: Modal's proxy-auth 401s, Modal's own
errors, and function timeouts (300 s).

## Fixed lease policy

- Image: Modal `debian_slim` (Python 3.12) with `/ryeos/guest` created. Not
  selectable. The identity is declarative: the image is built by the
  workspace's Modal image builder version, not pinned by registry digest.
- Sandbox command: `/bin/sh -c 'while [ ! -d /ryeos/run.go ]; do sleep 1;
  done; exec /ryeos/guest/entrypoint < /ryeos/run.stdin'`. The uploaded guest
  therefore becomes the sandbox's main process: the guest's exit ends the
  sandbox, and the sandbox's `returncode` is the guest's exit code (or the
  shell's 126/127 if the guest cannot be executed). Before run-once the
  sandbox only waits.
- Resources: 1 CPU, 1024 MiB memory, no GPU, no secrets, no volumes, no
  inbound ports.
- Lifetime: `timeout_seconds` is an integer from 60 to 86400.
- Name: `name` and `lease_set` match `[a-z0-9][a-z0-9-]{0,62}` in full.
  Sandboxes are created in the Modal app `ryeos-lease-sandboxes`, where a name
  is unique among running sandboxes.
- Policy digest: create computes `policy_sha256`, the SHA-256 of the
  canonical JSON (sorted keys, no spaces) of `policy`: schema
  `ryeos.modal_lease_policy.v2`, `lease_set`, `timeout_seconds`, cpu, memory,
  `block_network`, the sorted allow list, the image identity and the sandbox
  command. A repeated create succeeds only when the running lease's stored
  digest matches; otherwise it is refused as `name_taken_policy_mismatch`
  with both digests.
- Tags: `ryeos_facade=1`, `ryeos_lease_set`, `ryeos_lease_name`,
  `ryeos_policy`, then `ryeos_upload` (`claimed`, then the content sha256)
  and `ryeos_run` (`claimed`, then the stdin sha256).
- Ownership:
  - By name: a sandbox found by name in the lease app was created by the
    facade. `get` and `terminate` by name work even if it never got its
    tags (`tagged: false`), so a lease whose creating call died between
    create and tagging can still be found and ended. `create` with such a
    name is refused as `name_taken_untagged`. A concurrent create may still be
    between creating and tagging it, so the caller first gets it by name and
    retries after a short wait; it terminates by name only if the lease stays
    untagged. A lease of that name whose `ryeos_lease_name` tag names another
    lease is refused as `name_taken_tag_mismatch`. If the lease ends between
    the duplicate-name error and the lookup, create reports a 502
    `NotFoundError` and the caller retries.
  - By id: the sandbox must carry all four lease tags, and while it runs, the
    lease app's name lookup of its `ryeos_lease_name` must return the same
    sandbox (`name_verified: true`). A terminated sandbox can no longer be
    looked up by name, so `get`/`terminate` by id report it with
    `name_verified: false`; its `tagged: true` is then only what its own tags
    assert, since any sandbox in the workspace could carry copied tags.
    Nothing is done to a terminated sandbox beyond reading it.
  - If tagging fails right after create the facade terminates the new sandbox.
- Network, one of:
  - `{"type": "block-network"}`
  - `{"type": "domain-allow-list", "rules": [{"protocol": "https", "host": "<exact DNS name>"}]}`
    with 1 to 4 rules, each host once: lowercase labels of 1 to 63
    characters, at least two labels, an alphabetic first character in the
    last label, at most 253 characters, no trailing dot, port, scheme or
    whitespace. Punycode (`xn--`) labels are ordinary labels. Egress is TLS on
    port 443 only; Modal matches on SNI.
- Upload: once per lease, at most 64 MiB, and the bytes must match `sha256`.
  The claim is recorded both as the `ryeos_upload` tag (out of the guest's
  reach) and as an atomic `mkdir /ryeos/upload.claimed`; either one blocks a
  second upload. After the write and `chmod`, `mkdir /ryeos/upload.done`
  and the tag set to the content digest mark it complete.
- Stage: opaque named slots filled by sequential chunks, independent of the
  upload. A slot name matches `[a-z0-9-]{1,32}`; a lease starts at most 4
  distinct slots (the first chunk of a slot starts it). A chunk is 1 byte to
  32 MiB decoded, must match `chunk_sha256`, and is appended to
  `/ryeos/stage/<slot>` only when the file's current size (stat inside the
  sandbox, 0 when absent) equals `offset`; otherwise `offset_mismatch` with
  the current `slot_bytes`. The chunk is claimed by an atomic `mkdir
  /ryeos/stage-claim/<slot>/c<offset>`; a second claim of the same offset is
  `chunk_already_claimed` and a failed append after its claim leaves the slot
  wedged (terminate and recreate, as for upload). The slot total is at most
  4 GiB + 64 KiB (`slot_too_large`). A sealed slot takes no more chunks
  (`slot_sealed`). The slot constants (4 slots, 32 MiB chunk, 4 GiB + 64 KiB
  slot) are in the policy record (`policy.stage`), hence in `policy_sha256`.
- Stage-seal: `total_bytes` (1 to the slot cap) must equal the file size
  (`seal_size_mismatch`) and `sha256` must equal `sha256sum` of the file
  run inside the sandbox (`seal_digest_mismatch`). Then the tag
  `ryeos_stage_<slot>=<sha256>` is set and `mkdir /ryeos/stage/<slot>.done`
  marks it sealed. Sealing again with the same digest is a no-op success;
  with another digest it is `seal_digest_conflict`. Sealing a slot that
  never received a chunk is `slot_not_started`. A repeated seal also checks
  `total_bytes` against the file size. Sealing takes a claim `mkdir
  /ryeos/stage/<slot>.sealing` (released if the seal is refused); a chunk
  checks for it after taking its chunk claim and again after the append, and
  fails closed with `guest_step_failed` if a seal or the run claim raced in.
  Stage and a new seal are refused with `run_once_already_claimed` once the
  run was claimed. A slot is only started by a chunk at offset 0, checked
  before any claim is made; a refusal after starting a still-empty slot
  removes the start claim.
- Run-once: once per lease, only after a completed upload (the content-digest
  tag and `/ryeos/upload.done` both present) and with every started slot
  sealed (`slot_not_sealed`, naming the slot). The claim is the `ryeos_run`
  tag plus `mkdir /ryeos/run.claimed`. Stdin is the base64 decode of
  `stdin_b64`, at most 64 KiB, written verbatim to `/ryeos/run.stdin`; the
  guest reads it as a regular file and sees end of file after it. `mkdir
  /ryeos/run.go` then releases the sandbox command. The guest's stdout and
  stderr go to the sandbox's logs, which the facade does not read; its
  progress is observed through its own channel to the controller, and its end
  through the lease's terminal state.
- Base64 fields are refused by encoded length before they are decoded.

A failed upload or run after its claim leaves the claim in place; the lease
is then terminated and recreated, not retried.

## Refusals

HTTP-layer refusals: `unknown_route`, `method_not_allowed`,
`content_length_required`, `body_too_large`, `body_length_mismatch`,
`request_aborted`, `too_many_structures`, `invalid_json` (also duplicate keys,
non-UTF-8 and nesting the parser cannot hold). Policy refusals:

| `refusal` | Cause |
|---|---|
| `body_not_object` | body is not a JSON object |
| `unknown_field` | a field outside the route's fixed set (its name is echoed only if it matches `[A-Za-z0-9_]{1,32}`) |
| `missing_field` | a required field is absent |
| `image_refused` | `image` |
| `command_refused` | `command`, `entrypoint` or `args` |
| `gpu_refused` | `gpu` |
| `secrets_refused` | `secrets` |
| `volumes_refused` | `volumes` |
| `cidr_refused` | `cidr_allowlist` |
| `ports_refused` | `encrypted_ports` or `unencrypted_ports` |
| `resources_refused` | `cpu` or `memory` |
| `path_refused` | `path` |
| `invalid_name`, `invalid_lease_set` | name syntax |
| `invalid_sandbox_id` | not `sb-` followed by 1 to 64 alphanumerics |
| `ambiguous_selector` | not exactly one of `sandbox_id`, `name` |
| `timeout_out_of_range` | not an integer in [60, 86400] |
| `invalid_network`, `invalid_network_type` | network is not one of the two shapes |
| `empty_allow_list` | an allow list with no rules |
| `too_many_rules` | more than 4 rules |
| `invalid_rule` | a rule is not an object |
| `port_refused` | a rule names a port |
| `plain_http_refused` | rule protocol `http` |
| `non_https_rule_refused` | any other non-`https` protocol |
| `wildcard_host_refused` | a host containing `*` |
| `invalid_host` | anything else that is not an exact lowercase DNS name (IPv4/IPv6 literals, single labels, schemes, ports, empty or overlong labels, trailing dots, whitespace, uppercase) |
| `duplicate_host` | a host listed twice |
| `name_taken_policy_mismatch` | a lease of that name runs with another policy |
| `name_taken_untagged` | a lease of that name runs without its tags |
| `name_taken_tag_mismatch` | the lease of that name carries another `ryeos_lease_name` |
| `invalid_sha256`, `sha256_mismatch` | upload digest syntax or mismatch |
| `invalid_base64` | `content_b64` or `stdin_b64` is not standard base64 |
| `content_too_large`, `stdin_too_large` | over 64 MiB or 64 KiB |
| `not_facade_sandbox` | reached by id without the lease tags |
| `not_lease_app_sandbox` | reached by id, but the lease app's sandbox of that name is another one |
| `sandbox_terminated` | upload or run on an ended lease |
| `upload_already_claimed`, `run_once_already_claimed` | second upload or run; also a chunk or a new seal after the run was claimed |
| `upload_not_complete` | run-once before a completed upload |
| `invalid_slot`, `invalid_offset`, `invalid_total_bytes` | stage syntax |
| `chunk_too_large`, `chunk_empty`, `chunk_sha256_mismatch` | a chunk over 32 MiB decoded, with no bytes, or not matching `chunk_sha256` |
| `offset_mismatch`, `chunk_already_claimed` | a chunk not at the slot's current size; a second claim of one offset |
| `too_many_slots`, `slot_too_large` | a fifth slot; a slot past 4 GiB + 64 KiB |
| `slot_sealed`, `slot_not_started`, `slot_not_sealed` | chunk after (or during) seal; seal of an unstarted slot; run-once with an unsealed started slot |
| `seal_size_mismatch`, `seal_digest_mismatch`, `seal_digest_conflict` | seal checks |
| `guest_step_failed` | a fixed `chmod`/`mkdir` step failed; or a seal or the run claim landed while a chunk was in flight |

## Live checks before the first deploy is trusted

The tests replace the Modal SDK with an in-memory fake. These provider
behaviours are relied on and are confirmed only by a run against a throwaway
sandbox:

1. `modal.asgi_app(label=..., requires_proxy_auth=True)` serves a plain ASGI
   callable from an image without FastAPI installed, and rejects requests
   without a proxy token.
2. Modal's request body limit for web endpoints admits a 64 MiB upload
   (about 90 MB of JSON); `Content-Length` reaches the app.
3. `__file__` in the container is byte-identical to the deployed file, so
   `facade_sha256` equals the pinned digest.
4. `Sandbox.create` accepts `outbound_domain_allowlist` together with
   `block_network=False`, and the allow list enforces TLS-only SNI matching.
5. Whether `Sandbox.create` accepts `tags=`; if it does, the tags move into
   create and the untagged window disappears.
6. A duplicate running name raises `AlreadyExistsError`; the name is free
   again after termination; `Sandbox.from_name(app_name, name)` finds only
   running sandboxes.
7. `set_tags` replaces the whole tag set and `get_tags` returns it; tag
   values of 64 characters are accepted.
8. `Sandbox.list(app_id=...)` lists the app's running sandboxes, untagged
   ones included.
9. `App.lookup('ryeos-lease-sandboxes', create_if_missing=True)` names an
   app used by nothing but this facade in the workspace (no other deploy may
   use that name).
10. `terminate()` ends the sandbox and `poll()` reports it within 60 s. The
    SDK form `terminate(wait=True)` was not assumed.
11. `exec(...).wait()` returns the exit code; `exec` with raw bytes on stdin (proven by the launcher probe)
    and `cat > path` writes guest files.
12. The sandbox command's exit code is the sandbox's `returncode`, so the
    guest's exit is observed as the lease's terminal state; a sandbox ended
    by its timeout or by `terminate()` reports a non-`None` `poll()` and is
    reported as `terminated`.
13. A missing id makes `Sandbox.from_id` raise an exception whose class is
    named `NotFoundError` (the facade matches on the class name).
14. `Sandbox.from_id` and `get_tags` still work on a terminated sandbox.
15. The sandbox holds no Modal credentials: the guest cannot call
    `set_tags`, `exec` or any other Modal API on itself, so tag claims are out
    of its reach.
16. `Sandbox.open` and the rest of the legacy sandbox file API are refused by
    the server (`UnimplementedError`), so every file write goes through
    `exec`; the user `exec` runs as can `mkdir` under `/ryeos` and write `/ryeos/guest/entrypoint` and `/ryeos/run.stdin`.
17. Egress: DNS resolution stays open (residual channel noted), listed hosts
    are reachable over TLS on 443, non-listed hosts and non-443 ports are
    denied, and `block_network` denies everything.
18. A WebSocket echo through an allow-listed host works end to end.
19. Cleanup: after the probe, `list` for the probe's lease set and the
    untagged list are both empty.
20. One request per facade container at a time (Modal's default input
    concurrency): the 1024 MiB function memory is sized for one upload
    (about 90 MB of JSON, its parsed string and the decoded bytes). Raising
    input concurrency needs this redone.

21. `exec('sh', '-c', 'cat >> "$1"', 'sh', path)` fed raw bytes on
    `stdin` (`write`, `drain`, `write_eof`; binary-safe, proven by the
    launcher probe at about 1.2 MB/s) appends them and
    reports failure through a nonzero exit code, and `exec(...)` exposes
    `stdout.read()`
    (used for `stat -c %s`, `sha256sum` and `ls -1`) before `wait()` returns
    the exit code.
22. A slot can reach 4 GiB + 64 KiB by 32 MiB appends: the sandbox disk holds
    it, the append of a 32 MiB chunk through the `exec` stdin write is accepted as raw stdin (32 MiB sha-verified by the launcher probe), and
    the facade function (1024 MiB) survives one full-size chunk request
    (about 43 MB of JSON, its parsed string and the decoded bytes).
23. Modal's web request body limit admits the 32 MiB chunk frame (about 43
    MB of JSON) and `Content-Length` reaches the app.
24. Tag limits: a sandbox accepts the base lease tags plus `ryeos_upload`,
    `ryeos_run` and four `ryeos_stage_<slot>` tags (up to 41-character keys,
    64-character values); the facade's `_set_tags` replaces the whole set.
25. The seal digest: `sha256sum` of a 4 GiB slot completes inside the exec
    time limits and its output is what the facade compares; a guest cannot
    forge the tag (see 15).
26. `ls -1`, `stat`, `sha256sum`, `rmdir` and `mkdir -p` exist in the
    `debian_slim` image and run as the exec user.
27. `exec(...)` calls in the facade have no explicit timeout, because the
    fake SDK's `exec(*argv)` does not model one. Confirm that Modal's
    `Sandbox.exec(..., timeout=...)` is accepted, then bound the fixed
    steps (`_run`/`_read`), with a generous bound for the 4 GiB `sha256sum`.

## Tests

```text
python3 -I -B -m pytest bundles/modal-sandbox/facade -q -p no:cacheprovider
```
