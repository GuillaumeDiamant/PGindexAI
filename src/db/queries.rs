use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_postgres::types::{ToSql, Type};
use tracing::{debug, warn};

// ─────────────────────────────────────────────
// Analysis result types
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnusedIndexInfo {
    pub schemaname: String,
    pub tablename: String,
    pub indexname: String,
    pub indexdef: String,
    pub idx_scan: i64,
    pub idx_tup_read: i64,
    pub idx_tup_fetch: i64,
    pub size_bytes: i64,
    /// Start of the observation window (server restart)
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentationInfo {
    pub schemaname: String,
    pub tablename: String,
    pub indexname: String,
    pub relpages: i64,
    /// Measured B-tree leaf page fill (0..1), from pgstattuple's
    /// `pgstatindex().avg_leaf_density` (leaf byte-fill, source-verified;
    /// returned by the extension as a percent 0..100 — normalized here).
    /// `None` when pgstattuple is unavailable, the index is below the
    /// measurement cost threshold, or the probe failed — in that case
    /// the score rests on the legacy heuristic alone.
    pub leaf_density: Option<f64>,
    pub fragmentation_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingSargableIndex {
    pub schemaname: String,
    pub tablename: String,
    pub query_text: String,
    pub call_count: i64,
    pub mean_exec_time_ms: f64,
    pub min_exec_time_ms: f64,
    pub max_exec_time_ms: f64,
    pub sargable_columns: Vec<SargableColumn>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SargableColumn {
    pub column_name: String,
    /// Actual PostgreSQL type (e.g. "integer", "jsonb", "text[]", "int4range") —
    /// the real type name from `format_type(atttypid, atttypmod)` (not the
    /// lossy `information_schema.data_type`, which reports "ARRAY" /
    /// "USER-DEFINED" for arrays and extension types).
    pub column_type: String,
    pub is_nullable: bool,
    pub null_frac: f64,
    /// Density bucket (e.g. "int4", "varchar", "timestamptz", "default")
    pub density_bucket: String,
    /// The predicate operator that matched this column: "=" / ">" / "IN" /
    /// "BETWEEN" / "LIKE" / "IS NULL" (classic B-tree SARGable), or one of
    /// the GIN/GIST-family operators: "@>", "@@", "&&", "?", "?&", "?|", "@?".
    pub matched_operator: String,
    /// Resolved index access method: "btree" | "gin" | "gist".
    /// Computed from (matched_operator, column_type) after type enrichment;
    /// stays "btree" while the type is unknown (conservative).
    pub index_method: String,
}

/// Cardinality of a table (to estimate the size of a future index)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableCardinality {
    pub reltuples: f64,
    pub relpages: i64,
}

// ─────────────────────────────────────────────
// DMV queries
// ─────────────────────────────────────────────

/// Check whether the observation window is sufficient to qualify an
/// index as "unused".
///
/// The pg_stat_* counters are reset when PostgreSQL restarts:
/// an index never scanned may have been scanned just before the last restart.
/// The "unused" detection is therefore only applied if the server has been up for
/// at least `min_observation_minutes` minutes.
pub async fn observation_window_ok(
    pool: &Pool,
    min_observation_minutes: u64,
) -> Result<bool, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "SELECT now() - pg_postmaster_start_time() >= make_interval(mins => $1)",
            &[&(min_observation_minutes as i32)],
        )
        .await?;
    Ok(row.get::<_, bool>(0))
}

/// Fetch the unused indexes (never scanned within the observation window).
/// `max_rows` bounds the number of candidates (proposal budget per cycle).
pub async fn get_unused_indexes(
    pool: &Pool,
    max_rows: i64,
) -> Result<Vec<UnusedIndexInfo>, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;

    let rows = client
        .query(
            r#"
            SELECT
                s.schemaname,
                s.relname AS tablename,
                s.indexrelname AS indexname,
                pg_get_indexdef(s.indexrelid) AS indexdef,
                COALESCE(s.idx_scan, 0) AS idx_scan,
                COALESCE(s.idx_tup_read, 0) AS idx_tup_read,
                COALESCE(s.idx_tup_fetch, 0) AS idx_tup_fetch,
                pg_relation_size(s.indexrelid) AS size_bytes,
                pg_postmaster_start_time() AS installed_at
            FROM pg_stat_user_indexes s
            JOIN pg_index i ON s.indexrelid = i.indexrelid
            WHERE s.idx_scan = 0
              AND s.idx_tup_read = 0
              AND i.indisvalid = true
              AND i.indisprimary = false
              AND i.indisunique = false
            ORDER BY pg_relation_size(s.indexrelid) DESC
            LIMIT $1
            "#,
            &[&max_rows],
        )
        .await?;

    let mut result = Vec::new();
    for row in &rows {
        result.push(UnusedIndexInfo {
            schemaname: row.get("schemaname"),
            tablename: row.get("tablename"),
            indexname: row.get("indexname"),
            indexdef: row.get("indexdef"),
            idx_scan: row.get("idx_scan"),
            idx_tup_read: row.get("idx_tup_read"),
            idx_tup_fetch: row.get("idx_tup_fetch"),
            size_bytes: row.get("size_bytes"),
            installed_at: row.get("installed_at"),
        });
    }

    debug!("Found {} unused indexes", result.len());
    Ok(result)
}

/// Fetch the list of existing indexes per table (multi-schema)
/// Returns a map: (schemaname, tablename) → Vec<(indexname, indexdef)>
///
/// The schema filter is pushed into the query (`schemaname = ANY($1)`)
/// to avoid loading the whole catalog when a scope is configured.
pub async fn get_existing_indexes(
    pool: &Pool,
    schemas: &[String],
) -> Result<
    HashMap<(String, String), Vec<(String, String)>>,
    Box<dyn std::error::Error + Sync + Send>,
> {
    let client = pool.get().await?;

    let rows = if schemas.is_empty() {
        // All non-system schemas
        client
            .query(
                r#"
                SELECT schemaname, tablename, indexname, indexdef
                FROM pg_indexes
                WHERE schemaname NOT IN ('pg_catalog', 'information_schema')
                ORDER BY schemaname, tablename, indexname
                "#,
                &[],
            )
            .await?
    } else {
        client
            .query(
                r#"
                SELECT schemaname, tablename, indexname, indexdef
                FROM pg_indexes
                WHERE schemaname = ANY($1)
                  AND schemaname NOT IN ('pg_catalog', 'information_schema')
                ORDER BY schemaname, tablename, indexname
                "#,
                &[&schemas.to_vec()],
            )
            .await?
    };

    let mut map: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for row in &rows {
        let schemaname: String = row.get("schemaname");
        let key = (schemaname, row.get("tablename"));
        map.entry(key)
            .or_default()
            .push((row.get("indexname"), row.get("indexdef")));
    }

    debug!(
        "Loaded {} existing index groups (schemas: {:?})",
        map.len(),
        if schemas.is_empty() {
            "all".to_string()
        } else {
            schemas.join(",")
        }
    );
    Ok(map)
}

