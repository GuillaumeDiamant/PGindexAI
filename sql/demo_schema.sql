/* ============================================================================
   demo_schema.sql — Demonstration schema for PGindexAI
   ----------------------------------------------------------------------------
   Run: psql -U postgres -d postgres -f sql/demo_schema.sql
   ============================================================================ */

-- ── Database ─────────────────────────────────────────────────────────────
SELECT 'CREATE DATABASE pgindexai_demo'
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'pgindexai_demo')
\gexec

\c pgindexai_demo

-- ── Extension: pg_stat_statements (required by the SARGable detector) ────
-- Prerequisite: pg_stat_statements in shared_preload_libraries + a PostgreSQL
-- restart (install.sh does this automatically). No-op if already present.
-- Without it, the daemon runs in DEGRADED mode (missing-index detection off).
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

-- ── Table: customers ─────────────────────────────────────────────────────
DROP TABLE IF EXISTS orders, products, order_items, users CASCADE;

CREATE TABLE customers (
    id          SERIAL       PRIMARY KEY,
    email       VARCHAR(255) NOT NULL UNIQUE,
    first_name  VARCHAR(100),
    last_name   VARCHAR(100),
    country     VARCHAR(60)  NOT NULL DEFAULT 'FR',
    city        VARCHAR(100),
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_customers_country ON customers(country);
CREATE INDEX idx_customers_city    ON customers(city);
CREATE INDEX idx_customers_email   ON customers(email);

-- ── Table: products ──────────────────────────────────────────────────────
CREATE TABLE products (
    id          SERIAL       PRIMARY KEY,
    sku         VARCHAR(50)  NOT NULL UNIQUE,
    name        VARCHAR(255) NOT NULL,
    category    VARCHAR(100) NOT NULL,
    price       NUMERIC(12,2) NOT NULL,
    cost        NUMERIC(12,2) NOT NULL,
    stock_qty   INT          NOT NULL DEFAULT 0,
    is_active   BOOLEAN      NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_products_category ON products(category);
CREATE INDEX idx_products_sku       ON products(sku);
CREATE INDEX idx_products_price     ON products(price);
CREATE INDEX idx_products_stock     ON products(stock_qty);

-- ── Table: orders ────────────────────────────────────────────────────────
CREATE TABLE orders (
    id              SERIAL       PRIMARY KEY,
    customer_id     INT          NOT NULL REFERENCES customers(id),
    order_date      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    status          VARCHAR(30)  NOT NULL DEFAULT 'pending',
    total_amount    NUMERIC(14,2) NOT NULL DEFAULT 0,
    shipping_method VARCHAR(50),
    notes           TEXT,
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- Index that will become unused
CREATE INDEX idx_orders_status ON orders(status);

-- Fragmented index (will be rebuilt by the daemon)
CREATE INDEX idx_orders_date   ON orders(order_date);
CREATE INDEX idx_orders_customer ON orders(customer_id);

-- ── Table: order_items ───────────────────────────────────────────────────
CREATE TABLE order_items (
    id          SERIAL       PRIMARY KEY,
    order_id    INT          NOT NULL REFERENCES orders(id),
    product_id  INT          NOT NULL REFERENCES products(id),
    quantity    INT          NOT NULL DEFAULT 1,
    unit_price  NUMERIC(12,2) NOT NULL,
    discount    NUMERIC(5,2) NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_items_order   ON order_items(order_id);
CREATE INDEX idx_items_product ON order_items(product_id);
CREATE INDEX idx_items_date    ON order_items(created_at);

-- ── Table: users (to simulate sessions) ──────────────────────────────────
CREATE TABLE users (
    id          SERIAL       PRIMARY KEY,
    username    VARCHAR(80)  NOT NULL UNIQUE,
    role        VARCHAR(20)  NOT NULL DEFAULT 'customer',
    last_login  TIMESTAMPTZ,
    is_active   BOOLEAN      NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_users_role ON users(role);
CREATE INDEX idx_users_login ON users(last_login);

-- ── Data: customers ──────────────────────────────────────────────────────
INSERT INTO customers (email, first_name, last_name, country, city)
SELECT
    'user' || g || '@example.com',
    ('First' || g)::text,
    ('Last' || g)::text,
    (ARRAY['FR','US','DE','GB','JP','BR','IN','AU','CA','MX'])[1 + (g % 10)],
    (ARRAY['Paris','New York','Berlin','London','Tokyo','Sao Paulo','Mumbai','Sydney','Toronto','Mexico City'])[1 + (g % 10)]
FROM generate_series(1, 10000) g;

-- ── Data: products ───────────────────────────────────────────────────────
INSERT INTO products (sku, name, category, price, cost, stock_qty, is_active)
SELECT
    'SKU-' || LPAD(g::text, 6, '0'),
    'Product ' || g,
    (ARRAY['Electronics','Clothing','Food','Books','Sports','Home','Toys','Beauty'])[1 + (g % 8)],
    ROUND((random() * 990 + 10)::numeric, 2),
    ROUND((random() * 400 + 5)::numeric, 2),
    (random() * 500)::int,
    random() > 0.1
FROM generate_series(1, 5000) g;

-- ── Data: orders ─────────────────────────────────────────────────────────
INSERT INTO orders (customer_id, order_date, status, total_amount, shipping_method)
SELECT
    1 + (g % 10000),
    NOW() - (random() * 365 || ' days')::interval,
    (ARRAY['pending','confirmed','shipped','delivered','cancelled','returned'])[1 + (g % 6)],
    ROUND((random() * 9900 + 10)::numeric, 2),
    (ARRAY['standard','express','overnight','pickup'])[1 + (g % 4)]
FROM generate_series(1, 200000) g;

-- ── Data: order_items ────────────────────────────────────────────────────
INSERT INTO order_items (order_id, product_id, quantity, unit_price, discount)
SELECT
    1 + (g % 200000),
    1 + (g % 5000),
    1 + (g % 10),
    (random() * 500 + 5)::numeric,
    (random() * 0.3)::numeric
FROM generate_series(1, 500000) g;

-- ── Data: users ──────────────────────────────────────────────────────────
INSERT INTO users (username, role, last_login, is_active)
SELECT
    'user' || g,
    (ARRAY['customer','admin','moderator','support'])[1 + (g % 4)],
    NOW() - (random() * 90 || ' days')::interval,
    random() > 0.15
FROM generate_series(1, 2000) g;

-- ── Statistics ───────────────────────────────────────────────────────────
ANALYZE customers;
ANALYZE products;
ANALYZE orders;
ANALYZE order_items;
ANALYZE users;

-- ── Summary ──────────────────────────────────────────────────────────────
DO $$
DECLARE
    r RECORD;
BEGIN
    RAISE NOTICE '=== pgindexai_demo demo schema ===';
    FOR r IN SELECT relname, n_live_tup
              FROM pg_stat_user_tables
              ORDER BY relname
    LOOP
        RAISE NOTICE '  %: % rows', r.relname, r.n_live_tup;
    END LOOP;
    RAISE NOTICE '=== Index ===';
    FOR r IN SELECT indexrelname, pg_size_pretty(pg_relation_size(indexrelid)) AS size
              FROM pg_stat_user_indexes
              ORDER BY indexrelname
    LOOP
        RAISE NOTICE '  %: %', r.indexrelname, r.size;
    END LOOP;
END $$;
