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

-- The node above the native scan, above the filter, above pack.
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
-- Left to the core: another type, a cast, avg.
EXPLAIN (COSTS OFF) SELECT count(c) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT sum(a::bigint) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT avg(a) FROM agg_t;
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

-- A join below: pack turns its rows into batches.
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

-- Under a single-copy Gather.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE a > 100;
SELECT count(*) FROM agg_t WHERE a > 100;
RESET debug_parallel_query;

-- Left to the core: DISTINCT and FILTER in the aggregate, GROUP BY, a
-- window function, an empty relation, and the switch.
EXPLAIN (COSTS OFF) SELECT count(DISTINCT a) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT count(*) FILTER (WHERE a > 100) FROM agg_t;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM agg_t GROUP BY b;
EXPLAIN (COSTS OFF) SELECT count(*) OVER () FROM agg_t LIMIT 1;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t WHERE false;
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT count(*) FROM agg_t;
RESET tessera.enable;

DROP TABLE agg_t;
DROP FUNCTION agg_same(text);
DROP EXTENSION tessera;
