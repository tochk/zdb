-- Demo database for screenshots (README docs/screenshots/):
--   sudo -u postgres createdb -O zdb shop
--   psql -h 127.0.0.1 -U zdb shop -f scripts/demo-shop.sql
SELECT setseed(0.42);
CREATE TABLE customers (
  id          serial PRIMARY KEY,
  name        text NOT NULL,
  email       text NOT NULL UNIQUE,
  country     text NOT NULL,
  plan        text NOT NULL DEFAULT 'free',
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE products (
  id        serial PRIMARY KEY,
  sku       text NOT NULL UNIQUE,
  name      text NOT NULL,
  category  text NOT NULL,
  price     numeric(10,2) NOT NULL,
  stock     integer NOT NULL
);
CREATE TABLE orders (
  id           serial PRIMARY KEY,
  customer_id  integer NOT NULL REFERENCES customers(id),
  status       text NOT NULL,
  total        numeric(12,2) NOT NULL DEFAULT 0,
  ordered_at   timestamptz NOT NULL
);
CREATE TABLE order_items (
  order_id    integer NOT NULL REFERENCES orders(id),
  product_id  integer NOT NULL REFERENCES products(id),
  qty         integer NOT NULL,
  unit_price  numeric(10,2) NOT NULL,
  PRIMARY KEY (order_id, product_id)
);
CREATE INDEX orders_customer_idx ON orders (customer_id);
CREATE INDEX orders_ordered_at_idx ON orders (ordered_at);

WITH first(n) AS (SELECT unnest(ARRAY['Ada','Alan','Grace','Linus','Margaret','Ken','Barbara','Dennis','Edsger','Frances','Guido','Hedy','Ivan','Joan','Katherine','Leslie','Mary','Niklaus','Ole','Radia','Sophie','Tim','Yukihiro','Anders','Bjarne','Donald','Elena','Fatima','Hiro','Ingrid','Jonas','Lena','Marco','Nadia','Omar','Priya','Rosa','Sven','Tara','Wei'])),
     last(n)  AS (SELECT unnest(ARRAY['Lovelace','Turing','Hopper','Torvalds','Hamilton','Thompson','Liskov','Ritchie','Dijkstra','Allen','van Rossum','Lamarr','Sutherland','Clarke','Johnson','Lamport','Shaw','Wirth','Dahl','Perlman','Wilson','Berners-Lee','Matsumoto','Hejlsberg','Stroustrup','Knuth','Petrova','Haddad','Tanaka','Larsen','Berg','Novak','Rossi','Karimi','Farouk','Sharma','Garcia','Lund','Okafor','Chen'])),
     names AS (SELECT f.n AS fn, l.n AS ln, row_number() OVER (ORDER BY random()) AS rn FROM first f CROSS JOIN last l)
INSERT INTO customers (name, email, country, plan, created_at)
SELECT fn || ' ' || ln,
       lower(replace(fn || '.' || ln, ' ', '')) || rn || '@example.com',
       (ARRAY['Germany','France','Netherlands','Spain','Italy','Poland','Sweden','Norway','Japan','Canada','Brazil','India','Kenya','Australia','United States','United Kingdom'])[1 + floor(random()*16)::int],
       (ARRAY['free','free','free','pro','pro','team','enterprise'])[1 + floor(random()*7)::int],
       timestamptz '2024-01-01' + random() * interval '650 days'
FROM names;

WITH items(cat, n) AS (VALUES
  ('Keyboards', ARRAY['Mechanical Keyboard TKL','Split Ergonomic Keyboard','Low-Profile Keyboard','60% Hot-Swap Keyboard','Wireless Keyboard']),
  ('Mice',      ARRAY['Vertical Mouse','Trackball','Wireless Mouse','Gaming Mouse','Travel Mouse']),
  ('Displays',  ARRAY['27" 4K Monitor','34" Ultrawide','Portable 15" Display','24" 1440p Monitor','Monitor Arm']),
  ('Audio',     ARRAY['Studio Headphones','USB Microphone','Noise-Cancelling Earbuds','Desk Speakers','Audio Interface']),
  ('Storage',   ARRAY['NVMe SSD 2TB','Portable SSD 1TB','NAS Drive 8TB','USB-C Flash Drive','SD Card 256GB']),
  ('Cables',    ARRAY['USB-C Cable 2m','Thunderbolt 4 Cable','HDMI 2.1 Cable','Ethernet Cat6 5m','Magnetic Charging Cable']),
  ('Desk',      ARRAY['Standing Desk','Desk Mat XL','Laptop Stand','Cable Tray','Desk Lamp'])
)
INSERT INTO products (sku, name, category, price, stock)
SELECT upper(left(cat, 3)) || '-' || lpad((10 * o + v)::text, 4, '0'),
       name || CASE v WHEN 1 THEN '' WHEN 2 THEN ' Pro' ELSE ' Max' END,
       cat,
       round((15 + random() * 480)::numeric, 2),
       floor(random() * 400)::int
FROM items, unnest(n) WITH ORDINALITY AS u(name, o), generate_series(1, 3) v;

INSERT INTO orders (customer_id, status, ordered_at)
SELECT 1 + floor(random() * (SELECT count(*) FROM customers))::int,
       (ARRAY['paid','paid','paid','shipped','shipped','delivered','delivered','delivered','refunded','pending'])[1 + floor(random()*10)::int],
       timestamptz '2025-01-01' + random() * interval '300 days'
FROM generate_series(1, 25000);

INSERT INTO order_items (order_id, product_id, qty, unit_price)
SELECT DISTINCT ON (o.id, p.id) o.id, p.id, 1 + floor(random()*3)::int, p.price
FROM orders o
CROSS JOIN LATERAL (SELECT id, price FROM products WHERE o.id > 0 ORDER BY random() LIMIT 1 + floor(random()*3)::int) p;

UPDATE orders o SET total = s.t FROM (SELECT order_id, sum(qty * unit_price) t FROM order_items GROUP BY 1) s WHERE s.order_id = o.id;

CREATE VIEW customer_stats AS
SELECT c.id, c.name, c.country, c.plan, count(o.id) AS orders, coalesce(sum(o.total), 0) AS lifetime_value, max(o.ordered_at) AS last_order
FROM customers c LEFT JOIN orders o ON o.customer_id = c.id AND o.status <> 'refunded'
GROUP BY c.id;

CREATE SCHEMA analytics;
CREATE MATERIALIZED VIEW analytics.daily_revenue AS
SELECT date_trunc('day', ordered_at)::date AS day, count(*) AS orders, sum(total) AS revenue
FROM orders WHERE status <> 'refunded' GROUP BY 1 ORDER BY 1;
CREATE TABLE analytics.events (
  id bigserial PRIMARY KEY, customer_id integer REFERENCES customers(id), kind text NOT NULL, payload jsonb, at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO analytics.events (customer_id, kind, payload, at)
SELECT 1 + floor(random()*1600)::int,
       (ARRAY['login','view_product','add_to_cart','checkout','logout'])[1 + floor(random()*5)::int],
       jsonb_build_object('ua', (ARRAY['firefox','chrome','safari'])[1 + floor(random()*3)::int], 'ms', floor(random()*900)::int),
       timestamptz '2025-06-01' + random() * interval '120 days'
FROM generate_series(1, 50000);
CREATE FUNCTION analytics.revenue_between(a date, b date) RETURNS numeric LANGUAGE sql AS $$ SELECT coalesce(sum(revenue), 0) FROM analytics.daily_revenue WHERE day BETWEEN a AND b $$;
ANALYZE customers, products, orders, order_items, analytics.events;
