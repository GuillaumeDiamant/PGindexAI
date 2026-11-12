# PGindexAI

**Standalone PostgreSQL index management daemon, driven by the Laya AI.**

PGindexAI continuously monitors your PostgreSQL databases, detects unused and fragmented indexes and unindexed SARGable queries — then proposes corrective actions (DROP, CREATE, REINDEX) validated **exclusively** by the Laya decision engine.

## 🆕 New features (architectural rework)

1. **"No fallback" policy** — Laya unreachable or invalid response ⇒ **no DDL applied** (the daemon never decides on behalf of Laya)
2. **Deep health check** of Laya (real test payload sent to `/v1/systemone`, structurally validated response) before each batch
3. **SARGable detection via a real SQL parser** (sqlparser-rs, AST analysis — no more regex false positives)
4. **Online DDL** — `CREATE/REINDEX … CONCURRENTLY` (auto-commit) ; `DROP INDEX` in a transaction; post-DDL validation + cleanup of INVALID indexes
5. **Per-column-type target density** (`density_by_type`) — size estimation for created indexes
6. **Explicit naming convention** (`idx_<table>_<col>`) + length validation (63 bytes, short hash)
7. **Sized connection pool** (`pg_pool`) — configurable max size and wait timeout
8. **Explicit degraded state** — `pg_stat_statements` absent ⇒ missing-index detection disabled, daemon reported `DEGRADED`
9. **Multi-schema support** — configurable analysis scope (`analysis.schemas`)
10. **Observation window** — "unused" indexes are only proposed after sufficient server uptime (counters reset on restart)

**Security hardening (audit)**:

11. **Validated TLS** (rustls) on the PostgreSQL connection — chain + hostname **always** validated, optional `ssl_root_cert` (PEM), dedicated non-superuser role (`pgindexai`)
12. **Hardened installer** `install.sh` (no `eval`, targeted pg_hba, `shared_preload_libraries` merge, 0600 config, hardened systemd unit) + **`--dry-run`** mode + idempotent **`uninstall.sh`** (`--purge`, `--clean-legacy`)
13. **Proposal cap** per cycle (`max_proposals_per_cycle`), audit log rotation (0640, loud failures), config validation with unknown-key warnings, **CI** (fmt / clippy `-D warnings` / test / shellcheck)

---

## 🎯 Objective

Automate the full PostgreSQL index optimization cycle:

1. **Monitor** performance via `pg_stat_statements` and the system catalogs
2. **Detect** anomalies (unused indexes, fragmentation, missing indexes)
3. **Propose** fixes (DROP INDEX, CREATE INDEX, REINDEX)
4. **Validate** each action via the Laya decision engine (AI)
5. **Execute** the validated DDL with full traceability

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
                     │ (Healthy/     │     │ (online:      │
                     │  Degraded)    │     │  CONCURRENTLY│
                     └───────────────┘     │  + DROP tx)   │
                              │            └───────────────┘
                              ▼
                     ┌──────────────────┐
                     │ IndexLog Writer  │
                     │ (JSONL, rotation)│
                     └──────────────────┘