/// Maps a measured B-tree leaf fill (0..1) to a fragmentation score (0..1).
///
/// Empirically calibrated anchors (PostgreSQL 16 / pgstattuple 1.5,
/// measured on real databases, fresh builds of 100k–250k rows):
///   * healthy index: ~90 % leaf fill (measured 89.80, 89.83, 89.92, 90.05);
///   * worst case (50 % of the rows churned, then VACUUM): ~45 % leaf fill —
///     the index is roughly TWICE the size of the healthy one, and REINDEX
///     reclaims about half the space (measured 45.06 % on a clean table).
///
/// `score = clamp((0.90 − d) / 0.45, 0, 1)`: a healthy index (~90 %) scores
/// ≈ 0, an index at the 45 % anchor scores 1.0, and in between the score
/// is linear (e.g. 67.5 % fill → 0.5).
pub fn bloat_score_from_leaf_density(d: f64) -> f64 {
    const HEALTHY_FILL: f64 = 0.90;
    const WORST_FILL: f64 = 0.45;
    if !d.is_finite() {
        return 0.0;
    }
    let d = d.clamp(0.0, 1.0);
    ((HEALTHY_FILL - d) / (HEALTHY_FILL - WORST_FILL)).clamp(0.0, 1.0)
}

/// Fetch the fragmented B-tree indexes
///
/// Two criteria are combined, final score = `max` of the two:
///
///   1. LEGACY heuristic (works with no extra extension): dead tuples
///      ratio (50 %) + unanalyzed modifications (30 %) + large index never
///      scanned (20 %).
///   2. PHYSICAL bloat (requires `pgstattuple`): the measured leaf page
///      fill from `pgstatindex()` (a full index scan), mapped to a score by
///      `bloat_score_from_leaf_density`.
///
/// Known limitation of the legacy criterion: those statistics are reset by
/// VACUUM / ANALYZE, so the score drops to zero right after autovacuum — a
/// physically bloated index becomes invisible to it. The physical criterion
/// is robust against autovacuum: the free space left behind by VACUUM stays
/// on the leaf pages until a REINDEX.
///
/// `use_index_bloat = false` (or extension missing / probe failure) degrades
/// gracefully to criterion 1 alone — the previous behavior.
pub async fn get_fragmented_indexes(
    pool: &Pool,
    max_rows: i64,
    use_index_bloat: bool,
) -> Result<Vec<FragmentationInfo>, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;

    let rows = client
        .query(
            r#"
            WITH index_info AS (
                SELECT
                    s.schemaname,
                    pi.indrelid AS table_oid,
                    s.relname AS tablename,
                    s.indexrelname AS indexname,
                    s.indexrelid AS index_oid,
                    pg_relation_size(s.indexrelid) / 8192 AS relpages,
                    COALESCE(s.idx_tup_read, 0) AS idx_tup_read,
                    COALESCE(s.idx_scan, 0) AS idx_scan
                FROM pg_stat_user_indexes s
                JOIN pg_index pi ON s.indexrelid = pi.indexrelid
                JOIN pg_class pc ON s.indexrelid = pc.oid
                JOIN pg_am am ON pc.relam = am.oid
                WHERE am.amname = 'btree'
                  AND pg_relation_size(s.indexrelid) > 0
            ),
            table_stats AS (
                SELECT
                    c.oid,
                    COALESCE(t.n_live_tup, 0) AS n_live_tup,
                    COALESCE(t.n_dead_tup, 0) AS n_dead_tup,
                    COALESCE(t.n_mod_since_analyze, 0) AS n_mod_since_analyze
                FROM pg_class c
                LEFT JOIN pg_stat_user_tables t ON c.oid = t.relid
            )
            SELECT
                ii.schemaname,
                ii.tablename,
                ii.indexname,
                ii.index_oid,
                ii.relpages,
                ii.idx_scan,
                ts.n_live_tup,
                ts.n_dead_tup,
                ts.n_mod_since_analyze
            FROM index_info ii
            LEFT JOIN table_stats ts ON ts.oid = ii.table_oid
            ORDER BY ii.relpages DESC
            LIMIT $1
            "#,
            &[&max_rows],
        )
        .await?;

    let mut result = Vec::new();
    // Once a pgstatindex probe fails (extension missing, permissions, ...),
    // stop probing for the rest of the cycle: warn once, do not spam.
    let mut bloat_probe_disabled = false;

    for row in &rows {
        let relpages: i64 = row.get("relpages");
        let index_oid: u32 = row.get("index_oid"); // pg_stat_user_indexes.indexrelid (oid)
        let n_live_tup: i64 = row.get("n_live_tup");
        let n_dead_tup: i64 = row.get("n_dead_tup");
        let n_mod_since_analyze: i64 = row.get("n_mod_since_analyze");
        let idx_scan: i64 = row.get("idx_scan");

        // ── Criterion A (legacy): catalog statistics ────────────────────
        // 1. dead tuples ratio (50%)
        let total_tuples = (n_live_tup + n_dead_tup) as f64;
        let dead_ratio = if total_tuples > 0.0 {
            n_dead_tup as f64 / total_tuples
        } else {
            0.0
        };
        let mut legacy_score = dead_ratio * 0.5;

        // 2. unanalyzed modifications (30%)
        let mod_ratio = if n_live_tup > 0 {
            (n_mod_since_analyze as f64 / n_live_tup as f64).min(1.0)
        } else {
            0.0
        };
        legacy_score += mod_ratio * 0.3;

        // 3. large index never scanned (20%)
        if idx_scan == 0 && relpages > 100 {
            legacy_score += 0.2;
        }

        // ── Criterion B (physical): measured leaf fill ──────────────────
        // `pgstatindex` performs a FULL INDEX SCAN per call, so we only
        // measure indexes above 100 pages (cost guard). On the first
        // failure we disable probing for the cycle (graceful fallback).
        //
        // NB: the parameter type is declared explicitly (Oid): the OID is
        // passed to the `pgstatindex(regclass)` overload — an untyped $1
        // would resolve to the `pgstatindex(text)` overload and the driver
        // would reject the integer.
        let leaf_density: Option<f64> = if use_index_bloat
            && !bloat_probe_disabled
            && relpages > 100
        {
            let params = [(&index_oid as &(dyn ToSql + Sync), Type::OID)];
            match client
                .query_typed_opt(
                    "SELECT (pgstatindex($1::regclass)).avg_leaf_density",
                    &params,
                )
                .await
            {
                Ok(r) => r
                    .and_then(|r| r.get::<_, Option<f64>>(0))
                    // pgstattuple returns a PERCENT (0..100); normalize to 0..1
                    .map(|d| d / 100.0)
                    .filter(|d| d.is_finite()),
                Err(e) => {
                    warn!(
                            "pgstattuple probe failed for {}.{}: {e} — physical bloat \
                             detection disabled for this cycle (falling back to the legacy heuristic)",
                            row.get::<_, String>("schemaname"),
                            row.get::<_, String>("indexname"),
                        );
                    bloat_probe_disabled = true;
                    None
                }
            }
        } else {
            None
        };

        // Final score: the strongest signal wins (no regression possible —
        // the legacy score alone is the fallback when pgstattuple is absent).
        let bloat_score = leaf_density.map(bloat_score_from_leaf_density);
        let frag_score = bloat_score.map_or(legacy_score, |b| legacy_score.max(b));
        let fragmentation_pct = (frag_score * 100.0).min(100.0);

        result.push(FragmentationInfo {
            schemaname: row.get("schemaname"),
            tablename: row.get("tablename"),
            indexname: row.get("indexname"),
            relpages,
            leaf_density,
            fragmentation_pct,
        });
    }

    debug!(
        "Found {} potentially fragmented btree indexes",
        result.len()
    );
    Ok(result)
}

