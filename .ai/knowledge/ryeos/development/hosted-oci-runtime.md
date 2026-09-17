<!-- ryeos:signed:2026-09-17T02:46:25Z:8ba6275739a5cdfd861fbcb7b4e23dc84177a8fbfc716c2abe0d2075b171f11f:3FbGv93iur8ND2WF3eaOftfyGft6Xu8BankhgJJUjSUnuDcdQ99RJDf4okDWqrCiYW22FrSd8LIS5uNAc+nGCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

The protected binding is created for this exact node/app-root/account/lifetime
generation. A daemon restart inside the same `C` may retain it. Replacement of
the OCI init incarnation ends the generation: the administrator must prove the
old `C` and its descendants dead before the volume is reused, create a fresh
delegation, and publish a fresh protected binding. RyeOS must refuse a stale
binding rather than reinterpret it.

## Deployment sequence

1. The administrator selects an immutable image digest, signed isolation
   policy, exact non-root account, private persistent app root, and required
   namespace/network/filesystem policy.
2. The OCI supervisor creates `C` and retains its lifecycle authority. The
   installed Lillux adapter observes the exact init and `C`, refuses the host
   cgroup root or a shared direct membership, creates and delegates strict
   child `R`, and retains the durable intent needed for interrupted recovery.
3. Node initialization completes as the selected account. The administrator
   has the installed adapter write the protected binding inside the paused
   container namespace, and the
   root-only `ryeosd host-runtime` bootstrap opens that file, converts it to an
   inherited read-only descriptor, places the controller, drops permanently to
   UID/GID 10001, and execs the ordinary daemon.
4. Readiness is advertised only after the exact lifetime generation passes
   Lillux's disposable placement/freeze/kill/recovery probe.
5. Stop and replacement remain supervisor operations. Persistent state is not
   eligible for reassignment until host-side death and descendant cleanup are
   observed.

No generic hosted image globally requires this authority. A deployment opts
into the hard-contained profile only when its host adapter supplies it.

### OCI hook installation contract

The release target `contained-oci-hook-artifact` contains the host-installed
`ryeos-lillux-oci-hook`. Its executable and every parent directory must remain
root-owned and non-writable by the controller account. The OCI runtime invokes
the same pinned executable as `prestart` and `poststop`, supplies standard OCI
state on standard input, applies a finite hook timeout, and treats either hook
failure as a failed lifecycle operation. The host creates
`/var/lib/ryeos/contained-oci` as `0700 root:root`; it is not mounted into the
worker.

After installing the immutable hook artifact at a root-owned executable path,
the administrator runs `ryeos-lillux-oci-hook install-host-state` once. Each
OCI bundle then uses the exact installed path for both hooks, with argument
`prestart` or `poststop` respectively. Recovery after an interrupted runtime
callback uses `ryeos-lillux-oci-hook recover <container-id>`; it performs the
same death and empty-tree proof and cannot force release.

Prestart serializes host-state mutation, keys the reuse lease by the pinned
`/data/app` directory identity rather than the container ID, and records a
recoverable exact `intent` before kernel mutation. A fixed setup journal is
written before either the container index or volume lease, so a crash between
those secondary writes blocks all new setup until exact recovery converges
them. It records `prepared` before entering the container mount namespace,
publishes the protected binding, then records `active`. Poststop
requires exact init death and empty `C`/`R`, retires `R`, and records
`released`. Any interrupted or ambiguous phase remains non-reusable. An
operator must rerun poststop recovery against its retained record; deleting or
editing the record is not recovery evidence.

## Evidence classes

Structural smoke proves image wiring and closed inputs. Source-contract tests
prove parsers, refusal paths, and state-machine behavior. Installed
qualification alone may claim the selected host actually supplied containment.
The signed source verifier checks bounded record structure; it neither contacts
nor provisions a host. It accepts only structural-smoke record shape. The
repository test runner, rather than self-asserted evidence, establishes the
source-contract result. Source configuration leaves installed attestation
disabled. Only an administrator-signed replacement naming an exact attestor and
a verifier that authenticates its canonical payload may enable an installed
claim; an evidence checklist alone can never promote itself.

Installed evidence must identify the exact source and image, signed policy,
node, account, protected binding, host boot, OCI init birth identity, physical
scope identity, hook digest, and lifetime generation. It must cover missing/read-only/
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
