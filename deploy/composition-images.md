# Composition images

Each composition image carries one exact bundle set and starts with the init
profile of the same name. They exist so a deployment can run only the
capabilities it needs: the substrate alone, substrate plus scheduling, graph
workflows, knowledge, or the full standard set, plus the specialised
distributions named in the README table.

## How candidates are produced

`docker-bake.composition.hcl` defines one bake target per image, each selecting
its final stage of `Dockerfile.release`. The manual
`qualify-composition-images` workflow runs that bake, pushes the images with
provenance and SBOM attestations, and prints each image's digest reference.
Candidates are signed with the development publisher key, are tagged only with
a candidate tag, and are deployed by digest. Building or pushing a candidate
promotes nothing: only `ryeos-standard`, `ryeos-central-host`, `ryeos-local-inference` and
`ryeos-hosted-workflow` are published release images.

The bundle sets are defined in `scripts/pkg/bundle-sets.sh`; the init profiles
live in `bundles/.ai/node/init/profiles/`.

## Sets without an image

- `release-authority` and `standard-central-auth` are installable sets whose
  profiles are admitted, but no image has been decided for them yet.
- `full` is the developer set for local builds and is not meant to be
  deployed from an image. The `development` profile maps onto it.

CI checks that these three are the only sets without an image, so a new set
must either gain an image or be named here.

## Host-class expectations

What each profile requires of each kind of host is data in the profile files.
The offline matrix test
`crates/daemon/ryeos-node/tests/composition_host_class_matrix.rs` reads the
shipped profiles and judges them against synthetic host evidence; it is the
authority for host-class expectations, and this document deliberately repeats
none of them.

## Root drop on a hosted container

The entrypoint hands `/data` to the node account and re-executes itself as
that account with cleared groups, cleared inheritable capabilities and
no_new_privs. It also clears the capability bounding set when the host
allows it. Some hosted sandboxes refuse bounding-set changes; there the
entrypoint logs `host refuses capability bounding-set changes` and continues
with the other guarantees, which already leave the daemon with no
capabilities and no way to regain them.

The app root is handed over as an owner-private directory, mode 0700 with
no set-id bits, because the node refuses any other mode when it pins it.

## Signed offline operations on a hosted container

Changes that need the node stopped (policy replacement and `policy-apply`,
`external-execution compose` and its snapshot forms, capacity reissue and
reanchor, `allow-restart`, the resets) are signed requests, applied by `ryeosd`
itself before any other startup check can refuse. No shell is involved.

- Sign against the node's facts: `ryeos node offline-operation facts --json`
  (stopped node on this machine), the running node's `GET
  /offline-operations/facts`, or the facts every startup prints to the
  platform log. `ryeos node offline-operation sign` builds the request; it is
  valid for one hour unless `--lifetime-minutes` says otherwise, up to 24
  hours. A remote operator signs with a `remote_operator` grant that names the
  exact scope `ryeos.offline.<operation>`; wildcard grants never satisfy an
  offline scope, and an authority-bearing operation is delegable only when the
  node's signed profile says so.
- Running node: `ryeos node offline-operation submit --url <node> --file
  request.json` verifies the request as the startup applier will and queues it.
  The node keeps running unless `--stop` is given, which makes a plain operator
  stop; the supervisor or hosting platform must start the next incarnation
  (Render restarts every exit; Railway needs the restart policy Always).
- Node that cannot start: set `RYEOS_OFFLINE_OPERATION_REQUEST` (an absolute
  file path, or base64 of at most 64 KiB) and `RYEOS_OFFLINE_OPERATION_INPUTS`
  (a directory). The entrypoint copies them into the inbox as root, verifies
  nothing, and never blocks the start on a copy failure.
- `ryeos node offline-operation list` shows the ledger tip, the queue, the
  newest node-signed receipts and the refusals.

The platform is trusted: whoever can write the node's volume, or run commands
as root before it starts (a platform API key, Render's command override), can
sign as the local operator, because the operator key sits on that volume. The
ledger's chain tip makes a later rewrite of history detectable against an
earlier tip; it does not prevent one.

`RYEOS_OPERATOR_HOLD=1` remains as break-glass for a node whose operator has a
shell. No procedure depends on it, and a platform that fails the health check
during the hold may replace the instance. Point hosted health checks at
`/_ryeos/health` (liveness, 200 while the node boots and exits cleanly), not
`/_ryeos/ready`; the image `HEALTHCHECK` stays on `/_ryeos/ready`.
