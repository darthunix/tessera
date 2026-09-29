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

-- Without the kernels module the registry knows no predicate: no batch
-- filter, and the node runs the clause row by row over the native scan.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100;

LOAD 'tessera_kernels';
-- The clauses move from the scan to the node.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND b < 5;
SELECT filter_same($$SELECT count(*), sum(a) FROM filter_t WHERE a > 100 AND b < 5$$);
-- The node's path costs tessera.scan_cost_factor of the core's scan: past
-- 1 the core's scan is the cheaper.
SET tessera.scan_cost_factor = 1.5;
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND b < 5;
RESET tessera.scan_cost_factor;

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

-- Each clause runs in batches if the compiler takes it, else row-wise,
-- in the planner's order, over the rows the clauses before it kept.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a > 100 AND c = 'r150';
SELECT filter_same($$SELECT a FROM filter_t WHERE a > 100 AND c = 'r150'$$);
-- The planner orders the text comparison after the cheaper int4 one.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE c = 'r150' AND a > 100;
-- A null test costs nothing and comes first; it runs in batches too.
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
-- A batch clause after a row-wise one: the guard, which the compiler does
-- not take, still runs before the division.
EXPLAIN (COSTS OFF) SELECT count(*) FROM filter_t WHERE a > 0 AND b IS DISTINCT FROM 0 AND 10 / b > 1;
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a > 0 AND b IS DISTINCT FROM 0 AND 10 / b > 1$$);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a FROM filter_t WHERE a > 100 AND c <> 'r150' AND b < 5;
SELECT filter_same($$SELECT a FROM filter_t WHERE a > 100 AND c <> 'r150' AND b < 5$$);
-- A policy's clauses come before the user's, whatever their costs: the
-- cheaper division waits for the policy's row-wise guard.
CREATE TABLE filter_rls AS SELECT i AS a, i % 10 AS b FROM generate_series(1, 200) AS i;
ALTER TABLE filter_rls ENABLE ROW LEVEL SECURITY;
CREATE POLICY filter_rls_visible ON filter_rls USING (a > 0 AND b * 1 + 0 IS DISTINCT FROM 0);
CREATE ROLE regress_tessera_rls;
GRANT SELECT ON filter_rls TO regress_tessera_rls;
SET ROLE regress_tessera_rls;
EXPLAIN (COSTS OFF) SELECT count(*) FROM filter_rls WHERE 10 / b > 1;
SELECT filter_same($$SELECT count(*), sum(a) FROM filter_rls WHERE 10 / b > 1$$);
RESET ROLE;
DROP TABLE filter_rls;
DROP ROLE regress_tessera_rls;

