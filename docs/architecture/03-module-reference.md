# 03 — Module-by-module reference

## 1. `src/main.rs` — Entry point

### CLI

```
pgindexai [OPTIONS] <COMMAND>

Commands:
  daemon  Run the daemon (continuous analysis cycle)
  run     Run a single analysis cycle and exit
  help    Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>  Path to configuration file
      --dry-run          Analyze and print proposals only — no Laya call, no DDL executed
  -h, --help             Print help
```

`--dry-run` is a **global** flag: with `daemon`, each cycle analyzes and
prints the proposals but calls neither Laya nor DDL; with `run`, the daemon
exits after the analysis.

### Main functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `main()` | `async fn main() -> Result<(), Box<dyn Error + Sync + Send>>` | CLI parsing (incl. `--dry-run`), config loading, tracing init, **PostgreSQL pool (TLS per `ssl_mode`, size + timeout configurable)**, `check_pg_stat_statements` → **`DaemonState`**, `LayaClient` creation, **`deep_health()`**, `daemon`/`run` dispatch |
| `check_pg_stat_statements()` | `async fn check_pg_stat_statements(pool: &Pool) -> Result<DaemonState, …>` | Checks the `pg_stat_statements` extension. Present → `Healthy`, absent → **`Degraded`** (explicit warnings + install instructions) |
| `run_daemon()` | `async fn run_daemon(pool, laya, cfg, daemon_state, dry_run) -> …` | Loop: **a single reused `Analyzer`**, `analyze_cycle()` → `apply_proposals()` (except with `--dry-run`) → sleep **interruptible by SIGINT/SIGTERM** (graceful stop, "Daemon stopped.") |
| `run_once()` | `async fn run_once(pool, laya, cfg, daemon_state, dry_run) -> …` | One cycle: `analyze_cycle()` → print proposals → `apply_proposals()` **except with `--dry-run`** → exit |

### Internal types

| Type | Role |
|------|------|
| `Cli` | CLI struct (clap derive) |
| `Command` | Subcommands: `Daemon`, `Run` |

---

## 2. `src/state.rs` — Daemon state (new)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Healthy,    // pg_stat_statements active — all detections available
    Degraded,   // pg_stat_statements absent — missing-index detection DISABLED
}

impl DaemonState {
    pub fn is_degraded(self) -> bool;
}
```

| Value | Behavior |
|--------|--------------|
| `Healthy` | The 3 detectors are active |
| `Degraded` | `analyze_unused` + `analyze_fragmentation` active; `analyze_missing_sargable` **disabled** (no data source) — explicit warning on each cycle |

The state is determined **once at startup** (checking
`pg_extension`) then carried by the `Analyzer`.

---

## 3. `src/naming.rs` — Index naming convention (new)

```rust
pub const MAX_IDENTIFIER_LEN: usize = 63;   // NAMEDATALEN - 1 (PostgreSQL)

