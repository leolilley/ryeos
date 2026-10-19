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
promotes nothing: only `ryeos-standard` and `ryeos-hosted-workflow` are published release images.

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

## Operator window on a hosted container

Verbs such as `ryeos node external-execution compose` need the node stopped,
but a hosted container's entrypoint always ends by starting the daemon. Set
`RYEOS_OPERATOR_HOLD=1` and redeploy: first-boot initialization runs as usual,
then the entrypoint waits without starting `ryeosd`. Run the stopped-node
verbs over the platform's shell as the node account, for example
`setpriv --reuid=10000 --regid=10000 --clear-groups --no-new-privs ryeos node
external-execution compose ... --app-root /data/app`, then touch
`/data/app/.ai/state/operator-hold-released` to let the node start (only a
touch made after the hold begins counts), or unset the variable and redeploy.
Any value other than unset, `0` or `1` is refused, and the hold is refused
together with `RYEOS_HOST_RUNTIME_BINDING`. While held, the readiness probe
fails by design (the image HEALTHCHECK has a 60 s start period and 3
retries), so on a platform that restarts on a failing health check, disable
or lengthen the check for the hold and re-enable it after release.
