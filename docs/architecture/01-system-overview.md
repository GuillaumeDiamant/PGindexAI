# 01 — System overview

> **New features (architectural rework)**:
> 1. Explicit degraded state when `pg_stat_statements` is absent
> 2. **"No fallback"** policy: Laya unreachable → no DDL applied
> 3. SARGable detection via a **real SQL parser** (sqlparser-rs, AST)
> 4. **Online** DDL execution (`CONCURRENTLY`) + post-DDL validation and
>    cleanup of INVALID indexes
> 5. **Target density per column type** (index size estimation)
> 6. **Explicit naming convention** + length validation (63 bytes)
> 7. **Sized connection pool** (configurable size + timeout)
> 8. Laya **deep health check** (real test payload, validated response)
> 9. **Multi-schema support** (configurable analysis scope)
> 10. **Time threshold** on "unused" indexes (observation window)
>
> **Security hardening (audit)**:
> 11. **Validated TLS** (rustls) on the PostgreSQL connection + **dedicated
>     non-superuser PG role** (`pgindexai`)
> 12. **Hardened** installer (`install.sh`: no `eval`, targeted pg_hba, config
>     0600, `shared_preload_libraries` merge) + `--dry-run` mode +
>     idempotent `uninstall.sh` (`--purge`, `--clean-legacy`)
> 13. **Proposal cap** per cycle, log rotation, **CI**
>     (fmt / clippy / test / shellcheck), config validation with
>     unknown-key warnings

## 1. Objective

PGindexAI is a **standalone daemon** that continuously monitors a PostgreSQL database, detects index anomalies (unused, fragmented, missing) and proposes corrective actions validated **exclusively** by the Laya (AI) decision engine — **no decision is made on its behalf** (no heuristic fallback).

## 2. Global architecture

```
┌───────────────────────────────────────────────────────────────────────────┐
│                             pgindexai daemon                              │
│                                                                           │
│  ┌────────────────┐     ┌─────────────────────┐     ┌──────────────────┐ │
│  │    Analyzer    │────▶│     LayaClient      │────▶│   index_log      │ │
│  │                │     │  build_payload()    │     │   Writer         │ │
│  │  3 detectors   │     │  deep_health()      │     │  (JSON file)     │ │
│  │ + DaemonState  │     │  decision()         │     │                  │ │
│  └───────┬────────┘     │  /v1/systemone      │     └──────────────────┘ │
│          │              └──────────┬──────────┘                          │
│          │                         │                                      │
│          │              deep health check before each batch               │
│          │              Laya unreachable → NO DDL (no fallback)           │
│  ┌───────▼─────────────────────────────────────────────────────────────┐ │
│  │   DDL Engine (online: CONCURRENTLY + post-DDL validation)           │ │
│  │   CREATE / DROP / REINDEX ─────────────────────────▶  PostgreSQL    │ │
│  └─────────────────────────────────────────────────────────────────────┘ │
│                                                                           │
│  Cross-cutting: state.rs (Healthy/Degraded) · naming.rs (idx_<t>_<c>,    │
│  63 bytes) · config (multi-schema, pool, densities by type)              │
└───────────────────────────────────────────────────────────────────────────┘
                                 │
                       HTTP POST ▼
                 ┌────────────────────────────┐
                 │      Laya Server (AI)       │
                 │   http://127.0.0.1:8080     │
                 │   /v1/systemone             │
                 └────────────────────────────┘
```

## 3. Components

| Component | Module | Role |
|-----------|--------|------|
| **Analyzer** | `src/analyzer.rs` | Central engine — orchestrates the 3 detectors, holds the `DaemonState`, applies the no-fallback policy |
| **DaemonState** | `src/state.rs` | `Healthy` / `Degraded` state (pg_stat_statements) — when degraded, missing-index detection is disabled |
| **Naming** | `src/naming.rs` | `idx_<table>_<col>` convention, identifier validation, 63-byte limit, short hash (FNV-1a base 36) |
| **Detector : Unused Indexes** | `src/db/queries.rs::get_unused_indexes` | Indexes never scanned **since the last restart** (`idx_scan = 0`, `idx_tup_read = 0`) + observation-window gate |
| **Detector : Fragmentation** | `src/db/queries.rs::get_fragmented_indexes` | **B-tree indexes only**; heuristic score (dead 50 %, mod 30 %, unscanned bonus 20 %) |
| **Detector : SARGable** | `src/db/queries.rs::get_missing_sargable_indexes` | Real AST parser (sqlparser) + real types (`information_schema`) + `null_frac` (`pg_stats`) + target densities |
| **LayaClient** | `src/laya/mod.rs` | HTTP client — **`deep_health()`** (real test payload, validated response), `decision()`, `build_payload()` |
| **DDL Engine** | `src/ddl.rs` | **Online** execution: `CONCURRENTLY` in auto-commit (CREATE/REINDEX), DROP in BEGIN/COMMIT, post-DDL validation + cleanup of INVALID indexes |
| **IndexLog Writer** | `src/index_log.rs` | Structured history of operations (JSON, file rotation) |
| **Config** | `src/config.rs` | YAML loading — multi-schema, pool, densities by type, thresholds |
| **db Pool** | `src/db/mod.rs` | deadpool-postgres pool — **configurable max size + wait timeout** + **rustls TLS** (chain + hostname always validated, web PKI roots + optional PEM) |

