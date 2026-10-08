CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

-- The same result with Tessera on and off, as text.
CREATE FUNCTION union_same(query text) RETURNS text
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

CREATE TABLE union_a (a int, b bigint, t text);
INSERT INTO union_a
SELECT i, i * 10, CASE WHEN i % 7 = 0 THEN NULL ELSE 'a' || i END
FROM generate_series(1, 1000) AS i;
CREATE TABLE union_b (a int, b bigint, t text);
INSERT INTO union_b
SELECT CASE WHEN i % 5 = 0 THEN NULL ELSE i END, i * 100, 'b' || i
FROM generate_series(1, 700) AS i;
CREATE TABLE union_empty (a int, b bigint, t text);
ANALYZE union_a, union_b, union_empty;
SET max_parallel_workers_per_gather = 0;

-- An aggregate over UNION ALL of two filtered scans: TessAppend reads the
-- branches in turn and gives the aggregate their batches, each branch's
-- through the pack that forwards the batches of its subquery.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                              UNION ALL SELECT a FROM union_b WHERE a < 600) AS s;
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                              UNION ALL SELECT a FROM union_b WHERE a < 600) AS s;
SELECT union_same($$SELECT count(*), sum(a), count(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 600) AS s$$);
-- The columns in another order in each branch, one of them unused.
SELECT union_same($$SELECT count(*), sum(x), sum(y) FROM (SELECT a AS x, b AS y, t FROM union_a
                   UNION ALL SELECT a, b, t FROM union_b WHERE b > 1000) AS s$$);
SELECT union_same($$SELECT sum(y), min(x) FROM (SELECT b AS y, a AS x FROM union_a WHERE a < 50
                   UNION ALL SELECT b, a FROM union_b WHERE a > 650) AS s$$);

-- Five branches: an empty table, a branch without rows, a constant
-- target, a branch that is a plain scan.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), sum(a), sum(k) FROM (
    SELECT a, 1 AS k FROM union_a WHERE a > 900
    UNION ALL SELECT a, 2 FROM union_empty
    UNION ALL SELECT a, 3 FROM union_b WHERE a > 5000
    UNION ALL SELECT a, 4 FROM union_b
    UNION ALL SELECT a, 5 FROM union_a WHERE a < 10) AS s;
SELECT union_same($$SELECT count(*), sum(a), sum(k) FROM (
    SELECT a, 1 AS k FROM union_a WHERE a > 900
    UNION ALL SELECT a, 2 FROM union_empty
    UNION ALL SELECT a, 3 FROM union_b WHERE a > 5000
    UNION ALL SELECT a, 4 FROM union_b
    UNION ALL SELECT a, 5 FROM union_a WHERE a < 10) AS s$$);
-- Every branch empty.
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 5000
                              UNION ALL SELECT a FROM union_empty) AS s;
-- Nested UNION ALL, flattened into one Append.
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 500
    UNION ALL (SELECT a FROM union_b WHERE a > 500 UNION ALL SELECT a FROM union_a WHERE a < 20)) AS s$$);
-- Branches of int4 and int8: a set operation the planner does not
-- flatten, whose Append stays the core's.
EXPLAIN (COSTS OFF)
SELECT count(*), sum(x) FROM (SELECT a AS x FROM union_a WHERE a > 100
                              UNION ALL SELECT b FROM union_b WHERE b > 100) AS s;
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT a AS x FROM union_a WHERE a > 100
                   UNION ALL SELECT b FROM union_b WHERE b > 100) AS s$$);
-- Clauses that run row by row: TessFilter in each branch, forwarded.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*) FROM (SELECT a FROM union_a WHERE upper(t) LIKE 'A1%'
                      UNION ALL SELECT a FROM union_b WHERE upper(t) LIKE 'B1%') AS s;

-- Rows to a row-wise parent: the core's Append stays.
EXPLAIN (COSTS OFF)
SELECT a FROM union_a WHERE a > 995 UNION ALL SELECT a FROM union_b WHERE a > 695;
SELECT union_same($$SELECT a, t FROM union_a WHERE a > 995 UNION ALL SELECT a, t FROM union_b WHERE a > 695$$);

