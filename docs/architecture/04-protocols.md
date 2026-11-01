# 04 — Protocoles

> **Nouveautés** : health check **profond** (payload de test réel vers
> `/v1/systemone`, validation structurelle de la réponse), politique
> **sans fallback** (Laya injoignable → aucune DDL appliquée), détection
> SARGable par **vrai parseur AST** (sqlparser-rs), types de colonnes
> réels + densités cibles dans le contexte de décision.

## 1. Protocole Laya `/v1/systemone`

### 1.1 Health check profond (nouveau)

Au démarrage **et** avant chaque lot de propositions, le daemon envoie un
payload de test réel au serveur Laya — pas une simple sonde de connectivité :

```json
{
  "state": "deep-health-check",
  "questions": {
    "action": {
      "type": "choice",
      "instructions": "Choose the single best action for this PostgreSQL index situation.",
      "criteria": {
        "0": "Option A: no action needed",
        "1": "Option B: perform a maintenance action"
      }
    }
  }
}
```

La réponse est **validée structurellement** :

| Vérification | Échec → |
|--------------|---------|
| `answers.action.choice` ∈ `{"0", "1"}` | `LayaError::InvalidResponse` |
| `answers.action.confidence` ∈ `[0, 1]` | `LayaError::InvalidResponse` |
| Connexion + HTTP 2xx + JSON parsable | `LayaError::Unreachable` / `Http` / `Deserialize` |

Cela détecte les cas où la connectivité HTTP fonctionne mais que le pipeline
de décision (modèle, variante, parsing de réponse) est cassé.

### 1.2 Request de décision

**POST** `http://127.0.0.1:8080/v1/systemone`

Le payload est **spécialisé par proposition** : le daemon connaît déjà
l'action qu'il propose ; Laya arbitre entre *appliquer* et *ignorer* (et,
pour les CREATE, entre variante standard et filtrée) :

```json
{
  "state": "Index idx_products_stock on public.products never scanned since 2026-10-30T09:32:16Z. Size: 90112 bytes. Observed unused for at least 10080min.",
  "questions": {
    "action": {
      "type": "choice",
      "instructions": "Choose the single best action for this PostgreSQL index situation.",
      "criteria": {
        "0": "Keep: index might be needed later",
        "1": "Drop: index is unused — Index idx_products_stock on public.products never scanned…"
      }
    }
  }
}
```

**Critères envoyés selon le type de proposition** :

