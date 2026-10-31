//! Logging structuré des opérations d'index (DROP/CREATE/REINDEX).
//!
//! Trois niveaux de verbosité :
//! - **minimal** : timestamp, type CRUD, durée, schéma, nom objet
//! - **standard** : + DDL/DML de l'index modifié
//! - **verbose** : + métadonnées techniques (confiance, taille, fragmentation, reason)
//!
//! Rotation : si `max_size > 0`, le fichier courant est renommé `.1`, `.1` → `.2`, …
//! au-delà de `max_size` octets ; au-delà de `max_files`, le plus ancien est supprimé.
//!
//! Sécurité : un échec d'écriture est **toujours** journalisé en `error!`
//! (jamais `.ok()` silencieux) — chaque entrée est une trace d'audit.

use serde::Serialize;
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{error, info, warn};

// ─────────────────────────────────────────────
// Types de CRUD
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub enum CrudOperation {
    CreateIndex,
    DropIndex,
    Reindex,
}

impl Display for CrudOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrudOperation::CreateIndex => write!(f, "CREATE"),
            CrudOperation::DropIndex => write!(f, "DROP"),
            CrudOperation::Reindex => write!(f, "REINDEX"),
        }
    }
}

// ─────────────────────────────────────────────
// Entrées de log
// ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct IndexOperationLog {
    pub timestamp: String,
    pub operation: String,
    pub schema: String,
    pub object: String,
    pub duration_ms: f64,
    pub status: String,
    // standard+
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddl_statement: Option<String>,
    // verbose+
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fragmentation_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl IndexOperationLog {
    pub fn new(
        operation: CrudOperation,
        schema: impl Into<String>,
        object: impl Into<String>,
        duration: Duration,
        status: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            operation: operation.to_string(),
            schema: schema.into(),
            object: object.into(),
            duration_ms: duration.as_secs_f64() * 1000.0,
            status: status.into(),
            ddl_statement: None,
            confidence: None,
            index_size_bytes: None,
            fragmentation_pct: None,
            reason: None,
        }
    }

    /// Niveau standard : ajoute le DDL/DML
    pub fn with_ddl(mut self, ddl: impl Into<String>) -> Self {
        self.ddl_statement = Some(ddl.into());
        self
    }

    /// Niveau verbose : ajoute les métadonnées techniques
    pub fn with_metadata(
        mut self,
        confidence: f64,
        size_bytes: u64,
        fragmentation: f64,
        reason: impl Into<String>,
    ) -> Self {
        self.confidence = Some(confidence);
        self.index_size_bytes = Some(size_bytes);
        self.fragmentation_pct = Some(fragmentation);
        self.reason = Some(reason.into());
        self
    }

    /// Sérialiser en JSON
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            error!("Failed to serialize index log: {e}");
            "{\"error\":\"serialization_failed\"}".to_string()
        })
    }
}

// ─────────────────────────────────────────────
// Writer de log
// ─────────────────────────────────────────────

#[derive(Debug)]
pub struct IndexLogWriter {
    enabled: bool,
    verbosity: VerbosityLevel,
    file_path: Option<String>,
    max_size: u64,
    max_files: u64,
    file_writer: Mutex<Option<File>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum VerbosityLevel {
    Minimal,
    Standard,
    Verbose,
}

impl IndexLogWriter {
    pub fn new(
        enabled: bool,
        verbosity: &str,
        log_file: &str,
        max_size: u64,
        max_files: u64,
    ) -> Self {
        let verbosity = match verbosity {
            "standard" => VerbosityLevel::Standard,
            "verbose" => VerbosityLevel::Verbose,
            _ => VerbosityLevel::Minimal,
        };

        let file_writer = if enabled && !log_file.is_empty() {
            match Self::open_log_file(log_file) {
                Ok(f) => Some(f),
                Err(e) => {
                    error!("Cannot open index log file '{log_file}': {e}. Audit logging to stdout only (FILE LOGGING DISABLED until fixed).");
                    None
                }
            }
        } else {
            None
        };

        Self {
            enabled,
            verbosity,
            file_path: if log_file.is_empty() {
                None
            } else {
                Some(log_file.to_string())
            },
            max_size,
            max_files,
            file_writer: Mutex::new(file_writer),
        }
    }

