#!/usr/bin/env bash
# ============================================================================
# install.sh — Secure installation of PGindexAI
#
# Prerequisites:
#   - PostgreSQL 16+ (psql binary + working cluster)
#   - Rust toolchain (cargo)
#   - sudo access (default admin connection: 'sudo -u postgres psql' peer auth
#     — or --pg-password PASS / PG_PASSWORD env var when the 'postgres' role
#     has a password and pg_hba.conf allows it)
#   - Laya decision engine (REQUIRED — configured BEFORE the target
#     database; when missing, install.sh offers to download the official
#     laya.cpp binary + Hugging Face model and deploys the hardened 'laya'
#     systemd service in the same run — and ABORTS if Laya is still
#     unavailable, so no database is created or attached without it)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/GuillaumeDiamant/PGindexAI/main/install.sh | bash
#   # or
#   ./install.sh [OPTIONS]
#
# Options:
#   --demo           Install the demo database 'pgindexai_demo' (schema + workload
#                    data) and point the daemon at it. This is the DEFAULT.
#   --no-demo        Skip the demo; the daemon manages an EXISTING database of
#                    yours instead (the installer grants it the needed rights).
#   --db NAME        Target database for the daemon (implies --no-demo). NAME must
#                    already exist and be a valid PostgreSQL identifier.
#   --pg-password PASS
#                   Connect to the instance as the 'postgres' role with a
#                   PASSWORD (scram-sha-256 / md5) instead of the default
#                   'sudo -u postgres' (peer). Prefer the PG_PASSWORD env var
#                   (not visible in ps / shell history). PGHOST / PGPORT select
#                   a TCP endpoint.
#   --test           Run the full test suite (cargo test --all-targets) BEFORE
#                    installing; a failing test aborts the installation.
#   --dry-run        Show the actions without applying them.
#   -h, --help       Show this help and exit.
#
# Guarantees:
#   - Dedicated NON-superuser PostgreSQL role (pgindexai) with PEER auth —
#     no password: a pg_ident map (daemon OS user → pgindexai)
#   - pg_hba: TARGETED line (local all pgindexai peer map=pgindexai_map),
#     never a 'local all all trust'
#   - shared_preload_libraries is MERGED, never overwritten
#   - 0600 config with no secret by default (password only for remote TCP)
#   - No eval, no command interpreted as a string
#   - Hardened systemd unit (MemoryMax, CPUQuota, NoNewPrivileges, …)
# ============================================================================
set -euo pipefail

# ─────────────────────────────────────────────
# Colors & logging
# ─────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[✓]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*" >&2; }
error() { echo -e "${RED}[✗]${NC} $*" >&2; }
step()  { echo -e "${CYAN}→${NC} $*"; }

# sudo_note "reason" — explains WHY a step needs sudo, in plain language
# (which system file is written, which service is touched, why the admin
# connection goes through 'sudo -u postgres'). Printed BEFORE the sudo
# call, so the user sees the reason — not just a password prompt.
# Goes to stdout like step()/info(): it is part of the install narrative.
# Only print it when sudo is actually involved — in PG_PASSWORD mode the
# psql calls do NOT go through sudo, so peer-mode notes are conditional.
sudo_note() { echo -e "  ${YELLOW}[sudo]${NC} $*"; }

# ─────────────────────────────────────────────
# Global variables
# ─────────────────────────────────────────────
DRY_RUN=false
RUN_TESTS=false       # --test: run the full test suite before installing
INSTALL_DEMO=true     # --demo (default) / --no-demo: demo DB or a pre-existing one
TARGET_DB="pgindexai_demo"  # database the daemon manages (validated identifier)
DB_CHOSEN=false       # true once the user explicitly fixed the target DB
POSTGRES_USER="${POSTGRES_USER:-postgres}"
PG_PASSWORD="${PG_PASSWORD:-}"   # optional — password for the postgres role
                                 # (used instead of 'sudo -u postgres' peer
                                 # connections; prefer the PG_PASSWORD env var)

# Target user: the one owning the daemon, the config and the repo.
# - script as non-root → the current user
# - script via sudo    → $SUDO_USER (otherwise: owner of the current directory)
# NB: under sudo, $HOME/$USER point to root — they must be re-derived.
if [[ "$(id -u)" -ne 0 ]]; then
    TARGET_USER="${USER:-$(whoami)}"
elif [[ -n "${SUDO_USER:-}" ]]; then
    TARGET_USER="$SUDO_USER"
else
    TARGET_USER="$(stat -c %U . 2>/dev/null || true)"
fi
if [[ -z "${TARGET_USER:-}" ]]; then
    error "Cannot determine the target user (USER / SUDO_USER missing)."
    exit 1
fi
TARGET_HOME="$(getent passwd "$TARGET_USER" 2>/dev/null | cut -d: -f6 || true)"

PGINDEXAI_DIR="${TARGET_HOME}/.local/share/pgindexai"
CONFIG_DIR="${TARGET_HOME}/.config/PGindexAI"
CONFIG_FILE="$CONFIG_DIR/PGindexAI.yaml"
BIN_PATH="/usr/local/bin/pgindexai"
SERVICE_NAME="pgindexai"
DEMO_LOG="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/demo_install.log"  # full raw psql output of the demo install — in the directory containing install.sh
LAYA_SERVICE_NAME="laya"   # Laya decision engine (REQUIRED — must be up before the database steps)
LAYA_PORT=8080
LAYA_BACKEND="${LAYA_BACKEND:-}"   # optional override: 'cuda12' | 'vulkan' (default: auto-detect)
PG_IDENT_MAP="pgindexai_map"  # pg_ident.conf map name (daemon OS user → pgindexai)
PG_CONF=""
PG_HBA=""
PG_IDENT=""
PG_SSL="off"

# ─────────────────────────────────────────────
# Official Laya channel (laya.cpp + Hugging Face model)
# ─────────────────────────────────────────────
# Laya is EXTERNAL: it is the official laya.cpp release (native C++ inference
# for Laya typed decisions — ggml, CUDA/Vulkan/CoreML/CPU backends) plus the
# 'typed-decisions' model published on Hugging Face. Both are public,
# checksummed channels. These constants feed the runbook shown when Laya is
# missing and the optional in-place download (laya_install_official).
LAYA_UPSTREAM="lkarlslund/laya.cpp"
LAYA_RELEASE="r0004"
LAYA_MODEL_REPO="convaiinnovations/laya-typed-decisions"
# Official SHA-256 of the r0004 linux-amd64 assets (from the release
# sidecar JSONs — the 'laya-r0004-linux-amd64-<backend>.json' files):
LAYA_SHA256_CUDA12="ac747f10b86060eae948e51305a75f49619d288876d500e1113a779997c3169d"
LAYA_SHA256_VULKAN="c12aa6abd3d93b27670825157f9676b4f0038cf1326842d28c575a1a415b6b9c"
# Files of the 'typed-decisions' model, paths relative to the model dir.
# Single source of truth: the runbook (Option B), the in-place installer,
# the subdirectory creation and the "model complete" check all derive from
# this list, so they can no longer drift apart.
LAYA_MODEL_FILES=(
    config.json
    rl_agent_config.json
    model.safetensors
    encoder/config.json
    tokenizer/tokenizer.json
    tokenizer/tokenizer_config.json
)

# ─────────────────────────────────────────────
# Demo / target-database choice
# ─────────────────────────────────────────────
# validate_db_name "NAME" — accepts only a valid, UNQUOTED PostgreSQL
# identifier. The name is interpolated into SQL (CREATE DATABASE, GRANT …),
# so quotes, spaces and injection are ruled out by construction.
validate_db_name() {
    [[ "${1:-}" =~ ^[a-zA-Z_][a-zA-Z0-9_]*$ ]]
}

# prompt_target_db — reads an existing-database name from /dev/tty and
# re-asks until it is a valid identifier (EOF aborts the install).
prompt_target_db() {
    local name=""
    while true; do
        if ! IFS= read -r -p "Target database (must already exist): " name </dev/tty; then
            error "No database name given — custom mode cannot continue without one."
            error "Re-run with --db NAME (existing database) or --demo (demo database)."
            exit 1
        fi
        name="$(xargs <<<"$name" 2>/dev/null || true)"
        if validate_db_name "$name"; then
            echo "$name"
            return 0
        fi
        warn "'${name:-<empty>}' is not a valid PostgreSQL identifier (letters, digits, '_'; must not start with a digit)"
    done
}

# resolve_demo_choice — settles demo/custom mode.
# Flags (--demo / --no-demo / --db NAME) win; with no flag and a terminal,
# the installer asks (demo = default, i.e. the previous behavior).
resolve_demo_choice() {
    local has_tty=false
    if ( : </dev/tty ) 2>/dev/null; then
        has_tty=true
    fi
    if ! $DB_CHOSEN && $has_tty; then
        if ask "Install the demo database (schema + workload data) and attach the daemon to it? [Y/n]" Y; then
            INSTALL_DEMO=true
        else
            INSTALL_DEMO=false
            local db
            db="$(prompt_target_db)"
            TARGET_DB="$db"
            DB_CHOSEN=true
        fi
    fi
    if ! $INSTALL_DEMO && ! $DB_CHOSEN; then
        error "Custom mode (--no-demo) needs a target database."
        error "Re-run with --db NAME (an existing database) or --demo (demo database)."
        exit 1
    fi
    if $INSTALL_DEMO; then
        info "Mode: demo — the daemon will manage '$TARGET_DB'"
    else
        info "Mode: custom — the daemon will manage the existing database '$TARGET_DB'"
    fi
}

# ─────────────────────────────────────────────
# Argument parsing
# ─────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --demo) INSTALL_DEMO=true ;;
        --no-demo) INSTALL_DEMO=false ;;
        --db)
            if [[ $# -lt 2 || -z "${2:-}" || "${2:0:1}" == "-" ]]; then
                error "--db requires a database name (e.g. --db mydb)"
                exit 1
            fi
            if ! validate_db_name "$2"; then
                error "'$2' is not a valid PostgreSQL identifier (letters, digits, '_'; must not start with a digit)"
                exit 1
            fi
            TARGET_DB="$2"
            DB_CHOSEN=true
            INSTALL_DEMO=false
            shift
            ;;
        --db=*)
            if ! validate_db_name "${1#--db=}"; then
                error "'${1#--db=}' is not a valid PostgreSQL identifier (letters, digits, '_'; must not start with a digit)"
                exit 1
            fi
            TARGET_DB="${1#--db=}"
            DB_CHOSEN=true
            INSTALL_DEMO=false
            ;;
        --pg-password)
            if [[ $# -lt 2 || -z "${2:-}" || "${2:0:1}" == "-" ]]; then
                error "--pg-password requires a password"
                exit 1
            fi
            PG_PASSWORD="$2"
            shift
            ;;
        --pg-password=*)
            if [[ -z "${1#--pg-password=}" ]]; then
                error "--pg-password requires a password"
                exit 1
            fi
            PG_PASSWORD="${1#--pg-password=}"
            ;;
        --test) RUN_TESTS=true ;;
        --dry-run) DRY_RUN=true ;;
        -h|--help)
            sed -n '2,48p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) warn "Unknown argument: $1 (ignored)" ;;
    esac
    shift
