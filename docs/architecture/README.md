# Documentation d'architecture — PGindexAI

Documentation technique complète du daemon PGindexAI : surveillance, analyse et automatisation de la gestion d'index PostgreSQL pilotée par le moteur de décision Laya.

> **Refonte architecturale** — cette documentation couvre la dernière refonte
> (10 correctifs) : politique **sans fallback**, health check **profond** de
> Laya, détection SARGable par **vrai parseur AST** (sqlparser-rs), DDL
> **en ligne** (`CONCURRENTLY`), densité cible par type, convention de
> nommage (63 octets), pool de connexions dimensionné, état **dégradé**
> explicite, **multi-schéma** et **fenêtre d'observation** sur les index
> inutilisés.
>
> **Renforcement sécurité (audit)** : TLS **vérifié** (rustls, chaîne +
> hostname) avec `ssl_root_cert` optionnelle, rôle PG dédié non-superutilisateur,
> installateur durci (`install.sh`) + `uninstall.sh` (`--purge`), `--dry-run`,
> plafond de propositions par cycle, rotation des logs d'audit, validation de
> config avec avertissements, CI (fmt / clippy / test / shellcheck).
> Chaque fichier signale ses propres nouveautés.

## Table des matières

| Doc | Description |
|-----|-------------|
| [01-system-overview](./01-system-overview.md) | Vue d'ensemble, architecture, composants, cycle de vie |
| [02-component-diagrams](./02-component-diagrams.md) | Diagrammes de composants, séquence, données, flux |
| [03-module-reference](./03-module-reference.md) | Référence module par module (API, types, fonctions) |
| [04-protocols.md](./04-protocols.md) | Protocole Laya `/v1/systemone`, requêtes DMV PostgreSQL |
| [05-configuration.md](./05-configuration.md) | Configuration YAML complète, valeurs par défaut, variables d'environnement |
| [06-deployment.md](./06-deployment.md) | Installation, service systemd, monitoring, logging |

## Vue rapide

```
PostgreSQL             Daemon pgindexai              Serveur Laya
──────────             ───────────────              ──────────────
  │                      │                           │
  │── pg_stat_           │                           │
  │   statements         │   (1) deep_health()       │
  │── pg_class           │──────────────────────────▶│  payload de test
  │── pg_index           │◀──────────────────────────│  (réponse validée)
  │── pg_stats           │   FAILED ⇒ AUCUNE DDL     │
  │── information_schema │   (no fallback)           │
  │                      │                           │
  │                      │── cycle 60s ──▶           │
  │                      │                           │
  │                      │  (2) HTTP POST /v1/systemone
  │                      │──────────────────────────▶│  (contexte + choix)
  │                      │                           │── IA décide
  │                      │◀──────────────────────────│  (choice + confidence)
  │                      │                           │
  │                      │── APPLY/IGNORE ──▶        │
  │                      │   DDL en ligne            │
  │◀── CONCURRENTLY /    │   (validation + nettoyage
  │   BEGIN-DROP-COMMIT  │    des index INVALIDE)    │
```

## Points d'entrée

| Commande | Description |
|----------|-------------|
| `pgindexai daemon` | Boucle continue (défaut, cycle 60s) |
| `pgindexai run` | Cycle unique et exit |
| `pgindexai run --dry-run` | Analyse et affichage **seulement** — aucun appel Laya, aucune DDL |
| `pgindexai --config /path daemon` | Config personnalisée |

## Prérequis

- **PostgreSQL 16+** (idéalement avec `pg_stat_statements` activé — sans elle, le daemon passe en mode **dégradé** : détection d'index manquants désactivée)
- **Laya server** (`laya`) en mode serveur sur un port local — **requis pour appliquer des DDL** (politique sans fallback : sans Laya actif, le daemon reste en mode observation)
- **Rust toolchain** (pour compiler depuis les sources)

## Licence

MIT — Voir le fichier [LICENSE](../../LICENSE)
