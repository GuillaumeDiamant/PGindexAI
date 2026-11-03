use deadpool_postgres::Pool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::db::queries;
use crate::ddl;
use crate::index_log::IndexLogWriter;
use crate::laya::{DecisionAction, LayaClient, Proposal};
use crate::naming;
use crate::state::DaemonState;

/// Length limit for identifiers copied into a `reason` (Laya payload)
const SANITIZED_MAX_LEN: usize = 64;

/// Result of a proposal application pass: the DDL actually
/// executed (`applied`) is distinguished from idempotent no-ops (`skipped`:
/// index already present / already absent). Previously, no-ops were
/// counted as "applied" — the two are now reported separately.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    /// Proposals whose DDL was executed
    pub applied: Vec<Proposal>,
    /// Proposals with no effect (object already present / already absent)
    pub skipped: Vec<Proposal>,
}

pub struct Analyzer {
    pool: Pool,
    laya: LayaClient,
    config: Config,
    daemon_state: DaemonState,
    /// Shared (synchronous log, short-lived mutex) — never locked
    /// across an `await` by the caller.
    log_writer: Arc<IndexLogWriter>,
}

impl Analyzer {
    pub fn new(pool: Pool, laya: LayaClient, config: Config, daemon_state: DaemonState) -> Self {
        let il = &config.index_logging;
        let log_writer = Arc::new(IndexLogWriter::new(
            il.enabled,
            &il.verbosity,
            &il.log_file,
            il.max_file_size_bytes,
            il.max_files,
        ));
        Self {
            pool,
            laya,
            config,
            daemon_state,
            log_writer,
        }
    }

    /// Proposal budget per cycle (config default: 20)
    fn cap(&self) -> u32 {
        self.config.thresholds.max_proposals_per_cycle
    }

    /// Run a full analysis cycle
    pub async fn analyze_cycle(
        &self,
    ) -> Result<Vec<Proposal>, Box<dyn std::error::Error + Sync + Send>> {
        let mut proposals = Vec::new();

        // 1. Unused index analysis (observation window required)
        if self.config.actions.drop_unused {
            proposals.extend(self.analyze_unused().await?);
        }

        // 2. Fragmentation analysis
        if self.config.actions.rebuild_fragmented {
            proposals.extend(self.analyze_fragmentation().await?);
        }

        // 3. Missing SARGable index detection
        //    → DISABLED in degraded state (pg_stat_statements missing)
        if self.daemon_state.is_degraded() {
            warn!("DEGRADED mode: missing-index detection is DISABLED (pg_stat_statements unavailable)");
        } else {
            proposals.extend(self.analyze_missing_sargable().await?);
        }

        // Total budget per cycle (each detector is also bounded in SQL)
        let cap = self.cap();
        if proposals.len() > cap as usize {
            warn!(
                "Proposals cap reached: {} > {} — {} proposal(s) dropped this cycle (will be re-detected next)",
                proposals.len(),
                cap,
                proposals.len() - cap as usize
            );
            proposals.truncate(cap as usize);
        }

        debug!(
            "Analysis cycle complete: {} proposals generated",
            proposals.len()
        );
        Ok(proposals)
    }

    /// Check that a catalog identifier is safe to inject into DDL /
    /// a Laya payload. Otherwise: warning + exclusion (no blind execution).
    fn guard_identifier(&self, kind: &str, name: &str) -> bool {
        if naming::is_simple_identifier(name) {
            true
        } else {
            warn!("Identifier '{name}' ({kind}) does not match [a-z_][a-z0-9_]* — excluded from the management scope");
            false
        }
    }

    /// Analyze unused indexes
    async fn analyze_unused(
        &self,
    ) -> Result<Vec<Proposal>, Box<dyn std::error::Error + Sync + Send>> {
        let threshold_minutes = self.config.thresholds.unused_minutes;

        // Observation window: counters are reset on restart.
        // If the server has been up for less than `unused_minutes`, we don't judge.
        if !queries::observation_window_ok(&self.pool, threshold_minutes).await? {
            debug!(
                "Server uptime < {}min (unused threshold): skipping unused-index analysis this cycle",
                threshold_minutes
            );
            return Ok(Vec::new());
        }

        let unused = queries::get_unused_indexes(&self.pool, self.cap() as i64).await?;
        let mut proposals = Vec::new();

        for idx in unused {
            if !(self.guard_identifier("schema", &idx.schemaname)
                && self.guard_identifier("table", &idx.tablename)
                && self.guard_identifier("index", &idx.indexname))
            {
                continue;
            }

            let reason = format!(
                "Index {} on {}.{} never scanned since {}. Size: {} bytes. Observed unused for at least {}min.",
                idx.indexname,
                idx.schemaname,
                idx.tablename,
                idx.installed_at.to_rfc3339(),
                idx.size_bytes,
                threshold_minutes
            );

            let ddl = format!("DROP INDEX IF EXISTS {}.{}", idx.schemaname, idx.indexname);

            proposals.push(Proposal {
                action: DecisionAction::DropIndex,
                ddl,
                reason,
                confidence: 0.0, // Will be updated by Laya
                table: idx.tablename,
                schema: idx.schemaname,
                index_name: Some(idx.indexname),
                column: None,
            });
        }

        Ok(proposals)
    }

