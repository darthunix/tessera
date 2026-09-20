CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_limit';

CREATE TABLE filter_t (a int, b int, c text);
INSERT INTO filter_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i END, i % 10, 'r' || i
FROM generate_series(1, 200) AS i;

-- The rows of a query with Tessera on, once they equal the rows with it off.
CREATE FUNCTION filter_same(query text) RETURNS text
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
END
$$;

-- Without the kernels module the registry knows no predicate: no batch path.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100;

LOAD 'tessera_kernels';
-- The clauses move from the scan to the node.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND b < 5;
SELECT filter_same($$SELECT count(*), sum(a) FROM filter_t WHERE a > 100 AND b < 5$$);

-- Every comparison, a chain, the commutator, the unary minus.
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a = 50$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a <> 50$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a < 3$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a <= 3$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a >= 198$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a > 198$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE (a + 1) * 2 > 300$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE 100 < a$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE -a > -10$$);

-- A column the clauses need but the query does not: the scan below adds it.
EXPLAIN (VERBOSE, COSTS OFF) SELECT c FROM filter_t WHERE a > 195;
SELECT c FROM filter_t WHERE a > 195 ORDER BY c;
-- No column at all.
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE b > 5$$);
-- A projection above the node, since the node computes none.
EXPLAIN (COSTS OFF) SELECT a + 1 FROM filter_t WHERE a > 197;
SELECT a + 1 FROM filter_t WHERE a > 197;

-- More than one batch, batches left without rows, rows removed by count.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM filter_t WHERE a > 100;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM filter_t WHERE a > 1000;

-- A parameter, also NULL, through a generic plan.
SET plan_cache_mode = force_generic_plan;
PREPARE above(int) AS SELECT count(*) FROM filter_t WHERE a > $1;
EXPLAIN (COSTS OFF) EXECUTE above(150);
EXECUTE above(150);
EXECUTE above(NULL);
-- A pseudoconstant clause gates the scan: the node stays away.
PREPARE gated(int) AS SELECT count(*) FROM filter_t WHERE a > 150 AND $1 > 0;
EXPLAIN (COSTS OFF) EXECUTE gated(1);
EXECUTE gated(1);
DEALLOCATE above;
DEALLOCATE gated;
RESET plan_cache_mode;

-- A rescan with a changed parameter: a correlated subquery.
EXPLAIN (COSTS OFF)
SELECT o.a, (SELECT count(*) FROM filter_t AS i WHERE i.a > o.a) AS above
FROM filter_t AS o WHERE o.a > 197 ORDER BY o.a;
SELECT o.a, (SELECT count(*) FROM filter_t AS i WHERE i.a > o.a) AS above
FROM filter_t AS o WHERE o.a > 197 ORDER BY o.a;

-- A batch parent above: the limit node stops the input early.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 LIMIT 3;
SELECT a FROM filter_t WHERE a > 100 LIMIT 3;

-- Two filtered relations in a join.
SELECT filter_same($$SELECT count(*) FROM filter_t AS x JOIN filter_t AS y ON x.a = y.b WHERE x.a > 5 AND y.b > 5$$);

-- The clauses keep the planner's order: the guard comes first.
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a <> 5 AND 10 / (a - 5) > 0$$);
\set VERBOSITY terse
SELECT count(*) FROM filter_t WHERE 10 / (a - 5) > 0;
SET tessera.enable = off;
SELECT count(*) FROM filter_t WHERE 10 / (a - 5) > 0;
RESET tessera.enable;
\set VERBOSITY default

-- A parallel worker runs the node from the plan's text form.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT count(*) FROM filter_t WHERE a > 100;
SELECT count(*) FROM filter_t WHERE a > 100;
RESET debug_parallel_query;

-- A scrollable cursor: the planner adds Material above the node.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT a FROM filter_t WHERE a > 195;
FETCH 2 FROM c;
FETCH BACKWARD 1 FROM c;
COMMIT;

-- Only a SELECT gets the node.
EXPLAIN (COSTS OFF) UPDATE filter_t SET b = 0 WHERE a > 190;

SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100;
RESET tessera.enable;

DROP FUNCTION filter_same(text);
DROP TABLE filter_t;
DROP EXTENSION tessera;
