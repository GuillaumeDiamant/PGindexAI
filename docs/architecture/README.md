# Architecture documentation — PGindexAI

Complete technical documentation of the PGindexAI daemon: monitoring, analysis, and automation of PostgreSQL index management driven by the Laya decision engine.

> **Architectural rework** — this documentation covers the latest rework
> (10 fixes): **no-fallback** policy, Laya **deep** health check,
> SARGable detection via a **real AST parser** (sqlparser-rs), **online**
> DDL (`CONCURRENTLY`), target density by column type, naming
> convention (63 bytes), sized connection pool, explicit **degraded**
> state, **multi-schema** support, and an **observation window** for unused
> indexes.
>
> **Security hardening (audit)**: **validated** TLS (rustls, chain +
> hostname) with an optional `ssl_root_cert`, dedicated non-superuser PG role,
> hardened installer (`install.sh`) + `uninstall.sh` (`--purge`), `--dry-run`,
> proposal cap per cycle, audit log rotation, config
> validation with warnings, CI (fmt / clippy / test / shellcheck).
> Each file lists its own new features.

## Table of contents

| Doc | Description |
|-----|-------------|
| [01-system-overview](./01-system-overview.md) | Overview, architecture, components, lifecycle |
| [02-component-diagrams](./02-component-diagrams.md) | Component, sequence, data, and flow diagrams |
| [03-module-reference](./03-module-reference.md) | Module-by-module reference (API, types, functions) |
| [04-protocols.md](./04-protocols.md) | Laya `/v1/systemone` protocol, PostgreSQL DMV queries |
| [05-configuration.md](./05-configuration.md) | Full YAML configuration, defaults, environment variables |
| [06-deployment.md](./06-deployment.md) | Installation, systemd service, monitoring, logging |

## Quick view

```
PostgreSQL             Daemon pgindexai              Laya server
──────────             ───────────────              ──────────────
  │                      │                           │
  │── pg_stat_           │                           │
  │   statements         │   (1) deep_health()       │
  │── pg_class           │──────────────────────────▶│  test payload
  │── pg_index           │◀──────────────────────────│  (validated response)
  │── pg_stats           │   FAILED ⇒ NO DDL         │
  │── information_schema │   (no fallback)           │
  │                      │                           │
  │                      │── cycle 60s ──▶           │
  │                      │                           │
  │                      │  (2) HTTP POST /v1/systemone
  │                      │──────────────────────────▶│  (context + choice)
  │                      │                           │── AI decides
  │                      │◀──────────────────────────│  (choice + confidence)
  │                      │                           │
  │                      │── APPLY/IGNORE ──▶        │
  │                      │   online DDL              │
  │◀── CONCURRENTLY /    │   (validation + cleanup
  │   BEGIN-DROP-COMMIT  │    of INVALID indexes)    │
```

## Entry points

| Command | Description |
|----------|-------------|
| `pgindexai daemon` | Continuous loop (default, 60s cycle) |
| `pgindexai run` | Single cycle, then exit |
| `pgindexai run --dry-run` | Analysis and display **only** — no Laya call, no DDL |
| `pgindexai --config /path daemon` | Custom config |

## Prerequisites

- **PostgreSQL 16+** (ideally with `pg_stat_statements` enabled — without it, the daemon enters **degraded** mode: missing-index detection is disabled)
- **Laya server** (`laya`) in server mode on a local port — **required to apply DDL** (no-fallback policy: without an active Laya, the daemon stays in observation mode)
- **Rust toolchain** (to build from source)

## License

MIT — See the [LICENSE](../../LICENSE) file