```

### Components

| Module | Role |
|--------|------|
| **Analyzer** (`src/analyzer.rs`) | Central analysis engine — orchestrates the 3 detectors, holds the `DaemonState`, enforces the no-fallback policy |
| **DaemonState** (`src/state.rs`) | `Healthy` / `Degraded` state — degrades missing-index detection if `pg_stat_statements` is absent |
| **Naming** (`src/naming.rs`) | `idx_<table>_<col>` convention, identifier validation, 63-byte limit (short hash) |
| **Detector : Unused Indexes** | Identifies indexes never scanned **since the last restart** + observation-window gate (server uptime) |
| **Detector : Fragmentation** | **B-tree indexes only**; `score = max(legacy catalog statistics, pgstattuple physical leaf fill)` — the physical criterion is the only one that survives VACUUM/ANALYZE |
| **Detector : Missing SARGable** | AST parser (sqlparser) over `pg_stat_statements` + real types (`information_schema`) + `null_frac` (`pg_stats`) + target densities |
| **Laya Client** (`src/laya/`) | HTTP communication with the Laya server (`/v1/systemone`) — `deep_health()` (real test payload), `decision()` |
| **DDL Engine** (`src/ddl.rs`) | **Online** execution: `CONCURRENTLY` in auto-commit (CREATE/REINDEX), DROP in BEGIN/COMMIT, post-DDL validation + cleanup of INVALID indexes |
| **IndexLog Writer** (`src/index_log.rs`) | Structured audit logging of operations (timestamp, type, duration, status, DDL, confidence, size, fragmentation) with file rotation |
| **Config** (`src/config.rs`) | YAML loading — multi-schema, pool, per-type densities, thresholds |
| **db Pool** (`src/db/mod.rs`) | deadpool-postgres pool — configurable max size + wait timeout + **rustls TLS** (chain + hostname always validated, web PKI CA + optional PEM) |

## 📊 Detectors

### 1. Unused indexes

Detects indexes that have never been scanned or read **since the last PostgreSQL restart**:

```sql
-- Prerequisite gate (new): server uptime >= thresholds.unused_minutes
SELECT now() - pg_postmaster_start_time() >= make_interval(mins => 10080);

-- Criteria
idx_scan = 0 AND idx_tup_read = 0
AND indisvalid = true
AND indisprimary = false
AND indisunique = false
```

> **Why the gate?** The `pg_stat_*` counters are reset on
> restart: without this check, a freshly started server
> would produce DROP false positives.

**Proposed action:** `DROP INDEX IF EXISTS schema.index`

### 2. Fragmented indexes

PGindexAI scores **B-tree indexes only** (`pg_am.amname = 'btree'` — GiST/GIN/BRIN indexes do not have the same notion of fragmentation). Two criteria are combined and **the final score is the maximum of the two** (the strongest signal wins; the legacy criterion alone is the fallback):

**Criterion A — catalog statistics** (works with no extra extension):

| Criterion | Weight |
|---------|-------------|
| `n_dead_tup / (n_live_tup + n_dead_tup)` | 50 % |
| `n_mod_since_analyze / n_live_tup` (capped at 1.0) | 30 % |
| Bonus: large unscanned index (`idx_scan = 0` AND `relpages > 100`) | 20 % |

> **Known limitation:** these counters are **reset by VACUUM / ANALYZE** — and
> autovacuum does both in production. A physically bloated index (leaf pages
> half full) scores **0** with this criterion right after autovacuum ran.
> Criterion B exists to close that blind spot.

**Criterion B — physical bloat** (optional, requires the `pgstattuple` extension):

The measured B-tree leaf page fill (`pgstatindex().avg_leaf_density`, one full
index scan — only performed on indexes above **100 pages**, as a cost guard),
mapped to a 0–1 score with empirically calibrated anchors:

- healthy index ≈ **90 %** leaf fill → score ≈ 0
- worst case ≈ **45 %** (50 % of the rows churned, then VACUUM — index ≈ 2× the healthy size) → score = 1
- in between: linear

The free space left by VACUUM stays on the leaf pages until a REINDEX — this
criterion is **not reset by VACUUM/ANALYZE/autovacuum**. When `pgstattuple` is
missing, `use_index_bloat: false`, or the probe fails, the detector degrades
gracefully to criterion A alone (previous behavior; the daemon warns at startup
but does not degrade).

**Trigger threshold:** `score ≥ thresholds.fragmentation_pct` (default 70 %)

**Proposed action:** `REINDEX INDEX CONCURRENTLY schema.index`

### 3. Unindexed SARGable queries

Analyzes `pg_stat_statements` to identify columns usable in an index, using a **real SQL parser** (sqlparser-rs, AST analysis):

```
Filters:
  - calls ≥ thresholds.min_sargable_rows (100)
  - !~ ^(CREATE|ALTER|DROP|DELETE|INSERT|UPDATE|BEGIN|COMMIT|...)
  - !~ ^(SELECT|WITH)[\s\S]*\y(pg_|information_schema\.)
     (PostgreSQL ARE: `\y` is the word boundary, `\b` a literal backspace)
  - multi-schema scope (analysis.schemas, new)

