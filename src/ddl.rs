use deadpool_postgres::Pool;
use std::time::Instant;
use tracing::{error, info, warn};

use crate::db::queries;
use crate::index_log::{CrudOperation, IndexLogWriter, IndexOperationLog};

/// Exécuter un DDL avec une TRANSACTION EXPLICITE et validation post-commit.
///
/// Règle PostgreSQL : `CREATE INDEX CONCURRENTLY` et `REINDEX CONCURRENTLY`
/// ne peuvent PAS s'exécuter dans un bloc transactionnel.
/// - DDL atomique (DROP INDEX) → BEGIN … COMMIT, ROLLBACK en cas d'erreur
/// - DDL CONCURRENTLY → exécution directe (auto-commit), jamais dans une transaction
pub async fn run_in_transaction(
    pool: &Pool,
    ddl: &str,
) -> Result<u64, Box<dyn std::error::Error + Sync + Send>> {
    if is_concurrent_ddl(ddl) {
        // CONCURRENTLY : interdit dans une transaction, exécution directe
        let client = pool.get().await?;
        let rows = client.execute(ddl, &[]).await?;
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

/// Détecter un DDL CONCURRENTLY (CREATE INDEX CONCURRENTLY / REINDEX … CONCURRENTLY)
fn is_concurrent_ddl(ddl: &str) -> bool {
    ddl.to_uppercase().contains("CONCURRENTLY")
}

/// Exécuter un CREATE INDEX (CONCURRENTLY, hors transaction) avec :
/// - pré-check d'existence (idempotence)
/// - validation post-commit de la VALIDITÉ (indisvalid) — un build CONCURRENTLY
///   interrompu laisse un index INVALIDE qui doit être nettoyé
/// - logging structuré
pub async fn execute_create_index(
    pool: &Pool,
    ddl: &str,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    reason: Option<String>,
) -> Result<Option<String>, Box<dyn std::error::Error + Sync + Send>> {
    let index_name = extract_index_name(ddl);
    let schema = extract_schema_from_ddl(ddl);
    if let Some(ref name) = index_name {
        if queries::index_exists(pool, &schema, name).await? {
            info!("Index {} already exists, skipping CREATE", name);
            return Ok(None);
        }
    }

    info!("Executing CREATE INDEX: {ddl}");

    let object = index_name.clone().unwrap_or_else(|| "unknown".to_string());
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            // Validation post-commit : l'index doit EXISTER et être VALIDE
            if let Some(ref name) = index_name {
                if !queries::index_valid(pool, &schema, name).await? {
                    let cleaned = queries::cleanup_invalid_indexes(pool, &schema, name)
                        .await
                        .unwrap_or(0);
                    let err = format!(
                        "CREATE INDEX CONCURRENTLY a échoué : index {}.{name} absent ou INVALIDE ({} index invalide nettoyé)",
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
                    0, // Taille inconnue avant création
                    0.0,
                    reason
                        .clone()
                        .unwrap_or_else(|| "missing index suggestion".to_string()),
                ),
            );

            Ok(Some(object))
        }
        Err(e) => {
            error!("CREATE INDEX failed: {e}");

            // Nettoyage d'éventuel index invalide résiduel (build CONCURRENTLY interrompu)
            if let Some(ref name) = index_name {
                let cleaned = queries::cleanup_invalid_indexes(pool, &schema, name)
                    .await
                    .unwrap_or(0);
                if cleaned > 0 {
                    warn!(
                        "Nettoyé {cleaned} index invalide(s) résiduel(s) pour {}.{name}",
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

/// Exécuter un DROP INDEX avec transaction explicite et logging structuré
pub async fn execute_drop_index(
    pool: &Pool,
    ddl: &str,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    size_bytes: u64,
    reason: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    info!("Executing DROP INDEX (tx): {ddl}");

    let object = extract_index_name(ddl).unwrap_or_else(|| "unknown".to_string());
    let schema = extract_schema_from_ddl(ddl);
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            info!(
                "DROP INDEX successful ({} rows affected) in {:.2}ms",
                rows,
                duration.as_secs_f64() * 1000.0
            );

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

            Ok(())
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

/// Exécuter un REINDEX (CONCURRENTLY → auto-commit ; standard → transaction)
/// avec validation post-opération et logging structuré
pub async fn execute_reindex(
    pool: &Pool,
    ddl: &str,
    writer: &IndexLogWriter,
    confidence: Option<f64>,
    size_bytes: u64,
    fragmentation: f64,
    reason: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    if is_concurrent_ddl(ddl) {
        warn!("REINDEX CONCURRENTLY cannot run in a transaction; executing with auto-commit.");
    }
    info!("Executing REINDEX: {ddl}");

    let schema = extract_schema_from_ddl(ddl);
    let object = extract_index_name(ddl).unwrap_or_else(|| "unknown".to_string());
    let start = Instant::now();

    let result = run_in_transaction(pool, ddl).await;
    let duration = start.elapsed();

    match result {
        Ok(rows) => {
            // REINDEX CONCURRENTLY interrompu : l'index valide d'origine subsiste,
            // mais un index INVALIDE peut résider → nettoyage + échec
            if is_concurrent_ddl(ddl) {
                let cleaned = queries::cleanup_invalid_indexes(pool, &schema, &object)
                    .await
                    .unwrap_or(0);
                if cleaned > 0 {
                    let err = format!(
                        "REINDEX CONCURRENTLY a échoué en cours : {cleaned} index invalide(s) nettoyé(s), l'index d'origine est intact"
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
                        format!("REINDEX terminé mais l'index {schema}.{object} n'est plus valide");
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

            Ok(())
        }
        Err(e) => {
            error!("REINDEX failed (rolled back): {e}");

            // Un REINDEX CONCURRENTLY échoué peut laisser un index invalide
            let cleaned = queries::cleanup_invalid_indexes(pool, &schema, &object)
                .await
                .unwrap_or(0);
            if cleaned > 0 {
                warn!("Nettoyé {cleaned} index invalide(s) résiduel(s) pour {schema}.{object}");
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

/// Extraire le nom de l'index depuis un DDL (CREATE/DROP/REINDEX)
/// Gère : CREATE INDEX [CONCURRENTLY] [IF NOT EXISTS] [schema.]name ON …,
///        DROP INDEX [IF EXISTS] [schema.]name,
///        REINDEX INDEX [CONCURRENTLY] [schema.]name
pub fn extract_index_name(ddl: &str) -> Option<String> {
    let trimmed = ddl.trim();
    let upper = trimmed.to_uppercase();

    /// Dernier segment après le dernier '.' (gère les noms schema-qualifiés)
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
            // Le dernier mot avant ON qui n'est pas un mot-clé = le nom
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

/// Extraire le schéma depuis un DDL (CREATE … ON schema.t / DROP INDEX [IF EXISTS] schema.idx /
/// REINDEX INDEX [CONCURRENTLY] schema.idx). Défaut : "public".
pub fn extract_schema_from_ddl(ddl: &str) -> String {
    let upper = ddl.to_uppercase();

    /// Si `token` est de la forme `schema.nom`, renvoie `schema`.
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
}
