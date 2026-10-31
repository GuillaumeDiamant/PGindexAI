# 01 — Vue d'ensemble du système

> **Nouveautés (refonte architecturale)** :
> 1. État de dégradation explicite quand `pg_stat_statements` est absent
> 2. Politique **« sans fallback »** : Laya injoignable → aucune DDL appliquée
> 3. Détection SARGable par **vrai parseur SQL** (sqlparser-rs, AST)
> 4. Exécution DDL **en ligne** (`CONCURRENTLY`) + validation post-DDL et
>    nettoyage des index INVALIDE
> 5. **Densité cible par type de colonne** (estimation de taille des index)
> 6. **Convention de nommage explicite** + validation de longueur (63 octets)
> 7. **Pool de connexions dimensionné** (taille + timeout configurables)
> 8. **Health check profond** de Laya (payload de test réel, réponse validée)
> 9. **Support multi-schéma** (périmètre d'analyse configurable)
> 10. **Seuil temporel** sur les index « inutilisés » (fenêtre d'observation)
>
> **Renforcement sécurité (audit)** :
> 11. **TLS vérifié** (rustls) sur la connexion PostgreSQL + **rôle PG dédié
>     non-superutilisateur** (`pgindexai`)
> 12. Installateur **durci** (`install.sh` : sans `eval`, pg_hba ciblée, config
>     0600, merge `shared_preload_libraries`) + mode `--dry-run` +
>     `uninstall.sh` idempotent (`--purge`, `--clean-legacy`)
> 13. **Plafond de propositions** par cycle, rotation des logs, **CI**
>     (fmt / clippy / test / shellcheck), validation de config avec
>     avertissements clés inconnues

## 1. Objectif

PGindexAI est un **daemon autonome** qui surveille en continu une base PostgreSQL, détecte les anomalies d'index (inutilisés, fragmentés, manquants) et propose des actions correctives **exclusivement** validées par le moteur de décision Laya (IA) — **aucune décision n'est prise à sa place** (pas de fallback heuristique).

## 2. Architecture globale

```
┌───────────────────────────────────────────────────────────────────────────┐
│                             pgindexai daemon                              │
│                                                                           │
│  ┌────────────────┐     ┌─────────────────────┐     ┌──────────────────┐ │
│  │    Analyzer    │────▶│     LayaClient      │────▶│   index_log      │ │
│  │                │     │  build_payload()    │     │   Writer         │ │
│  │  3 détecteurs  │     │  deep_health()      │     │  (JSON file)     │ │
│  │ + DaemonState  │     │  decision()         │     │                  │ │
│  └───────┬────────┘     │  /v1/systemone      │     └──────────────────┘ │
│          │              └──────────┬──────────┘                          │
│          │                         │                                      │
│          │              health check PROFOND avant chaque lot             │
│          │              Laya injoignable → AUCUNE DDL (no fallback)       │
│  ┌───────▼─────────────────────────────────────────────────────────────┐ │
│  │   DDL Engine (en ligne : CONCURRENTLY + validation post-DDL)        │ │
│  │   CREATE / DROP / REINDEX ─────────────────────────▶  PostgreSQL    │ │
│  └─────────────────────────────────────────────────────────────────────┘ │
│                                                                           │
│  Transverses : state.rs (Healthy/Degraded) · naming.rs (idx_<t>_<c>,     │
│  63 octets) · config (multi-schéma, pool, densités par type)             │
└───────────────────────────────────────────────────────────────────────────┘
                                 │
                       HTTP POST ▼
                 ┌────────────────────────────┐
                 │      Serveur Laya (IA)      │
                 │   http://127.0.0.1:8080     │
                 │   /v1/systemone             │
                 └────────────────────────────┘
```

## 3. Composants

| Composant | Module | Rôle |
|-----------|--------|------|
| **Analyzer** | `src/analyzer.rs` | Moteur central — orchestre les 3 détecteurs, porte le `DaemonState`, applique la politique sans fallback |
| **DaemonState** | `src/state.rs` | État `Healthy` / `Degraded` (pg_stat_statements) — en dégradé, la détection d'index manquants est désactivée |
| **Naming** | `src/naming.rs` | Convention `idx_<table>_<col>`, validation d'identifiant, limite 63 octets, hash court (FNV-1a base 36) |
| **Detector : Unused Indexes** | `src/db/queries.rs::get_unused_indexes` | Index jamais scannés **depuis le redémarrage** (`idx_scan = 0`, `idx_tup_read = 0`) + gate fenêtre d'observation |
| **Detector : Fragmentation** | `src/db/queries.rs::get_fragmented_indexes` | Index **B-tree uniquement** ; score heuristique (dead 50 %, mod 30 %, bonus non-scanné 20 %) |
| **Detector : SARGable** | `src/db/queries.rs::get_missing_sargable_indexes` | Vrai parseur AST (sqlparser) + types réels (`information_schema`) + `null_frac` (`pg_stats`) + densités cibles |
| **LayaClient** | `src/laya/mod.rs` | Client HTTP — **`deep_health()`** (payload de test réel, réponse validée), `decision()`, `build_payload()` |
| **DDL Engine** | `src/ddl.rs` | Exécution **en ligne** : `CONCURRENTLY` en auto-commit (CREATE/REINDEX), DROP en BEGIN/COMMIT, validation post-DDL + nettoyage des index INVALIDE |
| **IndexLog Writer** | `src/index_log.rs` | Historisation structurée des opérations (JSON, rotation de fichiers) |
| **Config** | `src/config.rs` | Chargement YAML — multi-schéma, pool, densités par type, seuils |
| **db Pool** | `src/db/mod.rs` | Pool deadpool-postgres — **taille max + timeout d'attente configurables** + **TLS rustls** (chaîne + hostname toujours vérifiés, racines web PKI + PEM optionnel) |

## 4. Cycle de vie

### Démarrage

```
main()
  ├── parse CLI args (--config)
  ├── load config (YAML)
  ├── init tracing (JSON ou plain)
  ├── create PostgreSQL pool (taille + timeout configurables)
  ├── check pg_stat_statements  →  DaemonState (Healthy | Degraded)
  │     └── si absent : WARN "DEGRADED mode" (détection d'index manquants OFF)
  ├── create LayaClient
  ├── deep_health()  (payload de test réel envoyé à /v1/systemone)
  │     └── si FAILED : WARN "Decisions will be SKIPPED (no fallback)"
  └── match command:
        ├── daemon → loop { analyze_cycle() → apply_proposals() → sleep(cycle_seconds) }
        └── run   → analyze_cycle() → apply_proposals() → exit
```

### Cycle d'analyse (itératif en daemon)

```
1. analyze_unused()                     [si actions.drop_unused]
    ├── observation_window_ok()         ← NOUVEAU (fix #10)
    │     └── uptime serveur < unused_minutes → SAUT (les compteurs sont
    │         remis à zéro au redémarrage ; pas de fausse détection)
    ├── get_unused_indexes()  → pg_stat_user_indexes WHERE idx_scan = 0 AND idx_tup_read = 0
    └── for each unused index:
         └── Proposal { action: DropIndex,
                        ddl: "DROP INDEX IF EXISTS schema.idx",
                        reason: "… never scanned since <redémarrage>. Size: N bytes." }

2. analyze_fragmentation()              [si actions.rebuild_fragmented]
    ├── get_fragmented_indexes()  → pg_stat_user_indexes + pg_class + pg_am (btree) + pg_stat_user_tables
    └── for each index (score ≥ fragmentation_pct, défaut 70%):
         └── Proposal { action: RebuildIndex, ddl: "REINDEX INDEX CONCURRENTLY schema.idx",
                        reason: "fragmentation at X% (threshold: Y%)" }

3. analyze_missing_sargable()           [DÉSACTIVÉ si DaemonState::Degraded]
    ├── get_missing_sargable_indexes(schemas)
    │     ├── pg_stat_statements top 50 (calls > 10, DDL et pg_* exclus)
    │     ├── parse_query_for_sargable()  ← NOUVEAU : AST sqlparser (fix #3)
    │     ├── périmètre multi-schéma (fix #9) + exclusion tables pg_*
    │     ├── filtre colonnes déjà couvertes (pg_indexes)
    │     └── fetch_column_metadata() : type réel, nullabilité, null_frac (fix #5)
    └── for each uncovered column (calls ≥ min_sargable_rows):
         ├── naming::generate_index_name()  ← NOUVEAU (fix #6)
         ├── densité par bucket de type → taille estimée (fix #5)
         ├── null_frac > 0.10 → "WHERE col IS NOT NULL" (index filtré)
         └── Proposal { action: CreateIndex | CreateFilteredIndex,
                        ddl: "CREATE INDEX CONCURRENTLY IF NOT EXISTS schema.idx ON …",
                        reason: "SARGable query … type: …, density …, est. size …" }

4. apply_proposals()
    ├── deep_health()  ← NOUVEAU (fix #8) : 1 seul appel par lot
    │     └── FAILED → TOUTES les propositions SKIPPED (fix #2 : no fallback)
    ├── for each proposal:
    │   ├── build_payload(reason, criteria)  (critères dépendent du type)
    │   ├── laya.decision(payload) → { choice, confidence }
    │   ├── confidence ≥ min_confidence (0.7) ?  sinon skip
    │   ├── choice appliqué ?  sinon skip (debug)
    │   ├── action flag enabled (actions.*) ?  sinon skip (warn)
    │   └── match proposal.action:
    │         ├── DropIndex            → ddl::execute_drop_index()
    │         ├── RebuildIndex         → ddl::execute_reindex()
    │         └── CreateIndex*         → ddl::execute_create_index()
    │              (exécution en ligne : CONCURRENTLY, fix #4)
    └── return applied proposals
```

## 5. Détection des anomalies

### 5.1 Index inutilisés

**Source** : `pg_stat_user_indexes` + `pg_index`

**Gate préalable (nouveau)** : l'uptime du serveur doit être ≥ `thresholds.unused_minutes`,
sinon l'analyse est sautée (les compteurs `pg_stat_*` sont remis à zéro au redémarrage).

**Critères** :
- `idx_scan = 0` ET `idx_tup_read = 0` (jamais scanné **depuis le redémarrage**)
- `indisvalid = true` (index valide)
- `indisprimary = false` (pas une clé primaire)
- `indisunique = false` (pas un index unique)

**Action proposée** : `DROP INDEX IF EXISTS schema.index`

Le contexte envoyé à Laya inclut la date du redémarrage (`installed_at`) et la
durée minimale d'observation — le moteur de décision peut pondérer sa confiance
sur la solidité de l'évidence.

### 5.2 Index fragmentés

**Source** : `pg_stat_user_indexes` + `pg_index` + `pg_class` + `pg_am` + `pg_stat_user_tables`

**Périmètre (nouveau)** : index **B-tree uniquement** (`amname = 'btree'`) —
les index GiST/GIN/SP-GiST/BRIN n'ont pas la même notion de fragmentation.

**Score heuristique** (multi-critères pondéré) :

| Critère | Pondération |
|---------|-------------|
| `n_dead_tup / (n_live_tup + n_dead_tup)` | 50 % |
| `n_mod_since_analyze / n_live_tup` (bornée à 1.0) | 30 % |
| Bonus : index volumineux non scanné (`idx_scan = 0` ET `relpages > 100`) | 20 % |

**Seuil de décision** : `fragmentation_pct ≥ thresholds.fragmentation_pct` (défaut 70 %)

**avg_leaf_density** : `index_tuples / index_pages / 100` borné à `[0, 1]`
(proxy de la densité tuples/page ; B-tree PostgreSQL cible ~90 % de remplissage)

**Action proposée** : `REINDEX INDEX CONCURRENTLY schema.index`

### 5.3 Requêtes SARGable sans index

**Source** : `pg_stat_statements` + parseur **sqlparser-rs** (nouveau) + `information_schema.columns` + `pg_stats` + `pg_class`

**Filtres** :
- `calls > 10` (minimum observé)
- `calls ≥ min_sargable_rows` (défaut 100)
- Exclusion DDL, requêtes système, catalogues `pg_*`
- **Périmètre multi-schéma** : `analysis.schemas` non vide → seules ces tables

**SARGable détecté (AST)** :
- `col = v`, `col > v`, `col < v`, `col >= v`, `col <= v` (valeur non-colonne)
- `col IN (v1, …)` (non-négé), `col BETWEEN a AND b` (non-négé)
- `col IS [NOT] NULL`
- `col LIKE 'pref…%'` / `ILIKE` (motif **préfixe** uniquement)
- `AND` / `OR` / parenthèses imbriquées (parcours récursif)

**Rejeté (non-SARGable)** :
- `NOT ( … )`, `IS [NOT] DISTINCT FROM`
- `col NOT IN (…)`, `col NOT BETWEEN …`, `col NOT LIKE …`
- Fonctions sur la colonne (`LOWER(col)`, `UPPER(col)`, `CAST(col …)`)
- Comparaison colonne-contre-colonne
- `LIKE`/`ILIKE` avec joker en tête ou motif non littéral

**Enrichissement (nouveau)** : pour chaque colonne candidate, le daemon charge
le **type réel** (`information_schema.columns`), la **nullabilité**, la
**fraction de NULL** (`pg_stats.null_frac`), et la **cardinalité** de la table
(`pg_class.reltuples`) pour estimer la taille du futur index via la **densité
cible du bucket de type** (`density_by_type.*`).

**Index existant** : vérification via `pg_indexes` — les colonnes déjà couvertes
(1re position) sont filtrées.

**Action proposée** :
- Standard : `CREATE INDEX CONCURRENTLY IF NOT EXISTS schema.idx_table_col ON schema.table(col)`
- Filtré (si `null_frac > 0.10`) : `… WHERE col IS NOT NULL`
- Nom d'index : `idx_<table>_<col>` validé (≤ 63 octets, hash court si nécessaire)

## 6. Décision Laya

Avant chaque lot : **health check profond** (payload de test réel envoyé à
`/v1/systemone`, réponse validée structurellement). S'il échoue, **toutes**
les propositions du lot sont sautées — le daemon ne décide jamais à la place
de Laya (politique « sans fallback »).

Pour chaque proposition, le daemon envoie un payload **spécialisé** (le daemon
sait déjà l'action qu'il propose ; Laya arbitre appliquer / ignorer) :

```json
{
  "state": "Index idx_products_stock on public.products never scanned since 2026-10-30T09:32:16Z. Size: 90112 bytes. Observed unused for at least 10080min.",
  "questions": {
    "action": {
      "type": "choice",
      "instructions": "Choose the single best action for this PostgreSQL index situation.",
      "criteria": {
        "0": "Keep: index might be needed later",
        "1": "Drop: index is unused — …"
      }
    }
  }
}
```

| Proposition | `"0"` | `"1"` | `"2"` |
|-------------|-------|-------|-------|
| `DROP INDEX` | Keep (ignoré) | **APPLY** | — |
| `REINDEX` | Keep (ignoré) | **APPLY** | — |
| `CREATE INDEX` | Ignore | **APPLY** (standard) | **APPLY** (filtré) |

Réponse Laya :

```json
{
  "model": "typed-decisions",
  "answers": {
    "action": {
      "type": "choice",
      "choice": "1",
      "confidence": 0.85,
      "probabilities": { "0": 0.15, "1": 0.85 }
    }
  },
  "usage": { "input_tokens": 256, "output_tokens": 12 }
}
```

**Application conditionnelle** :
- `confidence < min_confidence` (défaut 0.7) → skip (aucune heuristique)
- `choice` = APPLY pour ce type de proposition → exécution DDL en ligne (CONCURRENTLY)
- `choice` = Keep/Ignore → log uniquement
- `choice` inattendu pour ce type de proposition → `Ignore` (skip, log)

**Sécurité** : chaque action DDL est couplée à un flag `actions.*` dans la
config. Exemple : `drop_unused: false` bloque tout DROP, `create_index: true`
autorise les CREATE.

## 7. Logging

### 7.1 Daemon logs (tracing)

Format JSON par défaut, niveau configurable via `RUST_LOG` ou `logging.level`.

### 7.2 Index operation logs (fichier)

Fichier JSONL structuré par défaut à `/var/log/pgindexai/index_operations.log` :

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
  "reason": "SARGable query on public.orders: status (type: character varying, 5000 calls, 0.1ms avg, null_frac: 0.00). Target density for varchar: 30 tuples/page, estimated index size: 129536 bytes."
}
```

**Niveaux de verbosité** :

| Niveau | Contenu |
|--------|---------|
| `minimal` | timestamp, type CRUD, durée, schéma, nom objet |
| `standard` | + DDL de l'index modifié, confiance, raison |
| `verbose` | + taille, fragmentation, détails techniques |

**Rotation** : 10 MB par fichier, 5 fichiers max (configurable).

## 8. Dépendances

| Crate | Rôle | Version |
|-------|------|---------|
| `tokio` | Runtime asynchrone | 1.x |
| `tokio-postgres` | Driver PostgreSQL | 0.7 |
| `deadpool-postgres` | Pool de connexions | 0.14 |
| `reqwest` | Client HTTP pour Laya | 0.12 |
| `serde` / `serde_yaml` | Sérialisation config | 1.x / 0.9 |
| `serde_json` | Sérialisation logs | 1.x |
| `clap` | Parsing CLI | 4.x |
| `tracing` / `tracing-subscriber` | Logging structuré | 0.1 / 0.3 |
| `chrono` | Gestion timestamps | 0.4 |
| `thiserror` | Gestion erreurs | 2.x |
| `dirs` | Chemin config par défaut | 6.x |
| `tokio-postgres-rustls` | Connecteur TLS pour PostgreSQL | 0.14 |
| `rustls` | Implémentation TLS (provider ring) | 0.23 |
| `rustls-pemfile` | Lecture des certificats `ssl_root_cert` (PEM) | 2.x |
| `webpki-roots` | Racines web PKI (Mozilla) par défaut | 0.26 |
| `sqlparser` | **Vrai parseur SQL (AST) pour la détection SARGable** | 0.52 |

## 9. Garanties de comportement (nouveau)

| Situation | Comportement garanti |
|-----------|----------------------|
| `pg_stat_statements` absent | Daemon `DEGRADED` : unused + fragmentation actifs, détection d'index manquants **désactivée** (pas de source de données) |
| Laya injoignable / réponse invalide | **Aucune DDL appliquée** — propositions générées et journalisées, application reportée au prochain cycle (no fallback) |
| Serveur PostgreSQL redémarré depuis < `unused_minutes` | Analyse « index inutilisés » **sautée** pour le cycle (pas de fausse détection) |
| Index B-tree absent | Les détecteurs de fragmentation ne proposent que des index B-tree |
| Laya hors ligne au démarrage | `deep_health()` FAILED → warning explicite ; le daemon continue (mode observation) |
| Nom d'index > 63 octets | Tronqué + suffixe hash court (FNV-1a base 36) — identifiant toujours valide |
| `ssl_mode ≠ disable` + certificat non vérifiable (chaîne ou hostname) | **Connexion refusée** (rustls vérifie toujours chaîne + hostname) — aucun mode « accept-insecure » |
| Hôte socket Unix (`/…`) + `ssl_mode ≠ disable` | Erreur explicite au démarrage (TLS impossible sur socket avec ce driver) |
| Identifiants sortant du catalogue PG (schéma/table/index) | Proposition **rejetée** avant envoi à Laya (aucune injection DDL) |
| Clé inconnue dans la config YAML | Chargement OK + **avertissement** précisant la clé ignorée |
| Nombre de propositions > `max_proposals_per_cycle` | Seules les N premières (ordre des détecteurs) sont envoyées à Laya |