Detected SARGable patterns (AST) — the operator AND the column's real
type resolve the index method (`resolve_index_method`):

  B-tree (scalar equality / range — any scalar type)
    - col = v, col > v, col < v, col >= v, col <= v   (value is not a column)
    - col IN (...) / col BETWEEN a AND b              (non-negated)
    - col IS NULL / col IS NOT NULL                   (selectivity-gated, see below)
    - col LIKE 'pref…%' / ILIKE                      (prefix pattern)
    - AND / OR / nested parentheses (recursive traversal)

  GIN (containment / key-existence / full-text / overlap)
    - jsonb   : col @> v,  col ? v,  col ?& v,  col ?| v,  col @? v
    - hstore  : col @> v,  col ? v
    - array   : col @> v,  col && v
    - tsvector: col @@ tsquery

  GIST (geometry / ranges)
    - geometry / geography : col @@ v  (spatial predicate),  col && v  (bbox)
    - range types (int4range, tsrange, …) : col @> v  (range containment)

Rejected (non-SARGable):
  - NOT (…), IS DISTINCT FROM, col NOT IN/BETWEEN/LIKE
  - Functions on the column (LOWER(col), UPPER(col), CAST(col AS …))
  - Column-to-column comparison
  - LIKE/ILIKE with a leading wildcard ('%…')
  - col <@ v  (reverse containment) — the indexable side is the RIGHT
    operand, never the left column, so it is intentionally not detected
```

**Enrichment**: for each candidate column, the daemon loads the
**real type** (`format_type(atttypid, atttypmod)` via `pg_attribute` —
`information_schema.data_type` is too lossy for arrays / user types), the
**nullability**, the **NULL fraction** (`pg_stats.null_frac`) and the table
**cardinality** (`pg_class.reltuples`) to resolve the index method and
estimate the size of the future index via the **target density of the type**
(`density_by_type.*`). A GIN/GIST index is typically larger than a B-tree, so
the estimate is multiplied by a configurable size factor
(`density_by_type.gin_size_factor`, `.gist_size_factor`).

**Proposed action:** `CREATE INDEX [CONCURRENTLY] idx_table_column ON
table [USING gin|gist] (column)` — `USING gin` / `USING gist` is emitted
only for GIN/GIST columns (B-tree DDL is unchanged: no `USING`).
If `null_frac > 0.10` → filtered index: `WHERE column IS NOT NULL`
(non-NULL predicates only).
Validated index name: ≤ 63 bytes (truncation + short hash if needed).
Toggle: `thresholds.use_gin_gist_detection` (default `true`) — set to `false`
to restrict detector #3 to B-tree proposals (pre-GIN/GIST behavior).
The chosen method is explicit in every CREATE audit/daemon log entry
(`index_method`: `btree` / `gin` / `gist`).

**NULL-predicate selectivity gate:** for `col IS NULL` / `col IS NOT NULL`,
PostgreSQL's planner picks index-scan vs sequential-scan almost purely on
**selectivity** (verified on PG 16: the B-tree is used only while the
predicate selects less than half the table). An index proposed for a NULL
predicate expected to select ≥ 50 % of the table would be created, never
scanned, dropped as unused, and the same query re-proposed — a
"ping-pong". PGindexAI therefore blocks such proposals
(`null_predicate_plan`), and for the rest steers the variant by polarity:
`IS NULL` → standard B-tree (a `WHERE col IS NOT NULL` partial index
structurally cannot serve it); `IS NOT NULL` → filtered
`WHERE col IS NOT NULL` (the ideal partial index).

> **Known limitations (honest):**
>
> - **Detection is operator + type driven, not function driven.** Only the
>   native operators above are recognized. A workload that calls a *function*
>   (e.g. PostGIS `ST_DWithin(geom, …)`, `ST_Contains(…)`) is **not**
>   detected — only the `@@` / `&&` / `@>` operators are. Express the
>   predicate in operator form, or index such columns manually.
> - **Unknown column type + GIN/GIST operator → best-effort GIN.** If the
>   column type cannot be resolved (a domain / composite type) but the
>   operator is a GIN/GIST one, PGindexAI defaults to **GIN**. Review the
>   proposal before applying it.
> - **GIN/GIST bloat is not covered by detector #2.** Detector #2 scores
>   physical bloat with `pgstatindex()`, a **B-tree-only** function, so a
>   bloated GIN/GIST index is not flagged for REINDEX there (it can still be
>   dropped by detector #1 when unused).

## ⚙️ Configuration

File: `~/.config/PGindexAI/PGindexAI.yaml`

```yaml
postgresql:
  host: "/var/run/postgresql"   # Unix socket, or IP (a socket implies ssl_mode: disable)
  port: 5432
  user: "pgindexai"              # Dedicated non-superuser role (see 06)
  database: "pgindexai_demo"
  password: ""                  # Required if pg_hba enforces scram-sha-256 / md5
  ssl_mode: "disable"           # disable | require | verify-ca | verify-full
  # ssl_root_cert: ""           # PEM of a self-signed CA (verify-ca / verify-full)

laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

# DDL action control
actions:
  create_index: true            # Allow CREATE INDEX
  drop_unused: true             # Allow DROP INDEX
  rebuild_fragmented: true      # Allow REINDEX

# Decision thresholds
thresholds:
  cycle_seconds: 60             # Analysis cycle frequency
  unused_minutes: 10080         # Observation window before a DROP proposal (7 days)
  fragmentation_pct: 70         # Fragmentation % that triggers a REBUILD
  use_index_bloat: true         # Physical bloat detection via pgstattuple (leaf fill);
                                # false = legacy catalog-statistics heuristic only
  min_sargable_rows: 100        # Minimum executions (pg_stat_statements.calls)
  min_confidence: 0.7           # Minimum Laya confidence to apply
  max_proposals_per_cycle: 20   # Proposal budget per cycle

# Analysis scope (new)
analysis:
  schemas: []                   # [] = all non-system schemas

# Connection pool (new)
pg_pool:
  max_size: 16                  # Max pool size
  wait_timeout_ms: 30000        # Wait timeout for a connection

# Target density per column type (new)
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
  default_density: 50           # any other type

# Logging
logging:
  level: "info"
  format: "json"

# Index operation logging
index_logging:
  enabled: true
  verbosity: "minimal"          # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760 # 10 MB
  max_files: 5
```

> The `analysis`, `pg_pool` and `density_by_type` sections are **optional**
> — robust defaults apply when they are absent.
> See [05-configuration.md](docs/architecture/05-configuration.md) for details.

## 🚀 Installation

### Prerequisites

- **PostgreSQL 16+** (ideally with the `pg_stat_statements` extension enabled — without it, the daemon enters degraded mode: missing-index detection disabled)
- **Laya server** (`laya`) in server mode on a local port — **required to apply DDL**; without it the daemon stays in observation mode (no database modification)
- **Rust toolchain** (to build from source)

### 🔧 Automatic installation script

An `install.sh` script is provided to automate the full installation:

```bash
# Direct execution
curl -fsSL https://raw.githubusercontent.com/GuillaumeDiamant/PGindexAI/main/install.sh | bash

# Or locally (after cloning)
./install.sh

