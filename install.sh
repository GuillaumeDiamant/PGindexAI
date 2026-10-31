#!/usr/bin/env bash
# ============================================================================
# install.sh — Installation sécurisée de PGindexAI
#
# Prérequis :
#   - PostgreSQL 16+ (binaire psql + cluster fonctionnel)
#   - Rust toolchain (cargo)
#   - Accès sudo
#
# Usage :
#   curl -fsSL https://raw.githubusercontent.com/GuillaumeDiamant/PGindexAI/main/install.sh | bash
#   # ou
#   ./install.sh [--dry-run]
#
# Garanties :
#   - Rôle PostgreSQL dédié NON superutilisateur (pgindexai, auth scram-sha-256)
#   - pg_hba : ligne CIBLÉE (local pgindexai pgindexai scram-sha-256), jamais
#     de 'local all all trust'
#   - shared_preload_libraries est fUSIONNÉ, jamais écrasé
#   - Config 0600, mot de passe généré (openssl rand), jamais echo'ed en log
#   - Pas d'eval, pas de commande interprétée en chaîne
#   - Unit systemd durcie (MemoryMax, CPUQuota, NoNewPrivileges, …)
# ============================================================================
set -euo pipefail

# ─────────────────────────────────────────────
# Couleurs & logs
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
# Variables globales
# ─────────────────────────────────────────────
DRY_RUN=false
POSTGRES_USER="${POSTGRES_USER:-postgres}"

# Utilisateur cible : celui qui possède le daemon, la config et le dépôt.
# - script en non-root → l'utilisateur courant
# - script via sudo    → $SUDO_USER (sinon : propriétaire du répertoire courant)
# NB : sous sudo, $HOME/$USER pointent vers root — il faut donc les redériver.
if [[ "$(id -u)" -ne 0 ]]; then
    TARGET_USER="${USER:-$(whoami)}"
elif [[ -n "${SUDO_USER:-}" ]]; then
    TARGET_USER="$SUDO_USER"
else
    TARGET_USER="$(stat -c %U . 2>/dev/null || true)"
fi
if [[ -z "${TARGET_USER:-}" ]]; then
    error "Impossible de déterminer l'utilisateur cible (USER / SUDO_USER absents)."
    exit 1
fi
TARGET_HOME="$(getent passwd "$TARGET_USER" 2>/dev/null | cut -d: -f6 || true)"

PGINDEXAI_DIR="${TARGET_HOME}/.local/share/pgindexai"
CONFIG_DIR="${TARGET_HOME}/.config/PGindexAI"
CONFIG_FILE="$CONFIG_DIR/PGindexAI.yaml"
BIN_PATH="/usr/local/bin/pgindexai"
SERVICE_NAME="pgindexai"
PGINDEXAI_PW=""          # mot de passe du rôle pgindexai (jamais affiché)
PG_CONF=""
PG_HBA=""
PG_SSL="off"

# ─────────────────────────────────────────────
# Analyse des arguments
# ─────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) warn "Argument inconnu : $arg (ignoré)" ;;
    esac
done

# run CMD [ARGS...] — sans eval : chaque argument est passé tel quel
run() {
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} $*"
    else
        "$@"
    fi
}

# ask "question" [défaut Y|N] — lit /dev/tty (fonctionne même via curl | bash),
# repli sur la valeur par défaut si aucune TTY n'est disponible.
ask() {
    local prompt="$1" def="${2:-Y}" answer=""
    if [[ ! -c /dev/tty ]]; then
        [[ "${def:0:1}" == [yY] ]]
        return
    fi
    read -r -p "$prompt " answer < /dev/tty 2>/dev/null || answer=""
    [[ "${answer:-$def}" =~ ^[Yy] ]]
}

# pg_show "SQL" — exécute une requête en -tAc en tant que $POSTGRES_USER
# (via sudo -u, sans mot de passe). Renvoie une chaîne vide en cas d'échec.
pg_show() {
    sudo -u "$POSTGRES_USER" psql -tAc "$1" 2>/dev/null || true
}