done

# run CMD [ARGS...] — no eval: each argument is passed as-is
run() {
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} $*"
    else
        "$@"
    fi
}

# pg_psql [psql options...] — runs psql as role $POSTGRES_USER, choosing
# the connection mode:
#   PG_PASSWORD set → PGPASSWORD=… psql -U postgres (scram-sha-256/md5 —
#                     local socket by default; PGHOST/PGPORT for TCP)
#   otherwise       → sudo -u postgres psql (peer auth, no password)
# Every installer call goes through this one helper, so the connection
# mode switches everywhere at once.
pg_psql() {
    if [[ -n "$PG_PASSWORD" ]]; then
        PGPASSWORD="$PG_PASSWORD" psql -U "$POSTGRES_USER" "$@"
    else
        sudo -u "$POSTGRES_USER" psql "$@"
    fi
}

# psql_stdin "SQL" [psql options...] — runs the SQL via STDIN, not as
# a command argument: a sensitive value (password) never goes
# through the local process list (`ps`) EVER.
# (The old `psql -c "…PASSWORD '…'"` remained visible during execution;
# the dry-run even displayed it on screen/in the log.)
psql_stdin() {
    local sql="$1"
    shift
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} pg_psql -q -v ON_ERROR_STOP=1 $* [SQL via stdin]"
    else
        printf '%s\n' "$sql" | pg_psql -q -v ON_ERROR_STOP=1 "$@"
    fi
}

# demo_psql "LABEL" SQL_FILE — runs a demo SQL file with a strict
# screen/log split:
#   * log    → the FULL raw psql output (every command tag — CREATE
#              TABLE, INSERT 0 N, DO, … — and all server messages) is
#              teed to $DEMO_LOG — strictly more detailed than the screen;
#   * screen → only the curated NOTICE/WARNING/ERROR lines (per-step
#              explanations + digests), the server prefix stripped; while
#              psql is silent (e.g. the long [8/9] phase) an ASCII activity
#              spinner (|/-\ + a braced {mm:ss} badge on its own tabstop)
#              keeps ticking so the run visibly works — it never reaches
#              the log, and only shows when stdout is a terminal.
# The connection mirrors pg_psql (password vs. peer mode). `stdbuf -oL`
# (coreutils, optional) keeps psql's stdout line-buffered so the raw log
# interleaves with the server messages in execution order.
# On failure, the tail of the raw output is shown for diagnosis.
demo_psql() {
    local label="$1" file="$2" log_dir
    local -a cmd
    if command -v stdbuf >/dev/null 2>&1; then
        if [[ -n "$PG_PASSWORD" ]]; then
            cmd=(env PGPASSWORD="$PG_PASSWORD" stdbuf -oL psql -U "$POSTGRES_USER")
        else
            cmd=(sudo -u "$POSTGRES_USER" stdbuf -oL psql)
        fi
    else
        if [[ -n "$PG_PASSWORD" ]]; then
            cmd=(env PGPASSWORD="$PG_PASSWORD" psql -U "$POSTGRES_USER")
        else
            cmd=(sudo -u "$POSTGRES_USER" psql)
        fi
    fi
    if $DRY_RUN; then
        # Display only — the password value itself is never echoed.
        if [[ -n "$PG_PASSWORD" ]]; then
            echo -e "${CYAN}[dry-run]${NC} psql -U $POSTGRES_USER -v ON_ERROR_STOP=1 -d $TARGET_DB < $file   (password mode)"
        else
            echo -e "${CYAN}[dry-run]${NC} sudo -u $POSTGRES_USER psql -v ON_ERROR_STOP=1 -d $TARGET_DB < $file   (peer mode)"
        fi
        echo -e "${CYAN}[dry-run]${NC} screen: NOTICE/WARNING/ERROR lines only (explanations + digests)"
        echo -e "${CYAN}[dry-run]${NC} log: full raw psql output -> $DEMO_LOG"
        return 0
    fi
    # Screen filter: server messages only (the curated NOTICEs, warnings,
    # errors), prefix stripped — see _demo_filter. It always exits 0, so
    # the pipeline status under `set -o pipefail` is psql's.
    local -a filt=(_demo_filter)
    # If the log file's directory is missing or not writable (e.g. a
    # read-only checkout), fall back to a screen-only run without capture.
    local use_tee=1
    log_dir="$(dirname "$DEMO_LOG")"
    if [[ ! -d "$log_dir" || ! -w "$log_dir" ]]; then
        warn "demo log directory not writable — skipping demo_install.log capture"
        use_tee=0
    fi
    if [[ $use_tee -eq 1 ]]; then
        printf '\n=== %s - %s ===\n' "$label" "$(date '+%Y-%m-%d %H:%M:%S')" >> "$DEMO_LOG"
    fi

    # ── No terminal: plain streaming (a spinner would corrupt piped output) ──
    if [[ ! -t 1 ]]; then
        if [[ $use_tee -eq 1 ]]; then
            if "${cmd[@]}" -v ON_ERROR_STOP=1 -d "$TARGET_DB" < "$file" 2>&1 | tee -a "$DEMO_LOG" | "${filt[@]}"; then
                return 0
            fi
        else
            if "${cmd[@]}" -v ON_ERROR_STOP=1 -d "$TARGET_DB" < "$file" 2>&1 | "${filt[@]}"; then
                return 0
            fi
        fi
        warn "psql failed ($label) — last 20 lines of the raw output:"
        if [[ $use_tee -eq 1 ]]; then
            tail -n 20 "$DEMO_LOG"
        fi
        return 1
    fi

    # ── Terminal: the pipeline runs in the background; its filtered output
    # flows through a FIFO that this foreground process consumes line by
    # line, drawing the activity spinner (|/-\ + a braced {mm:ss} badge)
    # in the gaps. One process owns the screen, so the spinner never
    # interleaves with the output lines — and it never reaches
    # demo_install.log.
    local tmpd fifo rcfile pipe_pid
    tmpd="$(mktemp -d "${TMPDIR:-/tmp}/pgindexai.XXXXXX")"
    fifo="$tmpd/screen"
    rcfile="$tmpd/rc"
    mkfifo "$fifo"
    # $tmpd is function-local and gone by the time the trap fires, so the
    # path MUST be expanded now, not at signal time.
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpd' 2>/dev/null || true" EXIT
    if [[ $use_tee -eq 1 ]]; then
        (
            rc=0
            "${cmd[@]}" -v ON_ERROR_STOP=1 -d "$TARGET_DB" < "$file" 2>&1 | tee -a "$DEMO_LOG" | "${filt[@]}" > "$fifo" || rc=$?
            printf '%s\n' "$rc" > "$rcfile"
        ) &
    else
        (
            rc=0
            "${cmd[@]}" -v ON_ERROR_STOP=1 -d "$TARGET_DB" < "$file" 2>&1 | "${filt[@]}" > "$fifo" || rc=$?
            printf '%s\n' "$rc" > "$rcfile"
        ) &
    fi
    pipe_pid=$!
    # Blocks until the background pipeline opens the FIFO for writing.
    exec 3< "$fifo"

    local frames='|/-\' idx=0 printed=0 line
    local t0
    t0="$(date +%s)"
    while :; do
        if IFS= read -r -t 0.5 line <&3; then
            if (( printed )); then
                printf '\r\033[K'
                printed=0
            fi
            printf '%s\n' "$line"
            continue
        fi
        # No line in the last 0.5s — pipeline finished, or just silent?
        if [[ -f "$rcfile" ]]; then
            break
        fi
        idx=$(( (idx + 1) % 4 ))
        # The elapsed badge sits on its own tabstop, wrapped in braces, so
        # the in-place re-draw never visually merges the counter with the
        # step label or with the next output line (narrow terminals,
        # incomplete erase-to-end-of-line support).
        printf '\r\033[K  %s %s...\t{ %s }' "${frames:idx:1}" "$label" "$(_demo_mmss $(( $(date +%s) - t0 )))"
        printed=1
    done
    # Writer is done: drain whatever is left in the FIFO. Clear the
    # spinner first so a leftover line can never land on top of it.
    if (( printed )); then
        printf '\r\033[K'
        printed=0
    fi
    while IFS= read -r line <&3; do
        printf '%s\n' "$line"
    done
    exec 3<&-
    if (( printed )); then
        printf '\r\033[K'
    fi
    wait "$pipe_pid" 2>/dev/null || true
    local rc
    rc="$(cat "$rcfile" 2>/dev/null || echo 1)"
    rm -rf "$tmpd"
    trap - EXIT
    if [[ "$rc" == "0" ]]; then
        return 0
    fi
    # `set -o pipefail`: a non-zero pipeline status means psql itself failed.
    warn "psql failed ($label) — last 20 lines of the raw output:"
    if [[ $use_tee -eq 1 ]]; then
        tail -n 20 "$DEMO_LOG"
    fi
    return 1
}

# _demo_filter — pass through only NOTICE/WARNING/ERROR server messages,
# prefix stripped. A bash read/printf loop on purpose: sed/awk buffer a
# non-tty stdout until EOF (every line would arrive at the END of the run,
# killing the live output); bash printf is an unbuffered write(2), so the
# output flows line by line. Always exits 0 → under `set -o pipefail` the
# pipeline status is psql's.
_demo_filter() {
    local line out
    while IFS= read -r line; do
        case "$line" in
            NOTICE:*)   out="${line#NOTICE:}" ;;
            WARNING:*)  out="${line#WARNING:}" ;;
            ERROR:*)    out="${line#ERROR:}" ;;
            *)          continue ;;
        esac
        out="${out#"${out%%[![:space:]]*}"}"
        printf '%s\n' "$out"
    done
}

# _demo_mmss SECONDS — format a duration as M:SS (activity spinner).
_demo_mmss() {
    printf '%d:%02d' $(( $1 / 60 )) $(( $1 % 60 ))
}

# ask "question" [default Y|N] — reads /dev/tty (works even via curl | bash),
# falls back to the default value if no controlling terminal is available.
# NB: NEVER redirect the stderr of `read`: `read -p` writes the prompt to it,
# and 2>/dev/null would make the question invisible to the user.
ask() {
    local prompt="$1" def="${2:-Y}" answer=""
    if ! ( : </dev/tty ) 2>/dev/null; then
        [[ "${def:0:1}" == [yY] ]]
        return
    fi
    IFS= read -r -p "$prompt " answer </dev/tty || answer=""
    [[ "${answer:-$def}" =~ ^[Yy] ]]
}

# pg_show "SQL" — runs a query with -tAc as $POSTGRES_USER (connection
# mode: see pg_psql — sudo peer or password). Returns an empty string on
# failure.
pg_show() {
    pg_psql -tAc "$1" 2>/dev/null || true
}

