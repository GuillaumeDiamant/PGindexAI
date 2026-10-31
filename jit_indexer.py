#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
jit_indexer.py — POC d'indexation prédictive "Juste-à-Temps" (JIT Indexing)
============================================================================

Orchestrate le pipeline en 3 étapes :

  1. SQL Server (VM A, 192.168.1.50)
     -> extraction des candidats "index manquant" depuis
        sys.dm_db_missing_index_group_stats / sys.dm_db_missing_index_groups /
        sys.dm_db_missing_index_details, complétés du volume de la table
        (sys.dm_db_partition_stats) et du trafic (sys.dm_db_index_usage_stats).

  2. Laya (VM B, 192.168.1.60:8080) — serveur HTTP du projet laya.cpp
     -> POST /v1/systemone (schéma JEV) avec un "state" textuel concis et une
        question de type "choice" à 3 options fermées :
            0 : Ignorer (aucune action)
            1 : Index non-clusteré standard sur les colonnes suggérées
            2 : Index filtré WHERE <col> IS NOT NULL
     -> réponse typée : answers.action.choice = "0" | "1" | "2"
        (+ confidence + probabilities). Latence visée < 5 ms, mesurée et
        journalisée à chaque appel.

  3. SQL Server (VM A)
     -> application de l'action physique choisie :
           CREATE NONCLUSTERED INDEX ... WITH (ONLINE = ON)
        pour ne pas bloquer la production (édition Enterprise/Developer ;
        repli automatique sur ONLINE = OFF avec avertissement sinon).

Exemples d'utilisation
-----------------------
  # Auto-test hors ligne (aucun SQL Server / Laya requis)
  python jit_indexer.py --selftest

  # Récupération de la liste des candidats, DDL simulé, Laya réel
  python jit_indexer.py --sql-password 'P@ss' --target-db POCDemo --dry-run

  # Test du côté SQL sans VM B (décision mock déterministe), DDL simulé
  python jit_indexer.py --sql-password 'P@ss' --mock-laya --dry-run

  # Exécution réelle (Laya décide, DDL ONLINE appliqué)
  python jit_indexer.py --sql-password 'P@ss' --target-db POCDemo

  # En mode démo, abaisser les seuils (le trafic POC est faible)
  python jit_indexer.py --sql-password 'P@ss' --min-score 1 --min-impact 0.3

AVERTISSEMENT (POC)
  Authentification 'sa', Encrypt=no et host 0.0.0.0 sont acceptables UNIQUEMENT
  dans un laboratoire isolé. Ne jamais réutiliser ce montage tel quel en
  production (TLS, moindres privilèges, LAYA_API_KEY, etc.).
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import sys
import time
from dataclasses import dataclass
from datetime import datetime
from typing import Dict, List, Optional, Tuple

# Dépendances optionnelles à l'import : le script reste importable (et
# --selftest reste utilisable) sur une machine sans pyodbc/requests.
try:
    import pyodbc  # type: ignore
except ImportError:  # pragma: no cover
    pyodbc = None

try:
    import requests  # type: ignore
except ImportError:  # pragma: no cover
    requests = None

LOG = logging.getLogger("jit-indexer")


class LayaError(RuntimeError):
    """Erreur d'appel ou de réponse du serveur Laya."""


# ---------------------------------------------------------------------------
# Structures
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class Candidate:
    """Candidat 'index manquant' issu des DMV de SQL Server."""

    object_id: int
    schema_name: str
    table_name: str
    index_definition: str
    equality_columns: Optional[str]
    inequality_columns: Optional[str]
    included_columns: Optional[str]
    user_seeks: int
    avg_total_user_cost: float
    avg_user_impact: float  # fraction 0..1 (0.82 = 82 %)
    last_user_seek: Optional[datetime]
    score: float  # user_seeks * avg_total_user_cost


@dataclass
class Decision:
    """Décision typée renvoyée par Laya (ou par le mock)."""

    choice: int  # 0 | 1 | 2
    confidence: Optional[float]
    probabilities: Dict[str, float]
    latency_ms: float
    source: str = "laya"


