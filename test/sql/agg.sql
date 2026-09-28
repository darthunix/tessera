CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_limit';

-- The same result with Tessera on and off, as text.
CREATE FUNCTION agg_same(query text) RETURNS text
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
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('MISMATCH on %s off %s', with_tessera, without_tessera);
    END IF;
    RETURN with_tessera::text;
END $$;

-- Several batches; a is NULL in every seventh row.
CREATE TABLE agg_t (a int, b int, c text);
INSERT INTO agg_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i END, i % 10, 'r' || i
FROM generate_series(1, 300) AS i;

-- Without the kernels module no aggregate is registered: the core aggregates.
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t;
LOAD 'tessera_kernels';

-- The node above the native scan, above the filter with a batch clause and
-- with a row-wise one.
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE a > 100;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE c <> 'r5';
SELECT agg_same($$SELECT count(*) FROM agg_t$$);
SELECT agg_same($$SELECT count(*) FROM agg_t WHERE a > 100$$);
SELECT agg_same($$SELECT count(*) FROM agg_t WHERE c <> 'r5'$$);
-- The same aggregate twice is one column of the scan tuple; expressions
-- above the aggregates are the plan's projection over that tuple.
SELECT agg_same($$SELECT count(*), count(*) FROM agg_t WHERE a > 290$$);
EXPLAIN (COSTS OFF, VERBOSE) SELECT count(*) + 1 AS above, count(*)::int AS narrow FROM agg_t;
SELECT agg_same($$SELECT count(*) + 1 AS above, count(*)::int AS narrow, count(*) > 100 AS many FROM agg_t$$);
-- HAVING is the plan's qual over the aggregates: a true and a false one.
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE a > 100 HAVING count(*) > 100;
SELECT agg_same($$SELECT count(*) FROM agg_t WHERE a > 100 HAVING count(*) > 100$$);
SELECT agg_same($$SELECT count(*) FROM agg_t WHERE a > 100 HAVING count(*) > 1000$$);
SELECT count(*) FROM agg_t WHERE a > 100 HAVING count(*) > 1000;
-- Nothing survives the filter.
SELECT agg_same($$SELECT count(*) FROM agg_t WHERE a > 1000000$$);

-- Aggregates of a column: count skips its NULLs, sum, min and max are
-- NULL without a value; the argument may be a chain over the column.
EXPLAIN (COSTS OFF, VERBOSE)
SELECT count(a), sum(a), min(a), max(a), count(*) FROM agg_t WHERE b > 5;
SELECT agg_same($$SELECT count(a), sum(a), min(a), max(a), count(*) FROM agg_t WHERE b > 5$$);
SELECT agg_same($$SELECT count(a), sum(a), min(a), max(a), count(*) FROM agg_t$$);
SELECT agg_same($$SELECT count(a), sum(a), min(a), max(a), count(*) FROM agg_t WHERE a > 1000000$$);
SELECT agg_same($$SELECT sum(a), min(a) FROM agg_t WHERE a IS NULL$$);
-- Survivors too few to fill a gathered column are evaluated at the end.
SELECT agg_same($$SELECT count(a), sum(a), min(a), max(a), count(*) FROM agg_t WHERE a % 50 = 0$$);
SELECT agg_same($$SELECT sum(a + 1), min(-a), max(a % 7), count(a * 2), sum(100 - a) FROM agg_t WHERE a > 200$$);
SELECT agg_same($$SELECT sum(a) / count(*) AS mean, sum(a)::numeric / 2 AS half FROM agg_t WHERE a > 200$$);
SELECT agg_same($$SELECT sum(x.a), max(y.b) FROM agg_t AS x JOIN agg_t AS y ON x.b = y.b WHERE x.a > 295$$);
-- A parameter in the argument, and a rescan with a changed one.
PREPARE shifted(int) AS SELECT sum(a + $1), count(a) FROM agg_t WHERE a > 290;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF, VERBOSE) EXECUTE shifted(1);
EXECUTE shifted(1);
EXECUTE shifted(1000);
RESET plan_cache_mode;
DEALLOCATE shifted;
SELECT o.b, (SELECT max(i.a + o.b) FROM agg_t AS i WHERE i.a < 50) AS shifted
FROM agg_t AS o WHERE o.a < 3 ORDER BY 1;
-- A hash aggregate under pack in a rescanned subquery must see the changed
-- parameter, or it reuses its table: the batch nodes pass it on themselves.
SELECT o.a, (SELECT count(*) FROM (SELECT b FROM agg_t AS i WHERE i.a < o.a GROUP BY b) AS s) AS groups
FROM agg_t AS o WHERE o.a < 5 ORDER BY 1;
-- The argument's column is deformed for the surviving rows only.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sum(b) FROM agg_t WHERE a > 290;
-- An error in the argument is the chain's.
SELECT sum(a + 2147483647) FROM agg_t;
-- Any int4 expression is an argument: a chain over one column as before,
-- anything else row by row, both through the projection provider.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sum(a + b), max(a * b), min(CASE WHEN a > 295 THEN b ELSE a END), sum(a + 1)
FROM agg_t WHERE a > 290;
SELECT agg_same($$SELECT sum(a + b), max(a * b), min(CASE WHEN a > 295 THEN b ELSE a END), sum(a + 1), count(length(c)) FROM agg_t WHERE a > 290$$);
SELECT agg_same($$SELECT sum(a + b), count(a * b), max(b - a) FROM agg_t$$);
SELECT agg_same($$SELECT sum(a + b), min(a * b) FROM agg_t WHERE a % 50 = 0$$);
SELECT agg_same($$SELECT sum(a + b) FROM agg_t WHERE a > 100 HAVING sum(a + b) > 1000$$);
SELECT agg_same($$SELECT sum(a + b) FROM agg_t WHERE a > 100 HAVING sum(a + b) > 100000$$);
-- The rows are computed once per batch and the error is the executor's.
SELECT sum(a / (b - 5)) FROM agg_t WHERE a > 290;
SELECT sum(a * b + 2147483647) FROM agg_t WHERE a > 290;
-- A sort above the node in a correlated subquery rescans it only when it
-- sees the parameter as the node's: the arguments' parameters are shown.
SELECT o.b,
       (SELECT s FROM (SELECT sum(i.a + i.b * o.b) AS s FROM agg_t AS i WHERE i.a < 50) AS x
        ORDER BY s) AS shifted
