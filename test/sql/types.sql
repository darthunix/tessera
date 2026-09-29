CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
LOAD 'tessera_limit';

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
-- Comparisons of dates, timestamps and timestamps with time zone run in
-- batches as those of their integers: infinities are the integers'
-- extremes, NULL is no row.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE d < '2000-01-05' AND ts >= '2000-01-01 03:00' AND tz <> '2000-01-01 00:05+00';
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d < '2000-01-05'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d >= '-infinity' AND d <= 'infinity' AND d <> '1999-12-25'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE ts BETWEEN '2000-01-01 02:00' AND '2000-01-01 12:30' OR ts > 'infinity'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE tz > '2000-01-01 00:10+00' AND tz <= 'infinity' OR tz = '-infinity'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE ts > '2000-01-01 12:00' OR ts = '1999-12-31 20:00' OR tz < '2000-01-01 00:03+00'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE ts < '2000-01-01 03:00' AND ts <> '1999-12-31 22:00' AND tz >= '2000-01-01 00:02+00' AND tz <> '2000-01-01 00:07+00'$$);
CREATE TABLE types_inf AS SELECT d, ts, tz FROM types_d
UNION ALL SELECT 'infinity', 'infinity', '-infinity' UNION ALL SELECT '-infinity', '-infinity', 'infinity';
SELECT types_same($$SELECT d, ts, tz FROM types_inf WHERE d > '2000-01-10' AND ts > '2000-01-01 05:00'$$);
SELECT types_same($$SELECT d FROM types_inf WHERE d < '1999-12-15' OR tz > '2000-01-01 00:30+00'$$);
DROP TABLE types_inf;
-- A date against a timestamp constant: the timestamp made the date bound
-- each comparison keeps, a midnight or within a day, infinities the
-- dates', dates past the timestamps' range above every finite timestamp
-- and below infinity, the constant on either side; a timestamp column
-- stays row by row.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE d >= date '2000-01-01' + interval '1 day';
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d >= date '1999-12-25' + interval '1 day' AND d < timestamp '2000-01-10 12:00'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d > timestamp '2000-01-03 12:00' OR d <= timestamp '1999-12-20 00:00:01'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d = timestamp '2000-01-03' OR d = timestamp '2000-01-04 00:00:01'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d <> timestamp '2000-01-03' AND d <> timestamp '2000-01-05 06:00'$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE timestamp '2000-01-02 06:00' < d AND timestamp '2000-01-12' >= d$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d < timestamp '1999-12-31 23:00' OR d > timestamp '1999-12-11 00:00' - interval '1 hour'$$);
CREATE TABLE types_dinf AS SELECT d FROM types_d
UNION ALL SELECT 'infinity' UNION ALL SELECT '-infinity' UNION ALL SELECT '294277-01-01' UNION ALL SELECT '5874897-12-31';
SELECT types_same($$SELECT d FROM types_dinf WHERE d < 'infinity'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d > '294276-12-31 23:59'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d = 'infinity'::timestamp OR d = '-infinity'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d >= '-infinity'::timestamp AND d <= '2000-01-01'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d <> 'infinity'::timestamp AND d > '1999-12-31 12:00'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d = '2000-01-01 06:00'::timestamp$$);
SELECT types_same($$SELECT d FROM types_dinf WHERE d <> '1999-12-31 18:00'::timestamp$$);
DROP TABLE types_dinf;
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE d > ts;
-- A smallint is its value sign-extended in the word and a boolean 0 or 1:
-- their comparisons run in batches as int4's; a smallint's arithmetic as
-- int4's, a smallint result checked against its range (22003), with an
-- integer as int4's, with a bigint as int8's over the smallint widened;
-- a boolean column is a condition, true where it is true, unknown where
-- NULL.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE s > 5 AND b AND s * 2 + 1 < 40;
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s > 5$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s = -3::int2 OR s <> 7 AND s < 20::int8$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s * 2 + 1 < 40 AND s - v::int2 > -1000 AND -s < 10$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s / 3 = 2 OR s % 7 = 1 OR s + 100000 > 100010 OR s * 3::int8 < -60$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s::int4 > 10 OR (v % 100)::int2 = 42$$);
SELECT types_same($$SELECT s * 1000, s + 32000, s::int8 - 5, s * 40 FROM types_f WHERE v < 50$$);
\set VERBOSITY terse
SELECT count(*) FROM types_f WHERE s * 2000::int2 > 0;
SELECT count(*) FROM types_f WHERE (v * 100)::int2 > 0;
SELECT count(*) FROM types_f WHERE s / (s - s) > 0;
\set VERBOSITY default
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE b$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE NOT b$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE b IS NOT TRUE AND b IS NOT UNKNOWN$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE b = false OR b < true AND s > 0$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE (b OR s > 20) AND NOT (b AND s < -20)$$);
SELECT types_same($$SELECT CASE WHEN b THEN v ELSE -v END, count(*) FROM types_f GROUP BY 1$$);
-- GREATEST and LEAST in batches: the argument that beats the result so
-- far by the type's comparison, NULLs aside, a scalar among them; abs of
-- an integer as GREATEST(x, -x), the smallest one failing as abs does;
-- IS [NOT] DISTINCT FROM as a condition of its parts.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE greatest(s, v % 7, 3) > 5 AND abs(s) < 10 AND s IS DISTINCT FROM 4;
SELECT types_same($$SELECT greatest(s, v % 7), least(s, v % 7, 0), greatest(s::int, NULL, v % 5), least(d, date '2000-01-05'), greatest(ts, timestamp '2000-01-01 05:00') FROM types_f WHERE v < 60$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE greatest(s, v % 7, 3) > 5 AND least(s, 0) < -10$$);
SELECT types_same($$SELECT abs(s), abs(v - 500), abs((v - 500)::int8) FROM types_f WHERE v < 40 OR v > 980$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE abs(s) < 10 AND abs(v - 500) > 400$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s IS DISTINCT FROM 4$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s IS NOT DISTINCT FROM 4 OR b IS DISTINCT FROM true$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE d IS DISTINCT FROM date '2000-01-03' AND (s + 1) IS DISTINCT FROM (v % 3)$$);
SELECT types_same($$SELECT count(*), sum(v) FROM types_f WHERE s IS DISTINCT FROM (CASE WHEN v > 2000 THEN s END)$$);
CREATE TABLE types_min AS SELECT -2147483648 AS x, (-32768)::int2 AS y;
\set VERBOSITY terse
SELECT count(*) FROM types_min WHERE abs(x) > 0;
SELECT count(*) FROM types_min WHERE abs(y) > 0;
\set VERBOSITY default
DROP TABLE types_min;
-- IN and NOT IN of more constants than an OR of comparisons takes: a set
-- of the integer words, sorted, each row's word looked up; NULL x
-- unknown, a NULL in the list making every miss unknown, for a smallint,
-- an integer, a date and a bigint expression, under NOT and in a CASE.
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_f WHERE v IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33);
CREATE FUNCTION types_list(step int, first int, count int, quote text DEFAULT '') RETURNS text
LANGUAGE sql AS $$
SELECT string_agg(quote || (first + g * step)::text || quote, ',') FROM generate_series(0, count - 1) AS g
$$;
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE v IN (%s)$$, types_list(7, 3, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE v NOT IN (%s)$$, types_list(7, 3, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE v IN (%s, %s)$$, types_list(-7, 300, 40), types_list(13, 5, 20)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE v NOT IN (%s, NULL)$$, types_list(7, 3, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE v IN (%s, NULL) OR v < 5$$, types_list(7, 3, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE s IN (%s)$$, types_list(3, -20, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE NOT (s IN (%s, NULL))$$, types_list(3, -20, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE (v % 100)::int8 IN (%s)$$, types_list(3, 1, 40)));
SELECT types_same(format($$SELECT count(*), sum(v) FROM types_f WHERE d IN (%s)$$, (SELECT string_agg(quote_literal(date '1999-12-10' + g), ',') FROM generate_series(0, 38) AS g)));
SELECT types_same(format($$SELECT sum(CASE WHEN v IN (%s) THEN 1 ELSE 0 END), count(*) FROM types_f$$, types_list(11, 0, 50)));
DROP FUNCTION types_list(int, int, int, text);
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
-- Text, varchar and bytea are hashed and compared by their bytes, without
-- a call through fmgr: a value stored compressed goes the type's way and
-- is one group with the same value computed, uncompressed; under a
-- nondeterministic collation every value goes the type's way.
CREATE TABLE types_b AS
SELECT i AS id, CASE WHEN i % 9 = 0 THEN NULL ELSE ('v' || (i % 23))::varchar END AS v,
       CASE WHEN i % 11 = 0 THEN NULL ELSE convert_to('b' || (i % 19), 'UTF8') END AS b,
       repeat('long', 3000) || (i % 5) AS big
FROM generate_series(1, 3000) AS i;
ANALYZE types_b;
SELECT pg_column_compression(big) IS NOT NULL AS compressed FROM types_b LIMIT 1;
SELECT types_same($$SELECT v, count(*) FROM types_b GROUP BY v$$);
SELECT types_same($$SELECT b, count(*), max(id) FROM types_b GROUP BY b$$);
SELECT types_same($$SELECT length(t), count(*) FROM (SELECT big AS t FROM types_b UNION ALL SELECT repeat('long', 3000) || (i % 7) FROM generate_series(1, 50) AS i) AS q GROUP BY t$$);
-- Values past a quarter of the dictionary's block of 64 kB are copies of their own.
SELECT types_same($$SELECT length(t), md5(t), count(*) FROM (SELECT repeat(v, 5000) AS t FROM types_b UNION ALL SELECT v FROM types_b) AS q GROUP BY t$$);
SELECT types_same($$SELECT length(t), md5(t) FROM (SELECT big AS t FROM types_b INTERSECT ALL SELECT repeat('long', 3000) || (i % 3) FROM generate_series(1, 700) AS i) AS q$$);
SELECT types_same($$SELECT md5(big) FROM (SELECT big FROM types_b EXCEPT ALL SELECT repeat('long', 3000) || (i % 3) FROM generate_series(1, 700) AS i) AS q$$);
CREATE COLLATION types_nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
SELECT types_same($$SELECT lower(t), count(*) FROM (SELECT (CASE WHEN id % 2 = 0 THEN upper(v) ELSE v END) COLLATE types_nd AS t FROM types_b) AS q GROUP BY t$$);
SELECT count(*) FROM (SELECT (CASE WHEN id % 2 = 0 THEN upper(v) ELSE v END) COLLATE types_nd AS t FROM types_b GROUP BY 1) AS q;
DROP COLLATION types_nd;
DROP TABLE types_b;
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

-- Sorts by keys a word does not hold: the kernels order the keys up to
-- the first such one, whose word is its abbreviated key (numeric, text
-- under "C", uuid), and the rows whose words are equal are ordered by the
-- type's comparison; numeric NaN and infinities, 1.0 and 1.00 equal,
-- text with long common prefixes, two such keys, float8 after an integer.
CREATE TABLE types_s AS
SELECT id, t, long, w,
       CASE WHEN id % 97 = 0 THEN 'NaN'::numeric WHEN id % 89 = 0 THEN 'Infinity'::numeric
            WHEN id % 83 = 0 THEN '-Infinity'::numeric ELSE n END AS n,
       one, md5((id % 700)::text)::uuid AS u,
       CASE WHEN id % 31 = 0 THEN NULL WHEN id % 37 = 0 THEN 'NaN'::float8
            WHEN id % 41 = 0 THEN '-0'::float8 ELSE (id % 90) / 7.0 END AS f
FROM types_g;
ANALYZE types_s;
EXPLAIN (COSTS OFF) SELECT id, n FROM types_s ORDER BY n, id;
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n, id$$);
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n DESC NULLS LAST, id$$);
SELECT types_order($$SELECT id, one FROM types_s ORDER BY one, id DESC$$);
EXPLAIN (COSTS OFF) SELECT id, t FROM types_s ORDER BY t COLLATE "C" DESC, id;
SELECT types_order($$SELECT id, t FROM types_s ORDER BY t COLLATE "C", id$$);
SELECT types_order($$SELECT id, t FROM types_s ORDER BY t COLLATE "C" DESC NULLS FIRST, id$$);
SELECT types_order($$SELECT id, long FROM types_s ORDER BY long COLLATE "C", id DESC$$);
SELECT types_order($$SELECT id, n, t FROM types_s ORDER BY n, t COLLATE "C" DESC, id$$);
SELECT types_order($$SELECT id, u FROM types_s ORDER BY u DESC, id$$);
SELECT types_order($$SELECT id, w, t FROM types_s ORDER BY w, t, id$$);
SELECT types_order($$SELECT id, w, f FROM types_s ORDER BY w DESC, f NULLS FIRST, id$$);
SELECT types_order($$SELECT id, upper(t) FROM types_s ORDER BY upper(t) COLLATE "C", n, id$$);
CREATE FUNCTION types_sort_method(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query
    LOOP
        IF line ~ 'TessSort|Sort Method|Rebuilt' THEN
            RETURN NEXT regexp_replace(line, '\d+', 'N', 'g');
        END IF;
    END LOOP;
END
$$;
-- A first key without an abbreviated key (float8; text under a collation
-- of libc too): every row one group, which the comparison orders, NaN
-- last, -0 equal to 0, NULLs at their place. varchar by text's family.
EXPLAIN (COSTS OFF) SELECT id, f FROM types_s ORDER BY f, id;
SELECT types_order($$SELECT id, f FROM types_s ORDER BY f, id$$);
SELECT types_order($$SELECT id, f FROM types_s ORDER BY f DESC NULLS LAST, id LIMIT 30$$);
SELECT types_order($$SELECT id, f, n FROM types_s ORDER BY f NULLS FIRST, n DESC, id LIMIT 200$$);
EXPLAIN (COSTS OFF) SELECT id FROM types_s ORDER BY t::varchar COLLATE "C", id;
SELECT types_order($$SELECT id, t::varchar FROM types_s ORDER BY t::varchar COLLATE "C" DESC, id$$);
-- Top-N: a heap in C by the items' words and then the comparisons; rows
-- whose words equal the worst's are compared by value (1.0 and 1.00, text
-- with a common prefix of 45 bytes), a bound past the rows, an offset,
-- keys in the reverse of the rows' order, rebuilt past 65536 records.
EXPLAIN (COSTS OFF) SELECT id, n FROM types_s ORDER BY n, id LIMIT 5;
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n, id LIMIT 5$$);
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n DESC NULLS LAST, id LIMIT 40$$);
SELECT types_order($$SELECT id, one FROM types_s ORDER BY one, id DESC LIMIT 3$$);
SELECT types_order($$SELECT id, long FROM types_s ORDER BY long COLLATE "C" DESC, id LIMIT 25$$);
SELECT types_order($$SELECT id, n, t FROM types_s ORDER BY n, t COLLATE "C" DESC, id LIMIT 30 OFFSET 7$$);
SELECT types_order($$SELECT id, w, t FROM types_s ORDER BY w, t, id LIMIT 100$$);
SELECT types_order($$SELECT id, u FROM types_s ORDER BY u, id LIMIT 6000$$);
SELECT types_order($$SELECT id FROM types_s ORDER BY t COLLATE "C" NULLS FIRST, id LIMIT 0$$);
SELECT types_order($$SELECT i, s FROM (SELECT i, (-i)::numeric AS s FROM generate_series(1, 200000) AS i) AS q ORDER BY s, i LIMIT 10$$);
SELECT types_sort_method($$SELECT i, s FROM (SELECT i, (-i)::numeric AS s FROM generate_series(1, 200000) AS i) AS q ORDER BY s, i LIMIT 10$$);
-- The five best rows come first and outlive every rebuild.
SELECT types_order($$SELECT i, s FROM (SELECT i, CASE WHEN i <= 5 THEN -1000000 - i ELSE -i END::numeric AS s FROM generate_series(1, 200000) AS i) AS q ORDER BY s, i LIMIT 10$$);
-- Past work_mem: runs on disk, merged in C by the words and the comparisons.
SET work_mem = '64kB';
SELECT types_sort_method($$SELECT id, long, n FROM types_s ORDER BY long COLLATE "C", n DESC, id$$);
SELECT types_order($$SELECT id, long, n FROM types_s ORDER BY long COLLATE "C", n DESC, id$$);
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n NULLS FIRST, id$$);
SELECT types_order($$SELECT id, w, t FROM types_s ORDER BY w, t, id$$);
-- A scrollable cursor over one run read by blocks.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT id, n FROM types_s ORDER BY n DESC, id;
FETCH ABSOLUTE 2500 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH LAST FROM c;
FETCH FIRST FROM c;
CLOSE c;
COMMIT;
RESET work_mem;
-- Rescan with a new parameter sorts anew; workers sort their shares and
-- TessGatherMerge merges them by their words and the comparisons, the
-- leader's share among them, a bound reaching the workers.
SELECT types_order($$SELECT x, (SELECT string_agg(id::text, ',') FROM (SELECT id FROM types_s WHERE w = x ORDER BY n DESC, id) AS q) FROM generate_series(0, 3) AS x$$);
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
EXPLAIN (COSTS OFF) SELECT id, n FROM types_s ORDER BY n, id;
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n, id$$);
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n DESC NULLS LAST, id$$);
SELECT types_order($$SELECT id, one FROM types_s ORDER BY one, id DESC$$);
SELECT types_order($$SELECT id, long FROM types_s ORDER BY long COLLATE "C", id$$);
SELECT types_order($$SELECT id, n, t FROM types_s ORDER BY n, t COLLATE "C" DESC, id$$);
SELECT types_order($$SELECT id, w, t FROM types_s ORDER BY w, t, id$$);
EXPLAIN (COSTS OFF) SELECT id, long FROM types_s ORDER BY long COLLATE "C" DESC, id LIMIT 20;
SELECT types_order($$SELECT id, long FROM types_s ORDER BY long COLLATE "C" DESC, id LIMIT 20$$);
SELECT types_order($$SELECT id, u FROM types_s ORDER BY u, id LIMIT 700$$);
-- A first key without an abbreviated key: the merge by the comparison.
EXPLAIN (COSTS OFF) SELECT id, f FROM types_s ORDER BY f DESC, id;
SELECT types_order($$SELECT id, f FROM types_s ORDER BY f DESC, id$$);
-- Two workers' shares of 200000 rows with equal words: numeric of 1000
-- values, text whose abbreviated keys share a prefix of 21 bytes.
CREATE TABLE types_p AS
SELECT i AS id, ((i % 1000) * 1.5)::numeric AS n, 'k' || repeat('x', 20) || (i % 777) AS t
FROM generate_series(1, 200000) AS i;
ANALYZE types_p;
SET parallel_leader_participation = off;
SELECT types_order($$SELECT id, n FROM types_p ORDER BY n, id$$);
SELECT types_order($$SELECT id, t FROM types_p ORDER BY t COLLATE "C" DESC, id$$);
SELECT types_order($$SELECT id, t, n FROM types_p ORDER BY t COLLATE "C", n DESC, id LIMIT 50$$);
SELECT types_order($$SELECT id, n FROM types_s ORDER BY n, id$$);
RESET parallel_leader_participation;
DROP TABLE types_p;
SET work_mem = '64kB';
SELECT types_order($$SELECT id, long, n FROM types_s ORDER BY long COLLATE "C", n DESC, id$$);
RESET work_mem;
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET max_parallel_workers_per_gather = 0;
DROP TABLE types_s;
DROP FUNCTION types_sort_method(text);

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
-- A column under a binary coercion is the column: varchar compared as
-- text, with varchar and with text; a domain over int4 as a word.
CREATE DOMAIN types_pos AS int CHECK (VALUE >= 0);
CREATE TABLE types_v AS SELECT id, t::varchar(10) AS v, w::types_pos AS p FROM types_h;
ANALYZE types_v;
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_v AS a JOIN types_v AS b ON a.v = b.v;
SELECT types_same($$SELECT count(*), sum(b.id) FROM types_v AS a JOIN types_v AS b ON a.v = b.v$$);
SELECT types_same($$SELECT count(*), sum(types_g.id) FROM types_g JOIN types_v ON types_g.t = types_v.v$$);
SELECT types_same($$SELECT count(*), sum(types_g.id) FROM types_g JOIN types_v ON types_g.w = types_v.p$$);
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_g JOIN types_v ON types_g.w = types_v.p;
DROP TABLE types_v;
DROP DOMAIN types_pos;
-- No Bloom filter below: the scan has the values, not their hashes.
SET tessera.join_bloom_ratio = 1;
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t WHERE types_g.long LIKE '%1%'$$);
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.w = types_h.w AND types_g.t = types_h.t WHERE types_g.long LIKE '%1%'$$);
RESET tessera.join_bloom_ratio;
-- Workers build one shared table of hashes.
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SET max_parallel_workers_per_gather = 2;
EXPLAIN (COSTS OFF) SELECT count(*) FROM types_g JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w;
SELECT types_same($$SELECT count(*), sum(types_h.id) FROM types_g JOIN types_h ON types_g.t = types_h.t AND types_g.w = types_h.w$$);
SELECT types_same($$SELECT count(*), count(types_h.id) FROM types_g LEFT JOIN types_h ON types_g.n = types_h.n$$);
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;
SET max_parallel_workers_per_gather = 0;
-- Past hash_mem both sides spill with the hashes as keys.
SET work_mem = '64kB';
SELECT types_same($$SELECT count(*), sum(b.id) FROM types_g AS a JOIN types_g AS b ON a.long = b.long AND a.t = b.t$$);
SELECT types_same($$SELECT count(*), count(b.id) FROM types_g AS a LEFT JOIN types_g AS b ON a.long = b.long AND a.w = b.w$$);
RESET work_mem;
DROP TABLE types_g, types_h;

