# Testing PGindexAI with the demo schema & workload

This directory contains a **self-contained demo** to exercise all three
detectors of PGindexAI against a realistic dataset — no production database
required. Everything here is safe to run repeatedly: it creates its own
`pgindexai_demo` database and never touches other data.

## Files in this directory

| File               | What it does |
|--------------------|--------------|
| `demo_schema.sql`  | Creates the `pgindexai_demo` database, **5 tables** (≈ 772k rows) and **23 indexes**, and enables `pg_stat_statements`. |
| `demo_workload.sql`| **8 phases** of realistic queries + heavy DML that put the 3 detectors in a position to find something. |
| `pgindexai.test.yaml` | Ready-to-use test config (peer-auth `postgres` user, lowered thresholds, no log file). |

## What the demo lets you verify

| # | Detector | What the demo makes it find | Action the daemon proposes |
|---|----------|------------------------------|----------------------------|
| 1 | **Unused indexes** | Indexes never scanned since the counters' reset (`idx_scan = 0`), e.g. `idx_products_stock`, `idx_customers_city` | `DROP INDEX IF EXISTS …` |
| 2 | **Fragmented indexes** | B-tree indexes on `orders`/`order_items` after heavy DML *(timing-dependent — see below)* | `REINDEX INDEX CONCURRENTLY …` |
| 3 | **Unindexed SARGable queries** | High-frequency filterable columns with no covering index, e.g. `orders.shipping_method`, `users.is_active` | `CREATE INDEX …` |

> **No-fallback guarantee:** the daemon *detects, generates and logs* proposals
> even with no Laya server available — but it only **applies** DDL when Laya is
> reachable and returns a valid decision. Without Laya you are in **observation
> mode** (nothing is modified). That makes the whole flow safe to run anywhere.

---

## Prerequisites