    /// Analyze index fragmentation (online rebuild: CONCURRENTLY)
    async fn analyze_fragmentation(
        &self,
    ) -> Result<Vec<Proposal>, Box<dyn std::error::Error + Sync + Send>> {
        let fragmented = queries::get_fragmented_indexes(&self.pool, self.cap() as i64).await?;
        let mut proposals = Vec::new();
        let threshold_pct = self.config.thresholds.fragmentation_pct;

        for idx in fragmented {
            if idx.fragmentation_pct >= threshold_pct as f64 {
                if !(self.guard_identifier("schema", &idx.schemaname)
                    && self.guard_identifier("table", &idx.tablename)
                    && self.guard_identifier("index", &idx.indexname))
                {
                    continue;
                }

                let reason = format!(
                    "Index {}.{} fragmentation at {:.1}% (threshold: {}%). Pages: {}, avg leaf density: {:.2}.",
                    idx.schemaname,
                    idx.indexname,
                    idx.fragmentation_pct,
                    threshold_pct,
                    idx.relpages,
                    idx.avg_leaf_density
                );

                let ddl = format!(
                    "REINDEX INDEX CONCURRENTLY {}.{}",
                    idx.schemaname, idx.indexname
                );

                proposals.push(Proposal {
                    action: DecisionAction::RebuildIndex,
                    ddl,
                    reason,
                    confidence: 0.0,
                    table: idx.tablename,
                    schema: idx.schemaname,
                    index_name: Some(idx.indexname),
                    column: None,
                });
            }
        }

        Ok(proposals)
    }

    /// Detect SARGable queries lacking an index
    async fn analyze_missing_sargable(
        &self,
    ) -> Result<Vec<Proposal>, Box<dyn std::error::Error + Sync + Send>> {
        let schemas = &self.config.analysis.schemas;
        let min_calls = self.config.thresholds.min_sargable_rows;
        let missing = queries::get_missing_sargable_indexes(
            &self.pool,
            schemas,
            min_calls as i64,
            self.cap() as i64,
        )
        .await?;
        let mut proposals = Vec::new();

        for miss in missing {
            // Ignore system catalogs (pg_*): we never index monitoring views
            if miss.tablename.starts_with("pg_") {
                debug!("Skipping system catalog table: {}", miss.tablename);
                continue;
            }

            // Table cardinality to estimate the size of the future index
            let cardinality =
                queries::get_table_cardinality(&self.pool, &miss.schemaname, &miss.tablename)
                    .await
                    .unwrap_or(crate::db::queries::TableCardinality {
                        reltuples: 0.0,
                        relpages: 0,
                    });

            for col in &miss.sargable_columns {
                if !(self.guard_identifier("schema", &miss.schemaname)
                    && self.guard_identifier("table", &miss.tablename)
                    && self.guard_identifier("column", &col.column_name))
                {
                    continue;
                }

                // Explicit naming convention + length validation
                let idx_name = match naming::generate_index_name(
                    &miss.schemaname,
                    &miss.tablename,
                    &col.column_name,
                ) {
                    Ok(name) => name,
                    Err(e) => {
                        warn!(
                            "Cannot generate index name for {}.{}: {}",
                            miss.schemaname, miss.tablename, e
                        );
                        continue;
                    }
                };

                // Target density per column type (config) → size estimation
                let density = self.density_for_bucket(&col.density_bucket);
                let est_pages = if density > 0.0 && cardinality.reltuples > 0.0 {
                    (cardinality.reltuples / density).ceil() as i64
                } else {
                    0
                };
                let est_bytes = est_pages.saturating_mul(8192);

                let reason = format!(
                    "SARGable query on {}.{}: {} (type: {}, {} calls, {:.1}ms avg, null_frac: {:.2}). \
                     No covering index. Target density for {}: {:.0} tuples/page, estimated index size: {} bytes.",
                    miss.schemaname,
                    miss.tablename,
                    col.column_name,
                    sanitize_reason_field(&col.column_type),
                    miss.call_count,
                    miss.mean_exec_time_ms,
                    col.null_frac,
                    col.density_bucket,
                    density,
                    est_bytes
                );

                // Filtered index if the column is nullable by > 10%
                // (default proposed variant — LAYA is free to choose
                // the other variant at apply time, see apply_proposals)
                let filtered = col.is_nullable && col.null_frac > 0.1;

                // Online DDL: CREATE INDEX CONCURRENTLY (does not block writes)
                let ddl = ddl::build_create_index_ddl(
                    &miss.schemaname,
                    &idx_name,
                    &miss.tablename,
                    &col.column_name,
                    filtered,
                );

                let action = if filtered {
                    DecisionAction::CreateFilteredIndex
                } else {
                    DecisionAction::CreateIndex
                };

                proposals.push(Proposal {
                    action,
                    ddl,
                    reason,
                    confidence: 0.0,
                    table: miss.tablename.clone(),
                    schema: miss.schemaname.clone(),
                    index_name: Some(idx_name),
                    column: Some(col.column_name.clone()),
                });
            }
        }

        Ok(proposals)
    }

