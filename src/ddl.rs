use deadpool_postgres::Pool;
use std::time::Instant;
use tracing::{error, info, warn};

use crate::db::queries;
use crate::index_log::{CrudOperation, IndexLogWriter, IndexOperationLog};

/// Outcome of a DDL operation:
/// - `Applied`: the DDL was executed
/// - `Skipped`: idempotent no-op (index already present / already absent) —
///   counted separately from "applied" in the cycle reports
///   (previously: no-ops were counted as "applied")
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlOutcome {
    Applied,
    Skipped,
}

/// Execute a DDL with an EXPLICIT TRANSACTION and post-commit validation.
///
/// PostgreSQL rule: `CREATE INDEX CONCURRENTLY` and `REINDEX CONCURRENTLY`
/// CANNOT be executed inside a transaction block.
/// - Atomic DDL (DROP INDEX) → BEGIN … COMMIT, ROLLBACK on error
/// - CONCURRENTLY DDL → direct execution (auto-commit), never inside a transaction
///
/// `timeout_secs` bounds the duration of CONCURRENTLY builds (0 = unlimited):
/// without a bound, a build on a large table can block the entire
/// analysis cycle (the daemon is sequential). NB: a too-short timeout on
/// a large table would create a build→cancelled→retry loop — hence the
/// generous default of 30 min (configurable via `thresholds.ddl_timeout_seconds`).
pub async fn run_in_transaction(
    pool: &Pool,
    ddl: &str,
    timeout_secs: u64,
) -> Result<u64, Box<dyn std::error::Error + Sync + Send>> {
    if is_concurrent_ddl(ddl) {
        // CONCURRENTLY: forbidden in a transaction, direct execution
        let client = pool.get().await?;
        // Per-session timeout (not per-statement): SET before, RESET after —
        // the connection returns to the pool in its initial state.
        if timeout_secs > 0 {
            client
                .simple_query(&format!("SET statement_timeout = {timeout_secs}s"))
                .await?;
        }
        let result = client.execute(ddl, &[]).await;
        if timeout_secs > 0 {
            // Even in case of error: reset before propagating (clean pool)
            let _ = client.simple_query("SET statement_timeout = 0").await;
        }
        let rows = result?;
        Ok(rows)
    } else {
        let mut client = pool.get().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| format!("BEGIN failed: {}", e))?;
        let rows = tx
            .execute(ddl, &[])
            .await
            .map_err(|e| format!("DDL failed in transaction: {}", e))?;
        tx.commit()
            .await
            .map_err(|e| format!("COMMIT failed: {}", e))?;
        Ok(rows)
    }
}

/// Detect a CONCURRENTLY DDL (CREATE INDEX CONCURRENTLY / REINDEX … CONCURRENTLY)
fn is_concurrent_ddl(ddl: &str) -> bool {
    ddl.to_uppercase().contains("CONCURRENTLY")
}

