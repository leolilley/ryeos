#!/usr/bin/env bash
# Entrypoint for ryeosd-full container.
#
# Ordinary mode seeds an absent installed generation exactly once. Subsequent
# boots never reconcile installed bundles from image contents: native bundle
# selection and stopped-node activation own those changes. External
# host-runtime mode consumes an administrator-created
# protected binding and does not run image-owned initialization.
#
# App root (/data/app) lives on the persistent /data volume, so operator
# trust, signing keys, node identity, and runtime state survive redeploys.
# A profile whose floor requires storage:durable_root measures this: when
# /data is the container's own layer rather than a volume, init refuses
# before any key or identity is created, and each platform restart repeats
# that refusal at the platform's own backoff.
#
# If init fails the container exits immediately — never start ryeosd against
# an unverified state.
#
# Run the container under an always-restart policy (Railway: Restart Policy
# "Always"; Docker: --restart always; Render restarts every exited service).
# The node exits 75 at each service-window rollover and expects to be started
# again, so an on-failure policy with a retry cap would eventually leave it
# down. A node that stopped itself on a refused fact records a restart
# inhibition and refuses every later start until an operator runs
# `ryeos node allow-restart`; this entrypoint waits a bounded time before such
# a start so an always-restart host retries that refusal slowly rather than in
# a tight loop, and so an operator can open a shell in the container and clear
# it while no daemon holds the node's state lock.
#
# Changes to a stopped node are signed offline operations, applied by ryeosd
# itself at the top of its startup (policy replacement, policy-apply,
# external-execution compose, capacity reissue and reanchor, allow-restart and
# the resets). A running node queues one through its authenticated route; a
# node that cannot start receives it from the platform: this script, still
# root and before anything else, copies the request file named by
# RYEOS_OFFLINE_OPERATION_REQUEST (an absolute path, preferred, or a base64
# value of at most 64 KiB) and the input files in the directory named by
# RYEOS_OFFLINE_OPERATION_INPUTS into the node's inbox. The copy verifies and
# decides nothing: it takes regular files only, names each by the digest of its
# bytes, never replaces one, copies a given file once, and a failure is logged
# and does not block the start. The environment never changes node state: the
# retired variables RYEOS_RESET_NODE_POLICY_GENERATION and
# RYEOS_EXECUTION_HISTORY_SCHEMA_CUT refuse the start when set.
#
# RYEOS_OPERATOR_HOLD=1 stays as break-glass for a host that offers a shell
# only while the container runs, and no procedure depends on it: after
# first-boot initialization, instead of starting ryeosd, the entrypoint waits
# with no daemon holding the state lock, so stopped-node `ryeos` commands can
# run in the platform shell. A platform whose health check fails during the
# hold may replace the instance. Touch <app root>/.ai/state/operator-hold-released
# to continue to the normal start (the marker is removed first), or unset the
# variable and redeploy. Unset, "0" and "1" are the only accepted values;
# anything else refuses to start. The hold is not supported with
# RYEOS_HOST_RUNTIME_BINDING and refuses there.

#
# Every ryeos command here runs as the image's node account, ryeos
# (uid 10000, gid 10000), never as root: lillux refuses root as the recorded
# actor of `ryeos node allow-restart` and `ryeos node capacity reissue`, so a
# node run as root could never have its inhibition cleared in place. tini
# starts this script as root; the first thing it does is hand /data (a
# platform volume, mounted root-owned) to the node account and re-execute
# itself as that account with no capabilities and no_new_privs set. An
# operator shell that clears an inhibition must run as the same account:
# `setpriv --reuid=10000 --regid=10000 --clear-groups --no-new-privs
# ryeos node allow-restart --app-root /data/app`.
#
# The one root step is the external host-runtime route: `ryeosd host-runtime`
# is an administrator bootstrap that refuses to run as anything but root. As
# root it opens the binding through a root-owned directory chain, validates
# the administrator-prepared process-scope delegation, installs the node's
# resource ceilings, places itself in the delegated controller cgroup, and
# then drops to the binding's own account before it executes the daemon. The
# daemon on that route therefore never runs as root either; this script only
# hands that bootstrap the root it needs and runs nothing else as root there.

set -euo pipefail

