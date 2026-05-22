#!/usr/bin/env bash
# Prepare a disposable local AUR package directory for testing ryeOS from
# this checkout. This script deliberately keeps yay/makepkg away from the
# live repository and deploy/aur/ryeos.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/pkg/prepare-local-aur-source.sh [--allow-dirty]

Creates:
  dist/aur/ryeos-<pkgver>+local.<shortsha>[.dirty].tar.gz
  dist/aur/pkgbuild/{PKGBUILD,.SRCINFO,ryeos.install}

Then install only from the generated disposable directory:
  yay -Bi --noconfirm dist/aur/pkgbuild

By default the git worktree must be clean. Use --allow-dirty when testing
uncommitted local package, bundle, or documentation changes.
EOF
}

die() {
    echo "prepare-local-aur-source.sh: $*" >&2
    exit 1
}

allow_dirty=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --allow-dirty)
            allow_dirty=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

cd "$repo_root"

[[ -f "$repo_root/Cargo.lock" ]] || die "Cargo.lock missing at repo root: $repo_root"
[[ -d "$repo_root/bundles/core/.ai" ]] || die "bundles/core/.ai missing; run scripts/populate-bundles.sh first"
[[ -d "$repo_root/bundles/standard/.ai" ]] || die "bundles/standard/.ai missing; run scripts/populate-bundles.sh first"
[[ -f "$repo_root/bundles/core/PUBLISHER_TRUST.toml" ]] || die "bundles/core/PUBLISHER_TRUST.toml missing; run scripts/populate-bundles.sh first"
[[ -f "$repo_root/bundles/standard/PUBLISHER_TRUST.toml" ]] || die "bundles/standard/PUBLISHER_TRUST.toml missing; run scripts/populate-bundles.sh first"
[[ -f "$repo_root/deploy/aur/ryeos/PKGBUILD" ]] || die "production PKGBUILD missing"
[[ -f "$repo_root/deploy/aur/ryeos/ryeos.install" ]] || die "ryeos.install missing"

command -v git >/dev/null 2>&1 || die "git is required"
command -v tar >/dev/null 2>&1 || die "tar is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
command -v makepkg >/dev/null 2>&1 || die "makepkg is required to generate .SRCINFO"

if [[ $allow_dirty -eq 0 ]] && [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    die "worktree is dirty; commit/stash changes or rerun with --allow-dirty"
fi

pkgbuild_template="$repo_root/deploy/aur/ryeos/PKGBUILD"
install_template="$repo_root/deploy/aur/ryeos/ryeos.install"
pkgver="$(sed -nE 's/^pkgver=([A-Za-z0-9._+:~-]+)$/\1/p' "$pkgbuild_template" | head -n 1)"
[[ -n "$pkgver" ]] || die "could not read pkgver from $pkgbuild_template"

shortsha="$(git rev-parse --short=8 HEAD 2>/dev/null || true)"
[[ -n "$shortsha" ]] || shortsha="nogit"
dirty_suffix=""
if [[ -n "$(git status --porcelain --untracked-files=all)" ]]; then
    dirty_suffix=".dirty"
fi

dist_dir="$repo_root/dist"
aur_dir="$dist_dir/aur"
pkgbuild_dir="$aur_dir/pkgbuild"
deploy_pkg_dir="$repo_root/deploy/aur/ryeos"

repo_real="$(realpath -m "$repo_root")"
deploy_real="$(realpath -m "$deploy_pkg_dir")"
aur_real="$(realpath -m "$aur_dir")"
pkgbuild_real="$(realpath -m "$pkgbuild_dir")"

[[ "$pkgbuild_real" != "$repo_real" ]] || die "refusing to use repo root as package build directory"
[[ "$pkgbuild_real" != "$deploy_real" ]] || die "refusing to use deploy/aur/ryeos as package build directory"
case "$pkgbuild_real/" in
    "$repo_real/dist/aur/"*) ;;
    *) die "refusing non-disposable package build directory: $pkgbuild_real" ;;
esac
case "$pkgbuild_real/" in
    "$deploy_real/"*) die "refusing to write under deploy/aur/ryeos" ;;
esac

for path in "$dist_dir" "$aur_dir" "$pkgbuild_dir"; do
    if [[ -L "$path" ]]; then
        die "refusing to use symlinked disposable path: $path"
    fi
done

mkdir -p "$aur_dir"