-- A hash join whose outer side is UNION ALL, text and NULL keys included.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), sum(s.a), count(s.t) FROM (SELECT a, t FROM union_a WHERE a > 300
                                             UNION ALL SELECT a, t FROM union_b) AS s
JOIN union_b AS d ON d.a = s.a;
SELECT union_same($$SELECT count(*), sum(s.a), count(s.t) FROM (SELECT a, t FROM union_a WHERE a > 300
                   UNION ALL SELECT a, t FROM union_b) AS s JOIN union_b AS d ON d.a = s.a$$);
-- A sort over UNION ALL, a text column carried along.
SELECT union_same($$SELECT a, t FROM (SELECT a, t FROM union_a WHERE a > 900
                   UNION ALL SELECT a, t FROM union_b WHERE a > 600) AS s ORDER BY a, t$$);

-- A limit above: the bound reaches every branch, a top-N sort in each.
EXPLAIN (VERBOSE, ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM (SELECT a FROM (SELECT a FROM union_a ORDER BY a DESC LIMIT 100) AS x
                      UNION ALL SELECT a FROM (SELECT a FROM union_b ORDER BY a LIMIT 100) AS y
                      LIMIT 3) AS s;
SELECT union_same($$SELECT a FROM (SELECT a FROM (SELECT a FROM union_a ORDER BY a DESC LIMIT 100) AS x
                   UNION ALL SELECT a FROM (SELECT a FROM union_b WHERE a IS NOT NULL ORDER BY a LIMIT 100) AS y
                   LIMIT 150) AS s$$);

-- Rescan: a correlated subquery rescans the branches with a new value,
-- and an initplan's parameter reaches both.
SELECT g, (SELECT count(*) FROM (SELECT a FROM union_a WHERE a > g
                                 UNION ALL SELECT a FROM union_b WHERE a > g) AS s) AS n
FROM generate_series(595, 1005, 100) AS g;
SELECT union_same($$SELECT g, (SELECT sum(a) FROM (SELECT a FROM union_a WHERE a > g
                   UNION ALL SELECT a FROM union_b WHERE a > g) AS s) FROM generate_series(0, 1000, 50) AS g$$);
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > (SELECT 990)
                   UNION ALL SELECT a FROM union_b WHERE a > (SELECT 690)) AS s$$);

-- Inheritance: the parent's rows and a child's, each read by TessFilter.
CREATE TABLE union_parent (a int, b int);
CREATE TABLE union_child (c int) INHERITS (union_parent);
INSERT INTO union_parent SELECT i, i % 10 FROM generate_series(1, 500) AS i;
INSERT INTO union_child SELECT i, i % 10, i FROM generate_series(501, 1200) AS i;
ANALYZE union_parent, union_child;
EXPLAIN (COSTS OFF) SELECT count(*), sum(a) FROM union_parent WHERE b < 3;
SELECT union_same($$SELECT count(*), sum(a) FROM union_parent WHERE b < 3$$);
SELECT union_same($$SELECT count(*), sum(a) FROM union_parent$$);

-- Range partitions, one of them partitioned again.
CREATE TABLE union_part (k int, v int) PARTITION BY RANGE (k);
CREATE TABLE union_part_1 PARTITION OF union_part FOR VALUES FROM (1) TO (1001);
CREATE TABLE union_part_2 PARTITION OF union_part FOR VALUES FROM (1001) TO (2001);
CREATE TABLE union_part_3 PARTITION OF union_part FOR VALUES FROM (2001) TO (4001)
    PARTITION BY RANGE (k);
CREATE TABLE union_part_3a PARTITION OF union_part_3 FOR VALUES FROM (2001) TO (3001);
CREATE TABLE union_part_3b PARTITION OF union_part_3 FOR VALUES FROM (3001) TO (4001);
INSERT INTO union_part SELECT i, i % 100 FROM generate_series(1, 4000) AS i;
ANALYZE union_part;
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM union_part WHERE v < 10;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE v < 10$$);
SELECT union_same($$SELECT count(*), sum(v) FROM union_part$$);
-- Pruned while planning: two partitions left.
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM union_part WHERE k > 2500 AND v < 10;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > 2500 AND v < 10$$);
-- One partition left: no Append.
EXPLAIN (COSTS OFF) SELECT count(*) FROM union_part WHERE k < 500 AND v < 10;
-- A parameter in a clause of another column prunes nothing.
EXPLAIN (COSTS OFF)
SELECT g, (SELECT count(*) FROM union_part WHERE v < g) FROM generate_series(1, 3) AS g;
SELECT union_same($$SELECT g, (SELECT count(*) FROM union_part WHERE v < g) FROM generate_series(1, 5) AS g$$);
-- Pruned while executing, as the core's Append prunes: the node shows
-- which children it read.
CREATE FUNCTION union_run(query text) RETURNS SETOF text
LANGUAGE plpgsql AS $$
DECLARE
    line text;