/// Check whether a column is already covered in first position by an
/// existing index on the table (indexdef parsing, no execution)
fn is_column_covered_by_index(indexdef: &str, column_name: &str) -> bool {
    // The indexdef looks like: CREATE INDEX idx ON schema.table USING btree (col1, col2)
    let upper = indexdef.to_uppercase();
    if let Some(paren_open) = upper.rfind('(') {
        let inner = &indexdef[paren_open + 1..];
        if let Some(paren_close) = inner.find(')') {
            let columns = &inner[..paren_close];
            for col in columns.split(',') {
                let cleaned = col
                    .trim()
                    .trim_matches('"')
                    .trim_matches(|c| c == ' ' || c == '\t');
                // Strip the operator/class (e.g.: col DESC, col COLLATE ...)
                let col_name = cleaned.split_whitespace().next().unwrap_or(cleaned);
                if col_name.to_lowercase() == column_name.to_lowercase() {
                    return true;
                }
            }
        }
    }
    false
}

/// Exclusion filter for `pg_stat_statements` SARGable candidates: removes
/// the daemon's own catalog queries and any query on a system schema.
///
/// NB — this is evaluated by PostgreSQL's ARE dialect, not PCRE:
///   - `\y` is the word boundary (`\b` is a LITERAL BACKSPACE — a `\bpg_`
///     filter therefore silently never matched);
///   - `[\s\S]` spans newlines (a plain `.` does not).
const SARGABLE_SYSTEM_QUERY_FILTER: &str = r"^(SELECT|WITH)[\s\S]*\y(pg_|information_schema\.)";

/// Detect SARGable queries with no index covering the filtered column.
///
/// - `min_calls`: minimum number of executions (pg_stat_statements.calls) —
///   applied in the SQL query (no more hardcoded `calls > 10` threshold).
/// - `max_queries`: budget of candidate queries examined (SQL LIMIT).
///
/// Result grouped by (schema, table): a JOIN query can produce columns
/// attributed to DIFFERENT tables (alias resolution).
pub async fn get_missing_sargable_indexes(
    pool: &Pool,
    schemas: &[String],
    min_calls: i64,
    max_queries: i64,
) -> Result<Vec<MissingSargableIndex>, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;

    let rows = client
        .query(
            r#"
            SELECT
                s.queryid,
                s.query,
                s.calls,
                s.mean_exec_time,
                s.min_exec_time,
                s.max_exec_time,
                s.rows
            FROM pg_stat_statements s
            WHERE s.calls >= $3
              AND s.query !~* $1
              AND s.query !~* $2
            ORDER BY (s.calls * GREATEST(s.mean_exec_time, 0.1)) DESC
            LIMIT $4
            "#,
            &[
                &r"^(CREATE|ALTER|DROP|DELETE|INSERT|UPDATE|BEGIN|COMMIT|ROLLBACK|EXPLAIN|ANALYZE|VACUUM|GRANT|REVOKE|SET|SHOW)".to_string(),
                &SARGABLE_SYSTEM_QUERY_FILTER.to_string(),
                &min_calls,
                &max_queries,
            ],
        )
        .await?;

    // Load the existing indexes per table (multi-schema, filtered on the SQL side)
    let existing_indexes = get_existing_indexes(pool, schemas).await?;

    // Schema gate: if schemas are configured, ignore tables out of scope
    let in_scope = |schemaname: &str| -> bool {
        schemas.is_empty() || schemas.iter().any(|s| s == schemaname)
    };

    let mut by_table: HashMap<(String, String), MissingSargableIndex> = HashMap::new();
    let mut column_queries: Vec<(String, String, String)> = Vec::new(); // (schema, table, column)

    for row in &rows {
        let query: String = row.get("query");
        let Some(attributed) = parse_query_for_sargable(&query) else {
            continue;
        };

        for (schemaname, tablename, column, operator) in attributed {
            if !in_scope(&schemaname) {
                continue;
            }
            // Ignore the system catalogs (pg_*)
            if tablename.starts_with("pg_") {
                continue;
            }

            let key = (schemaname.clone(), tablename.clone());

            // Filter out columns already covered by an existing index
            if let Some(indexes) = existing_indexes.get(&key) {
                let covered = indexes
                    .iter()
                    .any(|(_name, indexdef)| is_column_covered_by_index(indexdef, &column));
                if covered {
                    continue;
                }
            }

            let entry = by_table
                .entry(key.clone())
                .or_insert_with(|| MissingSargableIndex {
                    schemaname: key.0.clone(),
                    tablename: key.1.clone(),
                    query_text: truncate_query(&query),
                    call_count: row.get("calls"),
                    mean_exec_time_ms: row.get("mean_exec_time"),
                    min_exec_time_ms: row.get("min_exec_time"),
                    max_exec_time_ms: row.get("max_exec_time"),
                    sargable_columns: Vec::new(),
                });

            if !entry
                .sargable_columns
                .iter()
                .any(|c| c.column_name == column)
            {
                entry.sargable_columns.push(SargableColumn {
                    column_name: column.clone(),
                    column_type: "unknown".to_string(),
                    is_nullable: true,
                    null_frac: 0.0,
                    density_bucket: "default".to_string(),
                    matched_operator: operator,
                    // Resolved after type enrichment (below); conservative
                    // default while the type is unknown.
                    index_method: "btree".to_string(),
                });
                column_queries.push((schemaname, tablename, column));
            }
        }
    }

    // Enrich with the actual types (pg_attribute/format_type) and null_frac (pg_stats)
    let column_meta = fetch_column_metadata(pool, &column_queries).await?;
    for miss in by_table.values_mut() {
        for col in miss.sargable_columns.iter_mut() {
            if let Some(meta) = column_meta.get(&(
                miss.schemaname.clone(),
                miss.tablename.clone(),
                col.column_name.clone(),
            )) {
                col.column_type = meta.data_type.clone();
                col.is_nullable = meta.nullable;
                col.null_frac = meta.null_frac;
                col.density_bucket = map_pg_type_to_bucket(&col.column_type);
                // Resolve the index method now that the real type is known
                // ("btree" for scalar operators; "gin"/"gist" for
                // containment / full-text / overlap operators).
                col.index_method =
                    resolve_index_method(&col.matched_operator, &col.column_type).to_string();
            }
        }
    }

    let mut result: Vec<MissingSargableIndex> = by_table.into_values().collect();
    result.sort_by_key(|m| std::cmp::Reverse(m.call_count));

    debug!(
        "Found {} tables with uncovered SARGable columns",
        result.len()
    );
    Ok(result)
}

