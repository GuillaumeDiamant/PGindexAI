# Testing PGindexAI with the demo schema & workload

This directory contains a **self-contained demo** to exercise all three
detectors of PGindexAI against a realistic dataset — no production database
required. Everything here is safe to run repeatedly: it creates its own
`pgindexai_demo` database and never touches other data.

## Files in this directory

| File               | What it does |
|--------------------|--------------|
| `demo_schema.sql`  | Creates the `pgindexai_demo` database, **5 tables** (≈ 772k rows) and **23 indexes**, and enables `pg_stat_statements` + `pgstattuple`. |
| `demo_workload.sql`| **8 phases** of realistic queries + heavy DML that put the 3 detectors in a position to find something. |
| `pgindexai.test.yaml` | Ready-to-use test config (peer-auth `postgres` user, lowered thresholds, no log file). |

## What the demo lets you verify

| # | Detector | What the demo makes it find | Action the daemon proposes |
|---|----------|------------------------------|----------------------------|
| 1 | **Unused indexes** | Indexes never scanned since the counters' reset (`idx_scan = 0`), e.g. `idx_products_stock`, `idx_customers_city` | `DROP INDEX IF EXISTS …` |
| 2 | **Fragmented indexes** | Physically bloated B-trees on `order_items` after churn — leaf fill measured via `pgstattuple`, so it fires **even after VACUUM/ANALYZE** | `REINDEX INDEX CONCURRENTLY …` |
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
- **`pgstattuple`** (optional, recommended): powers the **physical bloat
  criterion** of detector **#2** (measured B-tree leaf fill — the only
  criterion that survives VACUUM/ANALYZE/autovacuum).
  - `demo_schema.sql` creates it in the demo database automatically (no-op if
    already present). No `shared_preload_libraries` entry needed — it is a
    regular contrib extension.
  - Since pgstattuple 1.5, the calling role must be a **superuser** or a
    member of **`pg_stat_scan_tables`** (the test config connects as
    `postgres`, so it works out of the box).
  - Without it, detector #2 degrades gracefully to the legacy heuristic alone
    — see [Triggering the fragmentation detector](#triggering-the-fragmentation-detector).
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
- enables `pg_stat_statements` and `pgstattuple` (no-ops if already present),
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

On a freshly loaded demo, `pgindexai run --dry-run` produces **8 or 9 proposals**
(4 `CREATE` + 4 or 5 `DROP`). Both variants are verified; the difference is
`idx_items_date`, see below. Example of the 8-proposal run:

```
INFO  Analysis complete: 8 proposals generated
INFO  Proposal: [DROP INDEX]   idx_customers_email on public.customers   ( 696320 bytes)
INFO  Proposal: [DROP INDEX]   idx_products_sku    on public.products    ( 180224 bytes)
INFO  Proposal: [DROP INDEX]   idx_customers_city  on public.customers   (  98304 bytes)
INFO  Proposal: [DROP INDEX]   idx_products_stock  on public.products    (  90112 bytes)
INFO  Proposal: [CREATE INDEX] SARGable public.orders.shipping_method (1000 calls, varchar)
INFO  Proposal: [CREATE INDEX] SARGable public.orders.total_amount    ( 800 calls, numeric)
INFO  Proposal: [CREATE INDEX] SARGable public.products.is_active     ( 800 calls, boolean)
INFO  Proposal: [CREATE INDEX] SARGable public.users.is_active        ( 800 calls, boolean)
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
- **4 × `CREATE INDEX`** → detector #3 found four high-frequency filterable
  columns with no covering index. `orders.shipping_method` and
  `orders.total_amount` (phase 7e) are the intended "missing index" targets;
  `users.is_active` / `products.is_active` are legitimate low-selectivity
  booleans the detector also surfaces. (The other SARGable columns of the
  workload — `orders.status`, `products.category`, `users.role`,
  `customers.country` — already have covering indexes in the demo schema and
  are correctly *not* proposed.)
- **`REINDEX`** (detector #2) is **absent** right after a fresh load — the
  indexes are healthy (~90 % leaf fill, score ≈ 0) and the legacy counters are
  ~0 after the final `ANALYZE`. Churn + `VACUUM` to trigger it — see
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

Detector #2 combines two criteria; the **final score is the maximum of the two**
(the strongest signal wins, and the legacy criterion alone is the fallback):

| Criterion | Source | Reset by VACUUM / ANALYZE? |
|-----------|--------|----------------------------|
| **A — catalog statistics** (legacy): dead-tuple ratio (50 %) + modifications-since-analyze (30 %) + large never-scanned bonus (20 %) | `pg_stat_user_tables` | **Yes** — the counters return to ~0, so the score drops to ~0 |
| **B — physical bloat** (optional, `pgstattuple`): measured B-tree leaf page fill | `pgstatindex()` — one full index scan per call, only on indexes > 100 pages (cost guard) | **No** — the free space stays on the leaf pages until a REINDEX |

Criterion B is mapped to a 0–1 score with empirically calibrated anchors
(PostgreSQL 16 / pgstattuple 1.5, measured on real data, 100k–250k-row tables):

- **healthy index ≈ 90 % leaf fill** (measured 89.8 – 90.1 %) → score ≈ 0
- **worst case ≈ 45 %** (50 % of the rows churned, then VACUUM — the index is
  ≈ 2× the size of the healthy one; REINDEX reclaims about half) → score = 1
- in between: linear (e.g. 67.5 % fill → 0.5)

> **Honest limitation of criterion A:** PostgreSQL resets `n_dead_tup` and
> `n_mod_since_analyze` on VACUUM/ANALYZE — and **autovacuum does both in
> production**. A physically bloated index (leaf pages half full) therefore
> scores **0** with the legacy heuristic right after autovacuum ran. Criterion B
> exists precisely to close that blind spot. When `pgstattuple` is missing — or
> `use_index_bloat: false`, or the probe fails — the detector degrades
> gracefully to criterion A alone (the pre-v0.2 behavior; the daemon does not
> degrade, it only warns at startup).

### Recommended recipe (vacuum-proof)

The workload ends with `VACUUM FULL` + `ANALYZE`, so a freshly loaded demo does
**not** cross the fragmentation threshold (expected, not a bug). To make the
daemon propose `REINDEX`es, churn a big table **and let maintenance run** — the
dead tuples are reclaimed (criterion A goes blind) but the leaf pages stay half
empty (criterion B catches them). Verified — produces exactly 4 `REINDEX`
proposals:

```bash
# 1. Churn: delete ~half of order_items
psql -U postgres -d pgindexai_demo -c "DELETE FROM order_items WHERE id % 2 = 0;"

# 2. Let maintenance run — this is the point of the test:
#    criterion A's counters reset, criterion B still sees the bloat
psql -U postgres -d pgindexai_demo -c "VACUUM ANALYZE order_items;"

# 3. Analyze
pgindexai run --dry-run -c sql/pgindexai.test.yaml | grep -i 'REINDEX'
```

Verified output (leaf fill measured by the daemon via pgstattuple):

```
INFO  Proposal: [REINDEX] Index public.order_items_pkey  fragmentation at 99.7% (threshold: 50%). Pages: 689, leaf fill: 45.1% (healthy ~90%).
INFO  Proposal: [REINDEX] Index public.idx_items_order   fragmentation at 84.5% (threshold: 50%). Pages: 550, leaf fill: 52.0% (healthy ~90%).
INFO  Proposal: [REINDEX] Index public.idx_items_product fragmentation at 98.1% (threshold: 50%). Pages: 220, leaf fill: 45.9% (healthy ~90%).
INFO  Proposal: [REINDEX] Index public.idx_items_date    fragmentation at 96.4% (threshold: 50%). Pages: 215, leaf fill: 46.6% (healthy ~90%).
```

> **Autovacuum caveat:** on a real (non-demo) database autovacuum may run
> between your churn and the analysis cycle — that is fine, that is exactly the
> scenario the physical criterion is built for. If you specifically want to test
> the *legacy-only* path (criterion A), stop autovacuum interfering for the
> test: `ALTER TABLE order_items SET (autovacuum_enabled = false);`

### Legacy-only recipe (no `pgstattuple` installed)

Without the extension, criterion A must do the scoring, so its counters must
**not** be reset — churn, and do **not** VACUUM/ANALYZE (and disable autovacuum
for the test, see above):

```bash
psql -U postgres -d pgindexai_demo -c "DELETE FROM order_items WHERE id % 2 = 0;"
# do NOT VACUUM/ANALYZE — the counters must stay high
pgindexai run --dry-run -c sql/pgindexai.test.yaml | grep -i 'REINDEX'
```

Verified output (legacy score = 50 % dead-tuple ratio + 30 %
modifications-since-analyze + the 20 % never-scanned bonus for the pkey and
`idx_items_date`):

```
INFO  Proposal: [REINDEX] Index public.order_items_pkey fragmentation at 75.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_order  fragmentation at 55.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_product fragmentation at 55.0% (threshold: 50%).
INFO  Proposal: [REINDEX] Index public.idx_items_date    fragmentation at 75.0% (threshold: 50%).
```

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
  role, so DDL permissions always work (and `postgres` can call
  `pgstatindex()` — superuser);
- lowers the thresholds so the detectors fire quickly
  (`unused_minutes: 1`, `fragmentation_pct: 50`);
- enables the physical bloat criterion (`use_index_bloat: true`) —
  `demo_schema.sql` installs `pgstattuple` in the demo database;
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
  use_index_bloat: true         # physical bloat via pgstattuple (installed by demo_schema.sql)
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
Two possible causes — see
[Triggering the fragmentation detector](#triggering-the-fragmentation-detector)
for the recipes:
- **`pgstattuple` missing** (or `use_index_bloat: false`) → only the legacy
  heuristic runs, and it is blind right after VACUUM/ANALYZE (counters reset).
  Install the extension: `CREATE EXTENSION IF NOT EXISTS pgstattuple;` (or
  `psql -f sql/demo_schema.sql` again on the demo database).
- **The index is below the 100-page cost guard** — `pgstatindex()` performs a
  full index scan per call, so small indexes are not physically measured (the
  legacy criterion can still score them).

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