# ─────────────────────────────────────────────
# Prerequisites
# ─────────────────────────────────────────────
check_sudo() {
    step "Checking sudo access..."
    sudo_note "sudo will be used to write the PostgreSQL config files, manage the 'laya' and 'pgindexai' systemd services, install the binary to /usr/local/bin — and (peer mode) to run admin queries as the 'postgres' role"
    if sudo -n true 2>/dev/null; then
        info "sudo available (without password)"
    else
        warn "sudo required — the script will ask you for your password"
    fi
}

check_postgres() {
    step "Checking PostgreSQL..."
    if [[ -z "$PG_PASSWORD" ]]; then
        sudo_note "admin queries run as 'sudo -u $POSTGRES_USER psql' (peer auth — the 'postgres' OS user is allowed the '$POSTGRES_USER' role, no password)"
    fi
    if ! command -v psql &>/dev/null; then
        error "psql not found — install PostgreSQL 16+"
        exit 1
    fi
    local version
    version="$(psql --version 2>/dev/null | grep -oE '[0-9]+' | head -1 || echo 0)"
    if [[ "${version:-0}" -ge 16 ]]; then
        info "PostgreSQL $version detected"
    else
        warn "PostgreSQL 16+ required (found version ${version:-unknown})"
    fi

    # Fail-fast connection probe: EVERYTHING downstream (role, grants,
    # extensions, demo) needs a working admin connection, so a broken one
    # aborts BEFORE any modification — with a mode-specific diagnosis.
    local probe
    if ! probe="$(pg_psql -tAc "SELECT 1" 2>&1)"; then
        if [[ -n "$PG_PASSWORD" ]]; then
            error "Cannot connect to PostgreSQL as role '$POSTGRES_USER' with the provided password."
            error "Check: the password is correct for role '$POSTGRES_USER';"
            error "pg_hba.conf allows 'scram-sha-256' (or 'md5') for this role"
            error "(local socket by default — set PGHOST / PGPORT to use TCP)."
        else
            error "Cannot connect to PostgreSQL as role '$POSTGRES_USER' ('sudo -u $POSTGRES_USER psql' failed)."
            error "Check: 'sudo -u $POSTGRES_USER psql' works on this machine;"
            error "or re-run with --pg-password PASS / PG_PASSWORD=… if the role"
            error "has a password and pg_hba.conf allows it (scram-sha-256 / md5)."
        fi
        if [[ -n "$probe" ]]; then
            error "Detail: $(head -c 200 <<<"$probe" | tr '\n' ' ')"
        fi
        exit 1
    fi
    if [[ -n "$PG_PASSWORD" ]]; then
        info "Admin connection OK (password — role '$POSTGRES_USER')"
    else
        info "Admin connection OK (sudo peer — 'sudo -u $POSTGRES_USER')"
    fi

    # Detecting config files via the server itself (no hardcoded path)
    PG_CONF="$(pg_show "SHOW config_file" | tr -d '[:space:]')"
    PG_HBA="$(pg_show "SHOW hba_file" | tr -d '[:space:]')"
    PG_SSL="$(pg_show "SHOW ssl" | tr -d '[:space:]')"
    if [[ -n "$PG_CONF" ]]; then
        info "postgresql.conf : $PG_CONF"
        info "pg_hba.conf     : ${PG_HBA:-?}"
        info "ssl             : ${PG_SSL:-?}"
    else
        warn "Cannot read the server state."
        warn "Automatic configuration will be limited."
    fi
}

check_rust() {
    step "Checking Rust..."
    # cargo may live in the target user's home (missing from root's PATH)
    local cargo_bin=""
    if command -v cargo &>/dev/null; then
        cargo_bin="$(command -v cargo)"
    elif [[ -n "${TARGET_HOME:-}" && -x "${TARGET_HOME}/.cargo/bin/cargo" ]]; then
        cargo_bin="${TARGET_HOME}/.cargo/bin/cargo"
        export PATH="${TARGET_HOME}/.cargo/bin:${PATH}"
    fi
    if [[ -n "$cargo_bin" ]]; then
        info "Rust toolchain detected ($cargo_bin)"
        return 0
    fi

    if ask "Rust not found. Install via rustup? [Y/n]" Y; then
        local rustup_script
        rustup_script="$(mktemp)"
        run curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$rustup_script"
        # Install FOR the target user (not root): the daemon runs under this account.
        # NB: the « -s » flag no longer exists in recent rustup-init → « -y » suffices.
        if [[ "$(id -u)" -eq 0 ]]; then
            sudo_note "running as root — rustup is installed for '$TARGET_USER' via 'sudo -u $TARGET_USER' (the daemon runs under that account)"
            run sudo -u "$TARGET_USER" sh "$rustup_script" -y --default-toolchain stable
        else
            run sh "$rustup_script" -y --default-toolchain stable
        fi
        run rm -f "$rustup_script"
        if [[ -n "${TARGET_HOME:-}" && -x "${TARGET_HOME}/.cargo/bin/cargo" ]]; then
            export PATH="${TARGET_HOME}/.cargo/bin:${PATH}"
        else
            source "$HOME/.cargo/env" 2>/dev/null || true
        fi
        info "rustup installed (for user $TARGET_USER)"
    else
        error "Rust is required to compile pgindexai"
        exit 1
    fi
}

# ─────────────────────────────────────────────
# Step 1: PostgreSQL (dedicated role, targeted hba, pg_stat_statements)
# ─────────────────────────────────────────────
# ensure_target_db — demo mode: create '$TARGET_DB' if missing (previous
# behavior). Custom mode: '$TARGET_DB' MUST already exist — we attach the
# daemon to the user's database, we never create it implicitly.
ensure_target_db() {
    local present
    present="$(pg_show "SELECT 1 FROM pg_database WHERE datname = '$TARGET_DB'")"
    if [[ -n "$present" ]]; then
        if $INSTALL_DEMO; then
            info "Database '$TARGET_DB' present"
        else
            info "Using existing database '$TARGET_DB'"
        fi
        return
    fi
    if $INSTALL_DEMO; then
        step "Creating demo database '$TARGET_DB'..."
        run pg_psql -qc "CREATE DATABASE $TARGET_DB"
    else
        error "Database '$TARGET_DB' does not exist — custom mode requires a pre-existing database."
        error "Create it first (CREATE DATABASE $TARGET_DB;) or re-run the installer with --demo."
        exit 1
    fi
}

# Ownership transfer of the target-database objects to pgindexai (DEMO MODE):
# CREATE INDEX / DROP INDEX / REINDEX require ownership of the table/index.
# NB: 'pg_toast' is EXCLUDED — TOAST indexes are system catalogs;
# an ALTER ... OWNER on them fails and ABORTs the whole DO block, canceling
# the whole transfer (without this exclusion, the transfer failed as soon as a
# table had a TOAST column: text/varchar/jsonb…).
transfer_ownership() {
    step "Transferring ownership of '$TARGET_DB' objects to 'pgindexai'..."
    if [[ -z "$PG_PASSWORD" ]]; then
        sudo_note "the demo objects are owned by the '$POSTGRES_USER' superuser — re-attributing them to 'pgindexai' needs superuser rights, hence 'sudo -u $POSTGRES_USER psql' (peer auth)"
    fi
    local sql
    sql="
DO \$\$
DECLARE r RECORD;
BEGIN
    FOR r IN
        SELECT c.oid::regclass AS cls
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.relkind IN ('r','v','m')
          AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')
          AND c.relowner <> (SELECT oid FROM pg_authid WHERE rolname = 'pgindexai')
    LOOP
        EXECUTE format('ALTER TABLE %s OWNER TO pgindexai', r.cls);
    END LOOP;
    FOR r IN
        SELECT ic.oid::regclass AS cls
        FROM pg_class ic
        JOIN pg_namespace n ON n.oid = ic.relnamespace
        WHERE ic.relkind = 'i'
          AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')
          AND ic.relowner <> (SELECT oid FROM pg_authid WHERE rolname = 'pgindexai')
    LOOP
        EXECUTE format('ALTER INDEX %s OWNER TO pgindexai', r.cls);
    END LOOP;
END
\$\$;
"
    if $DRY_RUN; then
        run pg_psql -d "$TARGET_DB" -qc "$sql"
        return 0
    fi
    local err
    if ! err="$(pg_psql -d "$TARGET_DB" -qc "$sql" 2>&1 >/dev/null)"; then
        error "Ownership transfer FAILED — the daemon will NOT be able to create/drop indexes until the '$TARGET_DB' objects are owned by 'pgindexai'."
        if [[ -n "$err" ]]; then
            error "Detail: $(head -c 300 <<<"$err" | tr '\n' ' ')"
        fi
        return 1
    fi
    info "Ownership applied (tables + indexes of '$TARGET_DB' → pgindexai)"
}

