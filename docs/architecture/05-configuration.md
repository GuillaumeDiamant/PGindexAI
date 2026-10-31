# 05 — Configuration

> **Nouveautés (sécurité)** : `postgresql.password` et
> `postgresql.ssl_root_cert` (CA auto-signée), `thresholds.max_proposals_per_cycle`
> (budget par cycle), suppression des champs morts (`laya.ready_timeout_seconds`,
> `laya.binary_path`, `laya.model_path`, `laya.variant`, `laya.cpu_mode`,
> `thresholds.idle_seconds`). Toute clé inconnue déclenche un **avertissement**
> visible au démarrage (pas une erreur silencieuse).

## 1. Emplacement

| Méthode | Chemin |
|---------|--------|
| Défaut | `~/.config/PGindexAI/PGindexAI.yaml` |
| CLI | `pgindexai --config /path/to/config.yaml daemon` |
| Fallback | `./PGindexAI.yaml` (si `config_dir()` est `None`) |

**Droits recommandés : `0600`** — le fichier peut contenir un mot de passe.

## 2. Configuration complète

```yaml
# ─────────────────────────────────────────────
# PostgreSQL
# ─────────────────────────────────────────────
postgresql:
  host: "/var/run/postgresql"     # Socket Unix, ou nom d'hôte/IP pour du TCP
  port: 5432                       # Port PostgreSQL
  user: "pgindexai"                 # Rôle dédié non superutilisateur (voir 06)
  database: "pgindexai_demo"            # Base cible
  password: ""                     # Requis si pg_hba exige scram-sha-256 / md5
  ssl_mode: "disable"              # disable | require | verify-ca | verify-full
  # ssl_root_cert: ""              # Chemin PEM d'une CA auto-signée (snakeoil,
                                   # interne) pour verify-ca / verify-full.
                                   # Sans cette clé : racines web PKI (Mozilla)
                                   # uniquement.

# ─────────────────────────────────────────────
# Serveur Laya (moteur de décision IA)
# ─────────────────────────────────────────────
laya:
  url: "http://127.0.0.1:8080"    # Endpoint HTTP
  timeout_seconds: 30              # Timeout requête /v1/systemone (1..=600)

# ─────────────────────────────────────────────
# Contrôle des actions DDL
# ─────────────────────────────────────────────
actions:
  create_index: true               # Autoriser CREATE INDEX
  drop_unused: true                # Autoriser DROP INDEX
  rebuild_fragmented: true         # Autoriser REINDEX

# ─────────────────────────────────────────────
# Seuils de décision
# ─────────────────────────────────────────────
thresholds:
  cycle_seconds: 60                # Fréquence du cycle (1..=86400)
  unused_minutes: 10080            # Fenêtre d'observation (défaut 7 jours)
  fragmentation_pct: 70            # % de fragmentation (1..=100)
  min_sargable_rows: 100           # Min. d'exécutions (pg_stat_statements.calls)
  min_confidence: 0.7              # Confiance min. de Laya pour APPLY (0.0..=1.0)
  max_proposals_per_cycle: 20      # Budget de propositions par cycle (1..=1000)

# ─────────────────────────────────────────────
# Périmètre d'analyse (multi-schéma)
# ─────────────────────────────────────────────
analysis:
  schemas: []                      # [] = tous les schémas non-système

# ─────────────────────────────────────────────
# Pool de connexions PostgreSQL
# ─────────────────────────────────────────────
pg_pool:
  max_size: 16                     # 1..=512
  wait_timeout_ms: 30000           # 100..=600000

# ─────────────────────────────────────────────
# Densité cible par type de colonne
# (tuples/page visés — sert à l'estimation de
# taille des futurs index envoyée à Laya)
# ─────────────────────────────────────────────
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

# ─────────────────────────────────────────────
# Logging (tracing)
# ─────────────────────────────────────────────
logging:
  level: "info"                    # trace | debug | info | warn | error
  format: "json"                   # json | text

# ─────────────────────────────────────────────
# Logging des opérations d'index (historisation)
# ─────────────────────────────────────────────
index_logging:
  enabled: true
  verbosity: "minimal"             # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760    # rotation à 10 MB (0 = désactivé)
  max_files: 5
```

## 3. Valeurs par défaut

| Paramètre | Défaut | Contraintes |
|-----------|--------|-------------|
| `postgresql.host` | — (requis) | non vide |
| `postgresql.port` | — (requis) | port valide |
| `postgresql.user` | — (requis) | non vide |
| `postgresql.database` | — (requis) | non vide |
| `postgresql.password` | `null` | requis si auth scram/md5 |
| `postgresql.ssl_mode` | `disable` | `disable` \| `require` \| `verify-ca` \| `verify-full` |
| `postgresql.ssl_root_cert` | `null` | chemin PEM lisible |
| `laya.url` | — (requis) | non vide |
| `laya.timeout_seconds` | `30` | `1..=600` |
| `actions.create_index` | `true` | bool |
| `actions.drop_unused` | `true` | bool |
| `actions.rebuild_fragmented` | `true` | bool |
| `thresholds.cycle_seconds` | `60` | `1..=86400` |
| `thresholds.unused_minutes` | `10080` (7 j) | `>= 1` |
| `thresholds.fragmentation_pct` | `70` | `1..=100` |
| `thresholds.min_sargable_rows` | `100` | `>= 1` |
| `thresholds.min_confidence` | `0.7` | `0.0..=1.0` |
| `thresholds.max_proposals_per_cycle` | `20` | `1..=1000` |
| `analysis.schemas` | `[]` (tous les schémas non-système) | liste |
| `pg_pool.max_size` | `16` | `1..=512` |
| `pg_pool.wait_timeout_ms` | `30000` | `100..=600000` |
| `density_by_type.*` | cf. §2 | `> 0` et finis |
| `logging.level` | `info` | niveau tracing |
| `logging.format` | `json` | `json` \| `text` |
| `index_logging.enabled` | `true` | bool |
| `index_logging.verbosity` | `minimal` | `minimal` \| `standard` \| `verbose` |
| `index_logging.log_file` | `""` (stdout) | chemin |
| `index_logging.max_file_size_bytes` | `10485760` | octets, `0` = off |
| `index_logging.max_files` | `5` | entier |