FROM (VALUES (1), (2), (3)) AS o(b);
SELECT agg_same($$SELECT o.b, (SELECT s FROM (SELECT sum(i.a + i.b * o.b) AS s FROM agg_t AS i WHERE i.a < 50) AS x ORDER BY s) FROM (VALUES (1), (2), (3)) AS o(b)$$);
-- Two computed columns of the subquery's filter, added by the node above
-- the forwarded batches of its limit.
EXPLAIN (COSTS OFF)
SELECT sum(x + y) FROM (SELECT a + 1 AS x, b * 2 AS y FROM agg_t WHERE a > 100 LIMIT 50) AS s;
SELECT agg_same($$SELECT sum(x + y), max(x) FROM (SELECT a + 1 AS x, b * 2 AS y FROM agg_t WHERE a > 100 LIMIT 50) AS s$$);
-- count reads no value: any argument type, text included.
EXPLAIN (COSTS OFF) SELECT count(c) FROM agg_t;
SELECT agg_same($$SELECT count(c), count(a), count(*) FROM agg_t WHERE b > 5$$);
-- Through the core's functions, over the batches: sum over bigint
-- (a numeric state), avg.
EXPLAIN (COSTS OFF) SELECT sum(a::bigint) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT avg(a) FROM agg_t;

-- A bigint column: min and max over int8, the filter and the argument
-- chains through the mixed operators of bigint with an integer constant,
-- and the cast of an int4 column as an argument.
CREATE TABLE agg8_t (a bigint, b int, c text);
INSERT INTO agg8_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i * 4294967296 + i END, i % 10, 'r' || i
FROM generate_series(1, 300) AS i;
EXPLAIN (COSTS OFF, VERBOSE)
SELECT min(a), max(a), count(a), count(*) FROM agg8_t WHERE a > 100;
SELECT agg_same($$SELECT min(a), max(a), count(a), count(*) FROM agg8_t WHERE a > 100$$);
SELECT agg_same($$SELECT min(a), max(a), count(a) FROM agg8_t$$);
SELECT agg_same($$SELECT min(a), max(a) FROM agg8_t WHERE a > 4294967296 * 400$$);
SELECT agg_same($$SELECT min(a + 1), max(a * 2), min(-a), max(a % 7), min(b::bigint * 3) FROM agg8_t WHERE a > 4294967296 * 290$$);
SELECT agg_same($$SELECT min(a), max(a) FROM agg8_t WHERE a % 50 = 0$$);
-- The bigint extremes in the partial mode of a parallel plan.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT min(a), max(a), count(c) FROM agg8_t WHERE a > 100;
SELECT min(a), max(a), count(c) FROM agg8_t WHERE a > 100;
RESET debug_parallel_query;
-- An overflow in the argument chain is the chain's.
SELECT max(a * 4294967296) FROM agg8_t;
-- Through the core's functions: sum over bigint.
EXPLAIN (COSTS OFF) SELECT sum(a) FROM agg8_t WHERE a > 100;
DROP TABLE agg8_t;
-- Pages and batches: wide rows make the scan pin many pages.
CREATE TABLE agg_wide AS
SELECT i AS a, repeat('x', 500) AS pad FROM generate_series(1, 1000) AS i;
SELECT agg_same($$SELECT count(*) FROM agg_wide WHERE a % 3 = 0$$);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM agg_wide WHERE a % 3 = 0;
-- Few survivors per batch are gathered into one call per 64: 333 rows over
-- 67 batches make six calls of the sum, the last over the 13 left.
SELECT agg_same($$SELECT sum(a), min(a), max(a), count(a) FROM agg_wide WHERE a % 3 = 0$$);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT sum(a) FROM agg_wide WHERE a % 3 = 0;
DROP TABLE agg_wide;

