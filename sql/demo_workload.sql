/* ============================================================================
   demo_workload.sql — Workload significatif pour détection d'anomalies
   ----------------------------------------------------------------------------
   Exécuter : sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql
   
   Objectif : créer des conditions réalistes déclenchant les 3 détecteurs :
   1. Index inutilisés (idx_scan = 0, idx_tup_read = 0)
   2. Index fragmentés (densité mesurable via pg_class)
   3. Requêtes SARGable sans index (pg_stat_statements, >100 appels)
   ============================================================================ */

-- ─────────────────────────────────────────────
-- Phase 0 : Workload utilisateur (génére pg_stat_statements)
-- ─────────────────────────────────────────────
SELECT '=== Phase 0 : Workload utilisateur (génére pg_stat_statements) ===';

-- 0a : Workload orders — shipping_method (AUCUN INDEX, SARGable)
DO $$
DECLARE
    methods TEXT[] := ARRAY['standard','express','overnight','pickup'];
    m TEXT;
    i INT;
BEGIN
    FOR i IN 1..50 LOOP
        FOR m IN SELECT unnest(methods) LOOP
            PERFORM o.id, o.customer_id, o.total_amount, o.order_date
            FROM orders o
            WHERE o.shipping_method = m
            ORDER BY o.order_date DESC
            LIMIT 100;
        END LOOP;
    END LOOP;
END $$;

-- 0b : Workload orders — total_amount (AUCUN INDEX, SARGable)
DO $$
DECLARE
    i INT;
BEGIN
    FOR i IN 1..500 LOOP
        PERFORM o.id, o.customer_id, o.order_date
        FROM orders o
        WHERE o.total_amount > (i * 10)::numeric
        ORDER BY o.total_amount DESC
        LIMIT 50;
    END LOOP;
END $$;

-- 0c : Workload orders — order_date filtre direct (AUCUN INDEX)
DO $$
DECLARE
    i INT;
BEGIN
    FOR i IN 1..500 LOOP
        PERFORM o.id, o.total_amount, o.status
        FROM orders o
        WHERE o.order_date BETWEEN NOW() - (i || ' days')::interval AND NOW()
        ORDER BY o.order_date DESC
        LIMIT 100;
    END LOOP;
END $$;

-- 0d : Workload customers — country (INDEX existe)
DO $$
DECLARE
    countries TEXT[] := ARRAY['France','Germany','Spain','Italy','Netherlands','Belgium'];
    c TEXT;
    i INT;
BEGIN
    FOR c IN SELECT unnest(countries) LOOP
        FOR i IN 1..50 LOOP
            PERFORM u.id, u.email
            FROM customers u
            WHERE u.country = c
            ORDER BY u.id DESC
            LIMIT 50;
        END LOOP;
    END LOOP;
END $$;

-- 0e : Workload products — category (INDEX existe)
DO $$
DECLARE
    cats TEXT[] := ARRAY['Electronics','Clothing','Food','Books','Sports','Home','Toys','Beauty'];
    cat TEXT;
    i INT;
BEGIN
    FOR cat IN SELECT unnest(cats) LOOP
        FOR i IN 1..100 LOOP
            PERFORM p.sku, p.name, p.price
            FROM products p
            WHERE p.category = cat
              AND p.is_active = true
              AND p.price < (i * 10)::numeric
            ORDER BY p.price;
        END LOOP;
    END LOOP;
END $$;

-- 0f : Workload users — role (INDEX existe)
DO $$
DECLARE
    roles TEXT[] := ARRAY['admin','user','moderator'];
    r TEXT;
    i INT;
BEGIN
    FOR r IN SELECT unnest(roles) LOOP
        FOR i IN 1..100 LOOP
            PERFORM u.id, u.username, u.last_login
            FROM users u
            WHERE u.role = r
              AND u.is_active = true
            ORDER BY u.last_login DESC
            LIMIT 50;
        END LOOP;
    END LOOP;
END $$;

-- 0g : Workload order_items — product_id (INDEX existe)
DO $$
DECLARE
    i INT;
BEGIN
    FOR i IN 1..500 LOOP
        PERFORM oi.order_id, oi.quantity, oi.unit_price
        FROM order_items oi
        WHERE oi.product_id = (1 + (i * 7 % 5000))
        ORDER BY oi.order_id DESC;
    END LOOP;
END $$;

-- 0h : Workload orders — status (INDEX existe)
DO $$
DECLARE
    statuses TEXT[] := ARRAY['pending','confirmed','shipped','delivered','cancelled'];
    s TEXT;
    i INT;
BEGIN
    FOR s IN SELECT unnest(statuses) LOOP
        FOR i IN 1..50 LOOP
            PERFORM o.id, o.total_amount
            FROM orders o
            WHERE o.status = s
            ORDER BY o.id DESC
            LIMIT 50;
        END LOOP;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 1 : Workload client (recherche commandes)
-- ─────────────────────────────────────────────
SELECT '=== Phase 1 : Recherche par client ===';