BEGIN
    FOR line IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || query LOOP
        IF line ~ 'Append|Subplans Removed|TessHeapScan|^ *(InitPlan|SubPlan) ' THEN
            RETURN NEXT regexp_replace(line, '\(actual rows=[0-9.]+ loops=([0-9]+)\)', '(loops=\1)');
        END IF;
    END LOOP;
END
$$;
-- At the start, by a generic plan's parameter: the children it prunes are
-- not even started, all of them for a bound past every partition, the
-- sub-partitioned one's for a bound within it.
-- A statement is planned when it first runs, not when it is prepared:
-- the core's runs with Tessera off, and its generic plan is the core's.
PREPARE union_prune(int) AS SELECT count(*), sum(k) FROM union_part WHERE k > $1 AND v < 10;
PREPARE union_prune_core(int) AS SELECT count(*), sum(k) FROM union_part WHERE k > $1 AND v < 10;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE union_prune(2500);
EXECUTE union_prune(2500);
SET tessera.enable = off;
EXPLAIN (COSTS OFF) EXECUTE union_prune_core(2500);
EXECUTE union_prune_core(2500);
SET tessera.enable = on;
EXPLAIN (COSTS OFF) EXECUTE union_prune(3500);
EXECUTE union_prune(3500);
SET tessera.enable = off;
EXECUTE union_prune_core(3500);
SET tessera.enable = on;
EXPLAIN (COSTS OFF) EXECUTE union_prune(5000);
EXECUTE union_prune(5000);
EXECUTE union_prune(0);
SET tessera.enable = off;
EXECUTE union_prune_core(0);
SET tessera.enable = on;
-- Planned without the values, nothing is pruned.
EXPLAIN (GENERIC_PLAN, COSTS OFF) SELECT count(*) FROM union_part WHERE k > $1 AND v < 10;
-- In a format other than text the line is shown when nothing is
-- removed too, as for the core's Append.
CREATE FUNCTION union_removed(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE 'EXPLAIN (FORMAT JSON, COSTS OFF) ' || query INTO plan;
    RETURN (SELECT coalesce(node ->> 'Custom Plan Provider', node ->> 'Node Type') || ': ' ||
                   coalesce(node ->> 'Subplans Removed', 'none')
            FROM jsonb_path_query_first(plan,
                '$[0]."Plan".** ? (@."Node Type" == "Append" || @."Custom Plan Provider" == "TessAppend")') AS node);
END $$;
SELECT union_removed($$EXECUTE union_prune(0)$$) AS tessera,
       union_removed($$EXECUTE union_prune_core(0)$$) AS core;
RESET plan_cache_mode;
DEALLOCATE union_prune;
DEALLOCATE union_prune_core;
-- At the start, by a stable function's value.
SET union_test.bound = '2500';
EXPLAIN (COSTS OFF)
SELECT count(*), sum(k) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10$$);
-- At the first read, by an InitPlan's value.
SELECT union_run($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$);
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$);
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 9000) AND v < 10$$);
-- Anew for every value of a correlated subquery's parameter, as it
-- rescans the node: each partition read for the values that reach it.
SELECT union_run($$SELECT g, (SELECT count(*) FROM union_part WHERE k > g * 1000 AND v < 10) FROM generate_series(0, 4) AS g$$);
SELECT union_same($$SELECT g, (SELECT count(*) FROM union_part WHERE k > g * 1000 AND v < 10) FROM generate_series(0, 4) AS g$$);
-- UNION ALL of two partitioned tables, two hierarchies pruned.
SELECT union_run($$SELECT count(*), sum(k) FROM (SELECT k, v FROM union_part UNION ALL SELECT k, v FROM union_part) AS u
                   WHERE k > (SELECT 3500) AND v < 10$$);
