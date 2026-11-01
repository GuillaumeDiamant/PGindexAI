# 06 — Déploiement et opérations

## 1. Installation

### Prérequis

- **PostgreSQL 16+** (idéalement avec `pg_stat_statements` activé — sans elle, le daemon passe en mode **dégradé**)
- **Rust toolchain** (pour compiler depuis les sources)
- **Systemd** (pour le service)
- **Serveur Laya** (moteur IA — requis **pour appliquer des DDL** ; sans lui le daemon reste en mode observation)

### Option A — Installateur durci (recommandé)

```bash
./install.sh                 # ou : ./install.sh --dry-run (simulation complète)
```

`install.sh` (shellcheck-clean, sans `eval`, prompts via `/dev/tty`) :
- détecte le cluster PostgreSQL via `SHOW config_file/hba_file/ssl` ;
- crée le **rôle dédié non-superutilisateur `pgindexai`** (mot de passe
  `openssl rand -hex 16`, jamais affiché) ;
- **prépend** la ligne ciblée `local all pgindexai scram-sha-256` dans
  `pg_hba.conf` (ne touche jamais aux autres lignes) ;
- **fusionne** `pg_stat_statements` dans `shared_preload_libraries` (jamais
  d'écrasement) puis redémarre PostgreSQL si besoin ;
- installe le binaire en `/usr/local/bin/pgindexai` ;
- génère `~/.config/PGindexAI/PGindexAI.yaml` en **0600** avec le mot de passe ;
- déplace la propriété des objets de démo vers `pgindexai` (si la base existe) ;
- déploie l'unité systemd **durcie** et active le service.

### Option B — Manuel

```bash
cargo build --release
sudo cp target/release/pgindexai /usr/local/bin/pgindexai

# Unité systemd : copier le template `config/pgindexai.service` puis remplacer
# les marqueurs $USER / $HOME par vos valeurs (User=, Group=, ExecStart, HOME)
sudo cp config/pgindexai.service /etc/systemd/system/pgindexai.service
sudo systemctl daemon-reload
sudo systemctl enable --now pgindexai

mkdir -p ~/.config/PGindexAI
cp config/PGindexAI.yaml ~/.config/PGindexAI/PGindexAI.yaml
chmod 0600 ~/.config/PGindexAI/PGindexAI.yaml
# Éditer ~/.config/PGindexAI/PGindexAI.yaml
```

L'unité déployée contient les durcissements : `MemoryMax=512M`,
`CPUQuota=100%`, `NoNewPrivileges=true`, `ProtectSystem=strict`,
`PrivateTmp=true`, `Restart=always`.

## 2. Démarrage du serveur Laya

```bash
laya \
  --model ~/.local/share/pgindexai/models/typed-decisions/ \
  --cpu \
  --server \
  --port 8080
```

Le serveur Laya **doit** être actif pour que le daemon applique des décisions :
politique **« sans fallback »** — si `deep_health()` échoue (Laya injoignable,
réponse invalide), **toutes** les propositions du cycle sont sautées. Sans Laya,
le daemon reste en **mode observation** : il détecte, génère et journalise les
propositions, mais ne modifie jamais la base.

Au démarrage, un **health check profond** est effectué (payload de test réel
envoyé à `/v1/systemone`, réponse validée) :

```
INFO  Laya deep health OK at http://127.0.0.1:8080 (choice=0, confidence=0.98)
```
ou

```
WARN  Laya deep health FAILED at http://127.0.0.1:8080: Laya not reachable …
WARN  Decisions will be SKIPPED (no fallback) until Laya is available.
```

## 3. Initialisation de la base de démo

```bash
# Schéma
psql -U postgres -f sql/demo_schema.sql

# Workload de test
sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql
```

## 4. Monitoring

### État du service

```bash
systemctl is-active pgindexai
systemctl status pgindexai
```

### Logs

```bash
# Journal systemd
journalctl -u pgindexai -f

# Logs récents
journalctl -u pgindexai --since "1 hour ago"

# Fichier de log des opérations (si configuré)
tail -f /var/log/pgindexai/index_operations.log
```

### Vérification PostgreSQL

```bash
# Extension pg_stat_statements
psql -U postgres -d pgindexai_demo -c "SELECT extname, extversion FROM pg_extension WHERE extname = 'pg_stat_statements';"

# Tables et index
psql -U postgres -d pgindexai_demo -c "\dt"
psql -U postgres -d pgindexai_demo -c "\di"

# Statistiques index
psql -U postgres -d pgindexai_demo -c "
SELECT schemaname, relname AS table_name, indexrelname AS index_name,
       idx_scan, idx_tup_read, pg_size_pretty(pg_relation_size(indexrelid))
FROM pg_stat_user_indexes
ORDER BY pg_relation_size(indexrelid) DESC;"

# Statistiques tables
psql -U postgres -d pgindexai_demo -c "
SELECT relname AS table_name, n_live_tup, n_dead_tup, n_mod_since_analyze
FROM pg_stat_user_tables
ORDER BY relname;"
```

## 5. Dépannage

### Laya non joignable

```
WARN  Laya deep health FAILED at http://127.0.0.1:8080: Laya not reachable …
WARN  Decisions will be SKIPPED (no fallback) until Laya is available.
WARN  Laya unavailable (…): 5 proposals SKIPPED (no fallback policy).
INFO  Applied: 0 proposal(s), skipped (no-op): 0
```

**Comportement** : le daemon ne décide **jamais** à la place de Laya —
toutes les propositions sont générées et journalisées, mais **aucune DDL
n'est appliquée** (politique « sans fallback »). Dès que Laya répond, le
daemon reprend automatiquement les décisions au cycle suivant (aucun
redémarrage nécessaire).

**Cause** : Le serveur Laya n'est pas lancé, le port est différent, ou
répond avec une structure invalide (`choice` non-chaine, `confidence` hors
`[0, 1]`).

**Solution** :
1. Vérifier que `laya --server --port 8080` est actif
2. Vérifier `laya.url` dans la config
3. Vérifier le firewall local
4. Tester la réponse : `curl -s localhost:8080/v1/systemone -d '<payload>'`

### pg_stat_statements manquant

```
WARN  pg_stat_statements extension not found.
WARN  Daemon running in DEGRADED mode: pg_stat_statements missing.
WARN  Missing-index detection is DISABLED until the extension is installed.
```

**Comportement** : le daemon passe en état **`Degraded`** (voir
`03-module-reference.md` § 2) — la détection d'index **manquants** est
désactivée (pas de source de données), mais les détecteurs d'index
**inutilisés** et **fragmentés** restent actifs.

**Cause** : L'extension n'est pas installée sur la base cible.

**Solution** :
```sql
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
```
puis redémarrer le daemon (`systemctl restart pgindexai`) pour repasser en
`Healthy`.

### Erreur de configuration

```
Config error: Cannot parse config at /path/to/PGindexAI.yaml: ...
```

**Cause** : YAML invalide (indentation, syntaxe) ou valeur hors plage
(`Config::validate()` refuse les configs dangereuses) — ou clés inconnues
(ceci n'est **pas** bloquant : avertissement, ex. `config : clé 'laya.variant'
inconnue (ignorée …)`).

**Solution** : Vérifier le YAML avec `yamllint` ou `python -c "import yaml; yaml.safe_load(open('PGindexAI.yaml'))"` ;
supprimer les clés inconnues signalées.

### Échec TLS (connexion PostgreSQL)

```
ERROR ssl_mode=verify-full mais certificat non vérifiable: InvalidCertificate(UnknownIssuer)
```

**Comportement** : la connexion est **refusée** (rustls vérifie toujours la
chaîne + le nom d'hôte — il n'existe pas de mode « accept-insecure »).

**Causes fréquentes** :
- CA auto-signée non fournie → ajouter `postgresql.ssl_root_cert: /chemin/ca.pem`
- CN/SAN du certificat ≠ `postgresql.host` (ex. `host: localhost` mais
  certificat émis pour `db.internal`) → aligner les deux
- `ssl_mode` ≠ `disable` avec un hôte **socket Unix** (`/var/run/postgresql`)
  → TLS impossible sur socket : mettre `ssl_mode: disable` pour un socket local

**Vérification** : `psql "sslmode=verify-full sslrootcert=/chemin/ca.pem" -c "SELECT ssl, version FROM pg_stat_ssl WHERE pid = pg_backend_pid();"`

### Index non détectés

**Cause** : Les statistiques ne sont pas à jour.

**Solution** :
```sql
ANALYZE;
-- ou pour une table spécifique
ANALYZE orders;
```

### Service ne démarre pas

```bash
# Vérifier les logs
journalctl -u pgindexai -n 50

# Vérifier la config (un cycle complet, sans appliquer de DDL)
pgindexai --config ~/.config/PGindexAI/PGindexAI.yaml run --dry-run

# Vérifier les permissions (le fichier doit être 0600)
ls -la ~/.config/PGindexAI/PGindexAI.yaml
```

## 6. Maintenance

### Redémarrage

```bash
sudo systemctl restart pgindexai
```

> **Après un redémarrage de PostgreSQL** : les compteurs
> `pg_stat_user_indexes` sont remis à zéro. Le daemon **sautera
> automatiquement** l'analyse « index inutilisés » tant que l'uptime du
> serveur est < `thresholds.unused_minutes` (défaut 7 jours = 10080 min) —
> aucune intervention nécessaire, ce qui évite les faux positifs de DROP.

### Mise à jour

```bash
cargo build --release
sudo cp target/release/pgindexai /usr/local/bin/pgindexai
sudo systemctl restart pgindexai
```

### Désinstallation

```bash
# Par défaut : service + binaire + config + données + logs
# (CONSERVE la base pgindexai_demo, le rôle pgindexai et la ligne pg_hba)
sudo ./uninstall.sh

# Simulation complète (aucune modification)
sudo ./uninstall.sh --dry-run

# Suppression complète : + base, rôle et ligne pg_hba (irréversible)
sudo ./uninstall.sh --purge

# + vestiges de l'ancien nom (laya-sql : binaire, config, logs, objets PG)
sudo ./uninstall.sh --clean-legacy
```

`uninstall.sh` (shellcheck-clean, sans `eval`, prompts via `/dev/tty`) :
- arrête et désactive le service, supprime l'unité (et `reset-failed`) ;
- arrête un éventuel processus orphelin (démarré à la main) ;
- supprime le binaire, la config (0600), les données et les logs ;
- avec `--purge` : `DROP DATABASE pgindexai_demo`, `DROP ROLE pgindexai`,
  retrait de la ligne dédiée de `pg_hba.conf` + reload PostgreSQL ;
- **ne modifie jamais** `shared_preload_libraries` ni l'extension
  `pg_stat_statements` (extension partagée — retrait manuel si réellement
  souhaité : retirer de `shared_preload_libraries` dans `postgresql.conf`,
  puis `sudo systemctl restart postgresql`).

## 7. Sécurité

### Contrôles d'actions

Chaque type d'action DDL est contrôlé par un flag dans la config :

| Flag | Action | Risque |
|------|--------|--------|
| `create_index: true` | CREATE INDEX (CONCURRENTLY) | Faible (index supplémentaire, construction en ligne) |
| `drop_unused: true` | DROP INDEX | Moyen (perte de capacité de requête) |
| `rebuild_fragmented: true` | REINDEX (CONCURRENTLY) | Faible (reconstruction en ligne, sans blocage) |

### Recommandations de sécurité

1. **Sans fallback** : sans Laya actif, **aucune** DDL n'est appliquée —
   la config ne peut pas contourner cette garantie (pas de mode « heuristique »)
2. **Production** : `drop_unused: false` en phase de rodage (les DROP restent
   soumis à la décision de Laya même avec le flag actif)
3. **min_confidence** : augmenter à `0.9` en production pour éviter les faux positifs
4. **TLS** : pour une connexion TCP distante, `ssl_mode: verify-full` (ou
   `verify-ca`) — rustls **vérifie toujours** la chaîne et le nom d'hôte, il
   n'existe pas de mode « accepter tout » ; ajouter `ssl_root_cert` (PEM)
   si votre CA est auto-signée. Socket Unix ⇒ `ssl_mode: disable`.
5. **Rôle dédié** : le daemon se connecte comme `pgindexai` (non-superutilisateur,
   `scram-sha-256`) — ne pas l'exécuter avec le superutilisateur `postgres`
6. **Config** : fichier en **0600** (contient le mot de passe) ; les clés
   inconnues déclenchent un avertissement explicite au chargement
7. **Logs** : configurer `index_logging.log_file` pour l'audit (rotation
   intégrée, mode 0640, échecs bruyants)
8. **Vérification sans effet** : `pgindexai run --dry-run` analyse et affiche
   les propositions sans appeler Laya ni exécuter de DDL
9. **Laya** : le health check profond (`deep_health()`) valide le pipeline
   de décision avant chaque lot — un serveur Laya « mort » mais qui répond
   aux sondes HTTP simples sera détecté

## 8. Architecture de déploiement

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Serveur PostgreSQL                             │
│                                                                         │
│  ┌──────────────┐  ┌──────────────────┐  ┌──────────────────────────┐  │
│  │  pgindexai_demo   │  │  pg_stat_        │  │  information_schema       │  │
│  │  (5 tables)  │→ │  statements      │  │  + pg_stats + pg_am       │  │
│  │  (23 index)  │  │  (requêtes)      │  │  (métadonnées colonnes)   │  │
│  └──────────────┘  └──────────────────┘  └──────────────────────────┘  │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  pgindexai daemon (systemd)                                      │   │
│  │  ┌──────────┐  ┌──────────────────────────┐  ┌───────────────┐  │   │
│  │  │ Analyzer │→ │ DDL Engine (en ligne)       │→ │ IndexLogWriter│  │   │
│  │  │ 3 détect.│  │ CONCURRENTLY / DROP tx      │  │ (JSONL)      │  │   │
│  │  │ + state  │  └──────────────────────────┘  └───────────────┘  │   │
│  │  │ (Healthy/│                                                      │   │
│  │  │ Degraded)│                                                      │   │
│  │  └──────────┘                                                      │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  laya --server (requis pour appliquer des DDL)                  │   │
│  │  /v1/systemone (IA décision) + deep health check par lot        │   │
│  │  Laya injoignable → AUCUNE DDL (no fallback)                    │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```