-- A join below: the hash join publishes batches, no pack between.
EXPLAIN (COSTS OFF)
SELECT count(*) FROM agg_t AS x JOIN agg_t AS y ON x.b = y.b WHERE x.a > 295;
SELECT agg_same($$SELECT count(*) FROM agg_t AS x JOIN agg_t AS y ON x.b = y.b WHERE x.a > 295$$);

-- A limit above reads the node's batch.
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t LIMIT 1;
SELECT count(*) FROM agg_t LIMIT 1;

-- Rescans: a correlated subquery, then a prepared statement with a generic plan.
SELECT o.a, (SELECT count(*) FROM agg_t AS i WHERE i.a > o.a) AS above
FROM agg_t AS o WHERE o.a > 296 ORDER BY 1;
PREPARE above(int) AS SELECT count(*) FROM agg_t WHERE a > $1;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE above(150);
EXECUTE above(150);
EXECUTE above(299);
RESET plan_cache_mode;
DEALLOCATE above;

-- A scrollable cursor gets a Material above the node.
BEGIN;
DECLARE agg_cursor SCROLL CURSOR FOR SELECT count(*) FROM agg_t WHERE a > 100;
FETCH ALL FROM agg_cursor;
FETCH BACKWARD ALL FROM agg_cursor;
COMMIT;