pub enum ValidatedName { AsIs(String), Truncated(String) }
pub enum NamingError { Empty }
```

| Function | Signature | Description |
|----------|-----------|-------------|
| `validate_identifier()` | `(raw: &str) -> Result<ValidatedName, NamingError>` | Cleans (lowercase, `[a-z0-9_]`), `_` prefix if starting with a digit, truncates to 63 bytes if needed |
| `generate_index_name()` | `(schema, table, column) -> Result<String, NamingError>` | **`idx_<table>_<col>`** convention; if too long → truncated + **short hash** (FNV-1a base 36, `[0-9a-z]`) to guarantee uniqueness |
| `is_simple_identifier()` | `(name: &str) -> bool` | True only if the identifier (from the PG catalog) is a simple unquoted `[a-z_][a-z0-9_]*` ≤ 63 bytes — **only these identifiers are injected** into a DDL or a Laya payload (exotic/quoted ones are excluded, logged as warning) |

**Examples**:

| Input | Result |
|--------|----------|
| `table = "orders"`, `column = "status"` | `idx_orders_status` |
| `table = "order_items"`, `column = "shipping_method"` | `idx_order_items_shipping_method` (33 bytes, OK) |
| very long column (overflow) | `idx_<table>_<truncated-col>_<hash6>` (≤ 63 bytes) |

---

## 4. `src/analyzer.rs` — Analysis engine

### Struct

```rust
pub struct Analyzer {
    pool: Pool,
    laya: LayaClient,
    config: Config,
    daemon_state: DaemonState,        // NEW
    log_writer: Arc<IndexLogWriter>, // shared (internal short mutex, synchronous log)
}
```

### Public methods

| Method | Return | Description |
|---------|--------|-------------|
| `new(pool, laya, config, daemon_state)` | `Self` | Constructor — initializes `IndexLogWriter` from the config |
| `daemon_state()` | `DaemonState` | Current state (Healthy / Degraded) |
| `analyze_cycle()` | `async → Vec<Proposal>` | Orchestrates the 3 detectors (per `actions.*` + `daemon_state`) |
| `apply_proposals(proposals)` | `async → Vec<Proposal>` | **single `deep_health()`** → Laya decision per proposal → **online DDL** (`CONCURRENTLY`) → log |

### Private methods

| Method | Description |
|---------|-------------|
| `cap()` | Cap on proposals per cycle (`thresholds.max_proposals_per_cycle`, default 20) — applied in SQL (`LIMIT`) in each detector **and** truncated globally in `analyze_cycle` |
| `guard_identifier(kind, name)` | **New** — rejects (warning) any catalog identifier not conforming to `is_simple_identifier` before DDL / Laya payload |
| `analyze_unused()` | **`observation_window_ok` gate** (uptime ≥ `unused_minutes`) → `get_unused_indexes(pool, cap)` → `guard_identifier` on schema/table/index → `Proposal { DropIndex }` (reason includes restart date + size + observation duration) |
| `analyze_fragmentation()` | `get_fragmented_indexes(pool, cap)` → `fragmentation_pct` threshold → `Proposal { RebuildIndex }` (`REINDEX INDEX CONCURRENTLY`) |
| `analyze_missing_sargable()` | `get_missing_sargable_indexes(pool, schemas, min_calls, cap)` → exclusion of `pg_*` → **`naming::generate_index_name`** → **density per bucket → estimated size × method factor** → `null_frac > 0.10` ⇒ `WHERE … IS NOT NULL` → `Proposal { CreateIndex \| CreateFilteredIndex }` (`CREATE INDEX CONCURRENTLY IF NOT EXISTS … [USING gin\|gist]`); the column's `index_method` (btree/gin/gist) drives the DDL + the `×N` size factor + the log label; **`use_gin_gist_detection = false`** skips non-B-tree columns (pre-GIN/GIST behavior) |
| `density_for_bucket(bucket)` | Bucket → target density from `config.density_by_type` (default 50) |
| `gin_gist_size_factor(method)` | **NEW** — `method` → multiplicative size factor: `gin` → `density_by_type.gin_size_factor` (3.0), `gist` → `.gist_size_factor` (2.0), else `1.0` (btree) |

### `analyze_cycle()` logic

```
1. if actions.drop_unused            → analyze_unused()
2. if actions.rebuild_fragmented     → analyze_fragmentation()
3. if daemon_state == Degraded       → WARN + SARGable analysis DISABLED
    else                              → analyze_missing_sargable()
4. if total > cap() → WARN + truncate to cap()
    (dropped proposals are re-detected on the next cycle)
```

### `apply_proposals()` logic (no-fallback policy)

```
if proposals empty → return []

0. deep_health()                      [1 call per batch]
    └── FAILED → WARN "N proposals SKIPPED (no fallback policy)"
               → return []  (NO DDL)

for proposal in proposals:
    1. criteria = per proposal.action:
          DropIndex            → {"0": Keep, "1": Drop — <reason>}
          RebuildIndex         → {"0": Keep, "1": Rebuild — <reason>}
          CreateIndex*         → {"0": Ignore, "1": Create std, "2": Create filtered}
    2. payload = laya.build_payload(proposal.reason, criteria)
    3. response = laya.decision(payload)
          └── Err → skip this proposal (no fallback)
    4. if confidence < min_confidence → skip (debug)
    5. action_applied = choice valid for this type
          └── no → skip (debug)
    6. if !action_allowed(proposal.action) → skip (warn)
    7. match proposal.action:
          DropIndex            → ddl::execute_drop_index()
          RebuildIndex         → ddl::execute_reindex()
          CreateIndex*         → ddl::execute_create_index()
    8. log result; if success → applied.push(proposal)