## 4. Variables d'environnement

| Variable | Usage |
|----------|-------|
| `RUST_LOG` | Surcharge le niveau de logging (ex: `RUST_LOG=debug`) |
| `RUST_BACKTRACE` | Active les backtraces (ex: `RUST_BACKTRACE=1`) |

## 5. Sémantique TLS (importante)

Le client utilise **rustls** (`tokio-postgres-rustls`). Conséquences :

| `ssl_mode` | Comportement effectif |
|------------|----------------------|
| `disable` | Pas de TLS. **Seul mode possible sur socket Unix** (erreur claire sinon). |
| `require` | TLS **avec vérification complète du certificat** (chaîne + nom d'hôte) — au moins aussi strict que libpq `verify-full`. |
| `verify-ca` | Identique à `require` avec rustls (la vérification est toujours totale). |
| `verify-full` | Identique à `require` avec rustls. |

- Les **racines de confiance** : web PKI (Mozilla) **et** le fichier PEM
  `ssl_root_cert` s'il est fourni. Un certificat auto-signé (snakeoil, CA
  interne) **sans** `ssl_root_cert` est **refusé** (`InvalidCertificate`).
- La différence avec libpq : `require` y signifie « chiffrer sans vérifier » ;
  ici tout mode TLS vérifie. C'est plus strict — c'est volontaire.
- Le nom d'hôte du certificat (CN/SAN) doit correspondre à `postgresql.host`
  (ex: `DNS:localhost,IP:127.0.0.1` pour du local).

## 6. Comportements induits par la configuration

| Configuration | Comportement |
|---------------|--------------|
| `analysis.schemas: []` | Tous les schémas non-système sont analysés |
| `analysis.schemas: [a, b]` | Uniquement les schémas listés |
| `pg_pool.wait_timeout_ms` | Si le pool est saturé, l'attente est bornée |
| `density_by_type.*` | Sert uniquement à l'**estimation de taille** des futurs index envoyée au contexte de décision Laya — aucun effet sur la détection |
| `thresholds.unused_minutes` | **Fenêtre minimale d'observation** : si le serveur PG tourne depuis moins de ce délai, l'analyse « index inutilisés » est sautée (les compteurs `pg_stat_*` sont remis à zéro au redémarrage) |
| `thresholds.min_confidence` | Seuil de confiance Laya ; **en dessous, la proposition est ignorée (aucun fallback heuristique)** |
| `thresholds.max_proposals_per_cycle` | Chaque détecteur est borné en SQL (`LIMIT`) **et** la liste totale est tronquée à ce budget avec un avertissement (les propositions écartées seront re-détectées au cycle suivant) |
| clé inconnue dans la config | **Avertissement** au démarrage (ex: `dashboard:`, `idle_seconds:`) — la clé est ignorée mais signalée |

## 7. Validation

Le chargement de config échoue avec :

| Erreur | Cause |
|--------|-------|
| `Read(path, err)` | Fichier inexistant ou inaccessible |
| `Parse(path, err)` | YAML invalide (indentation, syntaxe) ou type inattendu |
| `Validation("...")` | Incohérence de valeurs (liste des erreurs concaténées) — ex: `cycle_seconds = 0`, `min_confidence = 1.5`, `ssl_mode` non supporté |

Les erreurs de validation sont **cumulées** : toutes les incohérences sont
rapportées d'un coup, pas la première rencontrée.

## 8. Exemples de configuration

### Production (Laya actif, TLS vérifié)

```yaml
postgresql:
  host: "pg.example.com"
  port: 5432
  user: "pgindexai"
  database: "production"
  password: "…"                      # ou via pg_hba peer + socket
  ssl_mode: "verify-full"
  ssl_root_cert: "/etc/ssl/private/pgindexai-ca.pem"  # CA interne

laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

actions:
  create_index: true
  drop_unused: false    # DROP désactivé en production
  rebuild_fragmented: true

thresholds:
  min_confidence: 0.9
  min_sargable_rows: 500
  max_proposals_per_cycle: 10   # budget prudent en prod

analysis:
  schemas: ["public", "billing", "crm"]

pg_pool:
  max_size: 32
  wait_timeout_ms: 15000
```

### Safe mode (aucune action automatique)

```yaml
actions:
  create_index: false
  drop_unused: false
  rebuild_fragmented: false
```

### Dev mode (local, log détaillé)

```yaml
postgresql:
  host: "/var/run/postgresql"
  user: "pgindexai"
  database: "pgindexai_demo"
  ssl_mode: "disable"    # socket Unix → TLS inapplicable

laya:
  url: "http://127.0.0.1:8080"

logging:
  level: "debug"
  format: "text"

index_logging:
  verbosity: "verbose"
  log_file: "/tmp/pgindexai-operations.log"
```
