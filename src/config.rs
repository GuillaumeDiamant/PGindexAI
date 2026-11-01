use serde::Deserialize;
use std::path::PathBuf;
use tracing::{info, warn};

/// Chemin par défaut de la configuration
pub fn default_config_path() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        dir.join("PGindexAI").join("PGindexAI.yaml")
    } else {
        PathBuf::from("./PGindexAI.yaml")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub postgresql: PgConfig,
    pub laya: LayaConfig,
    pub actions: ActionsConfig,
    pub thresholds: Thresholds,
    #[serde(default)]
    pub analysis: AnalysisConfig,
    #[serde(default)]
    pub pg_pool: PgPoolConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub index_logging: IndexLoggingConfig,
    #[serde(default)]
    pub density_by_type: DensityByTypeConfig,
}

impl Config {
    /// Valide la cohérence des valeurs. Refuse les configs dangereuses
    /// (boucle chaude, pool illimité, plage de confiance hors [0,1], …).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs: Vec<String> = Vec::new();

        let t = &self.thresholds;
        if t.cycle_seconds < 1 {
            errs.push(
                "thresholds.cycle_seconds doit être >= 1 (protection contre la boucle chaude)"
                    .to_string(),
            );
        }
        if t.cycle_seconds > 86_400 {
            errs.push("thresholds.cycle_seconds doit être <= 86400 (1 jour)".to_string());
        }
        if t.unused_minutes < 1 {
            errs.push("thresholds.unused_minutes doit être >= 1".to_string());
        }
        if !(1..=100).contains(&t.fragmentation_pct) {
            errs.push("thresholds.fragmentation_pct doit être dans [1, 100]".to_string());
        }
        if !(0.0..=1.0).contains(&t.min_confidence) {
            errs.push("thresholds.min_confidence doit être dans [0.0, 1.0]".to_string());
        }
        if t.max_proposals_per_cycle < 1 {
            errs.push("thresholds.max_proposals_per_cycle doit être >= 1".to_string());
        }
        if t.max_proposals_per_cycle > 1_000 {
            errs.push("thresholds.max_proposals_per_cycle doit être <= 1000".to_string());
        }
        if t.min_sargable_rows == 0 {
            errs.push("thresholds.min_sargable_rows doit être >= 1".to_string());
        }
        if t.ddl_timeout_seconds > 86_400 {
            errs.push(
                "thresholds.ddl_timeout_seconds doit être <= 86400 (1 jour ; 0 = illimité)"
                    .to_string(),
            );
        }

        if !(1..=512).contains(&self.pg_pool.max_size) {
            errs.push(
                "pg_pool.max_size doit être dans [1, 512] (0 = illimité, refusé)".to_string(),
            );
        }
        if !(100..=600_000).contains(&self.pg_pool.wait_timeout_ms) {
            errs.push("pg_pool.wait_timeout_ms doit être dans [100, 600000]".to_string());
        }

        if self.laya.url.trim().is_empty() {
            errs.push("laya.url est vide".to_string());
        }
        if !(1..=600).contains(&self.laya.timeout_seconds) {
            errs.push("laya.timeout_seconds doit être dans [1, 600]".to_string());
        }

        let ssl = self.postgresql.ssl_mode.to_ascii_lowercase();
        if !matches!(
            ssl.as_str(),
            "disable" | "require" | "verify-ca" | "verify-full"
        ) {
            errs.push(format!(
                "postgresql.ssl_mode '{}' non supporté (disable | require | verify-ca | verify-full)",
                self.postgresql.ssl_mode
            ));
        }
        if (ssl == "verify-ca" || ssl == "verify-full") && self.postgresql.ssl_root_cert.is_none() {
            warn!(
                "ssl_mode={} : aucune ssl_root_cert fournie — les certificats auto-signés (snakeoil) ne seront pas acceptés (racines web PKI uniquement)",
                ssl
            );
        }
        if self.postgresql.host.trim().is_empty() {
            errs.push("postgresql.host est vide".to_string());
        }
        if self.postgresql.database.trim().is_empty() {
            errs.push("postgresql.database est vide".to_string());
        }
        if self.postgresql.user.trim().is_empty() {
            errs.push("postgresql.user est vide".to_string());
        }