# ---------------------------------------------------------------------------
# Connexion SQL Server (pyodbc, TLS désactivé — POC uniquement)
# ---------------------------------------------------------------------------
DRIVER_CANDIDATES = [
    "ODBC Driver 18 for SQL Server",
    "ODBC Driver 17 for SQL Server",
    "ODBC Driver 13 for SQL Server",
    "SQL Server Native Client 11.0",
    "SQL Server",
]


def connect_sql(host: str, port: int, user: str, password: str,
                database: str, driver_override: Optional[str]):
    """Connexion distante pyodbc avec TrustServerCertificate=yes / Encrypt=no."""
    if pyodbc is None:
        sys.exit("pyodbc manquant -> pip install pyodbc "
                 "(+ driver ODBC Microsoft SQL Server)")
    available = {d for d in pyodbc.drivers()}
    candidates = ([driver_override] if driver_override
                  else [d for d in DRIVER_CANDIDATES if d in available]
                  or DRIVER_CANDIDATES[:1])
    server = f"{host},{port}"
    last_err = "aucun essai"
    for driver in candidates:
        conn_str = (
            f"DRIVER={{{driver}}};"
            f"SERVER={server};"
            f"DATABASE={database};"
            f"UID={user};"
            f"PWD={password};"
            "TrustServerCertificate=yes;"
            "Encrypt=no;"
            "Connection Timeout=10;"
        )
        try:
            conn = pyodbc.connect(conn_str)
            LOG.info("SQL OK: %s (driver: %s | TLS: off, certificat: trusté POC)",
                     server, driver)
            return conn
        except pyodbc.Error as exc:
            last_err = f"{driver}: {str(exc).strip().splitlines()[0]}"
            LOG.warning("connexion échouée — %s", last_err)
    sys.exit(f"Impossible de joindre SQL Server ({last_err}). "
             "Vérifiez le réseau, le port 1433 et le driver ODBC SQL Server 2019+.")


# ---------------------------------------------------------------------------
# Requêtes SQL
# ---------------------------------------------------------------------------
SQL_MISSING_INDEXES = """
SET NOCOUNT ON;
SELECT TOP (@limit)
    d.object_id,
    OBJECT_SCHEMA_NAME(d.object_id) AS schema_name,
    OBJECT_NAME(d.object_id)        AS table_name,
    d.index_definition,
    d.equality_columns,
    d.inequality_columns,
    d.included_columns,
    gs.user_seeks,
    gs.avg_total_user_cost,
    gs.avg_user_impact,
    gs.last_user_seek,
    CAST(gs.user_seeks * gs.avg_total_user_cost AS FLOAT) AS score
FROM sys.dm_db_missing_index_group_stats gs
JOIN sys.dm_db_missing_index_groups g
    ON  gs.database_id        = g.database_id
    AND gs.index_group_handle = g.index_group_handle
JOIN sys.dm_db_missing_index_details d
    ON  g.database_id  = d.database_id
    AND g.index_handle = d.index_handle
WHERE d.database_id = DB_ID()
ORDER BY score DESC;
"""

SQL_VOLUME = """
SET NOCOUNT ON;
SELECT ISNULL(SUM(CASE WHEN p.index_id IN (0, 1) THEN p.rows END), 0) AS row_count,
       ISNULL(SUM(p.used_pages), 0) * 8.0 / 1024.0                    AS used_mb
FROM sys.dm_db_partition_stats p
WHERE p.object_id = @object_id;
"""

SQL_TRAFFIC = """
SET NOCOUNT ON;
SELECT ISNULL(SUM(user_seeks), 0) AS user_seeks,
       ISNULL(SUM(user_scans), 0) AS user_scans,
       ISNULL(SUM(user_lookups), 0) AS user_lookups,
       ISNULL(SUM(user_updates), 0) AS user_updates,
       MAX(last_user_seek)          AS last_user_seek
FROM sys.dm_db_index_usage_stats
WHERE object_id = @object_id AND database_id = DB_ID();
"""

SQL_COLUMN_NULLABILITY = """
SET NOCOUNT ON;
SELECT name, is_nullable FROM sys.columns WHERE object_id = @object_id;
"""