# ─────────────────────────────────────────────
# Prérequis
# ─────────────────────────────────────────────
check_sudo() {
    step "Vérification de l'accès sudo..."
    if sudo -n true 2>/dev/null; then
        info "sudo disponible (sans mot de passe)"
    else
        warn "sudo requis — le script vous demandera votre mot de passe"
    fi
}

check_postgres() {
    step "Vérification de PostgreSQL..."
    if ! command -v psql &>/dev/null; then
        error "psql introuvable — installez PostgreSQL 16+"
        exit 1
    fi
    local version
    version="$(psql --version 2>/dev/null | grep -oE '[0-9]+' | head -1 || echo 0)"
    if [[ "${version:-0}" -ge 16 ]]; then
        info "PostgreSQL $version détecté"
    else
        warn "PostgreSQL 16+ requis (version ${version:-inconnue} trouvée)"
    fi

    # Détection des fichiers de config via le serveur lui-même (pas de chemin codé)
    PG_CONF="$(pg_show "SHOW config_file" | tr -d '[:space:]')"
    PG_HBA="$(pg_show "SHOW hba_file" | tr -d '[:space:]')"
    PG_SSL="$(pg_show "SHOW ssl" | tr -d '[:space:]')"
    if [[ -n "$PG_CONF" ]]; then
        info "postgresql.conf : $PG_CONF"
        info "pg_hba.conf     : ${PG_HBA:-?}"
        info "ssl             : ${PG_SSL:-?}"
    else
        warn "Impossible de lire l'état du serveur (sudo -u $POSTGRES_USER psql ?)."
        warn "La configuration automatique sera limitée."
    fi
}

check_rust() {
    step "Vérification de Rust..."
    # cargo peut vivre dans le home de l'utilisateur cible (absent du PATH de root)
    local cargo_bin=""
    if command -v cargo &>/dev/null; then
        cargo_bin="$(command -v cargo)"
    elif [[ -n "${TARGET_HOME:-}" && -x "${TARGET_HOME}/.cargo/bin/cargo" ]]; then
        cargo_bin="${TARGET_HOME}/.cargo/bin/cargo"
        export PATH="${TARGET_HOME}/.cargo/bin:${PATH}"
    fi
    if [[ -n "$cargo_bin" ]]; then
        info "Rust toolchain détecté ($cargo_bin)"
        return 0
    fi

    if ask "Rust introuvable. Installer via rustup ? [Y/n]" Y; then
        local rustup_script
        rustup_script="$(mktemp)"
        run curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$rustup_script"
        # Installer POUR l'utilisateur cible (pas root) : le daemon tourne sous ce compte.
        # NB : le drapeau « -s » n'existe plus dans rustup-init récent → « -y » suffit.
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
        info "rustup installé (pour l'utilisateur $TARGET_USER)"
    else
        error "Rust est requis pour compiler pgindexai"
        exit 1
    fi
}

# ─────────────────────────────────────────────
# Étape 1 : PostgreSQL (rôle dédié, hba ciblée, pg_stat_statements)
# ─────────────────────────────────────────────
ensure_demo_db() {
    if [[ -n "$(pg_show "SELECT 1 FROM pg_database WHERE datname = 'pgindexai_demo'")" ]]; then
        info "Base 'pgindexai_demo' présente"
        return
    fi
    step "Création de la base 'pgindexai_demo'..."
    run sudo -u "$POSTGRES_USER" psql -qc "CREATE DATABASE pgindexai_demo"
}

