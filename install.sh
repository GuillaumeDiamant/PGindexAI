#!/usr/bin/env bash
# ============================================================================
# install.sh — Secure installation of PGindexAI
#
# Prerequisites:
#   - PostgreSQL 16+ (psql binary + working cluster)
#   - Rust toolchain (cargo)
#   - sudo access
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/GuillaumeDiamant/PGindexAI/main/install.sh | bash
#   # or
#   ./install.sh [--dry-run]
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

# ─────────────────────────────────────────────
# Global variables
# ─────────────────────────────────────────────
DRY_RUN=false
POSTGRES_USER="${POSTGRES_USER:-postgres}"

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
LAYA_SERVICE_NAME="laya"   # Laya decision engine (optional — required to apply DDL)
LAYA_PORT=8080
PG_IDENT_MAP="pgindexai_map"  # pg_ident.conf map name (daemon OS user → pgindexai)
PG_CONF=""
PG_HBA=""
PG_IDENT=""
PG_SSL="off"

# ─────────────────────────────────────────────
# Argument parsing
# ─────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
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

# psql_stdin "SQL" [psql options...] — runs the SQL via STDIN, not as
# a command argument: a sensitive value (password) never goes
# through the local process list (`ps`) EVER.
# (The old `psql -c "…PASSWORD '…'"` remained visible during execution;
# the dry-run even displayed it on screen/in the log.)
psql_stdin() {
    local sql="$1"
    shift
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} sudo -u $POSTGRES_USER psql -q -v ON_ERROR_STOP=1 $* [SQL via stdin]"
    else
        printf '%s\n' "$sql" | sudo -u "$POSTGRES_USER" psql -q -v ON_ERROR_STOP=1 "$@"
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

# pg_show "SQL" — runs a query with -tAc as $POSTGRES_USER
# (via sudo -u, no password). Returns an empty string on failure.
pg_show() {
    sudo -u "$POSTGRES_USER" psql -tAc "$1" 2>/dev/null || true
}

# ─────────────────────────────────────────────
# Prerequisites
# ─────────────────────────────────────────────
check_sudo() {
    step "Checking sudo access..."
    if sudo -n true 2>/dev/null; then
        info "sudo available (without password)"
    else
        warn "sudo required — the script will ask you for your password"
    fi
}

