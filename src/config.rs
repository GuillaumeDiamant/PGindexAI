use serde::Deserialize;
use std::path::PathBuf;
use tracing::{info, warn};

/// Default path for the configuration file
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
    /// Validates the coherence of the values. Rejects dangerous configs
    /// (hot loop, unbounded pool, confidence range outside [0,1], ...).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs: Vec<String> = Vec::new();

        let t = &self.thresholds;
        if t.cycle_seconds < 1 {
            errs.push(
                "thresholds.cycle_seconds must be >= 1 (protection against the hot loop)"
                    .to_string(),
            );
        }
        if t.cycle_seconds > 86_400 {
            errs.push("thresholds.cycle_seconds must be <= 86400 (1 day)".to_string());
        }
        if t.unused_minutes < 1 {
            errs.push("thresholds.unused_minutes must be >= 1".to_string());
        }
        if !(1..=100).contains(&t.fragmentation_pct) {
            errs.push("thresholds.fragmentation_pct must be in [1, 100]".to_string());
        }
        if !(0.0..=1.0).contains(&t.min_confidence) {
            errs.push("thresholds.min_confidence must be in [0.0, 1.0]".to_string());
        }
        if t.max_proposals_per_cycle < 1 {
            errs.push("thresholds.max_proposals_per_cycle must be >= 1".to_string());
        }
        if t.max_proposals_per_cycle > 1_000 {
            errs.push("thresholds.max_proposals_per_cycle must be <= 1000".to_string());
        }
        if t.min_sargable_rows == 0 {
            errs.push("thresholds.min_sargable_rows must be >= 1".to_string());
        }
        if t.ddl_timeout_seconds > 86_400 {
            errs.push(
                "thresholds.ddl_timeout_seconds must be <= 86400 (1 day; 0 = unlimited)"
                    .to_string(),
            );
        }

        if !(1..=512).contains(&self.pg_pool.max_size) {
            errs.push("pg_pool.max_size must be in [1, 512] (0 = unlimited, rejected)".to_string());
        }
        if !(100..=600_000).contains(&self.pg_pool.wait_timeout_ms) {
            errs.push("pg_pool.wait_timeout_ms must be in [100, 600000]".to_string());
        }

        if self.laya.url.trim().is_empty() {
            errs.push("laya.url is empty".to_string());
        }
        if !(1..=600).contains(&self.laya.timeout_seconds) {
            errs.push("laya.timeout_seconds must be in [1, 600]".to_string());
        }

        let ssl = self.postgresql.ssl_mode.to_ascii_lowercase();
        if !matches!(
            ssl.as_str(),
            "disable" | "require" | "verify-ca" | "verify-full"
        ) {
            errs.push(format!(
                "postgresql.ssl_mode '{}' not supported (disable | require | verify-ca | verify-full)",
                self.postgresql.ssl_mode
            ));
        }
        if (ssl == "verify-ca" || ssl == "verify-full") && self.postgresql.ssl_root_cert.is_none() {
            warn!(
                "ssl_mode={}: no ssl_root_cert provided — self-signed certificates (snakeoil) will not be accepted (web PKI roots only)",
                ssl
            );
        }
        if self.postgresql.host.trim().is_empty() {
            errs.push("postgresql.host is empty".to_string());
        }
        if self.postgresql.database.trim().is_empty() {
            errs.push("postgresql.database is empty".to_string());
        }
        if self.postgresql.user.trim().is_empty() {
            errs.push("postgresql.user is empty".to_string());
        }

        let densities: [(&str, f64); 12] = [
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
            ("gin_size_factor", self.density_by_type.gin_size_factor),
            ("gist_size_factor", self.density_by_type.gist_size_factor),
        ];
        for (name, v) in densities {
            if !(v > 0.0 && v.is_finite()) {
                errs.push(format!("density_by_type.{} must be > 0 and finite", name));
            }
        }

        // Enum-like fields: reject typos that would otherwise either PANIC
        // (`logging.level` → `EnvFilter::new` at startup) or silently degrade
        // (`verbosity` → an unknown value used to be treated as Minimal).
        if !matches!(
            self.logging.level.to_ascii_lowercase().as_str(),
            "trace" | "debug" | "info" | "warn" | "error" | "off"
        ) {
            errs.push(format!(
                "logging.level '{}' invalid (expected one of: trace, debug, info, warn, error, off)",
                self.logging.level
            ));
        }
        if !matches!(
            self.index_logging.verbosity.to_ascii_lowercase().as_str(),
            "minimal" | "standard" | "verbose"
        ) {
            errs.push(format!(
                "index_logging.verbosity '{}' invalid (expected one of: minimal, standard, verbose)",
                self.index_logging.verbosity
            ));
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errs.join("; ")))
        }
    }
}