    fn open_log_file(path: &str) -> Result<File, std::io::Error> {
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // 0640 : les entrées contiennent le DDL exécuté (trace d'audit) —
        // lecture propriétaire + groupe uniquement.
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            opts.mode(0o640);
        }
        opts.open(path)
    }

    /// Journaliser une opération. Synchronique, court — aucun `await` ici,
    /// donc la garde mutex n'est jamais tenue à travers un point de suspension.
    pub fn log(&self, log: IndexOperationLog) {
        if !self.enabled {
            return;
        }

        let json = log.to_json();

        // Toujours écrire dans stdout (tracing)
        match self.verbosity {
            VerbosityLevel::Minimal => {
                info!(
                    "INDEX {} schema={} object={} duration_ms={:.2} status={}",
                    log.operation, log.schema, log.object, log.duration_ms, log.status
                );
            }
            VerbosityLevel::Standard => {
                info!(
                    "INDEX {} schema={} object={} ddl=\"{}\" duration_ms={:.2} status={}",
                    log.operation,
                    log.schema,
                    log.object,
                    log.ddl_statement.as_deref().unwrap_or(""),
                    log.duration_ms,
                    log.status
                );
            }
            VerbosityLevel::Verbose => {
                let mut parts = vec![
                    format!("INDEX {} schema={}", log.operation, log.schema),
                    format!("object={}", log.object),
                    format!("duration_ms={:.2}", log.duration_ms),
                    format!("status={}", log.status),
                ];
                if let Some(ddl) = &log.ddl_statement {
                    parts.push(format!("ddl=\"{ddl}\""));
                }
                if let Some(conf) = log.confidence {
                    parts.push(format!("confidence={conf:.4}"));
                }
                if let Some(size) = log.index_size_bytes {
                    parts.push(format!("size_bytes={size}"));
                }
                if let Some(frag) = log.fragmentation_pct {
                    parts.push(format!("fragmentation={frag:.1}%"));
                }
                if let Some(reason) = &log.reason {
                    parts.push(format!("reason=\"{reason}\""));
                }
                info!("{}", parts.join(" "));
            }
        }

        // Écrire dans le fichier si configuré (rotation si nécessaire)
        if let Some(path) = self.file_path.as_deref() {
            if self.max_size > 0 {
                let needs_rotate = {
                    let guard = self.file_writer.lock().unwrap_or_else(|p| p.into_inner());
                    match guard.as_ref() {
                        Some(f) => f
                            .metadata()
                            .map(|m| m.len() >= self.max_size)
                            .unwrap_or(false),
                        None => true,
                    }
                };
                if needs_rotate {
                    self.rotate(path);
                }
            }
            let mut guard = self.file_writer.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(f) = guard.as_mut() {
                if let Err(e) = writeln!(f, "{json}").and_then(|_| f.flush()) {
                    error!("INDEX LOG: écriture de {path} impossible : {e} — cette entrée d'audit est PERDUE");
                }
            } else {
                error!("INDEX LOG: aucun fichier ouvert pour {path} — cette entrée d'audit est PERDUE (stdout seulement)");
            }
        }
    }