RESTART_INHIBITED_HOLD_SECONDS=30
OPERATOR_HOLD_POLL_SECONDS=5
NODE_UID=10000
NODE_GID=10000
OFFLINE_MAX_REQUEST_BYTES=65536
OFFLINE_MAX_INPUT_BYTES=262144
OFFLINE_MAX_INPUTS=16

# Print the first entry under `app_root`, the app root itself excluded, whose
# device number differs from `device`: the root of a mount nested inside the
# app root. Prints nothing when every entry is on the app root's filesystem.
first_entry_on_other_device() {
  local app_root="$1"
  local device="$2"
  local record
  while IFS= read -r -d '' record; do
    if [[ "${record%% *}" != "$device" ]]; then
      printf '%s\n' "${record#* }"
      return 0
    fi
  done < <(find "$app_root" -xdev -mindepth 1 -printf '%D %p\0')
}

# Hand every entry under the app root that the node account does not already
# own to it, as root, once per boot. The node account controls this tree, so
# the walk trusts nothing in it: it stays on the app root's filesystem (a
# nested mount is not the node's to take), never follows a symlink (a link is
# re-owned as itself, never its target), and refuses a multiply linked file
# the node account does not own instead of re-owning it, because such a file
# may be a hard link the node account planted to a file outside its tree. A
# directory cannot be hard linked, so its link count is not consulted. Every
# entry is checked before any is changed, so a refusal changes nothing. No
# node-account process runs during this walk, so the tree it checked is the
# tree it changes. A walk that stays on one filesystem still reaches the root
# directory of a mount nested under the app root, so any entry on another
# device than the app root itself is refused rather than re-owned. The second
# argument overrides the app root's device number and exists only so this
# comparison can be exercised without creating a mount.
hand_app_root_to_node_account() {
  local app_root="$1"
  local app_root_device="${2:-}"
  local entry
  local links

  if [[ -L "$app_root" || ! -d "$app_root" ]]; then
    echo "[entrypoint] refusing to start: $app_root must be a directory, not a symlink or other file" >&2
    return 1
  fi
  if [[ -z "$app_root_device" ]]; then
    app_root_device="$(stat -c %d -- "$app_root")"
  fi
  entry="$(first_entry_on_other_device "$app_root" "$app_root_device")"
  if [[ -n "$entry" ]]; then
    echo "[entrypoint] refusing to start: $entry is on another filesystem than the app root $app_root; a nested mount is never re-owned. An administrator must unmount it or move it outside the app root before the node can start." >&2
    return 1
  fi
  while IFS= read -r -d '' entry; do
    links="$(stat -c %h -- "$entry")"
    echo "[entrypoint] refusing to start: $entry is not owned by the node account $NODE_UID:$NODE_GID and has $links hard links; a multiply linked file is never re-owned. An administrator must remove the extra link or set its owner before the node can start." >&2
    return 1
  done < <(find "$app_root" -xdev ! -type d \( ! -user "$NODE_UID" -o ! -group "$NODE_GID" \) -links +1 -print0)
  find "$app_root" -xdev \( ! -user "$NODE_UID" -o ! -group "$NODE_GID" \) \
    \( -type d -o -links 1 \) -exec chown -h "$NODE_UID:$NODE_GID" -- {} +
  # The node pins directories under the app root as owner-private and refuses
  # any mode other than exactly 0700. A directory made under the default
  # umask is 0755, and one made under a platform volume that sets the group
  # bit is inherited by every directory created beneath it, so the five-digit
  # mode clears the set-id bits as well as the group and other access.
  chmod 00700 -- "$app_root"
  if [[ "$(stat -c %a -- "$app_root")" != 700 ]]; then
    echo "[entrypoint] refusing to start: $app_root could not be set to owner-private mode 0700" >&2
    return 1
  fi
}

# Log the owner, mode and name of each existing path given, so a refusal that
# turns on a directory mode is explained by the service log alone. A missing
# path is skipped, never an error.
log_path_modes() {
  local path
  for path in "$@"; do
    if [[ -e "$path" ]]; then
      echo "[entrypoint] $(stat -c '%a %U:%G %n' -- "$path" 2>&1 || true)"
    fi
  done
}

