#!/usr/bin/env bash
# Build the same closed Bundle-source view as the repository's disposable-node
# harnesses. Input must already have been populated and publisher-signed.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf 'usage: %s <populated-repository-root> <new-node-source-directory>\n' "$0" >&2
  exit 64
fi

populated_repository=$1
destination=$2
[[ $populated_repository == /* && $destination == /* ]] || {
  printf '%s\n' 'repository and destination coordinates must be absolute' >&2
  exit 64
}
[[ -d $populated_repository/bundles/.ai ]] || {
  printf 'populated repository has no Bundle source root: %s\n' "$populated_repository" >&2
  exit 66
}
[[ -f $populated_repository/scripts/pkg/bundle-sets.sh ]] || {
  printf 'populated repository has no bundle-set owner: %s\n' "$populated_repository" >&2
  exit 66
}
[[ ! -e $destination ]] || {
  printf 'node source destination already exists: %s\n' "$destination" >&2
  exit 73
}

policy=$populated_repository/bundles/standard/.ai/config/test/runtime-qualification.yaml
verifier=$populated_repository/bundles/standard/.ai/tools/test/verify-runtime.yaml
worker=$populated_repository/bundles/codex/.ai/workers/fixture/hosted.yaml
enrollment_worker=$populated_repository/bundles/codex/.ai/workers/fixture/enrollment.yaml
login=$populated_repository/bundles/codex/.ai/worker-executions/fixture/login.yaml
session=$populated_repository/bundles/codex/.ai/worker-executions/fixture/session.yaml
# Do not silently retain predecessor copies in another Bundle: that would
# create competing canonical definitions and preserve the invalid binary edge.
for relative in workers/fixture worker-executions/fixture; do
  [[ ! -e $populated_repository/bundles/standard/.ai/$relative ]] || {
    printf 'stale fixed-fixture definition remains in standard: %s\n' "$relative" >&2
    exit 65
  }
done
for definition in "$policy" "$verifier" "$worker" "$enrollment_worker" "$login" "$session"; do
  [[ -f $definition ]] || {
    printf 'populated qualification definition is missing: %s\n' "$definition" >&2
    exit 66
  }
  /usr/bin/head -n 1 -- "$definition" | /usr/bin/grep -Eq '^# ryeos:signed:' || {
    printf 'qualification definition is not publisher-signed: %s\n' "$definition" >&2
    exit 65
  }
done
if /usr/bin/grep -F -q -- 'FIXTURE_SOURCE_DIGEST' "$worker" "$enrollment_worker"; then
  printf '%s\n' 'populated fixture Worker still contains an unresolved source digest' >&2
  exit 65
fi

# shellcheck source=scripts/pkg/bundle-sets.sh
source "$populated_repository/scripts/pkg/bundle-sets.sh"
/usr/bin/mkdir -p -- "$destination"
/usr/bin/cp -a -- "$populated_repository/bundles/.ai" "$destination/.ai"
while IFS= read -r bundle_name; do
  bundle=$populated_repository/bundles/$bundle_name
  [[ -d $bundle/.ai ]] || {
    printf 'populated full-set Bundle is missing: %s\n' "$bundle" >&2
    exit 66
  }
  /usr/bin/mkdir -p -- "$destination/$bundle_name"
  /usr/bin/cp -a -- "$bundle/.ai" "$destination/$bundle_name/.ai"
  if [[ -f $bundle/PUBLISHER_TRUST.toml ]]; then
    /usr/bin/cp -a -- "$bundle/PUBLISHER_TRUST.toml" \
      "$destination/$bundle_name/PUBLISHER_TRUST.toml"
  fi
done < <(ryeos_bundle_set_names full)

printf '%s\n' \
  "prepared closed full node source for the enforced development profile at $destination" \
  'node initialization, trust admission, project signing and node start were not run'
