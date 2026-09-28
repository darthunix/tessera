\set ON_ERROR_STOP on
\if :{?scale}
\else
\set scale 1
\endif

-- The multiplier of the base row counts; a family scales its constants by it.
DROP TABLE IF EXISTS bench_scale;
CREATE TABLE bench_scale AS SELECT :scale::int AS scale;

DROP TABLE IF EXISTS bench_narrow;
CREATE TABLE bench_narrow AS
SELECT g AS c1, g + 1 AS c2, g + 2 AS c3, g + 3 AS c4,
       g + 4 AS c5, g + 5 AS c6, g + 6 AS c7, g + 7 AS c8
FROM generate_series(1, 2000000 * :scale) AS g;

DROP TABLE IF EXISTS bench_wide;
DO $do$
DECLARE
    columns text;
    multiplier int := (SELECT scale FROM bench_scale);
BEGIN
    SELECT string_agg(format('g + %s AS c%s', i, i), ', ')
      INTO columns
      FROM generate_series(1, 60) AS i;
    EXECUTE 'CREATE TABLE bench_wide AS SELECT ' || columns ||
            ' FROM generate_series(1, 250000 * ' || multiplier || ') AS g';
END
$do$;

DROP TABLE IF EXISTS bench_mixed;
CREATE TABLE bench_mixed AS
SELECT g AS a,
       CASE WHEN g % 7 = 0 THEN NULL ELSE 'b-' || g END AS b,
       g::bigint AS c,
       g AS d,
       CASE WHEN g % 11 = 0 THEN NULL ELSE 'e-' || g END AS e,
       g::bigint AS f
FROM generate_series(1, 500000 * :scale) AS g;

-- The join family: a dimension with a unique key, a fact table whose
-- foreign keys spread over it (every 13th NULL), and a table with four
-- rows per key. The int8 keys hold the same values as the int4 ones, and
-- `big` and `fk_big` values past the int4 range; `fk_miss` matches nothing.
DROP TABLE IF EXISTS bench_dim;
CREATE TABLE bench_dim AS
SELECT g AS id, g::bigint AS id8, g::bigint << 33 AS big, g AS d1, g % 100 AS d2
FROM generate_series(1, 100000 * :scale) AS g;

DROP TABLE IF EXISTS bench_fact;
CREATE TABLE bench_fact AS
SELECT fk, fk::bigint AS fk8, fk::bigint << 33 AS fk_big,
       fk + 100000 * :scale AS fk_miss, g AS f1
FROM generate_series(1, 2000000 * :scale) AS g,
     LATERAL (SELECT CASE WHEN g % 13 = 0 THEN NULL
                          ELSE (g::bigint * 7919 % (100000 * :scale))::int + 1
                     END AS fk) AS key;

DROP TABLE IF EXISTS bench_dup;
CREATE TABLE bench_dup AS
SELECT g % (25000 * :scale) + 1 AS k, g AS v
FROM generate_series(1, 100000 * :scale) AS g;

-- The sort family: keys in no order of the rows. The prime 7919 divides no
-- row count but a multiplier's own multiple of it, so k4 is a permutation
-- of 0 .. rows - 1; k8 holds the same order past the int4 range, few has
-- 1000 values, nul is NULL in every 11th row, sorted follows the rows.
DROP TABLE IF EXISTS bench_sort;
CREATE TABLE bench_sort AS
SELECT k4, k4::bigint * 5000000011 AS k8, k4 % 1000 AS few,
       CASE WHEN g % 11 = 0 THEN NULL ELSE k4 END AS nul,
       g AS sorted, g AS v, 'row-' || g AS t
FROM generate_series(1, 2000000 * :scale) AS g,
     LATERAL (SELECT (g::bigint * 7919 % (2000000 * :scale))::int AS k4) AS key;

-- The merge join cases: both sides in the order of their key, with an
-- index on it, ten outer rows per inner key.
DROP TABLE IF EXISTS bench_mj_outer;
CREATE TABLE bench_mj_outer AS
SELECT g / 10 AS k, g AS v FROM generate_series(0, 2000000 * :scale - 1) AS g;
CREATE INDEX bench_mj_outer_k ON bench_mj_outer (k);
DROP TABLE IF EXISTS bench_mj_inner;
CREATE TABLE bench_mj_inner AS
SELECT g AS k, g % 1000 AS w FROM generate_series(0, 200000 * :scale - 1) AS g;
CREATE INDEX bench_mj_inner_k ON bench_mj_inner (k);

-- The setop family: a table of four range partitions over its key, v of
-- 1000 values.
DROP TABLE IF EXISTS bench_part;
CREATE TABLE bench_part (k int, v int) PARTITION BY RANGE (k);
DO $do$
DECLARE
    part int := 500000 * (SELECT scale FROM bench_scale);
BEGIN
    FOR i IN 0..3 LOOP
        EXECUTE format('CREATE TABLE bench_part_%s PARTITION OF bench_part FOR VALUES FROM (%s) TO (%s)',
                       i, i * part + 1, (i + 1) * part + 1);
    END LOOP;
END
$do$;
INSERT INTO bench_part SELECT g, (g::bigint * 7919 % 1000)::int FROM generate_series(1, 2000000 * :scale) AS g;

-- The grouping cases of the win family group by expressions: statistics
-- on them give the planner the number of groups, which it would otherwise
-- take from the unique column underneath.
CREATE STATISTICS bench_narrow_c1_mod10 ON (c1 % 10) FROM bench_narrow;
CREATE STATISTICS bench_narrow_c2_mod1000 ON (c2 % 1000) FROM bench_narrow;

VACUUM (ANALYZE) bench_narrow;
VACUUM (ANALYZE) bench_wide;
VACUUM (ANALYZE) bench_mixed;
VACUUM (ANALYZE) bench_dim;
VACUUM (ANALYZE) bench_fact;
VACUUM (ANALYZE) bench_dup;
VACUUM (ANALYZE) bench_sort;
VACUUM (ANALYZE) bench_mj_outer;
VACUUM (ANALYZE) bench_mj_inner;
VACUUM (ANALYZE) bench_part;

SELECT pg_size_pretty(pg_total_relation_size('bench_narrow')) AS narrow,
       pg_size_pretty(pg_total_relation_size('bench_wide')) AS wide,
       pg_size_pretty(pg_total_relation_size('bench_mixed')) AS mixed,
       pg_size_pretty(pg_total_relation_size('bench_fact')) AS fact,
       pg_size_pretty(pg_total_relation_size('bench_sort')) AS sort;

-- Warm the relations in shared buffers.
SET tessera.enable = off;
SELECT count(*) FROM bench_narrow;
SELECT count(*) FROM bench_wide;
SELECT count(*) FROM bench_mixed;
SELECT count(*) FROM bench_dim;
SELECT count(*) FROM bench_fact;
SELECT count(*) FROM bench_dup;
SELECT count(*) FROM bench_sort;
SELECT count(*) FROM bench_mj_outer;
SELECT count(*) FROM bench_mj_inner;
SELECT count(*) FROM bench_part;
