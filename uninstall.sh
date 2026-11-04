#!/usr/bin/env bash
# ============================================================================
# uninstall.sh — Uninstallation of PGindexAI
#
# Usage:
#   sudo ./uninstall.sh [--dry-run] [--purge] [--clean-legacy]
#
# By default: stops and disables the service, removes the systemd unit, the
# binary, the config, the user's data and logs. PRESERVES the
# pgindexai_demo database, the pgindexai role and the pg_hba line (reinstallable,
# data preserved).
#
# Options:
#   --dry-run        full simulation, no modification
#   --purge          ALSO REMOVES the database + the role + the pg_hba line
#                    (irreversible, confirmation requested)
#   --clean-legacy   also removes the remnants of the old name (laya-sql)
#
# Safeguards:
#   - shared_preload_libraries and pg_stat_statements are NEVER modified
#     (shared extension — manual removal if truly desired)
#   - without a controlling terminal, the --purge confirmation falls back to the default (N)
#     : an irreversible operation is NEVER executed without an
#     explicit confirmation
#   - --purge refuses to act if the 'pgindexai' role owns objects in
#     other databases (DROP ROLE would otherwise fail, partially)
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

# ─────────────────────────────────────────────
# Global variables
# ─────────────────────────────────────────────
DRY_RUN=false
PURGE=false
CLEAN_LEGACY=false
POSTGRES_USER="${POSTGRES_USER:-postgres}"

# Target user: the one owning the config, the data and the logs.
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
if [[ -z "${TARGET_HOME:-}" ]]; then
    error "Cannot determine the home of '$TARGET_USER'."
    exit 1
fi

BIN_PATH="/usr/local/bin/pgindexai"
SERVICE_NAME="pgindexai"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
CONFIG_DIR="${TARGET_HOME}/.config/PGindexAI"
PGINDEXAI_DIR="${TARGET_HOME}/.local/share/pgindexai"
LOG_DIR="/var/log/pgindexai"
PG_HBA_DEFAULT="/etc/postgresql/16/main/pg_hba.conf"

# ─────────────────────────────────────────────
# Argument parsing
# ─────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --purge) PURGE=true ;;
        --clean-legacy) CLEAN_LEGACY=true ;;
        -h|--help)
            sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) warn "Unknown argument: $arg (ignored)" ;;
    esac
done

# run CMD [ARGS...] — no eval: each argument is passed as-is
run() {
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} $*"
    else
        "$@"
    fi
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

# pg_show "SQL" — query with -tAc as $POSTGRES_USER (empty on failure)
pg_show() {
    sudo -u "$POSTGRES_USER" psql -tAc "$1" 2>/dev/null || true
}

# hba_file — effective path of pg_hba.conf (SHOW hba_file, otherwise default)
hba_file() {
    local hba
    hba="$(pg_show 'SHOW hba_file' | tr -d '[:space:]')"
    printf '%s\n' "${hba:-$PG_HBA_DEFAULT}"
}

# ─────────────────────────────────────────────
# Step 1: systemd service + possible orphan process
# ─────────────────────────────────────────────
stop_service() {
    step "Service systemd..."
    if [[ -f "$SERVICE_FILE" ]]; then
        run sudo systemctl stop "$SERVICE_NAME" || warn "Stop impossible (already inactive?)"
        run sudo systemctl disable "$SERVICE_NAME" 2>/dev/null || true
        run sudo rm -f "$SERVICE_FILE"
        run sudo systemctl daemon-reload
        run sudo systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true
        info "Service stopped, disabled, unit removed"
    else
        info "No systemd unit found ($SERVICE_FILE)"
    fi

    # Process started manually (outside systemd)?
    if command -v pgrep &>/dev/null && pgrep -x "$SERVICE_NAME" >/dev/null 2>&1; then
        warn "Process $SERVICE_NAME running — stopping..."
        run sudo pkill -x "$SERVICE_NAME" || true
    fi
}

# ─────────────────────────────────────────────
# Step 2: binary
# ─────────────────────────────────────────────
remove_binary() {
    step "Binary..."
    if [[ -f "$BIN_PATH" ]]; then
        run sudo rm -f "$BIN_PATH"
        info "Binary removed: $BIN_PATH"
    else
        info "No binary found ($BIN_PATH)"
    fi
}

# ─────────────────────────────────────────────
# Step 3: user's config, data, logs
# ─────────────────────────────────────────────
remove_dirs() {
    step "Config, data and logs..."
    run sudo rm -rf "$CONFIG_DIR"
    run sudo rm -rf "$PGINDEXAI_DIR"
    run sudo rm -rf "$LOG_DIR"
    info "Removed: $CONFIG_DIR, $PGINDEXAI_DIR, $LOG_DIR"
}