setup_role() {
    step "Configuration du rôle dédié 'pgindexai' (non superutilisateur)..."

    local role_exists
    role_exists="$(pg_show "SELECT 1 FROM pg_roles WHERE rolname = 'pgindexai'")"

    if [[ -n "$role_exists" ]]; then
        info "Rôle 'pgindexai' existe déjà"
        # Si une config existante contient le mot de passe, le réappliquer
        # (garantie de cohérence rôle ↔ config)
        if [[ -f "$CONFIG_FILE" ]]; then
            local existing_pw
            existing_pw="$(sed -n 's/^[[:space:]]*password:[[:space:]]*//p' "$CONFIG_FILE" | head -1 | tr -d '"' | tr -d "'" || true)"
            if [[ -n "$existing_pw" ]]; then
                PGINDEXAI_PW="$existing_pw"
                run sudo -u "$POSTGRES_USER" psql -qc "ALTER ROLE pgindexai PASSWORD '${existing_pw}'"
                info "Mot de passe existant conservé (re-appliqué au rôle)"
            fi
        fi
    else
        PGINDEXAI_PW="$(openssl rand -hex 16)"
        run sudo -u "$POSTGRES_USER" psql -qc "CREATE ROLE pgindexai LOGIN PASSWORD '${PGINDEXAI_PW}'"
        info "Rôle 'pgindexai' créé (mot de passe généré — conservé dans la config 0600)"
    fi

    if [[ -z "$PGINDEXAI_PW" ]]; then
        # Rôle existant sans mot de passe connu : en générer un et l'imposer
        PGINDEXAI_PW="$(openssl rand -hex 16)"
        run sudo -u "$POSTGRES_USER" psql -qc "ALTER ROLE pgindexai PASSWORD '${PGINDEXAI_PW}'"
        warn "Mot de passe régénéré pour 'pgindexai' (aucun trouvé dans la config existante)"
    fi

    run sudo -u "$POSTGRES_USER" psql -qc "GRANT CONNECT ON DATABASE pgindexai_demo TO pgindexai"

    # Transfert de propriété des objets de démo vers pgindexai :
    # CREATE INDEX / DROP INDEX / REINDEX exigent la propriété de la table/index.
    run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "
DO \$\$
DECLARE r RECORD;
BEGIN
    FOR r IN
        SELECT c.oid::regclass AS cls
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.relkind IN ('r','v','m')
          AND n.nspname NOT IN ('pg_catalog','information_schema')
          AND c.relowner <> (SELECT oid FROM pg_authid WHERE rolname = 'pgindexai')
    LOOP
        EXECUTE format('ALTER TABLE %s OWNER TO pgindexai', r.cls);
    END LOOP;
    FOR r IN
        SELECT ic.oid::regclass AS cls
        FROM pg_class ic
        JOIN pg_namespace n ON n.oid = ic.relnamespace
        WHERE ic.relkind = 'i'
          AND n.nspname NOT IN ('pg_catalog','information_schema')
          AND ic.relowner <> (SELECT oid FROM pg_authid WHERE rolname = 'pgindexai')
    LOOP
        EXECUTE format('ALTER INDEX %s OWNER TO pgindexai', r.cls);
    END LOOP;
END
\$\$;
" 2>/dev/null || warn "Transfert de propriété partiel (objets déjà détenus par pgindexai ?)"
    info "Droits de propriété appliqués (tables + index de pgindexai_demo → pgindexai)"
}

ensure_hba() {
    step "pg_hba.conf : ligne ciblée pour pgindexai (scram-sha-256)..."
    local hba="${PG_HBA:-/etc/postgresql/16/main/pg_hba.conf}"
    local line="local   pgindexai   pgindexai   scram-sha-256"

    if [[ ! -f "$hba" ]]; then
        warn "pg_hba introuvable à '$hba' — ajout manuel requis : $line"
        return 1
    fi

    if sudo grep -Eq "^[[:space:]]*local[[:space:]]+pgindexai[[:space:]]+pgindexai" "$hba"; then
        info "Ligne pour pgindexai déjà présente"
    else
        # pg_hba est « first match wins » : la ligne dédiée est insérée EN TÊTE
        # pour qu'elle prévale sur toute ligne large ('local all all ...').
        local old_content
        if $DRY_RUN; then
            old_content="<contenu actuel de pg_hba.conf>"
        else
            old_content="$(sudo cat "$hba")"
        fi
        run sudo tee "$hba" >/dev/null <<EOF
$line

$old_content
EOF
        if $DRY_RUN; then
            echo -e "  ${CYAN}(dry-run : la ligne serait insérée en tête de pg_hba.conf)${NC}"
        else
            info "Ligne insérée en tête de pg_hba.conf (priorité first-match)"
        fi
    fi

    if sudo grep -Eq "^[[:space:]]*local[[:space:]]+all[[:space:]]+all[[:space:]]+trust" "$hba"; then
        warn "Attention : 'local all all trust' est présent dans pg_hba.conf."
        warn "La ligne dédiée à pgindexai la précède (donc appliquée), mais retirez la"
        warn "ligne trust pour durcir le cluster (tous les autres rôles sont concernés)."
    fi
}

