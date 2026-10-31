# 03 — Référence module par module

## 1. `src/main.rs` — Point d'entrée

### CLI

```
pgindexai [OPTIONS] <COMMAND>

Commands:
  daemon  Run the daemon (continuous analysis cycle)
  run     Run a single analysis cycle and exit
  help    Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>  Path to configuration file
      --dry-run          Analyze and print proposals only — no Laya call, no DDL executed
  -h, --help             Print help
```

`--dry-run` est un drapeau **global** : avec `daemon`, chaque cycle analyse et
affiche les propositions mais n'appelle ni Laya ni DDL ; avec `run`, le daemon
quitte après l'analyse.

### Fonctions principales

| Fonction | Signature | Description |
|----------|-----------|-------------|
| `main()` | `async fn main() -> Result<(), Box<dyn Error + Sync + Send>>` | Parsing CLI (dont `--dry-run`), chargement config, init tracing, **pool PostgreSQL (TLS selon `ssl_mode`, taille + timeout configurables)**, `check_pg_stat_statements` → **`DaemonState`**, création `LayaClient`, **`deep_health()`**, dispatch `daemon`/`run` |
| `check_pg_stat_statements()` | `async fn check_pg_stat_statements(pool: &Pool) -> Result<DaemonState, …>` | Vérifie l'extension `pg_stat_statements`. Présente → `Healthy`, absente → **`Degraded`** (warnings explicites + instructions d'installation) |
| `run_daemon()` | `async fn run_daemon(pool, laya, cfg, daemon_state, dry_run) -> …` | Boucle : **un seul `Analyzer`** réutilisé, `analyze_cycle()` → `apply_proposals()` (sauf `--dry-run`) → sommeil **interrompible par SIGINT/SIGTERM** (arrêt gracieux, « Daemon stopped. ») |
| `run_once()` | `async fn run_once(pool, laya, cfg, daemon_state, dry_run) -> …` | Un cycle : `analyze_cycle()` → affichage des propositions → `apply_proposals()` **sauf `--dry-run`** → exit |

### Types internes

| Type | Rôle |
|------|------|
| `Cli` | CLI struct (clap derive) |
| `Command` | Sous-commandes : `Daemon`, `Run` |

---

## 2. `src/state.rs` — État du daemon (nouveau)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Healthy,    // pg_stat_statements actif — toutes les détections disponibles
    Degraded,   // pg_stat_statements absent — détection d'index manquants DÉSACTIVÉE
}

impl DaemonState {
    pub fn is_degraded(self) -> bool;
}
```

| Valeur | Comportement |
|--------|--------------|
| `Healthy` | Les 3 détecteurs sont actifs |
| `Degraded` | `analyze_unused` + `analyze_fragmentation` actifs ; `analyze_missing_sargable` **désactivé** (pas de source de données) — warning explicite à chaque cycle |

L'état est déterminé **une seule fois au démarrage** (vérification de
`pg_extension`) puis porté par l'`Analyzer`.

---

## 3. `src/naming.rs` — Convention de nommage d'index (nouveau)

```rust
pub const MAX_IDENTIFIER_LEN: usize = 63;   // NAMEDATALEN - 1 (PostgreSQL)

