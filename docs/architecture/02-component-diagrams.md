# 02 — Diagrammes

## 1. Diagramme de composants

```
┌──────────────────────────────────────────────────────────────────────────┐
│                         pgindexai (binaire Rust)                          │
│                                                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                          main.rs                                   │  │
│  │                                                                    │  │
│  │  Cli (clap) → Config (YAML) → Pool (deadpool, taille+timeout)     │  │
│  │       → check_pg_stat_statements → DaemonState (Healthy|Degraded)  │  │
│  │       → LayaClient (reqwest) → deep_health() (payload de test)     │  │
│  │                                                                    │  │
│  │  ┌──────────────┐   ┌──────────────┐                               │  │
│  │  │  run_daemon()│   │   run_once() │                               │  │
│  │  │  loop {      │   │              │                               │  │
│  │  │    analyze   │   │   analyze    │                               │  │
│  │  │    apply     │   │   apply      │                               │  │
│  │  │    sleep     │   │   exit       │                               │  │
│  │  │  }           │   │              │                               │  │
│  │  └──────────────┘   └──────────────┘                               │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                       analyzer.rs                                  │  │
│  │                                                                    │  │
│  │  Analyzer { pool, laya, config, daemon_state, log_writer }        │  │
│  │                                                                    │  │
│  │  analyze_cycle():                                                  │  │
│  │    ├── analyze_unused()           → Vec<Proposal>  (gate uptime)   │  │
│  │    ├── analyze_fragmentation()    → Vec<Proposal>  (btree only)    │  │
│  │    └── analyze_missing_sargable() → Vec<Proposal> (si Healthy)     │  │
│  │          ├── parse AST (sqlparser)                                  │  │
│  │          ├── types réels + null_frac                                │  │
│  │          ├── naming::generate_index_name()                          │  │
│  │          └── densité par bucket → taille estimée                    │  │
│  │                                                                    │  │
│  │  apply_proposals():                                                │  │
│  │    ├── deep_health()  → FAILED ? → TOUTES SKIPPED (no fallback)    │  │
│  │    ├── Laya decision HTTP POST /v1/systemone                       │  │
│  │    ├── confidence ≥ min_confidence ?                               │  │
│  │    └── match decision → ddl::execute_*()   (en ligne :             │  │
│  │         CREATE/REINDEX CONCURRENTLY, DROP en transaction)          │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐          │
│  │  db/queries  │  │    ddl.rs    │  │   index_log.rs       │          │
│  │              │  │              │  │                      │          │
│  │  observation │  │  run_in_     │  │  IndexOperationLog   │          │
│  │  _window_ok  │  │  transaction │  │  IndexLogWriter      │          │
│  │  get_unused  │  │  is_concur   │  │  OperationTimer      │          │
│  │  get_frag    │  │  execute_    │  │  CrudOperation       │          │
│  │  get_sargable│  │  create_     │  │                      │          │
│  │  get_existing│  │  execute_    │  │  VerbosityLevel      │          │
│  │  index_exists│  │  drop_       │  │                      │          │
│  │  get_cardinal│  │  execute_    │  └──────────────────────┘          │
│  │  parse AST   │  │  reindex     │  ┌──────────────┐  ┌────────────┐  │
│  │  column_meta │  │  extract_    │  │   state.rs   │  │ naming.rs  │  │
│  │  type_bucket │  │  index_name  │  │ DaemonState  │  │ generate_  │  │
│  │  pool TLS    │  │  extract_    │  │ Healthy/     │  │ index_name │  │
│  └──────────────┘  │  schema_from │  │ Degraded     │  │ validate_  │  │
│                    └──────────────┘  └──────────────┘  │ identifier │  │
│                                                        │ is_simple_ │  │
│                                                        └────────────┘  │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                       config.rs                                    │  │
│  │                                                                    │  │
│  │  Config { postgresql, laya, actions, thresholds,                  │  │
│  │           analysis (multi-schéma), pg_pool,                       │  │
│  │           logging, index_logging, density_by_type }               │  │
│  │                                                                    │  │
│  │  AnalysisConfig { schemas: Vec<String> }                          │  │
│  │  PgPoolConfig { max_size, wait_timeout_ms }                       │  │
│  │  DensityByTypeConfig { int4, int8, float8, varchar, text,        │  │
│  │                        date, timestamptz, uuid, bool, default }   │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                     laya/ (HTTP client)                            │  │
│  │                                                                    │  │
│  │  LayaClient { client (reqwest + timeout), base_url }              │  │
│  │    ├── deep_health() → POST /v1/systemone (payload test réel)     │  │
│  │    │     └── valide choice ∈ {0,1} + confidence ∈ [0,1]           │  │
│  │    ├── decision(payload) → POST /v1/systemone                     │  │
│  │    └── build_payload(state, criteria) → LayaPayload               │  │
│  │                                                                    │  │
│  │  LayaError { Http, Deserialize, Unreachable, InvalidResponse }    │  │
│  │  LayaPayload { state, questions: ActionQuestion }                 │  │
│  │  LayaResponse { model, answers: ActionAnswer, usage }             │  │
│  │  DecisionAction { Ignore, CreateIndex, CreateFilteredIndex,       │  │
│  │                   DropIndex, RebuildIndex }                       │  │
│  │  Proposal { action, ddl, reason, confidence, table }              │  │
│  └───────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────────┘
                               │
                     HTTP POST ▼
               ┌────────────────────────────┐
               │      Serveur Laya (IA)      │
               │   http://127.0.0.1:8080     │
               │   /v1/systemone             │
               └────────────────────────────┘
                               │
                     PostgreSQL ▼
               ┌────────────────────────────┐
               │      PostgreSQL 16         │
               │      pgindexai_demo             │
               │  pg_stat_statements        │
               │  information_schema        │
               │  pg_stats                  │
               │  pg_stat_ssl (vérif. TLS)  │
               └────────────────────────────┘
```