-- GROUP BY: each row finds the record of its keys in a table, whose
-- payload holds the group's states; NULL keys form one group; the groups
-- go out in batches, to a row-wise parent one by one.
CREATE FUNCTION agg_explain(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
    limit_kb bigint := (SELECT setting::bigint FROM pg_settings WHERE name = 'work_mem') *
                       current_setting('hash_mem_multiplier')::float8;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
        -- Overrun repeats what the memory line says.
        CONTINUE WHEN line ~ 'Overrun: \d+ kB';
        IF line ~ 'Memory Usage: \d+ kB' THEN
            line := regexp_replace(line, 'Memory Usage: \d+ kB',
                CASE WHEN substring(line FROM 'Memory Usage: (\d+) kB')::bigint <= limit_kb
                     THEN 'Memory Usage: within hash_mem' ELSE 'Memory Usage: over hash_mem' END);
        END IF;
        RETURN NEXT line;
    END LOOP;
END $$;
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a), min(a), max(a) FROM agg_t GROUP BY b;
SELECT agg_explain($$SELECT b, count(*), sum(a) FROM agg_t WHERE a > 100 GROUP BY b$$);
SELECT agg_same($$SELECT b, count(*), count(a), sum(a), min(a), max(a) FROM agg_t GROUP BY b$$);
EXPLAIN (VERBOSE, COSTS OFF) SELECT a % 10, count(*), sum(b) FROM agg_t GROUP BY a % 10;
SELECT agg_same($$SELECT a % 10, count(*), sum(b) FROM agg_t GROUP BY a % 10$$);
SELECT agg_same($$SELECT a % 10 + 1, sum(b) * 2, max(a) - min(a) FROM agg_t GROUP BY a % 10$$);
SELECT agg_same($$SELECT b, a % 3, count(*), min(a) FROM agg_t GROUP BY b, a % 3$$);
SELECT agg_same($$SELECT b, count(*) FROM agg_t WHERE a > 100 GROUP BY b HAVING sum(a) > 3000$$);
SELECT agg_same($$SELECT b FROM agg_t GROUP BY b$$);
SELECT agg_same($$SELECT 1 AS one, count(*) FROM agg_t GROUP BY b$$);
SELECT agg_same($$SELECT b, count(*) FROM agg_t WHERE a < 0 GROUP BY b$$);
SELECT agg_same($$SELECT count(*), sum(a) FROM agg_t GROUP BY b / 100$$);
-- int8 keys and int8 extremes.
SELECT agg_same($$SELECT a::bigint * 4294967296 AS k, max(a::bigint * 3), min(b::bigint) FROM agg_t GROUP BY 1$$);
-- Over a join, and in a rescanned subquery with a parameter.
SELECT agg_same($$SELECT x.b, count(*), sum(y.a) FROM agg_t AS x JOIN agg_t AS y ON x.a = y.a GROUP BY x.b$$);
SELECT o.b, (SELECT max(s) FROM (SELECT i.b, sum(i.a) AS s FROM agg_t AS i WHERE i.a < o.a * 10 GROUP BY i.b) AS q) AS most
FROM agg_t AS o WHERE o.a < 4 ORDER BY 1;
-- Many groups: the planner expects few, the table grows; a sort above
-- reads the groups row by row.
CREATE TABLE agg_many (k int, v int);
ANALYZE agg_many;
INSERT INTO agg_many SELECT g, g % 7 FROM generate_series(1, 200000) AS g;
SELECT agg_explain($$SELECT k % 100000, sum(v) FROM agg_many GROUP BY k % 100000$$);
SELECT agg_same($$SELECT count(*), sum(s), min(s), max(s) FROM (SELECT k % 100000 AS g, sum(v) AS s FROM agg_many GROUP BY k % 100000) AS q$$);
SELECT k, sum(v) FROM agg_many GROUP BY k ORDER BY sum(v) DESC, k LIMIT 3;
DROP TABLE agg_many;
-- Spilling, at a hash_mem of 2 MB: 200000 groups of three rows, with NULL
-- keys and NULL values, go past it; the groups go into partitions, the
-- largest to disk while the input is read, and each partition's groups
-- on disk merge with those in memory. Every group is compared by an md5
-- of them all.
CREATE TABLE agg_spill (k int, k8 bigint, v int);
INSERT INTO agg_spill
SELECT CASE WHEN g % 97 = 0 THEN NULL ELSE g % 200000 END, g % 7,
       CASE WHEN g % 11 = 0 THEN NULL ELSE g END