/// Execute a CREATE INDEX (CONCURRENTLY, outside a transaction) with:
/// - existence pre-check (idempotence)
/// - post-commit validation of VALIDITY (indisvalid) — an interrupted
///   CONCURRENTLY build leaves an INVALID index that must be cleaned up
/// - structured logging
pub async fn execute_create_index(
    pool: &Pool,
    ddl: &str,
    timeout_secs: u64,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    reason: Option<String>,
) -> Result<DdlOutcome, Box<dyn std::error::Error + Sync + Send>> {
    let index_name = extract_index_name(ddl);
    let schema = extract_schema_from_ddl(ddl);
    if let Some(ref name) = index_name {
        if queries::index_exists(pool, &schema, name).await? {
            // Idempotent no-op: logged as "skipped" (no longer "success")
            info!("Index {name} already exists — CREATE skipped (idempotent no-op)");
            writer.log(
                IndexOperationLog::new(
                    CrudOperation::CreateIndex,
                    schema.clone(),
                    name.clone(),
                    std::time::Duration::ZERO,
                    "skipped (index already exists)",
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    0,
                    0.0,
                    reason
                        .clone()
                        .unwrap_or_else(|| "missing index suggestion".to_string()),
                ),
            );
            return Ok(DdlOutcome::Skipped);
        }
    }

    info!("Executing CREATE INDEX: {ddl}");

    let object = index_name.clone().unwrap_or_else(|| "unknown".to_string());
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl, timeout_secs).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            // Post-commit validation: the index must EXIST and be VALID
            if let Some(ref name) = index_name {
                if !queries::index_valid(pool, &schema, name).await? {
                    let cleaned = queries::cleanup_invalid_indexes(pool, &schema, name)
                        .await
                        .unwrap_or(0);
                    let err = format!(
                        "CREATE INDEX CONCURRENTLY failed: index {}.{name} absent or INVALID ({} invalid index cleaned up)",
                        schema, cleaned
                    );
                    writer.log(
                        IndexOperationLog::new(
                            CrudOperation::CreateIndex,
                            schema.clone(),
                            object.clone(),
                            duration,
                            format!("validation_error: {err}"),
                        )
                        .with_ddl(ddl.to_string())
                        .with_metadata(
                            confidence.unwrap_or(0.0),
                            0,
                            0.0,
                            reason
                                .clone()
                                .unwrap_or_else(|| "missing index suggestion".to_string()),
                        ),
                    );
                    return Err(err.into());
                }
            }

            info!(
                "CREATE INDEX successful ({} rows affected) in {:.2}ms",
                rows,
                duration.as_secs_f64() * 1000.0
            );

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::CreateIndex,
                    schema.clone(),
                    object.clone(),
                    duration,
                    "success",
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    0, // Size unknown before creation
                    0.0,
                    reason
                        .clone()
                        .unwrap_or_else(|| "missing index suggestion".to_string()),
                ),
            );

            Ok(DdlOutcome::Applied)
        }
        Err(e) => {
            error!("CREATE INDEX failed: {e}");

            // Cleanup of any residual invalid index (interrupted CONCURRENTLY build)
            if let Some(ref name) = index_name {
                let cleaned = queries::cleanup_invalid_indexes(pool, &schema, name)
                    .await
                    .unwrap_or(0);
                if cleaned > 0 {
                    warn!(
                        "Cleaned up {cleaned} residual invalid index(es) for {}.{name}",
                        schema
                    );
                }
            }

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::CreateIndex,
                    schema,
                    object,
                    duration,
                    format!("error: {e}"),
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    0,
                    0.0,
                    reason.unwrap_or_else(|| "missing index suggestion".to_string()),
                ),
            );

            Err(e)
        }
    }
}

/// Execute a DROP INDEX with explicit transaction and structured logging.
///
/// Idempotence: if the index is already absent, returns `DdlOutcome::Skipped`
/// (logged as "skipped") instead of counting a no-op as "applied".
pub async fn execute_drop_index(
    pool: &Pool,
    ddl: &str,
    timeout_secs: u64,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    size_bytes: u64,
    reason: Option<String>,
) -> Result<DdlOutcome, Box<dyn std::error::Error + Sync + Send>> {
    let index_name = extract_index_name(ddl);
    let schema = extract_schema_from_ddl(ddl);
    if let Some(ref name) = index_name {
        if !queries::index_exists(pool, &schema, name).await? {
            info!("Index {name} already absent — DROP skipped (idempotent no-op)");
            writer.log(
                IndexOperationLog::new(
                    CrudOperation::DropIndex,
                    schema.clone(),
                    name.clone(),
                    std::time::Duration::ZERO,
                    "skipped (index already absent)",
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    0,
                    0.0,
                    reason.clone().unwrap_or_else(|| "unused index".to_string()),
                ),
            );
            return Ok(DdlOutcome::Skipped);
        }
    }

    info!("Executing DROP INDEX (tx): {ddl}");

    let object = index_name.clone().unwrap_or_else(|| "unknown".to_string());
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl, timeout_secs).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            info!(
                "DROP INDEX successful ({} rows affected) in {:.2}ms",
                rows,
                duration.as_secs_f64() * 1000.0
            );
            // NB: `rows` is 0 for a DDL (CommandComplete without count) —
            // the absence of the index is detected by the pre-check above, not here.

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::DropIndex,
                    schema.clone(),
                    object.clone(),
                    duration,
                    "success",
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    size_bytes,
                    0.0,
                    reason.clone().unwrap_or_else(|| "unused index".to_string()),
                ),
            );

            Ok(DdlOutcome::Applied)
        }
        Err(e) => {
            error!("DROP INDEX failed (rolled back): {e}");

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::DropIndex,
                    schema,
                    object,
                    duration,
                    format!("error: {e}"),
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    size_bytes,
                    0.0,
                    reason.unwrap_or_else(|| "unused index".to_string()),
                ),
            );

            Err(e)
        }
    }
}

