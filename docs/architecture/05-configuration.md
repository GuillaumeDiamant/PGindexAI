# 05 — Configuration

> **New (security)**: `postgresql.password` and
> `postgresql.ssl_root_cert` (self-signed CA), `thresholds.max_proposals_per_cycle`
> (budget per cycle), removal of dead fields (`laya.ready_timeout_seconds`,
> `laya.binary_path`, `laya.model_path`, `laya.variant`, `laya.cpu_mode`,
> `thresholds.idle_seconds`). Any unknown key triggers a **warning**
> visible at startup (not a silent error).

## 1. Location

| Method | Path |
|---------|--------|
| Default | `~/.config/PGindexAI/PGindexAI.yaml` |
| CLI | `pgindexai --config /path/to/config.yaml daemon` |
| Fallback | `./PGindexAI.yaml` (if `config_dir()` is `None`) |

**Recommended permissions: `0600`** — the file may contain a password.

## 2. Full configuration

```yaml
# ─────────────────────────────────────────────
# PostgreSQL
# ─────────────────────────────────────────────
postgresql:
  host: "/var/run/postgresql"     # Unix socket, or hostname/IP for TCP
  port: 5432                       # PostgreSQL port
  user: "pgindexai"                 # Dedicated non-superuser role (see 06)
  database: "pgindexai_demo"            # Target database
  password: ""                     # Required if pg_hba requires scram-sha-256 / md5
  ssl_mode: "disable"              # disable | require | verify-ca | verify-full
  # ssl_root_cert: ""              # PEM path of a self-signed CA (snakeoil,
                                   # internal) for verify-ca / verify-full.
                                   # Without this key: web PKI (Mozilla) roots
                                   # only.

# ─────────────────────────────────────────────
# Laya server (AI decision engine)
# ─────────────────────────────────────────────
laya:
  url: "http://127.0.0.1:8080"    # HTTP endpoint
  timeout_seconds: 30              # /v1/systemone request timeout (1..=600)

# ─────────────────────────────────────────────
# DDL action control
# ─────────────────────────────────────────────
actions:
  create_index: true               # Allow CREATE INDEX
  drop_unused: true                # Allow DROP INDEX
  rebuild_fragmented: true         # Allow REINDEX

# ─────────────────────────────────────────────
# Decision thresholds
# ─────────────────────────────────────────────
thresholds:
  cycle_seconds: 60                # Cycle frequency (1..=86400)
  unused_minutes: 10080            # Observation window (default 7 days)
  fragmentation_pct: 70            # Fragmentation % (1..=100)
  use_index_bloat: true            # Physical bloat via pgstattuple (leaf fill)
  use_gin_gist_detection: true     # GIN/GIST index-need detection (jsonb/arrays/tsvector → GIN, geometry/ranges → GIST)
  min_sargable_rows: 100           # Min. executions (pg_stat_statements.calls)
  min_confidence: 0.7              # Min. Laya confidence for APPLY (0.0..=1.0)
  max_proposals_per_cycle: 20      # Proposal budget per cycle (1..=1000)
  ddl_timeout_seconds: 1800        # Long CONCURRENTLY DDL timeout (0 = unlimited, 0..=86400)

# ─────────────────────────────────────────────
# Analysis scope (multi-schema)
# ─────────────────────────────────────────────
analysis:
  schemas: []                      # [] = all non-system schemas

# ─────────────────────────────────────────────
# PostgreSQL connection pool
# ─────────────────────────────────────────────
pg_pool:
  max_size: 16                     # 1..=512
  wait_timeout_ms: 30000           # 100..=600000

# ─────────────────────────────────────────────
# Target density per column type
# (tuples/page targeted — used for the estimated
# size of future indexes sent to Laya)
# + GIN/GIST size factors (GIN/GIST indexes are
# typically larger than a B-tree on the same data)
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
  gin_size_factor: 3.0             # × est_bytes for GIN indexes
  gist_size_factor: 2.0            # × est_bytes for GIST indexes

# ─────────────────────────────────────────────
# Logging (tracing)
# ─────────────────────────────────────────────
logging:
  level: "info"                    # trace | debug | info | warn | error
  format: "json"                   # json | plain ("text" is also accepted)

# ─────────────────────────────────────────────
# Index operation logging (audit trail)
# ─────────────────────────────────────────────
index_logging:
  enabled: true
  verbosity: "minimal"             # minimal | standard | verbose
  log_file: "/var/log/pgindexai/index_operations.log"
  max_file_size_bytes: 10485760    # rotate at 10 MB (0 = disabled)
  max_files: 5
```

