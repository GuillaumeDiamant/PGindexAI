# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-12

### Added

- **Vacuum-resistant fragmentation detection (physical bloat criterion)**: when
  the optional `pgstattuple` extension is installed, detector #2 now also
  measures the actual B-tree leaf page fill (`pgstatindex()`, one full index
  scan per index above 100 pages — cost guard) and maps it to a score with
  empirically calibrated anchors (healthy ≈ 90 % fill → 0, worst case ≈ 45 % → 1,
  linear in between). Unlike the catalog statistics, this is **not reset by
  VACUUM/ANALYZE/autovacuum** — a bloated index is now detected even after
  maintenance has reclaimed the dead tuples.
- **`thresholds.use_index_bloat`** config knob (default `true`): set to `false`
  to restore the previous behavior (legacy heuristic only).
- **Startup check for `pgstattuple`** — warn-only (absence does not degrade the
  daemon; detector #2 falls back to the legacy heuristic, and a probe failure
  disables only the physical criterion for the current cycle).
- `sql/demo_schema.sql` now also runs `CREATE EXTENSION IF NOT EXISTS pgstattuple`,
  so detector #2's physical criterion works out of the box in the demo.
- **GIN/GIST index-need detection (detector #3)**: the SARGable detector now
  recognizes the containment / key-existence / full-text / overlap / range /
  spatial operators and resolves the index access method from the
  **(operator, real column type)** pair via `resolve_index_method`:
    - **GIN**: `jsonb @> / ? / ?& / ?| / @?`, `hstore @> / ?`, array `@> / &&`,
      `tsvector @@`
    - **GIST**: geometry/geography `@@ / &&` (spatial), range types (`int4range`,
      `tsrange`, …) `@>`
  The generated DDL emits `USING gin` / `USING gist` (B-tree DDL is unchanged —
  no `USING` clause). The column's **real type** is read with
  `format_type(atttypid, atttypmod)` (`information_schema.data_type` is too lossy
  for arrays / user types — it reports `ARRAY` / `USER-DEFINED`).
- **`thresholds.use_gin_gist_detection`** config knob (default `true`): set to
  `false` to restrict detector #3 to B-tree proposals (pre-GIN/GIST behavior).
- **`density_by_type.gin_size_factor` (default `3.0`) and `.gist_size_factor`
  (default `2.0`)**: GIN/GIST indexes are typically larger than a B-tree, so the
  size estimate is multiplied by these factors (validated > 0 and finite).
- **`index_method` made explicit** in the structured audit log
  (`IndexOperationLog.index_method`: `btree` / `gin` / `gist`, set at every CREATE
  site) and in the proposal (`Proposal.index_method`, `#[serde(default)]` for
  backward compatibility) — the audit trail now states which kind of index was built.

### Fixed

- **Detector #2 was blind to bloat after VACUUM/ANALYZE**: the legacy score is
  built from `n_dead_tup` / `n_mod_since_analyze`, which autovacuum resets — a
  physically bloated index (half-empty leaf pages) scored 0. The final score is
  now `max(legacy, physical)`, with the legacy heuristic untouched as the
  fallback (no regression when `pgstattuple` is absent).
- `FragmentationInfo.leaf_density` was an **estimate** derived from
  `pg_class.reltuples/relpages` (unreliable — e.g. multi-TID leaf items make it
  look like > 100 % "density"); it is now the **measured** leaf fill (0..1
  fraction), or `None` when the probe is unavailable.
- **Detector #3 (SARGable) could be blinded by the daemon's own queries**: the
  `pg_stat_statements` filter that excludes the daemon's internal catalog
  queries used `\b` as a "word boundary" — but in PostgreSQL's regex dialect
  `\b` is a **literal backspace**, so the filter never matched. The daemon's
  own high-call queries then dominated the `calls × time` ranking window and
  could push real user queries out of it (demo: 0 `CREATE INDEX` proposals
  instead of 4 — and it worsens the longer the daemon runs). Fixed with the
  correct ARE syntax (`\y` word boundary + `[\s\S]` to span newlines),
  extended to `information_schema`, plus a safety-net skip of `pg_catalog` /
  `information_schema` / `pg_toast` tables in the proposal loop and a live
  regression test.
- **`SET statement_timeout` / `SET lock_timeout` were syntax errors** — the DDL
  path built `SET statement_timeout = {n}s`, `SET LOCAL statement_timeout = {n}s`
  and `SET LOCAL lock_timeout = {n}s` **without quotes**, which PostgreSQL
  rejects (`trailing junk after numeric literal`). This broke **every** real
  index create / drop / reindex with `ddl_timeout_seconds > 0` — all prior runs
  were dry-run / Laya-unavailable, so it was never hit. Now emitted as `'{n}s'`.
  The `test_transactional_path_applies_statement_timeout` regression test now
  genuinely verifies the timeout (it had been passing for the wrong reason — the
  SET syntax error, not the timeout itself).
- **`CREATE INDEX` DDL was invalid** — `build_create_index_ddl` emitted a
  schema-qualified index name (`CREATE INDEX … public.idx ON public.tbl (col)`),
  but PostgreSQL forbids a schema prefix on the index in `CREATE INDEX`
  ("No schema name can be included here; the index is always created in the same
  schema as its parent table"). Every real `CREATE INDEX` failed with
  `syntax error at or near "."`. The index name is now unqualified (the table
  name stays schema-qualified, which is what places the index in the intended
  schema). Surfaced by the new `test_execute_create_index_audit_log_states_method`
  end-to-end test (the DDL was previously only string-tested, never executed).
- **`sanitize_reason_field` stripped array brackets** — the allowed-character
  filter removed `[`/`]`, so an array column type displayed as `text` instead of
  `text[]` in the proposal `reason`. Brackets are now preserved (matters for GIN
  detection on arrays).

### Changed

- `sql/README.md` — "Triggering the fragmentation detector" rewritten: honest
  documentation of the legacy criterion's limitation (counters reset by
  VACUUM/ANALYZE), the new scoring, the vacuum-proof trigger recipe (churn →
  VACUUM → 4 verified `REINDEX` proposals), the autovacuum caveat, the
  pgstattuple prerequisite and the `use_index_bloat` knob.
- `README.md` — fragmentation detector section and configuration reference
  updated to match.
- `README.md` — detector #3 section: rewritten to document the new **GIN/GIST
  support** — GIN (jsonb / hstore / array / tsvector) and GIST (geometry /
  range) operator patterns, `resolve_index_method`, the `USING gin|gist` DDL,
  the `use_gin_gist_detection` toggle and the honest limitations (operator- not
  function-driven; unknown type → GIN best-effort; GIN/GIST bloat not covered by
  detector #2). Replaces the prior "B-tree-only scope" note.
- `docs/architecture/01-system-overview.md` — behavioral-guarantees table
  updated: detector #2 remains B-tree-only (technical — `pgstatindex()` is
  B-tree-only) while detector #3 now covers B-tree + GIN + GIST.
- `docs/architecture/03-module-reference.md`, `04-protocols.md`,
  `05-configuration.md` — signatures (the `build_create_index_ddl` method
  parameter, `resolve_index_method`, the `index_method` fields), the SARGable
  patterns, and the new config keys (`use_gin_gist_detection`,
  `gin_size_factor`, `gist_size_factor`).
- `sql/demo_workload.sql` — added a `SET pg_stat_statements.track = 'all'` block
  (required: with the default `track = top`, the workload's `EXECUTE … USING`
  queries inside PL/pgSQL DO blocks are not recorded per-query, so detector #3
  would see nothing — requires superuser, which the demo runs as) and a
  **Phase 8 GIN/GIST workload** (events.payload `@>` ×2000, events.tags `&&`
  ×1500, bookings.period `@>` ×1500) alongside the existing Phase 7 B-tree
  SARGable workload.
- `sql/demo_schema.sql` — added `events` (jsonb `payload` + `text[]` `tags`, no
  index) and `bookings` (int4range `period`, no index) tables as GIN/GIST
  candidates.
- `sql/README.md` — added the GIN/GIST trigger recipe (Phase 7 + 8 workload), the
  verified `track = 'all'` note, and the verified expected output (GIN for
  events.payload + events.tags, GIST for bookings.period, B-tree for the scalar
  columns).

## [0.1.2] - 2026-11-04

### Added

- **Testing guide**: `sql/README.md` — how to test PGindexAI end-to-end with the demo
  (prerequisites, quick start, dry-run / real run / daemon, verified expected output,
  DDL verification queries, fragmentation-detector trigger recipe, reset, troubleshooting)
- **Test configuration**: `sql/pgindexai.test.yaml` — peer-auth `postgres` user (no
  password), lowered thresholds, index-op log disabled: a fast local test with no secret

### Fixed

- **`sql/demo_schema.sql`**: summary block now uses the correct
  `pg_stat_user_tables.relname` column (was `tablename` — aborted the load with
  `column "tablename" does not exist`)
- **`sql/demo_schema.sql`**: summary block now uses the correct
  `pg_stat_user_indexes.indexrelname` column (was `indexname`)
- **`sql/demo_schema.sql`**: now runs `CREATE EXTENSION IF NOT EXISTS pg_stat_statements`
  itself — previously the extension only existed when `install.sh` had created it, and
  was lost after any drop/reload of the demo database (SARGable detection silently off)
- **`sql/demo_workload.sql`**: fragmentation report no longer fails with
  `function pg_size_pretty(integer) is not unique` (explicit `::bigint` cast)

## [0.1.1] - 2026-11-04

### Fixed

- **DDL timeout now applies to the transactional path** (`DROP INDEX` / non-concurrent
  `REINDEX`): previously `thresholds.ddl_timeout_seconds` was only applied to
  `CONCURRENTLY` builds, so a `DROP INDEX` blocked on a lock held by a long-running
  transaction could hang the daemon forever. Now `SET LOCAL statement_timeout` and
  `SET LOCAL lock_timeout` bound the DDL and its lock waits inside the transaction
  (transaction-scoped, so the pooled connection stays clean).
- **Config value validation**: `logging.level` and `index_logging.verbosity` are now
  validated at load time. A bad `level` used to panic at startup (`EnvFilter::new`),
  and a typo'd `verbosity` was silently treated as `Minimal` — both now fail fast with
  a clear `ConfigError`.
- **Laya criteria order is now deterministic**: `ActionQuestion.criteria` is a
  `BTreeMap` (was `HashMap`), so the serialized JSON criteria keys are always emitted
  in sorted order ("0" < "1" < "2" < …) instead of an arbitrary hash-seeded order.

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
