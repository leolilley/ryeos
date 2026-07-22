#!/usr/bin/env bash

# Shared terminal presentation for operator-facing RyeOS scripts. This file is
# intentionally safe to source: it does not change the caller's shell options.

_RYEOS_TERM_MODE=plain
_RYEOS_TERM_COLOR=0
_RYEOS_TERM_UNICODE=0
_RYEOS_TERM_WIDTH=80
_RYEOS_TERM_ACTIVE=0
_RYEOS_TERM_SUSPENDED=0
_RYEOS_TERM_VERB=""
_RYEOS_TERM_LABEL=""
_RYEOS_TERM_DETAIL=""
_RYEOS_TERM_STARTED=0
_RYEOS_TERM_SPINNER_PID=""
_RYEOS_TERM_DEPTH=0
declare -a _RYEOS_TERM_STACK_VERB=()
declare -a _RYEOS_TERM_STACK_LABEL=()
declare -a _RYEOS_TERM_STACK_DETAIL=()
declare -a _RYEOS_TERM_STACK_STARTED=()
_RYEOS_TERM_TRAPS=0
_RYEOS_TERM_PREV_EXIT=""
_RYEOS_TERM_PREV_INT=""
_RYEOS_TERM_PREV_TERM=""

# Gruvbox semantic tokens shared with the RyeOS UI.
_RYEOS_TERM_TONE_ACTIVE='38;2;214;93;14'
_RYEOS_TERM_TONE_SUCCESS='1;38;2;142;192;124'
_RYEOS_TERM_TONE_WARNING='1;38;2;250;189;47'
_RYEOS_TERM_TONE_FAILURE='1;38;2;251;73;52'
_RYEOS_TERM_TONE_NEUTRAL='38;2;213;196;161'
_RYEOS_TERM_TONE_SECONDARY='38;2;168;153;132'

_ryeos_term_now() {
    printf '%(%s)T' -1
}

_ryeos_term_capture_trap() {
    local signal="$1" variable="$2" specification command=""
    specification="$(trap -p "$signal")"
    if [[ -n "$specification" ]]; then
        command="${specification#trap -- \'}"
        command="${command%\' $signal}"
    fi
    printf -v "$variable" '%s' "$command"
}

_ryeos_term_elapsed() {
    local elapsed now started="${1:-$_RYEOS_TERM_STARTED}"
    now="$(_ryeos_term_now)"
    elapsed=$(( now - started ))
    if (( elapsed < 60 )); then
        printf '%ss' "$elapsed"
    else
        printf '%dm %02ds' "$(( elapsed / 60 ))" "$(( elapsed % 60 ))"
    fi
}

_ryeos_term_detect_columns() {
    local detected="" terminal_size=""
    if [[ "$_RYEOS_TERM_MODE" == tty ]] && command -v stty >/dev/null 2>&1; then
        # Query the terminal attached to stderr. `tput cols` runs with stdout
        # captured by this function, so ncurses can see that pipe instead of
        # the operator's terminal and silently return its 80-column fallback.
        # That makes long spinner frames wrap and turns every carriage-return
        # repaint into a new line. stty reads the real terminal through fd 2.
        terminal_size="$(stty size <&2 2>/dev/null || true)"
        if [[ "$terminal_size" =~ ^[[:space:]]*[0-9]+[[:space:]]+([0-9]+)[[:space:]]*$ ]]; then
            detected="${BASH_REMATCH[1]}"
        fi
    fi
    if [[ -z "$detected" && "${COLUMNS:-}" =~ ^[0-9]+$ ]] && (( COLUMNS >= 2 )); then
        detected="$COLUMNS"
    fi
    if [[ ! "$detected" =~ ^[0-9]+$ ]] || (( detected < 2 )); then
        detected=80
    fi
    printf '%s' "$detected"
}