SELECT union_same($$SELECT count(*), sum(k) FROM (SELECT k, v FROM union_part UNION ALL SELECT k, v FROM union_part) AS u
                    WHERE k > (SELECT 3500) AND v < 10$$);
-- In a subquery of its own, whose range table the statement's offsets:
-- a sublink's InitPlan and a materialized CTE.
SELECT union_run($$SELECT x FROM generate_series(1, 3) AS x
                   WHERE x * 10 < (SELECT count(*) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10)$$);
SELECT union_same($$SELECT x FROM generate_series(1, 30) AS x
                    WHERE x * 10 < (SELECT count(*) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10)$$);
SELECT union_same($$WITH c AS MATERIALIZED (SELECT k, v FROM union_part WHERE k > (SELECT 1500) AND v < 20)
                    SELECT count(*), sum(k) FROM c JOIN union_a ON c.k = union_a.a$$);
-- The partitions grouped: TessAgg groups over TessAppend.
SELECT union_same($$SELECT v % 7 AS g, count(*), sum(k) FROM union_part WHERE v < 50 GROUP BY 1$$);

-- Parallel: in place of the core's Parallel Append, TessAppend shares the
-- children out among the participants, a partial child to any of them.
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM union_part WHERE v < 10;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE v < 10$$);
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 600) AS s$$);
SELECT union_same($$SELECT v % 7 AS g, count(*), sum(k) FROM union_part WHERE v < 50 GROUP BY 1$$);
-- Pruned while executing: at the start by a stable function, then by an
-- InitPlan; every participant finds the valid children and finishes the
-- others for all.
SELECT regexp_replace(line, 'loops=[0-9]+', 'loops=n')
FROM union_run($$SELECT count(*), sum(k) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10$$) AS line;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > current_setting('union_test.bound')::int AND v < 10$$);
SELECT regexp_replace(line, 'loops=[0-9]+', 'loops=n')
FROM union_run($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$) AS line;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$);
SELECT union_same($$SELECT count(*), sum(k) FROM (SELECT k, v FROM union_part UNION ALL SELECT k, v FROM union_part) AS u
                    WHERE k > (SELECT 3500) AND v < 10$$);
-- A child that is not partial, a table no worker may read, goes to one
-- participant.
ALTER TABLE union_b SET (parallel_workers = 0);
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                              UNION ALL SELECT a FROM union_b WHERE a < 600) AS s;
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 600) AS s$$);
-- Three of them and a partial one: each is read once, whoever takes it.
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 200
                   UNION ALL SELECT a FROM union_b WHERE a >= 200 AND a < 400
                   UNION ALL SELECT a FROM union_b WHERE a >= 400) AS s$$);
-- A worker takes the child that is not partial, the first, while the
-- leader, which starts from the last, reads the partial one: done with
-- that, the leader must find the other taken. Sleeps set the order: the
-- partial child's last page keeps the leader 0.2 s, the other child a
-- worker 0.5 s. Read twice, the other child's 16 rows would add 160 to
-- the sum.
CREATE FUNCTION union_slow(v int, seconds float8) RETURNS int
LANGUAGE plpgsql PARALLEL SAFE AS $$ BEGIN PERFORM pg_sleep(seconds); RETURN v; END $$;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 980 AND union_slow(a, 0.01) > 0
                              UNION ALL SELECT a FROM union_b WHERE a < 20 AND union_slow(a, 0.03) > 0) AS s;
SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 980 AND union_slow(a, 0.01) > 0
                              UNION ALL SELECT a FROM union_b WHERE a < 20 AND union_slow(a, 0.03) > 0) AS s;