-- Equal values of other forms: numeric 1.0, 1.00 and 1.000, float8 -0
-- and 0, text of either case under a case-insensitive collation. A group
-- of several keys goes out in its first row's form, as the core's hashed
-- grouping puts it out, though its value's number keeps the first form of
-- the whole input; a single key's group is its value's.
CREATE TABLE types_v AS
SELECT i AS id, i % 7 AS w,
       CASE i % 3 WHEN 0 THEN 1.0 WHEN 1 THEN 1.00 ELSE 1.000 END::numeric AS n,
       CASE WHEN i % 4 < 2 THEN '-0'::float8 ELSE 0::float8 END AS f,
       (CASE WHEN i % 5 < 2 THEN 'K' ELSE 'k' END || i % 2) COLLATE types_ci AS c
FROM generate_series(1, 3000) AS i;
ANALYZE types_v;
SET enable_sort = off;
EXPLAIN (COSTS OFF) SELECT w, n, count(*) FROM types_v GROUP BY w, n;
SELECT types_same($$SELECT w, n, count(*) FROM types_v GROUP BY w, n$$);
SELECT types_same($$SELECT w, f, count(*) FROM types_v GROUP BY w, f$$);
SELECT types_same($$SELECT w, c, count(*) FROM types_v GROUP BY w, c$$);
SELECT types_same($$SELECT DISTINCT n, f, c FROM types_v$$);
SELECT types_same($$SELECT n, count(*) FROM types_v GROUP BY n$$);
SELECT types_same($$SELECT w, n, f FROM types_v INTERSECT SELECT w, n, f FROM types_v WHERE id > 1000$$);
-- Past hash_mem: a partition's groups by a dictionary of its own.
SET work_mem = '64kB';
SELECT types_same($$SELECT id % 500, n, count(*) FROM types_v GROUP BY 1, 2$$);
RESET work_mem;
RESET enable_sort;
DROP TABLE types_v;
DROP COLLATION types_ci;

DROP FUNCTION types_order(text);
DROP FUNCTION types_same(text);
DROP TABLE types_f, types_d;
DROP EXTENSION tessera;