## 4. Lifecycle

### Startup

```
main()
  ├── parse CLI args (--config)
  ├── load config (YAML)
  ├── init tracing (JSON or plain)
  ├── create PostgreSQL pool (configurable size + timeout)
  ├── check pg_stat_statements  →  DaemonState (Healthy | Degraded)
  │     └── if absent: WARN "DEGRADED mode" (missing-index detection OFF)
  ├── create LayaClient
  ├── deep_health()  (real test payload sent to /v1/systemone)
  │     └── if FAILED: WARN "Decisions will be SKIPPED (no fallback)"
  └── match command:
        ├── daemon → loop { analyze_cycle() → apply_proposals() → sleep(cycle_seconds) }
        └── run   → analyze_cycle() → apply_proposals() → exit
```

### Analysis cycle (iterative in daemon)

```
1. analyze_unused()                     [if actions.drop_unused]
    ├── observation_window_ok()         ← NEW (fix #10)
    │     └── server uptime < unused_minutes → SKIP (the counters are
    │         reset on restart; no false detection)
    ├── get_unused_indexes()  → pg_stat_user_indexes WHERE idx_scan = 0 AND idx_tup_read = 0
    └── for each unused index:
         └── Proposal { action: DropIndex,
                        ddl: "DROP INDEX IF EXISTS schema.idx",
                        reason: "… never scanned since <restart>. Size: N bytes." }

2. analyze_fragmentation()              [if actions.rebuild_fragmented]
    ├── get_fragmented_indexes()  → pg_stat_user_indexes + pg_class + pg_am (btree) + pg_stat_user_tables
    └── for each index (score ≥ fragmentation_pct, default 70%):
         └── Proposal { action: RebuildIndex, ddl: "REINDEX INDEX CONCURRENTLY schema.idx",
                        reason: "fragmentation at X% (threshold: Y%)" }

3. analyze_missing_sargable()           [DISABLED if DaemonState::Degraded]
    ├── get_missing_sargable_indexes(schemas)
    │     ├── pg_stat_statements top 50 (calls > 10, DDL and pg_* excluded)
    │     ├── parse_query_for_sargable()  ← NEW: sqlparser AST (fix #3)
    │     ├── multi-schema scope (fix #9) + exclusion of pg_* tables
    │     ├── filter columns already covered (pg_indexes)
    │     └── fetch_column_metadata(): real type, nullability, null_frac (fix #5)
    └── for each uncovered column (calls ≥ min_sargable_rows):
         ├── naming::generate_index_name()  ← NEW (fix #6)
         ├── density per type bucket → estimated size (fix #5)
         ├── null_frac > 0.10 → "WHERE col IS NOT NULL" (filtered index)
         └── Proposal { action: CreateIndex | CreateFilteredIndex,
                        ddl: "CREATE INDEX CONCURRENTLY IF NOT EXISTS schema.idx ON …",
                        reason: "SARGable query … type: …, density …, est. size …" }

4. apply_proposals()
    ├── deep_health()  ← NEW (fix #8): 1 call per batch
    │     └── FAILED → ALL proposals SKIPPED (fix #2: no fallback)
    ├── for each proposal:
    │   ├── build_payload(reason, criteria)  (criteria depend on the type)
    │   ├── laya.decision(payload) → { choice, confidence }
    │   ├── confidence ≥ min_confidence (0.7)?  else skip
    │   ├── choice applied?  else skip (debug)
    │   ├── action flag enabled (actions.*)?  else skip (warn)
    │   └── match proposal.action:
    │         ├── DropIndex            → ddl::execute_drop_index()
    │         ├── RebuildIndex         → ddl::execute_reindex()
    │         └── CreateIndex*         → ddl::execute_create_index()
    │              (online execution: CONCURRENTLY, fix #4)
    └── return applied proposals
```

## 5. Anomaly detection

### 5.1 Unused indexes

**Source** : `pg_stat_user_indexes` + `pg_index`

**Prerequisite gate (new)**: the server uptime must be ≥ `thresholds.unused_minutes`,
otherwise the analysis is skipped (the `pg_stat_*` counters are reset on restart).