# Simulation mode (no modification)
./install.sh --dry-run
```

### ♻️ Uninstallation

```bash
# By default: service + binary + config + data + logs
# (KEEPS the pgindexai_demo database, the pgindexai role and the pg_hba line)
sudo ./uninstall.sh

# Full simulation (no modification)
sudo ./uninstall.sh --dry-run

# Full removal: + database, role and pg_hba line (irreversible, confirmation)
sudo ./uninstall.sh --purge

# + cleanup of leftovers from the old name (laya-sql)
sudo ./uninstall.sh --clean-legacy
```

`shared_preload_libraries` and the `pg_stat_statements` extension are **never**
modified (shared extension — remove manually if truly desired).

The script does the following automatically:

| Step | Action |
|-------|--------|
| 1. Prerequisites | Checks PostgreSQL, Rust, sudo access |
| 2. PostgreSQL | Detection via `SHOW config_file/hba_file/ssl`; **merges** `pg_stat_statements` into `shared_preload_libraries` (never overwrites); restarts if needed |
| 3. Dedicated role | Creates `pgindexai` (non-superuser, `scram-sha-256`) + password `openssl rand -hex 16` (never displayed); **prepends** the targeted line `local all pgindexai scram-sha-256` in `pg_hba.conf` |
| 4. Compilation | `cargo build --release` |
| 5. Installation | Binary → `/usr/local/bin/pgindexai` |
| 6. Configuration | Generates `~/.config/PGindexAI/PGindexAI.yaml` as **0600** with the password |
| 7. systemd service | Deploys the **hardened** unit (MemoryMax, NoNewPrivileges, …) and enables it |
| 8. Demo | Loads the demo schema and workload, transfers ownership to `pgindexai` |

### 1. PostgreSQL (manual)

```bash
# Add pg_stat_statements to shared_preload_libraries (MERGE — do not
# overwrite libraries already listed, see install.sh)
sudo sed -i "s/^shared_preload_libraries *= *'.*'/shared_preload_libraries = 'pg_stat_statements'/" \
  /etc/postgresql/16/main/postgresql.conf
# (or: shared_preload_libraries = 'other_lib, pg_stat_statements')

# Restart PostgreSQL
sudo systemctl restart postgresql

# Create the extension
sudo -u postgres psql -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"

# Dedicated role (recommended — the daemon should not run as the superuser)
sudo -u postgres psql -c "CREATE ROLE pgindexai LOGIN PASSWORD '<strong-password>';"
# pg_hba.conf — TARGETED line (prepend): local  all  pgindexai  scram-sha-256
```

### 2. Laya server

```bash
# Download the Laya binary (CUDA 12 or CPU)
mkdir -p ~/.local/share/pgindexai/models
# ... place the typed-decisions model ...

# Start in server mode
laya --model ~/.local/share/pgindexai/models/typed-decisions/ \
     --cpu --server --port 8080
```

### 3. PGindexAI

```bash
# From source
git clone https://github.com/GuillaumeDiamant/PGindexAI.git
cd pgindexai
cargo build --release

# Install the binary
sudo cp target/release/pgindexai /usr/local/bin/pgindexai

# Configuration
mkdir -p ~/.config/PGindexAI
cp config/PGindexAI.yaml ~/.config/PGindexAI/PGindexAI.yaml
# Edit ~/.config/PGindexAI/PGindexAI.yaml
```

### 4. systemd service (production)

```bash
# Template: config/pgindexai.service (reference — replace the
# $USER / $HOME markers with your values before copying)
sudo cp config/pgindexai.service /etc/systemd/system/pgindexai.service
sudo systemctl daemon-reload
sudo systemctl enable --now pgindexai
```

The unit includes the following hardening: `MemoryMax=512M`, `CPUQuota=100%`,
`NoNewPrivileges=true`, `ProtectSystem=strict`, `PrivateTmp=true`,
`Restart=always`.

## 💻 Usage

```bash
# Daemon mode (continuous loop)
pgindexai daemon