## 2. Diagramme de séquence — cycle d'analyse

```
┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
│ Analyzer │  │PostgreSQL│  │  Laya    │  │ DDL      │  │ Log      │
│          │  │          │  │ Server   │  │ Engine   │  │ Writer   │
└────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘
     │              │             │              │              │
     │  0. Gate fenêtre d'observation (index inutilisés)        │
     │─── observation_window_ok() ▶│             │              │
     │◀── true (uptime OK) ───────│             │              │
     │   (false → analyse unused sautée)                │       │
     │              │             │              │              │
     │  1. Unused Indexes        │              │              │
     │─── get_unused_indexes() ▶│              │              │
     │◀── Vec<UnusedIndexInfo> ──│             │              │
     │              │             │              │              │
     │  2. Fragmentation (btree only)                │          │
     │─── get_fragmented_indexes()▶│             │              │
     │◀── Vec<FragmentationInfo>─│              │              │
     │              │             │              │              │
     │  3. SARGable (si DaemonState = Healthy)        │         │
     │─── pg_stat_statements top50▶│             │              │
     │─── parse AST (sqlparser)  [côté Rust]          │         │
     │─── get_existing_indexes() ▶│             │              │
     │─── fetch_column_metadata()▶│             │              │
     │─── get_table_cardinality()▶│             │              │
     │◀── Vec<MissingSargable>───│              │              │
     │              │             │              │              │
     │  4. Vec<Proposal> (3 types)  [naming + densité]          │
     │              │             │              │              │
     │  5. deep_health()   [NOUVEAU : 1x par lot]              │
     │─── POST /v1/systemone (test)▶│          │              │
     │◀── response validée ───────│             │              │
     │   (FAILED → TOUTES les propositions SKIPPED, no fallback)│
     │              │             │              │              │
     │  6. for each proposal :                                  │
     │              │             │              │              │
     │  │  6a. Decision request        │              │         │
     │  │─── POST /v1/systemone ──────▶│              │         │
     │  │  (LayaPayload spécialisé)   │              │         │
     │  │◀── LayaResponse (choice, confidence) ──────│         │
     │  │                              │              │         │
     │  │  6b. confidence ≥ min_confidence ? (non → skip)       │
     │  │  6c. choice appliqué ? (non → skip)                  │
     │  │  6d. action flag enabled ? (non → skip)              │
     │  │                              │              │         │
     │  │  6e. DDL en ligne           │              │         │
     │  │─── execute_*() ─────────────▶│              │         │
     │  │   CONCURRENTLY → auto-commit│              │         │
     │  │   DROP INDEX → BEGIN/COMMIT │              │         │
     │  │─── DDL query ───────────────▶│              │         │
     │  │◀── Ok ──────────────────────│              │         │
     │  │   (validation post-DDL : index_valid,            │
     │  │    nettoyage index INVALIDE si build interrompu) │
     │  │                              │              │         │
     │  │  6f. Log résultat           │              │         │
     │  │─────────────────────────────────────────▶│           │
     │  │                              │              │         │
     │              │             │              │              │
     │  7. Vec<Proposal> appliquées                                   │
     │              │             │              │              │
```