DO $$
DECLARE
    c INT;
BEGIN
    FOR c IN 1..5000 LOOP
        PERFORM o.id, o.order_date, o.total_amount, o.status
        FROM orders o
        WHERE o.customer_id = (1 + (c * 17 % 10000))
          AND o.order_date >= NOW() - interval '90 days'
        ORDER BY o.order_date DESC;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 2 : Workload catalogue (recherche produits)
-- ─────────────────────────────────────────────
SELECT '=== Phase 2 : Recherche produits ===';

DO $$
DECLARE
    cat TEXT;
    cats TEXT[] := ARRAY['Electronics','Clothing','Food','Books','Sports','Home','Toys','Beauty'];
BEGIN
    FOR cat IN SELECT unnest(cats) LOOP
        PERFORM p.sku, p.name, p.price
        FROM products p
        WHERE p.category = cat
          AND p.is_active = true
          AND p.price < 100
        ORDER BY p.price;

        PERFORM p.sku, p.name
        FROM products p
        WHERE p.price BETWEEN 50 AND 200
          AND p.stock_qty > 10
        ORDER BY p.name;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 3 : Workload admin (rapports)
-- ─────────────────────────────────────────────
SELECT '=== Phase 3 : Rapports admin ===';

DO $$
DECLARE
    d DATE;
BEGIN
    FOR d IN SELECT generate_series(NOW()::date - 30, NOW()::date, '1 day')::date LOOP
        PERFORM o.order_date::date AS day,
               COUNT(*) AS nb_orders,
               SUM(o.total_amount) AS ca
        FROM orders o
        WHERE o.order_date::date = d
        GROUP BY o.order_date::date;

        PERFORM o.status, COUNT(*)
        FROM orders o
        WHERE o.order_date::date = d
        GROUP BY o.status;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 4 : Workload order_items
-- ─────────────────────────────────────────────
SELECT '=== Phase 4 : Articles de commande ===';

DO $$
DECLARE
    oid INT;
BEGIN
    FOR oid IN SELECT id FROM orders WHERE id % 50 = 0 LIMIT 100 LOOP
        PERFORM oi.product_id, p.name, oi.quantity, oi.unit_price
        FROM order_items oi
        JOIN products p ON p.id = oi.product_id
        WHERE oi.order_id = oid
        ORDER BY oi.quantity DESC;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 5 : Workload utilisateurs
-- ─────────────────────────────────────────────
SELECT '=== Phase 5 : Gestion utilisateurs ===';

SELECT u.username, u.last_login
FROM users u
WHERE u.role = 'admin'
  AND u.is_active = true
ORDER BY u.last_login DESC;

SELECT u.username, u.role
FROM users u
WHERE u.last_login > NOW() - interval '7 days'
ORDER BY u.last_login DESC;

DO $$
DECLARE
    prefix TEXT;
BEGIN
    FOR prefix IN SELECT DISTINCT substring(username, 1, 3) FROM users LIMIT 20 LOOP
        PERFORM u.id, u.username, u.role
        FROM users u
        WHERE u.username LIKE prefix || '%'
        ORDER BY u.username;
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Phase 6 : Génération de fragmentation réaliste
-- ─────────────────────────────────────────────
SELECT '=== Phase 6 : Fragmentation réaliste ===';

-- 6a : Suppression massive de lignes (crée des trous)
DELETE FROM order_items WHERE order_id < 100000;
DELETE FROM orders WHERE id < 100000;

-- 6b : Insertion massive (fill factor bas → fragmentation)
INSERT INTO orders (customer_id, order_date, status, total_amount, shipping_method)
SELECT
    1 + (g % 10000),
    NOW() + (g || ' days')::interval,
    (ARRAY['pending','confirmed','shipped'])[1 + (g % 3)],
    (random() * 5000 + 10)::numeric,
    (ARRAY['standard','express'])[1 + (g % 2)]
FROM generate_series(1, 50000) g;

-- 6c : INSERT order_items
INSERT INTO order_items (order_id, product_id, quantity, unit_price, discount)
SELECT
    100000 + g,
    1 + (g % 5000),
    1 + (g % 10),
    (random() * 500 + 5)::numeric,
    (random() * 0.3)::numeric
FROM generate_series(1, 50000) g;

-- 6d : UPDATE massif (modifie les index en place → fragmentation)
UPDATE orders SET status = 'delivered'
WHERE status = 'pending' AND order_date < NOW() - interval '30 days';

-- 6e : VACUUM FULL pour mesurer la densité après (fill factor = 100 par défaut)
VACUUM FULL orders;
VACUUM FULL order_items;

-- 6f : Créer un index qui sera inutilisé (cible DROP)
CREATE INDEX IF NOT EXISTS idx_orders_temp_status ON orders (status) WHERE status = 'pending';

-- ─────────────────────────────────────────────
-- Phase 7 : Workload répétitif SARGable (pg_stat_statements)
-- ─────────────────────────────────────────────
-- Utiliser EXECUTE ... USING pour que pg_stat_statements regroupe les requêtes
SELECT '=== Phase 7 : Workload SARGable répétitif ===';