def fetch_candidates(conn, limit: int, min_score: float, min_impact: float) -> List[Candidate]:
    """Extrait les candidats, déduplique par table (meilleur score retenu)."""
    with conn.cursor() as cur:
        cur.execute(SQL_MISSING_INDEXES, {"limit": max(1, limit * 5)})
        rows = cur.fetchall()

    best: Dict[Tuple[str, str], Candidate] = {}
    for r in rows:
        cand = Candidate(
            object_id=r.object_id,
            schema_name=r.schema_name,
            table_name=r.table_name,
            index_definition=r.index_definition or "",
            equality_columns=r.equality_columns,
            inequality_columns=r.inequality_columns,
            included_columns=r.included_columns,
            user_seeks=int(r.user_seeks or 0),
            avg_total_user_cost=float(r.avg_total_user_cost or 0.0),
            avg_user_impact=float(r.avg_user_impact or 0.0),
            last_user_seek=r.last_user_seek,
            score=float(r.score or 0.0),
        )
        if cand.score < min_score or cand.avg_user_impact < min_impact:
            continue
        key = (cand.schema_name, cand.table_name)
        if key not in best or cand.score > best[key].score:
            best[key] = cand
    return sorted(best.values(), key=lambda c: c.score, reverse=True)[:limit]


def fetch_table_metrics(conn, object_id: int) -> Dict[str, object]:
    """Volume (lignes, MB) + trafic (seeks/lookups/scans/updates) de la table."""
    with conn.cursor() as cur:
        cur.execute(SQL_VOLUME, {"object_id": object_id})
        vol = cur.fetchone()
        cur.execute(SQL_TRAFFIC, {"object_id": object_id})
        traf = cur.fetchone()
    return {
        "row_count": int(vol.row_count or 0),
        "used_mb": float(vol.used_mb or 0.0),
        "user_seeks": int(traf.user_seeks or 0),
        "user_scans": int(traf.user_scans or 0),
        "user_lookups": int(traf.user_lookups or 0),
        "user_updates": int(traf.user_updates or 0),
        "last_user_seek": traf.last_user_seek,
    }


def server_edition(conn) -> str:
    with conn.cursor() as cur:
        cur.execute("SELECT SERVERPROPERTY('Edition')")
        return str(cur.fetchone()[0] or "")


def index_exists(conn, table_qualified: str, name: str) -> bool:
    with conn.cursor() as cur:
        cur.execute(
            "SELECT COUNT(*) FROM sys.indexes "
            "WHERE object_id = OBJECT_ID(@table) AND name = @name",
            {"table": table_qualified, "name": name},
        )
        return bool(cur.fetchone()[0])


def apply_ddl(conn, sql: str, table_qualified: str, name: str) -> None:
    """Exécute le DDL d'indexation (ONLINE) puis vérifie l'index créé."""
    t0 = time.perf_counter_ns()
    with conn.cursor() as cur:
        cur.execute(sql)
    conn.commit()
    ms = (time.perf_counter_ns() - t0) / 1e6
    with conn.cursor() as cur:
        cur.execute(
            "SELECT type_desc, filter_definition FROM sys.indexes "
            "WHERE object_id = OBJECT_ID(@table) AND name = @name",
            {"table": table_qualified, "name": name},
        )
        row = cur.fetchone()
    LOG.info("index créé en %.0f ms : %s (%s%s)",
             ms, name,
             row.type_desc if row else "?",
             f", filtre=[{row.filter_definition}]" if row and row.filter_definition else "")


# ---------------------------------------------------------------------------
# Analyse de la suggestion + construction du payload Laya
# ---------------------------------------------------------------------------
_INDEX_DEF_RE = re.compile(
    r"ON\s+\[(?P<db>[^\]]*)\]\s*\.\s*\[(?P<schema>[^\]]*)\]\s*\.\s*\[(?P<table>[^\]]*)\]\s*"
    r"\((?P<cols>.*?)\)\s*(?:INCLUDE\s*\((?P<inc>.*?)\))?\s*$",
    re.IGNORECASE | re.DOTALL,
)