-- Without the leader.
SET parallel_leader_participation = off;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE v < 10$$);
SELECT regexp_replace(line, 'loops=[0-9]+', 'loops=n')
FROM union_run($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$) AS line;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE k > (SELECT 2500) AND v < 10$$);
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 600) AS s$$);
RESET parallel_leader_participation;
ALTER TABLE union_b RESET (parallel_workers);
-- Rescanned under the gather in a join: the children are shared out anew.
SET enable_material = off;
EXPLAIN (COSTS OFF)
SELECT x, n FROM (SELECT count(*) AS n FROM union_part WHERE v < 10) AS ss
RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true;
SELECT union_same($$SELECT x, n, s FROM (SELECT count(*) AS n, sum(k) AS s FROM union_part WHERE v < 10) AS ss
RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
-- Pruned by an InitPlan: the leader finds the valid children anew.
SELECT regexp_replace(line, 'loops=[0-9]+', 'loops=n')
FROM union_run($$SELECT x, n FROM (SELECT count(*) AS n FROM union_part WHERE k > (SELECT 2500) AND v < 10) AS ss
                 RIGHT JOIN (VALUES (1), (2)) AS v(x) ON true$$) AS line;
SELECT union_same($$SELECT x, n, s FROM (SELECT count(*) AS n, sum(k) AS s FROM union_part WHERE k > (SELECT 2500) AND v < 10) AS ss
RIGHT JOIN (VALUES (1), (2), (3)) AS v(x) ON true$$);
RESET enable_material;
SET enable_parallel_append = off;
EXPLAIN (COSTS OFF) SELECT count(*), sum(k) FROM union_part WHERE v < 10;
SELECT union_same($$SELECT count(*), sum(k) FROM union_part WHERE v < 10$$);
SELECT union_same($$SELECT count(*), sum(a) FROM (SELECT a FROM union_a WHERE a > 100
                   UNION ALL SELECT a FROM union_b WHERE a < 600) AS s$$);
RESET enable_parallel_append;
RESET max_parallel_workers_per_gather;
RESET parallel_setup_cost;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
RESET parallel_tuple_cost;
RESET min_parallel_table_scan_size;

-- UNION without ALL: TessAgg groups the branches' rows by every column
-- over TessAppend, in place of the core's HashAggregate over its Append.
SET max_parallel_workers_per_gather = 0;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a FROM union_a WHERE a > 100 UNION SELECT a FROM union_b WHERE a < 600;
SELECT union_same($$SELECT a FROM union_a WHERE a > 100 UNION SELECT a FROM union_b WHERE a < 600$$);
-- NULL is a value of its own; duplicates within a branch and across them.
SELECT union_same($$SELECT a % 50 AS x FROM union_a UNION SELECT a % 70 FROM union_b$$);
SELECT union_same($$SELECT a, b FROM union_a WHERE a < 300 UNION SELECT a, b / 10 FROM union_b$$);
SELECT union_same($$SELECT b FROM union_a UNION SELECT b FROM union_b UNION SELECT b FROM union_empty$$);
-- The plans show the first branch's columns, as the core's Append does.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a, b FROM union_a WHERE a < 300 UNION SELECT a, b / 10 FROM union_b WHERE a > 0;
-- A sort and a limit above.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a FROM union_a WHERE a > 100 UNION SELECT a FROM union_b WHERE a < 600 ORDER BY 1 DESC LIMIT 3;
SELECT a FROM union_a WHERE a > 100 UNION SELECT a FROM union_b WHERE a < 600 ORDER BY 1 DESC LIMIT 3;
-- In a subquery, and with more groups than work_mem holds.
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT a AS x FROM union_a UNION SELECT a FROM union_b) AS s$$);
SET work_mem = '64kB';
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT a * 1000 + b AS x FROM union_a
                   UNION SELECT a * 1000 + b FROM union_b UNION SELECT generate_series(1, 20000)) AS s$$);
RESET work_mem;
-- A UNION within another set operation, whose Append reads its columns by
-- position: the node's too. Projected to the other's column types (int4
-- within bigint), the core's: the projection would not find the set
-- operation's columns in the node's plan. So are the operations of a
-- recursive union.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a FROM union_a UNION SELECT a FROM union_b UNION ALL SELECT a FROM union_empty;
SELECT union_same($$SELECT a FROM union_a UNION SELECT a FROM union_b UNION ALL SELECT a FROM union_a WHERE a < 3$$);
EXPLAIN (COSTS OFF)
SELECT a FROM union_a UNION SELECT a FROM union_b UNION ALL SELECT b FROM union_empty;
SELECT union_same($$SELECT a FROM union_a UNION SELECT a FROM union_b UNION ALL SELECT b FROM union_a WHERE a < 3$$);
SELECT union_same($$WITH RECURSIVE r(n) AS ((SELECT a FROM union_a WHERE a < 4 UNION SELECT a FROM union_b WHERE a < 4)
                   UNION SELECT n + 1 FROM r WHERE n < 10) SELECT count(*), sum(n) FROM r$$);
