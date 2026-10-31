#!/usr/bin/env bash
# ============================================================================
# uninstall.sh — Désinstallation de PGindexAI
#
# Usage :
#   sudo ./uninstall.sh [--dry-run] [--purge] [--clean-legacy]
#
# Par défaut : arrête et désactive le service, supprime l'unité systemd, le
# binaire, la config, les données et les logs de l'utilisateur. CONSERVE la
# base pgindexai_demo, le rôle pgindexai et la ligne pg_hba (réinstallable,
# données préservées).
#
# Options :
#   --dry-run        simulation complète, aucune modification
#   --purge          SUPPRIME AUSSI la base + le rôle + la ligne pg_hba
#                    (irréversible, confirmation demandée)
#   --clean-legacy   retire aussi les vestiges de l'ancien nom (laya-sql)
#
# Garde-fous :
#   - shared_preload_libraries et pg_stat_statements ne sont JAMAIS modifiés
#     (extension partagée — retrait manuel si réellement souhaité)
#   - sans terminal contrôleur, la confirmation --purge passe au défaut (Y)
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
PURGE=false
CLEAN_LEGACY=false
POSTGRES_USER="${POSTGRES_USER:-postgres}"

# Utilisateur cible : celui qui possède la config, les données et les logs.
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
if [[ -z "${TARGET_HOME:-}" ]]; then
    error "Impossible de déterminer le home de '$TARGET_USER'."
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
# Analyse des arguments
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
# repli sur la valeur par défaut si aucun terminal contrôleur n'est disponible.
# NB : ne JAMAIS rediriger le stderr de `read` : `read -p` écrit le prompt dessus,
# et 2>/dev/null rendrait la question invisible pour l'utilisateur.
ask() {
    local prompt="$1" def="${2:-Y}" answer=""
    if ! ( : </dev/tty ) 2>/dev/null; then
        [[ "${def:0:1}" == [yY] ]]
        return
    fi
    IFS= read -r -p "$prompt " answer </dev/tty || answer=""
    [[ "${answer:-$def}" =~ ^[Yy] ]]
}

# pg_show "SQL" — requête en -tAc en tant que $POSTGRES_USER (vide si échec)
pg_show() {
    sudo -u "$POSTGRES_USER" psql -tAc "$1" 2>/dev/null || true
}

# hba_file — chemin effectif de pg_hba.conf (SHOW hba_file, sinon défaut)
hba_file() {
    local hba
    hba="$(pg_show 'SHOW hba_file' | tr -d '[:space:]')"
    printf '%s\n' "${hba:-$PG_HBA_DEFAULT}"
}

# ─────────────────────────────────────────────
# Étape 1 : service systemd + éventuel processus orphelin
# ─────────────────────────────────────────────
stop_service() {
    step "Service systemd..."
    if [[ -f "$SERVICE_FILE" ]]; then
        run sudo systemctl stop "$SERVICE_NAME" || warn "Arrêt impossible (déjà inactif ?)"
        run sudo systemctl disable "$SERVICE_NAME" 2>/dev/null || true
        run sudo rm -f "$SERVICE_FILE"
        run sudo systemctl daemon-reload
        run sudo systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true
        info "Service arrêté, désactivé, unité supprimée"
    else
        info "Aucune unité systemd trouvée ($SERVICE_FILE)"
    fi

    # Processus démarré à la main (hors systemd) ?
    if command -v pgrep &>/dev/null && pgrep -x "$SERVICE_NAME" >/dev/null 2>&1; then
        warn "Processus $SERVICE_NAME en cours — arrêt..."
        run sudo pkill -x "$SERVICE_NAME" || true
    fi
}

# ─────────────────────────────────────────────
# Étape 2 : binaire
# ─────────────────────────────────────────────
remove_binary() {
    step "Binaire..."
    if [[ -f "$BIN_PATH" ]]; then
        run sudo rm -f "$BIN_PATH"
        info "Binaire supprimé : $BIN_PATH"
    else
        info "Aucun binaire trouvé ($BIN_PATH)"
    fi
}

# ─────────────────────────────────────────────
# Étape 3 : config, données, logs de l'utilisateur
# ─────────────────────────────────────────────
remove_dirs() {
    step "Config, données et logs..."
    run sudo rm -rf "$CONFIG_DIR"
    run sudo rm -rf "$PGINDEXAI_DIR"
    run sudo rm -rf "$LOG_DIR"
    info "Supprimés : $CONFIG_DIR, $PGINDEXAI_DIR, $LOG_DIR"
}