def split_columns(blob: Optional[str]) -> List[str]:
    if not blob:
        return []
    out: List[str] = []
    for part in blob.split(","):
        name = part.strip().strip("[]").strip()
        if name:
            out.append(name)
    return out


def parse_index_definition(defn: str) -> Tuple[List[str], List[str]]:
    """Décompose index_definition en (colonnes clés, colonnes INCLUDE)."""
    m = _INDEX_DEF_RE.search(defn or "")
    if not m:
        raise ValueError(f"impossible de parser index_definition: {defn!r}")
    return split_columns(m.group("cols")), split_columns(m.group("inc"))


def choose_filter_column(conn, object_id: int, key_cols: List[str]) -> Tuple[str, bool]:
    """Choisit la meilleure colonne pour WHERE <col> IS NOT NULL.

    Règle POC : première colonne clé NULLABLE (un filtre IS NOT NULL n'a de
    sens que sur une colonne nullable). Repli : première colonne clé.
    """
    nullability: Dict[str, bool] = {}
    try:
        with conn.cursor() as cur:
            cur.execute(SQL_COLUMN_NULLABILITY, {"object_id": object_id})
            nullability = {r.name: bool(r.is_nullable) for r in cur.fetchall()}
    except Exception as exc:  # pragma: no cover
        LOG.warning("nullabilité des colonnes indisponible: %s", exc)
    for col in key_cols:
        if nullability.get(col):
            return col, True
    return (key_cols[0], False) if key_cols else ("", False)


def build_state_text(cand: Candidate, metrics: Dict[str, object],
                     key_cols: List[str], inc_cols: List[str]) -> str:
    """'State' textuel concis (en anglais, ~90-110 tokens, bien sous le budget)."""
    last = ""
    if metrics.get("last_user_seek"):
        age_s = max(0.0, (datetime.now() - metrics["last_user_seek"]).total_seconds())
        if age_s < 180:
            last = "within the last 3 minutes"
        elif age_s < 10800:
            last = f"about {int(age_s // 60)} minutes ago"
        else:
            last = f"about {int(age_s // 3600)} hours ago"
    cols = ", ".join(key_cols)
    inc = f", covering {', '.join(inc_cols)}" if inc_cols else ""
    text = (
        f"Production table {cand.schema_name}.{cand.table_name}: "
        f"{metrics['row_count']:,} rows, {metrics['used_mb']:,.0f} MB used. "
        f"Traffic: {metrics['user_seeks']:,} seeks, {metrics['user_lookups']:,} lookups, "
        f"{metrics['user_scans']:,} scans, {metrics['user_updates']:,} updates"
        + (f"; last seek {last}." if last else ".")
        + f" SQL Server suggests a nonclustered index ({cols}){inc} "
        f"for {cand.user_seeks:,} seeks at {cand.avg_user_impact:.0%} avg user impact "
        f"(score {cand.score:,.0f})."
    )
    return text


def build_criteria(key_cols: List[str], inc_cols: List[str], filter_col: str) -> Dict[str, str]:
    """Les 3 actions physiques fermées (chaque option < 48 tokens — budget Laya)."""
    cols = ", ".join(key_cols)
    inc_part = f" including {', '.join(inc_cols)}" if inc_cols else ""
    return {
        "0": "Ignore: keep current indexes, no change",
        "1": f"Create standard nonclustered index on ({cols}){inc_part}",
        "2": f"Create filtered nonclustered index on ({cols}) WHERE {filter_col} IS NOT NULL",
    }