_ryeos_term_refresh_width() {
    _RYEOS_TERM_WIDTH="$(_ryeos_term_detect_columns)"
    if [[ "$_RYEOS_TERM_MODE" == tty ]]; then
        # Never paint the terminal's final cell. Printing into the last column
        # sets autowrap on common terminals, so the next carriage-return repaint
        # starts on a fresh physical line and every spinner frame pollutes
        # scrollback. One reserved cell keeps the rich renderer in-place.
        _RYEOS_TERM_WIDTH=$(( _RYEOS_TERM_WIDTH - 1 ))
    fi
}

_ryeos_term_stop_spinner() {
    local pid="$_RYEOS_TERM_SPINNER_PID"
    _RYEOS_TERM_SPINNER_PID=""
    if [[ -n "$pid" ]]; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
}

_ryeos_term_clear() {
    _ryeos_term_stop_spinner
    if [[ "$_RYEOS_TERM_ACTIVE" == 1 && "$_RYEOS_TERM_MODE" == tty ]]; then
        printf '\r\033[2K' >&2
    fi
    _RYEOS_TERM_ACTIVE=0
}

ryeos_term_cleanup() {
    _ryeos_term_clear
    _RYEOS_TERM_SUSPENDED=0
}

ryeos_term_suspend() {
    local was_active="$_RYEOS_TERM_ACTIVE"
    _ryeos_term_clear
    if [[ "$was_active" == 1 || "$_RYEOS_TERM_SUSPENDED" == 1 ]]; then
        _RYEOS_TERM_SUSPENDED=1
    else
        _RYEOS_TERM_SUSPENDED=0
    fi
}

ryeos_term_resume() {
    local detail="${1:-}"
    if [[ "$_RYEOS_TERM_SUSPENDED" == 1 ]]; then
        _RYEOS_TERM_SUSPENDED=0
        ryeos_term_update "$_RYEOS_TERM_LABEL" "$detail"
    fi
}

_ryeos_term_exit_trap() {
    local status="$1"
    if (( status != 0 )) && { [[ "$_RYEOS_TERM_ACTIVE" == 1 ]] || [[ "$_RYEOS_TERM_SUSPENDED" == 1 ]]; }; then
        ryeos_term_end failure "$_RYEOS_TERM_VERB FAILED" "exit status $status"
        _RYEOS_TERM_DEPTH=0
        _ryeos_term_clear
    else
        _ryeos_term_clear
    fi
    if [[ -n "$_RYEOS_TERM_PREV_EXIT" ]]; then
        # shellcheck disable=SC2294 # trap -p necessarily returns shell source.
        eval "$_RYEOS_TERM_PREV_EXIT"
    fi
    return "$status"
}

# Use this from a script-owned EXIT trap before its resource cleanup. It keeps
# the helper's phase-failure behavior even when that script must replace EXIT.
ryeos_term_handle_exit() {
    local status="$1"
    _ryeos_term_exit_trap "$status" || true
}

_ryeos_term_signal_trap() {
    local signal="$1" status="$2" previous="$3"
    _ryeos_term_clear
    trap - "$signal"
    if [[ -n "$previous" ]]; then
        # shellcheck disable=SC2294 # trap -p necessarily returns shell source.
        eval "$previous"
    fi
    exit "$status"
}