return applied
```

---

## 5. `src/db/mod.rs` — PostgreSQL pool

### Functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `create_pool()` | `async fn create_pool(cfg: &PgConfig, pool_cfg: &PgPoolConfig) -> Result<Pool, …>` | Creates a **sized** deadpool-postgres pool with a TLS connector (rustls); **fail-fast**: invalid hostname or Unix socket + TLS → clear error; verifies connectivity (`SELECT 1`) + logs the effective TLS state via `pg_stat_ssl` |
| `build_tls_connector()` | `(cfg: &PgConfig) -> Result<MakeRustlsConnect, …>` | **NEW** — `ring` crypto provider; root store = **web PKI (Mozilla)** + `ssl_root_cert` (PEM, self-signed CA) if provided; no client auth |
| `validate_hostname()` | `(host: &str) -> Result<(), …>` | **NEW** — verifies that `postgresql.host` is a valid hostname for the TLS handshake (avoids a late cryptographic failure) |

### Pool parameters

| Parameter | Source | Default |
|-----------|--------|--------|
| `max_size` | `pg_pool.max_size` | 16 |
| `wait_timeout_ms` | `pg_pool.wait_timeout_ms` (bounded, no infinite blocking) | 30 000 |
| `recycling_method` | — | `Fast` |
| `runtime` | — | `Tokio1` |
| `ssl_mode` | `postgresql.ssl_mode` (mapping: `disable` → `Disable`; `require`/`verify-ca`/`verify-full` → `Require`) | `Disable` |
| `ssl_root_cert` | `postgresql.ssl_root_cert` (PEM, optional — extra CA) | — |

### TLS semantics (rustls) — key points

- **Chain + hostname validation is ALWAYS performed** at handshake
  (unlike libpq where `require` skips validation). `require` is therefore
  **at least as strict** as `verify-ca`/`verify-full`.
- The difference between the 3 active modes lies in the **accepted roots**:
  web PKI (Mozilla) by default, + `ssl_root_cert` (PEM) if provided (self-
  signed / snakeoil CA).
- **Unix socket**: TLS is impossible (no hostname for the handshake)
  → `ssl_mode` **must** be `disable`; otherwise an explicit startup error.
- The server certificate CN/SAN **must match** `postgresql.host`.
- State verified at startup via `pg_stat_ssl` (log "TLS actif" or warning).

---

## 6. `src/db/queries.rs` — DMV queries + SARGable parsing

### Result types

| Type | Key fields |
|------|-------------|
| `UnusedIndexInfo` | `schemaname`, `tablename`, `indexname`, `indexdef`, `idx_scan`, `idx_tup_read`, `idx_tup_fetch`, `size_bytes`, **`installed_at`** (server startup) |
| `FragmentationInfo` | `schemaname`, `tablename`, `indexname`, `relpages`, **`leaf_density`** (`Option<f64>`, measured 0..1), `fragmentation_pct` |
| `MissingSargableIndex` | `schemaname`, `tablename`, `query_text`, `call_count`, `mean_exec_time_ms`, `min/max_exec_time_ms`, `sargable_columns` |
| `SargableColumn` | `column_name`, **`column_type`** (real type via `format_type`), **`is_nullable`**, **`null_frac`**, **`matched_operator`** (`=`,`>`,`IN`,`BETWEEN`,`LIKE`,`IS NULL`,`@>`,`@@`,`&&`,`?`,`?&`,`?|`,`@?`), **`index_method`** (`btree`/`gin`/`gist`), **`density_bucket`** |
| `TableCardinality` | **`reltuples`**, **`relpages`** |

### Public functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `observation_window_ok()` | `(pool, min_minutes) → bool` | **NEW** — `now() - pg_postmaster_start_time() >= make_interval(mins => $1)`; gate for the unused analysis |
| `get_unused_indexes()` | `(pool, max_rows) → Vec<UnusedIndexInfo>` | `pg_stat_user_indexes` + `pg_index` (`idx_scan = 0 AND idx_tup_read = 0 AND indisvalid AND NOT indisprimary AND NOT indisunique`) + **`LIMIT $2`** (SQL-side budget) |
| `get_existing_indexes()` | `(pool, schemas) → HashMap<(schema, table), Vec<(name, def)>>` | `pg_indexes` excluding system schemas, **multi-schema filtering on the Rust side** |
| `get_fragmented_indexes()` | `(pool, max_rows, use_index_bloat) → Vec<FragmentationInfo>` | **B-tree only** (`pg_am`); score = **max**(legacy: dead 50% + mod 30% + bonus 20%, physical: `pgstattuple` leaf fill on indexes > 100 pages); **`LIMIT`** |
| `bloat_score_from_leaf_density()` | `(d: f64) → f64` | **NEW** — maps a measured leaf fill (0..1) to a 0..1 score: `clamp((0.90 − d) / 0.45, 0, 1)`; anchors (PG 16 / pgstattuple 1.5, measured): healthy ≈ 90 % → 0, worst case ≈ 45 % → 1; NaN-safe |
| `get_missing_sargable_indexes()` | `(pool, schemas, min_calls, max_queries) → Vec<MissingSargableIndex>` | `pg_stat_statements` (`calls ≥ min_calls`, **`LIMIT max_queries`**) → **sqlparser AST parse** → multi-schema scope → uncovered columns → **real types + `null_frac` enrichment** |
| `get_table_cardinality()` | `(pool, schema, table) → TableCardinality` | `pg_class` via `to_regclass(format('%I.%I', $1::text, $2::text))`; `reltuples::float8`, `relpages::bigint` |
| `index_exists()` | `(pool, schema, name) → bool` | `pg_indexes WHERE schemaname = $1 AND indexname = $2` |
| `index_valid()` | `(pool, schema, name) → bool` | **NEW** — `indisvalid` via `pg_index`: an **existing but INVALID** index (CONCURRENTLY interrupted) is not operational |
| `cleanup_invalid_indexes()` | `(pool, schema, name) → u64` | **NEW** — drops the INVALID indexes of this name via `pg_drop_index(oid)`; returns the number dropped |

### Private functions (AST parser — new)

| Function | Description |
|----------|-------------|
| `parse_query_for_sargable(query)` | **NEW** — Parses with `PostgreSqlDialect` (sqlparser 0.52) → SARGable columns **assigned to their table** (`Vec<(schema, table, column)>`); **JOIN alias resolution** (`a.status` → the table of alias `a`); an unqualified column is accepted only if the query references only ONE table (conservative behavior, no false positive); failure → `None` (debug) |
| `collect_sargable(expr, …)` | Recursive AST traversal: AND/OR, `=`,`>`,`<`,`>=`,`<=`, `IN` (non-negated), `BETWEEN` (non-negated), `IS [NOT] NULL`, `LIKE`/`ILIKE` prefix — and the **GIN/GIST operators** `@>`, `@@`, `&&`, `?`, `?&`, `?|`, `@?` (the matched operator is recorded per column as `matched_operator`); rejects `NOT`, functions on column, col-vs-col, and `@<` (reverse containment — the indexable side is the right operand) |
| `resolve_index_method(operator, data_type)` | **NEW** — resolves the index access method from the (matched operator, real column type) pair: scalar operators → `btree`; `@>`/`@@`/`&&`/`?`/`?&`/`?|`/`@?` + geometry/geography/`*range` → `gist`; + jsonb/hstore/`*[]`/tsvector → `gin`; unknown type + GIN/GIST operator → `gin` (best-effort) |
| `expr_as_column(expr)` | `Identifier` / `CompoundIdentifier` → column name |
| `expr_is_column_ref(expr)` | True if a column reference (excludes values) |
| `push_column(columns, name)` | Deduplicated column append |
| `like_pattern_is_prefix(pattern)` | `SingleQuotedString` without a leading `%`/`\%` → SARGable |
| `is_column_covered_by_index(indexdef, column)` | Checks the 1st position of the indexdef |
| `fetch_column_metadata(pool, schema, table, …)` | **NEW (per table)** — real type via `format_type(atttypid, atttypmod)` (from `pg_attribute` — `information_schema.data_type` is too lossy for arrays / user types), nullability + `pg_stats` (`null_frac::float8`) |
| `map_pg_type_to_bucket(data_type)` | PostgreSQL type → density bucket (`int4`, `int8`, `float8`, `varchar`, `text`, `date`, `timestamptz`, `uuid`, `bool`, `default`) |
| `truncate_query(query)` | Truncates to 200 characters (UTF-8 safe) + `…` — **local** use (logs) only, never sent to Laya |

### Unit tests (24, of which 2 are live)

*B-tree SARGable*: `eq_and_in`, `between_and_range`,
`like_prefix_vs_wildcard`, `function_on_column_rejected`,
`column_vs_column_rejected`, `compound_identifier_schema_table`,
`invalid_sql_returns_none`, `join_alias_resolution`,
`join_ambiguous_unqualified_column_is_dropped`,
`join_qualified_by_table_name_not_alias`, `cte_name_is_a_phantom_table_not_attributed`,
`truncate_query_multibyte_safe`.

*GIN/GIST (new)*: `gin_gist_operators_are_collected`,
`arrowat_reverse_containment_not_indexable_on_left`,
`scalar_and_gin_gist_same_column_prefers_gin_gist`, `resolve_index_method`,
`map_pg_type_to_bucket_handles_format_type_names`.

*Bloat score (new)*: `bloat_score_healthy_indexes_score_near_zero`,
`bloat_score_worst_case_scores_at_most_one`,
`bloat_score_all_demo_acceptance_values_pass_threshold`,
`bloat_score_linear_interpolation`, `bloat_score_nan_and_out_of_range_are_safe`.

*Live* (connection tests, **clean skip** without local PostgreSQL — CI-safe):
`cardinality_live`, `sargable_system_query_filter_live`.

---

## 7. `src/ddl.rs` — Online DDL execution

### Functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `run_in_transaction()` | `async fn (pool, ddl, timeout_secs) -> Result<u64, …>` | **NEW** — `CONCURRENTLY` → **auto-commit** (forbidden inside a PostgreSQL transaction); otherwise **`BEGIN → DDL → COMMIT`** with rollback on error; when `timeout_secs > 0` bounds the DDL via `SET [LOCAL] statement_timeout` + `SET LOCAL lock_timeout` (quoted `'{n}s'`, transaction-scoped / reset after) |
| `is_concurrent_ddl()` | `(ddl) → bool` | Detects `CONCURRENTLY` (case-insensitive) |
| `execute_create_index()` | `async fn (pool, ddl, timeout_secs, writer, confidence?, reason?, index_method?) -> Result<DdlOutcome, …>` | Pre-existence (`index_exists`) → DDL → **post-DDL validation: `index_valid`** (indisvalid); if INVALID → `cleanup_invalid_indexes` + logged failure; `index_method` (btree/gin/gist) made explicit in the daemon log **and** the audit entry |
| `execute_drop_index()` | `async fn (pool, ddl, timeout_secs, writer, confidence?, size_bytes, reason?) -> Result<DdlOutcome, …>` | `BEGIN → DROP INDEX → COMMIT` → log (success/failure + duration) |
| `execute_reindex()` | `async fn (pool, ddl, timeout_secs, writer, confidence?, size_bytes, fragmentation, reason?) -> Result<DdlOutcome, …>` | `REINDEX CONCURRENTLY` in auto-commit (explicit warning); if build interrupted → **cleanup of the INVALID index** (`pg_drop_index`) — the original index stays intact → otherwise `index_valid` re-checked |
| `build_create_index_ddl()` | `(schema, index_name, table, column, filtered, index_method) → String` | **NEW** — `CREATE INDEX CONCURRENTLY IF NOT EXISTS <name> ON schema.table [USING gin\|gist] (col) [WHERE col IS NOT NULL]`; `USING` emitted only when `index_method` ≠ `btree`; the index **name is not schema-qualified** (PostgreSQL forbids it) |
| `extract_index_name()` | `(ddl) → Option<String>` | Parses `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] name`, `DROP INDEX [IF EXISTS] …`, `REINDEX INDEX [CONCURRENTLY] …` |
| `extract_schema_from_ddl()` | `(ddl) → String` | Parses `ON schema.table` / `INDEX schema.name`, default `public` |

### Execution logic (all DDL)

```
1. name = extract_index_name(ddl) ; schema = extract_schema_from_ddl(ddl)
2. [CREATE] if index_exists(schema, name) → skip (info log)
3. start = Instant::now()
4. run_in_transaction(ddl)
      ├── CONCURRENTLY → direct execution (auto-commit)
      └── otherwise    → BEGIN → execute → COMMIT (error → rollback)