def build_ddl(schema: str, table: str, key_cols: List[str], inc_cols: List[str],
              filter_col: str, action: int, online_on: bool) -> Tuple[str, str]:
    """Génère (nom, DDL) de l'index choisi, avec WITH (ONLINE = ON/OFF)."""
    stem = re.sub(r"[^A-Za-z0-9]", "_", table)
    colstem = re.sub(r"[^A-Za-z0-9]", "_", "_".join(key_cols))
    name = (f"IXF_{stem}_{colstem}" if action == 2 else f"IX_{stem}_{colstem}")[:128]
    table_q = f"[{schema}].[{table}]"
    cols_q = ", ".join(f"[{c}]" for c in key_cols)
    inc_q = f"\n    INCLUDE ({', '.join(f'[{c}]' for c in inc_cols)})" if inc_cols else ""
    where_q = f"\n    WHERE [{filter_col}] IS NOT NULL" if action == 2 else ""
    online_q = "ONLINE = ON" if online_on else "ONLINE = OFF"
    sql = (
        f"CREATE NONCLUSTERED INDEX [{name}] ON {table_q} ({cols_q}){inc_q}{where_q}\n"
        f"    WITH ({online_q});"
    )
    return name, sql


# ---------------------------------------------------------------------------
# Client Laya (serveur HTTP laya.cpp, endpoint JEV /v1/systemone)
# ---------------------------------------------------------------------------
class LayaClient:
    """Client HTTP du serveur laya.cpp.

    Contrat (vérifié sur le code source de laya.cpp, src/protocol.cpp) :
      requête  : {"state": <texte>, "questions": {"action": {
                    "type": "choice", "instructions": <texte>,
                    "criteria": {"0": "...", "1": "...", "2": "..."}}}}
      réponse  : {"model": "...", "answers": {"action": {
                    "type": "choice", "choice": "1",
                    "confidence": 0.87, "action": {"act_probability": 0.9},
                    "probabilities": {"0": 0.1, "1": 0.8, "2": 0.1}}}, "usage": {...}}
    """

    INSTRUCTIONS = (
        "Choose the single best physical index maintenance action to apply "
        "right now to this production table."
    )

    def __init__(self, base_url: str, timeout_s: float, mock: bool):
        self.base_url = base_url.rstrip("/")
        self.timeout_s = timeout_s
        self.mock = mock
        if not mock and requests is None:
            sys.exit("requests manquant -> pip install requests")
        self._session = None if mock else requests.Session()

    # -- Cycle de vie -----------------------------------------------------
    def wait_ready(self, timeout_s: float = 60.0) -> None:
        """Attend que laya-cli ait chargé son checkpoint (GET /health)."""
        url = f"{self.base_url}/health"
        deadline = time.monotonic() + timeout_s
        last = "non tenté"
        while time.monotonic() < deadline:
            try:
                r = self._session.get(url, timeout=(1.0, 2.0))
                if r.status_code == 200:
                    LOG.info("laya-server prêt: %s", r.text[:160].strip())
                    return
                last = f"HTTP {r.status_code}: {r.text[:120]}"
            except requests.RequestException as exc:
                last = f"{type(exc).__name__}: {exc}"
            time.sleep(0.5)
        raise LayaError(f"laya-server non prêt sur {url} (dernier état: {last})")

    # -- Décision ----------------------------------------------------------
    def decide(self, state: str, criteria: Dict[str, str],
               ctx: Optional[Dict[str, object]] = None) -> Decision:
        if self.mock:
            return self._mock_decision(ctx or {})

        payload = {
            "state": state,
            "questions": {
                "action": {
                    "type": "choice",
                    "instructions": self.INSTRUCTIONS,
                    "criteria": criteria,
                }
            },
        }
        url = f"{self.base_url}/v1/systemone"
        last_err = "aucun essai"
        for attempt in range(1, 4):
            t0 = time.perf_counter_ns()
            try:
                r = self._session.post(url, json=payload, timeout=(1.0, self.timeout_s))
                elapsed_ms = (time.perf_counter_ns() - t0) / 1e6
            except (requests.ConnectionError, requests.Timeout) as exc:
                last_err = f"{type(exc).__name__}: {exc}"
                LOG.warning("appel Laya %d/3 en échec: %s", attempt, last_err)
                time.sleep(0.03 * attempt)
                continue

            if r.status_code in (408, 429, 500, 502, 503, 504):
                last_err = f"HTTP {r.status_code}: {r.text[:200]}"
                LOG.warning("appel Laya %d/3: %s", attempt, last_err)
                time.sleep(0.03 * attempt)
                continue
            if r.status_code != 200:
                # 4xx (400/413/422...) : erreur de payload — pas de retry utile.
                raise LayaError(f"laya HTTP {r.status_code}: {r.text[:500]}")

            try:
                data = r.json()
                answer = data["answers"]["action"]
                choice = int(str(answer["choice"]))
            except (ValueError, KeyError, TypeError) as exc:
                raise LayaError(f"réponse Laya inattendue: {r.text[:500]}") from exc
            if choice not in (0, 1, 2):
                raise LayaError(f"choix inattendu de Laya: {choice!r}")
            return Decision(
                choice=choice,
                confidence=answer.get("confidence"),
                probabilities={str(k): float(v) for k, v in
                               (answer.get("probabilities") or {}).items()},
                latency_ms=round(elapsed_ms, 3),
                source="laya",
            )
        raise LayaError(f"laya inaccessible après 3 tentatives (dernier: {last_err})")

    @staticmethod
    def _mock_decision(ctx: Dict[str, object]) -> Decision:
        """Décision déterministe (POC sans VM B) — couvre les 3 chemins."""
        impact = float(ctx.get("impact", 0.0))
        if impact < 0.30:
            choice, probs = 0, {0: 0.70, 1: 0.20, 2: 0.10}
        elif ctx.get("has_nullable_filter"):
            choice, probs = 2, {0: 0.10, 1: 0.35, 2: 0.55}
        else:
            choice, probs = 1, {0: 0.10, 1: 0.75, 2: 0.15}
        return Decision(
            choice=choice,
            confidence=round(0.5 + 0.4 * min(impact, 1.0), 2),
            probabilities={str(k): v for k, v in probs.items()},
            latency_ms=0.0,
            source="mock",
        )