ryeos_term_init() {
    local override="${RYEOS_TTY:-auto}"
    case "$override" in
        always)
            if [[ "${TERM:-}" == dumb ]]; then
                _RYEOS_TERM_MODE="plain"
            else
                _RYEOS_TERM_MODE="tty"
            fi
            ;;
        never) _RYEOS_TERM_MODE="plain" ;;
        auto)
            if [[ -t 1 && -t 2 && "${TERM:-}" != dumb ]]; then
                _RYEOS_TERM_MODE="tty"
            else
                _RYEOS_TERM_MODE="plain"
            fi
            ;;
        *)
            if [[ -t 1 && -t 2 && "${TERM:-}" != dumb ]]; then
                _RYEOS_TERM_MODE="tty"
            else
                _RYEOS_TERM_MODE="plain"
            fi
            ;;
    esac
    if [[ "$_RYEOS_TERM_MODE" == tty ]]; then
        _RYEOS_TERM_UNICODE=1
        [[ -z "${NO_COLOR+x}" ]] && _RYEOS_TERM_COLOR=1 || _RYEOS_TERM_COLOR=0
    else
        _RYEOS_TERM_UNICODE=0
        _RYEOS_TERM_COLOR=0
    fi
    _ryeos_term_refresh_width
    if [[ "$_RYEOS_TERM_TRAPS" == 0 ]]; then
        _ryeos_term_capture_trap EXIT _RYEOS_TERM_PREV_EXIT
        _ryeos_term_capture_trap INT _RYEOS_TERM_PREV_INT
        _ryeos_term_capture_trap TERM _RYEOS_TERM_PREV_TERM
        trap '_ryeos_term_exit_trap "$?"' EXIT
        trap '_ryeos_term_signal_trap INT 130 "$_RYEOS_TERM_PREV_INT"' INT
        trap '_ryeos_term_signal_trap TERM 143 "$_RYEOS_TERM_PREV_TERM"' TERM
        _RYEOS_TERM_TRAPS=1
    fi
}

ryeos_term_is_tty() {
    [[ "$_RYEOS_TERM_MODE" == tty ]]
}

_ryeos_term_tone() {
    local code="$1" value="$2"
    if [[ "$_RYEOS_TERM_COLOR" == 1 ]]; then
        printf '\033[%sm%s\033[0m' "$code" "$value"
    else
        printf '%s' "$value"
    fi
}

_ryeos_term_clamp() {
    local value="$1" limit="$2" marker='...' marker_width=3
    local current_width=0 index=0 ch ch_code ch_width value_width
    (( limit < 1 )) && limit=1
    value_width="$(_ryeos_term_visible_width "$value")"
    if (( value_width <= limit )); then
        printf '%s' "$value"
        return
    fi
    if [[ "$_RYEOS_TERM_UNICODE" == 1 ]]; then
        marker='…'
        marker_width=1
    fi
    if (( limit <= marker_width )); then
        printf '%s' "${marker:0:limit}"
        return
    fi
    while (( index < ${#value} )); do
        ch="${value:index:1}"
        printf -v ch_code '%d' "'$ch"
        if (( ch_code <= 127 )); then ch_width=1; else ch_width=2; fi
        (( current_width + ch_width > limit - marker_width )) && break
        printf '%s' "$ch"
        current_width=$(( current_width + ch_width ))
        index=$(( index + 1 ))
    done
    printf '%s' "$marker"
}

_ryeos_term_visible_width() {
    local value="$1" width=0 index ch ch_code
    for (( index=0; index<${#value}; index++ )); do
        ch="${value:index:1}"
        printf -v ch_code '%d' "'$ch"
        if (( ch_code <= 127 )); then
            width=$(( width + 1 ))
        else
            width=$(( width + 2 ))
        fi
    done
    printf '%s' "$width"
}

_ryeos_term_restore_message_state() {
    local was_active="$1" was_suspended="$2"
    _RYEOS_TERM_SUSPENDED="$was_suspended"
    if [[ "$was_active" == 1 ]]; then
        if [[ "$_RYEOS_TERM_MODE" == tty ]]; then
            ryeos_term_update "$_RYEOS_TERM_LABEL"
        else
            _RYEOS_TERM_ACTIVE=1
        fi
    fi
}

_ryeos_term_glyph() {
    case "$1:$_RYEOS_TERM_UNICODE" in
        success:1) printf '◆' ;;
        warning:1) printf '▲' ;;
        failure:1) printf '✕' ;;
        active:1) printf '%s' "${2:-⠹}" ;;
        info:1) printf '•' ;;
        success:0) printf 'OK' ;;
        warning:0) printf 'WARN' ;;
        failure:0) printf 'ERROR' ;;
        active:0) printf '..' ;;
        *) printf '-' ;;
    esac
}

