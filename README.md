# PGindexAI

**Daemon autonome de gestion d'index PostgreSQL, piloté par l'IA Laya.**

PGindexAI surveille en continu vos bases PostgreSQL, détecte les index inutilisés, fragmentés et les requêtes SARGable sans index — puis propose des actions correctives (DROP, CREATE, REINDEX) validées **exclusivement** par le moteur de décision Laya.

## 🆕 Nouveautés (refonte architecturale)

1. **Politique « sans fallback »** — Laya injoignable ou réponse invalide ⇒ **aucune DDL appliquée** (le daemon ne décide jamais à la place de Laya)
2. **Health check profond** de Laya (payload de test réel envoyé à `/v1/systemone`, réponse validée structurellement) avant chaque lot
3. **Détection SARGable par vrai parseur SQL** (sqlparser-rs, analyse de l'AST — fin des faux positifs des regex)
4. **DDL en ligne** — `CREATE/REINDEX … CONCURRENTLY` (auto-commit) ; `DROP INDEX` en transaction ; validation post-DDL + nettoyage des index INVALIDE
5. **Densité cible par type de colonne** (`density_by_type`) — estimation de taille des index créés
6. **Convention de nommage explicite** (`idx_<table>_<col>`) + validation de longueur (63 octets, hash court)
7. **Pool de connexions dimensionné** (`pg_pool`) — taille max et timeout d'attente configurables
8. **État dégradé explicite** — `pg_stat_statements` absent ⇒ détection d'index manquants désactivée, daemon signalé `DEGRADED`
9. **Support multi-schéma** — périmètre d'analyse configurable (`analysis.schemas`)
10. **Fenêtre d'observation** — les index « inutilisés » ne sont proposés qu'après un uptime serveur suffisant (compteurs remis à zéro au redémarrage)

**Renforcement sécurité (audit)** :

11. **TLS vérifié** (rustls) sur la connexion PostgreSQL — chaîne + hostname **toujours** vérifiées, `ssl_root_cert` (PEM) optionnelle, rôle dédié non-superutilisateur (`pgindexai`)
12. **Installateur durci** `install.sh` (sans `eval`, pg_hba ciblée, merge `shared_preload_libraries`, config 0600, unité systemd durcie) + mode **`--dry-run`** + **`uninstall.sh`** idempotent (`--purge`, `--clean-legacy`)
13. **Plafond de propositions** par cycle (`max_proposals_per_cycle`), rotation des logs d'audit (0640, échecs bruyants), validation de config avec avertissements clés inconnues, **CI** (fmt / clippy `-D warnings` / test / shellcheck)

---

## 🎯 Objectif

Automatiser le cycle complet d'optimisation d'index PostgreSQL :

1. **Surveiller** les performances via `pg_stat_statements` et les catalogues système
2. **Détecter** les anomalies (index inutilisés, fragmentation, index manquants)
3. **Proposer** des corrections (DROP INDEX, CREATE INDEX, REINDEX)
4. **Valider** chaque action via le moteur de décision Laya (IA)
5. **Exécuter** les DDL validés avec traçabilité complète

## 🏗️ Architecture

```
┌──────────────┐     ┌───────────────┐     ┌───────────────┐
│  PostgreSQL   │────▶│   Analyzer    │────▶│  Laya Server  │
│               │     │               │     │               │
│ pg_stat_      │     │ ┌───────────┐ │     │ typed-        │
│ statements    │     │ │ Unused    │ │     │ decisions     │
│ pg_class      │     │ └───────────┘ │     │ model         │
│ pg_index      │     │ ┌───────────┐ │     │               │
│ pg_stats      │     │ │ Frag-     │ │     │ Decision      │
│ information_  │     │ └───────────┘ │     │ engine        │
│ schema        │     │ ┌───────────┐ │     └───────────────┘
│               │     │ │ SARG-     │ │           │
└──────────────┘     │ │ able (AST)│ │           ▼
                     │ └───────────┘ │     ┌───────────────┐
                     │ DaemonState   │     │ DDL Engine    │
                     │ (Healthy/     │     │ (en ligne :   │
                     │  Degraded)    │     │  CONCURRENTLY│
                     └───────────────┘     │  + DROP tx)   │
                              │            └───────────────┘
                              ▼
                     ┌──────────────────┐
                     │ IndexLog Writer  │
                     │ (JSONL, rotation)│
                     └──────────────────┘
```

### Composants

| Module | Rôle |
|--------|------|
| **Analyzer** (`src/analyzer.rs`) | Moteur d'analyse central — orchestre les 3 détecteurs, porte le `DaemonState`, applique la politique sans fallback |
| **DaemonState** (`src/state.rs`) | État `Healthy` / `Degraded` — dégrade la détection d'index manquants si `pg_stat_statements` est absent |
| **Naming** (`src/naming.rs`) | Convention `idx_<table>_<col>`, validation d'identifiant, limite 63 octets (hash court) |
| **Detector : Unused Indexes** | Identifie les index jamais scannés **depuis le redémarrage** + gate fenêtre d'observation (uptime serveur) |
| **Detector : Fragmentation** | Index **B-tree uniquement** ; score multi-critères (dead tuples 50 %, modifications non analysées 30 %, bonus index inutilisé 20 %) |
| **Detector : Missing SARGable** | Parseur AST (sqlparser) sur `pg_stat_statements` + types réels (`information_schema`) + `null_frac` (`pg_stats`) + densités cibles |
| **Laya Client** (`src/laya/`) | Communication HTTP avec le serveur Laya (`/v1/systemone`) — `deep_health()` (payload de test réel), `decision()` |
| **DDL Engine** (`src/ddl.rs`) | Exécution **en ligne** : `CONCURRENTLY` en auto-commit (CREATE/REINDEX), DROP en BEGIN/COMMIT, validation post-DDL + nettoyage des index INVALIDE |
| **IndexLog Writer** (`src/index_log.rs`) | Historisation structurée des opérations (timestamp, type, durée, statut, DDL, confiance, taille, fragmentation) avec rotation de fichiers |
| **Config** (`src/config.rs`) | Chargement YAML — multi-schéma, pool, densités par type, seuils |
| **db Pool** (`src/db/mod.rs`) | Pool deadpool-postgres — taille max + timeout configurables + **TLS rustls** (chaîne + hostname toujours vérifiées, CA web PKI + PEM optionnel) |

## 📊 Détecteurs

### 1. Index inutilisés

Détecte les index qui n'ont jamais été scannés ni lus **depuis le dernier redémarrage** de PostgreSQL :

```sql
-- Gate préalable (nouveau) : uptime serveur ≥ thresholds.unused_minutes
SELECT now() - pg_postmaster_start_time() >= make_interval(mins => 10080);

-- Critères
idx_scan = 0 AND idx_tup_read = 0
AND indisvalid = true
AND indisprimary = false
AND indisunique = false
```

> **Pourquoi le gate ?** Les compteurs `pg_stat_*` sont remis à zéro au
> redémarrage : sans cette vérification, un serveur fraîchement démarré
> donnerait de faux positifs de DROP.

**Action proposée :** `DROP INDEX IF EXISTS schema.index`

### 2. Index fragmentés

PostgreSQL ne fournit pas `avg_leaf_density` directement. PGindexAI utilise un score heuristique multi-critères sur les index **B-tree uniquement** (`pg_am.amname = 'btree'` — les index GiST/GIN/BRIN n'ont pas la même notion de fragmentation) :

| Critère | Pondération |
|---------|-------------|
| `n_dead_tup / (n_live_tup + n_dead_tup)` | 50 % |
| `n_mod_since_analyze / n_live_tup` (bornée à 1.0) | 30 % |
| Bonus index volumineux non scanné (`idx_scan = 0` ET `relpages > 100`) | 20 % |

**Seuil de déclenchement :** `score ≥ thresholds.fragmentation_pct` (défaut 70 %)

**Action proposée :** `REINDEX INDEX CONCURRENTLY schema.index`

### 3. Requêtes SARGable sans index

Analyse `pg_stat_statements` pour identifier les colonnes utilisables dans un index, avec un **vrai parseur SQL** (sqlparser-rs, analyse de l'AST) :

```
Filtres :
  - calls ≥ thresholds.min_sargable_rows (100)
  - !~ ^(CREATE|ALTER|DROP|DELETE|INSERT|UPDATE|BEGIN|COMMIT|...)
  - !~ ^(SELECT|WITH).*\bpg_
  - périmètre multi-schéma (analysis.schemas, nouveau)

Patterns SARGable détectés (AST) :
  - col = v, col > v, col < v, col >= v, col <= v   (valeur non-colonne)
  - col IN (...) / col BETWEEN a AND b              (non-négés)
  - col IS NULL / col IS NOT NULL
  - col LIKE 'pref…%' / ILIKE                      (motif préfixe)
  - AND / OR / parenthèses imbriquées (parcours récursif)

Rejetés (non-SARGable) :
  - NOT (…), IS DISTINCT FROM, col NOT IN/BETWEEN/LIKE
  - Fonctions sur la colonne (LOWER(col), UPPER(col), CAST(col AS …))
  - Comparaison colonne-contre-colonne
  - LIKE/ILIKE avec joker en tête ('%…')
```

**Enrichissement (nouveau)** : pour chaque colonne candidate, le daemon charge
le **type réel** (`information_schema.columns`), la **nullabilité**, la
**fraction de NULL** (`pg_stats.null_frac`) et la **cardinalité** de la table
(`pg_class.reltuples`) pour estimer la taille du futur index via la
**densité cible du type** (`density_by_type.*`).

**Action proposée :** `CREATE INDEX idx_table_column ON table(column)`
Si `null_frac > 0.10` → index filtré : `WHERE column IS NOT NULL`
Nom d'index validé : ≤ 63 octets (troncation + hash court si nécessaire).

## ⚙️ Configuration

Fichier : `~/.config/PGindexAI/PGindexAI.yaml`

```yaml
postgresql:
  host: "/var/run/postgresql"   # Socket Unix ou IP (le socket ⇒ ssl_mode: disable)
  port: 5432
  user: "pgindexai"              # Rôle dédié non-superutilisateur (voir 06)
  database: "pgindexai_demo"
  password: ""                  # Requis si pg_hba exige scram-sha-256 / md5
  ssl_mode: "disable"           # disable | require | verify-ca | verify-full
  # ssl_root_cert: ""           # PEM d'une CA auto-signée (verify-ca / verify-full)

laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

# Contrôle des actions DDL
actions:
  create_index: true            # Autoriser CREATE INDEX
  drop_unused: true             # Autoriser DROP INDEX
  rebuild_fragmented: true      # Autoriser REINDEX

# Seuils de décision
thresholds:
  cycle_seconds: 60             # Fréquence du cycle d'analyse
  unused_minutes: 10080         # Fenêtre d'observation avant proposition DROP (7 jours)
  fragmentation_pct: 70         # % de fragmentation déclenchant REBUILD
  min_sargable_rows: 100        # Min. d'exécutions (pg_stat_statements.calls)
  min_confidence: 0.7           # Confiance min. de Laya pour appliquer
  max_proposals_per_cycle: 20   # Budget de propositions par cycle

# Périmètre d'analyse (nouveau)
analysis:
  schemas: []                   # [] = tous les schémas non-système

# Pool de connexions (nouveau)
pg_pool:
  max_size: 16                  # Taille max du pool
  wait_timeout_ms: 30000        # Timeout d'attente d'une connexion

# Densité cible par type de colonne (nouveau)
density_by_type:
  int4: 200                     # tuples/page (integer, smallint)
  int8: 200                     # bigint
  float8: 150                   # double precision, real, numeric
  varchar: 30                   # character varying
  text: 20
  date: 150
  timestamptz: 150              # timestamp (± time zone)
  uuid: 100
  bool: 300
  default_density: 50           # tout autre type

# Logging
logging:
  level: "info"
  format: "json"

# Logging des opérations d'index
index_logging:
  enabled: true
  verbosity: "minimal"          # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760 # 10 MB
  max_files: 5
```

> Les sections `analysis`, `pg_pool` et `density_by_type` sont **optionnelles**
> — des valeurs par défaut robustes s'appliquent si elles sont absentes.
> Voir [05-configuration.md](docs/architecture/05-configuration.md) pour le détail.

## 🚀 Installation

### Prérequis

- **PostgreSQL 16+** (idéalement avec l'extension `pg_stat_statements` activée — sans elle, le daemon passe en mode dégradé : détection d'index manquants désactivée)
- **Laya server** (`laya`) en mode serveur sur un port local — **requis pour appliquer des DDL** ; sans lui le daemon reste en mode observation (aucune modification de la base)
- **Rust toolchain** (pour compiler depuis les sources)

### 🔧 Script d'installation automatique

Un script `install.sh` est fourni pour automatiser l'installation complète :

```bash
# Exécution directe
curl -fsSL https://raw.githubusercontent.com/GuillaumeDiamant/PGindexAI/main/install.sh | bash

# Ou en local (après clonage)
./install.sh

# Mode simulation (aucune modification)
./install.sh --dry-run
```

### ♻️ Désinstallation

```bash
# Par défaut : service + binaire + config + données + logs
# (CONSERVE la base pgindexai_demo, le rôle pgindexai et la ligne pg_hba)
sudo ./uninstall.sh

# Simulation complète (aucune modification)
sudo ./uninstall.sh --dry-run

# Suppression complète : + base, rôle et ligne pg_hba (irréversible, confirmation)
sudo ./uninstall.sh --purge

# + nettoyage des vestiges de l'ancien nom (laya-sql)
sudo ./uninstall.sh --clean-legacy
```

`shared_preload_libraries` et l'extension `pg_stat_statements` ne sont **jamais**
modifiées (extension partagée — retrait manuel si réellement souhaité).

Le script réalise automatiquement :

| Étape | Action |
|-------|--------|
| 1. Prérequis | Vérifie PostgreSQL, Rust, accès sudo |
| 2. PostgreSQL | Détection via `SHOW config_file/hba_file/ssl` ; **fusion** de `pg_stat_statements` dans `shared_preload_libraries` (jamais d'écrasement) ; redémarre si besoin |
| 3. Rôle dédié | Crée `pgindexai` (non-superutilisateur, `scram-sha-256`) + mot de passe `openssl rand -hex 16` (jamais affiché) ; **prépend** la ligne ciblée `local all pgindexai scram-sha-256` dans `pg_hba.conf` |
| 4. Compilation | `cargo build --release` |
| 5. Installation | Binaire → `/usr/local/bin/pgindexai` |
| 6. Configuration | Génère `~/.config/PGindexAI/PGindexAI.yaml` en **0600** avec le mot de passe |
| 7. Service systemd | Déploie l'unité **durcie** (MemoryMax, NoNewPrivileges, …) et l'active |
| 8. Démo | Charge le schéma et le workload de démonstration, transfère la propriété vers `pgindexai` |

### 1. PostgreSQL (manuel)

```bash
# Ajouter pg_stat_statements à shared_preload_libraries (FUSION — ne pas
# écraser les bibliothèques déjà listées, cf. install.sh)
sudo sed -i "s/^shared_preload_libraries *= *'.*'/shared_preload_libraries = 'pg_stat_statements'/" \
  /etc/postgresql/16/main/postgresql.conf
# (ou : shared_preload_libraries = 'autre_lib, pg_stat_statements')

# Redémarrer PostgreSQL
sudo systemctl restart postgresql

# Créer l'extension
sudo -u postgres psql -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"

# Rôle dédié (recommandé — le daemon ne doit pas tourner avec le superutilisateur)
sudo -u postgres psql -c "CREATE ROLE pgindexai LOGIN PASSWORD '<mot-de-passe-fort>';"
# pg_hba.conf — ligne CIBLÉE (prépendre) : local  all  pgindexai  scram-sha-256
```

### 2. Laya server

```bash
# Télécharger le binaire Laya (CUDA 12 ou CPU)
mkdir -p ~/.local/share/pgindexai/models
# ... placer le modèle typed-decisions ...

# Lancer en mode serveur
laya --model ~/.local/share/pgindexai/models/typed-decisions/ \
     --cpu --server --port 8080
```

### 3. PGindexAI

```bash
# Depuis les sources
git clone https://github.com/GuillaumeDiamant/PGindexAI.git
cd pgindexai
cargo build --release

# Installer le binaire
sudo cp target/release/pgindexai /usr/local/bin/pgindexai

# Configuration
mkdir -p ~/.config/PGindexAI
cp config/PGindexAI.yaml ~/.config/PGindexAI/PGindexAI.yaml
# Éditer ~/.config/PGindexAI/PGindexAI.yaml
```

### 4. Service systemd (production)

```bash
# Template : config/pgindexai.service (référentielle — remplacer les marqueurs
# $USER / $HOME par vos valeurs avant copie)
sudo cp config/pgindexai.service /etc/systemd/system/pgindexai.service
sudo systemctl daemon-reload
sudo systemctl enable --now pgindexai
```

L'unité contient les durcissements : `MemoryMax=512M`, `CPUQuota=100%`,
`NoNewPrivileges=true`, `ProtectSystem=strict`, `PrivateTmp=true`,
`Restart=always`.

## 💻 Utilisation

```bash
# Mode daemon (boucle continue)
pgindexai daemon

# Cycle unique (one-shot)
pgindexai run

# Configuration personnalisée
pgindexai --config /path/to/PGindexAI.yaml daemon

# Sécuritaire : analyse + affichage des propositions SANS appel Laya ni DDL
pgindexai run --dry-run
```

### Sous-commandes

| Commande | Description |
|----------|-------------|
| `daemon` | Boucle d'analyse continue (par défaut, cycle de 60 s) |
| `run` | Exécute un cycle unique et quitte |
| `run --dry-run` | Analyse et affichage **seulement** (aucun effet de bord) |

### Variables d'environnement

| Variable | Rôle |
|----------|------|
| `RUST_LOG` | Niveau de log (`debug`, `info`, `warn`, `error`) — prioritaire sur `logging.level` |


## 📝 Logging

### Journaux du daemon

Par défaut via `journalctl` :

```bash
journalctl --unit=pgindexai -f
```

Format JSON par défaut, configurable dans `logging.format` (`json` ou `plain`).

### Historique des opérations d'index

Fichier structuré par défaut à `/var/log/pgindexai/index_operations.log` :

```json
{
  "timestamp": "2026-10-19T12:07:51.229853Z",
  "operation": "DROP INDEX",
  "schema": "public",
  "object": "idx_products_sku",
  "size_bytes": 176160,
  "duration_ms": 12,
  "status": "success",
  "confidence": 0.0006,
  "reason": "Index never scanned"
}
```

Niveaux de verbosité :

| Niveau | Contenu |
|--------|---------|
| `minimal` | Type d'opération, schéma, objet, durée, statut |
| `standard` | + DDL exécutée |
| `verbose` | + confiance, taille de l'index, fragmentation, raison de la proposition |

## 🧪 Démo

Un schéma de démonstration avec workload est fourni :

```bash
# Créer la base de démo
sudo -u postgres psql -c "CREATE DATABASE pgindexai_demo;"

# Charger le schéma
sudo -u postgres psql -d pgindexai_demo -f sql/demo_schema.sql

# Exécuter le workload
sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql

# Vérifier les propositions du daemon
journalctl --unit=pgindexai -n 50
```

Le workload crée :
- **5 tables** : `customers` (10k), `products` (5k), `orders` (200k), `order_items` (500k), `users` (2k)
- **23 index** dont certains inutilisés volontairement
- **8 phases** de workload incluant des patterns SARGable répétés et de la fragmentation réaliste

## 🔍 Cycle d'analyse

Chaque cycle (par défaut 60 s) :

```
1. [Unused Indexes]  → gate uptime serveur (≥ unused_minutes)
                      → pg_stat_user_indexes → indexes jamais scannés

2. [Fragmentation]   → pg_class + pg_am (btree) → score heuristique multi-critères

3. [SARGable]        → pg_stat_statements → parse AST (sqlparser)
                      → types réels + null_frac + densité → index manquant
                      (désactivé si pg_stat_statements absent : mode DEGRADED)
                          │
                          ▼
4. [Laya Health]     → deep_health() : payload de test réel sur /v1/systemone
                      → FAILED ⇒ TOUTES les propositions SKIPPED (no fallback)
                          │
                          ▼
5. [Laya Decision]   → HTTP POST /v1/systemone (par proposition)
                      → choix appliqué (selon le type) + confidence ≥ min_confidence
                          │
                          ▼
6. [DDL Execution]   → EN LIGNE : CREATE/REINDEX CONCURRENTLY (auto-commit),
                      → DROP INDEX en BEGIN/COMMIT
                      → validation post-DDL (index_valid) + nettoyage
                        des index INVALIDE → logging structuré
```

> **Garantie « sans fallback »** : tant que Laya n'est pas actif et valide,
> le daemon reste en mode *observation* — il détecte, génère et journalise
> les propositions, mais n'applique **aucune** DDL.

## 📁 Structure du projet

```
pgindexai/
├── Cargo.toml              # Dépendances Rust (sqlparser 0.52, rustls, …)
├── Cargo.lock              # Verrouillé (projet binaire)
├── install.sh              # Installateur durci (rôle dédié, pg_hba ciblée,
│                           # config 0600, unité systemd, --dry-run)
├── uninstall.sh            # Désinstallation (service, binaire, config,
│                           # --purge, --clean-legacy, --dry-run)
├── config/
│   ├── PGindexAI.yaml        # Template de configuration
│   └── pgindexai.service    # Unité systemd (référentielle)
├── sql/
│   ├── demo_schema.sql     # Schéma de démo (5 tables, 23 index)
│   └── demo_workload.sql   # Workload de test (8 phases)
├── .github/
│   └── workflows/
│       └── ci.yml          # CI : fmt, clippy, test, shellcheck
├── docs/
│   └── architecture/       # Documentation technique (7 fichiers)
├── src/
│   ├── main.rs             # Point d'entrée, CLI, boucle daemon, --dry-run
│   ├── analyzer.rs         # Moteur d'analyse (3 détecteurs + Laya + no-fallback)
│   ├── state.rs            # DaemonState (Healthy/Degraded)
│   ├── naming.rs           # Convention de nommage d'index (63 octets)
│   ├── config.rs           # Chargement YAML + validation + avertissements
│   ├── ddl.rs              # Exécution DDL en ligne (CONCURRENTLY)
│   ├── index_log.rs        # Historisation JSONL + rotation (0640)
│   ├── db/
│   │   ├── mod.rs          # Pool de connexions + TLS (rustls)
│   │   └── queries.rs      # Requêtes DMV + parseur AST (sqlparser)
│   └── laya/
│       ├── mod.rs          # Client HTTP Laya (deep_health + décision)
│       └── types.rs        # Types du protocole Laya
├── jit_indexer.py          # POC SQL Server (historique, hors daemon)
├── requirements.txt        # Dépendances du POC Python
├── LICENSE                 # Licence MIT
└── README.md
```

## 🛠️ Dépendances

| Crate | Rôle |
|-------|------|
| `tokio` | Runtime asynchrone |
| `tokio-postgres` | Driver PostgreSQL |
| `deadpool-postgres` | Pool de connexions (dimensionné) |
| `reqwest` | Client HTTP pour Laya |
| `serde` / `serde_yaml` / `serde_json` | Sérialisation config + logs |
| `clap` | Parsing CLI |
| `tracing` / `tracing-subscriber` | Logging structuré |
| `chrono` | Gestion des timestamps |
| `thiserror` | Gestion d'erreurs |
| `dirs` | Chemin config par défaut |
| `tokio-postgres-rustls` | Connecteur TLS pour PostgreSQL |
| `rustls` | Implémentation TLS (provider ring) — chaîne + hostname toujours vérifiées |
| `rustls-pemfile` | Lecture des CA `ssl_root_cert` (PEM) |
| `webpki-roots` | Racines web PKI (Mozilla) par défaut |
| `sqlparser` | **Vrai parseur SQL (AST) — détection SARGable** |

## 🔑 Points clés

- **Sans fallback** — le daemon ne décide jamais à la place de Laya : Laya absent ⇒ aucune DDL
- **Health check profond** de Laya (payload de test réel, réponse validée) avant chaque lot
- **DDL en ligne** (`CONCURRENTLY`) avec validation post-DDL + nettoyage des index INVALIDE
- **TLS vérifié** (rustls) sur PostgreSQL — chaîne + hostname toujours vérifiées, CA `ssl_root_cert` optionnelle, rôle dédié non-superutilisateur
- **Installateur durci** (`install.sh` : sans `eval`, pg_hba ciblée, config 0600) + **`--dry-run`** sans effet de bord + **`uninstall.sh`** idempotent (`--purge` pour base/rôle/pg_hba, `--clean-legacy`)
- **Détection SARGable par vrai parseur SQL** (AST, sqlparser-rs) — fin des faux positifs regex
- **Densité cible par type de colonne** — estimation de taille des index créés
- **Convention de nommage** `idx_<table>_<col>` validée (63 octets, hash court)
- **Multi-schéma** — périmètre d'analyse configurable
- **Mode dégradé explicite** quand `pg_stat_statements` est absent
- **Zéro dépendance externe** autre que PostgreSQL et Laya server
- **Configuration YAML** avec valeurs par défaut robustes
- **Logging structuré** (JSON ou texte) avec rotation de fichiers
- **Historisation complète** de toutes les opérations d'index
- **Service systemd** prêt pour la production
- **MIT License**

## 📄 Licence

MIT — Voir le fichier [LICENSE](LICENSE)