/// Cardinality of a table (pg_class) — to estimate the size of an index
pub async fn get_table_cardinality(
    pool: &Pool,
    schemaname: &str,
    tablename: &str,
) -> Result<TableCardinality, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT reltuples::float8, relpages::bigint FROM pg_class c WHERE c.oid = to_regclass(format('%I.%I', $1::text, $2::text))",
            &[&schemaname.to_string(), &tablename.to_string()],
        )
        .await?;
    match row {
        Some(r) => Ok(TableCardinality {
            reltuples: r.get("reltuples"),
            relpages: r.get("relpages"),
        }),
        None => Ok(TableCardinality {
            reltuples: 0.0,
            relpages: 0,
        }),
    }
}

/// Check whether an index already exists (multi-schema)
pub async fn index_exists(
    pool: &Pool,
    schemaname: &str,
    index_name: &str,
) -> Result<bool, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT 1 FROM pg_indexes WHERE schemaname = $1 AND indexname = $2",
            &[&schemaname, &index_name],
        )
        .await?;
    Ok(row.is_some())
}

/// Check that an index EXISTS and is VALID (indisvalid).
/// After an interrupted CREATE/REINDEX CONCURRENTLY, the index may exist
/// as INVALID — it must not be considered operational.
pub async fn index_valid(
    pool: &Pool,
    schemaname: &str,
    index_name: &str,
) -> Result<bool, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            r#"
            SELECT i.indisvalid
            FROM pg_index i
            JOIN pg_class c ON c.oid = i.indexrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'i'
            "#,
            &[&schemaname, &index_name],
        )
        .await?;
    Ok(row.map(|r| r.get::<_, bool>(0)).unwrap_or(false))
}

/// Drop the INVALID indexes (indisvalid = false) bearing this name —
/// leftovers of an interrupted CREATE/REINDEX CONCURRENTLY.
/// Returns the number of indexes dropped.
pub async fn cleanup_invalid_indexes(
    pool: &Pool,
    schemaname: &str,
    index_name: &str,
) -> Result<u64, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let rows = client
        .query(
            r#"
            SELECT i.indexrelid
            FROM pg_index i
            JOIN pg_class c ON c.oid = i.indexrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'i'
              AND NOT i.indisvalid
            "#,
            &[&schemaname, &index_name],
        )
        .await?;

    let mut dropped = 0u64;
    for row in &rows {
        let oid: i32 = row.get(0);
        // pg_drop_index: the internal function of DROP INDEX (works on invalid indexes)
        if client
            .execute("SELECT pg_catalog.pg_drop_index($1)", &[&oid])
            .await?
            > 0
        {
            dropped += 1;
        }
    }
    Ok(dropped)
}

// ─────────────────────────────────────────────
// Real SQL parsing (sqlparser-rs)
// ─────────────────────────────────────────────

/// Parsing result: SARGable columns **attributed to their table**
/// (schema, table, column, operator).
///
/// The fourth element is the operator that matched the predicate
/// ("btree" for classic scalar comparisons, or a GIN/GIST-family operator:
/// "@>", "@@", "&&", "?", "?&", "?|", "@?") — the analyzer uses it to
/// resolve the index method (B-tree / GIN / GIST).
///
/// In case of JOIN, columns are attributed via the aliases (`a.status` →
/// table of alias `a`); a NON qualified column is only accepted if
/// the query references only ONE table (otherwise the attribution would be a
/// false positive — that's the conservative behavior).
type AttributedColumns = Vec<(String, String, String, String)>;