**Criteria**:
- `idx_scan = 0` AND `idx_tup_read = 0` (never scanned **since the last restart**)
- `indisvalid = true` (valid index)
- `indisprimary = false` (not a primary key)
- `indisunique = false` (not a unique index)

**Proposed action**: `DROP INDEX IF EXISTS schema.index`

The context sent to Laya includes the restart date (`installed_at`) and the
minimum observation duration — the decision engine can weight its confidence
on the strength of the evidence.

### 5.2 Fragmented indexes

**Source** : `pg_stat_user_indexes` + `pg_index` + `pg_class` + `pg_am` + `pg_stat_user_tables`

**Scope (new)**: **B-tree indexes only** (`amname = 'btree'`) —
GiST/GIN/SP-GiST/BRIN indexes do not have the same notion of fragmentation.

**Heuristic score** (weighted multi-criteria):

| Criterion | Weight |
|---------|-------------|
| `n_dead_tup / (n_live_tup + n_dead_tup)` | 50 % |
| `n_mod_since_analyze / n_live_tup` (capped at 1.0) | 30 % |
| Bonus: large unscanned index (`idx_scan = 0` AND `relpages > 100`) | 20 % |

**Decision threshold**: `fragmentation_pct ≥ thresholds.fragmentation_pct` (default 70 %)

**avg_leaf_density**: `index_tuples / index_pages / 100` bounded to `[0, 1]`
(proxy for tuples/page density; PostgreSQL B-tree targets ~90 % fill)

**Proposed action**: `REINDEX INDEX CONCURRENTLY schema.index`

### 5.3 SARGable queries without index

**Source** : `pg_stat_statements` + **sqlparser-rs** parser (new) + `information_schema.columns` + `pg_stats` + `pg_class`

**Filters**:
- `calls > 10` (minimum observed)
- `calls ≥ min_sargable_rows` (default 100)
- Exclusion of DDL, system queries, `pg_*` catalogs
- **Multi-schema scope**: non-empty `analysis.schemas` → only these tables

**SARGable detected (AST)**:
- `col = v`, `col > v`, `col < v`, `col >= v`, `col <= v` (value not a column)
- `col IN (v1, …)` (non-negated), `col BETWEEN a AND b` (non-negated)
- `col IS [NOT] NULL`
- `col LIKE 'pref…%'` / `ILIKE` (**prefix** pattern only)
- `AND` / `OR` / nested parentheses (recursive traversal)

**Rejected (non-SARGable)**:
- `NOT ( … )`, `IS [NOT] DISTINCT FROM`
- `col NOT IN (…)`, `col NOT BETWEEN …`, `col NOT LIKE …`
- Functions on the column (`LOWER(col)`, `UPPER(col)`, `CAST(col …)`)
- Column-to-column comparison
- `LIKE`/`ILIKE` with a leading wildcard or non-literal pattern

**Enrichment (new)**: for each candidate column, the daemon loads
the **real type** (`information_schema.columns`), the **nullability**, the
**NULL fraction** (`pg_stats.null_frac`), and the table **cardinality**
(`pg_class.reltuples`) to estimate the size of the future index via the
**target density of the type bucket** (`density_by_type.*`).

**Existing index**: check via `pg_indexes` — columns already covered
(1st position) are filtered.

**Proposed action**:
- Standard: `CREATE INDEX CONCURRENTLY IF NOT EXISTS schema.idx_table_col ON schema.table(col)`
- Filtered (if `null_frac > 0.10`): `… WHERE col IS NOT NULL`
- Index name: `idx_<table>_<col>` validated (≤ 63 bytes, short hash if needed)

## 6. Laya decision

Before each batch: **deep health check** (a real test payload sent to
`/v1/systemone`, structurally validated response). If it fails, **all**
proposals in the batch are skipped — the daemon never decides on behalf
of Laya ("no fallback" policy).

For each proposal, the daemon sends a **specialized** payload (the daemon
already knows the action it proposes; Laya arbitrates apply / ignore):

```json
{
  "state": "Index idx_products_stock on public.products never scanned since 2026-10-30T09:32:16Z. Size: 90112 bytes. Observed unused for at least 10080min.",
  "questions": {
    "action": {
      "type": "choice",
      "instructions": "Choose the single best action for this PostgreSQL index situation.",
      "criteria": {
        "0": "Keep: index might be needed later",
        "1": "Drop: index is unused — …"
      }
    }
  }
}
```

| Proposal | `"0"` | `"1"` | `"2"` |
|-------------|-------|-------|-------|
| `DROP INDEX` | Keep (ignored) | **APPLY** | — |
| `REINDEX` | Keep (ignored) | **APPLY** | — |
| `CREATE INDEX` | Ignore | **APPLY** (standard) | **APPLY** (filtered) |