# Single cycle (one-shot)
pgindexai run

# Custom configuration
pgindexai --config /path/to/PGindexAI.yaml daemon

# Safe: analysis + display of proposals WITHOUT any Laya call or DDL
pgindexai run --dry-run
```

### Subcommands

| Command | Description |
|----------|-------------|
| `daemon` | Continuous analysis loop (default, 60s cycle) |
| `run` | Runs a single cycle and exits |
| `run --dry-run` | Analysis and display **only** (no side effects) |

### Environment variables

| Variable | Role |
|----------|------|
| `RUST_LOG` | Log level (`debug`, `info`, `warn`, `error`) — takes precedence over `logging.level` |


## 📝 Logging

### Daemon logs

By default via `journalctl`:

```bash
journalctl --unit=pgindexai -f
```

JSON format by default, configurable in `logging.format` (`json` or `plain`).

### Index operation history

Structured file, by default at `/var/log/pgindexai/index_operations.log`:

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

Verbosity levels:

| Level | Content |
|--------|---------|
| `minimal` | Operation type, schema, object, duration, status |
| `standard` | + DDL executed |
| `verbose` | + confidence, index size, fragmentation, proposal reason |

## 🧪 Demo

A self-contained demo schema + workload is provided in [`sql/`](sql/README.md) —
**see `sql/README.md` for the full testing guide** (dry-run, real run, DDL
verification, triggering the fragmentation detector, reset).

Quick start (analysis only, no DDL):

```bash
# Load the demo schema (creates pgindexai_demo, data, pg_stat_statements)
sudo -u postgres psql -v ON_ERROR_STOP=1 -f sql/demo_schema.sql

# Run the workload (8 phases — a few minutes)
sudo -u postgres psql -d pgindexai_demo -v ON_ERROR_STOP=1 -f sql/demo_workload.sql

# Analyze and PRINT proposals only (no Laya call, no DDL)
pgindexai run --dry-run -c sql/pgindexai.test.yaml
```

The demo creates:
- **5 tables**: `customers` (10k), `products` (5k), `orders` (200k), `order_items` (500k), `users` (2k)
- **23 indexes**, some intentionally unused
- **8 phases** of workload, including repeated SARGable patterns and realistic fragmentation

## 🔍 Analysis cycle

Each cycle (default 60s):

```
1. [Unused Indexes]  → server uptime gate (≥ unused_minutes)
                      → pg_stat_user_indexes → indexes never scanned

2. [Fragmentation]   → pg_class + pg_am (btree) → max(legacy stats, pgstattuple leaf fill)

3. [SARGable]        → pg_stat_statements → parse AST (sqlparser)
                      → real types + null_frac + density → missing index
                      (disabled if pg_stat_statements absent: DEGRADED mode)
                          │
                          ▼
4. [Laya Health]     → deep_health(): real test payload to /v1/systemone
                      → FAILED ⇒ ALL proposals SKIPPED (no fallback)
                          │
                          ▼
5. [Laya Decision]   → HTTP POST /v1/systemone (per proposal)
                      → choice applied (per type) + confidence ≥ min_confidence
                          │
                          ▼
6. [DDL Execution]   → ONLINE: CREATE/REINDEX CONCURRENTLY (auto-commit),
                      → DROP INDEX in BEGIN/COMMIT
                      → post-DDL validation (index_valid) + cleanup
                        of INVALID indexes → structured logging
