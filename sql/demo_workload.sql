/* ============================================================================
   demo_workload.sql — Meaningful workload for anomaly detection
   ----------------------------------------------------------------------------
   Run: sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql
   
   Goal: create realistic conditions triggering the 3 detectors:
   1. Unused indexes (idx_scan = 0, idx_tup_read = 0)
   2. Fragmented indexes (measurable density via pg_class)
   3. SARGable queries without index (pg_stat_statements, >100 calls)
   ============================================================================ */

-- ─────────────────────────────────────────────
-- Phase 0: User workload (generates pg_stat_statements)
-- ─────────────────────────────────────────────
SELECT '=== Phase 0: User workload (generates pg_stat_statements) ===';

-- 0a: orders workload — shipping_method (NO INDEX, SARGable)
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

-- 0b: orders workload — total_amount (NO INDEX, SARGable)
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

-- 0c: orders workload — order_date direct filter (NO INDEX)
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

-- 0d: customers workload — country (INDEX exists)
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

-- 0e: products workload — category (INDEX exists)
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

-- 0f: users workload — role (INDEX exists)
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

-- 0g: order_items workload — product_id (INDEX exists)
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

-- 0h: orders workload — status (INDEX exists)
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
-- Phase 1: Client workload (order search)
-- ─────────────────────────────────────────────
SELECT '=== Phase 1: Search by client ===';

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
-- Phase 2: Catalog workload (product search)
-- ─────────────────────────────────────────────
SELECT '=== Phase 2: Product search ===';

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
-- Phase 3: Admin workload (reports)
-- ─────────────────────────────────────────────
SELECT '=== Phase 3: Admin reports ===';

DO $$
DECLARE
    d DATE;
BEGIN
    FOR d IN SELECT generate_series(NOW()::date - 30, NOW()::date, '1 day')::date LOOP
        PERFORM o.order_date::date AS day,
               COUNT(*) AS n_orders,
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
-- Phase 4: order_items workload
-- ─────────────────────────────────────────────
SELECT '=== Phase 4: Order items ===';

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
-- Phase 5: Users workload
-- ─────────────────────────────────────────────
SELECT '=== Phase 5: User management ===';

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
-- Phase 6: Realistic fragmentation generation
-- ─────────────────────────────────────────────
SELECT '=== Phase 6: Realistic fragmentation ===';

-- 6a: Massive row deletion (creates holes)
DELETE FROM order_items WHERE order_id < 100000;
DELETE FROM orders WHERE id < 100000;

-- 6b: Massive insertion (low fill factor → fragmentation)
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

-- 6d: Massive UPDATE (modifies indexes in place → fragmentation)
UPDATE orders SET status = 'delivered'
WHERE status = 'pending' AND order_date < NOW() - interval '30 days';

-- 6e: VACUUM FULL to measure density afterwards (default fill factor = 100)
VACUUM FULL orders;
VACUUM FULL order_items;

-- 6f: Create an index that will be unused (DROP target)
CREATE INDEX IF NOT EXISTS idx_orders_temp_status ON orders (status) WHERE status = 'pending';

-- ─────────────────────────────────────────────
-- Phase 7: Repetitive SARGable workload (pg_stat_statements)
-- ─────────────────────────────────────────────
-- Use EXECUTE ... USING so that pg_stat_statements groups the queries
SELECT '=== Phase 7: Repetitive SARGable workload ===';

-- 7a: 5000 identical queries on shipping_method (NO INDEX, SARGable)
--     → PRIMARY TARGET: must reach >5000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..5000 LOOP
        EXECUTE 'SELECT id, customer_id, total_amount FROM orders WHERE shipping_method = $1'
        USING 'standard';
    END LOOP;
END $$;

-- 7b: 3000 identical queries on status (INDEX exists but we force the scan)
--     → must reach >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT id, total_amount, order_date FROM orders WHERE status = $1'
        USING 'pending';
    END LOOP;
END $$;

-- 7c: 3000 identical queries on category (INDEX exists)
--     → must reach >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT sku, name, price FROM products WHERE category = $1 AND is_active = true'
        USING 'Electronics';
    END LOOP;
END $$;

-- 7d: 2000 identical queries on role (INDEX exists)
--     → must reach >2000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..2000 LOOP
        EXECUTE 'SELECT id, username, last_login FROM users WHERE role = $1 AND is_active = true'
        USING 'admin';
    END LOOP;
END $$;

-- 7e: 3000 identical queries on total_amount (NO INDEX, SARGable)
--     → SECONDARY TARGET: must reach >3000 calls
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..3000 LOOP
        EXECUTE 'SELECT id, customer_id, order_date FROM orders WHERE total_amount > $1'
        USING 1000::numeric;
    END LOOP;
END $$;

-- 7f: 1000 identical queries on country (INDEX exists)
--     → must reach >1000 calls
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
-- End of workload
-- ─────────────────────────────────────────────
SELECT '=== Workload completed ===';

-- Force stats update
ANALYZE;

-- ─────────────────────────────────────────────
-- Report: used vs unused indexes
-- ─────────────────────────────────────────────
SELECT '--- Index usage (scans = 0 → DROP target) ---';

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
-- Report: fragmentation (actual density via pg_class)
-- ─────────────────────────────────────────────
SELECT '--- Fragmentation (pg_class density) ---';

SELECT
    c.relname AS table_name,
    i.relname AS index_name,
    pg_size_pretty((c2.relpages * 8192)::bigint) AS index_size,
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
-- Report: SARGable queries detected by pg_stat_statements
-- ─────────────────────────────────────────────
SELECT '--- Top queries by exec_time (pg_stat_statements) ---';

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
-- Report: detected unused indexes
-- ─────────────────────────────────────────────
SELECT '--- Unused indexes (idx_scan=0, idx_tup_read=0) ---';

SELECT
    schemaname || '.' || relname AS table,
    indexrelname AS index_name,
    pg_size_pretty(pg_relation_size(indexrelid)) AS size
FROM pg_stat_user_indexes
WHERE idx_scan = 0
  AND idx_tup_read = 0
ORDER BY pg_relation_size(indexrelid) DESC;
