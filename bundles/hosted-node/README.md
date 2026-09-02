<!-- ryeos:signed:2026-09-02T08:16:01Z:6f3ea40fb650b54471156c6af311e8a48814351e8218c5217644ed6c8525db2e:Fir0w3W+wDyVzmaWQIRcow1qQFQzhof+QTMVCziwRSWRIACYJxTPQVCKyhHU0562L7r0WTvwg7olfk7NBq79Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# Hosted-Node Bundle

The `hosted-node` bundle is the core-only profile marker and operator
documentation for a RyeOS node exposed as a remote target. The actual hosted
policy is required node-owned complete-generation state; the publisher-signed
`hosted-workflow` init profile supplies its initial body and initialization
re-signs the selected generation with the current node identity.

The bundle intentionally does not ship a `.ai/node/policies` generation.
Bundles cannot widen node-owned policy authority.

These notes live outside `.ai/` so the bundle does not acquire a runtime
dependency on the `knowledge` kind from `standard`.

## Bundle Boundary

Core owns generic decentralized protocol primitives:

- remote descriptors as trust/discovery pins;
- live node identity verification;
- one-time admission token claim;
- signed remote requests;
- target-node-local `authorized_keys` grants;
- remote doctor/configure/execute orchestration.

The node-owned `hosted` policy owns the operator choices:

- whether one-time-token admission is enabled;
- the bounded admission-token lifetime ceiling;
- whether plain HTTP is permitted for loopback descriptor URLs.

HTTPS for non-loopback URLs, live descriptor identity matching, wildcard-scope
refusal, out-of-band one-time token delivery, and target-node authorized-key
enforcement are protocol invariants, not configurable policy booleans.

`standard` remains workflow/runtime UX. It may use core remote commands to
connect to a hosted node, but it must not contain hosted provider
control-plane logic.

## Authority Model

A hosted provider may provision a node, manage DNS/TLS, display a
descriptor, and deliver one-time admission material. The provider is not
the hot-path execution authority.

Runtime authority remains local to the target RyeOS node:

1. the caller pins the hosted node descriptor;
2. the caller verifies the live `/public-key` identity matches the pin;
3. admission creates a normal authorized-key grant on the target node;
4. future remote requests are signed by the caller node key;
5. the hosted node authorizes each request against its local grants and
   explicit scopes.

Do not accept central bearer tokens, account membership, or provider UI
state as sufficient authority for RyeOS execution requests.

## Operator Runbook

Run the hosted node with a stable public URL and HTTPS termination before
sharing descriptors or admission tokens.

Operator choices are atomically recorded in the current node-signed
`.ai/node/policies/hosted.yaml` generation. The important invariants are:

- public non-loopback admission uses HTTPS;
- descriptors are trust pins, not credentials;
- admission must be explicitly enabled before tokens can be minted or claimed;
- admission tokens are one-time bootstrap material with a policy-bounded TTL;
- wildcard scopes are rejected;
- execution authority is the target node's local authorized-key store.

On the hosted node, export the descriptor and mint one short-lived token:

```bash
ryeos remote-descriptor \
  --name hosted-prod \
  --url https://node.example.com \
  --capabilities "remote-execute,bundle-install" \
  --provider-name "RyeOS Hosted Node" \
  --output ./hosted-prod.remote.yaml

ryeos admission-token \
  --label "caller-node" \
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head" \
  --ttl-secs 600
```

Deliver the descriptor and token to the caller out of band. The token is
shown only once and should be treated as sensitive until claimed.

On the caller node:

```bash
ryeos remote configure --descriptor ./hosted-prod.remote.yaml

ryeos remote admit \
  --remote hosted-prod \
  --token "<one-time-token>" \
  --label "caller-node" \
  --scopes ryeos.execute.service.objects/has \
  --scopes ryeos.execute.service.objects/put \
  --scopes ryeos.execute.service.objects/get \
  --scopes ryeos.execute.service.system/push-head

ryeos remote doctor --remote hosted-prod
```

After admission, the caller talks directly to the hosted node using
signed requests. The hosted provider is not consulted for each execution
authorization decision.

## Multi-Tenancy Boundary

This bundle is for hosted nodes, not shared-daemon tenant isolation.
Prefer isolated hosted nodes or containers per user/org/project boundary
until shared-daemon tenancy is explicitly required and the daemon has
principal-aware storage, vault, quota, and audit enforcement.
