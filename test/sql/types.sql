CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

-- The same result with Tessera on and off, as text.
CREATE FUNCTION types_same(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text[];
    without_tessera text[];
    wrapped text := format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO without_tessera;
    PERFORM set_config('tessera.enable', 'on', true);
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('on: %s off: %s', with_tessera, without_tessera);
    END IF;
    RETURN 'same';
END
$$;

-- Keys the table keeps in a word besides int4 and int8: int2, date, bool
-- as int4, timestamp and timestamptz as int8; negative values, NULL.
CREATE TABLE types_f AS
SELECT i AS id,
       CASE WHEN i % 11 = 0 THEN NULL ELSE (i % 50 - 25)::int2 END AS s,
       CASE WHEN i % 13 = 0 THEN NULL ELSE date '2000-01-01' + (i % 40 - 20) END AS d,
       CASE WHEN i % 17 = 0 THEN NULL ELSE timestamp '2000-01-01' + (i % 30) * interval '1 hour' - interval '10 hours' END AS ts,
       CASE WHEN i % 19 = 0 THEN NULL ELSE timestamptz '2000-01-01 00:00+00' + (i % 30) * interval '1 minute' END AS tz,
       CASE WHEN i % 23 = 0 THEN NULL ELSE i % 3 = 0 END AS b,
       i % 1000 AS v
FROM generate_series(1, 3000) AS i;
CREATE TABLE types_d AS
SELECT i AS id, (i - 25)::int2 AS s, date '2000-01-01' + (i - 20) AS d,
       timestamp '2000-01-01' + i * interval '1 hour' - interval '10 hours' AS ts,
       timestamptz '2000-01-01 00:00+00' + i * interval '1 minute' AS tz,
       i % 2 = 0 AS b, i AS w
FROM generate_series(0, 49) AS i;
ANALYZE types_f, types_d;
SET max_parallel_workers_per_gather = 0;
SET enable_mergejoin = off;
SET enable_nestloop = off;

-- Hash joins on each type, and int2 against int4 and int8.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f JOIN types_d ON types_f.d = types_d.d;
SELECT types_same($$SELECT count(*), sum(types_d.w) FROM types_f JOIN types_d ON types_f.s = types_d.s$$);
SELECT types_same($$SELECT count(*), sum(types_d.w) FROM types_f JOIN types_d ON types_f.d = types_d.d$$);
SELECT types_same($$SELECT count(*), sum(types_d.w) FROM types_f JOIN types_d ON types_f.ts = types_d.ts$$);
SELECT types_same($$SELECT count(*), sum(types_d.w) FROM types_f JOIN types_d ON types_f.tz = types_d.tz$$);
SELECT types_same($$SELECT count(*), sum(types_d.w) FROM types_f JOIN types_d ON types_f.b = types_d.b AND types_f.s = types_d.s$$);
SELECT types_same($$SELECT count(*) FROM types_f JOIN types_d ON types_f.s = types_d.w$$);
SELECT types_same($$SELECT count(*) FROM types_f JOIN types_d ON types_f.s = types_d.w::bigint$$);
SELECT types_same($$SELECT types_f.d, types_d.w FROM types_f LEFT JOIN types_d ON types_f.d = types_d.d WHERE types_f.v < 30$$);
-- A date against a timestamp compares other than bit for bit: the core's.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f JOIN types_d ON types_f.d = types_d.ts;
SELECT types_same($$SELECT count(*) FROM types_f JOIN types_d ON types_f.d = types_d.ts$$);

-- Grouping, DISTINCT and UNION by each type; the keys come back as the
-- core's values, NULL a group of its own.
EXPLAIN (COSTS OFF) SELECT d, count(*) FROM types_f GROUP BY d;
SELECT types_same($$SELECT s, count(*), sum(v) FROM types_f GROUP BY s$$);
SELECT types_same($$SELECT d, count(*), sum(v) FROM types_f GROUP BY d$$);
SELECT types_same($$SELECT ts, count(*) FROM types_f GROUP BY ts$$);
SELECT types_same($$SELECT tz, count(*) FROM types_f GROUP BY tz$$);
SELECT types_same($$SELECT b, s, count(*) FROM types_f GROUP BY b, s$$);
SELECT types_same($$SELECT d + 1, count(*) FROM types_f GROUP BY d + 1$$);
SELECT types_same($$SELECT DISTINCT d FROM types_f$$);
SELECT types_same($$SELECT DISTINCT b, ts FROM types_f$$);
EXPLAIN (COSTS OFF) SELECT d FROM types_f UNION SELECT d FROM types_d;
SELECT types_same($$SELECT d FROM types_f UNION SELECT d FROM types_d$$);
SELECT types_same($$SELECT s, b FROM types_f UNION SELECT s, b FROM types_d$$);

-- Sorts by each type, both directions and places of NULL, and a top-N.
EXPLAIN (COSTS OFF) SELECT id, d FROM types_f ORDER BY d, id;
CREATE FUNCTION types_order(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text;
    without_tessera text;
    wrapped text := format('SELECT string_agg(q::text, '','') FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO without_tessera;
    PERFORM set_config('tessera.enable', 'on', true);
    RETURN CASE WHEN with_tessera IS NOT DISTINCT FROM without_tessera THEN 'same' ELSE 'differ' END;
END
$$;
SELECT types_order($$SELECT id, s FROM types_f ORDER BY s, id$$);
SELECT types_order($$SELECT id, d FROM types_f ORDER BY d DESC NULLS LAST, id$$);
SELECT types_order($$SELECT id, ts FROM types_f ORDER BY ts NULLS FIRST, id DESC$$);
SELECT types_order($$SELECT id, tz FROM types_f ORDER BY tz DESC, id$$);
SELECT types_order($$SELECT id, b FROM types_f ORDER BY b, s, id$$);
SELECT types_order($$SELECT id, d FROM types_f ORDER BY d DESC, id LIMIT 7$$);

-- Keys a word does not hold: text, numeric, an expression over them; their
-- values get numbers through a dictionary by the type's hash and equality,
-- and go out as the values. numeric 1.0 and 1.00 are one group.
CREATE TABLE types_g AS
SELECT i AS id,
       CASE WHEN i % 17 = 0 THEN NULL ELSE 'k' || (i % 40) END AS t,
       CASE WHEN i % 13 = 0 THEN NULL ELSE ((i % 30) * 1.5)::numeric END AS n,
       CASE WHEN i % 2 = 0 THEN 1.0 ELSE 1.00 END::numeric AS one,
       i % 7 AS w, repeat('x', i % 50) || i % 60 AS long
FROM generate_series(1, 5000) AS i;
ANALYZE types_g;
EXPLAIN (COSTS OFF) SELECT t, count(*) FROM types_g GROUP BY t;
SELECT types_same($$SELECT t, count(*), sum(w), max(n) FROM types_g GROUP BY t$$);
SELECT types_same($$SELECT n, count(*), max(t) FROM types_g GROUP BY n$$);
SELECT types_same($$SELECT t, w, n, count(*) FROM types_g GROUP BY t, w, n$$);
SELECT types_same($$SELECT upper(t), count(*) FROM types_g GROUP BY upper(t)$$);
SELECT types_same($$SELECT count(*), sum(c) FROM (SELECT one, count(*) AS c FROM types_g GROUP BY one) AS q$$);
SELECT types_same($$SELECT long, count(*) FROM types_g GROUP BY long HAVING count(*) > 50$$);
SELECT types_same($$SELECT DISTINCT t FROM types_g$$);
SELECT types_same($$SELECT DISTINCT n, t FROM types_g$$);
EXPLAIN (COSTS OFF) SELECT t FROM types_g UNION SELECT label FROM (VALUES ('k1'), ('new')) AS v(label);
SELECT types_same($$SELECT t FROM types_g UNION SELECT label FROM (VALUES ('k1'), ('new')) AS v(label)$$);
SELECT types_same($$SELECT n FROM types_g UNION SELECT n * 2 FROM types_g$$);
-- Past hash_mem the rows spill with their values, by the values' hashes,
-- and each partition numbers its values anew.
SET work_mem = '64kB';
SET enable_sort = off;
SELECT types_same($$SELECT count(*), sum(c), max(m) FROM (SELECT long, t, count(*) AS c, max(id) AS m FROM types_g GROUP BY long, t) AS q$$);
SELECT types_same($$SELECT t, n, count(*) FROM types_g GROUP BY t, n$$);
RESET enable_sort;
RESET work_mem;
-- Rescan numbers the values anew.
SELECT types_same($$SELECT x, (SELECT count(*) FROM (SELECT t FROM types_g WHERE w = x GROUP BY t) AS q) FROM generate_series(0, 3) AS x$$);

-- Hash joins by keys a word does not hold: the table keeps the 64-bit hash
-- of the value, and the equality stays a join clause that decides the
-- pair; with a word key, every kind of join, NULL keys that never match,
-- numeric 1.0 equal to 1.00, a case-insensitive collation.
CREATE COLLATION types_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
CREATE TABLE types_h AS
SELECT i AS id,
       (CASE WHEN i % 2 = 0 THEN upper('k' || (i % 45)) ELSE 'k' || (i % 45) END) COLLATE types_ci AS c,
       CASE WHEN i % 9 = 0 THEN NULL ELSE 'k' || (i % 45) END AS t,
       CASE WHEN i % 11 = 0 THEN NULL ELSE ((i % 35) * 1.5)::numeric END AS n,
       1.00::numeric AS one, i % 7 AS w
FROM generate_series(1, 400) AS i;
ANALYZE types_h;
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_g JOIN types_h ON types_g.t = types_h.t;
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t$$);
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.n = types_h.n$$);
SELECT types_same($$SELECT count(*) FROM types_g JOIN types_h ON types_g.one = types_h.one AND types_g.w = types_h.w$$);
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w AND types_g.n = types_h.n$$);
SELECT types_same($$SELECT types_g.id, types_h.id FROM types_g LEFT JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w WHERE types_g.id < 300$$);
SELECT types_same($$SELECT types_g.id, types_h.id FROM types_g RIGHT JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w WHERE types_h.id < 100$$);
SELECT types_same($$SELECT count(*), count(types_g.id), count(types_h.id) FROM types_g FULL JOIN types_h ON types_g.n = types_h.n AND types_g.w = types_h.w$$);
SELECT types_same($$SELECT count(*) FROM types_g WHERE EXISTS (SELECT FROM types_h WHERE types_h.t = types_g.t AND types_h.w = types_g.w)$$);
SELECT types_same($$SELECT count(*) FROM types_g WHERE NOT EXISTS (SELECT FROM types_h WHERE types_h.t = types_g.t AND types_h.w = types_g.w)$$);
SELECT types_same($$SELECT count(*) FROM types_g JOIN types_h ON upper(types_g.t) = upper(types_h.t)$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_h AS a JOIN types_h AS b ON a.c = b.c;
SELECT types_same($$SELECT count(*) FROM types_h AS a JOIN types_h AS b ON a.c = b.c$$);
-- No Bloom filter below: the scan has the values, not their hashes.
SET tessera.join_bloom_ratio = 1;
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t WHERE types_g.long LIKE '%1%'$$);
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.w = types_h.w AND types_g.t = types_h.t WHERE types_g.long LIKE '%1%'$$);
RESET tessera.join_bloom_ratio;
-- Workers build one shared table of hashes.
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_g JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w;
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w$$);
SELECT types_same($$SELECT count(*), count(types_h.id) FROM types_g LEFT JOIN types_h ON types_g.n = types_h.n$$);
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET max_parallel_workers_per_gather = 0;
-- Past hash_mem both sides spill with the hashes as keys.
SET work_mem = '64kB';
SELECT types_same($$SELECT count(*), sum(b.id) FROM types_g AS a JOIN types_g AS b ON a.long = b.long AND a.t = b.t$$);
SELECT types_same($$SELECT count(*), count(b.id) FROM types_g AS a LEFT JOIN types_g AS b ON a.long = b.long AND a.w = b.w$$);
RESET work_mem;
DROP TABLE types_g, types_h;
DROP COLLATION types_ci;

DROP FUNCTION types_order(text);
DROP FUNCTION types_same(text);
DROP TABLE types_f, types_d;
DROP EXTENSION tessera;
