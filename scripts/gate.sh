#!/usr/bin/env bash
# Workspace test gate.
#
# This intentionally does NOT rebuild/repopulate bundles by default. Bundle
# refresh is expensive and should be an explicit authoring/release action.
#
# Usage:
#     ./scripts/gate.sh                         # full workspace tests
#     ./scripts/gate.sh -p ryeosd               # forwarded to nextest
#     ./scripts/gate.sh --refresh-bundles       # explicit full bundle refresh, then tests
#     ./scripts/gate.sh --refresh-bundles --no-tests
#     ./scripts/gate.sh --refresh-bundles --crash-qualification-only
#     ./scripts/gate.sh --refresh-bundles --skip-crash-qualification
#     ./scripts/gate.sh --bundle-set core --refresh-bundles
#
# CI-only split of the gate into one build and many shards (see
# .github/workflows/ci.yml). None of these change a plain local run:
#     ./scripts/gate.sh --nextest-archive FILE        # build the workspace test archive, run nothing
#     ./scripts/gate.sh --nextest-run-archive FILE --partition hash:1/8
#                                                     # run one shard from that archive, no compiling
#     ./scripts/gate.sh --crash-archive FILE          # build the crash-qualification archive + CLI
#     ./scripts/gate.sh --crash-run-archive FILE --crash-phase NAME [--crash-partition M/N]
#                                                     # one serial crash phase from that archive
#   NAME: directive | handoff | schedule-deploy | reset-scheduler
#
# CI/release jobs that need regenerated bundle binaries/manifests must pass
# --refresh-bundles explicitly. Local UI/dev loops should not.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
gate_tty=0
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$ROOT/scripts/lib/ryeos-terminal.sh"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$ROOT/scripts/pkg/bundle-sets.sh"
ryeos_term_init
if ryeos_term_is_tty; then
    gate_tty=1
fi

gate_info() {
    if [[ "$gate_tty" == 1 ]]; then ryeos_term_info "$*"; else printf 'gate: %s\n' "$*"; fi
}

gate_fail() {
    if [[ "$gate_tty" == 1 ]]; then ryeos_term_fail "$*"; else printf 'gate: %s\n' "$*" >&2; fi
}

# The custody token ban is a text scan: it runs before any build, whatever
# the options, so a banned token fails fast.
gate_info "custody token ban"
if ! "$ROOT/scripts/ci/check-custody-tokens.sh"; then
    gate_fail "custody token ban refused"
    exit 1
fi

# Default publisher signing key + owner — used by populate-bundles.sh.
# Override with KEY=... OWNER=... if you have a different setup.
KEY="${KEY:-$ROOT/.dev-keys/PUBLISHER_DEV.pem}"
OWNER="${OWNER:-RyeOS Development}"

skip_tests=0
refresh_bundles=0
skip_crash_qualification=0
crash_qualification_only=0
bundle_set="full"
nextest_archive=""
nextest_run_archive=""
crash_archive=""
crash_run_archive=""
crash_phase=""
crash_partition=""
nextest_args=()
selected_packages=()
explicit_workspace=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-tests)
            skip_tests=1
            shift
            ;;
        --refresh-bundles)
            refresh_bundles=1
            shift
            ;;
        --skip-crash-qualification)
            skip_crash_qualification=1
            shift
            ;;
        --crash-qualification-only)
            crash_qualification_only=1
            shift
            ;;
        --nextest-archive|--nextest-run-archive|--crash-archive|--crash-run-archive|--crash-phase|--crash-partition)
            [[ $# -ge 2 && -n "$2" ]] || { gate_fail "$1 requires a value"; exit 2; }
            case "$1" in
                --nextest-archive) nextest_archive="$2" ;;
                --nextest-run-archive) nextest_run_archive="$2" ;;
                --crash-archive) crash_archive="$2" ;;
                --crash-run-archive) crash_run_archive="$2" ;;
                --crash-phase) crash_phase="$2" ;;
                --crash-partition) crash_partition="$2" ;;
            esac
            shift 2
            ;;
        --bundle-set)
            [[ $# -ge 2 ]] || { gate_fail "--bundle-set requires a value"; exit 2; }
            bundle_set="$2"
            shift 2
            ;;
        -p|--package)
            [[ $# -ge 2 && -n "$2" ]] || { gate_fail "$1 requires a package"; exit 2; }
            selected_packages+=("$2")
            nextest_args+=("$1" "$2")
            shift 2
            ;;
        --package=*)
            selected_packages+=("${1#--package=}")
            nextest_args+=("$1")
            shift
            ;;
        -p?*)
            selected_packages+=("${1#-p}")
            nextest_args+=("$1")
            shift
            ;;
        --workspace|--all)
            explicit_workspace=1
            nextest_args+=("$1")
            shift
            ;;
        *)
            nextest_args+=("$1")
            shift
            ;;
    esac