| Proposition du daemon | `"0"` | `"1"` | `"2"` |
|-----------------------|-------|-------|-------|
| `DROP INDEX` | Keep (l'index pourra servir plus tard) | Drop | — |
| `REINDEX` | Keep (fragmentation acceptable) | Rebuild | — |
| `CREATE INDEX` | Ignore (pas d'index nécessaire) | Créer (standard) | Créer (filtré `WHERE … IS NOT NULL`) |

**Contexte enrichi** (nouveau) : pour les CREATE INDEX, le contexte de
décision inclut le **type réel de la colonne**, la fraction de NULL
(`null_frac`), la **densité cible** du bucket de type et la **taille
estimée** de l'index (`reltuples / density × 8192` octets) :

```
SARGable query on public.orders: shipping_method (type: character varying,
1000 calls, 0.1ms avg, null_frac: 0.00). No covering index. Target density
for varchar: 30 tuples/page, estimated index size: 163848192 bytes.
```

### Response

```json
{
  "model": "typed-decisions",
  "answers": {
    "action": {
      "type": "choice",
      "choice": "1",
      "confidence": 0.85,
      "probabilities": {
        "0": 0.05,
        "1": 0.95
      }
    }
  },
  "usage": {
    "input_tokens": 256,
    "output_tokens": 12
  }
}
```

Le daemon exige que :
- `choice` soit une **chaîne** (`"0"`, `"1"`, `"2"` — un entier brut provoque
  une erreur de deserialisation `LayaError::Deserialize`) ;
- `confidence` soit un flottant ∈ `[0, 1]` (vérifié par `deep_health()`).

Toute réponse malformée est traitée comme Laya indisponible : la
proposition (ou le lot, selon le contexte) est simplement sautée.

### Mapping `choice` → action

Le mapping dépend du **type de proposition** (le daemon sait déjà ce qu'il propose) :

| Proposition | `"0"` | `"1"` | `"2"` |
|------------|-------|-------|-------|
| `DropIndex` | Keep (ignoré) | **APPLY** `DROP INDEX IF EXISTS {schema}.{index}` | invalide |
| `RebuildIndex` | Keep (ignoré) | **APPLY** `REINDEX INDEX CONCURRENTLY {schema}.{index}` | invalide |
| `CreateIndex` / `CreateFilteredIndex` | Ignore | **APPLY** (variante standard) | **APPLY** (variante filtrée) |

> La DDL exécutée est toujours celle générée par le daemon (calculée à partir
> de `null_frac` : colonne nullable > 10 % → `WHERE col IS NOT NULL`). Les
> choix `"1"`/`"2"` sur une proposition CREATE signent tous deux « appliquer » ;
> ils ne modifient pas la DDL.

### Règles d'application (nouvelle politique « sans fallback »)

```
1. deep_health() FAILED (Laya injoignable / réponse invalide)
      → TOUTES les propositions du lot sont SKIPPED (aucune DDL)
        (1 seul appel par cycle, pas par proposition)

2. decision() OK :
   confidence ≥ min_confidence (0.7)  →  DDL exécutée en ligne (CONCURRENTLY)
   confidence < min_confidence        →  skip (debug log, aucune heuristique)

3. Erreur sur une décision individuelle (timeout, HTTP, JSON)
      →  seule cette proposition est skip (pas de fallback heuristique)

4. action flag désactivé en config (actions.*)  →  skip (warn log)
```

**Conséquence opérationnelle** : le daemon reste en mode « observation »
(les propositions sont générées et journalisées) tant que Laya n'est pas
actif et valide. Aucune décision n'est jamais prise à la place de Laya.

---

## 2. Requêtes DMV PostgreSQL

### 2.1 Index inutilisés

> **Nouveauté (fix #10)** : les compteurs `pg_stat_user_indexes` sont
> **remis à zéro à chaque redémarrage** de PostgreSQL. Prononcer un DROP
> sur un serveur fraîchement démarré serait une fausse détection. Le daemon
> vérifie donc d'abord la **fenêtre d'observation** (uptime serveur) avant
> même de lancer la requête.

**Gate préliminaire** :

```sql
SELECT now() - pg_postmaster_start_time() >= make_interval(mins => $1)
-- $1 = thresholds.unused_minutes (défaut 10080 = 7 jours)
```

Si `false` → l'analyse « index inutilisés » est **entièrement sautée** pour
ce cycle (debug log), pas d'erreur.

**Requête** : `pg_stat_user_indexes` + `pg_index`

```sql
SELECT
    s.schemaname,
    s.relname AS tablename,
    s.indexrelname AS indexname,
    pg_get_indexdef(s.indexrelid) AS indexdef,
    COALESCE(s.idx_scan, 0) AS idx_scan,
    COALESCE(s.idx_tup_read, 0) AS idx_tup_read,
    COALESCE(s.idx_tup_fetch, 0) AS idx_tup_fetch,
    pg_relation_size(s.indexrelid) AS size_bytes,
    pg_postmaster_start_time() AS installed_at   -- début de la fenêtre d'observation
FROM pg_stat_user_indexes s
JOIN pg_index i ON s.indexrelid = i.indexrelid
WHERE s.idx_scan = 0
  AND s.idx_tup_read = 0
  AND i.indisvalid = true
  AND i.indisprimary = false
  AND i.indisunique = false
ORDER BY pg_relation_size(s.indexrelid) DESC
LIMIT $1;   -- max_proposals_per_cycle (budget côté SQL, défaut 20)
```

**Filtres** :
- `idx_scan = 0` ET `idx_tup_read = 0` (jamais scanné **depuis le redémarrage**)
- `indisvalid = true` (index valide)
- `indisprimary = false` (pas une clé primaire)
- `indisunique = false` (pas un index unique)
- Uptime serveur ≥ `thresholds.unused_minutes` (gate ci-dessus)

Le contexte envoyé à Laya inclut la **date du redémarrage** (`installed_at`)
et la durée minimale d'observation — le moteur de décision peut ainsi
pondérer sa confiance sur la solidité de l'évidence.

---

### 2.2 Index fragmentés

> **Nouveauté** : seuls les index **B-tree** sont considérés
> (`JOIN pg_am ON … WHERE amname = 'btree'`) — les index GiST/GIN/SP-GiST/BRIN
> n'ont pas la même notion de fragmentation. Les lectures depuis `pg_class`
> sont castées (`relpages::bigint`, `reltuples::float8`) car le pilote
> postgres-types exige des types stricts (INT4/FLOAT4 ne sont pas élargis).

**Sources** : `pg_stat_user_indexes` + `pg_index` + `pg_class` + `pg_am` + `pg_stat_user_tables`

```sql
WITH index_info AS (
    SELECT
        s.schemaname,
        pi.indrelid AS table_oid,
        s.relname AS tablename,
        s.indexrelname AS indexname,
        pg_relation_size(s.indexrelid) / 8192 AS relpages,
        COALESCE(s.idx_tup_read, 0) AS idx_tup_read,
        COALESCE(s.idx_scan, 0) AS idx_scan,
        pc.relpages::bigint AS index_pages,
        pc.reltuples::float8 AS index_tuples
    FROM pg_stat_user_indexes s
    JOIN pg_index pi ON s.indexrelid = pi.indexrelid
    JOIN pg_class pc ON s.indexrelid = pc.oid
    JOIN pg_am am ON pc.relam = am.oid
    WHERE am.amname = 'btree'
      AND pg_relation_size(s.indexrelid) > 0
),
table_stats AS (
    SELECT
        c.oid,
        COALESCE(t.n_live_tup, 0) AS n_live_tup,
        COALESCE(t.n_dead_tup, 0) AS n_dead_tup,
        COALESCE(t.n_mod_since_analyze, 0) AS n_mod_since_analyze
    FROM pg_class c
    LEFT JOIN pg_stat_user_tables t ON c.oid = t.relid
)
SELECT
    ii.schemaname,
    ii.tablename,
    ii.indexname,
    ii.relpages,
    ii.index_pages,
    ii.index_tuples,
    ii.idx_scan,
    ts.n_live_tup,
    ts.n_dead_tup,
    ts.n_mod_since_analyze
FROM index_info ii
LEFT JOIN table_stats ts ON ts.oid = ii.table_oid
ORDER BY ii.relpages DESC
LIMIT $1;   -- max_proposals_per_cycle (budget côté SQL, défaut 20)
```

**Score de fragmentation** (heuristique multi-critères) :

```
frag_score = dead_ratio * 0.5
           + min(mod_ratio, 1.0) * 0.3
           + (idx_scan == 0 AND relpages > 100 ? 0.2 : 0.0)

fragmentation_pct = min(frag_score * 100, 100)
```

| Critère | Formule | Pondération |
|---------|---------|-------------|
| Dead tuples | `n_dead_tup / (n_live_tup + n_dead_tup)` | 50% |
| Modifications non-analysées | `n_mod_since_analyze / n_live_tup` | 30% |
| Bonus non-scanné | `idx_scan == 0 AND relpages > 100` | 20% |

Un index est proposé en REINDEX si `fragmentation_pct ≥ thresholds.fragmentation_pct` (défaut 70 %).

**Densité moyenne estimée** (contexte envoyé à Laya) :

```
avg_leaf_density = min(max(index_tuples / index_pages / 100, 0.0), 1.0)
```

---

### 2.3 Requêtes SARGable sans index

> **Nouveauté (fix #3, #5, #9)** : la détection SARGable utilise désormais un
> **vrai parseur SQL** (sqlparser-rs, dialecte PostgreSQL) — analyse de
> l'AST, pas des regex. Les types de colonnes réels (`information_schema`)
> et la fraction de NULL (`pg_stats`) enrichissent le contexte de décision,
> et le périmètre multi-schéma s'applique côté Rust.

**Requête de candidates** : `pg_stat_statements` (top N par poids `calls × time`, N = plafond de propositions)

```sql
SELECT
    s.queryid,
    s.query,
    s.calls,
    s.mean_exec_time,
    s.min_exec_time,
    s.max_exec_time,
    s.rows
FROM pg_stat_statements s
WHERE s.calls >= $3   -- thresholds.min_sargable_rows
  AND s.query !~* $1  -- ^(CREATE|ALTER|DROP|DELETE|INSERT|UPDATE|BEGIN|COMMIT|ROLLBACK|EXPLAIN|ANALYZE|VACUUM|GRANT|REVOKE|SET|SHOW)
  AND s.query !~* $2  -- ^(SELECT|WITH).*\bpg_   (catalogues système exclus)
ORDER BY (s.calls * GREATEST(s.mean_exec_time, 0.1)) DESC
LIMIT $4             -- max_proposals_per_cycle (budget côté SQL)
```

**Filtres** :
- `calls ≥ thresholds.min_sargable_rows` (défaut 100)
- `LIMIT max_proposals_per_cycle` (budget côté SQL, défaut 20)
- Exclusion des DDL, des requêtes système et des catalogues `pg_*`
- **Périmètre multi-schéma** : si `analysis.schemas` est non vide, les
  tables hors liste sont ignorées (fix #9)

#### 2.3.1 Analyse AST (sqlparser-rs)

Chaque candidate est parsée (`PostgreSqlDialect`) ; si le parsing échoue,
la requête est écartée (debug log). Le daemon identifie la table cible
(`FROM schema.table`, défaut `public`) puis parcourt l'arbre de la clause
`WHERE` pour collecter les colonnes SARGable.

**JOIN (nouveau)** : les colonnes sont attribuées à leur table via les
alias (`a.status` → table de l'alias `a`). Une colonne **non qualifiée**
n'est acceptée que si la requête ne référence qu'UNE table — comportement
conservateur qui évite les faux positifs (colonne ambiguë écartée, debug log).

**Opérateurs reconnus (SARGable)** :

| Pattern | Condition |
|---------|-----------|
| `col = v`, `col > v`, `col < v`, `col >= v`, `col <= v` | côté colonne : référence de colonne ; côté valeur : **pas** une colonne |
| `col IN (v1, …, vn)` (non-négé) | tous les éléments non colonnes |
| `col BETWEEN a AND b` (non-négé) | bornes non colonnes |
| `col IS NULL` / `col IS NOT NULL` | — |
| `col LIKE 'pref…%'` / `col ILIKE 'pref…%'` | motif **préfixe** (pas de `%` en tête) |
| `AND` / `OR` / parenthèses imbriquées | parcours récursif |

**Rejetés (non-SARGable)** :
- `NOT ( … )`, `col IS DISTINCT FROM …` (négation / sémantique non bornable)
- `col NOT IN (…)`, `col NOT BETWEEN …`
- Fonctions sur la colonne : `LOWER(col)`, `UPPER(col)`, `CAST(col AS …)`
- Comparaison colonne-contre-colonne : `a.col = b.col`
- `LIKE`/`ILIKE` avec jocker en tête (`'%…'`) ou motif non littéral
- `col NOT LIKE …`

#### 2.3.2 Filtrage des colonnes couvertes

```
Pour chaque requête SARGable détectée :
    1. extraire table + schéma depuis l'AST (FROM)
    2. vérifier le périmètre multi-schéma
    3. charger les index existants (pg_indexes, filtrés côté Rust)
    4. pour chaque colonne SARGable :
        si is_column_covered_by_index(indexdef, column) → skip
    5. si colonnes restantes > 0 → proposal
```

#### 2.3.3 Enrichissement métadonnées (nouveau)

Pour les colonnes candidates restantes, le daemon charge :

| Donnée | Source | Usage |
|--------|--------|-------|
| `data_type` réel (ex: `character varying`) | `information_schema.columns` | Contexte Laya + bucket de densité |
| `is_nullable` | `information_schema.columns` | Décision index filtré |
| `null_frac` | `pg_stats` (`null_frac::float8`) | Si > 0.10 → `WHERE col IS NOT NULL` |
| `reltuples` / `relpages` de la table | `pg_class` (`to_regclass`) | Estimation de taille du futur index |

**Buckets de densité** (`map_pg_type_to_bucket`) :

| Type PostgreSQL | Bucket | Densité défaut |
|-----------------|--------|----------------|
| `integer`, `smallint` | `int4` | 200 tuples/page |
| `bigint` | `int8` | 200 |
| `double precision`, `real`, `numeric`, `decimal` | `float8` | 150 |
| `character varying`, `character`, `char` | `varchar` | 30 |
| `text` | `text` | 20 |
| `date` | `date` | 150 |
| `timestamp (with/without time zone)`, `time (…)` | `timestamptz` | 150 |
| `uuid` | `uuid` | 100 |
| `boolean` | `bool` | 300 |
| tout autre type | `default` | 50 |

Toutes les densités sont configurables (`density_by_type.*`).

**Estimation de taille du futur index** (envoyée au contexte de décision) :

```
est_pages = ceil(reltuples / density_bucket)
est_bytes = est_pages × 8192
```

---

### 2.4 Vérification pg_stat_statements

```sql
SELECT extname, extversion
FROM pg_extension
WHERE extname = 'pg_stat_statements';
```

---

### 2.5 Vérification pré-existence index

```sql
SELECT 1
FROM pg_indexes
WHERE schemaname = $1
  AND indexname = $2;
```

---

## 3. Format de log des opérations d'index

### JSONL (fichier)

```json
{
  "timestamp": "2026-10-29T15:09:16.830445Z",
  "operation": "CREATE",
  "schema": "public",
  "object": "idx_orders_status",
  "duration_ms": 12.34,
  "status": "success",
  "ddl_statement": "CREATE INDEX CONCURRENTLY IF NOT EXISTS public.idx_orders_status ON public.orders (status)",
  "confidence": 0.85,
  "index_size_bytes": 0,
  "fragmentation_pct": 0.0,
  "reason": "SARGable query on public.orders: status (5000 calls, 0.1ms avg)"
}
```

### Contenu par niveau de verbosité

| Champ | `minimal` | `standard` | `verbose` |
|-------|:---------:|:----------:|:---------:|
| `timestamp` | ✅ | ✅ | ✅ |
| `operation` | ✅ | ✅ | ✅ |
| `schema` | ✅ | ✅ | ✅ |
| `object` | ✅ | ✅ | ✅ |
| `duration_ms` | ✅ | ✅ | ✅ |
| `status` | ✅ | ✅ | ✅ |
| `ddl_statement` | ❌ | ✅ | ✅ |
| `confidence` | ❌ | ✅ | ✅ |
| `reason` | ❌ | ✅ | ✅ |
| `index_size_bytes` | ❌ | ❌ | ✅ |
| `fragmentation_pct` | ❌ | ❌ | ✅ |

---

## 4. Tracing logs (daemon)

### Format

JSON par défaut (`logging.format = "json"`), ou texte brut (`"plain"`).

### Niveaux

| Niveau | Usage |
|--------|-------|
| `INFO` | Démarrage, état du daemon, propositions, DDL exécutées, « Applied: N proposal(s), skipped (no-op): M » |
| `WARN` | Laya injoignable / deep health FAILED (no fallback), mode DEGRADED, skip par action flag, `REINDEX CONCURRENTLY` auto-commit |
| `ERROR` | Échec DDL, erreur d'analyse, erreur d'application |
| `DEBUG` | Résultats détecteurs, parsing SQL, fenêtres d'observation sautées, skip par confidence |

### Exemples

```json
{"timestamp":"2026-10-30T21:30:12.101Z","level":"INFO","fields":{"message":"Daemon started. Cycle: 60s, state: Healthy"}}
{"timestamp":"2026-10-30T21:30:12.102Z","level":"WARN","fields":{"message":"Laya deep health FAILED at http://127.0.0.1:8080: Laya not reachable ...: connection refused"}}
{"timestamp":"2026-10-30T21:30:12.102Z","level":"WARN","fields":{"message":"Decisions will be SKIPPED (no fallback) until Laya is available."}}
{"timestamp":"2026-10-30T21:30:12.250Z","level":"INFO","fields":{"message":"Analysis complete: 5 proposals (147ms)"}}
{"timestamp":"2026-10-30T21:30:12.251Z","level":"WARN","fields":{"message":"Laya unavailable (Laya not reachable ...): 5 proposals SKIPPED (no fallback policy)."}}
{"timestamp":"2026-10-30T21:30:12.251Z","level":"INFO","fields":{"message":"Applied: 0 proposal(s), skipped (no-op): 0"}}
{"timestamp":"2026-10-30T21:30:12.250Z","level":"DEBUG","fields":{"message":"Server uptime < 10080min (unused threshold): skipping unused-index analysis this cycle"}}
{"timestamp":"2026-10-30T21:30:12.250Z","level":"DEBUG","fields":{"message":"DEGRADED mode: missing-index detection is DISABLED (pg_stat_statements unavailable)"}}
```