# ---------------------------------------------------------------------------
# Auto-test hors ligne
# ---------------------------------------------------------------------------
def selftest() -> int:
    defn = ("CREATE NONCLUSTERED INDEX [IX_SalesOrders_CustomerId] "
            "ON [POCDemo].[dbo].[SalesOrders] ([CustomerId], [OrderDate] )INCLUDE ([DueDate])")
    key_cols, inc_cols = parse_index_definition(defn)
    assert key_cols == ["CustomerId", "OrderDate"], key_cols
    assert inc_cols == ["DueDate"], inc_cols

    cand = Candidate(
        object_id=100, schema_name="dbo", table_name="SalesOrders",
        index_definition=defn, equality_columns="CustomerId",
        inequality_columns="OrderDate", included_columns="DueDate",
        user_seeks=120, avg_total_user_cost=0.82, avg_user_impact=0.82,
        last_user_seek=datetime.now(), score=98.4,
    )
    metrics = {"row_count": 500000, "used_mb": 480.0, "user_seeks": 120,
               "user_scans": 3, "user_lookups": 40, "user_updates": 12,
               "last_user_seek": datetime.now()}
    state = build_state_text(cand, metrics, key_cols, inc_cols)
    assert "dbo.SalesOrders" in state and "500,000 rows" in state, state

    criteria = build_criteria(key_cols, inc_cols, "OrderDate")
    assert set(criteria) == {"0", "1", "2"}
    for text in criteria.values():
        assert len(text.split()) <= 20, text

    name1, sql1 = build_ddl("dbo", "SalesOrders", key_cols, inc_cols, "OrderDate", 1, True)
    name2, sql2 = build_ddl("dbo", "SalesOrders", key_cols, inc_cols, "OrderDate", 2, True)
    assert "ONLINE = ON" in sql1 and "WHERE" not in sql1
    assert "ONLINE = ON" in sql2 and "WHERE [OrderDate] IS NOT NULL" in sql2
    assert name1.startswith("IX_") and name2.startswith("IXF_")

    assert LayaClient._mock_decision({"impact": 0.1, "has_nullable_filter": True}).choice == 0
    assert LayaClient._mock_decision({"impact": 0.8, "has_nullable_filter": True}).choice == 2
    assert LayaClient._mock_decision({"impact": 0.8, "has_nullable_filter": False}).choice == 1

    print("SELFTEST OK\n")
    print("  state   :", state)
    print("  criteria:", json.dumps(criteria, indent=2))
    print("  ddl(1)  :", sql1.replace("\n", " "))
    print("  ddl(2)  :", sql2.replace("\n", " "))
    return 0