FROM generate_series(1, 600000) AS g;
ANALYZE agg_spill;
SET work_mem = '1MB';
-- The partitions follow the planner's estimate of the groups, a sample's:
-- the counts of spilling are masked, and whether a partition split.
SELECT regexp_replace(line, '(Table Grows|Batches|Evictions|Spilled Chunks|Disk Usage): \d+', '\1: N')
FROM agg_explain($$SELECT k, count(*), sum(v) FROM agg_spill GROUP BY k$$) AS line
WHERE line !~ 'Split Partitions';
SELECT agg_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), count(v), sum(v), min(v), max(v) FROM agg_spill GROUP BY k) AS q$$);
-- A partition's groups went to disk many times, its file larger than
-- hash_mem, but they fit: the estimate of its groups merges it, no split.
SELECT count(*) AS splits
FROM agg_explain($$SELECT k, count(*), sum(v) FROM agg_spill GROUP BY k$$) AS line
WHERE line ~ 'Split Partitions';
-- Two keys, int4 and int8; HAVING over the merged states.
SELECT agg_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, k8, count(v), sum(v), max(k8) FROM agg_spill GROUP BY k, k8) AS q$$);
SELECT agg_same($$SELECT count(*), sum(c) FROM (SELECT k, count(*) AS c FROM agg_spill GROUP BY k HAVING count(*) > 2 AND min(v) > 1000) AS q$$);
-- Ten hot groups among the rare ones: their rows keep folding in memory.
SELECT agg_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT CASE WHEN v % 3 = 0 THEN v % 10 ELSE k END AS g, count(*), sum(v), min(v) FROM agg_spill GROUP BY 1) AS q$$);
-- 200000 groups the planner expects 10 of: partitions split into a level below.
CREATE FUNCTION agg_rows() RETURNS TABLE (k int, v int) LANGUAGE sql ROWS 10
AS 'SELECT g % 200000, g FROM generate_series(1, 600000) AS g';
SELECT agg_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT k, count(*), sum(v), min(v), max(v) FROM agg_rows() GROUP BY k) AS q$$);
SELECT count(*) AS splits
FROM agg_explain($$SELECT k, count(*), sum(v) FROM agg_rows() GROUP BY k$$) AS line
WHERE line ~ 'Split Partitions';
-- Row by row to a sort above, and a rescan with a parameter.
SELECT k, count(*), sum(v) FROM agg_spill GROUP BY k ORDER BY sum(v) DESC NULLS LAST, k LIMIT 3;
SELECT agg_same($$SELECT x, (SELECT count(*) FROM (SELECT k FROM agg_spill WHERE k8 < x GROUP BY k) AS q) FROM (VALUES (1), (4)) AS v(x)$$);
-- A partition merging more groups than its estimate: its index grows in
-- the middle of a chunk read back, which is no group of the table and
-- must stay out of the new index (a group found in it was never merged,
-- and went out twice: 117 of 300000 groups at a hash_mem of 256 kB).
CREATE TABLE agg_regrow AS SELECT g % 300000 AS k, g AS v FROM generate_series(1, 1000000) AS g;
ANALYZE agg_regrow;
SET work_mem = '128kB';
SET enable_sort = off;
SELECT agg_same($$SELECT k, count(*), sum(v) FROM agg_regrow GROUP BY k$$) NOT LIKE 'MISMATCH%' AS same;
RESET enable_sort;
DROP TABLE agg_regrow;
RESET work_mem;
DROP FUNCTION agg_rows();
DROP TABLE agg_spill;
-- Left to the core: grouping sets, a text key, a column the primary key
-- makes functionally dependent, and hash aggregation disabled.
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM agg_t GROUP BY GROUPING SETS ((b), ());
EXPLAIN (COSTS OFF) SELECT c, count(*) FROM agg_t GROUP BY c;
CREATE TABLE agg_pk (id int PRIMARY KEY, label text);
EXPLAIN (COSTS OFF) SELECT id, label, count(*) FROM agg_pk GROUP BY id;
DROP TABLE agg_pk;
SET enable_hashagg = off;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM agg_t GROUP BY b;
RESET enable_hashagg;
DROP FUNCTION agg_explain(text);

-- SELECT DISTINCT groups without aggregates: NULL a value of its own, two
-- keys, an int8 and an expression; over a filter; above it a count.
EXPLAIN (COSTS OFF) SELECT DISTINCT b FROM agg_t;
SELECT agg_same($$SELECT DISTINCT b FROM agg_t$$);
SELECT agg_same($$SELECT DISTINCT a % 13 FROM agg_t$$);
SELECT agg_same($$SELECT count(*), sum(b), sum(r) FROM (SELECT DISTINCT b, a % 3 AS r FROM agg_t) AS s$$);
EXPLAIN (COSTS OFF) SELECT count(*), sum(b), sum(r) FROM (SELECT DISTINCT b, a % 3 AS r FROM agg_t) AS s;
SELECT agg_same($$SELECT count(*), sum(x) FROM (SELECT DISTINCT a::bigint * 1000000000 AS x FROM agg_t WHERE a > 250) AS s$$);
SELECT agg_same($$SELECT count(*) FROM (SELECT DISTINCT a FROM agg_t) AS s$$);
SELECT agg_same($$SELECT DISTINCT b FROM agg_t WHERE false$$);
-- Left to the core: DISTINCT ON, a text key.
EXPLAIN (COSTS OFF) SELECT DISTINCT ON (b) b, a FROM agg_t ORDER BY b, a;
EXPLAIN (COSTS OFF) SELECT DISTINCT c FROM agg_t;

-- Under a single-copy Gather.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE a > 100;
SELECT count(*) FROM agg_t WHERE a > 100;
RESET debug_parallel_query;