## 3. Default values

| Parameter | Default | Constraints |
|-----------|--------|-------------|
| `postgresql.host` | — (required) | non-empty |
| `postgresql.port` | — (required) | valid port |
| `postgresql.user` | — (required) | non-empty |
| `postgresql.database` | — (required) | non-empty |
| `postgresql.password` | `null` | required if scram/md5 auth |
| `postgresql.ssl_mode` | `disable` | `disable` \| `require` \| `verify-ca` \| `verify-full` |
| `postgresql.ssl_root_cert` | `null` | readable PEM path |
| `laya.url` | — (required) | non-empty |
| `laya.timeout_seconds` | `30` | `1..=600` |
| `actions.create_index` | `true` | bool |
| `actions.drop_unused` | `true` | bool |
| `actions.rebuild_fragmented` | `true` | bool |
| `thresholds.cycle_seconds` | `60` | `1..=86400` |
| `thresholds.unused_minutes` | `10080` (7 d) | `1 ..= 525600` (1 year) |
| `thresholds.fragmentation_pct` | `70` | `1..=100` |
| `thresholds.use_index_bloat` | `true` | bool |
| `thresholds.use_gin_gist_detection` | `true` | bool |
| `thresholds.min_sargable_rows` | `100` | `>= 1` |
| `thresholds.min_confidence` | `0.7` | `0.0..=1.0` |
| `thresholds.max_proposals_per_cycle` | `20` | `1..=1000` |
| `thresholds.ddl_timeout_seconds` | `1800` (30 min) | `0..=86400` (0 = unlimited) |
| `analysis.schemas` | `[]` (all non-system schemas) | list |
| `pg_pool.max_size` | `16` | `1..=512` |
| `pg_pool.wait_timeout_ms` | `30000` | `100..=600000` |
| `density_by_type.*` | see §2 | `> 0` and finite |
| `density_by_type.gin_size_factor` | `3.0` | `> 0` and finite |
| `density_by_type.gist_size_factor` | `2.0` | `> 0` and finite |
| `logging.level` | `info` | tracing level |
| `logging.format` | `json` | `json` \| `plain` (or `text`) |
| `index_logging.enabled` | `true` | bool |
| `index_logging.verbosity` | `minimal` | `minimal` \| `standard` \| `verbose` |
| `index_logging.log_file` | `""` (stdout) | path |
| `index_logging.max_file_size_bytes` | `10485760` | bytes, `0` = off |
| `index_logging.max_files` | `5` | integer |

## 4. Environment variables

| Variable | Usage |
|----------|-------|
| `RUST_LOG` | Overrides the logging level (e.g. `RUST_LOG=debug`) |
| `RUST_BACKTRACE` | Enables backtraces (e.g. `RUST_BACKTRACE=1`) |

## 5. TLS semantics (important)

The client uses **rustls** (`tokio-postgres-rustls`). Consequences:

| `ssl_mode` | Effective behavior |
|------------|----------------------|
| `disable` | No TLS. **Only possible mode on a Unix socket** (clear error otherwise). |
| `require` | TLS **with full certificate validation** (chain + hostname) — at least as strict as libpq `verify-full`. |
| `verify-ca` | Identical to `require` with rustls (validation is always full). |
| `verify-full` | Identical to `require` with rustls. |

- **Trusted roots**: web PKI (Mozilla) **and** the `ssl_root_cert` PEM file
  if provided. A self-signed certificate (snakeoil, internal CA)
  **without** `ssl_root_cert` is **rejected** (`InvalidCertificate`).
- The difference vs libpq: `require` there means "encrypt without validating";
  here every TLS mode validates. That is stricter — it is intentional.
- The certificate hostname (CN/SAN) must match `postgresql.host`
  (e.g. `DNS:localhost,IP:127.0.0.1` for local).

## 6. Behavior driven by the configuration

