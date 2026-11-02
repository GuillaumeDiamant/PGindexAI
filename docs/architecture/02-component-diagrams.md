# 02 — Diagrams

## 1. Component diagram

```
┌──────────────────────────────────────────────────────────────────────────┐
│                         pgindexai (Rust binary)                           │
│                                                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                          main.rs                                   │  │
│  │                                                                    │  │
│  │  Cli (clap) → Config (YAML) → Pool (deadpool, size+timeout)       │  │
│  │       → check_pg_stat_statements → DaemonState (Healthy|Degraded)  │  │
│  │       → LayaClient (reqwest) → deep_health() (test payload)        │  │
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
│  │          ├── real types + null_frac                                 │  │
│  │          ├── naming::generate_index_name()                          │  │
│  │          └── density per bucket → estimated size                    │  │
│  │                                                                    │  │
│  │  apply_proposals():                                                │  │
│  │    ├── deep_health()  → FAILED ? → ALL SKIPPED (no fallback)       │  │
│  │    ├── Laya decision HTTP POST /v1/systemone                       │  │
│  │    ├── confidence ≥ min_confidence ?                               │  │
│  │    └── match decision → ddl::execute_*()   (online:                │  │
│  │         CREATE/REINDEX CONCURRENTLY, DROP in transaction)          │  │
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
│  │           analysis (multi-schema), pg_pool,                       │  │
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
│  │    ├── deep_health() → POST /v1/systemone (real test payload)     │  │
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
               │      Laya Server (AI)       │
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
               │  pg_stat_ssl (TLS check)   │
               └────────────────────────────┘
```

## 2. Sequence diagram — analysis cycle

```
┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
│ Analyzer │  │PostgreSQL│  │  Laya    │  │ DDL      │  │ Log      │
│          │  │          │  │ Server   │  │ Engine   │  │ Writer   │
└────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘
     │              │             │              │              │
     │  0. Observation window gate (unused indexes)             │
     │─── observation_window_ok() ▶│             │              │
     │◀── true (uptime OK) ───────│             │              │
     │   (false → unused analysis skipped)              │       │
     │              │             │              │              │
     │  1. Unused Indexes        │              │              │
     │─── get_unused_indexes() ▶│              │              │
     │◀── Vec<UnusedIndexInfo> ──│             │              │
     │              │             │              │              │
     │  2. Fragmentation (btree only)                │          │
     │─── get_fragmented_indexes()▶│             │              │
     │◀── Vec<FragmentationInfo>─│              │              │
     │              │             │              │              │
     │  3. SARGable (if DaemonState = Healthy)        │         │
     │─── pg_stat_statements top50▶│             │              │
     │─── parse AST (sqlparser)  [Rust side]          │         │
     │─── get_existing_indexes() ▶│             │              │
     │─── fetch_column_metadata()▶│             │              │
     │─── get_table_cardinality()▶│             │              │
     │◀── Vec<MissingSargable>───│              │              │
     │              │             │              │              │
     │  4. Vec<Proposal> (3 types)  [naming + density]          │
     │              │             │              │              │
     │  5. deep_health()   [NEW: 1x per batch]                 │
     │─── POST /v1/systemone (test)▶│          │              │
     │◀── validated response ─────│             │              │
     │   (FAILED → ALL proposals SKIPPED, no fallback)          │
     │              │             │              │              │
     │  6. for each proposal :                                  │
     │              │             │              │              │
     │  │  6a. Decision request        │              │         │
     │  │─── POST /v1/systemone ──────▶│              │         │
     │  │  (specialized LayaPayload)  │              │         │
     │  │◀── LayaResponse (choice, confidence) ──────│         │
     │  │                              │              │         │
     │  │  6b. confidence ≥ min_confidence ? (no → skip)        │
     │  │  6c. choice applied? (no → skip)                     │
     │  │  6d. action flag enabled ? (no → skip)               │
     │  │                              │              │         │
     │  │  6e. Online DDL             │              │         │
     │  │─── execute_*() ─────────────▶│              │         │
     │  │   CONCURRENTLY → auto-commit│              │         │
     │  │   DROP INDEX → BEGIN/COMMIT │              │         │
     │  │─── DDL query ───────────────▶│              │         │
     │  │◀── Ok ──────────────────────│              │         │
     │  │   (post-DDL validation: index_valid,             │
     │  │    cleanup INVALID indexes if build interrupted) │
     │  │                              │              │         │
     │  │  6f. Log result             │              │         │
     │  │─────────────────────────────────────────▶│           │
     │  │                              │              │         │
     │              │             │              │              │
     │  7. Vec<Proposal> applied                                      │
     │              │             │              │              │
```

## 3. Data diagram

