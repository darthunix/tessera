\set ON_ERROR_STOP on

DROP TABLE IF EXISTS bench_narrow;
CREATE TABLE bench_narrow AS
SELECT g AS c1, g + 1 AS c2, g + 2 AS c3, g + 3 AS c4,
       g + 4 AS c5, g + 5 AS c6, g + 6 AS c7, g + 7 AS c8
FROM generate_series(1, 2000000) AS g;

DROP TABLE IF EXISTS bench_wide;
DO $do$
DECLARE
    columns text;
BEGIN
    SELECT string_agg(format('g + %s AS c%s', i, i), ', ')
      INTO columns
      FROM generate_series(1, 60) AS i;
    EXECUTE 'CREATE TABLE bench_wide AS SELECT ' || columns ||
            ' FROM generate_series(1, 250000) AS g';
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
FROM generate_series(1, 500000) AS g;

VACUUM (ANALYZE) bench_narrow;
VACUUM (ANALYZE) bench_wide;
VACUUM (ANALYZE) bench_mixed;

SELECT pg_size_pretty(pg_total_relation_size('bench_narrow')) AS narrow,
       pg_size_pretty(pg_total_relation_size('bench_wide')) AS wide,
       pg_size_pretty(pg_total_relation_size('bench_mixed')) AS mixed;

-- Warm the relations in shared buffers.
SET tessera.enable = off;
SELECT count(*) FROM bench_narrow;
SELECT count(*) FROM bench_wide;
SELECT count(*) FROM bench_mixed;