_ryeos_term_render_active() {
    local frame="${1:-⠹}" suffix="" message elapsed message_limit
    [[ -n "$_RYEOS_TERM_DETAIL" ]] && suffix="  ·  $_RYEOS_TERM_DETAIL"
    elapsed="$(_ryeos_term_elapsed)"
    message_limit=$(( _RYEOS_TERM_WIDTH - 25 - ${#elapsed} ))
    (( _RYEOS_TERM_WIDTH <= 30 )) && message_limit=$(( _RYEOS_TERM_WIDTH - 10 - ${#_RYEOS_TERM_VERB} ))
    message="$(_ryeos_term_clamp "$_RYEOS_TERM_LABEL$suffix" "$message_limit")"
    if [[ "$_RYEOS_TERM_WIDTH" -le 30 ]]; then
        printf '\r\033[2K%s RYEOS %s %s' \
            "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_ACTIVE" "$(_ryeos_term_glyph active "$frame")")" \
            "$_RYEOS_TERM_VERB" "$message" >&2
    else
        printf '\r\033[2K%s  RYEOS  %-7s  %s  ·  %s' \
            "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_ACTIVE" "$(_ryeos_term_glyph active "$frame")")" \
            "$_RYEOS_TERM_VERB" "$message" "$elapsed" >&2
    fi
}

_ryeos_term_start_spinner() {
    [[ "$_RYEOS_TERM_MODE" == tty && "$_RYEOS_TERM_ACTIVE" == 1 && "$_RYEOS_TERM_SUSPENDED" == 0 ]] || return 0
    _ryeos_term_stop_spinner
    (
        local frame_index=0 resize_pending=0
        local interval="${RYEOS_TERM_SPINNER_INTERVAL:-0.1}"
        local -a frames=(⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏)
        trap - EXIT INT TERM
        trap 'resize_pending=1' WINCH
        while :; do
            # A resize can interrupt sleep. Keep the renderer alive, refresh
            # from the terminal fd, and repaint within the new boundary.
            sleep "$interval" || true
            if [[ "$resize_pending" == 1 ]]; then
                _ryeos_term_refresh_width
                resize_pending=0
            fi
            _ryeos_term_render_active "${frames[$frame_index]}"
            frame_index=$(( (frame_index + 1) % ${#frames[@]} ))
        done
    ) &
    _RYEOS_TERM_SPINNER_PID=$!
}

ryeos_term_begin() {
    local verb="$1" label="$2" label_limit
    if [[ "$_RYEOS_TERM_ACTIVE" == 1 || "$_RYEOS_TERM_SUSPENDED" == 1 ]]; then
        _ryeos_term_clear
        _RYEOS_TERM_STACK_VERB[_RYEOS_TERM_DEPTH]="$_RYEOS_TERM_VERB"
        _RYEOS_TERM_STACK_LABEL[_RYEOS_TERM_DEPTH]="$_RYEOS_TERM_LABEL"
        _RYEOS_TERM_STACK_DETAIL[_RYEOS_TERM_DEPTH]="$_RYEOS_TERM_DETAIL"
        _RYEOS_TERM_STACK_STARTED[_RYEOS_TERM_DEPTH]="$_RYEOS_TERM_STARTED"
        _RYEOS_TERM_DEPTH=$(( _RYEOS_TERM_DEPTH + 1 ))
    fi
    _RYEOS_TERM_SUSPENDED=0
    _ryeos_term_refresh_width
    label_limit=$(( _RYEOS_TERM_WIDTH - 20 ))
    (( _RYEOS_TERM_WIDTH <= 30 )) && label_limit=$(( _RYEOS_TERM_WIDTH - 10 - ${#verb} ))
    label="$(_ryeos_term_clamp "$label" "$label_limit")"
    _RYEOS_TERM_VERB="$verb"
    _RYEOS_TERM_LABEL="$label"
    _RYEOS_TERM_DETAIL=""
    _RYEOS_TERM_STARTED="$(_ryeos_term_now)"
    _RYEOS_TERM_ACTIVE=1
    if [[ "$_RYEOS_TERM_MODE" == tty ]]; then
        _ryeos_term_render_active
        _ryeos_term_start_spinner
    else
        printf 'RYEOS %s %s\n' "$verb" "$label" >&2
    fi
}

ryeos_term_update() {
    local label="$1" detail="${2:-}" message message_limit suffix=""
    _ryeos_term_stop_spinner
    _ryeos_term_refresh_width
    _RYEOS_TERM_LABEL="$label"
    _RYEOS_TERM_DETAIL="$detail"
    if [[ "$_RYEOS_TERM_MODE" == tty ]]; then
        _RYEOS_TERM_ACTIVE=1
        _ryeos_term_render_active
        _ryeos_term_start_spinner
    else
        [[ -n "$detail" ]] && suffix="  ·  $detail"
        message_limit=$(( _RYEOS_TERM_WIDTH - 25 ))
        message="$(_ryeos_term_clamp "$label$suffix" "$message_limit")"
        printf 'RYEOS %s %s\n' "$_RYEOS_TERM_VERB" "$message" >&2
    fi
}

ryeos_term_end() {
    local status="$1" label="$2" detail="${3:-}" started="${4:-$_RYEOS_TERM_STARTED}"
    local tone glyph suffix="" message parent_index elapsed message_limit
    _ryeos_term_clear
    _RYEOS_TERM_SUSPENDED=0
    _ryeos_term_refresh_width
    [[ -n "$detail" ]] && suffix="  ·  $detail"
    elapsed="$(_ryeos_term_elapsed "$started")"
    message_limit=$(( _RYEOS_TERM_WIDTH - 16 - ${#elapsed} ))
    (( _RYEOS_TERM_WIDTH <= 30 )) && message_limit=$(( _RYEOS_TERM_WIDTH - 9 ))
    message="$(_ryeos_term_clamp "$label$suffix" "$message_limit")"
    case "$status" in
        success) tone="$_RYEOS_TERM_TONE_SUCCESS" ;;
        warning) tone="$_RYEOS_TERM_TONE_WARNING" ;;
        *) tone="$_RYEOS_TERM_TONE_FAILURE"; status=failure ;;
    esac
    glyph="$(_ryeos_term_tone "$tone" "$(_ryeos_term_glyph "$status")")"
    if [[ "$_RYEOS_TERM_WIDTH" -le 30 ]]; then
        printf '%s RYEOS %s\n' "$glyph" "$message" >&2
    else
        printf '%s  RYEOS  %s  ·  %s\n' "$glyph" "$message" "$elapsed" >&2
    fi
    if (( _RYEOS_TERM_DEPTH > 0 )); then
        parent_index=$(( _RYEOS_TERM_DEPTH - 1 ))
        _RYEOS_TERM_DEPTH="$parent_index"
        _RYEOS_TERM_VERB="${_RYEOS_TERM_STACK_VERB[$parent_index]}"
        _RYEOS_TERM_LABEL="${_RYEOS_TERM_STACK_LABEL[$parent_index]}"
        _RYEOS_TERM_DETAIL="${_RYEOS_TERM_STACK_DETAIL[$parent_index]}"
        _RYEOS_TERM_STARTED="${_RYEOS_TERM_STACK_STARTED[$parent_index]}"
        _RYEOS_TERM_SUSPENDED=0
        unset "_RYEOS_TERM_STACK_VERB[$parent_index]"
        unset "_RYEOS_TERM_STACK_LABEL[$parent_index]"
        unset "_RYEOS_TERM_STACK_DETAIL[$parent_index]"
        unset "_RYEOS_TERM_STACK_STARTED[$parent_index]"
        ryeos_term_update "$_RYEOS_TERM_LABEL" "resuming"
    fi
}

ryeos_term_section() {
    local heading
    _ryeos_term_clear
    heading="$(_ryeos_term_clamp "$1" "$_RYEOS_TERM_WIDTH")"
    printf '\n%s\n' "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_NEUTRAL" "$heading")"
}

ryeos_term_row() {
    local key value row key_width padding
    key="$(_ryeos_term_clamp "$1" 12)"
    value="$(_ryeos_term_clamp "$2" "$(( _RYEOS_TERM_WIDTH - 16 ))")"
    key_width="$(_ryeos_term_visible_width "$key")"
    padding=$(( 12 - key_width ))
    (( padding < 0 )) && padding=0
    printf -v row '  %s%*s %s' "$key" "$padding" '' "$value"
    printf '%s\n' "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_NEUTRAL" "$row")"
}

ryeos_term_info() {
    local message was_active="$_RYEOS_TERM_ACTIVE" was_suspended="$_RYEOS_TERM_SUSPENDED"
    _ryeos_term_clear
    message="$(_ryeos_term_clamp "$1" "$(( _RYEOS_TERM_WIDTH - 14 ))")"
    printf '%s  RYEOS  %s\n' \
        "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_NEUTRAL" "$(_ryeos_term_glyph info)")" \
        "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_NEUTRAL" "$message")" >&2
    _ryeos_term_restore_message_state "$was_active" "$was_suspended"
}

ryeos_term_note() {
    local message was_active="$_RYEOS_TERM_ACTIVE" was_suspended="$_RYEOS_TERM_SUSPENDED"
    _ryeos_term_clear
    message="$(_ryeos_term_clamp "$1" "$(( _RYEOS_TERM_WIDTH - 14 ))")"
    if [[ "$_RYEOS_TERM_COLOR" == 1 ]]; then
        message="$(_ryeos_term_tone "$_RYEOS_TERM_TONE_SECONDARY" "$message")"
    fi
    printf '%s  RYEOS  %s\n' \
        "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_NEUTRAL" "$(_ryeos_term_glyph info)")" \
        "$message" >&2
    _ryeos_term_restore_message_state "$was_active" "$was_suspended"
}

ryeos_term_warn() {
    local was_active="$_RYEOS_TERM_ACTIVE" was_suspended="$_RYEOS_TERM_SUSPENDED"
    _ryeos_term_clear
    printf '%s  RYEOS  %s\n' \
        "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_WARNING" "$(_ryeos_term_glyph warning)")" \
        "$(_ryeos_term_clamp "$1" "$(( _RYEOS_TERM_WIDTH - 14 ))")" >&2
    _ryeos_term_restore_message_state "$was_active" "$was_suspended"
}

ryeos_term_fail() {
    local was_active="$_RYEOS_TERM_ACTIVE" was_suspended="$_RYEOS_TERM_SUSPENDED"
    _ryeos_term_clear
    printf '%s  RYEOS  %s\n' \
        "$(_ryeos_term_tone "$_RYEOS_TERM_TONE_FAILURE" "$(_ryeos_term_glyph failure)")" \
        "$(_ryeos_term_clamp "$1" "$(( _RYEOS_TERM_WIDTH - 14 ))")" >&2
    _ryeos_term_restore_message_state "$was_active" "$was_suspended"
}

ryeos_term_run() {
    local verb="$1" label="$2" status
    shift 2
    [[ "${1:-}" == -- ]] && shift
    ryeos_term_begin "$verb" "$label"
    ryeos_term_suspend
    if "$@"; then
        status=0
        ryeos_term_end success "$verb COMPLETE" "$label"
    else
        status=$?
        ryeos_term_end failure "$verb FAILED" "$label"
    fi
    return "$status"
}