/// Execute a REINDEX (CONCURRENTLY → auto-commit; standard → transaction)
/// with post-operation validation and structured logging
#[allow(clippy::too_many_arguments)] // flat signature uniform with create/drop
pub async fn execute_reindex(
    pool: &Pool,
    ddl: &str,
    timeout_secs: u64,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    size_bytes: u64,
    fragmentation: f64,
    reason: Option<String>,
) -> Result<DdlOutcome, Box<dyn std::error::Error + Sync + Send>> {
    if is_concurrent_ddl(ddl) {
        warn!("REINDEX CONCURRENTLY cannot run in a transaction; executing with auto-commit.");
    }
    info!("Executing REINDEX: {ddl}");

    let schema = extract_schema_from_ddl(ddl);
    let object = extract_index_name(ddl).unwrap_or_else(|| "unknown".to_string());
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl, timeout_secs).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            // Interrupted REINDEX CONCURRENTLY: the original valid index survives,
            // but an INVALID index may remain → cleanup + failure
            if is_concurrent_ddl(ddl) {
                let cleaned = queries::cleanup_invalid_indexes(pool, &schema, &object)
                    .await
                    .unwrap_or(0);
                if cleaned > 0 {
                    let err = format!(
                        "REINDEX CONCURRENTLY failed mid-way: {cleaned} invalid index(es) cleaned up, the original index is intact"
                    );
                    writer.log(
                        IndexOperationLog::new(
                            CrudOperation::Reindex,
                            schema.clone(),
                            object.clone(),
                            duration,
                            format!("validation_error: {err}"),
                        )
                        .with_ddl(ddl.to_string())
                        .with_metadata(
                            confidence.unwrap_or(0.0),
                            size_bytes,
                            fragmentation,
                            reason
                                .clone()
                                .unwrap_or_else(|| format!("fragmentation {:.1}%", fragmentation)),
                        ),
                    );
                    return Err(err.into());
                }
                if !queries::index_valid(pool, &schema, &object).await? {
                    let err =
                        format!("REINDEX finished but index {schema}.{object} is no longer valid");
                    writer.log(
                        IndexOperationLog::new(
                            CrudOperation::Reindex,
                            schema,
                            object,
                            duration,
                            format!("validation_error: {err}"),
                        )
                        .with_ddl(ddl.to_string())
                        .with_metadata(
                            confidence.unwrap_or(0.0),
                            size_bytes,
                            fragmentation,
                            reason
                                .unwrap_or_else(|| format!("fragmentation {:.1}%", fragmentation)),
                        ),
                    );
                    return Err(err.into());
                }
            }

            info!(
                "REINDEX successful ({} rows affected) in {:.2}ms",
                rows,
                duration.as_secs_f64() * 1000.0
            );

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::Reindex,
                    schema.clone(),
                    object.clone(),
                    duration,
                    "success",
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    size_bytes,
                    fragmentation,
                    reason
                        .clone()
                        .unwrap_or_else(|| format!("fragmentation {:.1}%", fragmentation)),
                ),
            );

            Ok(DdlOutcome::Applied)
        }
        Err(e) => {
            error!("REINDEX failed (rolled back): {e}");

            // A failed REINDEX CONCURRENTLY may leave an invalid index
            let cleaned = queries::cleanup_invalid_indexes(pool, &schema, &object)
                .await
                .unwrap_or(0);
            if cleaned > 0 {
                warn!("Cleaned up {cleaned} residual invalid index(es) for {schema}.{object}");
            }

            writer.log(
                IndexOperationLog::new(
                    CrudOperation::Reindex,
                    schema,
                    object,
                    duration,
                    format!("error: {e}"),
                )
                .with_ddl(ddl.to_string())
                .with_metadata(
                    confidence.unwrap_or(0.0),
                    size_bytes,
                    fragmentation,
                    reason.unwrap_or_else(|| format!("fragmentation {:.1}%", fragmentation)),
                ),
            );

            Err(e)
        }
    }
}

/// Build the index creation DDL (CONCURRENTLY, non-blocking),
/// with or without a filter predicate (filtered index `WHERE col IS NOT NULL`).
///
/// Used at PROPOSAL GENERATION and at its APPLICATION: the choice
/// of LAYA (standard vs filtered) determines the variant executed — not the
/// variant simply generated at analysis (both paths must
/// produce strictly identical DDL).
pub fn build_create_index_ddl(
    schema: &str,
    index_name: &str,
    table: &str,
    column: &str,
    filtered: bool,
) -> String {
    let base = format!(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {schema}.{index_name} ON {schema}.{table} ({column})"
    );
    if filtered {
        format!("{base} WHERE {column} IS NOT NULL")
    } else {
        base
    }
}

