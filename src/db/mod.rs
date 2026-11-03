use deadpool_postgres::{
    Config as DpConfig, ManagerConfig, Pool, PoolConfig as DpPoolConfig, RecyclingMethod, Runtime,
    SslMode,
};
use rustls::pki_types::ServerName;
use std::io::BufReader;
use std::time::Duration;
use tokio_postgres_rustls::MakeRustlsConnect;
use tracing::{info, warn};

use crate::config::{PgConfig, PgPoolConfig};

pub mod queries;

fn ssl_error(msg: impl std::fmt::Display) -> Box<dyn std::error::Error + Sync + Send> {
    Box::new(std::io::Error::other(msg.to_string()))
}

/// Builds the TLS connector (rustls):
/// - web PKI (Mozilla) roots by default,
/// - + `ssl_root_cert` (PEM) if provided (self-signed CA, snakeoil, …),
/// - no client certificate (PG auth goes through password / peer).
fn build_tls_connector(
    cfg: &PgConfig,
) -> Result<MakeRustlsConnect, Box<dyn std::error::Error + Sync + Send>> {
    // rustls 0.23: a default crypto provider is required (ring).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Some(path) = &cfg.ssl_root_cert {
        let file = std::fs::File::open(path)
            .map_err(|e| ssl_error(format!("ssl_root_cert: cannot open {path}: {e}")))?;
        let mut reader = BufReader::new(file);
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ssl_error(format!("ssl_root_cert: invalid PEM in {path}: {e}")))?;
        if certs.is_empty() {
            return Err(ssl_error(format!(
                "ssl_root_cert: no certificate found in {path}"
            )));
        }
        root_store.add_parsable_certificates(certs.iter().cloned());
        info!("ssl_root_cert: CA certificate(s) loaded from {path}");
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(tls_config))
}

/// Creates a PostgreSQL connection pool with TLS, configurable size and timeout.
pub async fn create_pool(
    cfg: &PgConfig,
    pool_cfg: &PgPoolConfig,
) -> Result<Pool, Box<dyn std::error::Error + Sync + Send>> {
    let mut dp_conf = DpConfig::new();
    dp_conf.host = Some(cfg.host.clone());
    dp_conf.port = Some(cfg.port);
    dp_conf.user = Some(cfg.user.clone());
    dp_conf.dbname = Some(cfg.database.clone());
    if let Some(pw) = &cfg.password {
        dp_conf.password = Some(pw.clone());
    }

    // TLS mode per config (ssl_mode is validated by Config::validate).
    // With rustls, certificate verification (chain + hostname) is ALWAYS
    // performed at handshake: `require` is therefore at least as strict as
    // `verify-ca`/`verify-full` (libpq). The difference between the three modes
    // lies in the accepted roots (web PKI + optional ssl_root_cert).
    let ssl = cfg.ssl_mode.to_ascii_lowercase();
    let ssl_mode = match ssl.as_str() {
        "disable" => SslMode::Disable,
        "require" | "verify-ca" | "verify-full" => SslMode::Require,
        other => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "ssl_mode '{other}' not supported (disable | require | verify-ca | verify-full)"
                ),
            )))
        }
    };
    dp_conf.ssl_mode = Some(ssl_mode);

    if ssl_mode != SslMode::Disable {
        // Unix socket: tokio-postgres refuses TLS without a hostname (and TLS
        // makes no sense on a local IPC socket) → clear and early error.
        if cfg.host.starts_with('/') {
            return Err(ssl_error(
                "host is a Unix socket: ssl_mode must be 'disable' (TLS is negotiated over TCP with a hostname)",
            ));
        }
        // Fail fast if the hostname is invalid for the TLS handshake
        validate_hostname(&cfg.host)?;
    }

    // Dynamic pool: max size + wait timeout configurable
    let mut dp_pool_cfg = DpPoolConfig::new(pool_cfg.max_size as usize);
    dp_pool_cfg.timeouts.wait = Some(Duration::from_millis(pool_cfg.wait_timeout_ms));

    let mgr_cfg = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };

    dp_conf.pool = Some(dp_pool_cfg);
    dp_conf.manager = Some(mgr_cfg);

    let connector = build_tls_connector(cfg)?;

    let pool = dp_conf
        .create_pool(Some(Runtime::Tokio1), connector)
        .map_err(|e| format!("Failed to create PostgreSQL pool: {}", e))?;

    // Check connectivity at startup
    let client = pool.get().await?;
    client.simple_query("SELECT 1").await?;

    // Effective TLS information for this connection
    if ssl_mode != SslMode::Disable {
        match client
            .query_opt(
                "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .await
        {
            Ok(Some(row)) => {
                let is_ssl: bool = row.get(0);
                if is_ssl {
                    info!(
                        "PostgreSQL pool created ({0}:{1}) — TLS active",
                        cfg.host, cfg.port
                    );
                } else {
                    warn!(
                        "PostgreSQL pool created ({0}:{1}) — ssl_mode={ssl} but the connection is NOT encrypted!",
                        cfg.host, cfg.port
                    );
                }
            }
            Ok(None) => warn!(
                "PostgreSQL pool created ({0}:{1}) — pg_stat_ssl unavailable, TLS state not verifiable",
                cfg.host, cfg.port
            ),
            Err(e) => warn!(
                "PostgreSQL pool created ({0}:{1}) — pg_stat_ssl check failed: {e}",
                cfg.host, cfg.port
            ),
        }
    } else {
        info!(
            "PostgreSQL pool created ({0}:{1}) — TLS disabled (ssl_mode=disable)",
            cfg.host, cfg.port
        );
    }
    Ok(pool)
}

/// Checks that the hostname provided in the config is a valid FQDN for
/// `verify-full` (avoids a late handshake failure with a cryptic message).
pub fn validate_hostname(host: &str) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    ServerName::try_from(host.to_string())
        .map(|_| ())
        .map_err(|e| {
            ssl_error(format!(
                "postgresql.host '{host}': invalid hostname for verify-full: {e}"
            ))
        })
}