# Copy one regular file into the offline-operation inbox under the digest of
# its bytes, once. A link, a special file, a file past `max_bytes` and a file
# already delivered are skipped; the inbox never has a file replaced. A marker
# records each digest delivered so that a platform that passes the same file
# on every boot copies it once. Verifies nothing: the daemon does that.
copy_into_offline_inbox() {
  local source_file="$1"
  local suffix="$2"
  local max_bytes="$3"
  local offline="$4"
  local size digest incoming

  if [[ -L "$source_file" || ! -f "$source_file" ]]; then
    echo "[entrypoint] offline operation: $source_file is not a regular file; not delivered" >&2
    return 1
  fi
  size="$(stat -c %s -- "$source_file")" || return 1
  if [[ "$size" -gt "$max_bytes" ]]; then
    echo "[entrypoint] offline operation: $source_file is $size bytes, over the $max_bytes limit; not delivered" >&2
    return 1
  fi
  digest="$(sha256sum -- "$source_file" | cut -d' ' -f1)" || return 1
  if [[ -e "$offline/delivered/$digest.$suffix" || -e "$offline/inbox/$digest.$suffix" ]]; then
    return 0
  fi
  incoming="$offline/inbox/.incoming-$$-$digest"
  cp -- "$source_file" "$incoming" || return 1
  chmod 0600 -- "$incoming" || return 1
  mv -n -- "$incoming" "$offline/inbox/$digest.$suffix" || return 1
  rm -f -- "$incoming"
  : > "$offline/delivered/$digest.$suffix"
  echo "[entrypoint] offline operation: delivered $digest.$suffix"
}