-- DISTINCT in an aggregate: a table of pairs per aggregate, a row going
-- in only when its pair is new; NULL arguments skipped; without and with
-- GROUP BY, where the core sorts and the node hashes instead.
EXPLAIN (COSTS OFF) SELECT count(DISTINCT b) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT b, count(DISTINCT a % 7) FROM agg_t GROUP BY b;
SELECT agg_same($$SELECT count(DISTINCT b) FROM agg_t$$);
SELECT agg_same($$SELECT count(DISTINCT a % 13), sum(DISTINCT a % 13), min(DISTINCT a), max(DISTINCT a) FROM agg_t$$);
SELECT agg_same($$SELECT count(DISTINCT a % 5), count(DISTINCT b), count(*), sum(a) FROM agg_t$$);
SELECT agg_same($$SELECT b, count(DISTINCT a % 7), sum(DISTINCT a % 7), count(*) FROM agg_t GROUP BY b$$);
SELECT agg_same($$SELECT a % 3, count(DISTINCT b), count(DISTINCT a::bigint * 1000000000) FROM agg_t GROUP BY a % 3$$);
SELECT agg_same($$SELECT count(DISTINCT a) FROM agg_t WHERE a > 280$$);
SELECT agg_same($$SELECT count(DISTINCT a) FROM agg_t WHERE false$$);
SELECT agg_same($$SELECT b, count(DISTINCT a) FROM agg_t WHERE a > 280 GROUP BY b HAVING count(DISTINCT a) > 1$$);
-- Many groups and pairs: the tables grow.
CREATE TABLE agg_dm AS SELECT g AS k, g * 7919 % 100003 AS v FROM generate_series(1, 200000) AS g;
ANALYZE agg_dm;
SELECT agg_same($$SELECT count(*), sum(n) FROM (SELECT k % 1000 AS g, count(DISTINCT v % 97) AS n FROM agg_dm GROUP BY k % 1000) AS s$$);
SELECT agg_same($$SELECT count(DISTINCT v) FROM agg_dm$$);
DROP TABLE agg_dm;
-- Left to the core: a text argument.
EXPLAIN (COSTS OFF) SELECT count(DISTINCT c) FROM agg_t;

-- Left to the core: FILTER in the aggregate, a window function, an empty
-- relation, and the switch.
EXPLAIN (COSTS OFF) SELECT count(*) FILTER (WHERE a > 100) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT count(*) OVER () FROM agg_t LIMIT 1;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE false;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t;
RESET tessera.enable;

-- Any other aggregate without GROUP BY: the core's transition function
-- over the arguments' columns of each batch, then its final function.
CREATE TABLE agg_any (a int, n numeric, f float8, t text, d date, b bool, j int8);
INSERT INTO agg_any
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i - 150 END, (i * 1.5)::numeric,
       CASE WHEN i % 11 = 0 THEN NULL ELSE i / 3.0 END,
       CASE WHEN i % 5 = 0 THEN NULL ELSE 't' || i END,
       date '2000-01-01' + i, i % 3 = 0, i::int8 * 100000000000