pub enum ValidatedName { AsIs(String), Truncated(String) }
pub enum NamingError { Empty }
```

| Fonction | Signature | Description |
|----------|-----------|-------------|
| `validate_identifier()` | `(raw: &str) -> Result<ValidatedName, NamingError>` | Nettoie (minuscules, `[a-z0-9_]`), préfixe `_` si chiffre en tête, tronque à 63 octets si nécessaire |
| `generate_index_name()` | `(schema, table, column) -> Result<String, NamingError>` | Convention **`idx_<table>_<col>`** ; si trop long → tronqué + **hash court** (FNV-1a base 36, `[0-9a-z]`) pour garantir l'unicité |
| `is_simple_identifier()` | `(name: &str) -> bool` | Vrai uniquement si l'identifiant (issu du catalogue PG) est simple non-quoté `[a-z_][a-z0-9_]*` ≤ 63 octets — **seuls ces identifiants sont injectés** dans un DDL ou un payload Laya (les exotiques/quotés sont exclus, loggés en warning) |

**Exemples** :

| Entrée | Résultat |
|--------|----------|
| `table = "orders"`, `column = "status"` | `idx_orders_status` |
| `table = "order_items"`, `column = "shipping_method"` | `idx_order_items_shipping_method` (33 octets, OK) |
| colonne très longue (dépassement) | `idx_<table>_<col-tronquée>_<hash6>` (≤ 63 octets) |

---

## 4. `src/analyzer.rs` — Moteur d'analyse

### Struct

```rust
pub struct Analyzer {
    pool: Pool,
    laya: LayaClient,
    config: Config,
    daemon_state: DaemonState,        // NOUVEAU
    log_writer: Arc<IndexLogWriter>, // partagé (log synchrone interne mutex court)
}
```

### Méthodes publiques

| Méthode | Retour | Description |
|---------|--------|-------------|
| `new(pool, laya, config, daemon_state)` | `Self` | Constructeur — initialise `IndexLogWriter` depuis la config |
| `daemon_state()` | `DaemonState` | État courant (Healthy / Degraded) |
| `analyze_cycle()` | `async → Vec<Proposal>` | Orchestre les 3 détecteurs (selon `actions.*` + `daemon_state`) |
| `apply_proposals(proposals)` | `async → Vec<Proposal>` | **`deep_health()` unique** → décision Laya par proposition → **DDL en ligne** (`CONCURRENTLY`) → log |

### Méthodes privées

| Méthode | Description |
|---------|-------------|
| `cap()` | Plafond de propositions par cycle (`thresholds.max_proposals_per_cycle`, défaut 20) — appliqué en SQL (`LIMIT`) dans chaque détecteur **et** tronqué globalement dans `analyze_cycle` |
| `guard_identifier(kind, name)` | **Nouveau** — rejette (warning) tout identifiant catalogue non conforme à `is_simple_identifier` avant DDL / payload Laya |
| `analyze_unused()` | **Gate `observation_window_ok`** (uptime ≥ `unused_minutes`) → `get_unused_indexes(pool, cap)` → `guard_identifier` sur schéma/table/index → `Proposal { DropIndex }` (raison inclut date de redémarrage + taille + durée d'observation) |
| `analyze_fragmentation()` | `get_fragmented_indexes(pool, cap)` → seuil `fragmentation_pct` → `Proposal { RebuildIndex }` (`REINDEX INDEX CONCURRENTLY`) |
| `analyze_missing_sargable()` | `get_missing_sargable_indexes(pool, schemas, min_calls, cap)` → exclusion `pg_*` → **`naming::generate_index_name`** → **densité par bucket → taille estimée** → `null_frac > 0.10` ⇒ `WHERE … IS NOT NULL` → `Proposal { CreateIndex \| CreateFilteredIndex }` (`CREATE INDEX CONCURRENTLY IF NOT EXISTS`) |
| `density_for_bucket(bucket)` | Bucket → densité cible depuis `config.density_by_type` (défaut 50) |

### Logique `analyze_cycle()`

```
1. if actions.drop_unused            → analyze_unused()
2. if actions.rebuild_fragmented     → analyze_fragmentation()
3. if daemon_state == Degraded       → WARN + analyse SARGable DÉSACTIVÉE
   else                              → analyze_missing_sargable()
4. si total > cap() → WARN + tronque à cap()
   (les propositions écartées sont re-détectées au cycle suivant)
```

### Logique `apply_proposals()` (politique sans fallback)

```
if proposals vide → return []

0. deep_health()                      [1 seul appel par lot]
   └── FAILED → WARN "N proposals SKIPPED (no fallback policy)"
              → return []  (AUCUNE DDL)

for proposal in proposals:
    1. criteria = selon proposal.action:
         DropIndex            → {"0": Keep, "1": Drop — <reason>}
         RebuildIndex         → {"0": Keep, "1": Rebuild — <reason>}
         CreateIndex*         → {"0": Ignore, "1": Create std, "2": Create filtré}
    2. payload = laya.build_payload(proposal.reason, criteria)
    3. response = laya.decision(payload)
         └── Err → skip cette proposition (pas de fallback)
    4. if confidence < min_confidence → skip (debug)
    5. action_applique = choice valide pour ce type
         └── non → skip (debug)
    6. if !action_allowed(proposal.action) → skip (warn)
    7. match proposal.action:
         DropIndex            → ddl::execute_drop_index()
         RebuildIndex         → ddl::execute_reindex()
         CreateIndex*         → ddl::execute_create_index()
    8. log résultat ; if succès → applied.push(proposal)