# ---------------------------------------------------------------------------
# Orchestration principale
# ---------------------------------------------------------------------------
ACTION_LABELS = {0: "IGNORER", 1: "INDEX NON-COMPTABLE", 2: "INDEX FILTRE"}


def setup_logging(verbose: bool) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)-7s %(message)s",
        datefmt="%H:%M:%S",
    )


def print_summary(results: List[Tuple[Candidate, Decision, str, Optional[str], Optional[str]]]) -> None:
    print()
    print("=" * 118)
    print("  JIT INDEXING — SYNTHÈSE DU POC")
    print("=" * 118)
    print(f"  {'Table':<26} {'Score':>9} {'Impact':>8}  {'Laya':>4}  {'Conf':>5}  "
          f"{'Laya_ms':>8}  {'Statut':<10}  Action / index")
    print("-" * 118)
    for cand, dec, status, name, _sql in results:
        conf = f"{dec.confidence:.2f}" if dec.confidence is not None else "  -"
        table = f"{cand.schema_name}.{cand.table_name}"
        label = ACTION_LABELS.get(dec.choice, "?")
        print(f"  {table:<26} {cand.score:>9,.0f} {cand.avg_user_impact * 100:>7.0f}%  "
              f"{dec.choice:>4}  {conf:>5}  {dec.latency_ms:>8.2f}  {status:<10}  "
              f"{label}  {name or '-'}")
    print("=" * 118)