setup_role() {
    step "Configuring the dedicated 'pgindexai' role (non-superuser, peer auth)..."
    if [[ -z "$PG_PASSWORD" ]]; then
        sudo_note "the role is created and granted via 'sudo -u $POSTGRES_USER psql' (peer auth — creating roles and granting system roles needs a superuser)"
    fi

    local role_exists
    role_exists="$(pg_show "SELECT 1 FROM pg_roles WHERE rolname = 'pgindexai'")"

    if [[ -n "$role_exists" ]]; then
        info "Role 'pgindexai' already exists"
    else
        # NO PASSWORD: local authentication goes through peer + pg_ident map
        # (ensure_pg_ident / ensure_hba run BEFORE this step in setup_postgresql).
        # A password can still be set manually for remote TCP (scram-sha-256).
        psql_stdin "CREATE ROLE pgindexai LOGIN"
        info "Role 'pgindexai' created (no password — peer auth)"
    fi

    # Migration from the old password (scram-sha-256) setup: the role still
    # holds a password. Clear it — but only AFTER the peer hba line + ident map
    # are in place AND a REAL peer connection as TARGET_USER is verified.
    # (Clearing it first would remove the only working auth method.)
    local has_pw
    has_pw="$(pg_show "SELECT 1 FROM pg_authid WHERE rolname = 'pgindexai' AND rolpassword IS NOT NULL")"
    if [[ -n "$has_pw" ]]; then
        if $DRY_RUN; then
            echo -e "  ${CYAN}(dry-run)${NC} verify peer auth as '$TARGET_USER' + ALTER ROLE pgindexai PASSWORD NULL"
        else
            sudo_note "verifying the daemon's own login first: 'sudo -u $TARGET_USER psql' as role 'pgindexai' — the legacy password is removed only if this peer connection works"
            local peer_check
            peer_check="$(sudo -u "$TARGET_USER" psql -U pgindexai -d "$TARGET_DB" -tAc "SELECT 1" 2>&1 || true)"
            if [[ "$peer_check" == "1" ]]; then
                psql_stdin "ALTER ROLE pgindexai PASSWORD NULL"
                info "Password removed from 'pgindexai' (peer auth verified for '$TARGET_USER')"
            else
                error "Peer connection as '$TARGET_USER' → 'pgindexai' FAILED — the role password is KEPT."
                error "Check pg_ident.conf (map '$PG_IDENT_MAP') and the pg_hba peer line, then re-run."
                if [[ -n "$peer_check" ]]; then
                    error "Detail: $(head -c 200 <<<"$peer_check" | tr '\n' ' ')"
                fi
                return 1
            fi
        fi
    fi

    run pg_psql -qc "GRANT CONNECT ON DATABASE $TARGET_DB TO pgindexai"
    # PostgreSQL 15+: CREATE is no longer granted to everyone on the public schema
    # by default — without this GRANT, CREATE/DROP INDEX fails (permission denied
    # for schema public) even if pgindexai owns the tables.
    # Custom mode: these GRANTs are the daemon's access to the user's database —
    # they are the "additional rights" requested for pre-existing databases.
    run pg_psql -d "$TARGET_DB" -qc "GRANT CREATE ON SCHEMA public TO pgindexai"
    # Least privilege for the physical bloat criterion: since pgstattuple 1.5,
    # pgstatindex() (full index scan) is restricted to SUPERUSER or members of
    # the predefined role pg_stat_scan_tables. Grant the narrow role — NOT
    # superuser. Idempotent; without it the daemon degrades gracefully
    # (legacy bloat heuristic + a WARN per cycle, no DDL impact).
    run pg_psql -d "$TARGET_DB" -qc "GRANT pg_stat_scan_tables TO pgindexai"
    # Full visibility of the workload in pg_stat_statements: the extension
    # itself HIDES the query text (and queryid) of statements executed by
    # ANOTHER role unless the viewer is a superuser or a member of the
    # predefined role pg_read_all_stats (PG16 pg_stat_statements.c:
    # is_allowed_role = has_privs_of_role(userid, ROLE_PG_READ_ALL_STATS)) —
    # those rows then show up as the literal '<insufficient privilege>'
    # placeholder. Without this grant, a workload run by any other role
    # (e.g. postgres) is invisible to the daemon → 0 index proposals.
    # pg_read_all_stats is a read-only predefined role (pg_stat_* views):
    # the least privilege that gives full query-text visibility.
    run pg_psql -d "$TARGET_DB" -qc "GRANT pg_read_all_stats TO pgindexai"

    # Ownership transfer (tables + indexes) to pgindexai — DEMO MODE ONLY:
    # the demo schema is created BY postgres and must be re-attributed so the
    # daemon can execute DDL on it (otherwise: permission denied on CREATE/DROP
    # INDEX). Custom mode: the pre-existing objects stay owned by their owner
    # (we never steal ownership of user data) — the daemon relies on the
    # GRANTs above instead. NB (demo): re-applied after setup_demo in main().
    if $INSTALL_DEMO; then
        transfer_ownership
    fi
}

ensure_hba() {
    step "pg_hba.conf: targeted line for pgindexai (peer)..."
    sudo_note "pg_hba.conf is a root-owned system file — it is read/edited with sudo, then PostgreSQL is reloaded with sudo"
    local hba="${PG_HBA:-/etc/postgresql/16/main/pg_hba.conf}"
    # pg_hba.conf: TYPE  DATABASE  USER  METHOD — the daemon connects to its
    # target database '$TARGET_DB' (not a database « pgindexai ») → DATABASE = « all ».
    # peer + map: ONLY the mapped OS user (TARGET_USER) can log in as pgindexai.
    local line="local   all   pgindexai   peer   map=${PG_IDENT_MAP}"

    if [[ ! -f "$hba" ]]; then
        warn "pg_hba not found at '$hba' — manual addition required: $line"
        return 1
    fi

    local hba_modified=false

    if sudo grep -Eq "^[[:space:]]*local[[:space:]]+all[[:space:]]+pgindexai[[:space:]]+peer[[:space:]]+map=${PG_IDENT_MAP}[[:space:]]*\$" "$hba"; then
        info "Peer line for pgindexai already present"
    elif sudo grep -Eq "^[[:space:]]*local[[:space:]]+(all|pgindexai_demo)[[:space:]]+pgindexai[[:space:]]+(scram-sha-256|md5)[[:space:]]*\$" "$hba"; then
        # Migration from the old scram-sha-256 / md5 line → peer + map.
        warn "Existing password-auth line (scram/md5) detected — replacing with peer..."
        # NB: '#' delimiter — the ERE alternation '|' must not collide with
        # the sed delimiter (a '|' delimiter breaks the s command).
        run sudo sed -i -E "s#^([[:space:]]*)local[[:space:]]+(all|pgindexai_demo)[[:space:]]+pgindexai[[:space:]]+(scram-sha-256|md5)[[:space:]]*\$#\1${line}#" "$hba"
        hba_modified=true
        info "Line replaced: $line"
    elif sudo grep -Eq "^[[:space:]]*local[[:space:]]+pgindexai[[:space:]]+pgindexai[[:space:]]+" "$hba"; then
        # Defective variant: « local pgindexai pgindexai » puts pgindexai in
        # the DATABASE column → the daemon (database pgindexai_demo) never matches
        # → « no pg_hba.conf entry for host [local] ».
        warn "Malformed line (DATABASE=pgindexai) detected — correcting to peer form..."
        run sudo sed -i -E "s|^([[:space:]]*)local[[:space:]]+pgindexai[[:space:]]+pgindexai([[:space:]]+.*)\$|\1${line}|" "$hba"
        hba_modified=true
        info "Line corrected: $line"
    else
        # pg_hba is « first match wins »: the dedicated line is inserted AT THE TOP
        # so it takes precedence over any broad line ('local all all ...').
        local old_content
        if $DRY_RUN; then
            old_content="<current content of pg_hba.conf>"
        else
            old_content="$(sudo cat "$hba")"
        fi
        run sudo tee "$hba" >/dev/null <<EOF
$line

$old_content
EOF
        if $DRY_RUN; then
            echo -e "  ${CYAN}(dry-run: the line would be inserted at the top of pg_hba.conf)${NC}"
        else
            info "Line inserted at the top of pg_hba.conf (first-match priority)"
        fi
        hba_modified=true
    fi

    # pg_hba.conf is reloaded on SIGHUP (reload) — NOT on every connection.
    if $hba_modified; then
        run sudo systemctl reload postgresql
        info "PostgreSQL reloaded (pg_hba.conf applied)"
    fi

    if sudo grep -Eq "^[[:space:]]*local[[:space:]]+all[[:space:]]+all[[:space:]]+trust" "$hba"; then
        warn "Warning: 'local all all trust' is present in pg_hba.conf."
        warn "The line dedicated to pgindexai precedes it (so it is applied), but remove the"
        warn "trust line to harden the cluster (all the other roles are affected)."
    fi
}

ensure_pg_ident() {
    step "pg_ident.conf: peer map for pgindexai (${PG_IDENT_MAP}: ${TARGET_USER} → pgindexai)..."
    sudo_note "pg_ident.conf is a root-owned system file — it is read/edited with sudo, then PostgreSQL is reloaded with sudo"
    local ident="${PG_IDENT:-/etc/postgresql/16/main/pg_ident.conf}"
    # pg_ident.conf: MAPNAME   SYSTEM-USERNAME   PG-USERNAME
    local line="$PG_IDENT_MAP   $TARGET_USER   pgindexai"

    if [[ ! -f "$ident" ]]; then
        warn "pg_ident.conf not found at '$ident' — manual addition required: $line"
        return 1
    fi

    local modified=false

    if sudo grep -Eq "^[[:space:]]*${PG_IDENT_MAP}[[:space:]]+${TARGET_USER}[[:space:]]+pgindexai[[:space:]]*\$" "$ident"; then
        info "Ident map already present"
    elif sudo grep -Eq "^[[:space:]]*${PG_IDENT_MAP}[[:space:]]" "$ident"; then
        # Map name already declared with different content → replace its line.
        warn "Existing map '${PG_IDENT_MAP}' does not map '${TARGET_USER} → pgindexai' — replacing..."
        run sudo sed -i -E "s|^([[:space:]]*)${PG_IDENT_MAP}[[:space:]]+.*|\1${line}|" "$ident"
        modified=true
        info "Map line replaced: $line"
    else
        # pg_ident.conf has NO first-match-wins semantics: map names are
        # unique, so appending is safe (unlike pg_hba.conf).
        run sudo bash -c "printf '\n# Added by pgindexai install.sh (peer auth)\n%s\n' \"$line\" >> \"$ident\""
        modified=true
        info "Ident map added: $line"
    fi

    # pg_ident.conf is applied on reload (same mechanism as pg_hba.conf).
    if $modified; then
        run sudo systemctl reload postgresql
        info "PostgreSQL reloaded (pg_ident.conf applied)"
    fi
}

ensure_pg_stat_statements() {
    step "Enabling pg_stat_statements..."
    sudo_note "if the extension is not loaded yet: shared_preload_libraries is merged into postgresql.conf (sudo) and PostgreSQL is restarted (sudo)"
    local lib="pg_stat_statements"

    # The extension is PER-DATABASE: check (and create) it in the daemon's
    # own database, where it will actually be queried.
    if [[ -n "$(pg_psql -d "$TARGET_DB" -tAc "SELECT 1 FROM pg_extension WHERE extname = '$lib'" 2>/dev/null || true)" ]]; then
        info "pg_stat_statements is active in '$TARGET_DB'"
        return 0
    fi

    # Current effective value
    local current
    current="$(pg_show "SHOW shared_preload_libraries" || echo "")"
    if echo " $current " | tr ',' ' ' | tr -s ' ' | grep -qw "$lib"; then
        info "Already in shared_preload_libraries (a restart may suffice)"
    fi

    local conf="${PG_CONF:-/etc/postgresql/16/main/postgresql.conf}"
    if [[ -z "$PG_CONF" || ! -r "$conf" ]]; then
        warn "Cannot modify '$conf' automatically."
        warn "Manual action: shared_preload_libraries += '$lib', restart PG,"
        warn "then CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"
        return 1
    fi

    local line
    line="$(grep -E "^[[:space:]]*shared_preload_libraries[[:space:]]*=" "$conf" | tail -n1 || true)"

    if grep -qw "$lib" <<<"$line"; then
        info "Already declared in $conf (restart required)"
    elif [[ -n "$line" ]]; then
        # MERGE: we keep the existing libraries, we add ours
        local val newval
        val="$(sed -E "s/^[[:space:]]*shared_preload_libraries[[:space:]]*=[[:space:]]*//" <<<"$line" \
               | sed -E "s/[[:space:]]*\$//" | sed -E "s/^['\"]//; s/['\"]\$//" | xargs)"
        newval="${val:+$val,}$lib"
        run sudo sed -i -E "s|^[[:space:]]*shared_preload_libraries[[:space:]]*=.*|shared_preload_libraries = '${newval}'|" "$conf"
        info "Merge: shared_preload_libraries = '${newval}'"
    else
        run sudo bash -c "printf '\n# Added by pgindexai install.sh\nshared_preload_libraries = '%s'\n' \"$lib\" >> \"$conf\""
        info "Added: shared_preload_libraries = '$lib'"
    fi

    # Restart if needed (pg_hba.conf, for its part, is reloaded by « reload » — handled in ensure_hba)
    if [[ -z "$(pg_psql -d "$TARGET_DB" -tAc "SELECT 1 FROM pg_extension WHERE extname = '$lib'" 2>/dev/null || true)" ]]; then
        warn "Restarting PostgreSQL (shared_preload_libraries modified)..."
        run sudo systemctl restart postgresql
        # Activity polling (30 s)
        for _ in {1..15}; do
            if [[ "$(pg_show "SELECT 1")" == "1" ]]; then
                break
            fi
            sleep 2
        done
        if [[ "$(pg_show "SELECT 1")" != "1" ]]; then
            error "PostgreSQL did not restart — check: sudo journalctl -u postgresql"
            return 1
        fi
        run pg_psql -d "$TARGET_DB" -qc "CREATE EXTENSION IF NOT EXISTS $lib"
        info "pg_stat_statements enabled"
    fi
}

