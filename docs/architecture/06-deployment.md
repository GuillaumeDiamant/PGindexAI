# 06 — Deployment and operations

## 1. Installation

### Prerequisites

- **PostgreSQL 16+** (ideally with `pg_stat_statements` enabled — without it, the daemon runs in **degraded** mode)
- **Rust toolchain** (to compile from source)
- **Systemd** (for the service)
- **Laya server** (AI engine — required **to apply DDL**; without it the daemon stays in observation mode)

### Option A — Hardened installer (recommended)

```bash
./install.sh                 # or: ./install.sh --dry-run (full simulation)
```

`install.sh` (shellcheck-clean, no `eval`, prompts via `/dev/tty`):
- detects the PostgreSQL cluster via `SHOW config_file/hba_file/ssl`;
- creates the **dedicated non-superuser role `pgindexai`** (password
  `openssl rand -hex 16`, never displayed);
- **prepends** the targeted line `local all pgindexai scram-sha-256` in
  `pg_hba.conf` (never touches the other lines);
- **merges** `pg_stat_statements` into `shared_preload_libraries` (never
  overrides it) then restarts PostgreSQL if needed;
- installs the binary to `/usr/local/bin/pgindexai`;
- generates `~/.config/PGindexAI/PGindexAI.yaml` with **0600** permissions and the password;
- moves ownership of the demo objects to `pgindexai` (if the database exists);
- deploys the **hardened** systemd unit and enables the service.

### Option B — Manual

```bash
cargo build --release
sudo cp target/release/pgindexai /usr/local/bin/pgindexai

# systemd unit: copy the template `config/pgindexai.service` then replace
# the $USER / $HOME markers with your values (User=, Group=, ExecStart, HOME)
sudo cp config/pgindexai.service /etc/systemd/system/pgindexai.service
sudo systemctl daemon-reload
sudo systemctl enable --now pgindexai

mkdir -p ~/.config/PGindexAI
cp config/PGindexAI.yaml ~/.config/PGindexAI/PGindexAI.yaml
chmod 0600 ~/.config/PGindexAI/PGindexAI.yaml
# Edit ~/.config/PGindexAI/PGindexAI.yaml
```

The deployed unit contains the hardening: `MemoryMax=512M`,
`CPUQuota=100%`, `NoNewPrivileges=true`, `ProtectSystem=strict`,
`PrivateTmp=true`, `Restart=always`.

## 2. Starting the Laya server

```bash
laya \
  --model ~/.local/share/pgindexai/models/typed-decisions/ \
  --cpu \
  --server \
  --port 8080
```

The Laya server **must** be active for the daemon to apply decisions:
**"no-fallback"** policy — if `deep_health()` fails (Laya unreachable,
invalid response), **all** proposals of the cycle are skipped. Without Laya,
the daemon stays in **observation mode**: it detects, generates and logs
proposals, but never modifies the database.

At startup, a **deep health check** is performed (real test payload
sent to `/v1/systemone`, response validated):

```
INFO  Laya deep health OK at http://127.0.0.1:8080 (choice=0, confidence=0.98)
```
or

```
WARN  Laya deep health FAILED at http://127.0.0.1:8080: Laya not reachable …
WARN  Decisions will be SKIPPED (no fallback) until Laya is available.
```

## 3. Initializing the demo database

```bash
# Schema
psql -U postgres -f sql/demo_schema.sql

# Test workload
sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql
```

## 4. Monitoring

### Service status

```bash
systemctl is-active pgindexai
systemctl status pgindexai
```

### Logs

```bash
# systemd journal
journalctl -u pgindexai -f

# Recent logs
journalctl -u pgindexai --since "1 hour ago"

# Operations log file (if configured)
tail -f /var/log/pgindexai/index_operations.log
```

### PostgreSQL checks

```bash
# pg_stat_statements extension
psql -U postgres -d pgindexai_demo -c "SELECT extname, extversion FROM pg_extension WHERE extname = 'pg_stat_statements';"

# Tables and indexes
psql -U postgres -d pgindexai_demo -c "\dt"
psql -U postgres -d pgindexai_demo -c "\di"

# Index statistics
psql -U postgres -d pgindexai_demo -c "
SELECT schemaname, relname AS table_name, indexrelname AS index_name,
       idx_scan, idx_tup_read, pg_size_pretty(pg_relation_size(indexrelid))
FROM pg_stat_user_indexes
ORDER BY pg_relation_size(indexrelid) DESC;"

# Table statistics
psql -U postgres -d pgindexai_demo -c "
SELECT relname AS table_name, n_live_tup, n_dead_tup, n_mod_since_analyze
FROM pg_stat_user_tables
ORDER BY relname;"
```