5. [CREATE/REINDEX CONCURRENTLY] post-DDL validation:
      ├── index_valid() (indisvalid)
      └── if INVALID → cleanup_invalid_indexes() (pg_drop_index) → logged failure
6. duration = start.elapsed()
7. IndexLogWriter.log(...)   (configured verbosity)
```

---

## 8. `src/index_log.rs` — Structured history

### Types

| Type | Description |
|------|-------------|
| `CrudOperation` | Enum: `CreateIndex`, `DropIndex`, `Reindex` |
| `VerbosityLevel` | Enum: `Minimal`, `Standard`, `Verbose` |
| `IndexOperationLog` | JSON-serializable struct (timestamp, operation, schema, object, duration, status, DDL, **index_method**, confidence, size, fragmentation, reason) — `index_method` (`btree`/`gin`/`gist`) is always set on CREATE INDEX entries so the audit trail states which kind of index was built |
| `IndexLogWriter` | Writer — `log(&self)` **synchronous** (internal short mutex, never held across an `await`; shareable via `Arc`), **loud failures** (a lost audit entry is logged with `error!`), file rotation, **0640** mode |

### Verbosity level

| Level | Log content |
|--------|----------------|
| `minimal` | `timestamp`, `operation`, `schema`, `object`, `duration_ms`, `status` |
| `standard` | + `ddl_statement`, **`index_method`** (CREATE INDEX), `confidence`, `reason` |
| `verbose` | + `fragmentation_pct`, `index_size_bytes` |

> The **JSON audit file** always carries every set field (via `to_json`,
> `skip_serializing_if = Option::is_none`) — so `index_method` is present on
> every CREATE INDEX entry regardless of the stdout verbosity level.

### File rotation

| Parameter | Default |
|-----------|--------|
| `max_file_size_bytes` | 10 MB |
| `max_files` | 5 |

Rotation: `path → path.1 → … → path.max_files` (the oldest is deleted);
reopened at **0640** (entries contain the executed DDL — audit trail).

### Loud failures (audit policy)

| Situation | Behavior |
|-----------|--------------|
| File impossible to open at startup | explicit `error!` — audit is **stdout only** (never silent) |
| Entry write failure | `error!` "this audit entry is LOST" |
| Rotation failure (rename/delete) | explicit `warn!`/`error!`, best-effort continuation |

---

## 9. `src/laya/mod.rs` — Laya HTTP client

### Struct

```rust
pub struct LayaClient {
    client: reqwest::Client,  // timeout set at the builder
    base_url: String,
}
```

### Methods

| Method | Return | Description |
|---------|--------|-------------|
| `new(base_url, timeout_secs)` | `Self` | Constructor — reqwest client with timeout |
| **`deep_health()`** | `async → Result<String, LayaError>` | **NEW** — real test payload sent to `/v1/systemone` + structural validation: `choice ∈ {"0","1"}` AND `confidence ∈ [0,1]`; detects a broken decision pipeline (model, variant, parsing) |
| `decision(payload)` | `async → Result<LayaResponse, LayaError>` | `POST /v1/systemone` — AI decision |
| `build_payload(state, criteria)` | `LayaPayload` | Builds the payload (criteria sorted `"0","1","2"…`) |

> The `state` text (hence the proposal reason) is built only from
> **validated** identifiers (PG catalog + `is_simple_identifier`) —
> **no raw query text** is ever sent to Laya.

### `LayaError`

| Variant | Triggered by |
|----------|----------------|
| `Unreachable(String)` | Connection error (Laya offline) |
| `Http(String)` | Non-2xx HTTP, response read, reqwest |
| `Deserialize(String, serde_json::Error)` | Invalid response JSON |
| `InvalidResponse(String)` | Unexpected `choice` or `confidence` outside `[0,1]` |

---

## 10. `src/laya/types.rs` — Laya protocol types

### Payload (daemon → Laya)

```rust
pub struct LayaPayload {
    pub state: String,           // Description of the situation
    pub questions: Questions,
}