-- Conditions of several parts in batches: OR, AND inside it, null tests,
-- boolean tests and short IN lists, in three-valued logic; the right side
-- of an OR runs only where the left one is not true.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a < 10 OR b = 3;
SELECT filter_same($$SELECT a, b FROM filter_t WHERE a < 10 OR b = 3$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a IS NULL OR (a > 190 AND b <> 3)$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE (a > 100) IS NOT TRUE$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE (a > 100) IS UNKNOWN$$);
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a IN (3, 5, 7, 14);
SELECT filter_same($$SELECT a FROM filter_t WHERE a IN (3, 5, 7, 14)$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a NOT IN (3, NULL)$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a + 1 IN (4, NULL) OR b IN (1, 2)$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE b = 0 OR 10 / b > 1$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE (b IN (1, NULL)) IS UNKNOWN$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE a IN (3::bigint, 5000000000)$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE c IS NOT NULL AND a + 1 IS NULL$$);
-- Conditional values: CASE, a simple CASE, COALESCE and NULLIF, a branch
-- computed only over the rows that take it.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE CASE WHEN b > 5 THEN a ELSE -a END > 50;
SELECT filter_same($$SELECT a FROM filter_t WHERE CASE WHEN b > 5 THEN a ELSE -a END > 50$$);
SELECT filter_same($$SELECT sum(CASE WHEN a % 3 = 0 THEN b ELSE 0 END), count(*) FROM filter_t WHERE a > 0$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE CASE WHEN b <> 0 THEN a / b ELSE 0 END > 20$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE COALESCE(a, b) > 5$$);
SELECT filter_same($$SELECT NULLIF(b, 0), a FROM filter_t WHERE a > 150$$);
SELECT filter_same($$SELECT CASE b WHEN 1 THEN 'one' WHEN 2 THEN c END, a FROM filter_t WHERE a > 180$$);
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
-- different widths run in batches, the int4 one cast.
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
-- Two computed sides, the second an expression of its own over the batch;
-- a guard before a division inside it; a sum over both.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE (a + 1) * (b + 2) > 300;
SELECT filter_same($$SELECT a, b FROM filter_t WHERE (a + 1) * (b + 2) > 300$$);
SELECT filter_same($$SELECT a, b FROM filter_t WHERE a + 1 > b * 2$$);
SELECT filter_same($$SELECT count(*) FROM filter_t WHERE a > 0 AND b IS DISTINCT FROM 0 AND a + 1 > 10 / b * 2$$);
SELECT filter_same($$SELECT sum((a + 1) * (b + 2)), count(*) FROM filter_t WHERE a > 100$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE a + 1 > b::bigint * 4294967296 * 20$$);
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a::bigint * 3 > 570;
SELECT filter_same($$SELECT a FROM filter_t WHERE a::bigint * 3 > 570$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE a::bigint < 5 OR a::bigint > 195$$);
-- An integer column against a bigint constant beyond its range.
EXPLAIN (COSTS OFF) SELECT a FROM filter_t WHERE a < 5000000000;
SELECT filter_same($$SELECT a FROM filter_t WHERE a < 5000000000 AND a > -5000000000 AND a > 195$$);
SELECT filter_same($$SELECT a FROM filter_t WHERE a > 5000000000 OR 5000000000 < a$$);
-- Functions over an int4 and an int8 in any shape: the int4 side is cast
-- to int8 as a step of its own; the errors are int8's.
EXPLAIN (COSTS OFF) SELECT a FROM filter8_t WHERE b + a > 4294967296 * 100;
SELECT filter_same($$SELECT b + a, a - b, b * a / 7 FROM filter8_t WHERE b + a > 4294967296 * 100$$);
SELECT filter_same($$SELECT a / b FROM filter8_t WHERE b <> 0 AND a / b > 4294967296 * 20$$);
SELECT filter_same($$SELECT a FROM filter8_t WHERE b * 4294967296 * 10 < a - b$$);
\set VERBOSITY terse
SELECT count(*) FROM filter8_t WHERE a / b > 0;
SELECT count(*) FROM filter8_t WHERE b * a * 4294967296 > 0;
\set VERBOSITY default
-- The explicit cast of a bigint to an integer, and its error past the range.
EXPLAIN (COSTS OFF) SELECT a FROM filter8_t WHERE (a % 4294967296)::int > 100;
SELECT filter_same($$SELECT (a % 4294967296)::int + 1 FROM filter8_t WHERE (a % 4294967296)::int > 100$$);
\set VERBOSITY terse
SELECT count(*) FROM filter8_t WHERE a::int > 0;
\set VERBOSITY default
SELECT filter_same($$SELECT a FROM filter8_t WHERE a IN (1, 2, 4294967297, 8589934594)$$);
-- An overflow in a bigint chain is reported as bigint's.
SELECT a FROM filter8_t WHERE a * 4294967296 > 1;
DROP TABLE filter8_t;

-- A table without clauses under a row-wise parent: the scan serves the
-- rows of each batch, the columns of its targets taken once per batch.
EXPLAIN (COSTS OFF) SELECT bit_or(a), max(c) FROM filter_t;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT bit_or(a), max(c) FROM filter_t;
SELECT filter_same($$SELECT bit_or(a), max(c), count(a), count(*) FROM filter_t$$);
-- Rows to the client, targets computed, NULL, a text column and a sort.
SELECT filter_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT a, b, c FROM filter_t) AS q$$);
SELECT filter_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT a % 7, c || '!', b * 2 FROM filter_t) AS q$$);
EXPLAIN (COSTS OFF) SELECT c FROM filter_t ORDER BY c DESC;
SELECT c FROM filter_t ORDER BY c DESC LIMIT 3;
-- A window function over the rows.
SELECT filter_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT a, row_number() OVER (ORDER BY c) FROM filter_t) AS q$$);
-- A cursor reads a few rows, then the rest.
BEGIN;
DECLARE filter_cursor CURSOR FOR SELECT a, c FROM filter_t;
FETCH 3 FROM filter_cursor;
FETCH 3 FROM filter_cursor;
MOVE 60 IN filter_cursor;
FETCH 3 FROM filter_cursor;
MOVE ALL IN filter_cursor;
FETCH 1 FROM filter_cursor;
COMMIT;
-- Rescanned per outer row, a parameter among the targets.
SELECT filter_same($$SELECT g, (SELECT bit_xor(a * g) FROM filter_t) FROM generate_series(1, 5) AS g$$);
-- Several pages, dead tuples and values stored outside the page.
CREATE TABLE filter_rows AS SELECT i AS a, repeat('x', i % 50) AS t FROM generate_series(1, 3000) AS i;
DELETE FROM filter_rows WHERE a % 4 = 0;
INSERT INTO filter_rows SELECT i, repeat(chr(96 + i % 26), 5000) FROM generate_series(3001, 3010) AS i;
SELECT filter_same($$SELECT bit_xor(a), max(length(t)), count(*) FROM filter_rows$$);
SELECT filter_same($$SELECT md5(string_agg(q::text, ',' ORDER BY q::text)) FROM (SELECT a, left(t, 3) FROM filter_rows) AS q$$);
DROP TABLE filter_rows;

DROP FUNCTION filter_same(text);

DROP TABLE filter_t;
DROP EXTENSION tessera;
