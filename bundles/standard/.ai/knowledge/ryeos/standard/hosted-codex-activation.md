<!-- ryeos:signed:2026-08-23T23:41:10Z:b3cab1f5fa5228716d008c4722e165f0d61528dca880c1298512bba967f37be2:B5iPt7ouZh/MaifNkvPXl+ZeIUGI8bKHK6lYfr2kUkZMtDvWtmltJEbVkNmanPtPSj33DXKRL9P+Oec3Cw22Aw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/standard"
name: "hosted-codex-activation"
title: "Hosted Codex Activation and Acceptance"
description: "Activation, credential ceremony, command routes, and release acceptance for the pinned Codex structured-session workload"
entry_type: runbook
version: "1.0.0"
```

# Hosted Codex Activation and Acceptance

The Codex bundle hosts the pinned Codex App Server using ChatGPT subscription
authentication managed by Codex. It does not route Codex through RyeOS local
inference. The executable and App Server schemas are pinned by activation and
source closure.

This is installed operator knowledge shipped by the standard bundle. It
documents how to activate and accept the optional Codex bundle; it is not a
RyeOS repository-development workflow.

## Activation

1. Publish a set containing `core`, `hosted-node`, and `codex`. Generic
   worker-execution runtime/preparer binaries belong to `core`; bridge/profile
   belong to `codex`.
2. Import and bind the exact realization in
   `.ai/config/codex/activation.yaml`.
3. Configure node-owned persistent-session limits. Bundles never enable node
   worker capacity themselves.
4. Authorize the remote client key as configured operator with exact worker,
   profile, project, and external-content scopes. Wildcards are unnecessary.
5. Open projectless login, call `credential.login.start`, finish the ephemeral
   ceremony, call `credential.account.read`, close it, and confirm the exact
   login epoch/account digest.
6. Start a pinned-project worker, call `session.start`, then `turn.start`,
   `turn.steer`, and `turn.interrupt`. Every turn is bound to the one returned
   remote thread; cross-thread targeting is rejected.
7. Resolve digest-fenced pending approvals. Command approval displays bounded
   command/cwd. File or permission expansion is deny-only without an exact
   admitted reviewable effect.
8. Complete work, validate the frozen candidate, then publish or discard.

The route IDs above are canonical. Inspect `ryeos codex --help` for the current
CLI presentation; every command must still match the signed command and service
contracts.

## Mechanical policy boundary

The signed profile launches Codex with immutable argv containing every
security-critical override; those immutable arguments are the sole
configuration authority. A same-UID process can replace a file in its writable
home, so the mode-0400 compatibility config and bridge drift checks are never
treated as an integrity boundary. If the node enables a generic enforced
isolation backend, RyeOS additionally overlays that file read-only, but hosted
Codex does not require Bubblewrap or another isolation backend. Immutable CLI
overrides fix login, credential store, approval routing,
permission profile, command network, shell environment, and disabled helpers
for process life. Thread start/resume checks supported response fields for
effective approval and sandbox policy.

For pinned Codex 0.147 the granular approval policy is inherited from immutable
CLI configuration. Request-level `approvalPolicy` is intentionally omitted
because the stable App Server rejects that granular field unless the forbidden
`experimentalApi` capability is enabled.

App Server inherits a cleared minimal environment and no RyeOS control FD.
Model commands receive the signed Codex permission profile and cannot access
profile home, boot/capsule metadata, callback authority, DBus/keyring
coordinates, or direct network through that contract. Without an enforced
node-isolation backend this is not an OS-level hostile-workload containment
claim. Stderr drains continuously to a non-retained private sink.

## Release acceptance

Run packaged artifacts in a disposable app/state root, never the developer's
installed node, and prove:

- remote configured-operator acceptance and rejection of another key;
- device login, confirmation, fresh-process continuity, refresh, and restart;
- real turn, pushed events, approval, interruption, and blocked-route cancel;
- daemon restart before/after contact, during approval, and after HEAD contact;
- candidate capture, closure/base validation, publish CAS, discard, and root
  finalization;
- revoke/retry under proved and unproved worker cleanup;
- Codex-absent `standard` and `central-host` publication still stage generic
  core worker-execution binaries; and
- signatures plus clean install/boot inventory resolution.

Environmental inability to run a probe is not passing evidence and does not
justify changing the live local installation.

See `knowledge:ryeos/core/execution/worker-hosted-execution` for the generic
authority, lifecycle, recovery, and publication contract beneath this
integration.