# ─────────────────────────────────────────────
# Étape 4 (option --purge) : base, rôle, ligne pg_hba
# ─────────────────────────────────────────────
remove_pg_objects() {
    step "Objets PostgreSQL (base + rôle + pg_hba)..."
    if ! ask "Supprimer DÉFINITIVEMENT la base 'pgindexai_demo' (toutes ses données) ET le rôle 'pgindexai' ? [Y/n]" Y; then
        warn "Suppression annulée : base et rôle conservés."
        return 0
    fi
    # Ordre : base d'abord (détient les tables/index), puis le rôle.
    run sudo -u "$POSTGRES_USER" psql -qc "DROP DATABASE IF EXISTS pgindexai_demo"
    run sudo -u "$POSTGRES_USER" psql -qc "DROP ROLE IF EXISTS pgindexai"
    info "Base et rôle supprimés"

    # Ligne dédiée (forme corrigée « local all pgindexai » ET variante
    # défectueuse « local pgindexai pgindexai » — colonne DATABASE).
    local hba
    hba="$(hba_file)"
    if [[ -f "$hba" ]] && sudo grep -Eq "^[[:space:]]*local[[:space:]]+(all|pgindexai_demo|pgindexai)[[:space:]]+pgindexai[[:space:]]+" "$hba"; then
        run sudo sed -i -E "/^[[:space:]]*local[[:space:]]+(all|pgindexai_demo|pgindexai)[[:space:]]+pgindexai[[:space:]]+.*$/d" "$hba"
        run sudo systemctl reload postgresql
        info "Ligne pg_hba supprimée, PostgreSQL rechargé"
    else
        info "Aucune ligne pg_hba dédiée trouvée (rien à retirer)"
    fi

    warn "pg_stat_statements est CONSERVÉ (extension partagée, shared_preload_libraries)."
    warn "Retrait manuel (si réellement souhaité) : retirer 'pg_stat_statements' de"
    warn "shared_preload_libraries dans postgresql.conf, puis redémarrer PostgreSQL."
}

# ─────────────────────────────────────────────
# Étape 5 (option --clean-legacy) : vestiges de l'ancien nom
# ─────────────────────────────────────────────
clean_legacy() {
    step "Vestiges de l'ancien nom (laya-sql)..."
    local legacy_unit="/etc/systemd/system/laya-sql.service"
    if [[ -f "$legacy_unit" ]]; then
        run sudo systemctl stop laya-sql 2>/dev/null || true
        run sudo systemctl disable laya-sql 2>/dev/null || true
        run sudo rm -f "$legacy_unit"
        run sudo systemctl daemon-reload
        info "Unité laya-sql supprimée"
    fi
    run sudo rm -f /usr/local/bin/laya-sql
    run sudo rm -rf "${TARGET_HOME}/.config/layaSQL"
    run sudo rm -rf /var/log/laya-sql
    # Objets PG legacy (déjà renommés en pgindexai* lors de la migration) — no-op si absents
    run sudo -u "$POSTGRES_USER" psql -qc "DROP DATABASE IF EXISTS laya_demo" 2>/dev/null || true
    run sudo -u "$POSTGRES_USER" psql -qc "DROP ROLE IF EXISTS laya_sql" 2>/dev/null || true
    info "Vestiges nettoyés : binaire, config, logs, objets PG éventuels"
}

# ─────────────────────────────────────────────
# Résumé
# ─────────────────────────────────────────────
verify() {
    echo ""
    echo "============================================"
    echo "  Désinstallation PGindexAI terminée"
    echo "============================================"
    echo ""
    echo "  Service    : $SERVICE_NAME — arrêté, unité supprimée"
    echo "  Binaire    : $BIN_PATH — supprimé"
    echo "  Fichiers   : config, données, logs supprimés"
    if $PURGE; then
        echo "  PostgreSQL : base pgindexai_demo + rôle pgindexai + ligne pg_hba supprimés"
    else
        echo "  PostgreSQL : CONSERVÉ (base pgindexai_demo, rôle pgindexai, ligne pg_hba)"
        echo "               → suppression complète : sudo ./uninstall.sh --purge"
    fi
    echo "  pg_stat_statements : conservé (extension partagée)"
    if $DRY_RUN; then
        echo ""
        echo "  📋 Mode dry-run — aucune modification n'a été appliquée"
    fi
    echo ""
    echo "  Réinstallation : sudo ./install.sh"
    echo ""
}

# ─────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────
main() {
    echo ""
    echo "============================================"
    echo "  PGindexAI — Désinstallation"
    echo "============================================"
    echo ""
    echo "  Utilisateur cible : $TARGET_USER"
    if $PURGE; then
        warn "Mode --purge : la base pgindexai_demo et le rôle pgindexai seront supprimés."
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