setup_postgresql() {
    ensure_target_db
    # Order matters (scram → peer migration): 1. pg_ident map + 2. pg_hba
    # peer line (+ reload) FIRST, then 3. setup_role — it clears the role
    # password only AFTER a real peer connection as TARGET_USER is verified.
    ensure_pg_ident
    ensure_hba
    setup_role
    # pg_stat_statements is REQUIRED for missing-index (SARGable) detection.
    # If it cannot be enabled, the daemon still works (REINDEX + unused-index
    # analysis) — it announces DEGRADED mode every cycle — so the install is
    # NOT aborted. But the previous `|| true` hid the failure entirely
    # (review note L6): the user got a green install with half the daemon
    # silently disabled. Make the gap impossible to miss:
    if ! ensure_pg_stat_statements; then
        warn "────────────────────────────────────────────────────────────────"
        warn "pg_stat_statements is NOT active in '$TARGET_DB'."
        warn "The daemon will run in DEGRADED mode: missing-index (SARGable)"
        warn "detection is DISABLED — only REINDEX + unused-index analysis work."
        warn "To fix:  shared_preload_libraries += 'pg_stat_statements' in postgresql.conf"
        warn "          sudo systemctl restart postgresql"
        warn "          CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"
        warn "          then re-run ./install.sh (idempotent)"
        warn "────────────────────────────────────────────────────────────────"
    fi
}

# ─────────────────────────────────────────────
# Step 2: Compilation & installation
# ─────────────────────────────────────────────
build_and_install() {
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # Under root: compile AS the target user (cargo in their home, target/ in their name)
    local cargo_exe="cargo"
    if [[ "$(id -u)" -eq 0 ]]; then
        if command -v cargo &>/dev/null; then
            cargo_exe="$(command -v cargo)"
        elif [[ -n "${TARGET_HOME:-}" && -x "${TARGET_HOME}/.cargo/bin/cargo" ]]; then
            cargo_exe="${TARGET_HOME}/.cargo/bin/cargo"
        fi
    fi

    # --test gate: run the full suite BEFORE anything is installed; a failing
    # test aborts the installation (the binary is not copied, the service is
    # not started). Dry-run: the suite is echoed, not run (nothing is
    # verifiable without executing it).
    if $RUN_TESTS; then
        step "Running the test suite (cargo test --all-targets)..."
        local tests_ok=true
        if [[ "$(id -u)" -eq 0 ]]; then
            sudo_note "running as root — the test suite runs as '$TARGET_USER' via 'sudo -u $TARGET_USER' (cargo and target/ live under that account)"
            ( cd "$repo_dir" && run sudo -u "$TARGET_USER" "$cargo_exe" test --all-targets ) || tests_ok=false
        else
            ( cd "$repo_dir" && run cargo test --all-targets ) || tests_ok=false
        fi
        if ! $tests_ok; then
            error "Test suite FAILED — aborting the installation."
            error "Fix the failing tests, or re-run without --test to install anyway."
            exit 1
        fi
        info "Test suite OK"
        echo ""
    fi

    step "Compiling pgindexai..."
    if [[ "$(id -u)" -eq 0 ]]; then
        sudo_note "running as root — compiling as '$TARGET_USER' via 'sudo -u $TARGET_USER' (cargo and target/ live under that account)"
        ( cd "$repo_dir" && run sudo -u "$TARGET_USER" "$cargo_exe" build --release )
    else
        ( cd "$repo_dir" && run cargo build --release )
    fi
    info "Compilation finished"

    step "Installing the binary → $BIN_PATH"
    sudo_note "the binary goes to a system path (/usr/local/bin) — copying and chmodding it needs sudo"
    run sudo cp "$repo_dir/target/release/pgindexai" "$BIN_PATH"
    run sudo chmod 755 "$BIN_PATH"

    step "Creating directories..."
    run mkdir -p "$PGINDEXAI_DIR" "$CONFIG_DIR"
    sudo_note "/var/log/pgindexai is a system directory — created with sudo, then chowned to '$TARGET_USER' so the daemon can write its audit log there"
    run sudo mkdir -p /var/log/pgindexai
    # The log directory is created via sudo → it ends up root:root. The
    # daemon runs as $TARGET_USER (systemd User=) and must write its audit
    # log there — so the chown is UNCONDITIONAL. (The previous gate on the
    # installer being root was silently skipped when the installer ran as a
    # regular user with sudo, leaving the directory root-owned and the daemon
    # failing with EACCES: "Permission denied (os error 13)" →
    # "FILE LOGGING DISABLED until fixed".)
    run sudo chown "$TARGET_USER" "$PGINDEXAI_DIR" "$CONFIG_DIR" /var/log/pgindexai
    # Any pre-existing root-owned log file inside (e.g. left by a root run)
    # is re-attributed too, otherwise the daemon cannot open it for writing.
    run sudo find /var/log/pgindexai -maxdepth 1 -user root -exec chown "$TARGET_USER" {} +
    info "Directories ready"
}

# ─────────────────────────────────────────────
# Step 3: Configuration (0600)
# ─────────────────────────────────────────────
setup_config() {
    step "Generating the configuration..."

    if [[ -f "$CONFIG_FILE" ]]; then
        if ask "Existing configuration → $CONFIG_FILE. Keep it? [Y/n]" Y; then
            info "Configuration kept"
            # Ensure 0600, the target ownership — and migrate away from the
            # old password-based setup (peer auth replaces it; the key stays,
            # set to null, so remote TCP with scram remains possible).
            run chmod 600 "$CONFIG_FILE"
            if [[ "$(id -u)" -eq 0 ]]; then
                run chown "$TARGET_USER" "$CONFIG_FILE"
            fi
            if grep -Eq "^[[:space:]]*password:" "$CONFIG_FILE"; then
                run sed -i -E "s|^([[:space:]]*)password:.*|\1password: null   # peer auth (local socket) — set a value only for remote TCP with scram|" "$CONFIG_FILE"
                info "Configuration migrated: 'password' → null (peer auth)"
            fi
            return
        fi
        cp "$CONFIG_FILE" "${CONFIG_FILE}.bak"
        if [[ "$(id -u)" -eq 0 ]]; then
            run chown "$TARGET_USER" "${CONFIG_FILE}.bak"
        fi
        warn "Backup → ${CONFIG_FILE}.bak"
    fi

    # TLS: the generated config points to the local Unix socket — the daemon
    # enforces ssl_mode=disable on socket (src/db/mod.rs: TLS is negotiated
    # over TCP with a hostname, not over local IPC). Server-side ssl=on
    # (snakeoil by default) changes nothing for a local IPC connection.
    # For a remote TCP deployment: host = DNS HOSTNAME + ssl_mode
    # require (or verify-ca/verify-full) — rustls verifies chain + name
    # at the handshake and REJECTS IP literals (ServerName is
    # DNS-only): « 127.0.0.1 » would NOT work (src/db/mod.rs).
    local ssl_mode="disable"

    # REINDEX requires table ownership. Custom mode: the daemon manages a
    # pre-existing database whose objects it does NOT own (we never transfer
    # ownership of user data) → rebuild_fragmented is disabled; the daemon
    # still creates/drops indexes (CREATE on schema public is granted).
    local rebuild_fragmented="true"
    if ! $INSTALL_DEMO; then
        rebuild_fragmented="false"
        warn "Custom mode: 'rebuild_fragmented' will be set to false (REINDEX requires table ownership)."
    fi

    run mkdir -p "$CONFIG_DIR"

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} write $CONFIG_FILE (password: null — peer auth, ssl_mode: $ssl_mode)"
    else
        cat > "$CONFIG_FILE" <<EOF
# Configuration of the PGindexAI daemon
# Generated by install.sh at $(date '+%F %T')
# Permissions: 0600 (no secret by default: peer auth on the local socket)

postgresql:
  host: "/var/run/postgresql"   # Local Unix socket (TLS not applicable on socket → disable)
  port: 5432
  user: "pgindexai"
  database: "$TARGET_DB"
  password: null               # peer auth (pg_ident map) — set a value only for remote TCP with scram
  ssl_mode: "${ssl_mode}"
  # ssl_root_cert: "/etc/ssl/certs/your-ca.pem"   # Self-signed CA (TCP only: verify-ca/verify-full)
laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

# Control of DDL actions — set to false to disable an action
actions:
  create_index: true
  drop_unused: true
  rebuild_fragmented: ${rebuild_fragmented}

# Decision thresholds
thresholds:
  cycle_seconds: 60        # Analysis cycle frequency
  unused_minutes: 10080    # Observation window (7 days) before DROP of an unused index
  fragmentation_pct: 70    # % of fragmentation triggering a REINDEX CONCURRENTLY
  min_sargable_rows: 100   # Min. executions (pg_stat_statements.calls) for a SARGable index
  min_confidence: 0.1      # Minimum Laya confidence to apply a decision
                          # (calibrated on laya-typed-decisions: the model's
                          #  confidence head reports ~0.10-0.38 and never
                          #  reaches 0.7 — a 0.7 floor silently rejects
                          #  EVERY decision, leaving "Applied: 0" forever)
  max_proposals_per_cycle: 20  # Proposal budget per cycle

# Analysis scope (multi-schema)
analysis:
  # List of schemas to analyze.
  # - empty list = all non-system schemas (default)
  # - example: ["public", "billing"]
  schemas: []

# PostgreSQL connection pool
pg_pool:
  max_size: 8
  wait_timeout_ms: 30000

# Target density per column type (tuples/page targeted, index size estimation)
density_by_type:
  int4: 200
  int8: 200
  float8: 150
  varchar: 30
  text: 20
  date: 150
  timestamptz: 150
  uuid: 100
  bool: 300
  default_density: 50

