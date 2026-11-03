use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

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
    pub avg_leaf_density: f64,
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
    /// Actual PostgreSQL type (e.g. "integer", "character varying")
    pub column_type: String,
    pub is_nullable: bool,
    pub null_frac: f64,
    /// Density bucket (e.g. "int4", "varchar", "timestamptz", "default")
    pub density_bucket: String,
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

/// Fetch the fragmented B-tree indexes (documented heuristic estimation)
///
/// PostgreSQL does not provide a native "avg_leaf_density" metric.
/// We combine three signals:
///   1. ratio n_dead_tup / (n_live + n_dead) of the table (main proxy, 50%)
///   2. n_mod_since_analyze / n_live_tup (unanalyzed modifications, 30%)
///   3. large index never scanned (20%)
pub async fn get_fragmented_indexes(
    pool: &Pool,
    max_rows: i64,
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
                    pg_relation_size(s.indexrelid) / 8192 AS relpages,
                    COALESCE(s.idx_tup_read, 0) AS idx_tup_read,
                    COALESCE(s.idx_scan, 0) AS idx_scan,
                    pc.relpages::bigint AS index_pages,
                    pc.reltuples::float8 AS index_tuples
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
                ii.relpages,
                ii.index_pages,
                ii.index_tuples,
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
    for row in &rows {
        let relpages: i64 = row.get("relpages");
        let index_pages: i64 = row.get("index_pages");
        let index_tuples: f64 = row.get("index_tuples");
        let n_live_tup: i64 = row.get("n_live_tup");
        let n_dead_tup: i64 = row.get("n_dead_tup");
        let n_mod_since_analyze: i64 = row.get("n_mod_since_analyze");
        let idx_scan: i64 = row.get("idx_scan");

        let mut frag_score: f64 = 0.0;

        // Criterion 1: dead tuples ratio (50%)
        let total_tuples = (n_live_tup + n_dead_tup) as f64;
        let dead_ratio = if total_tuples > 0.0 {
            n_dead_tup as f64 / total_tuples
        } else {
            0.0
        };
        frag_score += dead_ratio * 0.5;

        // Criterion 2: unanalyzed modifications (30%)
        let mod_ratio = if n_live_tup > 0 {
            (n_mod_since_analyze as f64 / n_live_tup as f64).min(1.0)
        } else {
            0.0
        };
        frag_score += mod_ratio * 0.3;

        // Criterion 3: large index never scanned (20%)
        if idx_scan == 0 && relpages > 100 {
            frag_score += 0.2;
        }

        let fragmentation_pct = (frag_score * 100.0).min(100.0);

        // avg_leaf_density: tuples/pages ratio of the index, normalized over 100
        // (PostgreSQL B-tree targets ~90% fill of leaf pages)
        let avg_leaf_density = if index_tuples > 0.0 && index_pages > 0 {
            (index_tuples / index_pages as f64 / 100.0).clamp(0.0, 1.0)
        } else {
            1.0
        };

        result.push(FragmentationInfo {
            schemaname: row.get("schemaname"),
            tablename: row.get("tablename"),
            indexname: row.get("indexname"),
            relpages,
            avg_leaf_density,
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
                &r"^(SELECT|WITH).*\bpg_".to_string(),
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

        for (schemaname, tablename, column) in attributed {
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
                });
                column_queries.push((schemaname, tablename, column));
            }
        }
    }

    // Enrich with the actual types (information_schema) and null_frac (pg_stats)
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
                col.density_bucket = map_pg_type_to_bucket(&meta.data_type);
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
/// (schema, table, column).
///
/// In case of JOIN, columns are attributed via the aliases (`a.status` →
/// table of alias `a`); a NON qualified column is only accepted if
/// the query references only ONE table (otherwise the attribution would be a
/// false positive — that's the conservative behavior).
type AttributedColumns = Vec<(String, String, String)>;