-- Text columns go through a dictionary of their values.
EXPLAIN (VERBOSE, COSTS OFF) SELECT t FROM union_a UNION SELECT t FROM union_b;
SELECT union_same($$SELECT t FROM union_a UNION SELECT t FROM union_b$$);
-- Under a Gather: the node's partial grouping of every participant's rows
-- of the branches over its parallel Append, TessGather, and its grouping
-- of their groups above; at the default costs over 200000 rows whose keys
-- the planner knows few, read in parallel whatever their size. NULL a
-- value of its own, two columns, a count above, the workers alone; a
-- branch without a partial path leaves the UNION serial, as the core's.
CREATE TABLE union_keys AS
SELECT i % 10 AS b, CASE WHEN i % 11 = 0 THEN NULL ELSE i % 13 END AS c
FROM generate_series(1, 200000) AS i;
ANALYZE union_keys;
SET max_parallel_workers_per_gather = 2;
SET min_parallel_table_scan_size = 0;
-- The node's model of a partial scan would keep so small a table serial.
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
EXPLAIN (VERBOSE, COSTS OFF) SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3;
SELECT union_same($$SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3$$);
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT c AS x FROM union_keys UNION SELECT b FROM union_keys WHERE c IS NULL) AS s$$);
-- The same as the left side of an EXCEPT.
EXPLAIN (VERBOSE, COSTS OFF)
(SELECT c FROM union_keys WHERE b < 5 UNION SELECT c FROM union_keys WHERE b > 3) EXCEPT SELECT b FROM union_keys WHERE b = 1;
SELECT union_same($$(SELECT c FROM union_keys WHERE b < 5 UNION SELECT c FROM union_keys WHERE b > 3)
                   EXCEPT SELECT b FROM union_keys WHERE b = 1$$);
SET parallel_leader_participation = off;
SELECT union_same($$SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3$$);
RESET parallel_leader_participation;
ALTER TABLE union_b SET (parallel_workers = 0);
EXPLAIN (VERBOSE, COSTS OFF) SELECT c FROM union_keys UNION SELECT a FROM union_b;
ALTER TABLE union_b RESET (parallel_workers);
SET max_parallel_workers_per_gather = 0;
RESET min_parallel_table_scan_size;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
DROP TABLE union_keys;
-- INTERSECT and EXCEPT, with ALL or not: grouping of both sides by every
-- column, the left side's rows first, counting each group's rows and the
-- right side's; a group goes out as many times as the operation says.
-- NULL equals NULL; duplicates on both sides; an empty side; keys of
-- words and through dictionaries; sides of other types, cast.
CREATE TABLE setop_l AS
SELECT i % 40 AS k, (i % 3)::int2 AS s, CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 13) END AS t,
       ((i % 9) * 0.5)::numeric AS n, date '2020-01-01' + i % 5 AS d
FROM generate_series(1, 2000) AS i;
CREATE TABLE setop_r AS
SELECT i % 25 AS k, (i % 2)::int2 AS s, CASE WHEN i % 7 = 0 THEN NULL ELSE 'v' || (i % 17) END AS t,
       CASE WHEN i % 2 = 0 THEN ((i % 9) * 0.5)::numeric ELSE ((i % 9) * 0.5)::numeric(10, 3) END AS n,
       date '2020-01-01' + i % 3 AS d