done

if [[ "$skip_crash_qualification" == "1" && "$crash_qualification_only" == "1" ]]; then
    gate_fail "--skip-crash-qualification conflicts with --crash-qualification-only"
    exit 2
fi
archive_modes=0
for mode_value in "$nextest_archive" "$nextest_run_archive" "$crash_archive" "$crash_run_archive"; do
    [[ -z "$mode_value" ]] || archive_modes=$((archive_modes + 1))
done
if (( archive_modes > 1 )); then
    gate_fail "only one of --nextest-archive, --nextest-run-archive, --crash-archive, --crash-run-archive may be given"
    exit 2
fi
if (( archive_modes == 1 )); then
    if [[ "$refresh_bundles" == "1" || "$skip_tests" == "1" || "$crash_qualification_only" == "1" ]]; then
        gate_fail "archive modes take staged bundles from the checkout and conflict with --refresh-bundles, --no-tests and --crash-qualification-only"
        exit 2
    fi
fi
if [[ -n "$crash_run_archive" && -z "$crash_phase" ]] || [[ -z "$crash_run_archive" && ( -n "$crash_phase" || -n "$crash_partition" ) ]]; then
    gate_fail "--crash-phase and --crash-partition go with --crash-run-archive, which requires --crash-phase"
    exit 2
