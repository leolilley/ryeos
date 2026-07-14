# AUR release metadata

The checked-in `PKGBUILD` files are templates, not publishable package
metadata. They intentionally contain `RELEASE_VERSION` and
`RELEASE_ARCHIVE_SHA256` instead of a version that can become stale or a
checksum bypass.

For a release, download the immutable release input once and run:

```sh
scripts/release/prepare-aur.sh \
  --tag v1.2.3 \
  --archive /path/to/ryeos-v1.2.3.tar.gz \
  --output /path/to/aur-output \
  --signer-fingerprint FULL_RELEASE_SIGNING_KEY_FINGERPRINT \
  --expected-sha256 EXPECTED_ARCHIVE_SHA256
```

The preparation step validates SemVer, requires an annotated GPG-signed tag
from the configured release key, requires the tag to resolve to the release
checkout, verifies the supplied archive digest, and renders checksum-pinned
metadata for both packages. It also generates `.SRCINFO` with `makepkg` and
runs `namcap` and `shellcheck` when those tools are installed.

Publish only the generated package directories. Do not replace the archive
URL with a branch, moving ref, VCS source, or unsigned mirror.