/// Analysis parameters (multi-schema)
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnalysisConfig {
    /// List of schemas to analyze.
    /// - empty list = all non-system schemas (default)
    /// - ["public", "billing"] = only these schemas
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
    /// Password (optional — required if pg_hba requires scram-sha-256 / md5).
    /// Store this file with 0600 permissions (the daemon reads it at startup).
    #[serde(default)]
    pub password: Option<String>,
    /// Path to a CA certificate (PEM) for verify-ca / verify-full.
    /// Without this key, the web PKI (Mozilla) roots are used.
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
    /// Interval of an analysis cycle (seconds). >= 1.
    #[serde(default = "default_cycle")]
    pub cycle_seconds: u64,
    /// "Unused" observation window (minutes).
    /// DROP is only proposed if the server has been up for >= this duration
    /// (protection against counters being reset after a reboot).
    /// Default: 10080 (7 days).
    #[serde(default = "default_unused_minutes")]
    pub unused_minutes: u64,
    /// Fragmentation threshold for proposing a rebuild (1..=100).
    #[serde(default = "default_fragmentation")]
    pub fragmentation_pct: u64,
    /// Physical B-tree bloat detection via pgstattuple (`pgstatindex`,
    /// full index scan on indexes above 100 pages). When `false` — or
    /// when the extension is missing / the probe fails — the detector
    /// falls back to the legacy heuristic alone (previous behavior).
    #[serde(default = "default_true")]
    pub use_index_bloat: bool,
    /// GIN/GIST index-need detection (jsonb / hstore / arrays / tsvector →
    /// GIN, geometry / ranges → GIST). When `false`, detector #3 only
    /// proposes B-tree indexes (classic scalar SARGable patterns) — the
    /// pre-GIN/GIST behavior.
    #[serde(default = "default_true")]
    pub use_gin_gist_detection: bool,
    /// Minimum number of EXECUTIONS (pg_stat_statements.calls) of a query
    /// for it to be a candidate for a SARGable index.
    /// (The historical name `min_sargable_rows` is kept for compatibility.)
    #[serde(default = "default_min_sargable")]
    pub min_sargable_rows: u64,
    /// Minimum confidence required from Laya to execute an action (0.0..=1.0).
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f64,
    /// Proposal budget per cycle (SQL limit + overall truncation).
    #[serde(default = "default_max_proposals")]
    pub max_proposals_per_cycle: u32,
    /// Timeout (seconds) of long DDL (CREATE/REINDEX CONCURRENTLY).
    /// 0 = unlimited. Default: 1800 (30 min).
    /// NB: a TOO SHORT timeout on a large table would create a
    /// build→cancelled→retry loop — keep a generous value.
    #[serde(default = "default_ddl_timeout")]
    pub ddl_timeout_seconds: u64,
}

/// NB: `Default` is implemented MANUALLY (not `derive`): when the
/// `logging:` section is absent, `#[serde(default)]` invokes
/// `Default::default()` — it must therefore produce the SAME values as the
/// per-field serde defaults (a derive would give `level: ""` = no log).
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