check_postgres() {
    step "Checking PostgreSQL..."
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

    # Detecting config files via the server itself (no hardcoded path)
    PG_CONF="$(pg_show "SHOW config_file" | tr -d '[:space:]')"
    PG_HBA="$(pg_show "SHOW hba_file" | tr -d '[:space:]')"
    PG_SSL="$(pg_show "SHOW ssl" | tr -d '[:space:]')"
    if [[ -n "$PG_CONF" ]]; then
        info "postgresql.conf : $PG_CONF"
        info "pg_hba.conf     : ${PG_HBA:-?}"
        info "ssl             : ${PG_SSL:-?}"
    else
        warn "Cannot read the server state (sudo -u $POSTGRES_USER psql?)."
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
ensure_demo_db() {
    if [[ -n "$(pg_show "SELECT 1 FROM pg_database WHERE datname = 'pgindexai_demo'")" ]]; then
        info "Database 'pgindexai_demo' present"
        return
    fi
    step "Creating database 'pgindexai_demo'..."
    run sudo -u "$POSTGRES_USER" psql -qc "CREATE DATABASE pgindexai_demo"
}

# Ownership transfer of pgindexai_demo objects to pgindexai:
# CREATE INDEX / DROP INDEX / REINDEX require ownership of the table/index.
# NB: 'pg_toast' is EXCLUDED — TOAST indexes are system catalogs;
# an ALTER ... OWNER on them fails and ABORTs the whole DO block, canceling
# the whole transfer (without this exclusion, the transfer failed as soon as a
# table had a TOAST column: text/varchar/jsonb…).
transfer_ownership() {
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
        run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "$sql"
        return 0
    fi
    local err
    if ! err="$(sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "$sql" 2>&1 >/dev/null)"; then
        error "Ownership transfer FAILED — the daemon will NOT be able to create/drop indexes until the pgindexai_demo objects are owned by 'pgindexai'."
        if [[ -n "$err" ]]; then
            error "Detail: $(head -c 300 <<<"$err" | tr '\n' ' ')"
        fi
        return 1
    fi
    info "Ownership applied (tables + indexes of pgindexai_demo → pgindexai)"
}

setup_role() {
    step "Configuring the dedicated 'pgindexai' role (non-superuser, peer auth)..."

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
            local peer_check
            peer_check="$(sudo -u "$TARGET_USER" psql -U pgindexai -d pgindexai_demo -tAc "SELECT 1" 2>&1 || true)"
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

    run sudo -u "$POSTGRES_USER" psql -qc "GRANT CONNECT ON DATABASE pgindexai_demo TO pgindexai"
    # PostgreSQL 15+: CREATE is no longer granted to everyone on the public schema
    # by default — without this GRANT, CREATE/DROP INDEX fails (permission denied
    # for schema public) even if pgindexai owns the tables.
    run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "GRANT CREATE ON SCHEMA public TO pgindexai"

    # Ownership transfer (tables + indexes) to pgindexai.
    # NB: re-applied after setup_demo in main() — the demo schema is
    # created BY postgres and must be re-attributed so the daemon can
    # execute DDL on it (otherwise: permission denied on CREATE/DROP INDEX).
    transfer_ownership
}

ensure_hba() {
    step "pg_hba.conf: targeted line for pgindexai (peer)..."
    local hba="${PG_HBA:-/etc/postgresql/16/main/pg_hba.conf}"
    # pg_hba.conf: TYPE  DATABASE  USER  METHOD — the daemon connects to the
    # database pgindexai_demo (not a database « pgindexai ») → DATABASE = « all ».
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
    local lib="pg_stat_statements"

    if [[ -n "$(pg_show "SELECT 1 FROM pg_extension WHERE extname = '$lib'")" ]]; then
        info "pg_stat_statements is active"
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
    if [[ -z "$(pg_show "SELECT 1 FROM pg_extension WHERE extname = '$lib'")" ]]; then
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
        run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "CREATE EXTENSION IF NOT EXISTS $lib"
        info "pg_stat_statements enabled"
    fi
}

setup_postgresql() {
    ensure_demo_db
    # Order matters (scram → peer migration): 1. pg_ident map + 2. pg_hba
    # peer line (+ reload) FIRST, then 3. setup_role — it clears the role
    # password only AFTER a real peer connection as TARGET_USER is verified.
    ensure_pg_ident
    ensure_hba
    setup_role
    ensure_pg_stat_statements || true
}

# ─────────────────────────────────────────────
# Step 2: Compilation & installation
# ─────────────────────────────────────────────
build_and_install() {
    step "Compiling pgindexai..."
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # Under root: compile AS the target user (cargo in their home, target/ in their name)
    if [[ "$(id -u)" -eq 0 ]]; then
        local cargo_exe
        if command -v cargo &>/dev/null; then
            cargo_exe="$(command -v cargo)"
        elif [[ -n "${TARGET_HOME:-}" && -x "${TARGET_HOME}/.cargo/bin/cargo" ]]; then
            cargo_exe="${TARGET_HOME}/.cargo/bin/cargo"
        else
            cargo_exe="cargo"
        fi
        ( cd "$repo_dir" && run sudo -u "$TARGET_USER" "$cargo_exe" build --release )
    else
        ( cd "$repo_dir" && run cargo build --release )
    fi
    info "Compilation finished"

    step "Installing the binary → $BIN_PATH"
    run sudo cp "$repo_dir/target/release/pgindexai" "$BIN_PATH"
    run sudo chmod 755 "$BIN_PATH"

    step "Creating directories..."
    run mkdir -p "$PGINDEXAI_DIR" "$CONFIG_DIR"
    run sudo mkdir -p /var/log/pgindexai
    if [[ "$(id -u)" -eq 0 ]]; then
        run chown "$TARGET_USER" "$PGINDEXAI_DIR" "$CONFIG_DIR" /var/log/pgindexai
    fi
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
  database: "pgindexai_demo"
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
  rebuild_fragmented: true

# Decision thresholds
thresholds:
  cycle_seconds: 60        # Analysis cycle frequency
  unused_minutes: 10080    # Observation window (7 days) before DROP of an unused index
  fragmentation_pct: 70    # % of fragmentation triggering a REINDEX CONCURRENTLY
  min_sargable_rows: 100   # Min. executions (pg_stat_statements.calls) for a SARGable index
  min_confidence: 0.7      # Minimum Laya confidence to apply a decision
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
ExecStart=$BIN_PATH daemon --config $CONFIG_FILE
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
# Step 4b: Laya decision engine (optional — required to apply DDL)
# ─────────────────────────────────────────────
# Laya is an EXTERNAL binary (not built by this script). When it is found
# on the machine (new location first, legacy 'laya-sql' layout after),
# install.sh services it as '$LAYA_SERVICE_NAME' (systemd, hardened).
# When it is missing, the install CONTINUES: the daemon runs in
# observation mode (proposals generated, no DDL applied) until Laya is
# reachable — the "no-fallback" policy guarantees the daemon never
# decides on its own.
setup_laya_service() {
    step "Laya decision engine (optional — required to apply DDL)..."

    # ── Locate the binary ────────────────────────────────────────────
    local laya_bin="" cand
    for cand in \
        "${TARGET_HOME}/.local/share/pgindexai/laya" \
        "${TARGET_HOME}/.local/share/laya-sql/laya"
    do
        [[ -x "$cand" ]] && { laya_bin="$cand"; break; }
    done

    # ── Locate the model (root dir containing rl_agent_config.json) ──
    local laya_model=""
    for cand in \
        "${TARGET_HOME}/.local/share/pgindexai/models/typed-decisions/typed-decisions" \
        "${TARGET_HOME}/.local/share/pgindexai/models/typed-decisions" \
        "${TARGET_HOME}/.local/share/laya-sql/models/typed-decisions/typed-decisions" \
        "${TARGET_HOME}/.local/share/laya-sql/models/typed-decisions"
    do
        [[ -f "$cand/rl_agent_config.json" ]] && { laya_model="$cand"; break; }
    done

    if [[ -z "$laya_bin" || -z "$laya_model" ]]; then
        warn "Laya not found (binary: ${laya_bin:-missing} — model: ${laya_model:-missing})."
        warn "The daemon will run in OBSERVATION MODE (no DDL) until Laya is"
        warn "reachable — 'no fallback' policy: the daemon never decides alone."
        warn "Manual start: laya --model <DIR> --cpu --server --port $LAYA_PORT"
        return 0
    fi

    info "Laya found — binary: $laya_bin"
    info "             model : $laya_model"

    local service_file="/etc/systemd/system/${LAYA_SERVICE_NAME}.service"
    if [[ -f "$service_file" ]]; then
        if ! ask "Existing Laya service → $service_file. Replace it? [Y/n]" Y; then
            return 0
        fi
    fi

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} write $service_file (User=$TARGET_USER, MemoryMax=4G, CPUQuota=100%)"
        echo -e "${CYAN}[dry-run]${NC} sudo systemctl enable --now $LAYA_SERVICE_NAME"
        return 0
    fi

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
ExecStart=$laya_bin --model $laya_model --cpu --server --port $LAYA_PORT
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
ReadWritePaths=$laya_model
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
            info "Laya service is active (http://127.0.0.1:${LAYA_PORT}/health OK)"
            return 0
        fi
        sleep 2
    done
    error "Laya is not answering after 30 s — check: sudo journalctl -u $LAYA_SERVICE_NAME -n 30"
    return 1
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

# ─────────────────────────────────────────────
# Step 5: Startup
# ─────────────────────────────────────────────
start_daemon() {
    step "Starting the daemon..."
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
    step "Installing the demo schema..."
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

    if ! sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -tAc \
        "SELECT 1 FROM information_schema.tables WHERE table_name = 'customers' LIMIT 1" 2>/dev/null | grep -q 1; then
        if [[ -f "$repo_dir/sql/demo_schema.sql" ]]; then
            run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -v ON_ERROR_STOP=1 -f "$repo_dir/sql/demo_schema.sql"
            info "Demo schema installed"
        fi
        if [[ -f "$repo_dir/sql/demo_workload.sql" ]]; then
            run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -v ON_ERROR_STOP=1 -f "$repo_dir/sql/demo_workload.sql" || warn "Partial demo workload"
        fi
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
    echo "  Binary     : $BIN_PATH"
    echo "  Config     : $CONFIG_FILE  (0600, password: null)"
    echo "  Auth       : peer — pg_ident map '$PG_IDENT_MAP: $TARGET_USER → pgindexai' (no password)"
    echo "  Logs       : /var/log/pgindexai/"
    echo "  Service    : systemctl status $SERVICE_NAME"
    echo "  Laya       : systemctl status $LAYA_SERVICE_NAME  (decision engine — required to apply DDL)"
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
    setup_postgresql
    echo ""
    build_and_install
    echo ""
    setup_config
    echo ""
    setup_systemd
    echo ""
    # Optional component: a missing Laya must NEVER abort the install —
    # the daemon simply stays in observation mode (no DDL) until Laya is
    # reachable. A Laya health failure only downgrades the final status.
    setup_laya_service || true
    echo ""
    local daemon_ok=true
    start_daemon || daemon_ok=false
    echo ""
    setup_demo || true
    # The demo schema is created BY postgres (after setup_role): re-apply
    # the ownership transfer so pgindexai can execute DDL on it.
    transfer_ownership
    echo ""
    verify
    # If the daemon is not active, the installation cannot declare itself
    # finished (previously: masked failure — the script exited « success »
    # while the service was in a crash loop).
    $daemon_ok
}

main "$@"