- **PostgreSQL 16+** running and reachable.
- **`pg_stat_statements` enabled**: `shared_preload_libraries` must include it and
  PostgreSQL must have been restarted since. `install.sh` does this
  automatically; otherwise do it manually (see
  [Manual setup: pg_stat_statements](#pg_stat_statements) at the end of this
  file).
  - Required for detector **#3**. Without it the daemon runs in **degraded**
    mode (detectors #1 and #2 still work).
- **The binary**: build it (`cargo build --release`) or use the installed
  `pgindexai` (`install.sh` places it at `/usr/local/bin/pgindexai`).
- **(Optional) Laya server** on a local port — **required only to apply DDL**.
  Without it the daemon stays in observation mode (proposals are still printed).

---

## Quick start (safe: analysis only, no Laya, no DDL)

```bash
# 1. Load the demo schema (creates pgindexai_demo + data + pg_stat_statements)
psql -U postgres -v ON_ERROR_STOP=1 -f sql/demo_schema.sql

# 2. Run the workload (8 phases — a few minutes)
psql -U postgres -d pgindexai_demo -v ON_ERROR_STOP=1 -f sql/demo_workload.sql

# 3. Analyze and PRINT proposals only (no side effects)
pgindexai run --dry-run -c sql/pgindexai.test.yaml
```

If step 3 prints `DROP INDEX` and `CREATE INDEX` proposals, detectors **#1 and #3
are working** — the test is a success. (Detector #2, fragmentation, is
timing-dependent and may legitimately not fire right after a fresh load — see
[Triggering the fragmentation detector](#triggering-the-fragmentation-detector).)
In all cases you have **not** modified the database.

---

## Step 1 — Load the demo schema

```bash
psql -U postgres -v ON_ERROR_STOP=1 -f sql/demo_schema.sql
```

This:
- creates the `pgindexai_demo` database (skips creation if it already exists),
- enables `pg_stat_statements` (no-op if already present),
- creates **5 tables** and loads **≈ 772k rows**,
- creates **23 indexes** (several intentionally left unused),
- runs `ANALYZE` and prints a summary.

Expected summary (tail of the output):

```
NOTICE:  === pgindexai_demo demo schema ===
NOTICE:    customers: 10000 rows
NOTICE:    order_items: 500000 rows
NOTICE:    orders: 200000 rows
NOTICE:    products: 5000 rows
NOTICE:    users: 2000 rows
NOTICE:  === Index ===
NOTICE:    customers_email_key: 680 kB
NOTICE:    idx_products_stock: 88 kB
…  (23 indexes total)
```

## Step 2 — Run the workload

```bash
psql -U postgres -d pgindexai_demo -v ON_ERROR_STOP=1 -f sql/demo_workload.sql
```

Takes a few minutes (≈ 17,000 query iterations + `VACUUM FULL`). The phases:

| Phase | What it does | Feeds detector |
|-------|--------------|----------------|
| 0 | Baseline queries across all tables (some on unindexed columns) | #3 (SARGable) |
| 1–5 | Realistic search / report / join workloads (use existing indexes) | keeps some indexes "used" |
| 6 | **Heavy DML**: mass DELETE + INSERT + UPDATE, then `VACUUM FULL`, plus a partial index on `orders(status)` (a *candidate* unused index — the phase 7 `status` filters may still use it) | #1 (unused) + #2 (fragmentation) |
| 7 | **Repetitive SARGable queries** (`EXECUTE … USING`) to build up `pg_stat_statements.calls` above the threshold | #3 (SARGable) |

The file ends with a final `ANALYZE` and four diagnostic reports (index usage /
scans = 0, fragmentation density, top queries by exec time, unused indexes) so
you can see the raw state before running the daemon.

## Step 3 — Run the daemon

### 3a. Dry-run (recommended first — no Laya, no DDL)

```bash
pgindexai run --dry-run -c sql/pgindexai.test.yaml
```

Prints every proposal and then stops: `DRY-RUN: skipping Laya + DDL — N
proposal(s) NOT applied`. **Nothing is modified.** This is the fastest way to
confirm the detectors fire.

### 3b. Real run (requires Laya to apply DDL)

Start Laya first (see `docs/architecture/06-deployment.md` § 2), then:

```bash
pgindexai run -c sql/pgindexai.test.yaml
```

The daemon now: generates proposals → asks Laya for each one → applies the
approved DDL (online `CONCURRENTLY` builds / transactional `DROP INDEX`) → logs
each operation. If Laya is unreachable, every proposal is **skipped** (no
fallback) and the database is left untouched.

### 3c. Daemon mode (continuous)

```bash
pgindexai daemon -c sql/pgindexai.test.yaml
```

Runs a cycle every `thresholds.cycle_seconds`, until `Ctrl-C` / `SIGTERM`
(graceful shutdown). The test config uses `logging.format: "text"`, so the
output is plain log lines you can read directly; if you switch to
`format: "json"`, you can then filter with e.g.
`jq -r 'select(.level=="INFO" or .level=="WARN") | .message'`.

---

## What to expect (verified output)

On a freshly loaded demo, `pgindexai run --dry-run` produces **7 or 8 proposals**
(3 `CREATE` + 4 or 5 `DROP`). Both variants are verified; the difference is
`idx_items_date`, see below. Example of the 8-proposal run:

```
INFO  Analysis complete: 8 proposals generated
INFO  Proposal: [DROP INDEX]   idx_items_date      on public.order_items  (1761280 bytes)
INFO  Proposal: [DROP INDEX]   idx_customers_email on public.customers   ( 696320 bytes)
INFO  Proposal: [DROP INDEX]   idx_products_sku    on public.products    ( 180224 bytes)
INFO  Proposal: [DROP INDEX]   idx_customers_city  on public.customers   (  98304 bytes)
INFO  Proposal: [DROP INDEX]   idx_products_stock  on public.products    (  90112 bytes)
INFO  Proposal: [CREATE INDEX] SARGable public.orders.shipping_method (1000 calls, varchar)
INFO  Proposal: [CREATE INDEX] SARGable public.users.is_active        ( 800 calls, boolean)
INFO  Proposal: [CREATE INDEX] SARGable public.products.is_active     ( 800 calls, boolean)
INFO  DRY-RUN: skipping Laya + DDL — 8 proposal(s) NOT applied
```

Reading it:
- **4–5 × `DROP INDEX`** → detector #1 found the indexes never scanned. The
  five DROP candidates are `idx_items_date`, `idx_customers_email`,
  `idx_products_sku`, `idx_customers_city`, `idx_products_stock` — but the
  phase 6 mass DML (`DELETE FROM order_items WHERE order_id < 100000`, 40 % of
  the rows) is a candidate for a **full index scan** on any `order_items`
  index. When the planner picks `idx_items_date` for it, that index is *used*
  and drops out of the list (verified: 3 scans / 250 004 tuples in one run →
  4 `DROP`s total; 0 scans in another run → 5 `DROP`s). Both outcomes are
  correct daemon behavior.
- **3 × `CREATE INDEX`** → detector #3 found three high-frequency filterable
  columns with no covering index. `orders.shipping_method` is the intended
  "missing index" target; `users.is_active` / `products.is_active` are
  legitimate low-selectivity booleans the detector also surfaces. (The
  workload also prepares `orders.total_amount` as a second missing-index
  target — phase 7e; whether it makes the top-N candidate set depends on the
  `calls × mean_exec_time` ranking, so the exact set can vary.)
- **`REINDEX`** (detector #2) may be **absent** right after a fresh load — see
  [Triggering the fragmentation detector](#triggering-the-fragmentation-detector).

## Verifying the DDL was actually applied (after a non-dry run)

```bash
psql -U postgres -d pgindexai_demo <<'SQL'
-- 1. The unused indexes should be GONE (if Laya approved the DROPs):
SELECT indexname FROM pg_indexes WHERE schemaname='public'
  AND indexname IN ('idx_products_stock','idx_customers_city','idx_customers_email',
                    'idx_products_sku','idx_items_date')
ORDER BY 1;
-- (empty result = they were dropped)

-- 2. The new SARGable indexes should EXIST (if Laya approved the CREATEs):
SELECT indexname, indexdef FROM pg_indexes WHERE schemaname='public'
  AND indexname LIKE 'idx_orders_shipping_method%'
  OR  indexname LIKE 'idx_%is_active%'
ORDER BY 1;

-- 3. The daemon's own audit trail (JSONL), if index_logging is enabled:
--    tail -n 20 /var/log/pgindexai/index_operations.log
SQL
```

---

## Triggering the fragmentation detector

Detector #2 scores fragmentation from **table** statistics
(`n_dead_tup`, `n_mod_since_analyze`). The workload runs `VACUUM FULL` (phase 6)
and ends with `ANALYZE`, which **leaves those counters at ~0** — so a freshly
loaded demo usually does **not** cross the fragmentation threshold. That is
expected, not a bug.

To make the daemon propose a `REINDEX`, generate post-analyze churn on a big
table and then run the cycle (verified — produces 4 `REINDEX` proposals):

```bash
# create churn: delete ~half of order_items (dead tuples + modifications
# since the last ANALYZE)
psql -U postgres -d pgindexai_demo -c "DELETE FROM order_items WHERE id % 2 = 0;"

# do NOT VACUUM/ANALYZE — the counters must stay high
pgindexai run --dry-run -c sql/pgindexai.test.yaml | grep -i 'REINDEX'
```

Verified output:

```
INFO  Proposal: [REINDEX] Index public.order_items_pkey fragmentation at 75.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_order  fragmentation at 55.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_product fragmentation at 55.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_date    fragmentation at 75.0% (threshold: 50%).
```

How the score is built (50 % dead-tuple ratio + 30 % modifications-since-analyze
+ a 20 % bonus for large never-scanned indexes), with the test config's
`fragmentation_pct: 50`: the `order_items` indexes cross the line — `idx_items_date`
and the pkey even get the bonus.

---

## Resetting the demo (start over)

The demo is idempotent for the tables it drops, but `customers` is **not** in
that list — so the clean way to reset is to drop the database and reload:

```bash
# 1. Disconnect any session holding the database (required, else DROP is blocked)
psql -U postgres -tAc "SELECT pg_terminate_backend(pid)
  FROM pg_stat_activity WHERE datname='pgindexai_demo' AND pid<>pg_backend_pid();"

# 2. Drop and reload from scratch
psql -U postgres -c "DROP DATABASE pgindexai_demo;"
psql -U postgres -v ON_ERROR_STOP=1 -f sql/demo_schema.sql
psql -U postgres -d pgindexai_demo -v ON_ERROR_STOP=1 -f sql/demo_workload.sql
```

> Note: `DROP DATABASE` fails with `database … is being accessed by other
> users` while any session is connected — step 1 clears that.

---

## Test configuration

A ready-to-use config is provided at **`sql/pgindexai.test.yaml`**. It is tuned
for a fast, local, no-secret test:

- connects as **`postgres`** (peer auth, **no password** needed) — simplest for
  a local test; the daemon and the demo objects are then both owned by the same
  role, so DDL permissions always work;
- lowers the thresholds so the detectors fire quickly
  (`unused_minutes: 1`, `fragmentation_pct: 50`);
- disables the index-operation log file (no `/var/log` write needed).

To test with the **dedicated `pgindexai` role** instead (closer to production),
point `postgresql.user` at `pgindexai`, set its `password`, and make sure that
role **owns** the demo objects (transfer ownership, as `install.sh` does):

```sql
ALTER DATABASE pgindexai_demo OWNER TO pgindexai;
GRANT ALL ON SCHEMA public TO pgindexai;
```

<details>
<summary>Reference: the test config (`sql/pgindexai.test.yaml`)</summary>

```yaml
postgresql:
  host: "/var/run/postgresql"   # Unix socket (or 127.0.0.1 for TCP)
  port: 5432
  user: "postgres"              # peer auth, no password — easiest for a local test
  database: "pgindexai_demo"
  password: ""
  ssl_mode: "disable"           # socket ⇒ disable

laya:
  url: "http://127.0.0.1:8080"
  timeout_seconds: 5

actions:
  create_index: true
  drop_unused: true
  rebuild_fragmented: true

thresholds:
  cycle_seconds: 30
  unused_minutes: 1             # LOW for the test (default 10080 = 7 days)
  fragmentation_pct: 50         # LOWER for the test (default 70)
  min_sargable_rows: 100
  min_confidence: 0.7
  max_proposals_per_cycle: 20
  ddl_timeout_seconds: 60

analysis:
  schemas: []                   # all non-system schemas

pg_pool:
  max_size: 8
  wait_timeout_ms: 5000

logging:
  level: "info"
  format: "text"                # text for easy reading (json in production)

index_logging:
  enabled: false                # no /var/log write needed for a test
  verbosity: "verbose"
  log_file: ""
  max_file_size_bytes: 0
  max_files: 1
```
</details>

---

## Troubleshooting / gotchas

### `password authentication failed for user "pgindexai"`
The shipped config uses the dedicated `pgindexai` role, which authenticates
with `scram-sha-256` (a password is required). For a quick local test, use the
`postgres` role (peer auth, no password) via the test config — or set a
password for `pgindexai` and put it in `postgresql.password`.

### No `DROP INDEX` proposals (detector #1 silent)
Detector #1 has an **observation-window gate**: it only runs when the server
has been up for at least `thresholds.unused_minutes` (default **7 days**),
because `pg_stat_*` counters reset on restart. If your server was just
restarted, the gate suppresses it (to avoid false-positive DROPs). For testing,
set `unused_minutes: 1` (as the test config does).

### No `REINDEX` proposals (detector #2 silent)
See [Triggering the fragmentation detector](#triggering-the-fragmentation-detector)
— it is intentionally timing-dependent.

### No `CREATE INDEX` proposals (detector #3 silent)
- **`pg_stat_statements` missing** → the daemon is in **degraded** mode and
  detector #3 is disabled. Confirm: `SELECT extname FROM pg_extension WHERE
  extname='pg_stat_statements';` and `SHOW shared_preload_libraries;`.
- **Not enough calls** → raise the workload repetitions or lower
  `thresholds.min_sargable_rows`.
- The daemon reads `pg_stat_statements` **across databases** (it does not filter
  by `dbid`), so entries recorded in a previous `pgindexai_demo` still count —
  this is why the demo works even right after a reload.

### `Laya deep health FAILED … Decisions will be SKIPPED (no fallback)`
Expected when Laya is not running. The daemon still **prints** all proposals
(especially in `--dry-run`); it just won't **apply** them. Start Laya to apply.

### `relation "pg_stat_statements" does not exist`
The extension isn't enabled: add `pg_stat_statements` to
`shared_preload_libraries`, restart PostgreSQL, then
`CREATE EXTENSION IF NOT EXISTS pg_stat_statements;` in the target database.
(`demo_schema.sql` already runs the `CREATE EXTENSION`; the prerequisite is the
`shared_preload_libraries` entry + restart.)

### `DROP DATABASE` → `is being accessed by other users`
Terminate the sessions first (see [Resetting the demo](#resetting-the-demo-start-over)).

---

<a id="pg_stat_statements"></a>
## Manual setup: pg_stat_statements (if not using `install.sh`)

```bash
# 1. Check what is already loaded — you must MERGE, not overwrite:
sudo -u postgres psql -c "SHOW shared_preload_libraries;"

# 2. Add pg_stat_statements to the FULL list (existing entries + the new one):
#    edit postgresql.conf:
#        shared_preload_libraries = 'existing_lib1, existing_lib2, pg_stat_statements'
#    (or, with the complete list:
#     sudo -u postgres psql -c "ALTER SYSTEM SET shared_preload_libraries='existing_libs, pg_stat_statements';")

# 3. Restart PostgreSQL (required — shared_preload_libraries is read at startup)
sudo systemctl restart postgresql

# 4. Create the extension in the demo database
sudo -u postgres psql -d pgindexai_demo -c "CREATE EXTENSION IF NOT EXISTS pg_stat_statements;"
```

---

## Related

- [`docs/architecture/06-deployment.md`](../docs/architecture/06-deployment.md) — full deployment, monitoring & troubleshooting
- [`README.md`](../README.md) — overview, detectors, configuration
- [`CHANGELOG.md`](../CHANGELOG.md) — version history
