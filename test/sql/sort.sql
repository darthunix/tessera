CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_limit';

-- a: NULL in every 7th row and each value in several rows; b: an int8
-- past the int4 range; c: text; d: unique, in no order of the rows.
CREATE TABLE sort_t (a int, b bigint, c text, d int);
INSERT INTO sort_t
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i % 50 - 25 END,
       (i % 13 - 6)::bigint * 5000000000,
       CASE WHEN i % 11 = 0 THEN NULL ELSE 'r' || i END,
       i * 37 % 500
FROM generate_series(0, 499) AS i;
ANALYZE sort_t;

-- The rows of a query with Tessera on, in their order, once they equal
-- those with it off; every query orders its rows fully.
CREATE FUNCTION sort_same(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    with_tessera text[];
    without_tessera text[];
    wrapped text := format('SELECT array_agg(q::text) FROM (%s) AS q', query);
BEGIN
    PERFORM set_config('tessera.enable', 'on', true);
    EXECUTE wrapped INTO with_tessera;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE wrapped INTO without_tessera;
    IF with_tessera IS DISTINCT FROM without_tessera THEN
        RETURN format('MISMATCH on %s off %s', with_tessera, without_tessera);
    END IF;
    RETURN format('%s rows', coalesce(cardinality(with_tessera), 0));
END
$$;

-- EXPLAIN ANALYZE with the memory, which depends on the platform, masked.
CREATE FUNCTION sort_explain(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query
    LOOP
        RETURN NEXT regexp_replace(regexp_replace(line, 'Memory Usage: \d+', 'Memory Usage: N'),
                                   '(Disk Usage|Runs|Merge Passes): \d+', '\1: N');
    END LOOP;
END
$$;

-- Without the kernels module there is nothing to sort with: the core sorts.
EXPLAIN (COSTS OFF) SELECT d FROM sort_t ORDER BY d;

LOAD 'tessera_kernels';
EXPLAIN (COSTS OFF) SELECT d FROM sort_t ORDER BY d;
SELECT sort_explain($$SELECT d, c FROM sort_t ORDER BY d$$);

-- One key of each kind, both directions and both places of NULL; the
-- comparison reads the node's rows in order through a subquery.
EXPLAIN (COSTS OFF) SELECT array_agg(q::text) FROM (SELECT d FROM sort_t ORDER BY d) AS q;
SELECT sort_same($$SELECT d FROM sort_t ORDER BY d$$);
SELECT sort_same($$SELECT d, c FROM sort_t ORDER BY d DESC$$);
SELECT sort_same($$SELECT b, d FROM sort_t ORDER BY b, d$$);
SELECT sort_same($$SELECT b, d FROM sort_t ORDER BY b DESC, d DESC$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a, d$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a NULLS FIRST, d$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a DESC, d$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a DESC NULLS LAST, d DESC$$);
EXPLAIN (COSTS OFF) SELECT a, d FROM sort_t ORDER BY a DESC NULLS LAST, d DESC;
-- Three keys, an int8 among them; a key the query does not return.
SELECT sort_same($$SELECT a, b, c, d FROM sort_t ORDER BY b, a NULLS FIRST, d DESC$$);
SELECT sort_same($$SELECT c FROM sort_t ORDER BY d$$);
EXPLAIN (VERBOSE, COSTS OFF) SELECT c FROM sort_t ORDER BY d;
-- A key computed by the child.
SELECT sort_same($$SELECT d, a + d AS x FROM sort_t WHERE a > 0 ORDER BY x, d$$);
-- A text key stays with the core.
EXPLAIN (COSTS OFF) SELECT d FROM sort_t ORDER BY c, d;

-- Over a filter: no row, one, a batch, a batch and one, and all.
SELECT sort_same($$SELECT d FROM sort_t WHERE d < 0 ORDER BY d$$);
SELECT sort_same($$SELECT d FROM sort_t WHERE d < 1 ORDER BY d$$);
SELECT sort_same($$SELECT d, c FROM sort_t WHERE d < 64 ORDER BY d DESC$$);
SELECT sort_same($$SELECT d, c FROM sort_t WHERE d < 65 ORDER BY d DESC$$);
SELECT sort_same($$SELECT d, c FROM sort_t WHERE b > 0 ORDER BY d$$);
EXPLAIN (COSTS OFF) SELECT d FROM sort_t WHERE b > 0 ORDER BY d;
-- A nullable key over a filter: NULLs among the rows kept, and NULLs only
-- among the rows the filter removed.
SELECT sort_same($$SELECT a, d FROM sort_t WHERE d > 250 ORDER BY a NULLS FIRST, d$$);
SELECT sort_same($$SELECT a, d FROM sort_t WHERE a > -100 ORDER BY a DESC, d$$);

-- A batch-aware parent above: an offset, and an aggregate over a subquery.
EXPLAIN (COSTS OFF) SELECT d FROM sort_t ORDER BY d OFFSET 490;
SELECT d FROM sort_t ORDER BY d OFFSET 490;
SELECT sort_same($$SELECT d, c FROM sort_t ORDER BY d DESC OFFSET 17$$);
-- Top-N: a limit above sets the node a bound, and it keeps the best rows in
-- a heap; every query orders its rows fully, so that the rows returned do
-- not depend on which of equal rows a heap keeps.
EXPLAIN (COSTS OFF) SELECT d FROM sort_t ORDER BY d LIMIT 3;
SELECT sort_explain($$SELECT d, c FROM sort_t ORDER BY d DESC LIMIT 3$$);
SELECT sort_same($$SELECT d, c FROM sort_t ORDER BY d LIMIT 3$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a NULLS FIRST, d LIMIT 20$$);
SELECT sort_same($$SELECT a, d FROM sort_t ORDER BY a DESC, d DESC LIMIT 64$$);
SELECT sort_same($$SELECT a, b, d FROM sort_t ORDER BY b, a, d LIMIT 65$$);
SELECT sort_same($$SELECT d FROM sort_t WHERE b > 0 ORDER BY d DESC LIMIT 7$$);
-- No row, one, all of them and more; an offset before the rows.
SELECT sort_same($$SELECT d FROM sort_t ORDER BY d LIMIT 0$$);
SELECT sort_same($$SELECT d FROM sort_t ORDER BY d LIMIT 1$$);
SELECT sort_same($$SELECT d FROM sort_t ORDER BY d LIMIT 500$$);
SELECT sort_same($$SELECT d FROM sort_t ORDER BY d LIMIT 1000$$);
SELECT sort_same($$SELECT d, c FROM sort_t ORDER BY d DESC OFFSET 490 LIMIT 5$$);
SELECT sort_same($$SELECT d FROM sort_t WHERE d < 0 ORDER BY d LIMIT 5$$);
-- WITH TIES passes no bound: the core sorts.
EXPLAIN (COSTS OFF) SELECT a FROM sort_t ORDER BY a FETCH FIRST 3 ROWS WITH TIES;
SELECT sort_same($$SELECT a FROM sort_t ORDER BY a FETCH FIRST 3 ROWS WITH TIES$$);
-- A bound from a parameter, and the node read again for a larger one:
-- the outer value sets the limit of the inner query at every rescan.
SET plan_cache_mode = force_generic_plan;
PREPARE top(int) AS SELECT d FROM sort_t ORDER BY d DESC LIMIT $1;
EXECUTE top(2);
EXECUTE top(4);
DEALLOCATE top;
RESET plan_cache_mode;
SELECT sort_same($$
SELECT x, (SELECT array_agg(d) FROM (SELECT d FROM sort_t ORDER BY d LIMIT x) s)
FROM (VALUES (3), (1), (5), (0), (2)) AS v(x) ORDER BY x$$);
SELECT sort_explain($$
SELECT x, (SELECT array_agg(d) FROM (SELECT d FROM sort_t ORDER BY d LIMIT x) s)
FROM (VALUES (3), (1), (5)) AS v(x)$$);

-- A merge join of the core above: it marks and restores its inner side,
-- which a Material above the node does.
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF)
SELECT s.d, t.d FROM (SELECT d FROM sort_t ORDER BY d) s JOIN (SELECT a FROM sort_t ORDER BY a) t(d) ON s.d = t.d;
SELECT sort_same($$
SELECT s.d, t.d FROM (SELECT d FROM sort_t ORDER BY d) s JOIN (SELECT a FROM sort_t ORDER BY a) t(d) ON s.d = t.d
ORDER BY s.d, t.d$$);
RESET enable_hashjoin;
RESET enable_nestloop;

-- A join below: the rows of the join are packed.
SELECT sort_same($$SELECT s.d, t.d FROM sort_t s JOIN sort_t t ON s.a = t.d ORDER BY s.d, t.d$$);

-- Many rows: several chunks of records and of values.
CREATE TABLE sort_big AS
SELECT i * 7919 % 100000 AS k, repeat('x', i % 300) AS v FROM generate_series(0, 99999) AS i;
ANALYZE sort_big;
SET work_mem = '64MB';
SELECT sort_same($$SELECT k, v FROM sort_big ORDER BY k DESC$$);
SELECT sort_explain($$SELECT k, v FROM sort_big ORDER BY k$$);
-- Top-N over keys in the reverse of the rows' order: every row beats the
-- ones kept, and the records are rebuilt from the heap's as they pile up.
CREATE TABLE sort_seq AS SELECT i, 'r' || i AS t FROM generate_series(1, 200000) AS i;
ANALYZE sort_seq;
-- The node replaces a serial sort: no Gather Merge of the core.
SET max_parallel_workers_per_gather = 0;
SELECT sort_same($$SELECT i, t FROM sort_seq ORDER BY i DESC LIMIT 10$$);
SELECT sort_explain($$SELECT i, t FROM sort_seq ORDER BY i DESC LIMIT 10$$);
SELECT sort_same($$SELECT i, t FROM sort_seq ORDER BY i DESC LIMIT 50000$$);
RESET max_parallel_workers_per_gather;
DROP TABLE sort_seq;
-- Past work_mem: sorted runs on disk, merged. At 64 kB sort_big, text of
-- up to 300 bytes, takes many runs, more than a merge takes at once, so
-- that passes merge them into longer ones first.
SET work_mem = '64kB';
SET max_parallel_workers_per_gather = 0;
SELECT sort_explain($$SELECT k, v FROM sort_big ORDER BY k$$);
SELECT sort_same($$SELECT k, v FROM sort_big ORDER BY k$$);
SELECT sort_same($$SELECT k, v FROM sort_big ORDER BY k DESC$$);
-- Every class of key, both directions and places of NULL, several keys,
-- equal keys (the key alone returned, as ties come in any order).
CREATE TABLE sort_ext AS
SELECT CASE WHEN i % 7 = 0 THEN NULL ELSE i % 50 - 25 END AS a,
       (i % 13 - 6)::bigint * 5000000000 AS b,
       CASE WHEN i % 11 = 0 THEN NULL ELSE 'r' || i END AS c,
       i * 7919 % 60000 AS d
FROM generate_series(0, 59999) AS i;
ANALYZE sort_ext;
SELECT sort_same($$SELECT a, b, c, d FROM sort_ext ORDER BY a NULLS FIRST, b DESC, d$$);
SELECT sort_same($$SELECT a, d FROM sort_ext ORDER BY a DESC NULLS LAST, d DESC$$);
SELECT sort_same($$SELECT b, d FROM sort_ext ORDER BY b, d$$);
SELECT sort_same($$SELECT a FROM sort_ext ORDER BY a$$);
SELECT sort_same($$SELECT b FROM sort_ext ORDER BY b DESC$$);
SELECT sort_same($$SELECT d FROM sort_ext WHERE d < 0 ORDER BY d$$);
SELECT sort_same($$SELECT d, c FROM sort_ext WHERE d = 7 ORDER BY d$$);
-- Rescans: none of the child's parameters changes and the last merge
-- starts again over the runs; one does and the rows are sorted anew.
SELECT sort_same($$
SELECT x, s.k FROM generate_series(1, 3) AS x,
LATERAL (SELECT k FROM sort_big ORDER BY k DESC OFFSET 99995) AS s ORDER BY x, s.k$$);
SELECT sort_same($$
SELECT x, (SELECT array_agg(k) FROM (SELECT k FROM sort_big WHERE k % 1000 = x ORDER BY k DESC) s)
FROM generate_series(1, 3) AS x ORDER BY x$$);
-- A scrollable cursor: the last merge writes one run, read by blocks in
-- either direction. k is 0 to 99999 once each: row n holds n - 1.
BEGIN;
DECLARE e SCROLL CURSOR FOR SELECT k FROM sort_big ORDER BY k;
FETCH ABSOLUTE 50000 FROM e;
FETCH BACKWARD 3 FROM e;
FETCH LAST FROM e;
FETCH PRIOR FROM e;
FETCH FIRST FROM e;
FETCH ABSOLUTE 65 FROM e;
FETCH BACKWARD 2 FROM e;
MOVE LAST IN e;
FETCH NEXT FROM e;
FETCH BACKWARD 2 FROM e;
CLOSE e;
COMMIT;
RESET max_parallel_workers_per_gather;
DROP TABLE sort_ext;
RESET work_mem;

-- A scrollable cursor: forward and backward, across batches and past both ends.
EXPLAIN (COSTS OFF) DECLARE c SCROLL CURSOR FOR SELECT d, a FROM sort_t ORDER BY d;
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT d, a FROM sort_t ORDER BY d;
FETCH 3 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH BACKWARD 2 FROM c;
FETCH ABSOLUTE 100 FROM c;
FETCH BACKWARD 70 FROM c;
FETCH LAST FROM c;
FETCH NEXT FROM c;
FETCH PRIOR FROM c;
FETCH ABSOLUTE 64 FROM c;
FETCH ABSOLUTE 65 FROM c;
FETCH ABSOLUTE 63 FROM c;
FETCH FIRST FROM c;
FETCH BACKWARD 1 FROM c;
FETCH FORWARD 1 FROM c;
-- A cursor opened backward: the rows are read before the first fetch.
MOVE LAST IN c;
FETCH BACKWARD 3 FROM c;
CLOSE c;
DECLARE b SCROLL CURSOR FOR SELECT d FROM sort_t ORDER BY d DESC;
FETCH BACKWARD 1 FROM b;
FETCH ABSOLUTE -2 FROM b;
CLOSE b;
COMMIT;

-- Rescans: a parameter of the child changes and the rows are sorted anew;
-- none does and the sorted rows come back.
SELECT sort_same($$
SELECT x, (SELECT array_agg(d) FROM (SELECT d FROM sort_t WHERE a = x ORDER BY d DESC) s)
FROM generate_series(-3, 3) AS x ORDER BY x$$);
EXPLAIN (COSTS OFF)
SELECT x, (SELECT array_agg(d) FROM (SELECT d FROM sort_t WHERE a = x ORDER BY d DESC) s)
FROM generate_series(-3, 3) AS x;
SELECT sort_same($$
SELECT x, s.d FROM generate_series(1, 3) AS x,
LATERAL (SELECT d FROM sort_t WHERE d > 495 ORDER BY d DESC OFFSET 0) AS s ORDER BY x, s.d$$);
-- The outer value reaches only the aggregate above the sort: the sorted
-- rows are read once and returned again on every rescan.
SELECT sort_same($$
SELECT x, (SELECT array_agg(d + x) FROM (SELECT d FROM sort_t WHERE d > 495 ORDER BY d DESC) s)
FROM generate_series(1, 3) AS x ORDER BY x$$);
SELECT sort_explain($$
SELECT x, (SELECT array_agg(d + x) FROM (SELECT d FROM sort_t WHERE d > 495 ORDER BY d DESC) s)
FROM generate_series(1, 3) AS x$$);

-- A generic plan with a parameter of the child.
SET plan_cache_mode = force_generic_plan;
PREPARE p(int) AS SELECT d FROM sort_t WHERE a > $1 ORDER BY d DESC;
EXPLAIN (COSTS OFF) EXECUTE p(20);
EXECUTE p(20);
EXECUTE p(23);
DEALLOCATE p;
RESET plan_cache_mode;

DROP FUNCTION sort_same(text);
DROP FUNCTION sort_explain(text);
DROP TABLE sort_big;
DROP TABLE sort_t;
DROP EXTENSION tessera;
