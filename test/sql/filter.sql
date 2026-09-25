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
-- Two columns in the chain: the other column is an operand of the step.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a + b > 200;
SELECT filter_same($$SELECT a, b FROM filter_t WHERE a + b > 200$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE b - a < -190$$);

-- A column the clauses need but the query does not: the scan below adds it.
EXPLAIN (VERBOSE, COSTS OFF) SELECT c FROM filter_t WHERE a > 195;
SELECT c FROM filter_t WHERE a > 195 ORDER BY c;
-- No column at all.
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE b > 5$$);
-- The node computes the projection itself.
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

-- Clauses from the first unsupported one on stay row-wise, in the
-- planner's order, over the rows the batch clauses kept.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND c = 'r150';
SELECT filter_same($$SELECT a FROM filter_t WHERE a > 100 AND c = 'r150'$$);
-- The planner orders the text comparison after the cheaper int4 one.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE c = 'r150' AND a > 100;
-- A null test costs nothing and comes first: the node is not offered.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND c IS NOT NULL;
-- An expensive clause moves behind a cheap batch one.
CREATE FUNCTION filter_slow(x int) RETURNS boolean
LANGUAGE plpgsql COST 1000 AS $$ BEGIN RETURN x % 2 = 0; END $$;
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE filter_slow(a) AND a > 190;
SELECT filter_same($$SELECT a FROM filter_t WHERE filter_slow(a) AND a > 190$$);
DROP FUNCTION filter_slow(int);
-- Rows removed by each part, and a column only the row-wise clause reads.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM filter_t WHERE a > 100 AND c <> 'r150';
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a > 100 AND c <> 'r150'$$);

-- The pack node keeps the scan's tuples: a batch clause deforms its column
-- for every row, the residual its column for the rows that survived.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM filter_t WHERE a > 100 AND c <> 'r150';
-- A column the query returns is deformed for the rows served.
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT b FROM filter_t WHERE a > 190;
-- A table with a dropped column is scanned with a projection: rows are copied.
CREATE TABLE filter_dropped (a int, x int, b int);
INSERT INTO filter_dropped SELECT i, i, i FROM generate_series(1, 100) AS i;
ALTER TABLE filter_dropped DROP COLUMN x;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM filter_dropped WHERE a > 90;
SELECT filter_same($$SELECT sum(b) FROM filter_dropped WHERE a > 90$$);
DROP TABLE filter_dropped;
-- A temporary table lives in local buffers.
CREATE TEMP TABLE filter_temp AS SELECT i AS a, 'r' || i AS c FROM generate_series(1, 100) AS i;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM filter_temp WHERE a > 90 AND c <> 'r95';
SELECT filter_same($$SELECT c FROM filter_temp WHERE a > 90 AND c <> 'r95'$$);
DROP TABLE filter_temp;

-- The scan reads pages itself: a page with more visible tuples than a batch
-- gives several batches, and dead tuples never reach a batch.
CREATE TABLE filter_dead AS SELECT i AS a FROM generate_series(1, 300) AS i;
DELETE FROM filter_dead WHERE a % 3 = 0;
BEGIN;
SAVEPOINT aborted;
INSERT INTO filter_dead SELECT i FROM generate_series(1000, 1099) AS i;
ROLLBACK TO aborted;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM filter_dead WHERE a > 0;
SELECT filter_same($$SELECT count(*), sum(a) FROM filter_dead WHERE a > 150$$);
COMMIT;
DROP TABLE filter_dead;
-- Values stored outside the page reach the residual and the parent.
CREATE TABLE filter_toast (a int, t text);
ALTER TABLE filter_toast ALTER COLUMN t SET STORAGE EXTERNAL;
INSERT INTO filter_toast SELECT i, repeat(chr(96 + i), 5000) FROM generate_series(1, 20) AS i;
SELECT filter_same($$SELECT a, length(t), left(t, 1) FROM filter_toast WHERE a > 15 AND t <> 'x'$$);
DROP TABLE filter_toast;
-- A column added with a default is missing from older tuples.
CREATE TABLE filter_missing AS SELECT i AS a FROM generate_series(1, 100) AS i;
ALTER TABLE filter_missing ADD COLUMN m int DEFAULT 5;
INSERT INTO filter_missing VALUES (101, 7);
SELECT filter_same($$SELECT sum(m), count(*) FROM filter_missing WHERE a > 90 AND m > 0$$);
DROP TABLE filter_missing;
-- A clause the planner folds away leaves no scan at all.
EXPLAIN (COSTS OFF) SELECT count(*) FROM filter_t WHERE a > 100 AND false;

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

-- A bigint column: the comparison of bigint with an integer constant and
-- with a bigint one, the commutator with the constant on the left, unary
-- minus, and the cast of an int4 column as a chain step. Two columns of
-- different widths stay a residual.
CREATE TABLE filter8_t (a bigint, b int);
INSERT INTO filter8_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i * 4294967296 + i END, i % 10
FROM generate_series(1, 200) AS i;
EXPLAIN (COSTS OFF) SELECT a FROM filter8_t WHERE a > 100;
SELECT filter_same($$SELECT a FROM filter8_t WHERE a > 4294967296 * 195$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE a < 100$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE 4294967296 * 195 < a$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE 100 < a AND a < 4294967296 * 3$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE -a < -(4294967296 * 195)$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE (a + 1) * 2 > 4294967296 * 390$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE a % 7 = 3$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE a / 4294967296 = 100$$);
EXPLAIN (COSTS OFF) SELECT a FROM filter8_t WHERE a + b > 4294967296 * 195;
SELECT filter_same($$SELECT a FROM filter8_t WHERE a + b > 4294967296 * 195$$);
-- Two columns of the table in batches: a column against a chain, NULLs in
-- either, int4 against int8.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > b * 20;
SELECT filter_same($$SELECT a, b FROM filter_t WHERE a > b * 20$$);
SELECT filter_same($$SELECT a, b FROM filter_t WHERE b = a$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE a < b$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE b * 4294967296 <= a$$);
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a::bigint * 3 > 570;
SELECT filter_same($$SELECT a FROM filter_t WHERE a::bigint * 3 > 570$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE a::bigint < 5 OR a::bigint > 195$$);
-- An integer column against a bigint constant beyond its range.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a < 5000000000;
SELECT filter_same($$SELECT a FROM filter_t WHERE a < 5000000000 AND a > -5000000000 AND a > 195$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE a > 5000000000 OR 5000000000 < a$$);
-- An overflow in a bigint chain is reported as bigint's.
SELECT a FROM filter8_t WHERE a * 4294967296 > 1;
DROP TABLE filter8_t;

DROP FUNCTION filter_same(text);

DROP TABLE filter_t;
DROP EXTENSION tessera;