FROM generate_series(1, 300) AS i;
EXPLAIN (COSTS OFF) SELECT max(t), sum(n), avg(f) FROM agg_any;
EXPLAIN (VERBOSE, COSTS OFF) SELECT max(t), string_agg(t, ',') FROM agg_any WHERE a > 0;
SELECT agg_same($$SELECT max(t), min(t), sum(n), avg(n), avg(f), stddev(f), sum(j), avg(j), avg(a) FROM agg_any$$);
-- Arguments of several kinds and polymorphic ones: a delimiter, two
-- columns, arrays and JSON of any element, booleans, dates.
SELECT agg_same($$SELECT string_agg(t, '|'), corr(a, f), array_agg(a), array_agg(t), json_agg(d), bool_and(b), bool_or(b), max(d), bit_or(a), every(a > -200) FROM agg_any$$);
-- Mixed with the node's own aggregates, over a filter, with NULL only.
SELECT agg_same($$SELECT count(*), sum(a), max(t), avg(f) FROM agg_any WHERE a > 100$$);
SELECT agg_same($$SELECT max(t), sum(n), array_agg(a) FROM agg_any WHERE t IS NULL$$);
SELECT agg_same($$SELECT max(t), sum(n), array_agg(a), string_agg(t, ',') FROM agg_any WHERE a > 1000$$);
-- An expression argument, computed over the batch or row by row.
SELECT agg_same($$SELECT max(t || '!'), sum(a * 2.5), string_agg(upper(t), ',') FROM agg_any$$);
-- HAVING and an expression above the aggregates.
SELECT agg_same($$SELECT max(t) || '?', sum(n) / 2 FROM agg_any HAVING avg(f) > 10$$);
SELECT agg_same($$SELECT max(t) FROM agg_any HAVING avg(f) > 1000$$);
-- Rescan: a correlated subquery computes the aggregates anew.
SELECT agg_same($$SELECT g, (SELECT string_agg(t, ',') FROM agg_any WHERE a > g * 50) FROM generate_series(1, 4) AS g$$);
-- An error in a transition function is the core's.
\set VERBOSITY terse
SELECT sum(1 / (a - 100)::numeric) FROM agg_any;
\set VERBOSITY default
-- Left to the core: ORDER BY and FILTER in an aggregate, DISTINCT over
-- text, an ordered-set aggregate.
EXPLAIN (COSTS OFF) SELECT string_agg(t, ',' ORDER BY t) FROM agg_any;
EXPLAIN (COSTS OFF) SELECT max(t) FILTER (WHERE b) FROM agg_any;
EXPLAIN (COSTS OFF) SELECT count(DISTINCT t) FROM agg_any;
SELECT agg_same($$SELECT count(DISTINCT t), count(DISTINCT a), count(DISTINCT d) FROM agg_any$$);
EXPLAIN (COSTS OFF) SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY f) FROM agg_any;
-- With GROUP BY: each group's state a word of its record, a by-reference
-- one the address of its copy; rows of one group follow one another in a
-- batch, groups start from the initial value.
EXPLAIN (COSTS OFF) SELECT b, max(t) FROM agg_any GROUP BY b;
SELECT agg_same($$SELECT b, max(t), min(t), sum(n), avg(f), string_agg(t, ','), array_agg(a) FROM agg_any GROUP BY b$$);
SELECT agg_same($$SELECT a % 7, max(t), sum(n), stddev(f), bool_and(b), json_agg(d) FROM agg_any GROUP BY a % 7$$);
-- A group whose arguments are all NULL, a NULL key, groups with the
-- node's own aggregates and a DISTINCT one, HAVING over a generic one.
SELECT agg_same($$SELECT a > 0, max(t), count(*), sum(a), count(DISTINCT a), avg(n) FROM agg_any WHERE t IS NULL GROUP BY a > 0$$);
SELECT agg_same($$SELECT b, max(t) || '!', sum(n) FROM agg_any GROUP BY b HAVING max(t) > 't5'$$);
-- Many groups: the table grows and adds chunks, which never move.
CREATE TABLE agg_groups AS
SELECT i % 20000 AS g, 'v' || i AS t, (i * 0.5)::numeric AS n FROM generate_series(1, 80000) AS i;
ANALYZE agg_groups;
SELECT agg_same($$SELECT count(*), max(m), sum(s), max(l) FROM (SELECT g, max(t) AS m, sum(n) AS s, length(string_agg(t, ',')) AS l FROM agg_groups GROUP BY g) AS q$$);
-- Rescan builds the groups anew.
SELECT agg_same($$SELECT x, (SELECT max(m) FROM (SELECT g, max(t) AS m FROM agg_groups WHERE g < x GROUP BY g) AS q) FROM generate_series(1, 3) AS x$$);
-- Their states cannot spill: past hash_mem by the planner's estimate the
-- grouping stays with the core.
SET work_mem = '64kB';
EXPLAIN (COSTS OFF) SELECT g, max(t) FROM agg_groups GROUP BY g;
RESET work_mem;
DROP TABLE agg_groups;
-- In a parallel plan: each participant's partial state, serialized where
-- the state is internal, for the core's Finalize Aggregate.
CREATE TABLE agg_any_big AS
SELECT i AS a, (i * 1.5)::numeric AS n, 't' || i AS t FROM generate_series(1, 200000) AS i;
ANALYZE agg_any_big;
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
EXPLAIN (COSTS OFF) SELECT max(t), sum(n), avg(n), string_agg(t, ',') IS NOT NULL FROM agg_any_big;
SELECT agg_same($$SELECT max(t), sum(n), avg(n), length(string_agg(t, ',')), array_length(array_agg(a), 1) FROM agg_any_big$$);
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
DROP TABLE agg_any, agg_any_big;

DROP TABLE agg_t;
DROP FUNCTION agg_same(text);
DROP EXTENSION tessera;