ensure_pg_stat_statements() {
    step "Activation de pg_stat_statements..."
    local lib="pg_stat_statements"

    if [[ -n "$(pg_show "SELECT 1 FROM pg_extension WHERE extname = '$lib'")" ]]; then
        info "pg_stat_statements est actif"
        return 0
    fi

    # Valeur effective en cours
    local current
    current="$(pg_show "SHOW shared_preload_libraries" || echo "")"
    if echo " $current " | tr ',' ' ' | tr -s ' ' | grep -qw "$lib"; then
        info "Déjà dans shared_preload_libraries (un redémarrage peut suffire)"
    fi

    local conf="${PG_CONF:-/etc/postgresql/16/main/postgresql.conf}"
    if [[ -z "$PG_CONF" || ! -r "$conf" ]]; then
        warn "Impossible de modifier '$conf' automatiquement."
        warn "Action manuelle : shared_preload_libraries += '$lib', redémarrer PG,"
        warn "puis CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"
        return 1
    fi

    local line
    line="$(grep -E "^[[:space:]]*shared_preload_libraries[[:space:]]*=" "$conf" | tail -n1 || true)"

    if grep -qw "$lib" <<<"$line"; then
        info "Déjà déclaré dans $conf (redémarrage nécessaire)"
    elif [[ -n "$line" ]]; then
        # FUSION : on conserve les bibliothèques existantes, on ajoute la nôtre
        local val newval
        val="$(sed -E "s/^[[:space:]]*shared_preload_libraries[[:space:]]*=[[:space:]]*//" <<<"$line" \
               | sed -E "s/[[:space:]]*\$//" | sed -E "s/^['\"]//; s/['\"]\$//" | xargs)"
        newval="${val:+$val,}$lib"
        run sudo sed -i -E "s|^[[:space:]]*shared_preload_libraries[[:space:]]*=.*|shared_preload_libraries = '${newval}'|" "$conf"
        info "Fusion : shared_preload_libraries = '${newval}'"
    else
        run sudo bash -c "printf '\n# Ajouté par pgindexai install.sh\nshared_preload_libraries = '%s'\n' \"$lib\" >> \"$conf\""
        info "Ajout : shared_preload_libraries = '$lib'"
    fi

    # Redémarrage (le préfixage de pg_hba n'exige pas de redémarrage : c'est relu à chaque connexion)
    if [[ -z "$(pg_show "SELECT 1 FROM pg_extension WHERE extname = '$lib'")" ]]; then
        warn "Redémarrage de PostgreSQL (shared_preload_libraries modifié)..."
        run sudo systemctl restart postgresql
        # Polling d'activité (30 s)
        for _ in {1..15}; do
            if [[ "$(pg_show "SELECT 1")" == "1" ]]; then
                break
            fi
            sleep 2
        done
        if [[ "$(pg_show "SELECT 1")" != "1" ]]; then
            error "PostgreSQL n'a pas redémarré — vérifiez : sudo journalctl -u postgresql"
            return 1
        fi
        run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -qc "CREATE EXTENSION IF NOT EXISTS $lib"
        info "pg_stat_statements activé"
    fi
}

setup_postgresql() {
    ensure_demo_db
    setup_role
    ensure_hba
    ensure_pg_stat_statements || true
}

# ─────────────────────────────────────────────
# Étape 2 : Compilation & installation
# ─────────────────────────────────────────────
build_and_install() {
    step "Compilation de pgindexai..."
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # Sous root : compiler SOUS l'utilisateur cible (cargos de son home, target/ à son nom)
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
    info "Compilation terminée"

    step "Installation du binaire → $BIN_PATH"
    run sudo cp "$repo_dir/target/release/pgindexai" "$BIN_PATH"
    run sudo chmod 755 "$BIN_PATH"

    step "Création des répertoires..."
    run mkdir -p "$PGINDEXAI_DIR" "$CONFIG_DIR"
    run sudo mkdir -p /var/log/pgindexai
    if [[ "$(id -u)" -eq 0 ]]; then
        run chown "$TARGET_USER" "$PGINDEXAI_DIR" "$CONFIG_DIR" /var/log/pgindexai
    fi
    info "Répertoires prêts"
}