## 3. Diagramme de données

```
┌───────────────────────────────────────────────────────────────────────────┐
│                     PostgreSQL DMV (Data Management Views)                │
├───────────────────────────────────────────────────────────────────────────┤
│                                                                           │
│  pg_stat_user_indexes                                                     │
│  ├── idx_scan, idx_tup_read, idx_tup_fetch                               │
│  ├── schemaname, relname (table), indexrelname (index)                   │
│  ├── pg_get_indexdef() → CREATE INDEX ... ON ... (col1, col2)            │
│  └── pg_relation_size() → taille en octets                               │
│                                                                           │
│  pg_index                                                                 │
│  ├── indisvalid, indisprimary, indisunique (filtres unused)              │
│  └── indrelid → table ciblée                                             │
│                                                                           │
│  pg_am  (NOUVEAU)                                                         │
│  └── amname = 'btree' → périmètre fragmentation                          │
│                                                                           │
│  pg_stat_user_tables                                                      │
│  ├── n_live_tup, n_dead_tup, n_mod_since_analyze                         │
│  └── relid → jointure avec pg_class                                      │
│                                                                           │
│  pg_stat_statements                                                       │
│  ├── queryid, query (requête textuelle → parse AST sqlparser)            │
│  └── calls, mean/max/min_exec_time, rows                                 │
│                                                                           │
│  pg_indexes                                                               │
│  ├── schemaname, tablename, indexname, indexdef                          │
│  └── source pour get_existing_indexes() (SARGable filter)                │
│                                                                           │
│  pg_class                                                                 │
│  ├── relname, relkind ('r'=table, 'i'=index)                             │
│  ├── relpages::bigint, reltuples::float8 (estimation cardinalité)        │
│  └── pg_relation_size() → taille des index                               │
│                                                                           │
│  information_schema.columns  (NOUVEAU)                                    │
│  ├── table_schema, table_name, column_name                               │
│  ├── data_type (ex: "character varying") → bucket de densité             │
│  └── is_nullable (YES/NO) → décision index filtré                        │
│                                                                           │
│  pg_stats  (NOUVEAU)                                                      │
│  ├── attname, null_frac::float8 → seuil 0.10 pour "WHERE IS NOT NULL"    │
│  └── schemaname + tablename → par table                                  │
│                                                                           │
│  pg_postmaster_start_time()  (NOUVEAU)                                    │
│  └── début de la fenêtre d'observation (gate index inutilisés)           │
│                                                                           │
│  pg_extension                                                             │
│  ├── extname, extversion                                                 │
│  └── vérification pg_stat_statements (DaemonState Healthy|Degraded)      │
│                                                                           │
└───────────────────────────────────────────────────────────────────────────┘
```

## 4. Flux de décision

