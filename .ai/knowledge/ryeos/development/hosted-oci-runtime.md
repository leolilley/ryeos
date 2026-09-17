<!-- ryeos:signed:2026-09-17T00:51:29Z:c9b7bf489b54a0328c692789c493a485f2407f2e0105a9a19496b746c363e8f3:5fVGOa96bKbic4l/YDC3g7/yTfghC9S2GybgfbrdJxfzKpChl+VOoKpLwWGaLVgsTWfbqZULToIQd/c7GlJPDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: hosted-oci-runtime
title: Hard-contained OCI hosted runtime
description: Authority, lifecycle, deployment, and qualification contract for hosted OCI workers
entry_type: reference
version: "1.0.0"
```

# Hard-contained OCI hosted runtime

## Ownership

This is a general RyeOS development capability. Repository-root `.ai/` owns
the authored capability and qualification definitions. Lillux owns Linux
process identity, cgroup-v2 interpretation, controller placement, freeze,
termination, and recovery mechanics. RyeOS binds an opaque Lillux scope
configuration to one exact app root, node signing identity, and non-root
controller account. An external OCI administrator owns container creation,
the lifecycle boundary, delegation, stop/removal, and volume-reuse decisions.

Railway externally fenced workers are a separate capability because Railway
does not currently supply this nested writable process-scope authority. Local
inference resource selection and GPU occupancy are also separate: device
visibility is not process containment.

The runtime products stay distinct. `ryeos-hosted-workflow` is the broadly
usable controller/workflow node and makes no hard-containment claim. Railway's
restricted worker node belongs exclusively to the external-container-workers
lane and uses an externally fenced service lifecycle. `ryeos-contained-workflow`
is this capability's fail-closed packaging: it contains the fixed account,
Lillux and RyeOS binaries, exact hosted-workflow bundle inventory, and signed
`contained-workflow` profile, but starts only from the fixed protected binding
prepared by an installed administrator adapter.

## Required topology

Let `C` be the actual host-observed cgroup for one OCI init-process
incarnation, and `R` the Lillux controller root. `R` must be a strict physical
descendant of `C` in the same cgroup-v2 mount and host boot. Every controller
and worker scope is below `R`. A bind-mounted path string, container ID, or
provider deployment name is not proof of this relationship.

The protected binding is created for this exact node/app-root/account/provider
generation. A daemon restart inside the same `C` may retain it. Replacement of
the OCI init incarnation ends the generation: the administrator must prove the
old `C` and its descendants dead before the volume is reused, create a fresh
delegation, and publish a fresh protected binding. RyeOS must refuse a stale
binding rather than reinterpret it.

## Deployment sequence

1. The administrator selects an immutable image digest, signed isolation
   policy, exact non-root account, private persistent app root, and required
   namespace/network/filesystem policy.
2. The OCI supervisor creates `C`, delegates a strict child `R`, and retains
   host-side lifecycle authority for `C`. Lillux validates physical identity,
   ownership, writability, controller placement, and child lifecycle controls.
3. Node initialization completes as the selected account. The administrator
   publishes the protected binding through the existing inherited read-only
   descriptor path and enters the daemon through `ryeosd host-runtime`.
4. Readiness is advertised only after the exact provider generation passes
   Lillux's disposable placement/freeze/kill/recovery probe.
5. Stop and replacement remain supervisor operations. Persistent state is not
   eligible for reassignment until host-side death and descendant cleanup are
   observed.

No generic hosted image globally requires this authority. A deployment opts
into the hard-contained profile only when its host adapter supplies it.

## Evidence classes

Structural smoke proves image wiring and closed inputs. Source-contract tests
prove parsers, refusal paths, and state-machine behavior. Installed
qualification alone may claim the selected host actually supplied containment.
The signed verifier checks a bounded evidence record; it neither contacts nor
provisions a host and cannot promote structural evidence into installed proof.

Installed evidence must identify the exact source and image, signed policy,
node, account, protected binding, host boot, OCI init birth identity, physical
scope identity, and provider generation. It must cover missing/read-only/
wrong-owner/replaced delegation, wrong node/app-root/account, stale binding,
real scoped execution, detached and nested descendants, writer-excluding
freeze, cancellation, daemon crash, OCI restart/replacement, cleanup before
reuse, unrelated-process safety, and the existing authenticated Codex
candidate-return workflow.

## Historical implementation disposition

The old `feature/container-hosted-workers` worktree is review history and must
remain untouched. Its protected binding and inherited descriptor transport
already landed independently. Its typed controller-account parsing is useful
API material but is not needed to interpret policy in RyeOS. Its daemon-owned
external provisioning command, entrypoint bootstrap, image enablement, and
Docker smoke topology are rejected: they coupled deployment authority to the
daemon and did not prove that the bind-mounted delegation and the OCI
lifecycle scope were the same physical hierarchy. The structural smoke may be
used as a negative wiring fixture only; it cannot be restored as an installed
gate. Native host-service cgroup provisioning remains valid for its native
owner and is not evidence for an OCI host.

## Exact remaining installed prerequisites

Source publication does not complete installed qualification. The selected
host still needs an administrator-owned adapter that can observe the OCI init
from the host side, retain exact birth and cgroup identities, create and
delegate `R` strictly below that observed `C`, launch the existing protected
RyeOS controller binding, stop the container, and attest descendant death
before persistent-volume release. The adapter and its signed isolation policy
must be reviewed and installed on a named disposable host.

That host must expose cgroup v2 delegation with writable child creation and
placement plus child `cgroup.freeze`, `cgroup.kill`, `cgroup.events`, and
`cgroup.procs`; a fixed non-root account and private app-root ownership; the
required PID, mount, user, network, and filesystem isolation; and immutable
image/source coordinates. Qualification then runs every required capability
and refusal in the signed config, records host-side identities before and
after daemon restart, container restart, and image replacement, and exercises
the existing authenticated Codex workflow. Until those observations exist,
the honest result is `source_contract`, not `installed_qualification`.
