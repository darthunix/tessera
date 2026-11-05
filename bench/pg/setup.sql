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

-- The wordkey family: keys of types the table keeps in a word besides int4
-- and int8: a date of five years, a timestamp of one hour steps, an int2 of
-- 1000 values, a boolean; and a dimension of the days.
DROP TABLE IF EXISTS bench_dates;
CREATE TABLE bench_dates AS
SELECT date '2020-01-01' + (g::bigint * 7919 % 1826)::int AS d,
       timestamp '2020-01-01' + (g::bigint * 7907 % 43824) * interval '1 hour' AS ts,
       (g % 1000 - 500)::int2 AS s, g % 3 = 0 AS b, g AS v
FROM generate_series(1, 2000000 * :scale) AS g;
-- The timestamptz cases of the win family: tz, hourly instants over five
-- years in a scrambled order, about 11 spans of a zone with summer time
-- and 1826 local days; tz_spread, row g in span (g * 37) % 80 of the 80
-- spans of such a zone from 1980 to 2019 (mid-January or mid-July of a
-- year), so that any 64 rows in a row lie in 64 different spans.
DROP TABLE IF EXISTS bench_tstz;
CREATE TABLE bench_tstz AS
SELECT timestamptz '2020-01-01 00:00+00' + (g::bigint * 7907 % 43824) * interval '1 hour' AS tz,
       make_timestamptz(1980 + (g::bigint * 37 % 80)::int / 2,
                        CASE WHEN g::bigint * 37 % 80 % 2 = 0 THEN 1 ELSE 7 END,
                        15, 12, 0, 0, 'UTC') + (g % 1440) * interval '1 minute' AS tz_spread
FROM generate_series(1, 2000000 * :scale) AS g;
DROP TABLE IF EXISTS bench_days;
CREATE TABLE bench_days AS
SELECT date '2020-01-01' + g AS d, g AS w FROM generate_series(0, 1825) AS g;

-- The joins of the anykey family by text and numeric keys: a dimension of
-- 100 000 keys and a fact table whose keys spread over it, every 13th NULL.
DROP TABLE IF EXISTS bench_tdim;
CREATE TABLE bench_tdim AS
SELECT 'key-' || g AS k, (g * 1.5)::numeric AS n, g % 100 AS w
FROM generate_series(1, 100000 * :scale) AS g;
DROP TABLE IF EXISTS bench_tfact;
CREATE TABLE bench_tfact AS
SELECT 'key-' || fk AS k, (fk * 1.5)::numeric AS n, fk % 100 AS w, g AS v
FROM generate_series(1, 2000000 * :scale) AS g,
     LATERAL (SELECT CASE WHEN g % 13 = 0 THEN NULL
                          ELSE (g::bigint * 7919 % (100000 * :scale))::int + 1
                     END AS fk) AS key;

-- The index family: reads through indexes. k is scattered over the pages
-- (a bitmap of it names rows of many pages), w has 97 values, id and d are
-- in the order of the rows (a btree of id, a BRIN of d), t text.
DROP TABLE IF EXISTS bench_idx;
CREATE TABLE bench_idx AS
SELECT g AS id, (g::bigint * 7919 % (2000000 * :scale))::int AS k, g % 97 AS w,
       date '2000-01-01' + g / 2000 AS d, 'row-' || (g % 10000) AS t
FROM generate_series(1, 2000000 * :scale) AS g;
CREATE INDEX bench_idx_k ON bench_idx (k);
CREATE INDEX bench_idx_w ON bench_idx (w);
CREATE INDEX bench_idx_d ON bench_idx USING brin (d);
CREATE INDEX bench_idx_id ON bench_idx (id);