/// Extract the index name from a DDL (CREATE/DROP/REINDEX)
/// Handles: CREATE INDEX [CONCURRENTLY] [IF NOT EXISTS] [schema.]name ON …,
///          DROP INDEX [IF EXISTS] [schema.]name,
///          REINDEX INDEX [CONCURRENTLY] [schema.]name
pub fn extract_index_name(ddl: &str) -> Option<String> {
    let trimmed = ddl.trim();
    let upper = trimmed.to_uppercase();

    /// Last segment after the last '.' (handles schema-qualified names)
    fn last_segment(s: &str) -> String {
        s.rsplit('.')
            .next()
            .map(|s| s.to_string())
            .unwrap_or_else(|| s.to_string())
    }

    // CREATE INDEX [UNIQUE] [CONCURRENTLY] [IF NOT EXISTS] <name> ON …
    if upper.starts_with("CREATE") {
        let after_create = &trimmed[6..];
        let after_create_upper = after_create.to_uppercase();
        if let Some(on_pos) = after_create_upper.find(" ON ") {
            let before_on = &after_create[..on_pos];
            // The last word before ON that is not a keyword = the name
            let words: Vec<&str> = before_on
                .split_whitespace()
                .filter(|w| {
                    let w_upper = w.to_uppercase();
                    !matches!(
                        w_upper.as_str(),
                        "INDEX" | "UNIQUE" | "CONCURRENTLY" | "IF" | "NOT" | "EXISTS"
                    )
                })
                .collect();
            if let Some(name) = words.last() {
                return Some(last_segment(name));
            }
        }
    }

    // DROP INDEX [IF EXISTS] [schema.]<name>
    if upper.starts_with("DROP INDEX") {
        let after = &trimmed[10..];
        let after_trimmed = after.trim();
        let after_upper = after_trimmed.to_uppercase();
        // Skip "IF EXISTS"
        let skip = if after_upper.starts_with("IF") { 2 } else { 0 };
        let name = after_trimmed.split_whitespace().nth(skip)?;
        return Some(last_segment(name));
    }

    // REINDEX [INDEX [CONCURRENTLY]] [schema.]<name>
    if upper.starts_with("REINDEX") {
        let after = &trimmed[7..];
        let after_trimmed = after.trim();
        let after_upper = after_trimmed.to_uppercase();
        if after_upper.starts_with("INDEX") {
            let after_idx = &after_trimmed[5..];
            let after_idx_trimmed = after_idx.trim();
            let after_idx_upper = after_idx_trimmed.to_uppercase();
            if after_idx_upper.starts_with("CONCURRENTLY") {
                let after_conc = &after_idx_trimmed[12..];
                let name = after_conc.split_whitespace().next()?;
                return Some(last_segment(name));
            } else {
                let name = after_idx_trimmed.split_whitespace().next()?;
                return Some(last_segment(name));
            }
        }
    }

    None
}