/// Extract the SARGable columns of a query via sqlparser-rs, with
/// table attribution (JOIN aliases included).
///
/// SARGable = column compared directly to a value/literal/placeholder:
/// - **B-tree** patterns: `col = v`, `col > v`, `col IN (v1,v2)`,
///   `col BETWEEN a AND b`, `col IS [NOT] NULL`, `col LIKE 'pref%'`
/// - **GIN/GIST** patterns: `col @> v` (containment), `col @@ v` (full-text /
///   intersects), `col && v` (overlap), `col ? v` / `col ?& v` / `col ?| v`
///   (jsonb/hstore keys), `col @? v` (jsonpath)
///
/// Functions on the column (LOWER(col), CAST(col...)) and `LIKE '%...'`
/// are NOT SARGable. The reverse containment `v <@ col` is NOT collected
/// (its indexable side is the right operand, not the left column).
/// The resolved (schema, table, column, operator) tuples let the analyzer
/// pick the index method (B-tree / GIN / GIST) from the column type.
fn parse_query_for_sargable(query: &str) -> Option<AttributedColumns> {
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let dialect_ref: &dyn sqlparser::dialect::Dialect = &dialect;

    let statements = match sqlparser::parser::Parser::parse_sql(dialect_ref, query) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("SQL parse failed for: {query}: {e}");
            return None;
        }
    };

    let mut out: AttributedColumns = Vec::new();

    for stmt in &statements {
        if let sqlparser::ast::Statement::Query(query_ast) = stmt {
            // CTE: its name is a PHANTOM table — `WITH cte AS (…) SELECT …
            // FROM cte` would attribute columns to `public.cte` and produce a
            // phantom CREATE INDEX proposal (regenerated every cycle).
            // Conservative: queries that use CTEs are out of scope.
            if query_ast
                .with
                .as_ref()
                .is_some_and(|with| !with.cte_tables.is_empty())
            {
                continue;
            }
            if let Some(select) = query_ast.body.as_select() {
                // Collect the tables (alias, schema, table) — first table AND JOINed tables
                let mut tables: Vec<(Option<String>, String, String)> = Vec::new();
                let push_table =
                    |relation: &sqlparser::ast::TableFactor,
                     tables: &mut Vec<(Option<String>, String, String)>| {
                        if let sqlparser::ast::TableFactor::Table { name, alias, .. } = relation {
                            let parts: Vec<String> =
                                name.0.iter().map(|p| p.value.clone()).collect();
                            if let Some((schema, table)) = match parts.len() {
                                1 => Some(("public".to_string(), parts[0].clone())),
                                2 => Some((parts[0].clone(), parts[1].clone())),
                                3 => Some((parts[1].clone(), parts[2].clone())),
                                _ => None,
                            } {
                                let alias = alias.as_ref().map(|a| a.name.value.clone());
                                tables.push((alias, schema, table));
                            }
                        }
                    };
                for from in &select.from {
                    push_table(&from.relation, &mut tables);
                    for join in &from.joins {
                        push_table(&join.relation, &mut tables);
                    }
                }
                if tables.is_empty() {
                    continue; // FROM subquery / function: out of scope
                }

                if let Some(selection) = &select.selection {
                    let mut raw: Vec<(Option<String>, String, String)> = Vec::new(); // (qualifier, column, operator)
                    collect_sargable(selection, &mut raw);

                    for (qualifier, col, op) in raw {
                        let resolved = match &qualifier {
                            // Qualified column: alias or table name
                            Some(q) => tables.iter().find(|(alias, _, t)| {
                                alias.as_deref() == Some(q.as_str()) || t == q
                            }),
                            // Bare column: safe only if the query references ONE table
                            // (otherwise the attribution would be a false positive)
                            None => tables.first().filter(|_| tables.len() == 1),
                        };
                        if let Some((_, s, t)) = resolved {
                            let (s, t) = (s.clone(), t.clone());
                            match out
                                .iter_mut()
                                .find(|(s2, t2, c2, _)| s2 == &s && t2 == &t && c2 == &col)
                            {
                                Some(entry) => {
                                    // Same column matched by a scalar AND a
                                    // GIN/GIST predicate: the GIN/GIST operator
                                    // wins (it is the appropriate access method).
                                    if entry.3 == "btree" && op != "btree" {
                                        entry.3 = op;
                                    }
                                }
                                None => out.push((s, t, col, op)),
                            }
                        }
                        // Unresolved (subquery, function, unknown alias) → ignored
                    }
                }
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Walk the expression tree and collect the SARGable (qualifier, column, operator)
fn collect_sargable(expr: &sqlparser::ast::Expr, out: &mut Vec<(Option<String>, String, String)>) {
    use sqlparser::ast::{BinaryOperator, UnaryOperator};

    match expr {
        // AND / OR: recursive on both operands;
        // simple comparisons: col = v, col > v, col < v, col >= v, col <= v
        sqlparser::ast::Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And | BinaryOperator::Or => {
                collect_sargable(left, out);
                collect_sargable(right, out);
            }
            BinaryOperator::Eq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq => {
                if let Some(qualified) = expr_as_qualified_column(left) {
                    // SARGable only if the right side is not a column
                    if !expr_is_column_ref(right) {
                        push_qualified(out, &qualified, "btree");
                    }
                }
            }
            // GIN/GIST-family operators (containment / full-text / overlap /
            // jsonb key existence):
            //   col @> v  (jsonb/array/range containment)
            //   col @@ v  (tsvector full-text | geometry intersects)
            //   col && v  (array overlap | geometry overlap)
            //   col ? v / col ?& v / col ?| v  (jsonb/hstore key existence)
            //   col @? v  (jsonpath existence)
            // The LEFT side must be a column and the RIGHT side must not be
            // one. NB: `v <@ col` (ArrowAt) is deliberately EXCLUDED — its
            // indexable side is the right operand, not the left column.
            BinaryOperator::AtArrow
            | BinaryOperator::AtAt
            | BinaryOperator::PGOverlap
            | BinaryOperator::Question
            | BinaryOperator::QuestionAnd
            | BinaryOperator::QuestionPipe
            | BinaryOperator::AtQuestion => {
                if let Some(qualified) = expr_as_qualified_column(left) {
                    if !expr_is_column_ref(right) {
                        push_qualified(out, &qualified, gin_gist_op_token(op));
                    }
                }
            }
            _ => {}
        },

        // NOT: non-SARGable (negation) → do not descend
        sqlparser::ast::Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        }
        | sqlparser::ast::Expr::IsNotDistinctFrom(_, _)
        | sqlparser::ast::Expr::IsDistinctFrom(_, _) => {}

        // (expr): recursive
        sqlparser::ast::Expr::Nested(inner) => {
            collect_sargable(inner, out);
        }

        // col IN (v1, v2, ...)
        sqlparser::ast::Expr::InList {
            expr,
            list,
            negated: false,
            ..
        } => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                if list.iter().all(|e| !expr_is_column_ref(e)) {
                    push_qualified(out, &qualified, "IN");
                }
            }
        }

        // col BETWEEN a AND b
        sqlparser::ast::Expr::Between {
            expr,
            negated: false,
            low,
            high,
        } => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                if !expr_is_column_ref(low) && !expr_is_column_ref(high) {
                    push_qualified(out, &qualified, "BETWEEN");
                }
            }
        }

        // col IS [NOT] NULL
        sqlparser::ast::Expr::IsNotNull(expr) | sqlparser::ast::Expr::IsNull(expr) => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                push_qualified(out, &qualified, "IS NULL");
            }
        }

        // col LIKE 'prefix%' (no leading % = SARGable)
        sqlparser::ast::Expr::Like {
            negated: false,
            expr,
            pattern,
            ..
        } => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                if like_pattern_is_prefix(pattern) {
                    push_qualified(out, &qualified, "LIKE");
                }
            }
        }
        sqlparser::ast::Expr::ILike {
            negated: false,
            expr,
            pattern,
            ..
        } => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                if like_pattern_is_prefix(pattern) {
                    push_qualified(out, &qualified, "LIKE");
                }
            }
        }

        _ => {}
    }
}

/// Return (qualifier, column) if the expression is a column reference.
/// - `col` → (None, col)
/// - `alias.col` → (Some(alias), col)
/// - `schema.table.col` → (Some(table), col)
fn expr_as_qualified_column(expr: &sqlparser::ast::Expr) -> Option<(Option<String>, String)> {
    match expr {
        sqlparser::ast::Expr::Identifier(ident) => Some((None, ident.value.clone())),
        sqlparser::ast::Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let col = parts.last()?.value.clone();
            let qualifier = if parts.len() >= 3 {
                Some(parts[parts.len() - 2].value.clone())
            } else {
                Some(parts[0].value.clone())
            };
            Some((qualifier, col))
        }
        _ => None,
    }
}

/// True if the expression references a column (not a literal)
fn expr_is_column_ref(expr: &sqlparser::ast::Expr) -> bool {
    matches!(
        expr,
        sqlparser::ast::Expr::Identifier(_) | sqlparser::ast::Expr::CompoundIdentifier(_)
    )
}

/// Push a (qualifier, column, operator) if it is not already present
fn push_qualified(
    out: &mut Vec<(Option<String>, String, String)>,
    qualified: &(Option<String>, String),
    op: &str,
) {
    if out
        .iter()
        .any(|(q, c, _)| q == &qualified.0 && c == &qualified.1)
    {
        return;
    }
    out.push((qualified.0.clone(), qualified.1.clone(), op.to_string()));
}

/// Map a GIN/GIST-family `BinaryOperator` to its SQL token (used as the
/// `matched_operator` of the column).
fn gin_gist_op_token(op: &sqlparser::ast::BinaryOperator) -> &'static str {
    use sqlparser::ast::BinaryOperator as B;
    match op {
        B::AtArrow => "@>",
        B::AtAt => "@@",
        B::PGOverlap => "&&",
        B::Question => "?",
        B::QuestionAnd => "?&",
        B::QuestionPipe => "?|",
        B::AtQuestion => "@?",
        _ => "btree",
    }
}

/// Resolve the index access method from the matched operator and the
/// column's real type.
///
/// Classic scalar operators (`=`, `>`, `IN`, `BETWEEN`, prefix `LIKE`,
/// `IS [NOT] NULL`) always target a **B-tree**. Containment / full-text /
/// overlap operators target GIN or GIST depending on the TYPE:
///
/// | operator | jsonb / hstore / `text[]` | tsvector | geometry / geography | `int4range` & co. |
/// |----------|---------------------------|----------|----------------------|-------------------|
/// | `@>` `?` `?&` `?|` `@?` `&&` `@@` | **GIN** | **GIN** | **GIST** | **GIST** |
///
/// Unknown types with a GIN/GIST operator default to GIN (jsonb is by far
/// the most common case — documented best-effort).
pub fn resolve_index_method(operator: &str, data_type: &str) -> &'static str {
    let is_gin_gist_op = matches!(operator, "@>" | "@@" | "&&" | "?" | "?&" | "?|" | "@?");
    if !is_gin_gist_op {
        return "btree";
    }
    let t = data_type.to_ascii_lowercase();
    // GIST: geometry / geography (PostGIS) and range types
    if t.contains("geometry") || t.contains("geography") || t.ends_with("range") || t == "range" {
        return "gist";
    }
    // GIN: jsonb, hstore, arrays, tsvector
    if t == "jsonb" || t == "hstore" || t.ends_with("[]") || t == "tsvector" {
        return "gin";
    }
    "gin"
}