fi
if [[ "$crash_qualification_only" == "1" ]]; then
    if [[ "$skip_tests" == "1" ]]; then
        gate_fail "--crash-qualification-only conflicts with --no-tests"
        exit 2
    fi
    if [[ "$refresh_bundles" != "1" || "$bundle_set" != "full" ]]; then
        gate_fail "--crash-qualification-only requires --refresh-bundles with the full bundle set"
        exit 2
    fi
    if [[ ${#nextest_args[@]} -ne 0 ]]; then
        gate_fail "--crash-qualification-only does not accept forwarded nextest arguments"
        exit 2
    fi
fi

if [[ "$refresh_bundles" == "1" ]]; then
    if [[ ! -s "$KEY" ]]; then
        gate_fail "signing key not found at $KEY"
        gate_fail "set KEY=/path/to/PUBLISHER.pem (or create $KEY)"
        exit 2
    fi
    gate_info "explicitly refreshing bundles (bundle-set: $bundle_set)"
    # --all: gate is a deliberate full bundle-set refresh; populate-bundles.sh
    # refuses an implicit full build without it.
    if [[ -n "${GATE_BUNDLE_TARGET_DIR:-}" ]]; then
        gate_info "using disposable bundle-build target: $GATE_BUNDLE_TARGET_DIR"
        CARGO_TARGET_DIR="$GATE_BUNDLE_TARGET_DIR" \
            "$ROOT/scripts/populate-bundles.sh" --key "$KEY" --owner "$OWNER" --bundle-set "$bundle_set" --all
        # Bundle binaries and signed manifests have been staged into their
        # source trees. The optimized build graph is no longer needed by the
        # test profile and must not compete with it for hosted-runner disk.
        "$CARGO" clean --target-dir "$GATE_BUNDLE_TARGET_DIR"
    else
        "$ROOT/scripts/populate-bundles.sh" --key "$KEY" --owner "$OWNER" --bundle-set "$bundle_set" --all
    fi
elif [[ "$skip_tests" == "1" ]]; then
    gate_fail "--no-tests without --refresh-bundles has nothing to do"
    gate_fail "pass --refresh-bundles --no-tests for explicit bundle preparation"
    exit 2
fi

if [[ "$skip_tests" == "1" ]]; then
    exit 0
fi

# A refresh has just built and staged the core bundle's release binaries. The
# publish idempotency tests need exactly those, and each nextest test is its
# own process, so name the staged directory rather than letting every such
# test run its own cold release build.
if [[ "$refresh_bundles" == "1" || -n "$nextest_run_archive" || -n "$crash_run_archive" ]]; then
    staged_core_bins="$ROOT/bundles/core/.ai/bin/x86_64-unknown-linux-gnu"
    if [[ -d "$staged_core_bins" ]]; then
        export RYEOS_TEST_HANDLER_BIN_DIR="$staged_core_bins"
    fi
fi

# Resource caps. The workspace has heavy integration tests (some spawn daemons),
# so running them at full parallelism can exhaust memory and lock up the machine.
# Default to half the available cores for both compilation and test execution.
# Override with GATE_TEST_THREADS / GATE_BUILD_JOBS, or set either to 0 to let
# cargo/nextest use their own defaults.
default_jobs="$(( $(nproc 2>/dev/null || echo 2) / 2 ))"
(( default_jobs < 1 )) && default_jobs=1
test_threads="${GATE_TEST_THREADS:-$default_jobs}"
build_jobs="${GATE_BUILD_JOBS:-$default_jobs}"

cargo_jobs_args=()
[[ "$build_jobs" != "0" ]] && cargo_jobs_args=(--build-jobs "$build_jobs")
test_threads_args=()
[[ "$test_threads" != "0" ]] && test_threads_args=(--test-threads "$test_threads")

# Cargo's target directory, for the CI archive modes.
gate_target_dir() {
    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        printf '%s\n' "$CARGO_TARGET_DIR"
    else
        printf '%s\n' "$ROOT/target"
    fi
}

# Archive modes extract here. Nextest keeps the archived target tree under
# <dir>/target.
archive_extract_dir="${GATE_ARCHIVE_EXTRACT_DIR:-$ROOT/target/gate-archive}"

# Extra files for an archive: nextest archives only the test binaries and the
# binaries each test names through CARGO_BIN_EXE, but tests also locate sibling
# workspace binaries beside their own executable. These ride in a tarball next
# to the archive, relative to the target directory.
gate_pack_extras() {
    local out="$1"
    shift
    local tdir
    tdir="$(gate_target_dir)"
    (
        cd "$tdir"
        {
            find debug -maxdepth 1 -type f -perm -u+x -print0
            for extra in "$@"; do printf '%s\0' "$extra"; done
        } | tar --null --files-from=- -cf "$out"
    )
}

gate_unpack_extras() {
    local extras="$1"
    [[ -f "$extras" ]] || { gate_fail "missing archive companion $extras"; exit 1; }
    mkdir -p "$archive_extract_dir/target"
    tar -xf "$extras" -C "$archive_extract_dir/target"
}

# One serial crash-qualification phase. Each phase is a single test binary run
# on one test thread; in the archive modes each runs in its own job.
crash_features="scheduling,model-provider-setup,crash-qualification-test-support,handoff-test-support"
crash_phase_test() {
    case "$1" in
        directive) echo directive_crash_recovery_e2e ;;
        handoff) echo handoff_crash_recovery_e2e ;;
        schedule-deploy) echo project_schedule_deploy_e2e ;;
        reset-scheduler) echo execution_history_reset_scheduler_e2e ;;
        *) return 1 ;;
    esac
}

if [[ -n "$crash_archive" ]]; then
    crash_tests=()
    for phase in directive handoff schedule-deploy reset-scheduler; do
        crash_tests+=(--test "$(crash_phase_test "$phase")")
    done
    gate_info "cargo nextest archive crash qualification (build_jobs=${build_jobs})"
    "$CARGO" nextest archive -p ryeosd --features "$crash_features" \
        "${crash_tests[@]}" "${cargo_jobs_args[@]}" --archive-file "$crash_archive"
    gate_info "building the crash-qualification ryeos CLI for scheduler reset cuts"
    cli_jobs_args=()
    [[ "$build_jobs" != "0" ]] && cli_jobs_args=(-j "$build_jobs")
    crash_cli_target="$(gate_target_dir)/crash-qualification"
    "$CARGO" build -p ryeos-cli \
        --target-dir "$crash_cli_target" \
        --features scheduling,model-provider-setup,ryeos-scheduler-service/crash-qualification-test-support \
        "${cli_jobs_args[@]}"
    gate_pack_extras "${crash_archive}.extras.tar" crash-qualification/debug/ryeos
    exit 0
fi