```
   Vec<Proposal> { action, ddl, reason, confidence, table }
         │
         ▼
   ┌────────────────────────────────────────┐
   │ deep_health() OK ?  [NOUVEAU]          │
   │  (payload de test réel, réponse        │
   │   validée structurellement)            │
   └──────┬─────────────────────────────────┘
   no ────┴──▶ TOUTES les propositions SKIPPED (no fallback, warn log)
   yes
         │
         ▼
   ┌──────────────────┐
   │ for each proposal │
   └──────┬───────────┘
         │
         ▼
   ┌──────────────────┐
   │ build_payload()  │──▶ POST /v1/systemone
   │ (critères        │    (spécialisés par type de proposition)
   │  par type)       │
   └──────┬───────────┘
         │
         ▼
   ┌──────────────────┐
   │ LayaResponse     │
   │ choice: "0"/"1"/"2"
   │ confidence: 0.85 │
   └──────┬───────────┘
         │
         ▼
   ┌──────────────────┐
   │ confidence ≥     │
   │ min_confidence?  │
   └──────┬───────────┘
    no ───┴──▶ skip (debug log, aucune heuristique)
    yes
         │
         ▼
   ┌──────────────────┐
   │ choice appliqué  │──▶ sinon skip (Ignore)
   └──────┬───────────┘
         │
         ▼
   ┌──────────────────┐
   │ actions.* flag?  │  (create_index, drop_unused, rebuild)
   └──────┬───────────┘
    no ───┴──▶ skip (warn log)
    yes
         │
         ▼
   ┌──────────────────────────────┐
   │ DDL en ligne [NOUVEAU]       │
   │ index_exists() → skip si OK  │
   │ CONCURRENTLY → auto-commit   │
   │ DROP INDEX → BEGIN/COMMIT    │
   │ validation + cleanup INVALID │
   └──────┬───────────────────────┘
         │
         ▼
   ┌──────────────────┐
   │ IndexLogWriter   │
   │ .log(log)        │──▶ stdout + fichier JSONL
   └──────────────────┘
```

## 5. Structure du projet

```
pgindexai/
├── Cargo.toml                    # Dépendances Rust (sqlparser 0.52, rustls,
│                                 # tokio-postgres-rustls, deadpool-postgres)
├── Cargo.lock                    # Verrouillé (projet binaire)
├── install.sh                    # Installateur durci : détection PG (SHOW),
│                                 # rôle dédié pgindexai, pg_hba ciblée,
│                                 # config 0600, unité systemd, --dry-run
├── uninstall.sh                  # Désinstallation : service, binaire, config,
│                                 # --purge (base + rôle + pg_hba),
│                                 # --clean-legacy, --dry-run
├── config/
│   ├── PGindexAI.yaml              # Template de configuration (multi-schéma,
│   │                             # TLS, pool, densités, seuils)
│   └── pgindexai.service          # Unité systemd (référentielle)
├── sql/
│   ├── demo_schema.sql           # Schéma de démo (5 tables, 23 index)
│   └── demo_workload.sql         # Workload de test (8 phases)
├── .github/
│   └── workflows/
│       └── ci.yml                # CI : fmt, clippy, test, shellcheck
├── docs/
│   └── architecture/
│       ├── README.md             # Table des matières
│       ├── 01-system-overview.md # Vue d'ensemble, architecture
│       ├── 02-component-diagrams.md # Diagrammes
│       ├── 03-module-reference.md # Référence modules
│       ├── 04-protocols.md       # Protocoles
│       ├── 05-configuration.md   # Configuration
│       └── 06-deployment.md      # Déploiement
├── src/
│   ├── main.rs                   # Point d'entrée, CLI, daemon/run, --dry-run
│   ├── analyzer.rs               # Moteur d'analyse (3 détecteurs + Laya)
│   ├── state.rs                  # DaemonState (Healthy/Degraded)
│   ├── naming.rs                 # Convention de nommage d'index (63 octets)
│   ├── config.rs                 # Chargement YAML + validation + avertissements
│   ├── ddl.rs                    # Exécution DDL en ligne (CONCURRENTLY)
│   ├── index_log.rs              # Historisation JSONL + rotation de fichiers
│   ├── db/
│   │   ├── mod.rs                # Pool de connexions + TLS (rustls)
│   │   └── queries.rs            # Requêtes DMV + parseur AST (sqlparser)
│   └── laya/
│       ├── mod.rs                # Client HTTP Laya (deep_health + décision)
│       └── types.rs              # Types du protocole Laya
└── README.md
```