/// Configuration of the index operation logging (audit trail)
/// NB: manual `Default` — same as `LoggingConfig` (absent section = same
/// values as the per-field serde defaults: audit ENABLED, not cut).
#[derive(Debug, Clone, Deserialize)]
pub struct IndexLoggingConfig {
    #[serde(default = "default_index_logging_enabled")]
    pub enabled: bool,
    /// minimal | standard | verbose
    #[serde(default = "default_index_logging_verbosity")]
    pub verbosity: String,
    /// Log file path (empty = stdout only)
    #[serde(default)]
    pub log_file: String,
    /// Rotation (bytes, 0 = disabled)
    #[serde(default = "default_index_logging_max_size")]
    pub max_file_size_bytes: u64,
    /// Max number of rotated files
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

/// Configuration of the PostgreSQL connection pool
#[derive(Debug, Clone, Deserialize)]
pub struct PgPoolConfig {
    /// Maximum size of the connection pool (1..=512)
    #[serde(default = "default_pool_max_size")]
    pub max_size: u32,
    /// Timeout waiting for a connection in the queue (ms)
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

/// Target density per column type (tuples per page targeted)
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
    /// Multiplicative factor applied to the size estimate of a **GIN** index
    /// (GIN is typically ~3× the B-tree size for the same column: per-value
    /// posting lists + token storage). Must be > 0.
    #[serde(default = "default_gin_size_factor")]
    pub gin_size_factor: f64,
    /// Multiplicative factor applied to the size estimate of a **GIST** index
    /// (GIST is typically ~2× the B-tree size: R-tree / GiST pages). Must be > 0.
    #[serde(default = "default_gist_size_factor")]
    pub gist_size_factor: f64,
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
            gin_size_factor: 3.0,
            gist_size_factor: 2.0,
        }
    }
}

// ─────────────────────────────────────────────
// Default value functions
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
} // 7 days
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
fn default_gin_size_factor() -> f64 {
    3.0
}
fn default_gist_size_factor() -> f64 {
    2.0
}

// ─────────────────────────────────────────────
// Unknown key detection (warning, non-blocking)
// ─────────────────────────────────────────────

/// Keys recognized per section. Any foreign key triggers a visible
/// warning (typo, dead feature, leftover config) instead of being
/// silently ignored.
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
            "use_index_bloat",
            "use_gin_gist_detection",
            "min_sargable_rows",
            "min_confidence",
            "max_proposals_per_cycle",
            "ddl_timeout_seconds",
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
            "gin_size_factor",
            "gist_size_factor",
        ],
    ),
];

/// Collect the unknown sections/keys of a config as warning messages.
/// Pure function (no logging) so it can be tested directly.
fn unknown_key_warnings(value: &serde_yaml::Value) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(top) = value.as_mapping() else {
        return warnings;
    };
    for (k, v) in top {
        let Some(section) = k.as_str() else { continue };
        let Some((_, keys)) = KNOWN_KEYS.iter().find(|(s, _)| *s == section) else {
            warnings.push(format!(
                "config: section '{}' unknown (ignored — e.g. 'dashboard' is not implemented)",
                section
            ));
            continue;
        };
        if let Some(inner) = v.as_mapping() {
            for ik in inner.keys() {
                if let Some(iks) = ik.as_str() {
                    if !keys.contains(&iks) {
                        warnings.push(format!(
                            "config: key '{}.{}' unknown (ignored — typo or removed feature?)",
                            section, iks
                        ));
                    }
                }
            }
        }
    }
    warnings
}

fn warn_unknown_keys(value: &serde_yaml::Value) {
    for warning in unknown_key_warnings(value) {
        warn!("{warning}");
    }
}