if [[ -n "$crash_run_archive" ]]; then
    phase_test="$(crash_phase_test "$crash_phase")" || {
        gate_fail "unknown crash phase: $crash_phase"
        exit 2
    }
    gate_unpack_extras "${crash_run_archive}.extras.tar"
    partition_args=()
    [[ -z "$crash_partition" ]] || partition_args=(--partition "$crash_partition")
    run_phase=("$CARGO" nextest run --archive-file "$crash_run_archive"
        --extract-to "$archive_extract_dir" --extract-overwrite --workspace-remap "$ROOT"
        -E "binary($phase_test)" "${partition_args[@]}" --test-threads 1)
    gate_info "qualifying crash phase $crash_phase${crash_partition:+ (partition $crash_partition)}"
    case "$crash_phase" in
        directive|schedule-deploy)
            "${run_phase[@]}"
            ;;
        reset-scheduler)
            RYEOS_TEST_CLI_BINARY="$archive_extract_dir/target/crash-qualification/debug/ryeos" \
                "${run_phase[@]}"
            ;;
        handoff)
            handoff_report_dir="$ROOT/target/qualification"
            mkdir -p "$handoff_report_dir"
            handoff_report="$handoff_report_dir/explicit-worker-handoff.json"
            handoff_report_candidate="$(mktemp "$handoff_report_dir/.explicit-worker-handoff.XXXXXX")"
            rm -f -- "$handoff_report" "$handoff_report_candidate"
            trap 'rm -f -- "$handoff_report_candidate"' EXIT
            RYEOS_HANDOFF_QUALIFICATION_REPORT="$handoff_report_candidate" \
                "${run_phase[@]}" --success-output immediate
            if [[ ! -f "$handoff_report_candidate" || ! -s "$handoff_report_candidate" ]]; then
                gate_fail "handoff qualification succeeded without a nonempty canonical report"
                exit 1
            fi
            mv -- "$handoff_report_candidate" "$handoff_report"
            trap - EXIT
            gate_info "retained explicit handoff qualification report: $handoff_report"
            ;;
    esac
    exit 0
fi

