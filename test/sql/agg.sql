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
-- Pages and batches: wide rows make the scan pin many pages.
CREATE TABLE agg_wide AS
SELECT i AS a, repeat('x', 500) AS pad FROM generate_series(1, 1000) AS i;
SELECT agg_same($$SELECT count(*) FROM agg_wide WHERE a % 3 = 0$$);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM agg_wide WHERE a % 3 = 0;
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
