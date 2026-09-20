CREATE EXTENSION tessera;
LOAD 'tessera_limit';

CREATE TABLE limit_t (a int, b text);
INSERT INTO limit_t
SELECT i, CASE WHEN i % 10 = 0 THEN NULL ELSE 'row' || i END
FROM generate_series(1, 100) AS i;
CREATE TABLE limit_empty (a int);

-- Without a pack node loaded the hook adds no path: the core node stays.
EXPLAIN (COSTS OFF) SELECT a FROM limit_t LIMIT 3;

LOAD 'tessera_nodes';
EXPLAIN (COSTS OFF) SELECT a FROM limit_t LIMIT 3;
SELECT a, b FROM limit_t LIMIT 3;
SELECT a, b FROM limit_t LIMIT 3 OFFSET 98;
SELECT a FROM limit_t OFFSET 95;
SELECT count(*) FROM (SELECT a FROM limit_t LIMIT ALL) AS s;
SELECT count(*) FROM (SELECT a FROM limit_t LIMIT NULL) AS s;

-- A count of zero stops before the first batch is read.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM limit_t LIMIT 0;
-- More than one batch, with NULL values passing through.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM limit_t LIMIT 70;
SELECT count(*), count(b), sum(a) FROM (SELECT a, b FROM limit_t LIMIT 70) AS s;

-- WITH TIES stays with the core node.
EXPLAIN (COSTS OFF)
SELECT a FROM limit_t ORDER BY a FETCH FIRST 3 ROWS WITH TIES;

-- The bound reaches the sort below the pack node: a top-N sort.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM limit_t ORDER BY a DESC LIMIT 3;
SELECT a FROM limit_t ORDER BY a DESC LIMIT 3;

-- A parameter is re-evaluated on every rescan.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, (SELECT count(*) FROM (SELECT a FROM limit_t LIMIT g) AS s) AS n
FROM generate_series(1, 3) AS g;
SELECT g, (SELECT count(*) FROM (SELECT a FROM limit_t LIMIT g) AS s) AS n
FROM generate_series(1, 3) AS g;
PREPARE limited(int) AS SELECT a FROM limit_t LIMIT $1;
EXECUTE limited(2);
EXECUTE limited(5);
DEALLOCATE limited;

-- A pseudoconstant clause gates the scan with a Result the node could not
-- read through: the core limit stays.
SET plan_cache_mode = force_generic_plan;
PREPARE gated(int) AS SELECT a FROM limit_t WHERE $1 > 0 LIMIT 3;
EXPLAIN (COSTS OFF) EXECUTE gated(1);
EXECUTE gated(1);
DEALLOCATE gated;
RESET plan_cache_mode;

-- A scrollable cursor: the planner adds Material above the node.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT a FROM limit_t LIMIT 5;
FETCH 2 FROM c;
FETCH BACKWARD 1 FROM c;
COMMIT;

-- A parallel worker runs the node from the plan's text form.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT a FROM limit_t WHERE a > 90 LIMIT 3;
SELECT a FROM limit_t WHERE a > 90 LIMIT 3;
RESET debug_parallel_query;

-- An empty child.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM limit_empty LIMIT 3;
SELECT a FROM limit_empty LIMIT 3;

SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a FROM limit_t LIMIT 3;
RESET tessera.enable;

\set VERBOSITY terse
SELECT a FROM limit_t LIMIT -1;
SELECT a FROM limit_t OFFSET -1;
\set VERBOSITY default

DROP TABLE limit_t, limit_empty;
DROP EXTENSION tessera;