# ─────────────────────────────────────────────
# Step 4 (option --purge): database, role, pg_hba line
# ─────────────────────────────────────────────
remove_pg_objects() {
    step "PostgreSQL objects (database + role + pg_hba)..."
    # Default confirmation is NO (N): without a tty (curl|bash, cron…) the purge
    # is canceled instead of executing — irreversible operation.
    if ! ask "Permanently REMOVE the 'pgindexai_demo' database (all its data) AND the 'pgindexai' role? (irreversible) [y/N]" N; then
        warn "Removal canceled (no explicit confirmation): database and role preserved."
        return 0
    fi

    # Safeguard: if the role owns objects in OTHER databases, DROP ROLE
    # would fail (and partially on top of that). We refuse BEFORE any removal
    # to keep the operation atomic — we never drop « half-way ».
    local foreign
    foreign="$(
        for db in $(pg_show "SELECT datname FROM pg_database WHERE datallowconn AND datname <> 'pgindexai_demo' ORDER BY 1"); do
            sudo -u "$POSTGRES_USER" psql -d "$db" -tAc "
                SELECT n.nspname || '.' || c.relname
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE c.relowner = (SELECT oid FROM pg_authid WHERE rolname = 'pgindexai')
                  AND n.nspname <> 'information_schema'
                  AND n.nspname !~ '^pg_'
            " 2>/dev/null | sed "s|^|  ${db}: |" || true
        done
    )"
    if [[ -n "$foreign" ]]; then
        error "The 'pgindexai' role owns objects in OTHER databases — purge refused as-is:"
        echo "$foreign" | head -10 >&2
        error "Transfer ownership or remove those objects, then re-run with --purge."
        return 1
    fi

    # Order: database first (holds the tables/indexes), then the role.
    # Explicit failure (no silent abort mid-purge: DROP ROLE
    # partially applied would be an inconsistent state).
    if ! run sudo -u "$POSTGRES_USER" psql -qc "DROP DATABASE IF EXISTS pgindexai_demo"; then
        error "DROP DATABASE failed (active connections?) — purge stopped, role preserved."
        return 1
    fi
    if ! run sudo -u "$POSTGRES_USER" psql -qc "DROP ROLE IF EXISTS pgindexai"; then
        error "DROP ROLE failed (residual privileges?) — see the error above."
        return 1
    fi
    info "Database and role removed"

    # Dedicated line (corrected form « local all pgindexai » AND
    # defective variant « local pgindexai pgindexai » — DATABASE column).
    local hba
    hba="$(hba_file)"
    if [[ -f "$hba" ]] && sudo grep -Eq "^[[:space:]]*local[[:space:]]+(all|pgindexai_demo|pgindexai)[[:space:]]+pgindexai[[:space:]]+" "$hba"; then
        run sudo sed -i -E "/^[[:space:]]*local[[:space:]]+(all|pgindexai_demo|pgindexai)[[:space:]]+pgindexai[[:space:]]+.*$/d" "$hba"
        run sudo systemctl reload postgresql
        info "pg_hba line removed, PostgreSQL reloaded"
    else
        info "No dedicated pg_hba line found (nothing to remove)"
    fi

    warn "pg_stat_statements is PRESERVED (shared extension, shared_preload_libraries)."
    warn "Manual removal (if truly desired): remove 'pg_stat_statements' from"
    warn "shared_preload_libraries in postgresql.conf, then restart PostgreSQL."
}

# ─────────────────────────────────────────────
# Step 5 (option --clean-legacy): remnants of the old name
# ─────────────────────────────────────────────
clean_legacy() {
    step "Remnants of the old name (laya-sql)..."
    local legacy_unit="/etc/systemd/system/laya-sql.service"
    if [[ -f "$legacy_unit" ]]; then
        run sudo systemctl stop laya-sql 2>/dev/null || true
        run sudo systemctl disable laya-sql 2>/dev/null || true
        run sudo rm -f "$legacy_unit"
        run sudo systemctl daemon-reload
        info "laya-sql unit removed"
    fi
    run sudo rm -f /usr/local/bin/laya-sql
    run sudo rm -rf "${TARGET_HOME}/.config/layaSQL"
    run sudo rm -rf /var/log/laya-sql
    # Legacy PG objects (already renamed to pgindexai* during the migration) — no-op if absent
    run sudo -u "$POSTGRES_USER" psql -qc "DROP DATABASE IF EXISTS laya_demo" 2>/dev/null || true
    run sudo -u "$POSTGRES_USER" psql -qc "DROP ROLE IF EXISTS laya_sql" 2>/dev/null || true
    info "Remnants cleaned: binary, config, logs, possible PG objects"
}

# ─────────────────────────────────────────────
# Summary
# ─────────────────────────────────────────────
verify() {
    echo ""
    echo "============================================"
    echo "  PGindexAI uninstallation finished"
    echo "============================================"
    echo ""
    echo "  Service    : $SERVICE_NAME — stopped, unit removed"
    echo "  Binary    : $BIN_PATH — removed"
    echo "  Files   : config, data, logs removed"
    if $PURGE; then
        echo "  PostgreSQL : pgindexai_demo database + pgindexai role + pg_hba line removed"
    else
        echo "  PostgreSQL : PRESERVED (pgindexai_demo database, pgindexai role, pg_hba line)"
        echo "               → full removal: sudo ./uninstall.sh --purge"
    fi
    echo "  pg_stat_statements : preserved (shared extension)"
    if $DRY_RUN; then
        echo ""
        echo "  📋 Dry-run mode — no modification has been applied"
    fi
    echo ""
    echo "  Reinstallation: sudo ./install.sh"
    echo ""
}

# ─────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────
main() {
    echo ""
    echo "============================================"
    echo "  PGindexAI — Uninstallation"
    echo "============================================"
    echo ""
    echo "  Target user: $TARGET_USER"
    if $PURGE; then
        warn "--purge mode: the pgindexai_demo database and the pgindexai role will be removed."
    fi
    echo ""
    stop_service
    echo ""
    remove_binary
    echo ""
    remove_dirs
    if $PURGE; then
        echo ""
        remove_pg_objects
    fi
    if $CLEAN_LEGACY; then
        echo ""
        clean_legacy
    fi
    verify
}

main "$@"
