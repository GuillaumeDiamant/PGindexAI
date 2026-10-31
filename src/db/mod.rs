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

/// Construit le connecteur TLS (rustls) :
/// - racines web PKI (Mozilla) par défaut,
/// - + `ssl_root_cert` (PEM) si fourni (CA auto-signée, snakeoil, …),
/// - sans certificat client (l'auth PG passe par le mot de passe / peer).
fn build_tls_connector(
    cfg: &PgConfig,
) -> Result<MakeRustlsConnect, Box<dyn std::error::Error + Sync + Send>> {
    // rustls 0.23 : un provider crypto par défaut est requis (ring).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if let Some(path) = &cfg.ssl_root_cert {
        let file = std::fs::File::open(path)
            .map_err(|e| ssl_error(format!("ssl_root_cert : impossible d'ouvrir {path} : {e}")))?;
        let mut reader = BufReader::new(file);
        let certs = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ssl_error(format!("ssl_root_cert : PEM invalide dans {path} : {e}")))?;
        if certs.is_empty() {
            return Err(ssl_error(format!(
                "ssl_root_cert : aucun certificat trouvé dans {path}"
            )));
        }
        root_store.add_parsable_certificates(certs.iter().cloned());
        info!("ssl_root_cert : certificat(s) CA chargé(s) depuis {path}");
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(tls_config))
}

/// Crée un pool de connexions PostgreSQL avec TLS, taille et timeout configurables.
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

    // Mode TLS selon la config (ssl_mode est validé par Config::validate).
    // Avec rustls, la vérification du certificat (chaîne + nom d'hôte) est TOUJOURS
    // effectuée au handshake : `require` est donc au moins aussi strict que
    // `verify-ca`/`verify-full` (libpq). La différence entre les trois modes
    // réside dans les racines acceptées (web PKI + éventuelle ssl_root_cert).
    let ssl = cfg.ssl_mode.to_ascii_lowercase();
    let ssl_mode = match ssl.as_str() {
        "disable" => SslMode::Disable,
        "require" | "verify-ca" | "verify-full" => SslMode::Require,
        other => {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "ssl_mode '{other}' non supporté (disable | require | verify-ca | verify-full)"
                ),
            )))
        }
    };
    dp_conf.ssl_mode = Some(ssl_mode);

    if ssl_mode != SslMode::Disable {
        // Socket Unix : tokio-postgres refuse le TLS sans nom d'hôte (et le TLS
        // n'a pas de sens sur un socket local IPC) → erreur claire et précoce.
        if cfg.host.starts_with('/') {
            return Err(ssl_error(
                "host est un socket Unix : ssl_mode doit être 'disable' (le TLS se négocie sur TCP avec un nom d'hôte)",
            ));
        }
        // Fail fast si le nom d'hôte est invalide pour le handshake TLS
        validate_hostname(&cfg.host)?;
    }

    // Pool dynamique : taille max + timeout d'attente configurable
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

    // Vérifier la connectivité dès le démarrage
    let client = pool.get().await?;
    client.simple_query("SELECT 1").await?;

    // Information TLS effective sur cette connexion
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
                        "PostgreSQL pool créé ({0}:{1}) — TLS actif",
                        cfg.host, cfg.port
                    );
                } else {
                    warn!(
                        "PostgreSQL pool créé ({0}:{1}) — ssl_mode={ssl} mais la connexion n'est PAS chiffrée !",
                        cfg.host, cfg.port
                    );
                }
            }
            Ok(None) => warn!(
                "PostgreSQL pool créé ({0}:{1}) — pg_stat_ssl indisponible, état TLS non vérifiable",
                cfg.host, cfg.port
            ),
            Err(e) => warn!(
                "PostgreSQL pool créé ({0}:{1}) — vérification pg_stat_ssl en échec : {e}",
                cfg.host, cfg.port
            ),
        }
    } else {
        info!(
            "PostgreSQL pool créé ({0}:{1}) — TLS désactivé (ssl_mode=disable)",
            cfg.host, cfg.port
        );
    }
    Ok(pool)
}

/// Vérifie que le nom d'hôte fourni dans la config est un FQDN valide pour
/// `verify-full` (évite l'échec tardif du handshake avec un message cryptographique).
pub fn validate_hostname(host: &str) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    ServerName::try_from(host.to_string())
        .map(|_| ())
        .map_err(|e| {
            ssl_error(format!(
                "postgresql.host '{host}' : nom d'hôte invalide pour verify-full : {e}"
            ))
        })
}