# Logging
logging:
  level: "info"
  format: "json"

# History of index operations
index_logging:
  enabled: true
  verbosity: "minimal"   # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760  # rotation at 10 MB
  max_files: 5
EOF
        chmod 600 "$CONFIG_FILE"
        # Under root: the file written is root:root — give it back to the
        # target user, otherwise the daemon (which runs under that account) → EACCES (os error 13).
        if [[ "$(id -u)" -eq 0 ]]; then
            run chown "$TARGET_USER" "$CONFIG_FILE"
        fi
        info "Configuration → $CONFIG_FILE (0600)"
    fi
    warn "Check the file before starting the daemon (Laya URL, schemas, thresholds)"
}

# ─────────────────────────────────────────────
# Step 4: Hardened systemd service
# ─────────────────────────────────────────────
setup_systemd() {
    step "Installing the systemd service..."
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"

    if [[ -f "$service_file" ]]; then
        if ! ask "Existing service → $service_file. Replace it? [Y/n]" Y; then
            return
        fi
    fi

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} write $service_file (User=$TARGET_USER, MemoryMax=512M, CPUQuota=100%)"
        return
    fi

    sudo_note "the unit file goes to /etc/systemd/system (root-owned) — written, reloaded and enabled with sudo"

    local tmp
    tmp="$(mktemp)"
    cat > "$tmp" <<SERVICE
[Unit]
Description=PGindexAI - PostgreSQL Index Management Daemon
Documentation=https://github.com/GuillaumeDiamant/PGindexAI
After=network.target postgresql.service
Wants=postgresql.service

[Service]
Type=simple
User=$TARGET_USER
Group=$TARGET_USER
ExecStart="$BIN_PATH" daemon --config "$CONFIG_FILE"
Restart=always
RestartSec=10
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=HOME=$TARGET_HOME

# Resources
MemoryMax=512M
CPUQuota=100%

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/log/pgindexai
PrivateTmp=true
PrivateDevices=true
RestrictSUIDSGID=true
LockPersonality=true
SystemCallArchitectures=native

# Journal
StandardOutput=journal
StandardError=journal
SyslogIdentifier=pgindexai

[Install]
WantedBy=multi-user.target
SERVICE

    run sudo install -m 644 "$tmp" "$service_file"
    run rm -f "$tmp"
    run sudo systemctl daemon-reload
    run sudo systemctl enable "$SERVICE_NAME"
    info "Service installed and enabled"
}

# ─────────────────────────────────────────────
# Laya decision engine (REQUIRED — configured before the database steps)
# ─────────────────────────────────────────────
# Laya is an EXTERNAL binary (not built by this script). When it is found
# on the machine (neutral 'laya/' layout first, then the legacy 'pgindexai/'
# and 'laya-sql/' layouts), install.sh services it as '$LAYA_SERVICE_NAME'
# (systemd, hardened). The neutral name keeps Laya reusable by other tools.
# Laya is a PREREQUISITE: this step runs BEFORE the target database is
# created or attached, and a missing or unhealthy Laya ABORTS the install —
# no database work without the decision engine. The daemon's own
# "no-fallback" policy (observation mode until Laya is reachable) still
# guards runtime; the installer simply guarantees Laya comes first.
setup_laya_service() {
    step "Laya decision engine (REQUIRED — before the database steps)..."

    # ── Locate the binary ────────────────────────────────────────────
    local laya_bin="" cand
    for cand in \
        "${TARGET_HOME}/.local/share/laya/laya" \
        "${TARGET_HOME}/.local/share/pgindexai/laya" \
        "${TARGET_HOME}/.local/share/laya-sql/laya"
    do
        [[ -x "$cand" ]] && { laya_bin="$cand"; break; }
    done

    # ── Locate the model (root dir containing rl_agent_config.json) ──
    local laya_model=""
    for cand in \
        "${TARGET_HOME}/.local/share/laya/models/typed-decisions/typed-decisions" \
        "${TARGET_HOME}/.local/share/laya/models/typed-decisions" \
        "${TARGET_HOME}/.local/share/pgindexai/models/typed-decisions/typed-decisions" \
        "${TARGET_HOME}/.local/share/pgindexai/models/typed-decisions" \
        "${TARGET_HOME}/.local/share/laya-sql/models/typed-decisions/typed-decisions" \
        "${TARGET_HOME}/.local/share/laya-sql/models/typed-decisions"
    do
        [[ -f "$cand/rl_agent_config.json" ]] && { laya_model="$cand"; break; }
    done

    if [[ -z "$laya_bin" || -z "$laya_model" ]]; then
        warn "Laya not found (binary: ${laya_bin:-missing} — model: ${laya_model:-missing})."
        warn "Laya is a PREREQUISITE: the installer aborts BEFORE creating or"
        warn "attaching the target database if the decision engine is unavailable."
        if $DRY_RUN; then
            warn "[dry-run] a real run would stop here unless Laya is installed first."
            echo ""
            laya_print_runbook
            return 0
        fi
        echo ""
        laya_print_runbook
        # NB: default is N — under `curl | bash` (no tty) `ask` returns the
        # default silently, and an unrequested ~2 GB download in a piped
        # install would be a surprise. The runbook above is always printed,
        # so nothing is lost; answer 'y' interactively to install in place.
        if ! ask "Install Laya now from the official channel (laya.cpp ${LAYA_RELEASE} + ${LAYA_MODEL_REPO})? [y/N]" N; then
            error "Laya is required before the database steps — install it (runbook above), then re-run the installer."
            return 1
        fi
        if ! laya_install_official; then
            error "Automatic Laya install failed — follow the manual procedure above, then re-run the installer."
            return 1
        fi
        # laya_install_official places the pieces at the canonical paths (the
        # first candidates of the two detection loops above) — verify and
        # adopt them, then continue to the service deployment below: the
        # hardened 'laya' unit is deployed in THIS same run, no re-run of
        # the installer needed.
        if [[ -x "${TARGET_HOME}/.local/share/laya/laya" \
              && -f "${TARGET_HOME}/.local/share/laya/models/typed-decisions/typed-decisions/rl_agent_config.json" ]]; then
            laya_bin="${TARGET_HOME}/.local/share/laya/laya"
            laya_model="${TARGET_HOME}/.local/share/laya/models/typed-decisions/typed-decisions"
        else
            error "In-place Laya install reported success but the files are missing —"
            error "follow the manual procedure above, then re-run the installer."
            return 1
        fi
    fi

    info "Laya found — binary: $laya_bin"
    info "             model : $laya_model"

    local service_file="/etc/systemd/system/${LAYA_SERVICE_NAME}.service"
    if [[ -f "$service_file" ]]; then
        if ! ask "Existing Laya service → $service_file. Replace it? [Y/n]" Y; then
            # Keep the existing deployment — but do not claim « Laya OK »
            # without checking it (review note M2): if it answers, run the
            # same deep check the daemon gates on; if it does not answer,
            # say so (the daemon stays in observation mode until it is up).
            if laya_health_up; then
                laya_report_deep_check || true
            else
                warn "Laya is not answering on 127.0.0.1:${LAYA_PORT} — the daemon will run in"
                warn "OBSERVATION mode (no DDL) until the service is up."
            fi
            return 0
        fi
    fi

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} write $service_file (User=$TARGET_USER, MemoryMax=4G, CPUQuota=100%)"
        echo -e "${CYAN}[dry-run]${NC} sudo systemctl enable --now $LAYA_SERVICE_NAME"
        return 0
    fi

    sudo_note "the 'laya' systemd unit is written to /etc/systemd/system and managed with sudo (systemd units are root-owned)"

    # Orphan instance (manual start, nohup...): if it holds the port,
    # the systemd unit would crash-loop. Stop it first.
    if command -v pgrep &>/dev/null && pgrep -x "${LAYA_SERVICE_NAME}" >/dev/null 2>&1; then
        warn "A Laya process is already running (manual start) — stopping it..."
        run sudo pkill -x "${LAYA_SERVICE_NAME}" || true
        sleep 1
    fi

    local tmp
    tmp="$(mktemp)"
    cat > "$tmp" <<SERVICE
[Unit]
Description=PGindexAI Laya decision engine (HTTP server, /v1/systemone)
Documentation=https://github.com/GuillaumeDiamant/PGindexAI
After=network.target

[Service]
Type=simple
User=$TARGET_USER
Group=$TARGET_USER
ExecStart="$laya_bin" --model "$laya_model" --cpu --server --port $LAYA_PORT
Restart=always
RestartSec=5
Environment=PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=HOME=$TARGET_HOME

# Resources — CPU inference server (~850 MB of weights + runtime)
MemoryMax=4G
CPUQuota=100%

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths="$laya_model"
PrivateTmp=true
PrivateDevices=true
RestrictSUIDSGID=true
LockPersonality=true
SystemCallArchitectures=native

# Journal
StandardOutput=journal
StandardError=journal
SyslogIdentifier=$LAYA_SERVICE_NAME

[Install]
WantedBy=multi-user.target
SERVICE

    run sudo install -m 644 "$tmp" "$service_file"
    run rm -f "$tmp"
    run sudo systemctl daemon-reload
    run sudo systemctl enable "$LAYA_SERVICE_NAME"
    run sudo systemctl restart "$LAYA_SERVICE_NAME"

    # Activity polling (30 s) — the model is loaded at startup
    for _ in {1..15}; do
        if laya_health_up; then
            break
        fi
        sleep 2
    done
    if ! laya_health_up; then
        error "Laya is not answering after 30 s — check: sudo journalctl -u $LAYA_SERVICE_NAME -n 30"
        return 1
    fi
    info "Laya service is active (http://127.0.0.1:${LAYA_PORT}/health OK)"

    # Deep check (review note M2): /health alone can answer OK while the
    # decision pipeline is broken — the daemon would then skip EVERY
    # proposal (its deep_health gate has no fallback). The installer must
    # not claim « Laya OK » on /health alone: mirror the daemon's check.
    if ! laya_report_deep_check; then
        warn "Install continues — after fixing the model:"
        warn "    sudo systemctl restart ${LAYA_SERVICE_NAME}"
    fi
    return 0
}

# laya_health_up — probe the Laya HTTP endpoint (curl when available,
# /dev/tcp fallback otherwise).
laya_health_up() {
    if command -v curl &>/dev/null; then
        curl -sf -m 2 "http://127.0.0.1:${LAYA_PORT}/health" >/dev/null 2>&1
    else
        (exec 3<>"/dev/tcp/127.0.0.1/${LAYA_PORT}") 2>/dev/null
    fi
}