```

> **"No fallback" guarantee**: until Laya is active and valid,
> the daemon stays in *observation* mode — it detects, generates and logs
> proposals but applies **no** DDL at all.

## 📁 Project structure

```
pgindexai/
├── Cargo.toml              # Rust dependencies (sqlparser 0.52, rustls, …)
├── Cargo.lock              # Locked (binary project)
├── install.sh              # Hardened installer (dedicated role, targeted pg_hba,
│                           # 0600 config, systemd unit, --dry-run)
├── uninstall.sh            # Uninstallation (service, binary, config,
│                           # --purge, --clean-legacy, --dry-run)
├── config/
│   ├── PGindexAI.yaml        # Configuration template
│   └── pgindexai.service    # systemd unit (reference)
├── sql/
│   ├── demo_schema.sql     # Demo schema (5 tables, 23 indexes)
│   └── demo_workload.sql   # Test workload (8 phases)
├── .github/
│   └── workflows/
│       └── ci.yml          # CI: fmt, clippy, test, shellcheck
├── docs/
│   └── architecture/       # Technical documentation (7 files)
├── src/
│   ├── main.rs             # Entry point, CLI, daemon loop, --dry-run
│   ├── analyzer.rs         # Analysis engine (3 detectors + Laya + no-fallback)
│   ├── state.rs            # DaemonState (Healthy/Degraded)
│   ├── naming.rs           # Index naming convention (63 bytes)
│   ├── config.rs           # YAML loading + validation + warnings
│   ├── ddl.rs              # Online DDL execution (CONCURRENTLY)
│   ├── index_log.rs        # JSONL audit logging + rotation (0640)
│   ├── db/
│   │   ├── mod.rs          # Connection pool + TLS (rustls)
│   │   └── queries.rs      # DMV queries + AST parser (sqlparser)
│   └── laya/
│       ├── mod.rs          # Laya HTTP client (deep_health + decision)
│       └── types.rs        # Laya protocol types
├── LICENSE                 # MIT license
└── README.md
```

## 🛠️ Dependencies

| Crate | Role |
|-------|------|
| `tokio` | Async runtime |
| `tokio-postgres` | PostgreSQL driver |
| `deadpool-postgres` | Connection pool (sized) |
| `reqwest` | HTTP client for Laya |
| `serde` / `serde_yaml` / `serde_json` | Config + log serialization |
| `clap` | CLI parsing |
| `tracing` / `tracing-subscriber` | Structured logging |
| `chrono` | Timestamp handling |
| `thiserror` | Error handling |
| `dirs` | Default config path |
| `tokio-postgres-rustls` | TLS connector for PostgreSQL |
| `rustls` | TLS implementation (ring provider) — chain + hostname always validated |
| `rustls-pemfile` | Reading the `ssl_root_cert` CA (PEM) |
| `webpki-roots` | Default web PKI roots (Mozilla) |
| `sqlparser` | **Real SQL parser (AST) — SARGable detection** |

## 🔑 Key points

- **No fallback** — the daemon never decides on behalf of Laya: Laya absent ⇒ no DDL
- **Deep health check** of Laya (real test payload, validated response) before each batch
- **Online DDL** (`CONCURRENTLY`) with post-DDL validation + cleanup of INVALID indexes
- **Validated TLS** (rustls) on PostgreSQL — chain + hostname always validated, optional `ssl_root_cert` CA, dedicated non-superuser role
- **Hardened installer** (`install.sh`: no `eval`, targeted pg_hba, 0600 config) + side-effect-free **`--dry-run`** + idempotent **`uninstall.sh`** (`--purge` for database/role/pg_hba, `--clean-legacy`)
- **SARGable detection via a real SQL parser** (AST, sqlparser-rs) — no more regex false positives
- **Per-column-type target density** — size estimation for created indexes
- **Naming convention** `idx_<table>_<col>` validated (63 bytes, short hash)
- **Multi-schema** — configurable analysis scope
- **Explicit degraded mode** when `pg_stat_statements` is absent
- **Zero external dependencies** other than PostgreSQL and the Laya server
- **YAML configuration** with robust defaults
- **Structured logging** (JSON or text) with file rotation
- **Complete audit trail** of all index operations
- **Production-ready systemd service**
- **MIT License**

## 📄 License

MIT — See the [LICENSE](LICENSE) file