# Deliver the platform-supplied offline-operation request and inputs into the
# inbox of an initialized node, as root before the node account takes the
# tree. Nothing here can stop the start: every failure is logged and the
# start goes on.
deliver_offline_operation_request() {
  local app_root="$1"
  local offline="$app_root/.ai/state/offline-operations"
  local request="${RYEOS_OFFLINE_OPERATION_REQUEST:-}"
  local inputs="${RYEOS_OFFLINE_OPERATION_INPUTS:-}"
  local staged directory input count=0

  # The root pass delivers and re-executes as the node account, which would
  # otherwise read the same files again without the access the root pass had.
  if [[ -n "${RYEOS_OFFLINE_OPERATION_DELIVERED:-}" ]]; then
    return 0
  fi
  if [[ -z "$request" && -z "$inputs" ]]; then
    return 0
  fi
  if [[ -L "$app_root" || ! -f "$app_root/.ai/node/identity/private_key.pem" ]]; then
    echo "[entrypoint] offline operation: the node is not initialized; nothing delivered" >&2
    return 0
  fi
  for directory in "$app_root/.ai/state" "$offline" "$offline/inbox" "$offline/delivered"; do
    if [[ -L "$directory" ]]; then
      echo "[entrypoint] offline operation: $directory is a link; nothing delivered" >&2
      return 0
    fi
    [[ -d "$directory" ]] || mkdir -m 700 -- "$directory" || {
      echo "[entrypoint] offline operation: $directory could not be created; nothing delivered" >&2
      return 0
    }
  done
  # Inputs first, so the request is never in the inbox without them.
  if [[ -n "$inputs" ]]; then
    if [[ -L "$inputs" || ! -d "$inputs" ]]; then
      echo "[entrypoint] offline operation: RYEOS_OFFLINE_OPERATION_INPUTS is not a directory" >&2
    else
      while IFS= read -r -d '' input; do
        count=$((count + 1))
        if [[ "$count" -gt "$OFFLINE_MAX_INPUTS" ]]; then
          echo "[entrypoint] offline operation: more than $OFFLINE_MAX_INPUTS input files; the rest are not delivered" >&2
          break
        fi
        copy_into_offline_inbox "$input" input "$OFFLINE_MAX_INPUT_BYTES" "$offline" || true
      done < <(find "$inputs" -maxdepth 1 -mindepth 1 -print0)
    fi
  fi
  if [[ -n "$request" ]]; then
    if [[ "$request" == /* ]]; then
      copy_into_offline_inbox "$request" json "$OFFLINE_MAX_REQUEST_BYTES" "$offline" || true
    else
      if [[ "${#request}" -gt $((OFFLINE_MAX_REQUEST_BYTES * 2)) ]]; then
        echo "[entrypoint] offline operation: the request value is over the size limit; not delivered" >&2
        return 0
      fi
      staged="$(mktemp -p "$offline" .request-XXXXXX)" || return 0
      if printf '%s' "$request" | base64 -d > "$staged" 2>/dev/null; then
        copy_into_offline_inbox "$staged" json "$OFFLINE_MAX_REQUEST_BYTES" "$offline" || true
      else
        echo "[entrypoint] offline operation: RYEOS_OFFLINE_OPERATION_REQUEST is neither an absolute path nor base64; not delivered" >&2
      fi
      rm -f -- "$staged"
    fi
  fi
}

# Refuse a start that sets a retired environment route. The environment no
# longer changes node state: a signed offline operation does, and a variable
# that silently did nothing would leave the operator believing it had.
refuse_retired_environment() {
  if [[ -n "${RYEOS_RESET_NODE_POLICY_GENERATION:-}" ]]; then
    echo "[entrypoint] RYEOS_RESET_NODE_POLICY_GENERATION no longer exists; replace the policy generation with a signed node.reset.policy-generation offline operation" >&2
    return 1
  fi
  if [[ -n "${RYEOS_EXECUTION_HISTORY_SCHEMA_CUT:-}" ]]; then
    echo "[entrypoint] RYEOS_EXECUTION_HISTORY_SCHEMA_CUT no longer exists; cut the execution history with a signed node.reset.execution-history offline operation" >&2
    return 1
  fi
}

# Refuse to seed an app root that is not fresh. A node with a completed init
# or a published policy generation (the markers the daemon treats as
# initialized) must not be silently re-initialized under this image's profile
# because its bundle directory went missing. An identity alone is what a first
# init that died before completion leaves, and it is seeded again.
require_fresh_app_root() {
  local app_root="$1"
  local held

  for held in \
    "$app_root/.ai/config/onboarding/init-completion.json" \
    "$app_root/.ai/node/policies"; do
    if [[ -e "$held" || -L "$held" ]]; then
      echo "[entrypoint] refusing to start: the installed bundle generation is absent but $held exists. This app root is initialized, so it is not re-seeded from the image; restore the bundle generation, or start a fresh app root" >&2
      return 1
    fi
  done
}

# Re-execute this script as the node account before any ryeos command. As
# root: own /data and the app root for the node account (the platform mounts
# the volume root-owned on every boot, so this is per boot, not per image),
# then drop every privilege: supplementary groups, every capability from the
# bounding and inheritable sets, and the ability to regain privilege through
# a setuid or file-capability executable. Already the node account: continue.
# Any other non-root account is the platform's choice and is kept, but named.
# The external host-runtime route keeps root only for its administrator
# bootstrap; see the header.
run_as_node_account() {
  local uid
  uid="$(id -u)"
  if [[ "$uid" == "$NODE_UID" ]]; then
    deliver_offline_operation_request /data/app
    return 0
  fi
  if [[ "$uid" != 0 ]]; then
    echo "[entrypoint] running as uid $uid (platform-selected), not the image's node account $NODE_UID" >&2
    deliver_offline_operation_request /data/app
    return 0
  fi
  if [[ -n "${RYEOS_HOST_RUNTIME_BINDING:-}" ]]; then
    echo "[entrypoint] external host-runtime binding: keeping root only for the ryeosd host-runtime bootstrap, which drops to the binding's account before the daemon runs"
    return 0
  fi
  if [[ -L /data || ! -d /data ]]; then
    echo "[entrypoint] refusing to start: /data must be a mounted directory, not a symlink or other file" >&2
    return 1
  fi
  mkdir -p /data/app
  deliver_offline_operation_request /data/app
  chown -h "$NODE_UID:$NODE_GID" /data
  hand_app_root_to_node_account /data/app
  echo "[entrypoint] /data handed to the node account $NODE_UID:$NODE_GID; dropping root"
  log_path_modes /data /data/app
  echo "[entrypoint] umask $(umask)"
  echo "[entrypoint] /data mount: $(awk '$2=="/data" {print $1, $3, $4}' /proc/self/mounts 2>&1 || true)"
  local -a cap_args=(--inh-caps=-all)
  # Some hosted sandboxes refuse changes to the capability bounding set. The
  # uid change already empties the effective and permitted sets, and
  # no_new_privs keeps them empty, so the drop proceeds without that one flag
  # and says so.
  if setpriv --bounding-set=-all -- true 2>/dev/null; then
    cap_args+=(--bounding-set=-all)
  else
    echo "[entrypoint] host refuses capability bounding-set changes; dropping root without clearing the bounding set" >&2
  fi
  export HOME=/data/app
  export RYEOS_OFFLINE_OPERATION_DELIVERED=1
  exec setpriv --reuid="$NODE_UID" --regid="$NODE_GID" --clear-groups \
    --no-new-privs "${cap_args[@]}" \
    -- "${BASH_SOURCE[0]}" "$@"
}

# Fill TRUST_ARGS only after an operator explicitly opts in to trusting
# publisher documents baked into the image. Release images need no trust
# arguments: `ryeos init` pins the official publisher from key bytes compiled
# into the CLI. A locally built image signed by a development/custom key must
# instead be started with RYEOS_TRUST_BAKED_PUBLISHERS=1.
collect_baked_publisher_trust_args() {
  local source_dir="$1"
  local trust_file
  local trust_files=()

  TRUST_ARGS=()
  case "${RYEOS_TRUST_BAKED_PUBLISHERS:-0}" in
    0|"")
      return 0
      ;;
    1)
      if [[ -f "$source_dir/.ai/PUBLISHER_TRUST.toml" ]]; then
        trust_files+=("$source_dir/.ai/PUBLISHER_TRUST.toml")
      fi
      for trust_file in "$source_dir"/*/PUBLISHER_TRUST.toml; do
        [[ -f "$trust_file" ]] && trust_files+=("$trust_file")
      done
      if [[ ${#trust_files[@]} -eq 0 ]]; then
        echo "[entrypoint] RYEOS_TRUST_BAKED_PUBLISHERS=1, but no baked publisher trust documents were found under $source_dir" >&2
        return 1
      fi
      echo "[entrypoint] explicitly trusting ${#trust_files[@]} baked publisher document(s)"
      for trust_file in "${trust_files[@]}"; do
        TRUST_ARGS+=(--trust-file "$trust_file")
      done
      ;;
    *)
      echo "[entrypoint] invalid RYEOS_TRUST_BAKED_PUBLISHERS value; use 0 (default) or 1" >&2
      return 1
      ;;
  esac
}

container_bind() {
  printf '[::]:%s' "${PORT:-8000}"
}

# Select exactly one daemon entry contract. An external host-runtime binding
# means the administrator/environment builder has already initialized the
# bound app-root generation and delegated its process-scope controller. The
# ordinary container entry must neither rewrite that generation as root nor
# silently fall back to direct daemon execution.
build_ryeos_daemon_args() {
  local app_root="$1"

  DAEMON_ARGS=()
  if [[ -n "${RYEOS_HOST_RUNTIME_BINDING:-}" ]]; then
    [[ "$RYEOS_HOST_RUNTIME_BINDING" = /* ]] || {
      echo "[entrypoint] RYEOS_HOST_RUNTIME_BINDING must be an absolute protected path" >&2
      return 1
    }
    DAEMON_ARGS=(host-runtime --binding "$RYEOS_HOST_RUNTIME_BINDING")
    return 0
  fi
  DAEMON_ARGS=(--app-root "$app_root")
  # The image seed a signed policy replacement is applied from. The daemon
  # never reads it from the environment: it arrives as flags, and only for an
  # image that pins its substrate by digest.
  if [[ "${RYEOS_SUBSTRATE_IMAGE:-}" =~ @sha256:[0-9a-f]{64}$ ]]; then
    DAEMON_ARGS+=(--image-seed-root /opt/ryeos --substrate-image-digest "${RYEOS_SUBSTRATE_IMAGE##*@}")
  fi
}

# Build the lifecycle argument vector without knowing any distribution or
# provider name. Every image must set the one generic init-profile variable. It is
# first-publication authority only: an absent generation receives the exact
# mapped profile, while any present generation occupant is preserved and left to
# Rust's signed complete-generation validation. A malformed or partial occupant
# therefore fails; packaging never falls back to the profile.
#
# Whenever the profile is passed, the deployment must also declare its host
# class in RYEOS_INIT_HOST_CLASS. The image never sets it: the class selects
# which of the profile's signed capability floors the node holds, and it is a
# property of where the image is deployed, never inferred from the image or
# from what the host happens to prove.
build_ryeos_init_args() {
  local source_dir="$1"
  local app_root="$2"
  local bind="$3"
  local policy_generation="$app_root/.ai/node/policies"

  [[ -n "${RYEOS_INIT_NODE_PROFILE:-}" ]] || {
    echo "[entrypoint] RYEOS_INIT_NODE_PROFILE is required" >&2
    return 1
  }
  [[ "${RYEOS_SUBSTRATE_IMAGE:-}" =~ @sha256:[0-9a-f]{64}$ ]] || {
    echo "[entrypoint] RYEOS_SUBSTRATE_IMAGE must be pinned by digest" >&2
    return 1
  }
  [[ "${RYEOS_SUBSTRATE_PROTOCOL:-}" =~ ^[1-9][0-9]*$ ]] || {
    echo "[entrypoint] RYEOS_SUBSTRATE_PROTOCOL must be a nonzero integer" >&2
    return 1
  }
  local -a profile_args=(--node-profile "$RYEOS_INIT_NODE_PROFILE")
  INIT_ARGS=(
    init
    --non-interactive
    --app-root "$app_root"
    --source "$source_dir"
    --bind "$bind"
    --substrate-image-digest "${RYEOS_SUBSTRATE_IMAGE##*@}"
    --substrate-protocol "$RYEOS_SUBSTRATE_PROTOCOL"
  )
  warn_on_changed_host_class "$app_root"
  # An absent generation is published from the image seed exactly once. A
  # present one is never replaced here: replacing it is a signed offline
  # operation that names the seed's digest.
  if [[ ! -e "$policy_generation" && ! -L "$policy_generation" ]]; then
    require_init_host_class || return 1
    INIT_ARGS+=("${profile_args[@]}" --host-class "$RYEOS_INIT_HOST_CLASS")
  else
    echo "[entrypoint] preserving existing signed node policy generation"
  fi
}

# The host class an initialized node recorded in its substrate identity, or
# nothing when the node has none recorded. Rust verifies the signed record;
# this read only informs the warning below.
recorded_host_class() {
  local identity="$1/.ai/node/substrate-identity.json"
  local content

  [[ -f "$identity" && ! -L "$identity" ]] || return 0
  content="$(<"$identity")"
  if [[ "$content" =~ \"host_class\"[[:space:]]*:[[:space:]]*\"([a-z][a-z0-9_]*)\" ]]; then
    printf '%s' "${BASH_REMATCH[1]}"
  fi
}

# A node's host class is fixed when it is initialized. A deployment that now
# declares another class is told so plainly: the node keeps its recorded
# class, and init refuses any profile selection for the new one.
warn_on_changed_host_class() {
  local recorded

  [[ -n "${RYEOS_INIT_HOST_CLASS:-}" ]] || return 0
  recorded="$(recorded_host_class "$1")"
  if [[ -n "$recorded" && "$recorded" != "$RYEOS_INIT_HOST_CLASS" ]]; then
    echo "[entrypoint] warning: RYEOS_INIT_HOST_CLASS is $RYEOS_INIT_HOST_CLASS, but this node was initialized under host class $recorded. A node's host class does not change: it keeps $recorded. Set RYEOS_INIT_HOST_CLASS=$recorded, or start a fresh app root to run under $RYEOS_INIT_HOST_CLASS." >&2
  fi
}

require_init_host_class() {
  [[ -n "${RYEOS_INIT_HOST_CLASS:-}" ]] || {
    echo "[entrypoint] RYEOS_INIT_HOST_CLASS is required to initialize from RYEOS_INIT_NODE_PROFILE; set it to a host class the profile declares (for example local or hosted)" >&2
    return 1
  }
}

# Seconds to wait before starting ryeosd: the bounded hold while the node's
# restart inhibition record exists, else 0. ryeosd itself re-reads the record
# and refuses the start, so this only paces the restart loop.
restart_inhibition_hold_seconds() {
  local app_root="$1"

  if [[ -e "$app_root/.ai/state/restart-inhibited.json" ]]; then
    printf '%s' "$RESTART_INHIBITED_HOLD_SECONDS"
  else
    printf '0'
  fi
}

# Wait out the bounded hold before any ryeosd start, on every start route.
hold_while_restart_inhibited() {
  local app_root="$1"
  local hold

  hold="$(restart_inhibition_hold_seconds "$app_root")"
  if [[ "$hold" -gt 0 ]]; then
    echo "[entrypoint] restart inhibited: ryeosd will refuse this start until 'ryeos node allow-restart --app-root $app_root' runs; waiting ${hold}s first" >&2
    sleep "$hold"
  fi
}

# Succeed when an operator hold is requested, fail quietly when it is not, and
# fail with a message on any value other than unset, "0" or "1".
operator_hold_requested() {
  case "${RYEOS_OPERATOR_HOLD:-0}" in
    0) return 1 ;;
    1) return 0 ;;
    *)
      echo "[entrypoint] RYEOS_OPERATOR_HOLD must be unset, 0 or 1" >&2
      return 2
      ;;
  esac
}

# When an operator hold is requested, stay stopped until the release marker
# appears, then remove it and return so the caller starts ryeosd normally.
hold_for_operator() {
  local app_root="$1"
  local marker="$app_root/.ai/state/operator-hold-released"
  local status=0

  operator_hold_requested || status=$?
  [[ "$status" -eq 0 ]] || return 0
  # Only a touch made after the hold begins releases it, and the state
  # directory is created here so that touch cannot fail.
  mkdir -p -- "$app_root/.ai/state"
  rm -f -- "$marker"
  echo "[entrypoint] node held for operator commands; ryeosd is not running. Touch $marker to start it, or unset RYEOS_OPERATOR_HOLD and redeploy" >&2
  until [[ -e "$marker" ]]; do
    sleep "$OPERATOR_HOLD_POLL_SECONDS"
  done
  rm -f -- "$marker"
}

main() {
  local effective_bind
  local hold_status=0
  operator_hold_requested || hold_status=$?
  [[ "$hold_status" -ne 2 ]] || return 1
  refuse_retired_environment || return 1
  run_as_node_account "$@"
  effective_bind="$(container_bind)"

  build_ryeos_daemon_args /data/app
  if [[ "${DAEMON_ARGS[0]}" == "host-runtime" ]]; then
    if [[ "$(id -u)" != 0 ]]; then
      echo "[entrypoint] refusing to start: RYEOS_HOST_RUNTIME_BINDING is set, but ryeosd host-runtime is an administrator bootstrap and this container started as uid $(id -u), not root" >&2
      return 1
    fi
    if [[ "$hold_status" -eq 0 ]]; then
      echo "[entrypoint] RYEOS_OPERATOR_HOLD is not supported with RYEOS_HOST_RUNTIME_BINDING" >&2
      return 1
    fi
    hold_while_restart_inhibited /data/app
    echo "[entrypoint] starting the root host-runtime bootstrap; the daemon itself runs as the binding's account"
    exec ryeosd "${DAEMON_ARGS[@]}"
  fi

  mkdir -p /data
  if [[ ! -e /data/app/.ai/bundles && ! -L /data/app/.ai/bundles ]]; then
    require_fresh_app_root /data/app || return 1
    echo "[entrypoint] installed bundle generation absent; seeding from substrate image"
    collect_baked_publisher_trust_args /opt/ryeos
    log_path_modes /data/app/.ai/node /data/app/.ai/node/native-capacity
    build_ryeos_init_args /opt/ryeos /data/app "$effective_bind"
    ryeos "${INIT_ARGS[@]}" "${TRUST_ARGS[@]}"
  else
    echo "[entrypoint] preserving installed bundle generation; image seed is first-boot only"
  fi

  hold_for_operator /data/app
  hold_while_restart_inhibited /data/app

  if [[ "$(id -u)" == 0 ]]; then
    echo "[entrypoint] refusing to start ryeosd as root; the node account is uid $NODE_UID" >&2
    return 1
  fi
  echo "[entrypoint] bootstrap check complete, starting daemon as uid $(id -u)"
  # Daemon bootstrap auto-inits any artifacts `ryeos init` doesn't produce
  # (e.g. public-identity.json, vault keypair). Idempotent — no-op when
  # already written.
  exec ryeosd "${DAEMON_ARGS[@]}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