/// Extract the schema from a DDL (CREATE … ON schema.t / DROP INDEX [IF EXISTS] schema.idx /
/// REINDEX INDEX [CONCURRENTLY] schema.idx). Default: "public".
pub fn extract_schema_from_ddl(ddl: &str) -> String {
    let upper = ddl.to_uppercase();

    /// If `token` is of the form `schema.name`, returns `schema`.
    fn schema_from(token: &str) -> Option<String> {
        let pos = token.find('.')?;
        let s = token[..pos].trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    // CREATE … ON schema.table
    if let Some(on_pos) = upper.find(" ON ") {
        if let Some(first) = ddl[on_pos + 4..].split_whitespace().next() {
            if let Some(s) = schema_from(first) {
                return s;
            }
        }
    }
    // DROP INDEX [IF EXISTS] schema.idx
    if let Some(pos) = upper.find("EXISTS ") {
        if let Some(first) = ddl[pos + 7..].split_whitespace().next() {
            if let Some(s) = schema_from(first) {
                return s;
            }
        }
    }
    // REINDEX INDEX [CONCURRENTLY] schema.idx
    if upper.starts_with("REINDEX") {
        for t in ddl[7..].split_whitespace() {
            let tu = t.to_uppercase();
            if tu == "INDEX" || tu == "CONCURRENTLY" {
                continue;
            }
            if let Some(s) = schema_from(t) {
                return s;
            }
            break;
        }
    }
    "public".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_index_name_create() {
        let ddl = "CREATE INDEX idx_test ON public.orders (status)";
        assert_eq!(extract_index_name(ddl), Some("idx_test".to_string()));
    }

    #[test]
    fn test_extract_index_name_create_unique() {
        let ddl = "CREATE UNIQUE INDEX idx_unique ON public.orders (sku)";
        assert_eq!(extract_index_name(ddl), Some("idx_unique".to_string()));
    }

    #[test]
    fn test_extract_index_name_create_concurrent() {
        let ddl = "CREATE INDEX CONCURRENTLY idx_conc ON public.orders (status)";
        assert_eq!(extract_index_name(ddl), Some("idx_conc".to_string()));
    }

    #[test]
    fn test_extract_index_name_create_concurrent_if_not_exists() {
        let ddl =
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS myschema.idx_conc ON myschema.orders (status)";
        assert_eq!(extract_index_name(ddl), Some("idx_conc".to_string()));
    }

    #[test]
    fn test_extract_index_name_drop() {
        let ddl = "DROP INDEX IF EXISTS public.idx_test";
        assert_eq!(extract_index_name(ddl), Some("idx_test".to_string()));
    }

    #[test]
    fn test_extract_index_name_drop_no_if() {
        let ddl = "DROP INDEX public.idx_test";
        assert_eq!(extract_index_name(ddl), Some("idx_test".to_string()));
    }

    #[test]
    fn test_extract_index_name_reindex() {
        let ddl = "REINDEX INDEX public.idx_frag";
        assert_eq!(extract_index_name(ddl), Some("idx_frag".to_string()));
    }

    #[test]
    fn test_extract_index_name_reindex_concurrent() {
        let ddl = "REINDEX INDEX CONCURRENTLY idx_frag";
        assert_eq!(extract_index_name(ddl), Some("idx_frag".to_string()));
    }

    #[test]
    fn test_extract_index_name_reindex_concurrent_qualified() {
        let ddl = "REINDEX INDEX CONCURRENTLY myschema.idx_frag";
        assert_eq!(extract_index_name(ddl), Some("idx_frag".to_string()));
    }

    #[test]
    fn test_extract_schema_from_ddl() {
        let ddl1 = "CREATE INDEX idx_test ON public.orders (status)";
        assert_eq!(extract_schema_from_ddl(ddl1), "public");

        let ddl2 = "CREATE INDEX idx_test ON myschema.orders (status)";
        assert_eq!(extract_schema_from_ddl(ddl2), "myschema");

        let ddl3 = "REINDEX INDEX idx_frag";
        assert_eq!(extract_schema_from_ddl(ddl3), "public");

        let ddl4 = "REINDEX INDEX myschema.idx_frag";
        assert_eq!(extract_schema_from_ddl(ddl4), "myschema");

        let ddl5 = "DROP INDEX IF EXISTS billing.idx_orders_date";
        assert_eq!(extract_schema_from_ddl(ddl5), "billing");

        let ddl6 = "REINDEX INDEX CONCURRENTLY analytics.idx_events_ts";
        assert_eq!(extract_schema_from_ddl(ddl6), "analytics");
    }

    #[test]
    fn test_is_concurrent_ddl() {
        assert!(is_concurrent_ddl("CREATE INDEX CONCURRENTLY x ON t (c)"));
        assert!(is_concurrent_ddl("REINDEX INDEX CONCURRENTLY x"));
        assert!(!is_concurrent_ddl("CREATE INDEX x ON t (c)"));
        assert!(!is_concurrent_ddl("DROP INDEX x"));
        assert!(!is_concurrent_ddl("REINDEX INDEX x"));
    }

    #[test]
    fn test_build_create_index_ddl_standard() {
        let ddl = build_create_index_ddl("public", "idx_orders_status", "orders", "status", false);
        assert_eq!(
            ddl,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS public.idx_orders_status ON public.orders (status)"
        );
    }

    #[test]
    fn test_build_create_index_ddl_filtered() {
        let ddl = build_create_index_ddl("public", "idx_orders_status", "orders", "status", true);
        assert_eq!(
            ddl,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS public.idx_orders_status ON public.orders (status) WHERE status IS NOT NULL"
        );
    }

    #[test]
    fn test_build_create_index_ddl_roundtrip_extraction() {
        // The DDL produced by the builder remain extractable (name + schema)
        for filtered in [false, true] {
            let ddl = build_create_index_ddl("billing", "idx_events_ts", "events", "ts", filtered);
            assert_eq!(extract_index_name(&ddl), Some("idx_events_ts".to_string()));
            assert_eq!(extract_schema_from_ddl(&ddl), "billing");
            if filtered {
                assert!(is_concurrent_ddl(&ddl));
            }
        }
    }
}