/// Check whether a LIKE pattern is a prefix ('abc%') → SARGable
fn like_pattern_is_prefix(pattern: &sqlparser::ast::Expr) -> bool {
    if let sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) = pattern {
        !s.starts_with('%') && !s.starts_with("\\%")
    } else {
        false
    }
}

// ─────────────────────────────────────────────
// Column metadata
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ColumnMeta {
    data_type: String,
    nullable: bool,
    null_frac: f64,
}

/// Fetch actual type, nullability and null_frac for the candidate columns
async fn fetch_column_metadata(
    pool: &Pool,
    triples: &[(String, String, String)],
) -> Result<HashMap<(String, String, String), ColumnMeta>, Box<dyn std::error::Error + Sync + Send>>
{
    let mut map: HashMap<(String, String, String), ColumnMeta> = HashMap::new();
    if triples.is_empty() {
        return Ok(map);
    }

    let client = pool.get().await?;

    // A targeted query per (schema, table) — no more full scan of information_schema
    let mut seen_tables: Vec<(String, String)> = triples
        .iter()
        .map(|(s, t, _)| (s.clone(), t.clone()))
        .collect();
    seen_tables.sort();
    seen_tables.dedup();

    let wanted: std::collections::HashSet<(String, String, String)> =
        triples.iter().cloned().collect();

    for (s, t) in &seen_tables {
        // `format_type(atttypid, atttypmod)` returns the REAL type name
        // ("jsonb", "text[]", "int4range", "geometry") — unlike
        // information_schema.columns.data_type, which reports "ARRAY" for
        // arrays and "USER-DEFINED" for extension types (hstore, geometry).
        // The real name is required to resolve the index method
        // (B-tree / GIN / GIST).
        let rows = client
            .query(
                r#"
                SELECT a.attname AS column_name,
                       format_type(a.atttypid, a.atttypmod) AS data_type,
                       NOT a.attnotnull AS is_nullable
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_attribute a ON a.attrelid = c.oid
                WHERE n.nspname = $1 AND c.relname = $2
                  AND a.attnum > 0 AND NOT a.attisdropped
                "#,
                &[s, t],
            )
            .await?;
        for row in rows {
            let key = (s.clone(), t.clone(), row.get::<_, String>("column_name"));
            if !wanted.contains(&key) {
                continue;
            }
            map.insert(
                key,
                ColumnMeta {
                    data_type: row.get("data_type"),
                    nullable: row.get::<_, bool>("is_nullable"),
                    null_frac: 0.0,
                },
            );
        }
    }

    // pg_stats: null_frac (per schema+table, fixed parameters)
    for (s, t) in &seen_tables {
        let rows = client
            .query(
                "SELECT attname, null_frac::float8 FROM pg_stats WHERE schemaname = $1 AND tablename = $2",
                &[s, t],
            )
            .await?;
        for row in rows {
            let attname: String = row.get("attname");
            let null_frac: f64 = row.get("null_frac");
            if let Some(meta) = map.get_mut(&(s.clone(), t.clone(), attname)) {
                meta.null_frac = null_frac;
            }
        }
    }

    Ok(map)
}

/// Map a PostgreSQL type to a density bucket (config density_by_type).
///
/// Handles BOTH `information_schema` names ("character varying",
/// "timestamp with time zone") and `format_type()` names ("varchar",
/// "timestamptz", "text[]"). Non-B-tree types (jsonb, hstore, tsvector,
/// arrays, geometry, ranges) fall back to the `default` bucket — they are
/// size-estimated with the `gin_size_factor` / `gist_size_factor` scaling
/// applied by the analyzer.
fn map_pg_type_to_bucket(data_type: &str) -> String {
    let t = data_type.to_ascii_lowercase();
    // Strip a size typmod: "character varying(255)" → "character varying"
    let base = t.split('(').next().unwrap_or(t.as_str()).trim();
    // Arrays → no B-tree density bucket (GIN territory)
    if base.ends_with("[]") {
        return "default".to_string();
    }
    match base {
        "integer" | "int" | "smallint" | "int2" => "int4".to_string(),
        "bigint" | "int8" => "int8".to_string(),
        "double precision" | "float8" | "real" | "float4" | "numeric" | "decimal" => {
            "float8".to_string()
        }
        "character varying" | "varchar" | "character" | "bpchar" | "char" => "varchar".to_string(),
        "text" => "text".to_string(),
        "date" => "date".to_string(),
        "timestamptz"
        | "timestamp with time zone"
        | "timestamp"
        | "timestamp without time zone"
        | "timetz"
        | "time"
        | "time with time zone"
        | "time without time zone" => "timestamptz".to_string(),
        "uuid" => "uuid".to_string(),
        "boolean" | "bool" => "bool".to_string(),
        _ => "default".to_string(),
    }
}

