CREATE EXTENSION tessera;
LOAD 'tessera_nodes';

\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set pack_test :libdir '/tessera_pack_test' :dlsuffix
LOAD :'pack_test';

CREATE FUNCTION tessera_test_pack_paths()
RETURNS boolean
AS :'pack_test', 'tessera_test_pack_paths'
LANGUAGE C STRICT;

SELECT tessera_test_pack_paths() AS paths \gset
\echo :paths

-- The sink, a stand-in for a batch-aware parent, wraps the scan of every
-- pack_* table in the pack node and returns the rows of the batches.
CREATE TABLE pack_t (a int, b text);
INSERT INTO pack_t
SELECT i, CASE WHEN i % 10 = 0 THEN NULL ELSE 'row' || i END
FROM generate_series(1, 100) AS i;
CREATE TABLE pack_empty (a int);
EXPLAIN (COSTS OFF) SELECT a, b FROM pack_t WHERE a > 90;
SELECT a, b FROM pack_t WHERE a > 90;
SELECT count(*), sum(a), count(b) FROM pack_t;

-- Batches follow the parent's request, at most 64 rows each.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_t;
SET pack_test.batch_rows = 10;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_t;
SET pack_test.batch_rows = 200;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_t;
SET pack_test.batch_rows = 7;
SELECT count(*), sum(a) FROM pack_t;
RESET pack_test.batch_rows;

-- An empty child publishes nothing.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_empty;

-- A limit above stops after the first batch.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_t LIMIT 3;
SELECT a FROM pack_t LIMIT 3;

-- Rescan: a correlated subplan runs the node once per outer row.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, (SELECT count(*) FROM pack_t AS p WHERE p.a <= g) AS n
FROM generate_series(1, 3) AS g;
SELECT g, (SELECT count(*) FROM pack_t AS p WHERE p.a <= g) AS n
FROM generate_series(1, 3) AS g;

-- A scrollable cursor: the planner adds Material, the node never scans
-- backward.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT a FROM pack_t;
FETCH 2 FROM c;
FETCH BACKWARD 1 FROM c;
COMMIT;

-- A parallel worker runs the node from the plan's text form.
SET debug_parallel_query = on;
EXPLAIN (COSTS OFF) SELECT a FROM pack_t WHERE a > 98;
SELECT count(*), sum(a) FROM pack_t;
RESET debug_parallel_query;

SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT a FROM pack_t;
RESET tessera.enable;

-- A trim node between the sink and the pack node keeps the rows whose
-- first column is at most the setting: the unary helper forwards the
-- merged request and skips a batch left without rows.
SET pack_test.trim = 50;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(a) FROM pack_t;
SELECT count(a), sum(a) FROM pack_t;
SELECT a, b FROM pack_t WHERE a > 45;
SET pack_test.trim = 0;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM pack_t;
-- The trim node stops after its first batch with rows.
SET pack_test.trim = 80;
SET pack_test.stop = on;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(a) FROM pack_t;
SELECT count(a), max(a) FROM pack_t;
RESET pack_test.stop;
-- The smaller batch limit of the sink and the trim node reaches the pack.
SET pack_test.trim = 50;
SET pack_test.batch_rows = 10;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a, b FROM pack_t WHERE a > 45;
RESET pack_test.batch_rows;
RESET pack_test.trim;

-- A parent that asks for rows cannot stand above the pack node.
SET pack_test.rows_mode = on;
\set VERBOSITY terse
SELECT count(*) FROM pack_t;
\set VERBOSITY default
RESET pack_test.rows_mode;

DROP TABLE pack_t, pack_empty;
DROP FUNCTION tessera_test_pack_paths();
DROP EXTENSION tessera;