return applied
```

---

## 5. `src/db/mod.rs` — Pool PostgreSQL

### Fonctions

| Fonction | Signature | Description |
|----------|-----------|-------------|
| `create_pool()` | `async fn create_pool(cfg: &PgConfig, pool_cfg: &PgPoolConfig) -> Result<Pool, …>` | Crée un pool deadpool-postgres **dimensionné** avec connecteur TLS (rustls) ; **fail-fast** : hostname invalide ou socket Unix + TLS → erreur claire ; vérifie la connectivité (`SELECT 1`) + log de l'état TLS effectif via `pg_stat_ssl` |
| `build_tls_connector()` | `(cfg: &PgConfig) -> Result<MakeRustlsConnect, …>` | **NOUVEAU** — provider crypto `ring` ; store de racines = **web PKI (Mozilla)** + `ssl_root_cert` (PEM, CA auto-signée) si fourni ; sans auth client |
| `validate_hostname()` | `(host: &str) -> Result<(), …>` | **NOUVEAU** — vérifie que `postgresql.host` est un nom d'hôte valide pour le handshake TLS (évite un échec tardif cryptographique) |

### Paramètres du pool

| Paramètre | Source | Défaut |
|-----------|--------|--------|
| `max_size` | `pg_pool.max_size` | 16 |
| `wait_timeout_ms` | `pg_pool.wait_timeout_ms` (bornée, pas de blocage infini) | 30 000 |
| `recycling_method` | — | `Fast` |
| `runtime` | — | `Tokio1` |
| `ssl_mode` | `postgresql.ssl_mode` (mapping : `disable` → `Disable` ; `require`/`verify-ca`/`verify-full` → `Require`) | `Disable` |
| `ssl_root_cert` | `postgresql.ssl_root_cert` (PEM, optionnel — CA supplémentaire) | — |

### Sémantique TLS (rustls) — à retenir

- **La vérification chaîne + nom d'hôte est TOUJOURS effectuée** au handshake
  (contrairement à libpq où `require` saute la vérification). `require` est donc
  **au moins aussi strict** que `verify-ca`/`verify-full`.
- La différence entre les 3 modes actifs réside dans les **racines acceptées** :
  web PKI (Mozilla) par défaut, + `ssl_root_cert` (PEM) si fourni (CA
  auto-signée / snakeoil).
- **Socket Unix** : le TLS est impossible (pas de nom d'hôte pour le handshake)
  → `ssl_mode` **doit** être `disable` ; sinon erreur explicite au démarrage.
- Le CN/SAN du certificat serveur **doit correspondre** à `postgresql.host`.
- État vérifié au démarrage via `pg_stat_ssl` (log `TLS actif` ou warning).

---

## 6. `src/db/queries.rs` — Requêtes DMV + parsing SARGable

### Types de résultats

| Type | Champs clés |
|------|-------------|
| `UnusedIndexInfo` | `schemaname`, `tablename`, `indexname`, `indexdef`, `idx_scan`, `idx_tup_read`, `idx_tup_fetch`, `size_bytes`, **`installed_at`** (démarrage serveur) |
| `FragmentationInfo` | `schemaname`, `tablename`, `indexname`, `relpages`, `avg_leaf_density`, `fragmentation_pct` |
| `MissingSargableIndex` | `schemaname`, `tablename`, `query_text`, `call_count`, `mean_exec_time_ms`, `min/max_exec_time_ms`, `sargable_columns` |
| `SargableColumn` | `column_name`, **`column_type`**, **`is_nullable`**, **`null_frac`**, `filter_expr`, **`density_bucket`** |
| `TableCardinality` | **`reltuples`**, **`relpages`** |

### Fonctions publiques

| Fonction | Signature | Description |
|----------|-----------|-------------|
| `observation_window_ok()` | `(pool, min_minutes) → bool` | **NOUVEAU** — `now() - pg_postmaster_start_time() >= make_interval(mins => $1)` ; gate de l'analyse unused |
| `get_unused_indexes()` | `(pool, max_rows) → Vec<UnusedIndexInfo>` | `pg_stat_user_indexes` + `pg_index` (`idx_scan = 0 AND idx_tup_read = 0 AND indisvalid AND NOT indisprimary AND NOT indisunique`) + **`LIMIT $2`** (budget côté SQL) |
| `get_existing_indexes()` | `(pool, schemas) → HashMap<(schema, table), Vec<(name, def)>>` | `pg_indexes` hors schémas système, **filtrage multi-schéma côté Rust** |
| `get_fragmented_indexes()` | `(pool, max_rows) → Vec<FragmentationInfo>` | **B-tree uniquement** (`pg_am`) ; score 3 critères (dead 50 %, mod 30 %, bonus 20 %) ; cast `::bigint`/`::float8` ; **`LIMIT`** |
| `get_missing_sargable_indexes()` | `(pool, schemas, min_calls, max_queries) → Vec<MissingSargableIndex>` | `pg_stat_statements` (`calls ≥ min_calls`, **`LIMIT max_queries`**) → **parse AST sqlparser** → périmètre multi-schéma → colonnes non couvertes → **enrichissement types réels + `null_frac`** |
| `get_table_cardinality()` | `(pool, schema, table) → TableCardinality` | `pg_class` via `to_regclass(format('%I.%I', $1::text, $2::text))` ; `reltuples::float8`, `relpages::bigint` |
| `index_exists()` | `(pool, schema, name) → bool` | `pg_indexes WHERE schemaname = $1 AND indexname = $2` |
| `index_valid()` | `(pool, schema, name) → bool` | **NOUVEAU** — `indisvalid` via `pg_index` : un index **existant mais INVALIDE** (CONCURRENTLY interrompu) n'est pas opérationnel |
| `cleanup_invalid_indexes()` | `(pool, schema, name) → u64` | **NOUVEAU** — supprime les index INVALIDES de ce nom via `pg_drop_index(oid)` ; renvoie le nombre supprimés |

### Fonctions privées (parseur AST — nouveau)

| Fonction | Description |
|----------|-------------|
| `parse_query_for_sargable(query)` | **NOUVEAU** — Parse `PostgreSqlDialect` (sqlparser 0.52) → colonnes SARGable **attribuées à leur table** (`Vec<(schema, table, column)>`) ; **résolution d'alias JOIN** (`a.status` → table de l'alias `a`) ; une colonne non qualifiée n'est acceptée que si la requête ne référence qu'UNE table (comportement conservateur, pas de faux positif) ; échec → `None` (debug) |
| `collect_sargable(expr, …)` | Parcours récursif de l'AST : AND/OR, `=`,`>`,`<`,`>=`,`<=`, `IN` (non-négé), `BETWEEN` (non-négé), `IS [NOT] NULL`, `LIKE`/`ILIKE` préfixe ; rejette `NOT`, fonctions sur colonne, col-vs-col |
| `expr_as_column(expr)` | `Identifier` / `CompoundIdentifier` → nom de colonne |
| `expr_is_column_ref(expr)` | Vrai si référence de colonne (exclusion des valeurs) |
| `push_column(columns, name)` | Ajout dédupliqué de la colonne |
| `like_pattern_is_prefix(pattern)` | `SingleQuotedString` sans `%`/`\%` en tête → SARGable |
| `is_column_covered_by_index(indexdef, column)` | Vérifie la 1re position de l'indexdef |
| `fetch_column_metadata(pool, schema, table, …)` | **NOUVEAU (par table)** — `information_schema.columns` (type, nullabilité) + `pg_stats` (`null_frac::float8`) |
| `map_pg_type_to_bucket(data_type)` | Type PostgreSQL → bucket de densité (`int4`, `int8`, `float8`, `varchar`, `text`, `date`, `timestamptz`, `uuid`, `bool`, `default`) |
| `truncate_query(query)` | Tronque à 200 caractères (sûr UTF-8) + `…` — usage **local** (logs) uniquement, jamais envoyé à Laya |

### Tests unitaires (11 + 1 live)

`eq_and_in`, `between_and_range`, `like_prefix_vs_wildcard`,
`function_on_column_rejected`, `column_vs_column_rejected`,
`compound_identifier_schema_table`, `invalid_sql_returns_none`,
`join_alias_resolution`, `join_ambiguous_unqualified_column_is_dropped`,
`join_qualified_by_table_name_not_alias`, `truncate_query_multibyte_safe`.
+ `cardinality_live` (test de connexion, **skip proprement** sans PostgreSQL local — sûr en CI).

---

## 7. `src/ddl.rs` — Exécution DDL en ligne

### Fonctions

| Fonction | Signature | Description |
|----------|-----------|-------------|
| `run_in_transaction()` | `async fn (pool, ddl) -> Result<u64, …>` | **NOUVEAU** — `CONCURRENTLY` → **auto-commit** (interdit dans une transaction PostgreSQL) ; sinon **`BEGIN → DDL → COMMIT`** avec rollback en cas d'erreur |
| `is_concurrent_ddl()` | `(ddl) → bool` | Détecte `CONCURRENTLY` (case-insensitive) |
| `execute_create_index()` | `async fn (pool, ddl, writer, confidence?, reason?) -> Result<Option<String>, …>` | Pré-existence (`index_exists`) → DDL → **validation post-DDL : `index_valid`** (indisvalid) ; si INVALIDE → `cleanup_invalid_indexes` + échec loggé |
| `execute_drop_index()` | `async fn (pool, ddl, writer, confidence?, size_bytes, reason?) -> Result<(), …>` | `BEGIN → DROP INDEX → COMMIT` → log (succès/échec + durée) |
| `execute_reindex()` | `async fn (pool, ddl, writer, confidence?, size_bytes, fragmentation, reason?) -> Result<(), …>` | `REINDEX CONCURRENTLY` en auto-commit (warning explicite) ; si build interrompu → **nettoyage de l'index INVALIDE** (`pg_drop_index`) — l'index d'origine reste intact → sinon `index_valid` re-vérifié |
| `extract_index_name()` | `(ddl) → Option<String>` | Parse `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] name`, `DROP INDEX [IF EXISTS] …`, `REINDEX INDEX [CONCURRENTLY] …` |
| `extract_schema_from_ddl()` | `(ddl) → String` | Parse `ON schema.table` / `INDEX schema.name`, défaut `public` |

### Logique d'exécution (toutes DDL)

```
1. name = extract_index_name(ddl) ; schema = extract_schema_from_ddl(ddl)
2. [CREATE] if index_exists(schema, name) → skip (info log)
3. start = Instant::now()
4. run_in_transaction(ddl)
     ├── CONCURRENTLY → execute direct (auto-commit)
     └── sinon        → BEGIN → execute → COMMIT (erreur → rollback)