-- The indexjoin family: a table looked up through its indexes from a
-- smaller one. id is unique and in the table's order, o4 holds four rows
-- a key in order, s is unique and scattered over the pages, tk and dk are
-- a text and a date key, h holds 0 for one row in 100 and is unique
-- otherwise. The outer side's keys: in id's order (k), shuffled (ks),
-- past the end for every other row (km), 1000 values repeated (kr), as
-- bigint (k8), text (kt) and date (kd), 0 for one row in 64 (kh); sel
-- spreads a filter over every page.
DROP TABLE IF EXISTS bench_lookup;
CREATE TABLE bench_lookup AS
SELECT g AS id, g / 4 AS o4, (g::bigint * 7919 % (2000000 * :scale))::int AS s,
       'key-' || g AS tk, date '1900-01-01' + g AS dk,
       CASE WHEN g % 100 = 0 THEN 0 ELSE g END AS h, g % 1000 AS v
FROM generate_series(1, 2000000 * :scale) AS g;
ALTER TABLE bench_lookup ADD PRIMARY KEY (id);
CREATE INDEX bench_lookup_o4 ON bench_lookup (o4);
CREATE INDEX bench_lookup_s ON bench_lookup (s);
CREATE INDEX bench_lookup_tk ON bench_lookup (tk);
CREATE INDEX bench_lookup_dk ON bench_lookup (dk);
CREATE INDEX bench_lookup_h ON bench_lookup (h);
DROP TABLE IF EXISTS bench_lookup_outer;
CREATE TABLE bench_lookup_outer AS
SELECT g AS id, g * 10 AS k, ((g::bigint * 7919) % (200000 * :scale) + 1)::int * 10 AS ks,
       CASE WHEN g % 2 = 0 THEN g * 10 ELSE 2000000 * :scale + g END AS km,
       (g % 1000 + 1) * 100 AS kr, (g * 10)::bigint AS k8, 'key-' || (g * 10) AS kt,
       date '1900-01-01' + g * 10 AS kd, CASE WHEN g % 64 = 0 THEN 0 ELSE g * 10 END AS kh,
       g % 100 AS sel
FROM generate_series(1, 200000 * :scale) AS g;

-- The semi and anti joins of the indexjoin and lookupcost families: four
-- rows a key k in the index's order, pos the row's place among them, and
-- r a value each row takes apart of the others of its key, with no
-- index, so that a clause on it runs over the rows a key's lookup finds.
DROP TABLE IF EXISTS bench_four;
CREATE TABLE bench_four AS
SELECT g / 4 AS k, g % 4 AS pos, abs(hashint4(g)) % 1000 AS r
FROM generate_series(0, 800000 * :scale - 1) AS g;
CREATE INDEX bench_four_k ON bench_four (k);

-- The scancost family's parallel query with next to nothing to read: the
-- workers' start and finish alone.
DROP TABLE IF EXISTS bench_tiny;
CREATE TABLE bench_tiny AS SELECT g AS a FROM generate_series(1, 1000) AS g;
ALTER TABLE bench_tiny SET (parallel_workers = 2);

-- The grouping cases of the win family group by expressions: statistics
-- on them give the planner the number of groups, which it would otherwise
-- take from the unique column underneath.
CREATE STATISTICS bench_narrow_c1_mod10 ON (c1 % 10) FROM bench_narrow;
CREATE STATISTICS bench_narrow_c2_mod1000 ON (c2 % 1000) FROM bench_narrow;
-- The groupings of the anyagg family.
CREATE STATISTICS bench_mixed_d_mod100 ON (d % 100) FROM bench_mixed;
CREATE STATISTICS bench_mixed_a_mod50000 ON (a % 50000) FROM bench_mixed;

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
VACUUM (ANALYZE) bench_dates;
VACUUM (ANALYZE) bench_tstz;
VACUUM (ANALYZE) bench_days;
VACUUM (ANALYZE) bench_tdim;
VACUUM (ANALYZE) bench_tfact;
VACUUM (ANALYZE) bench_idx;
VACUUM (ANALYZE) bench_tiny;
VACUUM (ANALYZE) bench_lookup;
VACUUM (ANALYZE) bench_lookup_outer;
VACUUM (ANALYZE) bench_four;

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
SELECT count(*) FROM bench_dates;
SELECT count(*) FROM bench_tstz;
SELECT count(*) FROM bench_days;
SELECT count(*) FROM bench_idx;
