# Hosted-Node Bundle

The `hosted-node` bundle packages core-only operator policy for a RyeOS
node that is exposed as a public remote target. It is intentionally
outside `standard`: a hosted node does not need the workflow/runtime
bundle just to participate in decentralized remote admission.

Runtime bundle contents currently depend only on the core `node` kind:

- `node:hosted/policy`

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

`hosted-node` owns hosted/operator defaults:

- public HTTPS deployment expectations;
- stricter remote exposure policy;
- admission and grant-change audit expectations;
- provider boundary documentation and runbooks outside the runtime item
  graph.

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

Operator defaults are recorded in `node:hosted/policy`. The
important invariants are:

- public non-loopback admission uses HTTPS;
- descriptors are trust pins, not credentials;
- admission tokens are one-time bootstrap material;
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
  --scopes "ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.objects/get,ryeos.execute.service.system/push-head"

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
