# AUR release metadata

The checked-in `PKGBUILD` files are templates, not publishable package
metadata. They intentionally contain `RELEASE_VERSION` and
checksum placeholders (including `RELEASE_BUNDLE_ARCHIVE_SHA256` for `ryeos`)
instead of versions that can become stale or a checksum bypass.

The `ryeos` package has two immutable inputs:

- the signed-tag source archive, used to build the host binaries; and
- `ryeos-bundles-<version>-x86_64.tar.gz`, produced by the release workflow
  after populating and signing the full bundle set with the official publisher
  key.

The AUR build copies the second artifact verbatim into `/usr/share/ryeos`. It
does not regenerate CAS data, re-sign bundle content, or import packaged trust
documents as authority. `ryeos init` verifies the installed artifact against
the official publisher key compiled into the release binary.

For a release, download both immutable inputs once and run:

```sh
scripts/release/prepare-aur.sh \
  --tag v1.2.3 \
  --archive /path/to/ryeos-v1.2.3.tar.gz \
  --bundle-archive /path/to/ryeos-bundles-1.2.3-x86_64.tar.gz \
  --output /path/to/aur-output \
  --signer-fingerprint FULL_RELEASE_SIGNING_KEY_FINGERPRINT \
  --expected-sha256 EXPECTED_ARCHIVE_SHA256 \
  --expected-bundle-sha256 EXPECTED_BUNDLE_ARCHIVE_SHA256
```

The preparation step validates SemVer, requires an annotated GPG-signed tag
from the configured release key, requires the tag to resolve to the release
checkout, verifies both supplied archive digests, checks the bundle artifact
layout and official publisher metadata, and renders checksum-pinned metadata
for both packages. It also generates `.SRCINFO` with `makepkg` and runs
`namcap` and `shellcheck` when those tools are installed.

`scripts/release/package-bundle-artifact.sh` is the deterministic artifact
packager. It runs production init preflight without `--trust-file` before
archiving and rejects private-key-shaped files or PEM private-key material in
the staged tree. The release workflow invokes it only after populating bundles
with the `RYEOS_PUBLISHER_KEY` BuildKit secret, then uploads the archive and
checksum as immutable GitHub release assets. A production artifact cannot be
materialized from a normal checkout alone: the official private publisher key
is intentionally required and is not stored in this repository.

Publish only the generated package directories. Do not replace either archive
URL with a branch, moving ref, locally populated bundle tree, or unsigned
mirror.
