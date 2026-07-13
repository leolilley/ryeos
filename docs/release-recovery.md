# Release recovery

The release workflow never overwrites immutable image tags or GitHub release
assets. It automatically resumes states whose identity can be verified:

- an existing image is reused only when its digest has the expected keyless
  workflow signature and its signed index carries the qualified source
  provenance and SBOM;
- an existing bundle archive and checksum are downloaded and verified again;
- an archive-only upload is canonical, so the workflow derives and publishes
  its missing checksum; and
- mutable `latest` tags move only after every immutable output passes again.

Two ambiguous states intentionally stop for operator review.

## Unsigned immutable image tag

An interruption can occur after an image manifest is pushed but before Cosign
records the workflow signature. The next run quarantines that tag. It does not
trust self-asserted registry provenance and will never sign or overwrite the
pre-existing digest automatically.

Record the tag and digest from the workflow error, confirm that the expected
workflow signature is absent, and check whether any other artifact for that
version was published. The safest recovery is to burn the incomplete version
and cut a new signed release tag. If repository policy explicitly permits
reusing the version, a package administrator may delete only the confirmed
incomplete, unsigned container version and rerun the workflow from the same
qualified source. Never delete or replace a signed immutable version.

## Checksum without an archive

A checksum-only GitHub release state is also quarantined because a checksum
does not prove which missing bytes should be restored. Confirm that the archive
asset is absent, remove only the orphan checksum through the release's asset
management controls, and rerun. Do not generate an archive merely to satisfy
an unexplained existing digest.

Archive-only is different: the archive contains officially signed bundle
material, receives both structural and plain-`ryeos init` cryptographic
preflight, and is therefore safe to keep as canonical while deriving its
checksum.