pub struct Questions {
    pub action: ActionQuestion,
}

pub struct ActionQuestion {
    #[serde(rename = "type")]
    pub type_: String,           // "choice"
    pub instructions: String,    // Instructions for the AI
    pub criteria: HashMap<String, String>,  // Numbered options "0","1","2"…
}
```

### Response (Laya → daemon)

```rust
pub struct LayaResponse {
    pub model: String,           // "typed-decisions"
    pub answers: Answers,
    pub usage: Usage,
}

pub struct Answers {
    pub action: ActionAnswer,
}

pub struct ActionAnswer {
    #[serde(rename = "type")]
    pub type_: String,           // "choice"
    pub choice: String,          // "0" | "1" | "2" (per proposal type)
    pub confidence: f64,         // 0.0 - 1.0
    pub probabilities: HashMap<String, f64>,
}

pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
```

### `DecisionAction`

| Variant | Display |
|----------|---------|
| `Ignore` | `IGNORE` |
| `CreateIndex` | `CREATE INDEX` |
| `CreateFilteredIndex` | `CREATE INDEX (filtered)` |
| `DropIndex` | `DROP INDEX` |
| `RebuildIndex` | `REINDEX` |

The response `choice` is applied **relative to the proposal type**
(in `analyzer::apply_proposals`): `DropIndex`/`RebuildIndex` →
`"1"` = APPLY (otherwise `Ignore`); `CreateIndex`/`CreateFilteredIndex` →
`"1"` = standard, `"2"` = filtered (otherwise `Ignore`) (see `04-protocols.md` § 1.2).

### Proposal (internal)

```rust
pub struct Proposal {
    pub action: DecisionAction,
    pub ddl: String,
    pub reason: String,
    pub confidence: f64,
    pub table: String,
    pub schema: String,                 // #[serde(default)]
    pub index_name: Option<String>,     // #[serde(default)]
    pub column: Option<String>,         // #[serde(default)]
    pub index_method: Option<String>,   // #[serde(default)] — "btree"|"gin"|"gist" (CREATE INDEX)
}
```

Custom serialization: `DecisionAction` ↔ string (`"IGNORE"`, `"CREATE INDEX"`, …).

---

## 11. `src/config.rs` — Configuration

### Config types

| Type | Fields |
|------|--------|
| `Config` | `postgresql`, `laya`, `actions`, `thresholds`, **`analysis`**, **`pg_pool`**, `logging`, `index_logging`, **`density_by_type`** |
| `PgConfig` | `host`, `port`, `user`, `database`, `ssl_mode`, **`password`** (optional), **`ssl_root_cert`** (optional, PEM path) |
| `LayaConfig` | `url`, `timeout_seconds` |
| `ActionsConfig` | `create_index`, `drop_unused`, `rebuild_fragmented` |
| `Thresholds` | `cycle_seconds`, `unused_minutes`, `fragmentation_pct`, **`use_index_bloat`**, **`use_gin_gist_detection`**, `min_sargable_rows`, `min_confidence`, **`max_proposals_per_cycle`**, **`ddl_timeout_seconds`** |
| **`AnalysisConfig`** (new) | `schemas: Vec<String>` (empty = all non-system schemas) |
| **`PgPoolConfig`** (new) | `max_size: u32`, `wait_timeout_ms: u64` |
| `LoggingConfig` | `level`, `format` |
| `IndexLoggingConfig` | `enabled`, `verbosity`, `log_file`, `max_file_size_bytes`, `max_files` |
| **`DensityByTypeConfig`** (new) | `int4`, `int8`, `float8`, `varchar`, `text`, `date`, `timestamptz`, `uuid`, `bool`, `default_density`, **`gin_size_factor`**, **`gist_size_factor`** (all `f64`) |

All new fields are `#[serde(default)]` — an existing config
without these sections remains valid.