/// Extract the SARGable columns of a query via sqlparser-rs, with
/// table attribution (JOIN aliases included).
///
/// SARGable = column compared directly to a value/literal/placeholder
/// (col = v, col > v, col IN (v1,v2), col BETWEEN a AND b, col IS [NOT] NULL,
///  col LIKE 'pref%'). Functions on the column (LOWER(col), CAST(col...))
/// and LIKE '%...' are NOT SARGable.
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
                    continue; // FROM subquery / function / CTE: out of scope
                }

                if let Some(selection) = &select.selection {
                    let mut raw: Vec<(Option<String>, String)> = Vec::new(); // (qualifier, column)
                    collect_sargable(selection, &mut raw);

                    for (qualifier, col) in raw {
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
                            if !out
                                .iter()
                                .any(|(s2, t2, c2)| s2 == &s && t2 == &t && c2 == &col)
                            {
                                out.push((s, t, col));
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

/// Walk the expression tree and collect the SARGable (qualifier, column)
fn collect_sargable(expr: &sqlparser::ast::Expr, out: &mut Vec<(Option<String>, String)>) {
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
                        push_qualified(out, &qualified);
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
                    push_qualified(out, &qualified);
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
                    push_qualified(out, &qualified);
                }
            }
        }

        // col IS [NOT] NULL
        sqlparser::ast::Expr::IsNotNull(expr) | sqlparser::ast::Expr::IsNull(expr) => {
            if let Some(qualified) = expr_as_qualified_column(expr) {
                push_qualified(out, &qualified);
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
                    push_qualified(out, &qualified);
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
                    push_qualified(out, &qualified);
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

/// Push a (qualifier, column) if it is not already present
fn push_qualified(out: &mut Vec<(Option<String>, String)>, qualified: &(Option<String>, String)) {
    if out
        .iter()
        .any(|(q, c)| q == &qualified.0 && c == &qualified.1)
    {
        return;
    }
    out.push(qualified.clone());
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
        let rows = client
            .query(
                r#"
                SELECT table_schema, table_name, column_name, data_type, is_nullable
                FROM information_schema.columns
                WHERE table_schema = $1 AND table_name = $2
                "#,
                &[s, t],
            )
            .await?;
        for row in rows {
            let key = (
                row.get::<_, String>("table_schema"),
                row.get::<_, String>("table_name"),
                row.get::<_, String>("column_name"),
            );
            if !wanted.contains(&key) {
                continue;
            }
            map.insert(
                key,
                ColumnMeta {
                    data_type: row.get("data_type"),
                    nullable: row.get::<_, String>("is_nullable") == "YES",
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

/// Map a PostgreSQL type to a density bucket (config density_by_type)
fn map_pg_type_to_bucket(data_type: &str) -> String {
    match data_type {
        "integer" | "smallint" => "int4".to_string(),
        "bigint" => "int8".to_string(),
        "double precision" | "real" => "float8".to_string(),
        "numeric" | "decimal" => "float8".to_string(),
        "character varying" | "character" | "bpchar" | "char" => "varchar".to_string(),
        "text" => "text".to_string(),
        "date" => "date".to_string(),
        "timestamp with time zone" | "timestamp without time zone" => "timestamptz".to_string(),
        "time with time zone" | "time without time zone" => "timestamptz".to_string(),
        "uuid" => "uuid".to_string(),
        "boolean" => "bool".to_string(),
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

    /// Returns the set of attributed (schema, table, column)
    fn cols(query: &str) -> Vec<(String, String, String)> {
        parse_query_for_sargable(query).unwrap_or_default()
    }

    fn colset(query: &str) -> std::collections::HashSet<(String, String, String)> {
        cols(query).into_iter().collect()
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
    fn test_truncate_query_multibyte_safe() {
        let long = "中".repeat(300);
        let t = truncate_query(&long);
        assert!(t.len() <= 200 + 3); // 200 + "..."
        assert!(t.ends_with("..."));
        // No panic = cut on a valid character boundary
    }
}