5. [CREATE/REINDEX CONCURRENTLY] validation post-DDL :
     ├── index_valid() (indisvalid)
     └── si INVALIDE → cleanup_invalid_indexes() (pg_drop_index) → échec loggé
6. duration = start.elapsed()
7. IndexLogWriter.log(...)   (verbosité configurée)
```

---

## 8. `src/index_log.rs` — Historisation structurée

### Types

| Type | Description |
|------|-------------|
| `CrudOperation` | Enum : `CreateIndex`, `DropIndex`, `Reindex` |
| `VerbosityLevel` | Enum : `Minimal`, `Standard`, `Verbose` |
| `IndexOperationLog` | Struct sérialisable en JSON (timestamp, opération, schéma, objet, durée, statut, DDL, confiance, taille, fragmentation, raison) |
| `IndexLogWriter` | Writer — `log(&self)` **synchrone** (mutex court interne, jamais tenu à travers un `await` ; partageable via `Arc`), **échecs bruyants** (une entrée d'audit perdue est loggée en `error!`), rotation de fichiers, mode **0640** |

### Niveau de verbosité

| Niveau | Contenu du log |
|--------|----------------|
| `minimal` | `timestamp`, `operation`, `schema`, `object`, `duration_ms`, `status` |
| `standard` | + `ddl_statement`, `confidence`, `reason` |
| `verbose` | + `fragmentation_pct`, `index_size_bytes` |

### Rotation de fichiers

| Paramètre | Défaut |
|-----------|--------|
| `max_file_size_bytes` | 10 MB |
| `max_files` | 5 |

Rotation : `path → path.1 → … → path.max_files` (le plus ancien est supprimé) ;
réouverture à **0640** (les entrées contiennent le DDL exécuté — trace d'audit).

### Échecs bruyants (politique d'audit)

| Situation | Comportement |
|-----------|--------------|
| Fichier impossible à ouvrir au démarrage | `error!` explicite — audit **stdout uniquement** (jamais de silence) |
| Échec d'écriture d'une entrée | `error!` « cette entrée d'audit est PERDUE » |
| Échec de rotation (rename/suppression) | `warn!`/`error!` explicites, continuation au mieux |

---

## 9. `src/laya/mod.rs` — Client HTTP Laya

### Struct

```rust
pub struct LayaClient {
    client: reqwest::Client,  // timeout configuré au builder
    base_url: String,
}
```

### Méthodes

| Méthode | Retour | Description |
|---------|--------|-------------|
| `new(base_url, timeout_secs)` | `Self` | Constructeur — reqwest client avec timeout |
| **`deep_health()`** | `async → Result<String, LayaError>` | **NOUVEAU** — payload de test réel envoyé à `/v1/systemone` + validation structurelle : `choice ∈ {"0","1"}` ET `confidence ∈ [0,1]` ; détecte un pipeline de décision cassé (modèle, variante, parsing) |
| `decision(payload)` | `async → Result<LayaResponse, LayaError>` | `POST /v1/systemone` — décision IA |
| `build_payload(state, criteria)` | `LayaPayload` | Construit le payload (critères triés `"0","1","2"…`) |

> Le texte de `state` (donc la raison de la proposition) n'est construit qu'à
> partir d'identifiants **validés** (catalogue PG + `is_simple_identifier`) —
> **aucun texte de requête brut** n'est envoyé à Laya.

### `LayaError`

| Variante | Déclenchée par |
|----------|----------------|
| `Unreachable(String)` | Erreur de connexion (Laya hors ligne) |
| `Http(String)` | HTTP non 2xx, lecture de réponse, reqwest |
| `Deserialize(String, serde_json::Error)` | JSON de réponse invalide |
| `InvalidResponse(String)` | `choice` inattendu ou `confidence` hors `[0,1]` |

---

## 10. `src/laya/types.rs` — Types du protocole Laya

### Payload (daemon → Laya)

```rust
pub struct LayaPayload {
    pub state: String,           // Description de la situation
    pub questions: Questions,
}