    /// Rotation : path → path.1, path.1 → path.2, … ; path.max_files supprimé.
    fn rotate(&self, path: &str) {
        if self.max_files > 0 {
            let oldest = format!("{path}.{}", self.max_files);
            match std::fs::remove_file(&oldest) {
                Ok(()) => {}
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    warn!("rotation : suppression de {oldest} impossible : {e}");
                }
                Err(_) => {}
            }
            for i in (1..self.max_files).rev() {
                let from = format!("{path}.{i}");
                if std::path::Path::new(&from).exists() {
                    let to = format!("{path}.{}", i + 1);
                    if let Err(e) = std::fs::rename(&from, &to) {
                        warn!("rotation : renommage {from} → {to} impossible : {e}");
                    }
                }
            }
        }
        if let Err(e) = std::fs::rename(path, format!("{path}.1")) {
            error!("rotation : renommage {path} → {path}.1 impossible : {e} (continuation dans le même fichier)");
            return;
        }
        match Self::open_log_file(path) {
            Ok(f) => {
                *self.file_writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
                info!("Index log rotated : {path} → {path}.1");
            }
            Err(e) => {
                error!("rotation : réouverture de {path} impossible : {e} — audit logging en stdout seulement");
                *self.file_writer.lock().unwrap_or_else(|p| p.into_inner()) = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_minimal_log() {
        let writer = IndexLogWriter::new(true, "minimal", "", 0, 0);
        let log = IndexOperationLog::new(
            CrudOperation::DropIndex,
            "public",
            "idx_test",
            Duration::from_millis(150),
            "success",
        );
        let json = log.to_json();
        assert!(json.contains("\"operation\":\"DROP\""));
        assert!(json.contains("\"schema\":\"public\""));
        assert!(json.contains("\"object\":\"idx_test\""));
        assert!(!json.contains("\"ddl_statement\""));
        // Log sur stdout (tracing n'est pas initialisé en test — pas d'effet de bord)
        writer.log(log);
    }

    #[test]
    fn test_standard_log() {
        let writer = IndexLogWriter::new(true, "standard", "", 0, 0);
        let log = IndexOperationLog::new(
            CrudOperation::CreateIndex,
            "public",
            "idx_new",
            Duration::from_millis(200),
            "success",
        )
        .with_ddl("CREATE INDEX idx_new ON public.orders (status)");
        let json = log.to_json();
        assert!(json.contains("\"operation\":\"CREATE\""));
        assert!(json.contains("\"ddl_statement\""));
        assert!(json.contains("CREATE INDEX idx_new"));
        writer.log(log);
    }

    #[test]
    fn test_verbose_log() {
        let writer = IndexLogWriter::new(true, "verbose", "", 0, 0);
        let log = IndexOperationLog::new(
            CrudOperation::Reindex,
            "public",
            "idx_frag",
            Duration::from_millis(500),
            "success",
        )
        .with_ddl("REINDEX INDEX public.idx_frag")
        .with_metadata(0.85, 1048576, 95.0, "high fragmentation");
        let json = log.to_json();
        assert!(json.contains("\"confidence\":0.85"));
        assert!(json.contains("\"index_size_bytes\":1048576"));
        assert!(json.contains("\"fragmentation_pct\":95.0"));
        writer.log(log);
    }

    #[test]
    fn test_rotation_renames_and_keeps_max_files() {
        let dir = std::env::temp_dir().join(format!("pgindexai_rot_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ops.log").to_string_lossy().to_string();

        let writer = IndexLogWriter::new(true, "minimal", &path, 16, 3);

        // Écrire jusqu'à déclencher la rotation plusieurs fois
        for i in 0..10 {
            writer.log(
                IndexOperationLog::new(
                    CrudOperation::CreateIndex,
                    "public",
                    "idx",
                    Duration::from_millis(1),
                    format!("entry-{i}"),
                )
                .with_ddl(format!("CREATE INDEX idx{i} ON t (c)")),
            );
        }

        // Le fichier courant existe et des fichiers rotés sont présents
        assert!(std::path::Path::new(&path).exists());
        assert!(
            std::path::Path::new(&format!("{path}.1")).exists(),
            "expected {path}.1"
        );

        // Au-delà de max_files : pas de .4 (max_files=3 → .1 .2 .3)
        assert!(
            !std::path::Path::new(&format!("{path}.4")).exists(),
            "max_files must be enforced"
        );

        // Nettoyage
        let _ = std::fs::remove_dir_all(&dir);
    }
}
