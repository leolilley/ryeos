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