Laya response:

```json
{
  "model": "typed-decisions",
  "answers": {
    "action": {
      "type": "choice",
      "choice": "1",
      "confidence": 0.85,
      "probabilities": { "0": 0.15, "1": 0.85 }
    }
  },
  "usage": { "input_tokens": 256, "output_tokens": 12 }
}
```

**Conditional application**:
- `confidence < min_confidence` (default 0.7) → skip (no heuristic)
- `choice` = APPLY for this proposal type → online DDL execution (CONCURRENTLY)
- `choice` = Keep/Ignore → log only
- Unexpected `choice` for this proposal type → `Ignore` (skip, log)

**Security**: each DDL action is coupled to an `actions.*` flag in the
config. Example: `drop_unused: false` blocks any DROP, `create_index: true`
allows CREATEs.

## 7. Logging

### 7.1 Daemon logs (tracing)

JSON format by default, level configurable via `RUST_LOG` or `logging.level`.

### 7.2 Index operation logs (file)

Structured JSONL file, by default at `/var/log/pgindexai/index_operations.log`:

```json
{
  "timestamp": "2026-10-29T15:09:16.830445Z",
  "operation": "CREATE",
  "schema": "public",
  "object": "idx_orders_status",
  "duration_ms": 12.34,
  "status": "success",
  "ddl_statement": "CREATE INDEX CONCURRENTLY IF NOT EXISTS public.idx_orders_status ON public.orders (status)",
  "confidence": 0.85,
  "index_size_bytes": 0,
  "fragmentation_pct": 0.0,
  "reason": "SARGable query on public.orders: status (type: character varying, 5000 calls, 0.1ms avg, null_frac: 0.00). Target density for varchar: 30 tuples/page, estimated index size: 129536 bytes."
}
```

**Verbosity levels**:

| Level | Content |
|--------|---------|
| `minimal` | timestamp, CRUD type, duration, schema, object name |
| `standard` | + DDL of the modified index, confidence, reason |
| `verbose` | + size, fragmentation, technical details |

**Rotation**: 10 MB per file, 5 files max (configurable).

## 8. Dependencies

| Crate | Role | Version |
|-------|------|---------|
| `tokio` | Async runtime | 1.x |
| `tokio-postgres` | PostgreSQL driver | 0.7 |
| `deadpool-postgres` | Connection pool | 0.14 |
| `reqwest` | HTTP client for Laya | 0.12 |
| `serde` / `serde_yaml` | Config serialization | 1.x / 0.9 |
| `serde_json` | Log serialization | 1.x |
| `clap` | CLI parsing | 4.x |
| `tracing` / `tracing-subscriber` | Structured logging | 0.1 / 0.3 |
| `chrono` | Timestamp handling | 0.4 |
| `thiserror` | Error handling | 2.x |
| `dirs` | Default config path | 6.x |
| `tokio-postgres-rustls` | TLS connector for PostgreSQL | 0.14 |
| `rustls` | TLS implementation (ring provider) | 0.23 |
| `rustls-pemfile` | Reading the `ssl_root_cert` certificates (PEM) | 2.x |
| `webpki-roots` | Default web PKI roots (Mozilla) | 0.26 |
| `sqlparser` | **Real SQL parser (AST) for SARGable detection** | 0.52 |

## 9. Behavioral guarantees (new)

| Situation | Guaranteed behavior |
|-----------|----------------------|
| `pg_stat_statements` absent | Daemon `DEGRADED`: unused + fragmentation active, missing-index detection **disabled** (no data source) |
| Laya unreachable / invalid response | **No DDL applied** — proposals generated and logged, application deferred to the next cycle (no fallback) |
| PostgreSQL server restarted less than `unused_minutes` ago | "Unused indexes" analysis **skipped** for the cycle (no false detection) |
| Non-B-tree index | Fragmentation detectors only propose B-tree indexes |
| Laya offline at startup | `deep_health()` FAILED → explicit warning; the daemon continues (observation mode) |
| Index name > 63 bytes | Truncated + short hash suffix (FNV-1a base 36) — identifier always valid |
| `ssl_mode ≠ disable` + unvalidatable certificate (chain or hostname) | **Connection refused** (rustls always validates chain + hostname) — no "accept-insecure" mode |
| Unix socket host (`/…`) + `ssl_mode ≠ disable` | Explicit error at startup (TLS not possible on socket with this driver) |
| Identifiers outside the PG catalog (schema/table/index) | Proposal **rejected** before being sent to Laya (no DDL injection) |
| Unknown key in the YAML config | Loading OK + **warning** naming the ignored key |
| Number of proposals > `max_proposals_per_cycle` | Only the first N (detector order) are sent to Laya |