pub struct Questions {
    pub action: ActionQuestion,
}

pub struct ActionQuestion {
    #[serde(rename = "type")]
    pub type_: String,           // "choice"
    pub instructions: String,    // Instructions pour l'IA
    pub criteria: HashMap<String, String>,  // Options numérotées "0","1","2"…
}
```

### Réponse (Laya → daemon)

```rust
pub struct LayaResponse {
    pub model: String,           // "typed-decisions"
    pub answers: Answers,
    pub usage: Usage,
}

pub struct Answers {
    pub action: ActionAnswer,
}

pub struct ActionAnswer {
    #[serde(rename = "type")]
    pub type_: String,           // "choice"
    pub choice: String,          // "0" | "1" | "2" (selon le type de proposition)
    pub confidence: f64,         // 0.0 - 1.0
    pub probabilities: HashMap<String, f64>,
}

pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
```

### `DecisionAction`

| Variante | Display |
|----------|---------|
| `Ignore` | `IGNORE` |
| `CreateIndex` | `CREATE INDEX` |
| `CreateFilteredIndex` | `CREATE INDEX (filtered)` |
| `DropIndex` | `DROP INDEX` |
| `RebuildIndex` | `REINDEX` |

Le `choice` de la réponse est appliqué **relativement au type de proposition**
(dans `analyzer::apply_proposals`) : `DropIndex`/`RebuildIndex` →
`"1"` = APPLY (sinon `Ignore`) ; `CreateIndex`/`CreateFilteredIndex` →
`"1"` = standard, `"2"` = filtré (sinon `Ignore`) (voir `04-protocols.md` § 1.2).

### Proposal (interne)

```rust
pub struct Proposal {
    pub action: DecisionAction,
    pub ddl: String,
    pub reason: String,
    pub confidence: f64,
    pub table: String,
}
```

Sérialisation custom : `DecisionAction` ↔ string (`"IGNORE"`, `"CREATE INDEX"`, …).

---

## 11. `src/config.rs` — Configuration

### Types de config

| Type | Champs |
|------|--------|
| `Config` | `postgresql`, `laya`, `actions`, `thresholds`, **`analysis`**, **`pg_pool`**, `logging`, `index_logging`, **`density_by_type`** |
| `PgConfig` | `host`, `port`, `user`, `database`, `ssl_mode`, **`password`** (optionnelle), **`ssl_root_cert`** (optionnelle, chemin PEM) |
| `LayaConfig` | `url`, `timeout_seconds` |
| `ActionsConfig` | `create_index`, `drop_unused`, `rebuild_fragmented` |
| `Thresholds` | `cycle_seconds`, `unused_minutes`, `fragmentation_pct`, `min_sargable_rows`, `min_confidence`, **`max_proposals_per_cycle`** |
| **`AnalysisConfig`** (nouveau) | `schemas: Vec<String>` (vide = tous les schémas non-système) |
| **`PgPoolConfig`** (nouveau) | `max_size: u32`, `wait_timeout_ms: u64` |
| `LoggingConfig` | `level`, `format` |
| `IndexLoggingConfig` | `enabled`, `verbosity`, `log_file`, `max_file_size_bytes`, `max_files` |
| **`DensityByTypeConfig`** (nouveau) | `int4`, `int8`, `float8`, `varchar`, `text`, `date`, `timestamptz`, `uuid`, `bool`, `default_density` (tous `f64`) |

Tous les nouveaux champs sont `#[serde(default)]` — une config existante
sans ces sections reste valide.