if [[ -e "$pkgbuild_dir" ]]; then
    case "$(realpath -m "$pkgbuild_dir")/" in
        "$aur_real/"*) rm -rf "$pkgbuild_dir" ;;
        *) die "refusing to remove package build dir outside dist/aur: $pkgbuild_dir" ;;
    esac
fi
mkdir -p "$pkgbuild_dir"

tarball="$aur_dir/ryeos-${pkgver}+local.${shortsha}${dirty_suffix}.tar.gz"
tmp_tarball="${tarball}.tmp"
rm -f "$tmp_tarball" "$tarball"

echo "[prepare-local-aur-source] creating source tarball: ${tarball#$repo_root/}"
find . \
    \( -path './.git' \
    -o -path './.jj' \
    -o -path './target' \
    -o -path './dist' \
    -o -path './.cache' \
    -o -path './.local' \
    -o -path './.ryeos' \) -prune \
    -o -mindepth 1 \( -type f -o -type l \) -print0 \
    | tar -C "$repo_root" -czf "$tmp_tarball" \
        --exclude='./.git' \
        --exclude='./.git/*' \
        --exclude='./.jj' \
        --exclude='./.jj/*' \
        --exclude='./target' \
        --exclude='./target/*' \
        --exclude='./dist' \
        --exclude='./dist/*' \
        --exclude='./.cache' \
        --exclude='./.cache/*' \
        --exclude='./.local' \
        --exclude='./.local/*' \
        --exclude='./.ryeos' \
        --exclude='./.ryeos/*' \
        --transform "s|^\./|ryeos-${pkgver}/|" \
        --null -T -
mv "$tmp_tarball" "$tarball"

tar_listing="$(tar -tzf "$tarball")"

if ! grep -qx "ryeos-${pkgver}/Cargo.lock" <<<"$tar_listing"; then
    die "source tarball is missing Cargo.lock"
fi
if ! grep -q "^ryeos-${pkgver}/bundles/core/\.ai/" <<<"$tar_listing"; then
    die "source tarball is missing bundles/core/.ai"
fi
if ! grep -q "^ryeos-${pkgver}/bundles/standard/\.ai/" <<<"$tar_listing"; then
    die "source tarball is missing bundles/standard/.ai"
fi
if ! grep -qx "ryeos-${pkgver}/bundles/core/PUBLISHER_TRUST.toml" <<<"$tar_listing"; then
    die "source tarball is missing bundles/core/PUBLISHER_TRUST.toml"
fi
if ! grep -qx "ryeos-${pkgver}/bundles/standard/PUBLISHER_TRUST.toml" <<<"$tar_listing"; then
    die "source tarball is missing bundles/standard/PUBLISHER_TRUST.toml"
fi

bad_paths="$(grep -E "^ryeos-${pkgver}/(\.git|\.jj|target|dist|\.cache|\.local|\.ryeos)(/|$)" <<<"$tar_listing" || true)"
if [[ -n "$bad_paths" ]]; then
    printf '%s\n' "$bad_paths" >&2
    die "source tarball contains forbidden local/build state"
fi

sha256="$(sha256sum "$tarball" | awk '{print $1}')"

echo "[prepare-local-aur-source] generating disposable package dir: ${pkgbuild_dir#$repo_root/}"
awk \
    -v pkgver="$pkgver" \
    -v tarball="$tarball" \
    -v sha256="$sha256" \
    '
        /^makedepends=/ {
            print "makedepends=('\''rust'\'' '\''cargo'\'' '\''gcc'\'')"
            next
        }
        /^source=/ {
            print "source=(\"ryeos-" pkgver ".tar.gz::file://" tarball "\")"
            next
        }
        /^sha256sums=/ {
            print "sha256sums=('\''" sha256 "'\'')"
            next
        }
        { print }
    ' "$pkgbuild_template" > "$pkgbuild_dir/PKGBUILD"
cp "$install_template" "$pkgbuild_dir/ryeos.install"

(
    cd "$pkgbuild_dir"
    makepkg --printsrcinfo > .SRCINFO
)

echo
echo "Generated local AUR source:"
echo "  tarball:  ${tarball#$repo_root/}"
echo "  sha256:   $sha256"
echo "  pkgbuild: ${pkgbuild_dir#$repo_root/}"
echo
echo "Next safe install command:"
echo "  yay -Bi --noconfirm dist/aur/pkgbuild"
echo
echo "Then initialize the packaged dev-signed bundles with:"
echo "  ryeos init --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml"