-- 7a : 5000 requêtes identiques sur shipping_method (AUCUN INDEX, SARGable)
--     → CIBLE PRINCIPALE : doit atteindre >5000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..5000 LOOP
        EXECUTE 'SELECT id, customer_id, total_amount FROM orders WHERE shipping_method = $1'
        USING 'standard';
    END LOOP;
END $$;

-- 7b : 3000 requêtes identiques sur status (INDEX existe mais on force le scan)
--     → doit atteindre >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT id, total_amount, order_date FROM orders WHERE status = $1'
        USING 'pending';
    END LOOP;
END $$;

-- 7c : 3000 requêtes identiques sur category (INDEX existe)
--     → doit atteindre >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT sku, name, price FROM products WHERE category = $1 AND is_active = true'
        USING 'Electronics';
    END LOOP;
END $$;

-- 7d : 2000 requêtes identiques sur role (INDEX existe)
--     → doit atteindre >2000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..2000 LOOP
        EXECUTE 'SELECT id, username, last_login FROM users WHERE role = $1 AND is_active = true'
        USING 'admin';
    END LOOP;
END $$;

-- 7e : 3000 requêtes identiques sur total_amount (AUCUN INDEX, SARGable)
--     → CIBLE SECONDAIRE : doit atteindre >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT id, customer_id, order_date FROM orders WHERE total_amount > $1'
        USING 1000::numeric;
    END LOOP;
END $$;

-- 7f : 1000 requêtes identiques sur country (INDEX existe)
--     → doit atteindre >1000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..1000 LOOP
        EXECUTE 'SELECT id, email FROM customers WHERE country = $1'
        USING 'France';
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- Fin du workload
-- ─────────────────────────────────────────────
SELECT '=== Workload terminé ===';

-- Forcer la mise à jour des stats
ANALYZE;

-- ─────────────────────────────────────────────
-- Rapport : indexes utilisés vs inutilisés
-- ─────────────────────────────────────────────
SELECT '--- Index usage (scans = 0 → cible DROP) ---';

SELECT
    schemaname || '.' || relname AS table,
    indexrelname AS index_name,
    idx_scan AS scans,
    pg_size_pretty(pg_relation_size(indexrelid)) AS size,
    idx_tup_read AS tuples_read
FROM pg_stat_user_indexes
WHERE idx_scan = 0
ORDER BY pg_relation_size(indexrelid) DESC;

-- ─────────────────────────────────────────────
-- Rapport : fragmentation (densité réelle via pg_class)
-- ─────────────────────────────────────────────
SELECT '--- Fragmentation (densité pg_class) ---';

SELECT
    c.relname AS table_name,
    i.relname AS index_name,
    pg_size_pretty(c2.relpages * 8192) AS index_size,
    c2.relpages AS pages,
    c.reltuples AS estimated_rows,
    CASE
        WHEN c2.relpages > 0 THEN ROUND(
            (c.reltuples::float / GREATEST(c2.relpages, 1))::numeric, 0
        )
        ELSE 0
    END AS rows_per_page
FROM pg_class c
JOIN pg_class i ON i.relkind = 'i'
JOIN pg_index ix ON ix.indrelid = c.oid AND ix.indexrelid = i.oid
JOIN pg_class c2 ON c2.oid = i.oid
WHERE c.relname IN ('orders', 'order_items')
  AND c2.relpages > 0
ORDER BY c2.relpages DESC;

-- ─────────────────────────────────────────────
-- Rapport : requêtes SARGable détectées par pg_stat_statements
-- ─────────────────────────────────────────────
SELECT '--- Top requêtes par exec_time (pg_stat_statements) ---';

SELECT
    queryid,
    LEFT(query, 120) AS query_short,
    calls,
    ROUND((total_exec_time / calls)::numeric, 2) AS avg_ms,
    ROUND((total_exec_time / 1000)::numeric, 2) AS total_s
FROM pg_stat_statements
WHERE dbid = (SELECT oid FROM pg_database WHERE datname = 'pgindexai_demo')
  AND query !~ '^SET|^BEGIN|^COMMIT|^ANALYZE|^DELETE FROM|^INSERT INTO|^VACUUM|^UPDATE '
  AND calls > 100
ORDER BY total_exec_time DESC
LIMIT 30;

-- ─────────────────────────────────────────────
-- Rapport : indexes inutilisés détectés
-- ─────────────────────────────────────────────
SELECT '--- Indexes inutilisés (idx_scan=0, idx_tup_read=0) ---';

SELECT
    schemaname || '.' || relname AS table,
    indexrelname AS index_name,
    pg_size_pretty(pg_relation_size(indexrelid)) AS size
FROM pg_stat_user_indexes
WHERE idx_scan = 0
  AND idx_tup_read = 0
ORDER BY pg_relation_size(indexrelid) DESC;