### Default values

| Parameter | Default |
|-----------|--------|
| `ssl_mode` | `"disable"` |
| `ssl_root_cert` | — (none; web PKI roots only) |
| `password` | — (null; peer/ident auth or local trust) |
| `timeout_seconds` (laya) | `30` |
| `create_index` | `true` |
| `drop_unused` | `true` |
| `rebuild_fragmented` | `true` |
| `cycle_seconds` | `60` |
| `unused_minutes` | **`10080` (7 days)** |
| `fragmentation_pct` | `70` |
| `use_index_bloat` | `true` |
| `use_gin_gist_detection` | `true` |
| `min_sargable_rows` | `100` |
| `min_confidence` | `0.7` |
| `max_proposals_per_cycle` | `20` |
| `analysis.schemas` | `[]` (all non-system schemas) |
| `pg_pool.max_size` | `16` |
| `pg_pool.wait_timeout_ms` | `30000` |
| `logging.level` | `"info"` |
| `logging.format` | `"json"` |
| `index_logging.enabled` | `true` |
| `index_logging.verbosity` | `"minimal"` |
| `index_logging.max_file_size_bytes` | `10485760` (10 MB) |
| `index_logging.max_files` | `5` |
| `density_by_type.int4` | `200.0` |
| `density_by_type.int8` | `200.0` |
| `density_by_type.float8` | `150.0` |
| `density_by_type.varchar` | `30.0` |
| `density_by_type.text` | `20.0` |
| `density_by_type.date` | `150.0` |
| `density_by_type.timestamptz` | `150.0` |
| `density_by_type.uuid` | `100.0` |
| `density_by_type.bool` | `300.0` |
| `density_by_type.default_density` | `50.0` |
| `density_by_type.gin_size_factor` | `3.0` |
| `density_by_type.gist_size_factor` | `2.0` |