    /// Get the target density for a bucket from the config
    fn density_for_bucket(&self, bucket: &str) -> f64 {
        let d = &self.config.density_by_type;
        match bucket {
            "int4" => d.int4,
            "int8" => d.int8,
            "float8" => d.float8,
            "varchar" => d.varchar,
            "text" => d.text,
            "date" => d.date,
            "timestamptz" => d.timestamptz,
            "uuid" => d.uuid,
            "bool" => d.bool,
            _ => d.default_density,
        }
    }

    /// Send the proposals to Laya and apply those with sufficient confidence.
    ///
    /// "No fallback" policy: if Laya is unreachable, NO decision
    /// is applied (the proposals are simply logged).
    pub async fn apply_proposals(
        &self,
        proposals: &[Proposal],
    ) -> Result<ApplyResult, Box<dyn std::error::Error + Sync + Send>> {
        let mut applied = Vec::new();
        let mut skipped = Vec::new();

        if proposals.is_empty() {
            return Ok(ApplyResult { applied, skipped });
        }

        // Single check of Laya availability (deep health check)
        match self.laya.deep_health().await {
            Ok(_) => debug!("Laya available (deep health OK)"),
            Err(e) => {
                warn!(
                    "Laya unavailable ({}): {} proposals SKIPPED (no fallback policy).",
                    e,
                    proposals.len()
                );
                return Ok(ApplyResult { applied, skipped });
            }
        }

        for proposal in proposals {
            // Build the payload
            let criteria = match proposal.action {
                DecisionAction::DropIndex => HashMap::from([
                    (
                        "0".to_string(),
                        "Keep: index might be needed later".to_string(),
                    ),
                    (
                        "1".to_string(),
                        format!("Drop: index is unused — {}", proposal.reason),
                    ),
                ]),
                DecisionAction::RebuildIndex => HashMap::from([
                    (
                        "0".to_string(),
                        "Keep: fragmentation acceptable".to_string(),
                    ),
                    (
                        "1".to_string(),
                        format!("Rebuild: index is fragmented — {}", proposal.reason),
                    ),
                ]),
                DecisionAction::CreateIndex | DecisionAction::CreateFilteredIndex => {
                    let mut m = HashMap::new();
                    m.insert("0".to_string(), "Ignore: no index needed".to_string());
                    m.insert(
                        "1".to_string(),
                        format!("Create standard index — {}", proposal.reason),
                    );
                    m.insert(
                        "2".to_string(),
                        format!(
                            "Create filtered index (WHERE IS NOT NULL) — {}",
                            proposal.reason
                        ),
                    );
                    m
                }
                DecisionAction::Ignore => {
                    HashMap::from([("0".to_string(), "No action needed".to_string())])
                }
            };

            let payload = LayaClient::build_payload(&proposal.reason, criteria);

            // Call Laya
            match self.laya.decision(&payload).await {
                Ok(response) => {
                    let choice = response.answers.action.choice;
                    let confidence = response.answers.action.confidence;

                    info!(
                        "Laya decision: choice={}, confidence={}, input_tokens={}",
                        choice, confidence, response.usage.input_tokens
                    );

                    // Check the confidence
                    if confidence < self.config.thresholds.min_confidence {
                        debug!(
                            "Confidence {} < threshold {}, skipping",
                            confidence, self.config.thresholds.min_confidence
                        );
                        continue;
                    }

                    // Apply if Laya confirms (choice=1 = APPLY for this proposal)
                    let decision = match proposal.action {
                        DecisionAction::DropIndex => {
                            if choice == "1" {
                                Some(DecisionAction::DropIndex)
                            } else {
                                Some(DecisionAction::Ignore)
                            }
                        }
                        DecisionAction::RebuildIndex => {
                            if choice == "1" {
                                Some(DecisionAction::RebuildIndex)
                            } else {
                                Some(DecisionAction::Ignore)
                            }
                        }
                        DecisionAction::CreateIndex | DecisionAction::CreateFilteredIndex => {
                            match choice.as_str() {
                                "1" => Some(DecisionAction::CreateIndex),
                                "2" => Some(DecisionAction::CreateFilteredIndex),
                                _ => Some(DecisionAction::Ignore),
                            }
                        }
                        _ => None,
                    };
                    if let Some(d) = decision {
                        match d {
                            DecisionAction::Ignore => {
                                info!("Laya recommended IGNORE: {}", proposal.reason);
                            }
                            DecisionAction::CreateIndex | DecisionAction::CreateFilteredIndex => {
                                info!("Laya recommended CREATE INDEX: {}", proposal.reason);
                                if self.config.actions.create_index {
                                    // Laya's CHOICE (1 = standard, 2 = filtered) determines the
                                    // DDL executed — not the variant generated at analysis.
                                    // (Fixed regression: both choices executed the same DDL.)
                                    let filtered = matches!(d, DecisionAction::CreateFilteredIndex);
                                    let ddl_to_run = match (
                                        proposal.index_name.as_deref(),
                                        proposal.column.as_deref(),
                                    ) {
                                        (Some(idx_name), Some(column))
                                            if !proposal.schema.is_empty() =>
                                        {
                                            ddl::build_create_index_ddl(
                                                &proposal.schema,
                                                idx_name,
                                                &proposal.table,
                                                column,
                                                filtered,
                                            )
                                        }
                                        _ => proposal.ddl.clone(),
                                    };
                                    match ddl::execute_create_index(
                                        &self.pool,
                                        &ddl_to_run,
                                        self.config.thresholds.ddl_timeout_seconds,
                                        &self.log_writer,
                                        Some(confidence),
                                        Some(proposal.reason.clone()),
                                    )
                                    .await
                                    {
                                        Ok(ddl::DdlOutcome::Applied) => {
                                            applied.push(proposal.clone());
                                        }
                                        Ok(ddl::DdlOutcome::Skipped) => {
                                            skipped.push(proposal.clone());
                                        }
                                        Err(e) => warn!("Failed to create index: {}", e),
                                    }
                                }
                            }
                            DecisionAction::DropIndex => {
                                info!("Laya recommended DROP INDEX: {}", proposal.reason);
                                if self.config.actions.drop_unused {
                                    // Get the index size before dropping
                                    let size_bytes =
                                        self.get_index_size(&proposal.ddl).await.unwrap_or(0);
                                    match ddl::execute_drop_index(
                                        &self.pool,
                                        &proposal.ddl,
                                        self.config.thresholds.ddl_timeout_seconds,
                                        &self.log_writer,
                                        Some(confidence),
                                        size_bytes,
                                        Some(proposal.reason.clone()),
                                    )
                                    .await
                                    {
                                        Ok(ddl::DdlOutcome::Applied) => {
                                            applied.push(proposal.clone());
                                        }
                                        Ok(ddl::DdlOutcome::Skipped) => {
                                            skipped.push(proposal.clone());
                                        }
                                        Err(e) => warn!("Failed to drop index: {}", e),
                                    }
                                }
                            }
                            DecisionAction::RebuildIndex => {
                                info!("Laya recommended REINDEX: {}", proposal.reason);
                                if self.config.actions.rebuild_fragmented {
                                    // Get the fragmentation
                                    let frag = self
                                        .get_index_fragmentation(&proposal.ddl)
                                        .await
                                        .unwrap_or(0.0);
                                    match ddl::execute_reindex(
                                        &self.pool,
                                        &proposal.ddl,
                                        self.config.thresholds.ddl_timeout_seconds,
                                        &self.log_writer,
                                        Some(confidence),
                                        self.get_index_size(&proposal.ddl).await.unwrap_or(0),
                                        frag,
                                        Some(proposal.reason.clone()),
                                    )
                                    .await
                                    {
                                        Ok(ddl::DdlOutcome::Applied) => {
                                            applied.push(proposal.clone());
                                        }
                                        Ok(ddl::DdlOutcome::Skipped) => {
                                            // REINDEX has no IF EXISTS: in practice
                                            // this branch is never reached.
                                            skipped.push(proposal.clone());
                                        }
                                        Err(e) => warn!("Failed to rebuild index: {}", e),
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Laya decision failed: {} — proposal skipped (no fallback)",
                        e
                    );
                }
            }
        }

        if !skipped.is_empty() {
            info!(
                "Apply summary: {} applied, {} skipped (no-op idempotent)",
                applied.len(),
                skipped.len()
            );
        }
        Ok(ApplyResult { applied, skipped })
    }

    /// Get the size of an index from a DDL (schema-qualified)
    async fn get_index_size(
        &self,
        ddl: &str,
    ) -> Result<u64, Box<dyn std::error::Error + Sync + Send>> {
        let index_name = crate::ddl::extract_index_name(ddl);
        if let Some(name) = index_name {
            let schema = crate::ddl::extract_schema_from_ddl(ddl);
            let client = self.pool.get().await?;
            let row = client
                .query_opt(
                    r#"
                    SELECT pg_relation_size(c.oid)::bigint
                    FROM pg_class c
                    JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'i'
                    "#,
                    &[&schema, &name],
                )
                .await?;
            Ok(row.map(|r| r.get::<_, i64>(0) as u64).unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    /// Get the fragmentation rate of an index (same score as the analysis:
    /// dead tuples 50% + unanalyzed modifications 30% + large index never scanned 20%)
    async fn get_index_fragmentation(
        &self,
        ddl: &str,
    ) -> Result<f64, Box<dyn std::error::Error + Sync + Send>> {
        let index_name = crate::ddl::extract_index_name(ddl);
        if let Some(name) = index_name {
            let schema = crate::ddl::extract_schema_from_ddl(ddl);
            let client = self.pool.get().await?;
            let row = client
                .query_opt(
                    r#"
                    WITH ii AS (
                        SELECT s.idx_scan,
                               pg_relation_size(s.indexrelid) / 8192 AS relpages,
                               pi.indrelid AS table_oid
                        FROM pg_stat_user_indexes s
                        JOIN pg_index pi ON s.indexrelid = pi.indexrelid
                        JOIN pg_class c ON c.oid = s.indexrelid
                        JOIN pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = $1 AND s.indexrelname = $2
                        LIMIT 1
                    )
                    SELECT ii.relpages, ii.idx_scan,
                           COALESCE(t.n_live_tup, 0) AS n_live_tup,
                           COALESCE(t.n_dead_tup, 0) AS n_dead_tup,
                           COALESCE(t.n_mod_since_analyze, 0) AS n_mod_since_analyze
                    FROM ii
                    LEFT JOIN pg_stat_user_tables t ON t.relid = ii.table_oid
                    "#,
                    &[&schema, &name],
                )
                .await?;

            if let Some(r) = row {
                let relpages: i64 = r.get("relpages");
                let idx_scan: i64 = r.get("idx_scan");
                let n_live_tup: i64 = r.get("n_live_tup");
                let n_dead_tup: i64 = r.get("n_dead_tup");
                let n_mod_since_analyze: i64 = r.get("n_mod_since_analyze");

                let total = (n_live_tup + n_dead_tup) as f64;
                let dead_ratio = if total > 0.0 {
                    n_dead_tup as f64 / total
                } else {
                    0.0
                };
                let mod_ratio = if n_live_tup > 0 {
                    (n_mod_since_analyze as f64 / n_live_tup as f64).min(1.0)
                } else {
                    0.0
                };
                let unused_big = if idx_scan == 0 && relpages > 100 {
                    0.2
                } else {
                    0.0
                };

                Ok(((dead_ratio * 0.5 + mod_ratio * 0.3 + unused_big) * 100.0).min(100.0))
            } else {
                Ok(0.0)
            }
        } else {
            Ok(0.0)
        }
    }
}

/// Sanitize a field (column type from the catalog) before inserting it
/// into a `reason` sent to Laya: alphanumeric + `_ ()/,` only,
/// bounded length. Everything else is removed — never uncontrolled raw text.
fn sanitize_reason_field(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .take(SANITIZED_MAX_LEN)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '(' | ')' | ',' | '/'))
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_reason_field() {
        assert_eq!(
            sanitize_reason_field("character varying"),
            "character varying"
        );
        assert_eq!(sanitize_reason_field("int[]"), "int"); // brackets removed
        assert_eq!(sanitize_reason_field(""), "unknown");
        assert_eq!(
            sanitize_reason_field("a; DROP TABLE x; --"),
            "a DROP TABLE x "
        );
        let long = &"x".repeat(200);
        assert!(sanitize_reason_field(long).len() <= SANITIZED_MAX_LEN);
    }
}