# ─────────────────────────────────────────────
# Étape 3 : Configuration (0600)
# ─────────────────────────────────────────────
setup_config() {
    step "Génération de la configuration..."

    if [[ -f "$CONFIG_FILE" ]]; then
        if ask "Configuration existante → $CONFIG_FILE. La conserver ? [Y/n]" Y; then
            info "Configuration conservée"
            # S'assurer du 0600 et du mot de passe cohérent
            run chmod 600 "$CONFIG_FILE"
            if ! grep -q "password:" "$CONFIG_FILE"; then
                warn "Aucun 'password:' dans la config existante — requis si pg_hba exige scram."
            fi
            return
        fi
        cp "$CONFIG_FILE" "${CONFIG_FILE}.bak"
        warn "Sauvegarde → ${CONFIG_FILE}.bak"
    fi

    # Déterminer le mode TLS par défaut à partir de l'état du serveur
    local ssl_mode="disable" ssl_cert_comment=""
    if [[ "$PG_SSL" == "on" ]]; then
        ssl_mode="require"
        ssl_cert_comment=$'\n  # Pour un CA auto-signé (snakeoil, interne), passez plutôt :\n  #   ssl_mode: "verify-full"\n  #   ssl_root_cert: "/etc/ssl/certs/votre-ca.pem"  # chemin PEM\n'
    fi

    run mkdir -p "$CONFIG_DIR"

    # Le mot de passe est injecté directement (pas d'affichage)
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} écrire $CONFIG_FILE (password: <généré>, ssl_mode: $ssl_mode)"
        PGINDEXAI_PW="${PGINDEXAI_PW:-<généré>}"
    else
        cat > "$CONFIG_FILE" <<EOF
# Configuration du daemon PGindexAI
# Générée par install.sh le $(date '+%F %T')
# Droits : 0600 (contient un mot de passe)

postgresql:
  host: "/var/run/postgresql"   # Socket Unix local (TLS inapplicable sur socket → disable)
  port: 5432
  user: "pgindexai"
  database: "pgindexai_demo"
  password: "${PGINDEXAI_PW}"
  ssl_mode: "${ssl_mode}"
  # ssl_root_cert: "/etc/ssl/certs/votre-ca.pem"   # CA auto-signée (verify-ca/verify-full)
$ssl_cert_comment
laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

# Contrôle des actions DDL — mettre false pour désactiver une action
actions:
  create_index: true
  drop_unused: true
  rebuild_fragmented: true

# Seuils de décision
thresholds:
  cycle_seconds: 60        # Fréquence du cycle d'analyse
  unused_minutes: 10080    # Fenêtre d'observation (7 jours) avant DROP d'un index inutilisé
  fragmentation_pct: 70    # % de fragmentation déclenchant un REINDEX CONCURRENTLY
  min_sargable_rows: 100   # Min. d'exécutions (pg_stat_statements.calls) pour un index SARGable
  min_confidence: 0.7      # Confiance minimale de Laya pour appliquer une décision
  max_proposals_per_cycle: 20  # Budget de propositions par cycle

# Périmètre d'analyse (multi-schéma)
analysis:
  # Liste des schémas à analyser.
  # - liste vide = tous les schémas non-système (défaut)
  # - exemple : ["public", "billing"]
  schemas: []

# Pool de connexions PostgreSQL
pg_pool:
  max_size: 8
  wait_timeout_ms: 30000

# Densité cible par type de colonne (tuples/page visés, estimation de taille d'index)
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

# Historisation des opérations d'index
index_logging:
  enabled: true
  verbosity: "minimal"   # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760  # rotation à 10 MB
  max_files: 5
