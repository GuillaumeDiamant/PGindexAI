# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-03

First tagged release.

### Added

- **Core daemon**: continuous PostgreSQL index monitoring and optimization cycle
  (`daemon` loop, one-shot `run`, `run --dry-run` — analysis only, no Laya call, no DDL)
- **Three detectors**:
  - unused indexes (never scanned since last restart, observation-window gate on server uptime)
  - fragmented B-tree indexes (multi-criteria heuristic score: dead tuples 50 %,
    unanalyzed modifications 30 %, unused-index bonus 20 %)
  - unindexed SARGable queries (real SQL parser — sqlparser AST analysis, no regex false positives)
- **No-fallback policy**: Laya unreachable or invalid response ⇒ no DDL applied
- **Deep health check** of Laya (real test payload to `/v1/systemone`, structurally validated) before each batch
- **Online DDL**: `CREATE/REINDEX … CONCURRENTLY` (auto-commit), `DROP INDEX` in a transaction,
  post-DDL validation + cleanup of INVALID indexes
- **Per-column-type target density** (`density_by_type`) for created-index size estimation
- **Explicit naming convention** `idx_<table>_<col>` with 63-byte limit (short hash)
- **Sized connection pool** (deadpool-postgres) with configurable max size and wait timeout
- **Explicit degraded state**: `pg_stat_statements` absent ⇒ missing-index detection disabled, daemon reported `DEGRADED`
- **Multi-schema analysis scope** (`analysis.schemas`)
- **Proposal cap** per cycle (`max_proposals_per_cycle`)
- **Validated TLS** (rustls) on the PostgreSQL connection — chain + hostname always validated, optional `ssl_root_cert` (PEM)
- **Structured audit log** of index operations (JSONL, verbosity levels, file rotation 0640)
- **Hardened installer** (`install.sh`): dedicated non-superuser role, targeted `pg_hba.conf` line,
  0600 config, hardened systemd unit (`MemoryMax`, `NoNewPrivileges`, `ProtectSystem=strict`), `--dry-run`
- **Idempotent uninstaller** (`uninstall.sh`): service, binary, config, data, logs;
  `--purge` (database + role + pg_hba line, with confirmation), `--clean-legacy` (old laya-sql leftovers), `--dry-run`;
  never touches `shared_preload_libraries` / `pg_stat_statements`
- **Demo schema and workload**: 5 tables, 23 indexes (some intentionally unused), 8 workload phases
- **CI**: fmt, clippy `-D warnings`, tests, shellcheck
- **Architecture documentation** (7 files): system overview, component diagrams, module reference,
  protocols, configuration, deployment

### Changed

- Removed the legacy SQL Server POC (`jit_indexer.py`, `requirements.txt`) — replaced by the Rust daemon
- Translated README and architecture documentation to English

### Fixed

- **Installer**: works under sudo — `TARGET_USER`/`TARGET_HOME` derived from `SUDO_USER`
  (config, unit `User=`, chown), cargo detection outside root's PATH, recent rustup-init flags,
  build as the target user
- **Installer**: `ask()` prompt now visible (the `read` question was swallowed by `2>/dev/null`)
- **Installer**: `pg_hba.conf` line corrected to `local all pgindexai` (the database column was
  never matched), detection/correction of the defective variant, PostgreSQL reload after any hba change
- **Installer**: config regenerated/restored under sudo is chowned to the target user
  (was root:root 0600 → daemon could not read it, EACCES)
- **Installer**: `ssl_mode` always `disable` on a local Unix socket — `require` on a socket
  is rejected by the daemon and caused a crash loop at startup
- **Installer**: daemon DDL path fixed on fresh installs and on pre-existing installations
- **Installer**: password no longer visible in `ps`, DDL timeout + `skipped` status, safer purge
- **Config**: absent sections no longer silently break behavior — robust defaults apply
- **Analyzer**: Laya's standard vs filtered index choice is now honored