        let densities: [(&str, f64); 10] = [
            ("int4", self.density_by_type.int4),
            ("int8", self.density_by_type.int8),
            ("float8", self.density_by_type.float8),
            ("varchar", self.density_by_type.varchar),
            ("text", self.density_by_type.text),
            ("date", self.density_by_type.date),
            ("timestamptz", self.density_by_type.timestamptz),
            ("uuid", self.density_by_type.uuid),
            ("bool", self.density_by_type.bool),
            ("default_density", self.density_by_type.default_density),
        ];
        for (name, v) in densities {
            if !(v > 0.0 && v.is_finite()) {
                errs.push(format!("density_by_type.{} doit être > 0 et fini", name));
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errs.join("; ")))
        }
    }
}

/// Paramètres d'analyse (multi-schéma)
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnalysisConfig {
    /// Liste des schémas à analyser.
    /// - liste vide = tous les schémas non-système (défaut)
    /// - ["public", "billing"] = uniquement ces schémas
    #[serde(default)]
    pub schemas: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PgConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    /// disable | require | verify-ca | verify-full
    #[serde(default = "default_ssl_mode")]
    pub ssl_mode: String,
    /// Mot de passe (optionnel — requis si pg_hba exige scram-sha-256 / md5).
    /// Stocker ce fichier en 0600 (le daemon le lit au démarrage).
    #[serde(default)]
    pub password: Option<String>,
    /// Chemin vers un certificat CA (PEM) pour verify-ca / verify-full.
    /// Sans cette clé, les racines web PKI (Mozilla) sont utilisées.
    #[serde(default)]
    pub ssl_root_cert: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayaConfig {
    pub url: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionsConfig {
    #[serde(default = "default_true")]
    pub create_index: bool,
    #[serde(default = "default_true")]
    pub drop_unused: bool,
    #[serde(default = "default_true")]
    pub rebuild_fragmented: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds {
    /// Intervalle d'un cycle d'analyse (secondes). >= 1.
    #[serde(default = "default_cycle")]
    pub cycle_seconds: u64,
    /// Fenêtre d'observation d'« inutilisé » (minutes).
    /// Le DROP n'est proposé que si le serveur est up depuis >= cette durée
    /// (protection contre la remise à zéro des compteurs après reboot).
    /// Défaud : 10080 (7 jours).
    #[serde(default = "default_unused_minutes")]
    pub unused_minutes: u64,
    /// Seuil de fragmentation pour proposer une reconstruction (1..=100).
    #[serde(default = "default_fragmentation")]
    pub fragmentation_pct: u64,
    /// Nombre minimum d'EXÉCUTIONS (pg_stat_statements.calls) d'une requête
    /// pour qu'elle soit candidate à un index SARGable.
    /// (Le nom historique `min_sargable_rows` est conservé pour compatibilité.)
    #[serde(default = "default_min_sargable")]
    pub min_sargable_rows: u64,
    /// Confiance minimale exigée de Laya pour exécuter une action (0.0..=1.0).
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    /// Budget de propositions par cycle (limite SQL + troncature totale).
    #[serde(default = "default_max_proposals")]
    pub max_proposals_per_cycle: u32,
    /// Timeout (secondes) des DDL longs (CREATE/REINDEX CONCURRENTLY).
    /// 0 = illimité. Défaut : 1800 (30 min).
    /// NB : un timeout TROP COURT sur une grosse table créerait une boucle
    /// build→annulé→retry — garder une valeur généreuse.
    #[serde(default = "default_ddl_timeout")]
    pub ddl_timeout_seconds: u64,
}

/// NB : `Default` est implémenté MANUELLEMENT (pas `derive`) : quand la
/// section `logging:` est absente, `#[serde(default)]` invoque
/// `Default::default()` — il doit donc produire les MÊMES valeurs que les
/// defaults serde par champ (un derive donnerait `level: ""` = aucun log).
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

/// Configuration du logging des opérations d'index (historisation)
/// NB : `Default` manuel — idem `LoggingConfig` (section absente = mêmes
/// valeurs que les defaults serde par champ : audit ENABLED, pas coupé).
#[derive(Debug, Clone, Deserialize)]
pub struct IndexLoggingConfig {
    #[serde(default = "default_index_logging_enabled")]
    pub enabled: bool,
    /// minimal | standard | verbose
    #[serde(default = "default_index_logging_verbosity")]
    pub verbosity: String,
    /// Chemin du fichier de log (vide = stdout uniquement)
    #[serde(default)]
    pub log_file: String,
    /// Rotation (octets, 0 = désactivé)
    #[serde(default = "default_index_logging_max_size")]
    pub max_file_size_bytes: u64,
    /// Nombre max de fichiers rotés
    #[serde(default = "default_index_logging_max_files")]
    pub max_files: u64,
}

impl Default for IndexLoggingConfig {
    fn default() -> Self {
        Self {
            enabled: default_index_logging_enabled(),
            verbosity: default_index_logging_verbosity(),
            log_file: String::new(),
            max_file_size_bytes: default_index_logging_max_size(),
            max_files: default_index_logging_max_files(),
        }
    }
}

/// Configuration du pool de connexions PostgreSQL
#[derive(Debug, Clone, Deserialize)]
pub struct PgPoolConfig {
    /// Taille maximale du pool de connexions (1..=512)
    #[serde(default = "default_pool_max_size")]
    pub max_size: u32,
    /// Timeout d'attente d'une connexion dans la file (ms)
    #[serde(default = "default_pool_wait_timeout_ms")]
    pub wait_timeout_ms: u64,
}

impl Default for PgPoolConfig {
    fn default() -> Self {
        Self {
            max_size: 16,
            wait_timeout_ms: 30_000,
        }
    }
}

/// Densité cible par type de colonne (tuples par page visés)
#[derive(Debug, Clone, Deserialize)]
pub struct DensityByTypeConfig {
    #[serde(default = "default_density_int4")]
    pub int4: f64,
    #[serde(default = "default_density_int8")]
    pub int8: f64,
    #[serde(default = "default_density_float8")]
    pub float8: f64,
    #[serde(default = "default_density_varchar")]
    pub varchar: f64,
    #[serde(default = "default_density_text")]
    pub text: f64,
    #[serde(default = "default_density_date")]
    pub date: f64,
    #[serde(default = "default_density_timestamptz")]
    pub timestamptz: f64,
    #[serde(default = "default_density_uuid")]
    pub uuid: f64,
    #[serde(default = "default_density_bool")]
    pub bool: f64,
    #[serde(default = "default_density_default")]
    pub default_density: f64,
}

impl Default for DensityByTypeConfig {
    fn default() -> Self {
        Self {
            int4: 200.0,
            int8: 200.0,
            float8: 150.0,
            varchar: 30.0,
            text: 20.0,
            date: 150.0,
            timestamptz: 150.0,
            uuid: 100.0,
            bool: 300.0,
            default_density: 50.0,
        }
    }
}

// ─────────────────────────────────────────────
// Fonctions de valeur par défaut
// ─────────────────────────────────────────────

fn default_ssl_mode() -> String {
    "disable".to_string()
}
fn default_timeout() -> u64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_cycle() -> u64 {
    60
}
fn default_unused_minutes() -> u64 {
    10_080
} // 7 jours
fn default_fragmentation() -> u64 {
    70
}
fn default_min_sargable() -> u64 {
    100
}
fn default_min_confidence() -> f64 {
    0.7
}
fn default_max_proposals() -> u32 {
    20
}
fn default_ddl_timeout() -> u64 {
    1_800
} // 30 min
fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "json".to_string()
}
fn default_index_logging_enabled() -> bool {
    true
}
fn default_index_logging_verbosity() -> String {
    "minimal".to_string()
}
fn default_index_logging_max_size() -> u64 {
    10 * 1024 * 1024
}
fn default_index_logging_max_files() -> u64 {
    5
}
fn default_pool_max_size() -> u32 {
    16
}
fn default_pool_wait_timeout_ms() -> u64 {
    30_000
}
fn default_density_int4() -> f64 {
    200.0
}
fn default_density_int8() -> f64 {
    200.0
}
fn default_density_float8() -> f64 {
    150.0
}
fn default_density_varchar() -> f64 {
    30.0
}
fn default_density_text() -> f64 {
    20.0
}
fn default_density_date() -> f64 {
    150.0
}
fn default_density_timestamptz() -> f64 {
    150.0
}
fn default_density_uuid() -> f64 {
    100.0
}
fn default_density_bool() -> f64 {
    300.0
}
fn default_density_default() -> f64 {
    50.0
}

// ─────────────────────────────────────────────
// Détection des clés inconnues (avertissement, non bloquant)
// ─────────────────────────────────────────────

/// Clés reconnues par section. Toute clé étrangère déclenche un avertissement
/// visible (typo, fonctionnalité morte, config résiduelle) au lieu d'être
/// ignorée silencieusement.
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    (
        "postgresql",
        &[
            "host",
            "port",
            "user",
            "database",
            "ssl_mode",
            "password",
            "ssl_root_cert",
        ],
    ),
    ("laya", &["url", "timeout_seconds"]),
    (
        "actions",
        &["create_index", "drop_unused", "rebuild_fragmented"],
    ),
    (
        "thresholds",
        &[
            "cycle_seconds",
            "unused_minutes",
            "fragmentation_pct",
            "min_sargable_rows",
            "min_confidence",
            "max_proposals_per_cycle",
        ],
    ),
    ("analysis", &["schemas"]),
    ("pg_pool", &["max_size", "wait_timeout_ms"]),
    ("logging", &["level", "format"]),
    (
        "index_logging",
        &[
            "enabled",
            "verbosity",
            "log_file",
            "max_file_size_bytes",
            "max_files",
        ],
    ),
    (
        "density_by_type",
        &[
            "int4",
            "int8",
            "float8",
            "varchar",
            "text",
            "date",
            "timestamptz",
            "uuid",
            "bool",
            "default_density",
        ],
    ),
];

fn warn_unknown_keys(value: &serde_yaml::Value) {
    let Some(top) = value.as_mapping() else {
        return;
    };
    for (k, v) in top {
        let Some(section) = k.as_str() else { continue };
        let Some((_, keys)) = KNOWN_KEYS.iter().find(|(s, _)| *s == section) else {
            warn!(
                "config : section '{}' inconnue (ignorée — ex. 'dashboard' n'est pas implémenté)",
                section
            );
            continue;
        };
        if let Some(inner) = v.as_mapping() {
            for ik in inner.keys() {
                if let Some(iks) = ik.as_str() {
                    if !keys.contains(&iks) {
                        warn!(
                            "config : clé '{}.{}' inconnue (ignorée — typo ou fonctionnalité retirée ?)",
                            section, iks
                        );
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────
// Chargement de la config
// ─────────────────────────────────────────────

pub fn load(path: &PathBuf) -> Result<Config, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.clone(), e))?;
    let value: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|e| ConfigError::Parse(path.clone(), e))?;
    warn_unknown_keys(&value);
    let config: Config =
        serde_yaml::from_value(value).map_err(|e| ConfigError::Parse(path.clone(), e))?;
    config.validate()?;
    info!("Config loaded from {}", path.display());
    Ok(config)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Cannot read config at {}: {}", .0.display(), .1)]
    Read(PathBuf, std::io::Error),
    #[error("Cannot parse config at {}: {}", .0.display(), .1)]
    Parse(PathBuf, serde_yaml::Error),
    #[error("Invalid config: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une config MINIMALE (sections `logging` / `index_logging` absentes)
    /// doit produire les mêmes valeurs que les defaults serde par champ.
    ///
    /// Régression : avec `#[derive(Default)]`, l'absence de section invoquait
    /// `Default::default()` → `level: ""` (daemon SANS aucun log) et
    /// `index_logging.enabled: false` (audit silencieusement coupé).
    #[test]
    fn test_defaults_when_sections_absent() {
        // NB : `thresholds` est REQUIS par design (fail-fast) — une section
        // vide vérifie en bonus les defaults serde par champ.
        let yaml = r#"
postgresql:
  host: localhost
  port: 5432
  user: pgindexai
  database: pgindexai_demo
  ssl_mode: disable
laya:
  url: http://127.0.0.1:8080
actions:
  create_index: true
  drop_unused: true
  rebuild_fragmented: true
thresholds: {}
"#;
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let cfg: Config = serde_yaml::from_value(value).unwrap();

        assert_eq!(cfg.logging.level, "info");
        assert_eq!(cfg.logging.format, "json");
        assert!(
            cfg.index_logging.enabled,
            "l'audit doit rester ENABLED sans section"
        );
        assert_eq!(cfg.index_logging.verbosity, "minimal");
        assert_eq!(cfg.index_logging.max_file_size_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.index_logging.max_files, 5);

        // `thresholds: {}` → les defaults serde par champ doivent s'appliquer
        assert_eq!(cfg.thresholds.cycle_seconds, 60);
        assert_eq!(cfg.thresholds.ddl_timeout_seconds, 1800);

        // Cohérence : `Default::default()` = mêmes valeurs que le parse serde
        assert_eq!(LoggingConfig::default().level, cfg.logging.level);
        assert_eq!(
            IndexLoggingConfig::default().enabled,
            cfg.index_logging.enabled
        );
    }
}