if [[ "$crash_qualification_only" != "1" ]]; then
    # The Core reporter revisions are stdlib-only Python with their own
    # unit tests; no cargo target runs them.
    if [[ "$skip_tests" != "1" && -z "$nextest_run_archive" ]]; then
        gate_info "running Core reporter unit tests"
        python3 -I -B "$ROOT/bundles/core/tests/reporter/test_reporter.py"
    fi
    if [[ -n "$nextest_run_archive" ]]; then
        gate_unpack_extras "${nextest_run_archive}.extras.tar"
        gate_info "cargo nextest run --archive-file (test_threads=${test_threads}) ${nextest_args[*]:-}"
        "$CARGO" nextest run --archive-file "$nextest_run_archive" \
            --extract-to "$archive_extract_dir" --extract-overwrite --workspace-remap "$ROOT" \
            --no-fail-fast "${test_threads_args[@]}" "${nextest_args[@]}"
        exit 0
    fi
    workspace_args=()
    if [[ ${#selected_packages[@]} -eq 0 && "$explicit_workspace" == 0 ]]; then
        workspace_args=(--workspace)
    fi
    gate_features=""
    # Cargo --workspace widens even an explicit -p selection. Package-scoped
    # checks must also avoid features belonging to unselected packages.
    for package in ryeos-handler-bins ryeosd ryeos-cli ryeos-api; do
        package_selected=0
        if [[ ${#selected_packages[@]} -eq 0 || "$explicit_workspace" == 1 ]]; then
            package_selected=1
        else
            for selection in "${selected_packages[@]}"; do
                # Cargo accepts package globs and name@version selectors.
                # Match their name component; forward the original unchanged.
                # The unquoted pattern is deliberate: selections may be globs.
                # shellcheck disable=SC2254
                case "$package" in ${selection%@*}) package_selected=1 ;; esac
            done
        fi
        [[ "$package_selected" == 1 ]] || continue
        composition_feature="$(ryeos_bundle_set_cargo_features "$bundle_set" "$package")" || {
            gate_fail "unknown bundle-set composition: $bundle_set"
            exit 2
        }
        [[ -z "$composition_feature" ]] || gate_features+="${gate_features:+,}$composition_feature"
    done
    feature_args=()
    [[ -z "$gate_features" ]] || feature_args=(--features "$gate_features")
    if [[ -n "$nextest_archive" ]]; then
        gate_info "cargo nextest archive ${workspace_args[*]:-} (build_jobs=${build_jobs}) ${nextest_args[*]:-}"
        "$CARGO" nextest archive "${workspace_args[@]}" \
            "${feature_args[@]}" "${cargo_jobs_args[@]}" "${nextest_args[@]}" \
            --archive-file "$nextest_archive"
        gate_pack_extras "${nextest_archive}.extras.tar"
        exit 0
    fi
    gate_info "cargo nextest run ${workspace_args[*]:-} --no-fail-fast (build_jobs=${build_jobs}, test_threads=${test_threads}) ${nextest_args[*]:-}"
    "$CARGO" nextest run "${workspace_args[@]}" --no-fail-fast \
        "${feature_args[@]}" \
        "${cargo_jobs_args[@]}" "${test_threads_args[@]}" "${nextest_args[@]}"
fi

# A production daemon must not expose the crash-cut endpoint, so this matrix
# cannot be part of the default-feature workspace binary. A full, explicitly
# refreshed source gate has the exact signed runtime bytes required by the
# process-kill qualification and must execute every durable cut before release.
if [[ "$refresh_bundles" == "1" \
    && "$bundle_set" == "full" \
    && "$skip_crash_qualification" != "1" \
    && ${#nextest_args[@]} -eq 0 ]]; then
    crash_qualification_features="scheduling,model-provider-setup,crash-qualification-test-support,handoff-test-support"
    gate_info "qualifying directive native-resume crash cuts"
        "$CARGO" nextest run -p ryeosd \
        --features "$crash_qualification_features" \
        --test directive_crash_recovery_e2e \
        "${cargo_jobs_args[@]}" --test-threads 1

    gate_info "qualifying explicit cross-site worker handoff crash cuts"
    handoff_report_dir="$ROOT/target/qualification"
    mkdir -p "$handoff_report_dir"
    handoff_report="$handoff_report_dir/explicit-worker-handoff.json"
    handoff_report_candidate="$(mktemp "$handoff_report_dir/.explicit-worker-handoff.XXXXXX")"
    rm -f -- "$handoff_report" "$handoff_report_candidate"
    cleanup_handoff_report_candidate() {
        rm -f -- "$handoff_report_candidate"
    }
    trap cleanup_handoff_report_candidate EXIT
    RYEOS_HANDOFF_QUALIFICATION_REPORT="$handoff_report_candidate" \
        "$CARGO" nextest run -p ryeosd \
        --features "$crash_qualification_features" \
        --test handoff_crash_recovery_e2e \
        "${cargo_jobs_args[@]}" --test-threads 1 --success-output immediate
    if [[ ! -f "$handoff_report_candidate" || ! -s "$handoff_report_candidate" ]]; then
        gate_fail "handoff qualification succeeded without a nonempty canonical report"
        exit 1
    fi
    mv -- "$handoff_report_candidate" "$handoff_report"
    trap - EXIT
    gate_info "retained explicit handoff qualification report: $handoff_report"

    gate_info "qualifying project schedule deploy crash cuts"
    "$CARGO" nextest run -p ryeosd \
        --features "$crash_qualification_features" \
        --test project_schedule_deploy_e2e \
        "${cargo_jobs_args[@]}" --test-threads 1

    # The offline reset runs in the CLI process, so the CLI beside ryeosd must
    # carry the selected scheduler participant and its reset crash cuts. It is
    # built into its own target directory so the normal CLI beside ryeosd is
    # not replaced, and the test is pointed at it explicitly.
    gate_info "building the crash-qualification ryeos CLI for scheduler reset cuts"
    cli_jobs_args=()
    [[ "$build_jobs" != "0" ]] && cli_jobs_args=(-j "$build_jobs")
    crash_cli_target="$ROOT/target/crash-qualification"
    "$CARGO" build -p ryeos-cli \
        --target-dir "$crash_cli_target" \
        --features scheduling,model-provider-setup,ryeos-scheduler-service/crash-qualification-test-support \
        "${cli_jobs_args[@]}"
    gate_info "qualifying scheduler execution-history reset crash cuts"
    RYEOS_TEST_CLI_BINARY="$crash_cli_target/debug/ryeos" "$CARGO" nextest run -p ryeosd \
        --features "$crash_qualification_features" \
        --test execution_history_reset_scheduler_e2e \
        "${cargo_jobs_args[@]}" --test-threads 1
fi