## 5. Troubleshooting

### Laya unreachable

```
WARN  Laya deep health FAILED at http://127.0.0.1:8080: Laya not reachable …
WARN  Decisions will be SKIPPED (no fallback) until Laya is available.
WARN  Laya unavailable (…): 5 proposals SKIPPED (no fallback policy).
INFO  Applied: 0 proposal(s), skipped (no-op): 0
```

**Behavior**: the daemon **never** decides on behalf of Laya —
all proposals are generated and logged, but **no DDL
is applied** ("no-fallback" policy). As soon as Laya responds, the
daemon automatically resumes decisions on the next cycle (no
restart required).

**Cause**: The Laya server is not running, the port is different, or
it responds with an invalid structure (`choice` not a string, `confidence`
outside `[0, 1]`).

**Solution**:
1. Check that `laya --server --port 8080` is active
2. Check `laya.url` in the config
3. Check the local firewall
4. Test the response: `curl -s localhost:8080/v1/systemone -d '<payload>'`

### pg_stat_statements missing

```
WARN  pg_stat_statements extension not found.
WARN  Daemon running in DEGRADED mode: pg_stat_statements missing.
WARN  Missing-index detection is DISABLED until the extension is installed.
```

**Behavior**: the daemon switches to state **`Degraded`** (see
`03-module-reference.md` § 2) — **missing**-index detection is
disabled (no data source), but the **unused** and **fragmented**
index detectors remain active.

**Cause**: The extension is not installed on the target database.

**Solution**:
```sql
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
```
then restart the daemon (`systemctl restart pgindexai`) to go back to
`Healthy`.

### Configuration error

```
Config error: Cannot parse config at /path/to/PGindexAI.yaml: ...
```

**Cause**: Invalid YAML (indentation, syntax) or value out of range
(`Config::validate()` rejects dangerous configs) — or unknown keys
(this is **not** blocking: warning, e.g. `config: key 'laya.variant'
unknown (ignored — typo or removed feature?)`).

**Solution**: Validate the YAML with `yamllint` or `python -c "import yaml; yaml.safe_load(open('PGindexAI.yaml'))"`;
remove the unknown keys that were reported.

### TLS failure (PostgreSQL connection)

```
ERROR ssl_mode=verify-full but certificate not verifiable: InvalidCertificate(UnknownIssuer)
```

**Behavior**: the connection is **refused** (rustls always validates the
chain + hostname — there is no "accept-insecure" mode).

**Common causes**:
- Self-signed CA not provided → add `postgresql.ssl_root_cert: /path/ca.pem`
- Certificate CN/SAN ≠ `postgresql.host` (e.g. `host: localhost` but
  certificate issued for `db.internal`) → align the two
- `ssl_mode` ≠ `disable` with a **Unix socket** host (`/var/run/postgresql`)
  → TLS is impossible on a socket: set `ssl_mode: disable` for a local socket

**Verification**: `psql "sslmode=verify-full sslrootcert=/path/ca.pem" -c "SELECT ssl, version FROM pg_stat_ssl WHERE pid = pg_backend_pid();"`

### Indexes not detected

**Cause**: The statistics are not up to date.

**Solution**:
```sql
ANALYZE;
-- or for a specific table
ANALYZE orders;
```

### Service does not start

```bash
# Check the logs
journalctl -u pgindexai -n 50

# Check the config (one full cycle, without applying DDL)
pgindexai --config ~/.config/PGindexAI/PGindexAI.yaml run --dry-run

# Check the permissions (the file must be 0600)
ls -la ~/.config/PGindexAI/PGindexAI.yaml
```

## 6. Maintenance

### Restart

```bash
sudo systemctl restart pgindexai
```

> **After a PostgreSQL restart**: the
> `pg_stat_user_indexes` counters are reset to zero. The daemon **will
> automatically skip** the "unused indexes" analysis as long as the
> server uptime is < `thresholds.unused_minutes` (default 7 days = 10080 min) —
> no intervention needed, which avoids false-positive DROPs.

### Update

```bash
cargo build --release
sudo cp target/release/pgindexai /usr/local/bin/pgindexai
sudo systemctl restart pgindexai
```