### Valeurs par défaut

| Paramètre | Défaut |
|-----------|--------|
| `ssl_mode` | `"disable"` |
| `ssl_root_cert` | — (aucune ; racines web PKI seules) |
| `password` | — (null ; auth peer/ident ou trust local) |
| `timeout_seconds` (laya) | `30` |
| `create_index` | `true` |
| `drop_unused` | `true` |
| `rebuild_fragmented` | `true` |
| `cycle_seconds` | `60` |
| `unused_minutes` | **`10080` (7 jours)** |
| `fragmentation_pct` | `70` |
| `min_sargable_rows` | `100` |
| `min_confidence` | `0.7` |
| `max_proposals_per_cycle` | `20` |
| `analysis.schemas` | `[]` (tous les schémas non-système) |
| `pg_pool.max_size` | `16` |
| `pg_pool.wait_timeout_ms` | `30000` |
| `logging.level` | `"info"` |
| `logging.format` | `"json"` |
| `index_logging.enabled` | `true` |
| `index_logging.verbosity` | `"minimal"` |
| `index_logging.max_file_size_bytes` | `10485760` (10 MB) |
| `index_logging.max_files` | `5` |
| `density_by_type.int4` | `200.0` |
| `density_by_type.int8` | `200.0` |
| `density_by_type.float8` | `150.0` |
| `density_by_type.varchar` | `30.0` |
| `density_by_type.text` | `20.0` |
| `density_by_type.date` | `150.0` |
| `density_by_type.timestamptz` | `150.0` |
| `density_by_type.uuid` | `100.0` |
| `density_by_type.bool` | `300.0` |
| `density_by_type.default_density` | `50.0` |