```
┌───────────────────────────────────────────────────────────────────────────┐
│                     PostgreSQL DMV (Data Management Views)                │
├───────────────────────────────────────────────────────────────────────────┤
│                                                                           │
│  pg_stat_user_indexes                                                     │
│  ├── idx_scan, idx_tup_read, idx_tup_fetch                               │
│  ├── schemaname, relname (table), indexrelname (index)                   │
│  ├── pg_get_indexdef() → CREATE INDEX ... ON ... (col1, col2)            │
│  └── pg_relation_size() → size in bytes                                  │
│                                                                           │
│  pg_index                                                                 │
│  ├── indisvalid, indisprimary, indisunique (unused filters)              │
│  └── indrelid → target table                                             │
│                                                                           │
│  pg_am  (NEW)                                                             │
│  └── amname = 'btree' → fragmentation scope                              │
│                                                                           │
│  pg_stat_user_tables                                                      │
│  ├── n_live_tup, n_dead_tup, n_mod_since_analyze                         │
│  └── relid → join with pg_class                                          │
│                                                                           │
│  pg_stat_statements                                                       │
│  ├── queryid, query (text query → parse AST sqlparser)                   │
│  └── calls, mean/max/min_exec_time, rows                                 │
│                                                                           │
│  pg_indexes                                                               │
│  ├── schemaname, tablename, indexname, indexdef                          │
│  └── source for get_existing_indexes() (SARGable filter)                 │
│                                                                           │
│  pg_class                                                                 │
│  ├── relname, relkind ('r'=table, 'i'=index)                             │
│  ├── relpages::bigint, reltuples::float8 (cardinality estimate)          │
│  └── pg_relation_size() → index size                                     │
│                                                                           │
│  information_schema.columns  (NEW)                                        │
│  ├── table_schema, table_name, column_name                               │
│  ├── data_type (e.g.: "character varying") → density bucket              │
│  └── is_nullable (YES/NO) → filtered index decision                      │
│                                                                           │
│  pg_stats  (NEW)                                                          │
│  ├── attname, null_frac::float8 → threshold 0.10 for "WHERE IS NOT NULL" │
│  └── schemaname + tablename → per table                                  │
│                                                                           │
│  pg_postmaster_start_time()  (NEW)                                        │
│  └── start of the observation window (unused-index gate)                 │
│                                                                           │
│  pg_extension                                                             │
│  ├── extname, extversion                                                 │
│  └── check pg_stat_statements (DaemonState Healthy|Degraded)             │
│                                                                           │
└───────────────────────────────────────────────────────────────────────────┘
```

## 4. Decision flow

```
   Vec<Proposal> { action, ddl, reason, confidence, table }
         │
         ▼
   ┌────────────────────────────────────────┐
   │ deep_health() OK ?  [NEW]              │
   │  (real test payload, response          │
   │   structurally validated)              │
   └──────┬─────────────────────────────────┘
   no ────┴──▶ ALL proposals SKIPPED (no fallback, warn log)
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
   │ (criteria        │    (specialized by proposal type)
   │  by type)        │
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
    no ───┴──▶ skip (debug log, no heuristic)
    yes
         │
         ▼
   ┌──────────────────┐
   │ choice applied   │──▶ else skip (Ignore)
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
   │ Online DDL [NEW]             │
   │ index_exists() → skip if OK  │
   │ CONCURRENTLY → auto-commit   │
   │ DROP INDEX → BEGIN/COMMIT    │
   │ validation + cleanup INVALID │
   └──────┬───────────────────────┘
         │
         ▼
   ┌──────────────────┐
   │ IndexLogWriter   │
   │ .log(log)        │──▶ stdout + JSONL file
   └──────────────────┘
```

## 5. Project structure

```
pgindexai/
├── Cargo.toml                    # Rust dependencies (sqlparser 0.52, rustls,
│                                 # tokio-postgres-rustls, deadpool-postgres)
├── Cargo.lock                    # Locked (binary project)
├── install.sh                    # Hardened installer: PG detection (SHOW),
│                                 # dedicated pgindexai role, targeted pg_hba,
│                                 # config 0600, systemd unit, --dry-run
├── uninstall.sh                  # Uninstallation: service, binary, config,
│                                 # --purge (database + role + pg_hba),
│                                 # --clean-legacy, --dry-run
├── config/
│   ├── PGindexAI.yaml              # Configuration template (multi-schema,
│   │                             # TLS, pool, densities, thresholds)
│   └── pgindexai.service          # systemd unit (reference)
├── sql/
│   ├── demo_schema.sql           # Demo schema (5 tables, 23 indexes)
│   └── demo_workload.sql         # Test workload (8 phases)
├── .github/
│   └── workflows/
│       └── ci.yml                # CI: fmt, clippy, test, shellcheck
├── docs/
│   └── architecture/
│       ├── README.md             # Table of contents
│       ├── 01-system-overview.md # Overview, architecture
│       ├── 02-component-diagrams.md # Diagrams
│       ├── 03-module-reference.md # Module reference
│       ├── 04-protocols.md       # Protocols
│       ├── 05-configuration.md   # Configuration
│       └── 06-deployment.md      # Deployment
├── src/
│   ├── main.rs                   # Entry point, CLI, daemon/run, --dry-run
│   ├── analyzer.rs               # Analysis engine (3 detectors + Laya)
│   ├── state.rs                  # DaemonState (Healthy/Degraded)
│   ├── naming.rs                 # Index naming convention (63 bytes)
│   ├── config.rs                 # YAML loading + validation + warnings
│   ├── ddl.rs                    # Online DDL execution (CONCURRENTLY)
│   ├── index_log.rs              # JSONL history + file rotation
│   ├── db/
│   │   ├── mod.rs                # Connection pool + TLS (rustls)
│   │   └── queries.rs            # DMV queries + AST parser (sqlparser)
│   └── laya/
│       ├── mod.rs                # Laya HTTP client (deep_health + decision)
│       └── types.rs              # Laya protocol types
└── README.md
```