def parse_args(argv: Optional[List[str]]):
    p = argparse.ArgumentParser(
        description="POC JIT Indexing : SQL Server missing indexes -> décision Laya "
                    "(laya.cpp, < 5 ms) -> DDL d'index ONLINE.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    # SQL Server (VM A)
    p.add_argument("--sql-host", default="192.168.1.50")
    p.add_argument("--sql-port", type=int, default=1433)
    p.add_argument("--sql-user", default="sa")
    p.add_argument("--sql-password", default=os.environ.get("SQL_PWD", ""))
    p.add_argument("--sql-driver", default=None, help="Forcer le nom du driver ODBC")
    p.add_argument("--target-db", default="POCDemo")
    # Laya (VM B)
    p.add_argument("--laya-url", default="http://192.168.1.60:8080")
    p.add_argument("--laya-timeout", type=float, default=5.0,
                   help="timeout par appel (s) — la décision doit tenir en < 5 ms")
    p.add_argument("--ready-timeout", type=float, default=60.0,
                   help="délai max d'attente du /health de laya-server (s)")
    # Filtres
    p.add_argument("--limit", type=int, default=3, help="nb de tables traitées")
    p.add_argument("--min-score", type=float, default=1.0,
                   help="seuil score (seeks * avg_cost) — à monter en production")
    p.add_argument("--min-impact", type=float, default=0.30,
                   help="seuil avg_user_impact, fraction 0..1 — à monter en production")
    # Modes
    p.add_argument("--dry-run", action="store_true", help="afficher le DDL sans l'exécuter")
    p.add_argument("--mock-laya", action="store_true",
                   help="décision mock déterministe (tester le SQL sans VM B)")
    p.add_argument("--selftest", action="store_true", help="auto-test hors ligne")
    p.add_argument("--verbose", action="store_true")
    a = p.parse_args(argv)
    if a.selftest:
        return a
    if not a.mock_laya and requests is None:
        p.error("requests manquant -> pip install requests")
    if not a.sql_password:
        p.error("--sql-password (ou variable SQL_PWD) requis")
    return a


def main(argv: Optional[List[str]] = None) -> int:
    args = parse_args(argv)
    setup_logging(args.verbose)
    if args.selftest:
        return selftest()

    # --- 1. Connexion SQL + capacité ONLINE -------------------------------
    conn = connect_sql(args.sql_host, args.sql_port, args.sql_user,
                       args.sql_password, args.target_db, args.sql_driver)
    try:
        edition = server_edition(conn)
        online_on = edition.lower() in ("enterprise", "developer")
        LOG.info("SQL Server: %s -> WITH (ONLINE = %s)", edition, "ON" if online_on else "OFF")
        if not online_on:
            LOG.warning("ONLINE = ON exige l'édition Enterprise/Developer : repli sur "
                        "ONLINE = OFF (verrous classiques de maintenance).")

        # --- 2. Candidates "index manquant" -------------------------------
        candidates = fetch_candidates(conn, args.limit, args.min_score, args.min_impact)
        if not candidates:
            LOG.info("Aucun candidat 'index manquant' (score >= %.0f et impact >= %.0f%%).",
                     args.min_score, args.min_impact * 100)
            LOG.info("Pour le POC : exécutez d'abord sql/demo_workload.sql puis réessayez.")
            return 0
        LOG.info("%d candidat(s) retenu(s):", len(candidates))
        for c in candidates:
            LOG.info("  - %s.%s | score=%.1f | impact=%.0f%% | seeks=%d",
                     c.schema_name, c.table_name, c.score, c.avg_user_impact * 100, c.user_seeks)

        # --- 3. Client Laya -------------------------------------------------
        client = LayaClient(args.laya_url, args.laya_timeout, mock=args.mock_laya)
        if args.mock_laya:
            LOG.warning("MODE MOCK : décisions déterministes (Laya non utilisé).")
        else:
            client.wait_ready(args.ready_timeout)

        # --- 4. Boucle : état -> décision -> DDL ----------------------------
        results: List[Tuple[Candidate, Decision, str, Optional[str], Optional[str]]] = []
        for cand in candidates:
            table_q = f"{cand.schema_name}.{cand.table_name}"
            try:
                key_cols, inc_cols = parse_index_definition(cand.index_definition)
            except ValueError as exc:
                LOG.error("abandon %s: %s", table_q, exc)
                continue
            if not key_cols:
                LOG.error("abandon %s: suggestion sans colonnes", table_q)
                continue

            filter_col, has_nullable = choose_filter_column(conn, cand.object_id, key_cols)
            metrics = fetch_table_metrics(conn, cand.object_id)
            state = build_state_text(cand, metrics, key_cols, inc_cols)
            criteria = build_criteria(key_cols, inc_cols, filter_col)
            LOG.info("STATE    : %s", state)
            LOG.info("CRITERIA : %s", json.dumps(criteria))

            decision = client.decide(state, criteria,
                                     ctx={"impact": cand.avg_user_impact,
                                          "has_nullable_filter": has_nullable})
            LOG.info("DECISION : action=%d (%s) confidence=%s probs=%s "
                     "latence=%.2f ms [%s]",
                     decision.choice, ACTION_LABELS.get(decision.choice, "?"),
                     decision.confidence, decision.probabilities,
                     decision.latency_ms, decision.source)

            if decision.choice == 0:
                LOG.info("aucune action physique sur %s", table_q)
                results.append((cand, decision, "IGNORED", None, None))
                continue

            name, sql = build_ddl(cand.schema_name, cand.table_name, key_cols, inc_cols,
                                  filter_col, decision.choice, online_on)
            if args.dry_run:
                LOG.info("DRY-RUN — SQL qui serait exécuté :\n%s", sql)
                results.append((cand, decision, "DRY-RUN", name, sql))
                continue
            if index_exists(conn, table_q, name):
                LOG.info("l'index %s existe déjà — action passée", name)
                results.append((cand, decision, "EXISTS", name, None))
                continue
            try:
                apply_ddl(conn, sql, table_q, name)
                results.append((cand, decision, "APPLIED", name, sql))
            except pyodbc.Error as exc:
                msg = str(exc).strip().splitlines()[0]
                LOG.error("DDL en échec sur %s : %s", table_q, msg)
                try:
                    conn.rollback()
                except pyodbc.Error:
                    pass
                results.append((cand, decision, "ERROR", name, sql))

        print_summary(results)
        return 0 if all(r[2] in ("APPLIED", "IGNORED", "EXISTS", "DRY-RUN") for r in results) else 1
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
