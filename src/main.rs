#![deny(warnings)]

mod analyzer;
mod config;
mod db;
mod ddl;
mod index_log;
mod laya;
mod naming;
mod state;

use clap::{Parser, Subcommand};
use state::DaemonState;
use std::path::PathBuf;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "pgindexai",
    about = "Real-time PostgreSQL index management driven by Laya decision engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to configuration file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Analyze and print proposals only — no Laya call, no DDL executed
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the daemon (continuous analysis cycle)
    Daemon,
    /// Run a single analysis cycle and exit
    Run,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let cli = Cli::parse();

    // Load the configuration
    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let cfg = config::load(&config_path).map_err(|e| format!("Config error: {}", e))?;

    // Initialize logging
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.logging.level));
    if cfg.logging.format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_ansi(false)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    info!("PGindexAI starting...");

    // Create the PostgreSQL connection pool (TLS per ssl_mode, configurable size)
    let pool = db::create_pool(&cfg.postgresql, &cfg.pg_pool).await?;

    // Check that pg_stat_statements is enabled → daemon state
    let daemon_state = check_pg_stat_statements(&pool).await?;
    if daemon_state == DaemonState::Degraded {
        warn!("Daemon running in DEGRADED mode: pg_stat_statements missing.");
        warn!("Missing-index detection is DISABLED until the extension is installed.");
    }

    // Check that pgstattuple is installed (OPTIONAL extension).
    // Absence does NOT degrade the daemon: the fragmentation detector
    // falls back to the legacy heuristic (which is blind to bloat left
    // behind by VACUUM — see the physical bloat criterion in queries.rs).
    check_pgstattuple(&pool).await;

    // Create the Laya client
    let laya = laya::LayaClient::new(&cfg.laya.url, cfg.laya.timeout_seconds);

    // DEEP health check: an actual test payload, not just GET /health
    match laya.deep_health().await {
        Ok(status) => info!("Laya deep health OK at {} ({})", cfg.laya.url, status),
        Err(e) => {
            warn!("Laya deep health FAILED at {}: {}", cfg.laya.url, e);
            warn!("Decisions will be SKIPPED (no fallback) until Laya is available.");
        }
    }

    if cli.dry_run {
        info!("DRY-RUN mode: proposals will be printed but NOT executed (no Laya, no DDL)");
    }

    match cli.command {
        Command::Daemon => run_daemon(pool, laya, cfg, daemon_state, cli.dry_run).await,
        Command::Run => run_once(pool, laya, cfg, daemon_state, cli.dry_run).await,
    }
}

/// Run a single cycle
async fn run_once(
    pool: deadpool_postgres::Pool,
    laya: laya::LayaClient,
    cfg: config::Config,
    daemon_state: DaemonState,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let analyzer = analyzer::Analyzer::new(pool, laya, cfg, daemon_state);

    // Analyze
    let proposals = match analyzer.analyze_cycle().await {
        Ok(p) => p,
        Err(e) => {
            error!("Analysis error: {e}");
            Vec::new()
        }
    };
    info!("Analysis complete: {} proposals generated", proposals.len());

    for p in &proposals {
        info!("Proposal: [{}] {}", p.action, p.reason);
    }

    if dry_run {
        info!(
            "DRY-RUN: skipping Laya + DDL — {} proposal(s) NOT applied",
            proposals.len()
        );
        return Ok(());
    }

    // Apply the Laya decisions
    let result = analyzer.apply_proposals(&proposals).await?;
    info!(
        "Applied: {} proposal(s), skipped (no-op): {}",
        result.applied.len(),
        result.skipped.len()
    );

    Ok(())
}