FROM generate_series(1, 700) AS i;
INSERT INTO setop_l VALUES (NULL, NULL, NULL, NULL, NULL), (NULL, NULL, NULL, NULL, NULL);
INSERT INTO setop_r VALUES (NULL, NULL, NULL, NULL, NULL);
ANALYZE setop_l, setop_r;
EXPLAIN (VERBOSE, COSTS OFF) SELECT k, t FROM setop_l EXCEPT SELECT k, t FROM setop_r;
EXPLAIN (VERBOSE, COSTS OFF) SELECT k FROM setop_l INTERSECT ALL SELECT k FROM setop_r;
SELECT union_same($$SELECT k, t FROM setop_l EXCEPT SELECT k, t FROM setop_r$$);
SELECT union_same($$SELECT k, t FROM setop_l EXCEPT ALL SELECT k, t FROM setop_r$$);
SELECT union_same($$SELECT k, t FROM setop_l INTERSECT SELECT k, t FROM setop_r$$);
SELECT union_same($$SELECT k, t FROM setop_l INTERSECT ALL SELECT k, t FROM setop_r$$);
SELECT union_same($$SELECT k FROM setop_l EXCEPT ALL SELECT k FROM setop_r$$);
SELECT union_same($$SELECT k FROM setop_r EXCEPT ALL SELECT k FROM setop_l$$);
SELECT union_same($$SELECT s, d FROM setop_l INTERSECT ALL SELECT s, d FROM setop_r$$);
SELECT union_same($$SELECT n FROM setop_l EXCEPT SELECT n FROM setop_r$$);
-- Of equal values of other scales a group goes out in its first row's, as
-- the core's does, though the dictionary keeps a value's first form of the
-- whole input.
SELECT union_same($$SELECT n, k FROM setop_l INTERSECT ALL SELECT n, k FROM setop_r$$);
SELECT union_same($$SELECT n, k FROM setop_r EXCEPT SELECT n, k FROM setop_l WHERE k > 10$$);
SELECT union_same($$SELECT t FROM setop_l EXCEPT ALL SELECT t FROM setop_r WHERE k > 100$$);
SELECT union_same($$SELECT t FROM setop_l WHERE k > 100 INTERSECT SELECT t FROM setop_r$$);
SELECT union_same($$SELECT k, t FROM setop_l EXCEPT SELECT k::bigint, t FROM setop_r$$);
SELECT union_same($$SELECT k + 1, upper(t) FROM setop_l WHERE s = 1 EXCEPT ALL SELECT k, upper(t) FROM setop_r$$);
-- A group of 2000 copies: its rows go on across batches.
SELECT union_same($$SELECT 1 FROM setop_l EXCEPT ALL SELECT 1 FROM setop_r WHERE false$$);
SELECT union_same($$SELECT s FROM setop_l INTERSECT ALL SELECT s FROM setop_l WHERE k < 20$$);
-- Numeric 1.0 and 1.000 are one group: the left side's value goes out.
SELECT union_same($$SELECT n::text FROM (SELECT n FROM setop_r INTERSECT SELECT n FROM setop_l) AS q$$);
SELECT union_same($$SELECT n::text FROM (SELECT n FROM setop_r INTERSECT ALL SELECT n FROM setop_l) AS q$$);
-- Above: a sort and a limit; within another set operation, the node's in
-- the node's: EXCEPT of EXCEPT, INTERSECT within UNION, UNION and
-- INTERSECT of two columns as the sides of EXCEPT ALL, a count above.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT k FROM setop_l EXCEPT SELECT k FROM setop_r ORDER BY 1 DESC LIMIT 3;
SELECT k FROM setop_l EXCEPT SELECT k FROM setop_r ORDER BY 1 DESC LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF)
SELECT k FROM setop_l EXCEPT SELECT k FROM setop_r EXCEPT SELECT a FROM union_a;
SELECT union_same($$SELECT k FROM setop_l EXCEPT SELECT k FROM setop_r EXCEPT SELECT a FROM union_a WHERE a < 30$$);
SELECT union_same($$SELECT k FROM setop_l INTERSECT SELECT k FROM setop_r UNION SELECT a FROM union_b WHERE a < 5$$);
EXPLAIN (VERBOSE, COSTS OFF)
(SELECT k, t FROM setop_l UNION SELECT k, t FROM setop_r) EXCEPT ALL (SELECT a, t FROM union_a INTERSECT SELECT k, t FROM setop_l);
SELECT union_same($$(SELECT k, t FROM setop_l UNION SELECT k, t FROM setop_r)
                   EXCEPT ALL (SELECT a, t FROM union_a INTERSECT SELECT k, t FROM setop_l)$$);
SELECT union_same($$SELECT count(*), sum(x) FROM ((SELECT k AS x FROM setop_l UNION SELECT a FROM union_b)
                   INTERSECT ALL SELECT a % 50 FROM union_a) AS s$$);