# laya_deep_health_up — the SAME structural check the daemon's
# LayaClient::deep_health runs before applying any proposal: POST a real
# decision payload to /v1/systemone and require a structured answer
# (choice ∈ {0,1}, confidence ∈ [0,1]). /health alone can answer OK while
# the decision pipeline (model load, response format) is broken — the daemon
# would then skip EVERY proposal (its deep_health gate has no fallback).
# Requires curl (a POST with a body; the /dev/tcp fallback of
# laya_health_up cannot cover it).
laya_deep_health_up() {
    local body
    body="$(curl -sf -m 60 -X POST "http://127.0.0.1:${LAYA_PORT}/v1/systemone" \
        -H 'Content-Type: application/json' \
        -d '{"state":"install-deep-health-check","questions":{"action":{"type":"choice","instructions":"Choose the single best action for this PostgreSQL index situation.","criteria":{"0":"Option A: no action needed","1":"Option B: perform a maintenance action"}}}}' 2>/dev/null)" \
        || return 1
    grep -qE '"choice"[[:space:]]*:[[:space:]]*"(0|1)"' <<<"$body" || return 1
    # Terminator ([^0-9.]|$) after the value: rejects 12 (read as "1"+"2")
    # and 1.5 (read as "1"+".").
    grep -qE '"confidence"[[:space:]]*:[[:space:]]*(0(\.[0-9]+)?|1(\.0+)?)([^0-9.]|$)' <<<"$body"
}

# laya_report_deep_check — run laya_deep_health_up with user-visible reporting.
# Returns 1 only when the decision pipeline is broken; 0 when OK, or when the
# probe cannot run at all (no curl — the daemon runs its own deep_health on
# the first proposal in that case).
laya_report_deep_check() {
    if ! command -v curl &>/dev/null; then
        warn "curl unavailable — Laya deep check skipped (the daemon runs its own"
        warn "deep_health probe on the first proposal)."
        return 0
    fi
    if laya_deep_health_up; then
        info "Laya deep check OK — the decision pipeline answers a real payload (choice + confidence valid)"
        return 0
    fi
    warn "Laya /health is OK but the DECISION pipeline failed the deep check:"
    warn "a real /v1/systemone payload got no valid answer (choice ∈ {0,1},"
    warn "confidence ∈ [0,1] — the same validation the daemon's deep_health runs)."
    warn "Consequence: the daemon will SKIP every proposal (deep_health gate, no fallback)."
    warn "Fix the model (file, --model flag, variant), then restart the service."
    return 1
}

# ─────────────────────────────────────────────
# Step 4b (cont.): official Laya channel
#   binary — laya.cpp releases (https://github.com/lkarlslund/laya.cpp, MIT)
#   model  — Hugging Face (https://huggingface.co/convaiinnovations/laya-typed-decisions)
# ─────────────────────────────────────────────
# laya_pick_backend — pick the official linux-amd64 binary variant:
#   cuda12 : needs the NVIDIA driver (libcuda.so.1) — ~1.1 GB
#   vulkan : needs libvulkan.so.1 (e.g. 'sudo apt install libvulkan1') — ~78 MB
# Override with LAYA_BACKEND=cuda12|vulkan.
laya_pick_backend() {
    if [[ "${LAYA_BACKEND:-}" == "cuda12" || "${LAYA_BACKEND:-}" == "vulkan" ]]; then
        echo "$LAYA_BACKEND"
        return 0
    fi
    if [[ -n "${LAYA_BACKEND:-}" ]]; then
        # warn() writes to stderr — it cannot pollute the value captured
        # through command substitution by the caller.
        warn "Invalid LAYA_BACKEND='${LAYA_BACKEND}' (expected: cuda12|vulkan) — falling back to auto-detect"
    fi
    if ldconfig -p 2>/dev/null | grep -q 'libcuda\.so\.1'; then
        echo "cuda12"
    else
        echo "vulkan"
    fi
}

# laya_backend_spec — echoes: url <TAB> sha256 <TAB> human_size
laya_backend_spec() {
    local backend
    backend="$(laya_pick_backend)"
    if [[ "$backend" == "cuda12" ]]; then
        printf '%s\t%s\t%s\n' \
            "https://github.com/${LAYA_UPSTREAM}/releases/download/${LAYA_RELEASE}/laya-${LAYA_RELEASE}-linux-amd64-cuda12" \
            "${LAYA_SHA256_CUDA12}" "~1.1 GB"
    else
        printf '%s\t%s\t%s\n' \
            "https://github.com/${LAYA_UPSTREAM}/releases/download/${LAYA_RELEASE}/laya-${LAYA_RELEASE}-linux-amd64-vulkan" \
            "${LAYA_SHA256_VULKAN}" "~78 MB"
    fi
}

# sha256_of FILE — SHA-256 with the first available tool (empty if none)
sha256_of() {
    local f="$1" out=""
    if command -v sha256sum &>/dev/null; then
        out="$(sha256sum "$f" 2>/dev/null | awk '{print $1}')" || out=""
    elif command -v shasum &>/dev/null; then
        out="$(shasum -a 256 "$f" 2>/dev/null | awk '{print $1}')" || out=""
    elif command -v openssl &>/dev/null; then
        out="$(openssl dgst -sha256 -r "$f" 2>/dev/null | awk '{print $1}')" || out=""
    fi
    echo "${out:-}"
}

# laya_print_runbook — self-contained, copy-pasteable procedure to install
# the official Laya (binary + model + sanity check + service re-run).
# Always printed when Laya is missing — even under `curl | bash`.
laya_print_runbook() {
    local url sha size backend
    IFS=$'\t' read -r url sha size <<<"$(laya_backend_spec)"
    backend="$(laya_pick_backend)"

    cat <<EOF
============================================================================
  Laya decision engine — official install procedure (laya.cpp ${LAYA_RELEASE})
============================================================================
  Binary (laya.cpp, MIT) : https://github.com/${LAYA_UPSTREAM}
  Model  (Hugging Face)  : ${LAYA_MODEL_REPO}
  Backend                : ${backend}   (override: LAYA_BACKEND=cuda12|vulkan)
    cuda12 : needs the NVIDIA driver (libcuda.so.1) — ~1.1 GB
    vulkan : needs libvulkan1 (e.g. sudo apt install libvulkan1) — ~78 MB

  # 1. Binary (laya.cpp ${LAYA_RELEASE}, backend: ${backend})
  mkdir -p ~/.local/share/laya
  curl -fSL -o ~/.local/share/laya/laya '${url}'
  echo '${sha}  ~/.local/share/laya/laya' | sha256sum -c -

  # 2. Model (~807 MB)
  #    Option A — HF CLI: pip install -U huggingface_hub
  hf download ${LAYA_MODEL_REPO} --local-dir ~/.local/share/laya/models/typed-decisions/typed-decisions
  #    Option B — direct download (curl only)
  M=~/.local/share/laya/models/typed-decisions/typed-decisions
  for f in ${LAYA_MODEL_FILES[@]}; do
      mkdir -p "\${M}/\$(dirname "\${f}")"   # subdirs: curl cannot write into a missing directory
      curl -fSL -o "\${M}/\${f}" "https://huggingface.co/${LAYA_MODEL_REPO}/resolve/main/\${f}"
  done

  # 3. Sanity check (CPU inference — the model takes ~20 s to load)
  ~/.local/share/laya/laya --model ~/.local/share/laya/models/typed-decisions/typed-decisions --cpu --server --port ${LAYA_PORT} &
  sleep 20 && curl -sf http://127.0.0.1:${LAYA_PORT}/health && echo "  → Laya OK"
  pkill -x laya || true

  # 4. Deploy the hardened 'laya' systemd service (re-run the installer)
  #    - with a local clone of the PGindexAI repo:  sudo ./install.sh (from the repo dir)
  #    - or re-run the 'curl | bash' command that started this install
  #    The PGindexAI daemon auto-detects Laya on its next decision batch
  #    (deep health check) — NO daemon restart required.
============================================================================
EOF
}