EOF
        chmod 600 "$CONFIG_FILE"
        info "Configuration → $CONFIG_FILE (0600)"
    fi
    warn "Vérifiez le fichier avant de lancer le daemon (URL Laya, schémas, seuils)"
}

# ─────────────────────────────────────────────
# Étape 4 : Service systemd durci
# ─────────────────────────────────────────────
setup_systemd() {
    step "Installation du service systemd..."
    local service_file="/etc/systemd/system/${SERVICE_NAME}.service"

    if [[ -f "$service_file" ]]; then
        if ! ask "Service existant → $service_file. Le remplacer ? [Y/n]" Y; then
            return
        fi
    fi

    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} écrire $service_file (User=$TARGET_USER, MemoryMax=512M, CPUQuota=100%)"
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

# Ressources
MemoryMax=512M
CPUQuota=100%

# Durcissement
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
    info "Service installé et activé"
}

# ─────────────────────────────────────────────
# Étape 5 : Démarrage
# ─────────────────────────────────────────────
start_daemon() {
    step "Démarrage du daemon..."
    if $DRY_RUN; then
        echo -e "${CYAN}[dry-run]${NC} sudo systemctl restart $SERVICE_NAME"
        return
    fi
    sudo systemctl restart "$SERVICE_NAME"

    # Polling d'activité (30 s)
    for _ in {1..15}; do
        if sudo systemctl is-active --quiet "$SERVICE_NAME"; then
            info "Daemon PGindexAI est actif"
            return 0
        fi
        sleep 2
    done

    warn "Le daemon n'est pas actif — vérifiez : sudo journalctl -u $SERVICE_NAME -n 30"
    return 0
}

# ─────────────────────────────────────────────
# Étape 6 : Démo (schéma + workload)
# ─────────────────────────────────────────────
setup_demo() {
    step "Installation du schéma de démo..."
    local repo_dir
    repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

    if ! sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -tAc \
        "SELECT 1 FROM information_schema.tables WHERE table_name = 'customers' LIMIT 1" 2>/dev/null | grep -q 1; then
        if [[ -f "$repo_dir/sql/demo_schema.sql" ]]; then
            run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -v ON_ERROR_STOP=1 -f "$repo_dir/sql/demo_schema.sql"
            info "Schéma de démo installé"
        fi
        if [[ -f "$repo_dir/sql/demo_workload.sql" ]]; then
            run sudo -u "$POSTGRES_USER" psql -d pgindexai_demo -v ON_ERROR_STOP=1 -f "$repo_dir/sql/demo_workload.sql" || warn "Workload de démo partiel"
        fi
    else
        info "Schéma de démo déjà installé"
    fi
}

# ─────────────────────────────────────────────
# Vérification finale
# ─────────────────────────────────────────────
verify() {
    echo ""
    echo "============================================"
    echo "  Installation PGindexAI terminée"
    echo "============================================"
    echo ""
    echo "  Binaire    : $BIN_PATH"
    echo "  Config     : $CONFIG_FILE  (0600)"
    echo "  Logs       : /var/log/pgindexai/"
    echo "  Service    : systemctl status $SERVICE_NAME"
    echo "  Daemon     : journalctl -u $SERVICE_NAME -f"
    echo ""
    echo "  Commandes utiles :"
    echo "    pgindexai run --dry-run   → cycle unique, AUCUNE DDL"
    echo "    pgindexai daemon          → mode continu"
    echo "    journalctl -u $SERVICE_NAME -n 50"
    echo "    tail -f /var/log/pgindexai/index_operations.log"
    echo ""
    if $DRY_RUN; then
        echo "  📋 Mode dry-run — aucune modification n'a été appliquée"
        echo ""
    fi
    echo "  Rappel durcissement : supprimez 'local all all trust' de pg_hba.conf"
    echo "  (si présent) pour que SEUL le rôle pgindexai reste accessible en local."
    echo ""
}

# ─────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────
main() {
    echo ""
    echo "============================================"
    echo "  PGindexAI Installer (sécurisé)"
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
    start_daemon
    echo ""
    setup_demo || true
    echo ""
    verify
}

main "$@"
