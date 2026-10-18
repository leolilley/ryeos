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
# RYEOS_OPERATOR_HOLD=1 opens an operator window on a host that offers a shell
# only while the container runs: after first-boot initialization, instead of
# starting ryeosd, the entrypoint waits with no daemon holding the state lock,
# so stopped-node `ryeos` commands can run in the platform shell. Touch
# <app root>/.ai/state/operator-hold-released to continue to the normal start
# (the marker is removed first), or unset the variable and redeploy. Unset,
# "0" and "1" are the only accepted values; anything else refuses to start.
# The hold is not supported with RYEOS_HOST_RUNTIME_BINDING and refuses there.

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
    return 0
  fi
  if [[ "$uid" != 0 ]]; then
    echo "[entrypoint] running as uid $uid (platform-selected), not the image's node account $NODE_UID" >&2
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
  chown -h "$NODE_UID:$NODE_GID" /data
  hand_app_root_to_node_account /data/app
  echo "[entrypoint] /data handed to the node account $NODE_UID:$NODE_GID; dropping root"
  exec setpriv --reuid="$NODE_UID" --regid="$NODE_GID" --clear-groups \
    --no-new-privs --bounding-set=-all --inh-caps=-all \
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
  case "${RYEOS_RESET_NODE_POLICY_GENERATION:-0}" in
    0|"")
      if [[ ! -e "$policy_generation" && ! -L "$policy_generation" ]]; then
        require_init_host_class || return 1
        INIT_ARGS+=("${profile_args[@]}" --host-class "$RYEOS_INIT_HOST_CLASS")
      else
        echo "[entrypoint] preserving existing signed node policy generation"
      fi
      ;;
    1)
      if [[ -e "$policy_generation" || -L "$policy_generation" ]]; then
        echo "[entrypoint] explicitly replacing obsolete signed node policy generation"
        require_init_host_class || return 1
        INIT_ARGS+=(
          "${profile_args[@]}"
          --host-class "$RYEOS_INIT_HOST_CLASS"
          --replace-node-policy-generation
          --confirm-node-policy-generation-replacement
        )
      else
        require_init_host_class || return 1
        INIT_ARGS+=("${profile_args[@]}" --host-class "$RYEOS_INIT_HOST_CLASS")
      fi
      ;;
    *)
      echo "[entrypoint] invalid RYEOS_RESET_NODE_POLICY_GENERATION value; use 0 (default) or 1" >&2
      return 1
      ;;
  esac
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

build_execution_history_schema_cut_args() {
  local app_root="$1"
  local cut="${RYEOS_EXECUTION_HISTORY_SCHEMA_CUT:-}"

  EXECUTION_HISTORY_SCHEMA_CUT_ARGS=()
  if [[ -z "$cut" ]]; then
    return 0
  fi
  if [[ ! "$cut" =~ ^([0-9]+):([0-9]+)$ ]] || [[ "${BASH_REMATCH[1]}" == "${BASH_REMATCH[2]}" ]]; then
    echo "[entrypoint] invalid RYEOS_EXECUTION_HISTORY_SCHEMA_CUT value; use exact distinct epochs FROM:TO" >&2
    return 1
  fi
  EXECUTION_HISTORY_SCHEMA_CUT_ARGS=(
    node reset execution-history
    --app-root "$app_root"
    --confirm
    --schema-cut-from "${BASH_REMATCH[1]}"
    --schema-cut-to "${BASH_REMATCH[2]}"
  )
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
    echo "[entrypoint] installed bundle generation absent; seeding from substrate image"
    collect_baked_publisher_trust_args /opt/ryeos
    build_ryeos_init_args /opt/ryeos /data/app "$effective_bind"
    ryeos "${INIT_ARGS[@]}" "${TRUST_ARGS[@]}"
  else
    if [[ "${RYEOS_RESET_NODE_POLICY_GENERATION:-0}" != 0 && -n "${RYEOS_RESET_NODE_POLICY_GENERATION:-}" ]]; then
      echo "[entrypoint] node-policy replacement cannot use the first-boot image seed; use an authorized offline operation" >&2
      return 1
    fi
    echo "[entrypoint] preserving installed bundle generation; image seed is first-boot only"
  fi

  build_execution_history_schema_cut_args /data/app
  if [[ ${#EXECUTION_HISTORY_SCHEMA_CUT_ARGS[@]} -gt 0 ]]; then
    echo "[entrypoint] applying exact idempotent execution-history schema cut ${RYEOS_EXECUTION_HISTORY_SCHEMA_CUT}"
    ryeos "${EXECUTION_HISTORY_SCHEMA_CUT_ARGS[@]}"
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