# laya_install_official — download the official Laya (binary + model) into
# the canonical layout, verify the checksums, sanity-check the HTTP server.
# Idempotent: existing pieces are kept. DRY_RUN-safe. The systemd service
# itself is NOT deployed here — setup_laya_service adopts the freshly
# installed files (canonical paths) and deploys it in the same run.
laya_install_official() {
    local url sha size backend
    IFS=$'\t' read -r url sha size <<<"$(laya_backend_spec)"
    backend="$(laya_pick_backend)"

    local laya_home="${TARGET_HOME}/.local/share/laya"
    local model_dir="${laya_home}/models/typed-decisions/typed-decisions"

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} mkdir -p ${model_dir} (incl. encoder/ and tokenizer/ subdirectories)"
        echo -e "${CYAN}[dry-run]${NC} download laya.cpp ${LAYA_RELEASE} '${backend}' binary (${size}) → ${laya_home}/laya  [sha256 ${sha}]"
        echo -e "${CYAN}[dry-run]${NC} download model ${LAYA_MODEL_REPO} (~807 MB) → ${model_dir}"
        echo -e "${CYAN}[dry-run]${NC} sanity check: laya --model <dir> --cpu --server --port ${LAYA_PORT} + GET /health + deep check (POST /v1/systemone)"
        return 0
    fi

    if ! command -v curl &>/dev/null; then
        error "curl is required for the download — follow the manual procedure above"
        return 1
    fi

    # Create the model dir AND every subdirectory the files need (encoder/,
    # tokenizer/): curl refuses to write a file whose parent directory does
    # not exist (curl error 23 — "Failure writing output to destination").
    # A plain `mkdir -p "$model_dir"` alone is not enough.
    for f in "${LAYA_MODEL_FILES[@]}"; do
        run mkdir -p "$(dirname "${model_dir}/${f}")"
    done
    if [[ "$(id -u)" -eq 0 ]]; then
        run chown -R "$TARGET_USER" "${TARGET_HOME}/.local/share/laya"
    fi

    # ── 1) Binary (official release — SHA-256 verified) ───────────────
    if [[ -x "${laya_home}/laya" ]]; then
        info "Binary already present — skipping download: ${laya_home}/laya"
    else
        step "Downloading the Laya binary (laya.cpp ${LAYA_RELEASE}, '${backend}', ${size})..."
        local tmp
        # Stage the download inside laya_home (created above) instead of /tmp:
        # /tmp is often a small tmpfs that cannot hold a 1.1 GB cuda12 binary,
        # and a cross-filesystem `mv` would re-copy the whole file. A hidden
        # dotfile in the target dir stays on the same filesystem, so the `mv`
        # below is an atomic rename.
        tmp="$(mktemp "${laya_home}/.laya.XXXXXX")"
        if ! run curl -fSL --retry 3 -o "$tmp" "$url"; then
            rm -f "$tmp"
            error "Binary download failed: $url"
            return 1
        fi
        local got
        got="$(sha256_of "$tmp")"
        if [[ -z "$got" || "$got" != "$sha" ]]; then
            rm -f "$tmp"
            error "SHA-256 MISMATCH — expected ${sha}"
            error "               got      ${got:-unavailable (no sha256 tool?)}"
            error "Refusing to install a binary whose checksum cannot be verified."
            return 1
        fi
        run mv "$tmp" "${laya_home}/laya"
        run chmod 755 "${laya_home}/laya"
        info "Binary installed: ${laya_home}/laya (sha256 verified)"
    fi

    # ── 2) Model (Hugging Face, direct files — no extra tooling) ──────
    # The model is "complete" only if EVERY expected file is present and
    # non-empty AND model.safetensors is at least 100 MB. A partial or
    # truncated download from an earlier failed run must never be accepted
    # on a re-run — the missing pieces are re-fetched below.
    local base="https://huggingface.co/${LAYA_MODEL_REPO}/resolve/main"
    local f p ms
    local model_complete=true
    for f in "${LAYA_MODEL_FILES[@]}"; do
        [[ -s "${model_dir}/${f}" ]] || model_complete=false
    done
    ms="$(stat -c %s "${model_dir}/model.safetensors" 2>/dev/null || echo 0)"
    [[ "$ms" -ge 100000000 ]] || model_complete=false
    if $model_complete; then
        info "Model already present — skipping download: ${model_dir}"
    else
        step "Downloading the Laya model (${LAYA_MODEL_REPO}, ~807 MB)..."
        local dl_ok=true
        for f in "${LAYA_MODEL_FILES[@]}"; do
            p="${model_dir}/${f}"
            if [[ "$f" == "model.safetensors" ]]; then
                ms="$(stat -c %s "$p" 2>/dev/null || echo 0)"
                [[ "$ms" -ge 100000000 ]] && continue   # large file already valid — keep it
            elif [[ -s "$p" ]]; then
                continue   # idempotent: keep valid files, fetch only what is missing
            fi
            if ! run curl -fSL --retry 3 -o "$p" "${base}/${f}"; then
                warn "Model file download failed: ${f}"
                dl_ok=false
            fi
        done
        if [[ "$dl_ok" != "true" ]]; then
            error "Model download incomplete — re-run, or use the HF CLI (Option A above)"
            return 1
        fi
        ms="$(stat -c %s "${model_dir}/model.safetensors" 2>/dev/null || echo 0)"
        if [[ "$ms" -lt 100000000 ]]; then
            error "model.safetensors looks incomplete (${ms} bytes) — re-run the download"
            return 1
        fi
        info "Model installed: ${model_dir}"
    fi

    if [[ "$(id -u)" -eq 0 ]]; then
        run chown -R "$TARGET_USER" "${laya_home}"
    fi

    # ── 3) Sanity check (CPU server + /health + deep decision), then stop it ──
    local sanity_ok=true
    if laya_health_up; then
        info "Laya already answers on http://127.0.0.1:${LAYA_PORT}/health (existing instance kept)"
        # Review note M2: /health OK is not enough — the daemon gates every
        # proposal on a real decision answer (deep_health, no fallback).
        laya_report_deep_check || sanity_ok=false
    else
        step "Sanity check: starting Laya (--cpu --server --port ${LAYA_PORT})..."
        local server_log
        server_log="$(mktemp)"
        if [[ "$(id -u)" -eq 0 ]]; then
            sudo_note "running as root — the sanity-check server runs as '$TARGET_USER' via 'sudo -u $TARGET_USER', its log written with 'sudo tee'"
            # Root branch: redirect through `sudo tee` so the log is written
            # in the target user's context (a bare `>file` after `sudo` would
            # create a root-owned file — SC2024).
            sudo -u "$TARGET_USER" "${laya_home}/laya" --model "$model_dir" --cpu --server --port "$LAYA_PORT" 2>&1 \
                | sudo tee "$server_log" >/dev/null &
        else
            "${laya_home}/laya" --model "$model_dir" --cpu --server --port "$LAYA_PORT" >"$server_log" 2>&1 &
        fi
        local up=false
        for _ in {1..30}; do
            if laya_health_up; then
                up=true
                break
            fi
            sleep 2
        done
        if $up; then
            info "Sanity check OK — Laya answers on http://127.0.0.1:${LAYA_PORT}/health"
            # Review note M2: /health OK is not enough — the daemon gates every
            # proposal on a real decision answer (deep_health, no fallback).
            # Probe it now, while this instance owns the port (it is stopped
            # right below).
            laya_report_deep_check || sanity_ok=false
        else
            sanity_ok=false
            warn "Sanity check FAILED — Laya did not answer within 60 s. Last log lines:"
            tail -n 20 "$server_log" 2>/dev/null | sed 's/^/    /' || true
        fi
        # Stop the probe instance: the systemd service (deployed on the
        # re-run) is the canonical owner of the port.
        if command -v pkill &>/dev/null; then
            run pkill -x "${LAYA_SERVICE_NAME}" || true
            sleep 1
        fi
        rm -f "$server_log"
    fi

    # ── 4) Result ─────────────────────────────────────────────────────
    echo ""
    if ! $sanity_ok; then
        warn "Laya is installed but NOT verified: the sanity check failed (see above)."
        warn "Fix the cause, then re-run:  sudo ./install.sh"
        return 1
    fi
    info "Laya is installed and verified."
    info "Re-run the installer to deploy the hardened 'laya' systemd service:"
    info "    sudo ./install.sh"
    info "The PGindexAI daemon auto-detects Laya on its next decision batch"
    info "(deep health check) — NO daemon restart required."
    return 0
}

# ─────────────────────────────────────────────
# Step 5: Startup
# ─────────────────────────────────────────────
start_daemon() {
    step "Starting the daemon..."
    sudo_note "systemd services are root-owned — the daemon is (re)started and polled with 'sudo systemctl'"
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} sudo systemctl restart $SERVICE_NAME"
        return
    fi
    sudo systemctl restart "$SERVICE_NAME"

    # Activity polling (30 s)
    for _ in {1..15}; do
        if sudo systemctl is-active --quiet "$SERVICE_NAME"; then
            info "PGindexAI daemon is active"
            return 0
        fi
        sleep 2
    done

    error "The daemon is not active after 30 s — check: sudo journalctl -u $SERVICE_NAME -n 30"
    return 1
}

# ─────────────────────────────────────────────
# Step 6: Demo (schema + workload)
# ─────────────────────────────────────────────
setup_demo() {
    step "Installing the demo (schema + workload)..."
    if [[ -z "$PG_PASSWORD" ]]; then
        sudo_note "the demo SQL is run as 'sudo -u $POSTGRES_USER psql' (peer auth — no password)"
    fi
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

    if ! pg_psql -d "$TARGET_DB" -tAc \
        "SELECT 1 FROM information_schema.tables WHERE table_name = 'customers' LIMIT 1" 2>/dev/null | grep -q 1; then
        # The SQL goes through STDIN, not `psql -f`: the postgres user cannot
        # read files under a private (drwxr-x---) home directory, so `psql -f`
        # fails with "Permission denied" even though the SQL itself is valid.
        # Feeding it on stdin lets OUR shell (which owns the file) do the read.
        if [[ -f "$repo_dir/sql/demo_schema.sql" ]]; then
            if demo_psql "schema" "$repo_dir/sql/demo_schema.sql"; then
                info "Demo schema installed"
            else
                warn "Demo schema installation FAILED — see the psql output above"
            fi
        fi
        if [[ -f "$repo_dir/sql/demo_workload.sql" ]]; then
            if demo_psql "workload" "$repo_dir/sql/demo_workload.sql"; then
                info "Demo workload installed"
            else
                warn "Partial demo workload — the workload SQL failed (see psql output above)"
            fi
        fi
        info "Full raw psql output (more detail than the screen): $DEMO_LOG"
    else
        info "Demo schema already installed"
    fi
}

# ─────────────────────────────────────────────
# Final verification
# ─────────────────────────────────────────────
verify() {
    echo ""
    echo "============================================"
    echo "  PGindexAI installation finished"
    echo "============================================"
    echo ""
    local mode="existing database"
    if $INSTALL_DEMO; then
        mode="demo database"
    fi
    local pg_admin
    if [[ -n "$PG_PASSWORD" ]]; then
        pg_admin="password (PGPASSWORD → role '$POSTGRES_USER')"
    else
        pg_admin="sudo -u $POSTGRES_USER (peer)"
    fi
    echo "  Binary     : $BIN_PATH"
    echo "  Target DB  : $TARGET_DB  ($mode)"
    echo "  Config     : $CONFIG_FILE  (0600, password: null)"
    echo "  Auth       : peer — pg_ident map '$PG_IDENT_MAP: $TARGET_USER → pgindexai' (no password)"
    echo "  PG admin   : $pg_admin  (installer admin connection)"
    echo "  Logs       : /var/log/pgindexai/"
    if $INSTALL_DEMO; then
        echo "  Demo log   : $DEMO_LOG  (full raw psql output — strictly more detailed than the screen)"
    fi
    echo "  Service    : systemctl status $SERVICE_NAME"
    echo "  Laya       : systemctl status $LAYA_SERVICE_NAME  (decision engine — prerequisite, up before the DB steps)"
    echo "  Daemon     : journalctl -u $SERVICE_NAME -f"
    echo ""
    echo "  Useful commands:"
    echo "    pgindexai run --dry-run   → single cycle, NO DDL"
    echo "    pgindexai daemon          → continuous mode"
    echo "    journalctl -u $SERVICE_NAME -n 50"
    echo "    tail -f /var/log/pgindexai/index_operations.log"
    echo ""
    if $DRY_RUN; then
        echo "  📋 Dry-run mode — no modification has been applied"
        echo ""
    fi
    echo "  Hardening reminder: remove 'local all all trust' from pg_hba.conf"
    echo "  (if present) so that ONLY the pgindexai role remains accessible locally."
    echo ""
}

# ─────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────
main() {
    echo ""
    echo "============================================"
    echo "  PGindexAI Installer (secure)"
    echo "============================================"
    echo ""

    check_sudo
    check_postgres
    check_rust
    echo ""
    resolve_demo_choice
    echo ""
    # Hard prerequisite: Laya must be available BEFORE the target database
    # is created or attached — no database work without the decision
    # engine. A missing or unhealthy Laya aborts the install here (under
    # `set -e`, a non-zero return stops the script before setup_postgresql).
    setup_laya_service
    echo ""
    setup_postgresql
    echo ""
    build_and_install
    echo ""
    setup_config
    echo ""
    setup_systemd
    echo ""
    local daemon_ok=true
    start_daemon || daemon_ok=false
    echo ""
    # Demo mode only: load the demo schema + workload, then re-apply the
    # ownership transfer (the schema is created BY postgres — the daemon
    # needs DDL rights on it). Custom mode: nothing to load, and the
    # pre-existing objects are NOT re-attributed (no ownership stealing).
    if $INSTALL_DEMO; then
        setup_demo || true
        transfer_ownership
    fi
    echo ""
    verify
    # If the daemon is not active, the installation cannot declare itself
    # finished (previously: masked failure — the script exited « success »
    # while the service was in a crash loop).
    $daemon_ok
}

main "$@"