### Uninstall

```bash
# Default: service + binary + config + data + logs
# (KEEPS the pgindexai_demo database, the pgindexai role and the pg_hba line)
sudo ./uninstall.sh

# Full simulation (no change)
sudo ./uninstall.sh --dry-run

# Complete removal: + database, role and pg_hba line (irreversible)
sudo ./uninstall.sh --purge

# + leftovers from the old name (laya-sql: binary, config, logs, PG objects)
sudo ./uninstall.sh --clean-legacy
```

`uninstall.sh` (shellcheck-clean, no `eval`, prompts via `/dev/tty`):
- stops and disables the service, removes the unit (and `reset-failed`);
- stops any orphan process (started by hand);
- removes the binary, the config (0600), the data and the logs;
- with `--purge`: `DROP DATABASE pgindexai_demo`, `DROP ROLE pgindexai`,
  removal of the dedicated line from `pg_hba.conf` + PostgreSQL reload;
- **never modifies** `shared_preload_libraries` nor the
  `pg_stat_statements` extension (shared extension — manual removal if truly
  wanted: remove it from `shared_preload_libraries` in `postgresql.conf`,
  then `sudo systemctl restart postgresql`).

## 7. Security

### Action control

Each DDL action type is controlled by a flag in the config:

| Flag | Action | Risk |
|------|--------|--------|
| `create_index: true` | CREATE INDEX (CONCURRENTLY) | Low (extra index, online build) |
| `drop_unused: true` | DROP INDEX | Medium (loss of query capability) |
| `rebuild_fragmented: true` | REINDEX (CONCURRENTLY) | Low (online rebuild, no blocking) |

### Security recommendations

1. **No fallback**: without an active Laya, **no** DDL is applied —
   the config cannot bypass this guarantee (no "heuristic" mode)
2. **Production**: `drop_unused: false` during the burn-in phase (DROPs remain
   subject to Laya's decision even with the flag active)
3. **min_confidence**: raise it to `0.9` in production to avoid false positives
4. **TLS**: for a remote TCP connection, `ssl_mode: verify-full` (or
   `verify-ca`) — rustls **always validates** the chain and the hostname, there
   is no "accept all" mode; add `ssl_root_cert` (PEM)
   if your CA is self-signed. Unix socket ⇒ `ssl_mode: disable`.
5. **Dedicated role**: the daemon connects as `pgindexai` (non-superuser,
   `scram-sha-256`) — do not run it as the superuser `postgres`
6. **Config**: file at **0600** (contains the password); unknown
   keys trigger an explicit warning at load time
7. **Logs**: set `index_logging.log_file` for audit (built-in
   rotation, mode 0640, loud failures)
8. **Side-effect-free check**: `pgindexai run --dry-run` analyzes and prints
   proposals without calling Laya or running DDL
9. **Laya**: the deep health check (`deep_health()`) validates the decision
   pipeline before each batch — a "dead" Laya server that still answers
   simple HTTP probes will be detected

## 8. Deployment architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          PostgreSQL Server                              │
│                                                                         │
│  ┌──────────────┐  ┌──────────────────┐  ┌──────────────────────────┐  │
│  │  pgindexai_demo   │  │  pg_stat_        │  │  information_schema       │  │
│  │  (5 tables)  │→ │  statements      │  │  + pg_stats + pg_am       │  │
│  │  (23 indexes)│  │  (queries)       │  │  (column metadata)        │  │
│  └──────────────┘  └──────────────────┘  └──────────────────────────┘  │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  pgindexai daemon (systemd)                                      │   │
│  │  ┌──────────┐  ┌──────────────────────────┐  ┌───────────────┐  │   │
│  │  │ Analyzer │→ │ DDL Engine (online)         │→ │ IndexLogWriter│  │   │
│  │  │ 3 detect.│  │ CONCURRENTLY / DROP tx      │  │ (JSONL)      │  │   │
│  │  │ + state  │  └──────────────────────────┘  └───────────────┘  │   │
│  │  │ (Healthy/│                                                      │   │
│  │  │ Degraded)│                                                      │   │
│  │  └──────────┘                                                      │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│                                                                         │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │  laya --server (required to apply DDL)                          │   │
│  │  /v1/systemone (AI decision) + deep health check per batch      │   │
│  │  Laya unreachable → NO DDL (no fallback)                        │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```