/// Daemon mode: continuous analysis loop, graceful shutdown (SIGINT / SIGTERM)
async fn run_daemon(
    pool: deadpool_postgres::Pool,
    laya: laya::LayaClient,
    cfg: config::Config,
    daemon_state: DaemonState,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let cycle_secs = cfg.thresholds.cycle_seconds;

    info!(
        "Daemon started. Cycle: {}s, state: {:?}, dry_run: {}",
        cycle_secs, daemon_state, dry_run
    );

    // The analyzer (and its log writer) is created once, then reused on each cycle
    let analyzer = analyzer::Analyzer::new(pool, laya, cfg, daemon_state);

    let mut shutdown = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        let start = std::time::Instant::now();

        // Analyze
        let proposals = match analyzer.analyze_cycle().await {
            Ok(p) => p,
            Err(e) => {
                error!("Analysis error: {e}");
                Vec::new()
            }
        };

        let duration = start.elapsed().as_millis() as f64;

        info!(
            "Analysis complete: {} proposals ({}ms)",
            proposals.len(),
            duration
        );
        for p in &proposals {
            info!("Proposal: [{}] {}", p.action, p.reason);
        }

        // Apply (unless dry-run)
        if !dry_run {
            match analyzer.apply_proposals(&proposals).await {
                Ok(result) => {
                    info!(
                        "Applied: {} proposal(s), skipped (no-op): {}",
                        result.applied.len(),
                        result.skipped.len()
                    );
                }
                Err(e) => {
                    error!("Apply error: {e}");
                }
            }
        } else {
            info!("DRY-RUN: skipping Laya + DDL this cycle");
        }

        // Wait for the next cycle — interruptible by Ctrl-C or SIGTERM
        let sleep_dur = std::time::Duration::from_secs(cycle_secs);
        tokio::select! {
            _ = tokio::time::sleep(sleep_dur) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received — shutting down gracefully (next cycle skipped)");
                break;
            }
            _ = shutdown.recv() => {
                info!("SIGTERM received — shutting down gracefully (next cycle skipped)");
                break;
            }
        }
    }

    info!("Daemon stopped.");
    Ok(())
}

/// Check that pg_stat_statements is enabled and return the daemon state
async fn check_pg_stat_statements(
    pool: &deadpool_postgres::Pool,
) -> Result<DaemonState, Box<dyn std::error::Error + Sync + Send>> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT 1 FROM pg_extension WHERE extname = 'pg_stat_statements'",
            &[],
        )
        .await?;

    if row.is_none() {
        warn!("pg_stat_statements is NOT installed!");
        warn!("This extension is REQUIRED for missing-index detection.");
        warn!("To fix:");
        warn!("  1. Add to postgresql.conf: shared_preload_libraries = 'pg_stat_statements'");
        warn!("  2. Restart PostgreSQL");
        warn!("  3. Run: CREATE EXTENSION IF NOT EXISTS pg_stat_statements;");
        warn!("Daemon continues in DEGRADED mode: unused/fragmentation detection works,");
        warn!("but missing-index detection is DISABLED (no data source).");
        Ok(DaemonState::Degraded)
    } else {
        info!("pg_stat_statements is active");
        Ok(DaemonState::Healthy)
    }
}

/// Check that pgstattuple is installed (OPTIONAL extension).
/// Absence does NOT change the daemon state: the fragmentation detector
/// degrades gracefully to the legacy heuristic (dead tuples + unanalyzed
/// modifications), which underestimates bloat after VACUUM/ANALYZE.
async fn check_pgstattuple(pool: &deadpool_postgres::Pool) {
    let Ok(client) = pool.get().await else {
        return;
    };
    let Ok(row) = client
        .query_opt(
            "SELECT 1 FROM pg_extension WHERE extname = 'pgstattuple'",
            &[],
        )
        .await
    else {
        return;
    };
    if row.is_none() {
        warn!("pgstattuple is NOT installed: PHYSICAL BLOAT detection is DISABLED.");
        warn!("The fragmentation detector falls back to the legacy heuristic, which is");
        warn!("blind to bloat left behind by VACUUM/ANALYZE (dead-tuple counters are reset).");
        warn!("To enable:  CREATE EXTENSION pgstattuple;");
        warn!("(since pgstattuple 1.5, the connection user must be a SUPERUSER");
        warn!("or a member of pg_stat_scan_tables to call pgstatindex()).");
    } else {
        info!("pgstattuple is active (physical bloat detection enabled)");
    }
}