// ─────────────────────────────────────────────
// Config loading
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

    /// A MINIMAL config (sections `logging` / `index_logging` absent)
    /// must produce the same values as the per-field serde defaults.
    ///
    /// Regression: with `#[derive(Default)]`, the absence of a section invoked
    /// `Default::default()` → `level: ""` (daemon with NO log at all) and
    /// `index_logging.enabled: false` (audit silently cut).
    #[test]
    fn test_defaults_when_sections_absent() {
        // NB: `thresholds` is REQUIRED by design (fail-fast) — an empty
        // section also verifies the per-field serde defaults as a bonus.
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
            "the audit must stay ENABLED without a section"
        );
        assert_eq!(cfg.index_logging.verbosity, "minimal");
        assert_eq!(cfg.index_logging.max_file_size_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.index_logging.max_files, 5);

        // `thresholds: {}` → the per-field serde defaults must apply
        assert_eq!(cfg.thresholds.cycle_seconds, 60);
        assert_eq!(cfg.thresholds.ddl_timeout_seconds, 1800);

        // Coherence: `Default::default()` = same values as the serde parse
        assert_eq!(LoggingConfig::default().level, cfg.logging.level);
        assert_eq!(
            IndexLoggingConfig::default().enabled,
            cfg.index_logging.enabled
        );
    }

    /// Regression: a FULLY valid config (all 10 sections, all documented keys)
    /// must trigger NO unknown-key warning — in particular
    /// `thresholds.ddl_timeout_seconds`, which was missing from `KNOWN_KEYS`
    /// and produced a false warning on every startup with the shipped config.
    #[test]
    fn test_known_keys_cover_full_config() {
        let yaml = r#"
postgresql:
  host: localhost
  port: 5432
  user: pgindexai
  database: pgindexai_demo
  ssl_mode: disable
  password: ""
  ssl_root_cert: /etc/ssl/certs/pg-ca.pem
laya:
  url: http://127.0.0.1:8080
  timeout_seconds: 30
actions:
  create_index: true
  drop_unused: true
  rebuild_fragmented: true
thresholds:
  cycle_seconds: 60
  unused_minutes: 10080
  fragmentation_pct: 30
  use_index_bloat: true
  use_gin_gist_detection: true
  min_sargable_rows: 1
  min_confidence: 0.7
  max_proposals_per_cycle: 20
  ddl_timeout_seconds: 1800
analysis:
  schemas: []
pg_pool:
  max_size: 8
  wait_timeout_ms: 5000
logging:
  level: info
  format: json
index_logging:
  enabled: true
  verbosity: standard
  log_file: /var/log/pgindexai/operations.log
  max_file_size_bytes: 10485760
  max_files: 5
density_by_type:
  int4: 250
  int8: 200
  float8: 150
  varchar: 50
  text: 40
  date: 150
  timestamptz: 150
  uuid: 50
  bool: 10
  default_density: 50
  gin_size_factor: 3
  gist_size_factor: 2
"#;
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        assert!(
            unknown_key_warnings(&value).is_empty(),
            "the shipped config must trigger no unknown-key warning"
        );

        // Detection still works: a typo'd key AND an unknown section are both flagged
        let bad: serde_yaml::Value = serde_yaml::from_str(
            r#"
thresholds:
  ddl_timeout_secondz: 1800
dashboard:
  enabled: true
"#,
        )
        .unwrap();
        let warnings = unknown_key_warnings(&bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("thresholds.ddl_timeout_secondz")),
            "typo'd key must be detected: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("section 'dashboard' unknown")),
            "unknown section must be detected: {warnings:?}"
        );
    }

    /// Regression (v0.1.1): `logging.level` and `index_logging.verbosity` are
    /// validated. Before the fix, a bad `level` PANICKED at startup
    /// (`EnvFilter::new`) and a typo'd `verbosity` was silently swallowed
    /// (treated as Minimal) instead of failing fast.
    #[test]
    fn test_invalid_level_and_verbosity_are_rejected() {
        let config_with = |level: &str, verbosity: &str| -> Config {
            let yaml = format!(
                r#"
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
thresholds:
  cycle_seconds: 60
logging:
  level: {level}
index_logging:
  verbosity: {verbosity}
"#
            );
            let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
            serde_yaml::from_value(value).unwrap()
        };

        // Valid values: accepted
        let cfg = config_with("info", "standard");
        assert!(cfg.validate().is_ok());

        // Invalid level (used to panic in EnvFilter::new at startup)
        let cfg = config_with("verbose!", "standard");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("logging.level"),
            "invalid level must be rejected, got: {err}"
        );

        // Invalid verbosity (used to be silently treated as Minimal)
        let cfg = config_with("info", "verbos");
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("index_logging.verbosity"),
            "invalid verbosity must be rejected, got: {err}"
        );
    }
}