-- Past hash_mem: the groups of words spill their records, the groups of a
-- dictionary their rows, each with its side.
SET work_mem = '64kB';
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT g AS x FROM generate_series(1, 30000) AS g
                   EXCEPT ALL SELECT g * 2 FROM generate_series(1, 10000) AS g) AS s$$);
SELECT union_same($$SELECT count(*), max(x) FROM (SELECT 'k' || (g % 20000) AS x FROM generate_series(1, 40000) AS g
                   INTERSECT ALL SELECT 'k' || (g * 3) FROM generate_series(1, 10000) AS g) AS s$$);
SELECT union_same($$SELECT count(*), max(x) FROM (SELECT 'k' || g AS x FROM generate_series(1, 30000) AS g
                   EXCEPT SELECT 'k' || (g * 7) FROM generate_series(1, 10000) AS g) AS s$$);
RESET work_mem;
-- Rescan: a set operation in a correlated subquery is not the query's own.
SELECT union_same($$SELECT x, (SELECT count(*) FROM (SELECT k FROM setop_l WHERE s = x
                   EXCEPT SELECT k FROM setop_r) AS q) FROM generate_series(0, 2) AS x$$);
DROP TABLE setop_l, setop_r;
-- UNION ALL the planner does not make a relation of, sorted.
EXPLAIN (VERBOSE, COSTS OFF)
SELECT a FROM union_a WHERE a > 990 UNION ALL SELECT b FROM union_b WHERE b > 69000 ORDER BY 1 LIMIT 4;
SELECT a FROM union_a WHERE a > 990 UNION ALL SELECT b FROM union_b WHERE b > 69000 ORDER BY 1 LIMIT 4;
RESET max_parallel_workers_per_gather;

-- A branch that gives a set operation's column as a constant: the node's
-- plan would show the first branch's targets in its columns' place, and
-- the core leaves a constant there rather than read the child's column,
-- so every row got the first branch's constant (found by
-- tessera-crosscheck, plan 9.8). Such a set operation stays the core's,
-- serial and parallel, the constant in either branch.
SET max_parallel_workers_per_gather = 0;
EXPLAIN (COSTS OFF) SELECT 1 FROM union_a UNION SELECT a FROM union_b;
SELECT union_same($$SELECT 1 FROM union_a UNION SELECT a FROM union_b$$);
SELECT union_same($$SELECT a, t FROM union_a UNION SELECT 7, 'b7' FROM union_b$$);
SELECT union_same($$SELECT a FROM union_a INTERSECT SELECT 3 FROM union_b$$);
SELECT union_same($$SELECT 3 FROM union_a EXCEPT ALL SELECT a FROM union_b$$);
SELECT union_same($$SELECT x FROM (SELECT 1 AS x FROM union_a UNION SELECT a FROM union_b) AS q ORDER BY x LIMIT 3$$);
SET max_parallel_workers_per_gather = 2;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;
SELECT union_same($$SELECT a, b FROM union_a UNION SELECT a, 10 FROM union_b WHERE b > 0$$);
SELECT union_same($$SELECT 1 FROM union_a UNION SELECT a FROM union_b$$);
RESET min_parallel_table_scan_size;
RESET parallel_tuple_cost;
RESET parallel_setup_cost;
RESET max_parallel_workers_per_gather;

-- An empty left side of INTERSECT or EXCEPT makes no group: the core's
-- SetOp does not read its right side, so an error the right side's rows
-- would raise is not raised; the node does not read it either (found by
-- tessera-crosscheck, plan 9.8).
EXPLAIN (COSTS OFF) SELECT a FROM union_empty EXCEPT ALL SELECT a + 2147483647 FROM union_a;
SELECT union_same($$SELECT a FROM union_empty EXCEPT ALL SELECT a + 2147483647 FROM union_a$$);
SELECT union_same($$SELECT a FROM union_empty INTERSECT SELECT a + 2147483647 FROM union_a$$);
SELECT union_same($$SELECT a FROM union_a WHERE a < 0 EXCEPT SELECT a + 2147483647 FROM union_b$$);

DROP TABLE union_part, union_parent, union_child, union_a, union_b, union_empty;
DROP FUNCTION union_same(text);
DROP FUNCTION union_run(text);
RESET union_test.bound;
DROP EXTENSION tessera;