### Validation (`Config::validate`) et clés inconnues

- `validate()` refuse les configs dangereuses : `cycle_seconds ∉ [1, 86400]`,
  `fragmentation_pct ∉ [1, 100]`, `min_confidence ∉ [0,1]`,
  `max_proposals_per_cycle ∉ [1, 1000]`, `pg_pool.max_size ∉ [1, 512]`,
  `wait_timeout_ms ∉ [100, 600000]`, `ssl_mode` hors
  `disable|require|verify-ca|verify-full`, `host`/`user`/`database` vides,
  densités non strictement positives.
- **Clés inconnues** : `warn_unknown_keys()` — avertissement par clé ignorée
  (ex. `config : clé 'laya.variant' inconnue (ignorée — typo ou fonctionnalité
  retirée ?)`), chargement non bloquant.
- `ssl_mode = verify-ca|verify-full` **sans** `ssl_root_cert` → warning
  (certificats auto-signés non acceptés).

### Chemin config par défaut

```
~/.config/PGindexAI/PGindexAI.yaml    (sur Linux)
./PGindexAI.yaml                     (fallback)
```

---

## 12. `sql/` — Scripts de démo

### `sql/demo_schema.sql`

- Crée la base `pgindexai_demo` si elle n'existe pas
- 5 tables : `customers`, `products`, `orders`, `order_items`, `users`
- 23 index : 15 index métier explicites + 8 contraintes PK/UK
- `ANALYZE` sur toutes les tables

### `sql/demo_workload.sql`

- 8 phases de workload réaliste
- Génère `pg_stat_statements` entries pour les 3 détecteurs
- Inclut fragmentation réaliste (DELETE + INSERT + UPDATE + VACUUM FULL)
- Workload SARGable répétitif (17 000 requêtes via `EXECUTE … USING`)