| Configuration | Behavior |
|---------------|--------------|
| `analysis.schemas: []` | All non-system schemas are analyzed |
| `analysis.schemas: [a, b]` | Only the listed schemas |
| `pg_pool.wait_timeout_ms` | If the pool is saturated, the wait is bounded |
| `density_by_type.*` | Used only for the **size estimation** of future indexes sent to the Laya decision context — no effect on detection |
| `density_by_type.gin_size_factor` / `.gist_size_factor` | Multiplicative factor on the size estimate for **GIN** / **GIST** proposals (these access methods are typically larger than a B-tree on the same data). `btree` proposals use a factor of `1.0` — their estimate is unchanged |
| `thresholds.unused_minutes` | **Minimum observation window**: if the PG server has been up for less than this delay, the "unused indexes" analysis is skipped (the `pg_stat_*` counters are reset at restart) |
| `thresholds.use_index_bloat` | `true` = the fragmentation detector also measures **physical B-tree bloat** via `pgstattuple` (the only criterion that survives VACUUM/ANALYZE/autovacuum). `false` = legacy catalog-statistics heuristic only. Absence of the extension never degrades the daemon — it only disables this criterion (warning at startup) |
| `thresholds.use_gin_gist_detection` | `true` = detector #3 also detects the GIN/GIST operator patterns (`jsonb @>/?/?&/?|/@?`, `tsvector @@`, arrays `@>/&&`, geometry `@@/&&`, ranges `@>`) and proposes **`USING gin` / `USING gist`** indexes. `false` = detector #3 restricted to classic B-tree SARGable patterns (the pre-GIN/GIST behavior) |
| `thresholds.min_confidence` | Laya confidence threshold; **below it, the proposal is ignored (no heuristic fallback)** |
| `thresholds.max_proposals_per_cycle` | Each detector is bounded in SQL (`LIMIT`) **and** the total list is truncated to this budget with a warning (dropped proposals will be re-detected on the next cycle) |
| `thresholds.ddl_timeout_seconds` | Bounds the duration of `CONCURRENTLY` builds (`statement_timeout` per session, reset afterwards); `0` = unlimited. Keep it generous: a too-short timeout on a large table would create a build→cancelled→retry loop |
| unknown key in the config | **Warning** at startup (e.g. `dashboard:`, `idle_seconds:`) — the key is ignored but reported |

## 7. Validation

Config loading fails with:

| Error | Cause |
|--------|-------|
| `Read(path, err)` | File missing or inaccessible |
| `Parse(path, err)` | Invalid YAML (indentation, syntax) or unexpected type |
| `Validation("...")` | Value inconsistency (list of errors concatenated) — e.g. `cycle_seconds = 0`, `min_confidence = 1.5`, unsupported `ssl_mode`, `density_by_type.gin_size_factor` not `> 0` or not finite |

Validation errors are **accumulated**: all inconsistencies are
reported at once, not just the first one encountered.

## 8. Configuration examples

### Production (Laya active, TLS validated)

```yaml
postgresql:
  host: "pg.example.com"
  port: 5432
  user: "pgindexai"
  database: "production"
  password: "…"                      # or via pg_hba peer + socket
  ssl_mode: "verify-full"
  ssl_root_cert: "/etc/ssl/private/pgindexai-ca.pem"  # Internal CA

laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 30

actions:
  create_index: true
  drop_unused: false    # DROP disabled in production
  rebuild_fragmented: true

thresholds:
  min_confidence: 0.9
  min_sargable_rows: 500
  max_proposals_per_cycle: 10   # conservative budget in prod

analysis:
  schemas: ["public", "billing", "crm"]

pg_pool:
  max_size: 32
  wait_timeout_ms: 15000
```

### Safe mode (no automatic action)

```yaml
actions:
  create_index: false
  drop_unused: false
  rebuild_fragmented: false
```

### Dev mode (local, detailed log)

```yaml
postgresql:
  host: "/var/run/postgresql"
  user: "pgindexai"
  database: "pgindexai_demo"
  ssl_mode: "disable"    # Unix socket → TLS not applicable

laya:
  url: "http://127.0.0.1:8080"

logging:
  level: "debug"
  format: "text"

index_logging:
  verbosity: "verbose"
  log_file: "/tmp/pgindexai-operations.log"
```