### Validation (`Config::validate`) and unknown keys

- `validate()` rejects dangerous configs: `cycle_seconds ∉ [1, 86400]`,
  `fragmentation_pct ∉ [1, 100]`, `min_confidence ∉ [0,1]`,
  `max_proposals_per_cycle ∉ [1, 1000]`, `pg_pool.max_size ∉ [1, 512]`,
  `wait_timeout_ms ∉ [100, 600000]`, `ssl_mode` outside
  `disable|require|verify-ca|verify-full`, empty `host`/`user`/`database`,
  densities not strictly positive.
- **Unknown keys**: `warn_unknown_keys()` — a warning per ignored key
  (e.g. `config: key 'laya.variant' unknown (ignored — typo or removed
  feature?)`), non-blocking load.
- `ssl_mode = verify-ca|verify-full` **without** `ssl_root_cert` → warning
  (self-signed certificates not accepted).

### Default config path

```
~/.config/PGindexAI/PGindexAI.yaml    (on Linux)
./PGindexAI.yaml                     (fallback)
```

---

## 12. `sql/` — Demo scripts

### `sql/demo_schema.sql`

- Creates the `pgindexai_demo` database if it doesn't exist
- 5 tables: `customers`, `products`, `orders`, `order_items`, `users`
- 23 indexes: 15 explicit business indexes + 8 PK/UK constraints
- `ANALYZE` on all tables

### `sql/demo_workload.sql`

- 8 phases of realistic workload
- Generates `pg_stat_statements` entries for the 3 detectors
- Includes realistic fragmentation (DELETE + INSERT + UPDATE + VACUUM FULL)
- Repetitive SARGable workload (17 000 queries via `EXECUTE … USING`)
