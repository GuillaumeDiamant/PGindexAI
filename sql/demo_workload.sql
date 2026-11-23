/* ============================================================================
   demo_workload.sql — Meaningful workload for anomaly detection
   ----------------------------------------------------------------------------
   Run: sudo -u postgres psql -d pgindexai_demo -f sql/demo_workload.sql
   
   Goal: create realistic conditions triggering the 3 detectors:
   1. Unused indexes (idx_scan = 0, idx_tup_read = 0)
   2. Fragmented indexes (measurable density via pg_class)
   3. SARGable queries without index (pg_stat_statements, >100 calls)

    Output split (driven by install.sh `demo_psql`):
      * screen → only the explanatory NOTICE lines (progress + digests);
      * log    → the FULL raw psql output (every command tag, all server
                 messages) in demo_install.log, next to install.sh —
                 strictly more detailed than the screen;
      * spinner → while psql is silent (e.g. the long [8/9] phase), an
                  ASCII activity spinner (|/-\ + elapsed time) ticks on
                  screen — TTY only, never written to the log.
   ============================================================================ */

-- ─────────────────────────────────────────────
-- pg_stat_statements tracking (required for detector #3)
-- ─────────────────────────────────────────────
-- Detector #3 (SARGable / missing-index) reads pg_stat_statements. By
-- default `pg_stat_statements.track = top` records only TOP-LEVEL
-- statements, so the workload queries below (issued inside PL/pgSQL DO
-- blocks via EXECUTE) would NOT be recorded as individual queries — the
-- detector would see nothing. `track = all` records them too. Requires
-- superuser (this demo runs as the `postgres` superuser). Without it,
-- detector #3 is blind to the workload.
SET pg_stat_statements.track = 'all';

-- ─────────────────────────────────────────────
-- Narration (screen output — see demo_psql in install.sh)
-- ─────────────────────────────────────────────
DO $$
BEGIN
    RAISE NOTICE 'PGindexAI demo — phase 2 of 2: 9 steps of realistic traffic, creating the evidence the 3 detectors will rank on.';
END $$;

-- ─────────────────────────────────────────────
-- Phase 0: User workload (generates pg_stat_statements)
DO $$
BEGIN
    RAISE NOTICE '[1/9] Everyday app traffic: ~3,450 queries on orders, customers, products, users, line items (filters, date ranges, paging)';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[2/9] "My orders" page: 5,000 per-customer lookups (last 90 days) — real scan counts for the indexed columns';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[3/9] Storefront: 16 product searches (category + price cap, price range + stock filters)';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[4/9] Dashboards: 30 days of daily aggregates (order count, revenue, status breakdown)';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[5/9] Order details: 100 "view order" listings (line items joined to products)';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[6/9] Admin screens: active admins + recent logins, then ~20 username prefix searches';
END $$;

-- Digest: how many admins / recent logins (no row dump on screen)
DO $$
DECLARE
    n_admin  INT;
    n_recent INT;
BEGIN
    SELECT count(*) INTO n_admin
    FROM users WHERE role = 'admin' AND is_active = true;

    SELECT count(*) INTO n_recent
    FROM users WHERE last_login > NOW() - interval '7 days';

    RAISE NOTICE 'Users: % active admins, % logins in the last 7 days',
        n_admin, n_recent;
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[7/9] Churn: ~350k deletes + 100k inserts + a mass UPDATE on orders/order_items, then VACUUM FULL — measurable fragmentation';
    RAISE NOTICE 'plus idx_orders_temp_status, an index the workload never touches — a clean unused-index target';
END $$;

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
DO $$
BEGIN
    RAISE NOTICE '[8/9] Hot paths: 17,000 repeated parameterized queries, several on columns WITHOUT an index — the missing-index targets';
END $$;
-- Use EXECUTE ... USING so that pg_stat_statements groups the queries

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
-- Phase 8: GIN/GIST workload (jsonb / array / range)
DO $$
BEGIN
    RAISE NOTICE '[9/9] Advanced types: 5,000 JSONB containment, array-overlap and date-range queries on events + bookings (deliberately unindexed) — the GIN/GIST targets';
END $$;
-- Triggers the GIN/GIST detector (detector #3, extended):
--   events.payload @> {...}  → GIN (jsonb containment)
--   events.tags   && ARRAY   → GIN (array overlap)
--   bookings.period @> range → GIST (range containment)

-- 8a: 2000 jsonb containment queries (GIN target — events.payload, NO INDEX)
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..2000 LOOP
        EXECUTE 'SELECT id, type, occurred_at FROM events WHERE payload @> $1'
        USING '{"type":"login","ok":true}'::jsonb;
    END LOOP;
END $$;

-- 8b: 1500 array overlap queries (GIN target — events.tags, NO INDEX)
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..1500 LOOP
        EXECUTE 'SELECT id, type FROM events WHERE tags && $1'
        USING ARRAY['web']::text[];
    END LOOP;
END $$;

-- 8c: 1500 range containment queries (GIST target — bookings.period, NO INDEX)
DO $$
DECLARE
    j INT;
BEGIN
    FOR j IN 1..1500 LOOP
        EXECUTE 'SELECT id, room FROM bookings WHERE period @> $1'
        USING int4range(j % 100, (j % 100) + 5);
    END LOOP;
END $$;

-- ─────────────────────────────────────────────
-- End of workload
-- ─────────────────────────────────────────────
DO $$ BEGIN RAISE NOTICE 'Phase 2 done — statistics refreshed (ANALYZE). What the daemon will now find:'; END $$;

-- Force stats update
ANALYZE;

-- ─────────────────────────────────────────────
-- Report: index usage (unused = DROP target) — digest
-- ─────────────────────────────────────────────
DO $$
DECLARE
    n_total   INT;
    n_zero    INT;   -- never scanned (idx_scan = 0)
    n_unused  INT;   -- never scanned AND never read (detector #1 criterion)
    zero_bytes BIGINT;
    top_names TEXT;
BEGIN
    SELECT count(*) INTO n_total FROM pg_stat_user_indexes;

    SELECT count(*), COALESCE(sum(pg_relation_size(indexrelid)), 0)
      INTO n_zero, zero_bytes
    FROM pg_stat_user_indexes
    WHERE idx_scan = 0;

    SELECT count(*) INTO n_unused
    FROM pg_stat_user_indexes
    WHERE idx_scan = 0 AND idx_tup_read = 0;

    SELECT left(string_agg(indexrelname, ', '
               ORDER BY pg_relation_size(indexrelid) DESC), 80)
      INTO top_names
    FROM pg_stat_user_indexes
    WHERE idx_scan = 0;

    RAISE NOTICE '   (1) unused     → % of % indexes never scanned (% drop candidates), % reclaimable — largest: %',
        n_zero, n_total, n_unused, pg_size_pretty(zero_bytes), top_names;
END $$;

-- ─────────────────────────────────────────────
-- Report: fragmentation (actual density via pg_class) — digest
-- ─────────────────────────────────────────────
DO $$
DECLARE
    n_frag   INT;
    top_name TEXT;
    top_rpp  NUMERIC;
BEGIN
    SELECT count(*) INTO n_frag
    FROM pg_class c
    JOIN pg_class i ON i.relkind = 'i'
    JOIN pg_index ix ON ix.indrelid = c.oid AND ix.indexrelid = i.oid
    JOIN pg_class c2 ON c2.oid = i.oid
    WHERE c.relname IN ('orders', 'order_items')
      AND c2.relpages > 0;

    SELECT i.relname,
           ROUND((c.reltuples::float / GREATEST(c2.relpages, 1))::numeric, 0)
      INTO top_name, top_rpp
    FROM pg_class c
    JOIN pg_class i ON i.relkind = 'i'
    JOIN pg_index ix ON ix.indrelid = c.oid AND ix.indexrelid = i.oid
    JOIN pg_class c2 ON c2.oid = i.oid
    WHERE c.relname IN ('orders', 'order_items')
      AND c2.relpages > 0
    ORDER BY c2.relpages DESC
    LIMIT 1;

    RAISE NOTICE '   (2) fragmented → % indexes on orders/order_items — largest: % (% rows/page)',
        n_frag, top_name, top_rpp;
END $$;

-- ─────────────────────────────────────────────
-- Report: SARGable queries (pg_stat_statements) — digest
-- ─────────────────────────────────────────────
DO $$
DECLARE
    n_tracked INT;
    top       TEXT;
BEGIN
    SELECT count(*) INTO n_tracked
    FROM pg_stat_statements
    WHERE dbid = (SELECT oid FROM pg_database WHERE datname = 'pgindexai_demo')
      AND query !~ '^SET|^BEGIN|^COMMIT|^ANALYZE|^DELETE FROM|^INSERT INTO|^VACUUM|^UPDATE '
      AND calls > 100;

    SELECT left(string_agg(s.q, ' | ' ORDER BY s.t DESC), 120)
      INTO top
    FROM (
        SELECT LEFT(regexp_replace(query, E'[\\n\\r]+', ' ', 'g'), 70) AS q,
               total_exec_time AS t
        FROM pg_stat_statements
        WHERE dbid = (SELECT oid FROM pg_database WHERE datname = 'pgindexai_demo')
          AND query !~ '^SET|^BEGIN|^COMMIT|^ANALYZE|^DELETE FROM|^INSERT INTO|^VACUUM|^UPDATE '
          AND calls > 100
        ORDER BY total_exec_time DESC
        LIMIT 3
    ) s;

    RAISE NOTICE '   (3) missing    → % hot queries (>100 calls) ranked for missing indexes — slowest: %',
        n_tracked, top;
END $$;