/// Truncate a query safely (no splitting of a UTF-8 character)
fn truncate_query(query: &str) -> String {
    const MAX: usize = 200;
    if query.len() <= MAX {
        return query.to_string();
    }
    let mut end = MAX;
    while !query.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &query[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the attributed (schema, table, column, operator) list
    fn cols(query: &str) -> Vec<(String, String, String, String)> {
        parse_query_for_sargable(query).unwrap_or_default()
    }

    fn colset(query: &str) -> std::collections::HashSet<(String, String, String)> {
        cols(query)
            .into_iter()
            .map(|(s, t, c, _op)| (s, t, c))
            .collect()
    }

    /// Returns the operator matched for a column (first occurrence)
    fn operator_of(query: &str, column: &str) -> Option<String> {
        cols(query)
            .into_iter()
            .find(|(_, _, c, _)| c == column)
            .map(|(_, _, _, op)| op)
    }

    #[test]
    fn test_sargable_eq_and_in() {
        let set = colset("SELECT * FROM orders WHERE status = 'open' AND id IN (1,2,3)");
        assert!(set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "status".to_string()
        )));
        assert!(set.contains(&("public".to_string(), "orders".to_string(), "id".to_string())));
    }

    #[test]
    fn test_sargable_between_and_range() {
        let set = colset(
            "SELECT * FROM orders WHERE created_at BETWEEN '2024-01-01' AND '2024-12-31' AND total > 100",
        );
        assert!(set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "created_at".to_string()
        )));
        assert!(set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "total".to_string()
        )));
    }

    #[test]
    fn test_sargable_like_prefix_vs_wildcard() {
        // 'prefix%' is SARGable, '%suffix' is not
        let set = colset("SELECT * FROM users WHERE email LIKE 'a@corp.com' OR name LIKE '%admin'");
        assert!(set.contains(&(
            "public".to_string(),
            "users".to_string(),
            "email".to_string()
        )));
        assert!(!set.contains(&(
            "public".to_string(),
            "users".to_string(),
            "name".to_string()
        )));
    }

    #[test]
    fn test_non_sargable_function_on_column() {
        // LOWER(email) = ... is NOT SARGable (function on the column)
        assert!(cols("SELECT * FROM users WHERE LOWER(email) = 'x@y.z'").is_empty());
    }

    #[test]
    fn test_column_vs_column_not_sargable() {
        // a = b (column vs column) is NOT SARGable
        assert!(cols("SELECT * FROM t WHERE a = b").is_empty());
    }

    #[test]
    fn test_compound_identifier_schema_table() {
        let set = colset("SELECT * FROM myschema.orders WHERE status = 'x'");
        assert!(set.contains(&(
            "myschema".to_string(),
            "orders".to_string(),
            "status".to_string()
        )));
    }

    #[test]
    fn test_invalid_sql_returns_none() {
        assert!(parse_query_for_sargable("SELECT FROM WHERE").is_none());
    }

    #[test]
    fn test_join_alias_resolution() {
        // Each column is attributed to the table of ITS alias
        let set = colset(
            "SELECT * FROM public.orders a JOIN public.users b ON a.user_id = b.id \
             WHERE a.status = 'open' AND b.email = 'x@y.z'",
        );
        assert!(set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "status".to_string()
        )));
        assert!(set.contains(&(
            "public".to_string(),
            "users".to_string(),
            "email".to_string()
        )));
        // a.user_id / b.id: column vs column → non-SARGable
        assert!(!set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "user_id".to_string()
        )));
        assert!(!set.contains(&("public".to_string(), "users".to_string(), "id".to_string())));
    }

    #[test]
    fn test_join_ambiguous_unqualified_column_is_dropped() {
        // Unqualified `id` with 2 tables: uncertain attribution → excluded (no false positive)
        assert!(cols("SELECT * FROM t1 x JOIN t2 y ON x.k = y.k WHERE id = 5").is_empty());
    }

    #[test]
    fn test_join_qualified_by_table_name_not_alias() {
        // Qualification by the TABLE NAME (not the alias) is resolved
        let set = colset(
            "SELECT * FROM public.orders JOIN public.users u ON orders.user_id = u.id \
             WHERE orders.status = 'shipped'",
        );
        assert!(set.contains(&(
            "public".to_string(),
            "orders".to_string(),
            "status".to_string()
        )));
    }

    #[test]
    fn test_cte_name_is_a_phantom_table_not_attributed() {
        // A CTE name is NOT a real table: `FROM cte` must not yield an index
        // proposal on `public.cte` (a phantom, regenerated every cycle).
        // Conservative behavior: CTE queries are skipped entirely.
        assert!(parse_query_for_sargable(
            "WITH cte AS (SELECT id, status FROM orders) SELECT * FROM cte WHERE status = 'open'"
        )
        .is_none());
        assert!(cols(
            "WITH c AS (SELECT * FROM orders), d AS (SELECT * FROM users) \
             SELECT * FROM c, d WHERE c.status = 'open'"
        )
        .is_empty());
    }

    // ── GIN/GIST operator detection ────────────────────────────────

    #[test]
    fn test_gin_gist_operators_are_collected() {
        // jsonb containment
        assert_eq!(
            operator_of(
                "SELECT * FROM events WHERE payload @> '{\"type\":\"login\"}'",
                "payload"
            ),
            Some("@>".to_string())
        );
        // jsonb/hstore key existence
        assert_eq!(
            operator_of("SELECT * FROM events WHERE payload ? 'type'", "payload"),
            Some("?".to_string())
        );
        assert_eq!(
            operator_of("SELECT * FROM events WHERE meta ?& ARRAY['a','b']", "meta"),
            Some("?&".to_string())
        );
        assert_eq!(
            operator_of("SELECT * FROM events WHERE meta ?| ARRAY['a','b']", "meta"),
            Some("?|".to_string())
        );
        // jsonpath existence
        assert_eq!(
            operator_of("SELECT * FROM events WHERE payload @? '$.user'", "payload"),
            Some("@?".to_string())
        );
        // tsvector full-text (RHS is a function, not a column → accepted)
        assert_eq!(
            operator_of(
                "SELECT * FROM posts WHERE search @@ to_tsquery('postgres')",
                "search"
            ),
            Some("@@".to_string())
        );
        // array overlap
        assert_eq!(
            operator_of("SELECT * FROM posts WHERE tags && ARRAY['sql']", "tags"),
            Some("&&".to_string())
        );
        // range containment
        assert_eq!(
            operator_of("SELECT * FROM bookings WHERE period @> 42", "period"),
            Some("@>".to_string())
        );
    }

    #[test]
    fn test_arrowat_reverse_containment_not_indexable_on_left() {
        // `v <@ col` and `col <@ literal`: the indexable side is the array
        // (right operand) — never the left column → no candidate.
        assert!(cols("SELECT * FROM t WHERE 5 <@ period").is_empty());
        assert!(cols("SELECT * FROM t WHERE period <@ ARRAY[1,2,3]").is_empty());
    }

    #[test]
    fn test_scalar_and_gin_gist_same_column_prefers_gin_gist() {
        // Same column in a scalar AND a containment predicate → the
        // GIN/GIST operator wins (it is the appropriate access method).
        assert_eq!(
            operator_of(
                "SELECT * FROM t WHERE payload @> '{}' AND payload = '{}'::jsonb",
                "payload"
            ),
            Some("@>".to_string())
        );
    }

    #[test]
    fn test_resolve_index_method() {
        // GIN
        assert_eq!(resolve_index_method("@>", "jsonb"), "gin");
        assert_eq!(resolve_index_method("?", "hstore"), "gin");
        assert_eq!(resolve_index_method("@@", "tsvector"), "gin");
        assert_eq!(resolve_index_method("&&", "text[]"), "gin");
        assert_eq!(
            resolve_index_method("@>", "character varying(255)[]"),
            "gin"
        );
        // GIST
        assert_eq!(resolve_index_method("@>", "int4range"), "gist");
        assert_eq!(resolve_index_method("@@", "geometry"), "gist");
        assert_eq!(resolve_index_method("&&", "geometry"), "gist");
        assert_eq!(resolve_index_method("@@", "geography"), "gist");
        assert_eq!(resolve_index_method("@>", "timestamptz range"), "gist");
        // B-tree (scalar operators always)
        assert_eq!(resolve_index_method("=", "integer"), "btree");
        assert_eq!(resolve_index_method("IN", "text"), "btree");
        assert_eq!(
            resolve_index_method("LIKE", "character varying(100)"),
            "btree"
        );
        assert_eq!(resolve_index_method("btree", "jsonb"), "btree");
        // GIN/GIST operator + unknown type → best-effort GIN (jsonb is the
        // overwhelmingly common case)
        assert_eq!(resolve_index_method("@>", "unknown"), "gin");
        // Case-insensitive on the type
        assert_eq!(resolve_index_method("@>", "JSONB"), "gin");
    }

    #[test]
    fn test_map_pg_type_to_bucket_handles_format_type_names() {
        // information_schema names (legacy)
        assert_eq!(map_pg_type_to_bucket("character varying"), "varchar");
        assert_eq!(
            map_pg_type_to_bucket("timestamp with time zone"),
            "timestamptz"
        );
        // format_type names (new source)
        assert_eq!(map_pg_type_to_bucket("character varying(255)"), "varchar");
        assert_eq!(map_pg_type_to_bucket("timestamptz"), "timestamptz");
        assert_eq!(map_pg_type_to_bucket("integer"), "int4");
        assert_eq!(map_pg_type_to_bucket("bigint"), "int8");
        // Non-B-tree types → default bucket (GIN/GIST territory)
        assert_eq!(map_pg_type_to_bucket("jsonb"), "default");
        assert_eq!(map_pg_type_to_bucket("text[]"), "default");
        assert_eq!(map_pg_type_to_bucket("int4range"), "default");
        assert_eq!(map_pg_type_to_bucket("geometry"), "default");
    }

    #[test]
    fn test_cardinality_live() {
        // Test against the local PG (pgindexai_demo) — verifies the full path of get_table_cardinality.
        // Skipped cleanly if the demo database is not available.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            let mut c = deadpool_postgres::Config::new();
            c.host = Some("/var/run/postgresql".to_string());
            c.port = Some(5432);
            c.user = Some("postgres".to_string());
            c.dbname = Some("pgindexai_demo".to_string());
            c.create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .ok()
        });
        let Some(pool) = pool else {
            eprintln!("SKIP test_cardinality_live: no local PostgreSQL");
            return;
        };
        let r = rt.block_on(async { get_table_cardinality(&pool, "public", "orders").await });
        match r {
            Ok(t) if t.reltuples > 0.0 => println!("CARDINALITY orders: {t:?}"),
            _ => eprintln!("SKIP test_cardinality_live: no pgindexai_demo.orders data"),
        }
    }

    #[test]
    fn test_sargable_system_query_filter_live() {
        // Regression: the exclusion filter must ACTUALLY match in
        // PostgreSQL's ARE dialect. The previous `\bpg_` version never
        // matched (`\b` is a literal backspace in ARE, not a word
        // boundary) and the daemon's own high-call queries crowded real
        // user queries out of the candidate window.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool = rt.block_on(async {
            let mut c = deadpool_postgres::Config::new();
            c.host = Some("/var/run/postgresql".to_string());
            c.port = Some(5432);
            c.user = Some("postgres".to_string());
            c.dbname = Some("pgindexai_demo".to_string());
            c.create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .ok()
        });
        let Some(pool) = pool else {
            eprintln!("SKIP test_sargable_system_query_filter_live: no local PostgreSQL");
            return;
        };
        let excluded = |query: &str| {
            rt.block_on(async {
                let client = pool.get().await.unwrap();
                client
                    .query_one("SELECT $1 ~* $2", &[&query, &SARGABLE_SYSTEM_QUERY_FILTER])
                    .await
                    .unwrap()
                    .get::<_, bool>(0)
            })
        };
        // Daemon-internal queries → MUST be excluded (multi-line and single-line)
        assert!(excluded(
            "WITH index_info AS (\n SELECT s.schemaname\n FROM pg_stat_user_indexes s\n WHERE s.schemaname = 'public')"
        ));
        assert!(excluded(
            "SELECT table_schema, table_name, column_name, data_type, is_nullable\nFROM information_schema.columns\nWHERE table_schema = ANY($1)"
        ));
        assert!(excluded(
            "SELECT now() - pg_postmaster_start_time() >= make_interval(mins => 1)"
        ));
        // User queries → must NOT be excluded
        assert!(!excluded(
            "SELECT id, customer_id, total_amount FROM orders WHERE shipping_method = $1"
        ));
        // Word boundary: a user table with a `pg`-like name must survive
        assert!(!excluded("SELECT * FROM my_spg_table WHERE x = 1"));
    }

    #[test]
    fn test_truncate_query_multibyte_safe() {
        let long = "中".repeat(300);
        let t = truncate_query(&long);
        assert!(t.len() <= 200 + 3); // 200 + "..."
        assert!(t.ends_with("..."));
        // No panic = cut on a valid character boundary
    }

    // ── bloat_score_from_leaf_density ─────────────────────────────────
    // Anchors below were measured on PostgreSQL 16 / pgstattuple 1.5.

    #[test]
    fn test_bloat_score_healthy_indexes_score_near_zero() {
        // Measured leaf fills of healthy (freshly built) indexes:
        // 89.80, 89.83, 89.92, 90.05 — all must score well below the
        // 50 % proposal threshold.
        for d in [0.8980, 0.8983, 0.8992, 0.9005, 0.95, 1.0] {
            assert!(
                bloat_score_from_leaf_density(d) < 0.02,
                "healthy fill {d} should score < 0.02"
            );
        }
    }

    #[test]
    fn test_bloat_score_worst_case_scores_at_most_one() {
        // 50 % churn + VACUUM anchors: 45.06 % (clean table), and the
        // demo acceptance state: 45.13 % (pkey), 45.87 %, 46.64 %, 51.97 %
        // — all score ~1.0 (the maximum).
        for d in [0.4506, 0.4513, 0.4587, 0.4664, 0.5197] {
            assert!(
                bloat_score_from_leaf_density(d) > 0.84,
                "worst-case fill {d} should score > 0.84"
            );
        }
        // At and below the worst-case anchor → clamped to exactly 1.0
        assert!((bloat_score_from_leaf_density(0.45) - 1.0).abs() < 1e-9);
        assert!((bloat_score_from_leaf_density(0.30) - 1.0).abs() < 1e-9);
        assert!((bloat_score_from_leaf_density(0.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_bloat_score_all_demo_acceptance_values_pass_threshold() {
        // Real densities measured on the demo database in the acceptance
        // scenario (50 % churn → VACUUM → ANALYZE), against the 50 %
        // threshold of sql/pgindexai.test.yaml.
        for d in [0.4513, 0.4587, 0.4664, 0.5197] {
            let score = bloat_score_from_leaf_density(d) * 100.0;
            assert!(score >= 50.0, "density {d} → score {score} must be >= 50");
        }
        // The worst case of the demo (idx_items_order, 51.97 % fill):
        let s = bloat_score_from_leaf_density(0.5197) * 100.0;
        assert!((s - 84.5).abs() < 0.2, "expected ~84.5, got {s}");
    }

    #[test]
    fn test_bloat_score_linear_interpolation() {
        // 67.5 % is exactly the midpoint of [45 %, 90 %] → 0.5
        assert!((bloat_score_from_leaf_density(0.675) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_bloat_score_nan_and_out_of_range_are_safe() {
        // NaN / infinities must never leak into the score (no panic, no NaN)
        assert_eq!(bloat_score_from_leaf_density(f64::NAN), 0.0);
        assert_eq!(bloat_score_from_leaf_density(f64::INFINITY), 0.0);
        assert_eq!(bloat_score_from_leaf_density(f64::NEG_INFINITY), 0.0);
        // Out-of-range inputs are clamped to [0, 1]
        assert!((bloat_score_from_leaf_density(-0.5) - 1.0).abs() < 1e-9);
        assert_eq!(bloat_score_from_leaf_density(1.5), 0.0);
    }
}
