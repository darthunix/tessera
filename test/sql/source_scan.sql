-- The planning contract of a batch source (capability source-scan): the
-- test source's wrapper, tessera_test_scan, generates a table's rows
-- from its options, and its sources read the tables through a batch
-- node. Every query over a foreign table ft_x is compared with the same
-- query over a heap table ht_x of the same rows, planned by the core.
CREATE EXTENSION tessera;
-- EXPLAIN in the same words on every core: see the file.
\getenv abs_srcdir PG_ABS_SRCDIR
\set core_neutral :abs_srcdir '/sql/include/core_neutral.sql'
\set ECHO none
\i :core_neutral
\set ECHO all
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';
\getenv libdir PG_LIBDIR
\getenv dlsuffix PG_DLSUFFIX
\set scan_source :libdir '/tessera_scan_source_test' :dlsuffix
LOAD :'scan_source';

CREATE FUNCTION tessera_test_scan_fdw_handler()
RETURNS fdw_handler
AS :'scan_source', 'tessera_test_scan_fdw_handler'
LANGUAGE C STRICT;

-- Register the test sources named, in this order, and none else.
CREATE FUNCTION tessera_test_scan_sources(text[])
RETURNS void
AS :'scan_source', 'tessera_test_scan_sources'
LANGUAGE C STRICT;

-- How many times each source was asked since its registration.
CREATE FUNCTION tessera_test_scan_source_calls()
RETURNS text
AS :'scan_source', 'tessera_test_scan_source_calls'
LANGUAGE C STRICT;

-- The sources registered, in the order the planner asks them.
CREATE FUNCTION tessera_test_scan_source_listed()
RETURNS text
AS :'scan_source', 'tessera_test_scan_source_listed'
LANGUAGE C STRICT;

-- The asks of each form of the test source's batches since the last call.
CREATE FUNCTION tessera_test_scan_source_asks()
RETURNS text
AS :'scan_source', 'tessera_test_scan_source_asks'
LANGUAGE C STRICT;

-- The fault the faulty source makes, NULL for none.
CREATE FUNCTION tessera_test_scan_source_fault(text)
RETURNS void
AS :'scan_source', 'tessera_test_scan_source_fault'
LANGUAGE C;

CREATE FOREIGN DATA WRAPPER tessera_test_scan HANDLER tessera_test_scan_fdw_handler;
CREATE SERVER scan_server FOREIGN DATA WRAPPER tessera_test_scan;

-- The rows of query, over the foreign tables as planned now and over the
-- heap tables (ft_ named ht_) with Tessera off: 'same, n rows' or the
-- difference.
CREATE FUNCTION source_same(query text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    saved text := current_setting('tessera.enable');
    foreign_rows text[];
    heap_rows text[];
BEGIN
    EXECUTE format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q', query)
        INTO foreign_rows;
    PERFORM set_config('tessera.enable', 'off', true);
    EXECUTE format('SELECT array_agg(q::text ORDER BY q::text) FROM (%s) AS q',
                   replace(query, 'ft_', 'ht_'))
        INTO heap_rows;
    PERFORM set_config('tessera.enable', saved, true);
    IF foreign_rows IS DISTINCT FROM heap_rows THEN
        RETURN format('MISMATCH %s against %s', foreign_rows, heap_rows);
    END IF;
    RETURN format('same, %s rows', coalesce(cardinality(foreign_rows), 0));
END $$;

-- A table of 1000 rows: a is i, b is i % 10, c is i % 7 with every fifth
-- NULL, d the text of i % 13.
CREATE FOREIGN TABLE ft_t (a int4, b int4 OPTIONS (mod '10'),
    c int8 OPTIONS (mod '7', nulls '5'), d text OPTIONS (mod '13'))
    SERVER scan_server OPTIONS (rows '1000');
CREATE TABLE ht_t AS
    SELECT i AS a, i % 10 AS b, CASE WHEN i % 5 = 0 THEN NULL ELSE (i % 7)::int8 END AS c,
        (i % 13)::text AS d
    FROM generate_series(1, 1000) AS i;
ANALYZE ht_t;

-- The wrapper's own rows, with Tessera off: a scan, its clauses, a
-- grouping, and more than one group of 64 rows.
SET tessera.enable = off;
SELECT source_same('SELECT * FROM ft_t');
SELECT source_same('SELECT a, d FROM ft_t WHERE b = 3 AND c > 2');
SELECT source_same('SELECT b, count(*), sum(c), count(d) FROM ft_t GROUP BY b');
SELECT source_same('SELECT count(*) FROM ft_t WHERE c IS NULL');
EXPLAIN (COSTS OFF) SELECT a FROM ft_t WHERE b = 3;

-- Two paths: one that starts at once and one cheaper in total, which the
-- core keeps under a LIMIT.
CREATE FOREIGN TABLE ft_two (a int4) SERVER scan_server
    OPTIONS (rows '1000', startup '0', total '1000', startup2 '500', total2 '600');
CREATE TABLE ht_two AS SELECT i AS a FROM generate_series(1, 1000) AS i;
EXPLAIN (COSTS OFF) SELECT a FROM ft_two;
EXPLAIN (COSTS OFF) SELECT a FROM ft_two LIMIT 3;
SELECT source_same('SELECT a FROM ft_two');

-- Paths with path keys only: the rows come in the order of a.
CREATE FOREIGN TABLE ft_sorted (a int4, b int4 OPTIONS (mod '10')) SERVER scan_server
    OPTIONS (rows '300', pathkeys 'on');
CREATE TABLE ht_sorted AS SELECT i AS a, i % 10 AS b FROM generate_series(1, 300) AS i;
EXPLAIN (COSTS OFF) SELECT a FROM ft_sorted ORDER BY a;
SELECT (SELECT array_agg(a) FROM (SELECT a FROM ft_sorted ORDER BY a) AS q) =
    (SELECT array_agg(i) FROM generate_series(1, 300) AS i) AS in_order;

-- A path parameterized by a join clause, on the inner side of a nested
-- loop.
CREATE FOREIGN TABLE ft_join (a int4, b int4 OPTIONS (mod '10')) SERVER scan_server
    OPTIONS (rows '1000', parameterized 'on');
CREATE TABLE ht_join AS SELECT i AS a, i % 10 AS b FROM generate_series(1, 1000) AS i;
CREATE TABLE outer_rows AS SELECT i * 7 AS x FROM generate_series(1, 20) AS i;
ANALYZE outer_rows;
EXPLAIN (COSTS OFF)
    SELECT o.x, f.b FROM outer_rows AS o JOIN ft_join AS f ON f.a <= o.x AND f.a >= o.x;
SELECT source_same('SELECT o.x, f.b FROM outer_rows AS o JOIN ft_join AS f ON f.a <= o.x AND f.a >= o.x');

-- A partial path: the participants divide the groups among them. One
-- that is not parallel-aware is disabled, there for the planner to see:
-- the core's plan never takes it.
CREATE FOREIGN TABLE ft_par (a int4, b int4 OPTIONS (mod '10')) SERVER scan_server
    OPTIONS (rows '5000', partial 'on');
CREATE FOREIGN TABLE ft_unaware (a int4, b int4 OPTIONS (mod '10')) SERVER scan_server
    OPTIONS (rows '5000', partial 'unaware');
CREATE TABLE ht_par AS SELECT i AS a, i % 10 AS b FROM generate_series(1, 5000) AS i;
CREATE TABLE ht_unaware AS SELECT * FROM ht_par;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
EXPLAIN (COSTS OFF) SELECT a, b FROM ft_par;
SELECT source_same('SELECT a, b FROM ft_par');
SELECT source_same('SELECT b, count(*) FROM ft_par GROUP BY b');
EXPLAIN (COSTS OFF) SELECT a, b FROM ft_unaware;
SELECT source_same('SELECT a, b FROM ft_unaware');
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
RESET tessera.enable;

-- The SQLSTATE and message of a statement's error.
CREATE FUNCTION source_error(statement text)
RETURNS text
LANGUAGE plpgsql
AS $$
BEGIN
    EXECUTE statement;
    RETURN 'no error';
EXCEPTION WHEN OTHERS THEN
    RETURN SQLSTATE || ': ' || SQLERRM;
END
$$;

-- The plan rows of the first node of query's plan named name, a node
-- type or a custom provider.
CREATE FUNCTION plan_rows(query text, name text) RETURNS float8
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (FORMAT JSON) %s', query) INTO plan;
    RETURN jsonb_path_query_first(plan,
        'strict $.**?(@."Node Type" == $n || @."Custom Plan Provider" == $n)."Plan Rows"',
        jsonb_build_object('n', name))::float8;
END $$;

-- spec: source-scan/No source reads the relation
-- Every source returns NULL: the core's ForeignScan, packed under a
-- batch parent, as without sources.
SELECT tessera_test_scan_sources(ARRAY['none']);
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_t GROUP BY b;
SELECT source_same('SELECT b, count(*) FROM ft_t GROUP BY b');
SELECT tessera_test_scan_source_calls();

-- spec: source-scan/Batch parents over a source
-- A grouping, a sort under a limit and a hash join read the table, which
-- has no clauses, through the source's node and no TessPack.
SELECT tessera_test_scan_sources(ARRAY['reader']);
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(c) FROM ft_t GROUP BY b;
SELECT source_same('SELECT b, count(*), sum(c) FROM ft_t GROUP BY b');
EXPLAIN (COSTS OFF) SELECT a, d FROM ft_t ORDER BY c, a LIMIT 5;
SELECT source_same('SELECT a, d FROM (SELECT a, d FROM ft_t ORDER BY c, a LIMIT 5) AS q');
SET enable_mergejoin = off;
SET enable_nestloop = off;
EXPLAIN (COSTS OFF) SELECT f.a, o.x FROM ft_t AS f JOIN outer_rows AS o ON f.a = o.x;
SELECT source_same('SELECT f.a, o.x FROM ft_t AS f JOIN outer_rows AS o ON f.a = o.x');
RESET enable_mergejoin;
RESET enable_nestloop;
-- The rows of a scan the source answers row by row: a projection, a row
-- parent.
SELECT source_same('SELECT a + 1, d || ''x'' FROM ft_t');
-- The path alone has the rows of the path offered, whatever the source
-- estimates.
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_rows '7');
SELECT plan_rows('SELECT b FROM ft_t', 'TessTestSource') AS source_rows;
SET tessera.enable = off;
SELECT plan_rows('SELECT b FROM ft_t', 'Foreign Scan') AS wrapper_rows;
RESET tessera.enable;
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_rows);

-- spec: source-scan/A source read again
-- A nested loop scans the table through its source once for each outer
-- row, every scan all its rows: the inner side of a left join.
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_material = off;
EXPLAIN (COSTS OFF)
    SELECT o.x, f.b FROM outer_rows AS o LEFT JOIN ft_t AS f ON f.a < o.x;
SELECT source_same('SELECT o.x, f.b FROM outer_rows AS o LEFT JOIN ft_t AS f ON f.a < o.x');
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_material;

-- spec: source-scan/The path offered
-- Under a LIMIT the core keeps the path that starts at once beside the
-- one cheaper in total: the source is offered the latter, its private
-- data with it, and its path, cheaper than both, says so.
ALTER FOREIGN TABLE ft_two OPTIONS (ADD source_startup '0', ADD source_total '100');
EXPLAIN (COSTS OFF) SELECT a FROM ft_two LIMIT 3;
SELECT source_same('SELECT count(*) FROM (SELECT a FROM ft_two LIMIT 3) AS q');

-- spec: source-scan/A parallel grouping
-- A partial grouping reads the source's partial node in each
-- participant, TessGather above it; and a scan of the rows, TessGather
-- over the source's partial node.
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a) FROM ft_par GROUP BY b;
SELECT source_same('SELECT b, count(*), sum(a) FROM ft_par GROUP BY b');
EXPLAIN (COSTS OFF) SELECT a, b FROM ft_par;
SELECT source_same('SELECT a, b FROM ft_par');
-- The partial path alone has the rows of the partial path offered, one
-- participant's.
ALTER FOREIGN TABLE ft_par OPTIONS (ADD source_rows '7');
SELECT plan_rows('SELECT a, b FROM ft_par', 'TessTestSource') AS source_rows;
SET tessera.enable = off;
SELECT plan_rows('SELECT a, b FROM ft_par', 'Foreign Scan') AS wrapper_rows;
RESET tessera.enable;
ALTER FOREIGN TABLE ft_par OPTIONS (DROP source_rows);

-- spec: source-scan/A path refused
-- A core path, a custom path tess_path_create did not build, a batch path
-- of another relation, with parameters, with path keys, without
-- projection, serial for a partial offer, partial for a serial one,
-- partial and not parallel-aware, partial and not parallel-safe, of
-- another number of workers, and serial and parallel-safe where the offer
-- is not.
SELECT tessera_test_scan_sources(ARRAY['faulty']);
SELECT fault, source_error('EXPLAIN SELECT b, count(*) FROM ft_par GROUP BY b')
FROM unnest(ARRAY['core', 'other_custom', 'other_relation', 'params', 'pathkeys',
    'no_projection', 'serial_for_partial', 'partial_for_serial', 'partial_unaware',
    'partial_unsafe', 'workers']) AS fault,
    LATERAL tessera_test_scan_source_fault(fault);
SET max_parallel_workers_per_gather = 0;
SELECT tessera_test_scan_source_fault('serial_safe');
SELECT source_error('EXPLAIN SELECT b, count(*) FROM ft_par GROUP BY b');
RESET max_parallel_workers_per_gather;
SELECT tessera_test_scan_source_fault(NULL);
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
SELECT tessera_test_scan_sources(ARRAY[]::text[]);

-- spec: source-scan/Clauses in batches and by rows
-- The source checks neither clause: TessFilter checks the one the
-- compiler takes in batches, the regular expression by rows.
SELECT tessera_test_scan_sources(ARRAY['reader']);
EXPLAIN (COSTS OFF) SELECT a, d FROM ft_t WHERE b = 3 AND d ~ '1';
SELECT source_same('SELECT a, d FROM ft_t WHERE b = 3 AND d ~ ''1''');

-- spec: source-scan/A source that checks every clause
-- Every clause checked exactly, one on a column the query does not
-- select: the source's node alone, with the rows of the path offered,
-- the wrapper's, whatever the source estimates.
ALTER FOREIGN TABLE ft_t OPTIONS (ADD exact 'a,b', ADD source_rows '7');
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 100 AND b = 3;
SELECT source_same('SELECT d FROM ft_t WHERE a > 100 AND b = 3');
SELECT plan_rows('SELECT d FROM ft_t WHERE a > 100 AND b = 3', 'TessTestSource') AS source_rows;
SET tessera.enable = off;
SELECT plan_rows('SELECT d FROM ft_t WHERE a > 100 AND b = 3', 'Foreign Scan') AS wrapper_rows;
RESET tessera.enable;

-- spec: source-scan/A source that checks some clauses
-- One of two clauses checked exactly: the first under the source's
-- node, the other under TessFilter alone.
ALTER FOREIGN TABLE ft_t OPTIONS (SET exact 'a', DROP source_rows);
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 100 AND b = 3;
SELECT source_same('SELECT d FROM ft_t WHERE a > 100 AND b = 3');

-- spec: source-scan/A source that only skips rows
-- A clause used only to skip groups: TessFilter checks it and counts as
-- removed only the rows it rejects itself, 29 of the second group here
-- against 901 when the source skips nothing.
ALTER FOREIGN TABLE ft_t OPTIONS (DROP exact, ADD prune 'a');
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
    SELECT d FROM ft_t WHERE a < 100;
SELECT source_same('SELECT d FROM ft_t WHERE a < 100');
ALTER FOREIGN TABLE ft_t OPTIONS (DROP prune);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
    SELECT d FROM ft_t WHERE a < 100;
SELECT source_same('SELECT d FROM ft_t WHERE a < 100');

-- spec: source-scan/A clause on an outer row's value
-- A correlated subquery scans the table by the outer row's value, checked
-- exactly, then used only to skip groups.
ALTER FOREIGN TABLE ft_t OPTIONS (ADD exact 'a');
SELECT explain_neutral('SELECT o.x, (SELECT count(*) FROM ft_t AS f WHERE f.a < o.x * 10) FROM outer_rows AS o');
SELECT source_same('SELECT o.x, (SELECT count(*) FROM ft_t AS f WHERE f.a < o.x * 10) FROM outer_rows AS o');
ALTER FOREIGN TABLE ft_t OPTIONS (DROP exact, ADD prune 'a');
SELECT explain_neutral('SELECT o.x, (SELECT count(*) FROM ft_t AS f WHERE f.a < o.x * 10) FROM outer_rows AS o');
SELECT source_same('SELECT o.x, (SELECT count(*) FROM ft_t AS f WHERE f.a < o.x * 10) FROM outer_rows AS o');

-- spec: source-scan/A value fixed for the statement
-- A generic plan of a prepared statement, executed with two values, by
-- the parameter and by $1 + 1; a clause on now(); a clause on an
-- uncorrelated subquery's value, checked exactly and used to skip
-- groups. The subquery's notice shows when it runs: never under a plain
-- EXPLAIN.
CREATE FUNCTION noisy_max() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
    RAISE NOTICE 'the subquery ran';
    RETURN 300;
END $$;
ALTER FOREIGN TABLE ft_t OPTIONS (DROP prune, ADD exact 'a');
SET plan_cache_mode = force_generic_plan;
PREPARE source_param(int) AS SELECT count(*), sum(a) FROM ft_t WHERE a < $1;
PREPARE source_param_plus(int) AS SELECT count(*), sum(a) FROM ft_t WHERE a < $1 + 1;
PREPARE heap_param(int) AS SELECT count(*), sum(a) FROM ht_t WHERE a < $1;
EXPLAIN (COSTS OFF) EXECUTE source_param(10);
EXPLAIN (COSTS OFF) EXECUTE source_param_plus(10);
EXECUTE source_param(10);
EXECUTE source_param(500);
EXECUTE source_param_plus(10);
EXECUTE source_param_plus(500);
EXECUTE heap_param(10);
EXECUTE heap_param(500);
EXECUTE heap_param(11);
EXECUTE heap_param(501);
DEALLOCATE source_param;
DEALLOCATE source_param_plus;
DEALLOCATE heap_param;
RESET plan_cache_mode;
EXPLAIN (COSTS OFF)
    SELECT count(*) FROM ft_t WHERE a < extract(year FROM now())::int - 1500;
SELECT source_same('SELECT count(*) FROM ft_t WHERE a < extract(year FROM now())::int - 1500');
SELECT explain_neutral('SELECT count(*), sum(a) FROM ft_t WHERE a < (SELECT noisy_max())');
SELECT count(*), sum(a) FROM ft_t WHERE a < (SELECT noisy_max());
ALTER FOREIGN TABLE ft_t OPTIONS (DROP exact, ADD prune 'a');
SELECT explain_neutral('SELECT count(*), sum(a) FROM ft_t WHERE a < (SELECT noisy_max())');
SELECT count(*), sum(a) FROM ft_t WHERE a < (SELECT noisy_max());
SELECT count(*), sum(a) FROM ht_t WHERE a < 300;
ALTER FOREIGN TABLE ft_t OPTIONS (DROP prune);

-- spec: source-scan/Clauses under a row security policy
-- A partitioned table with a policy, a heap partition and a foreign one,
-- read by a role the policy binds, by a clause that is not leakproof and
-- by one that is: the policy's clause and the leakproof one are usable,
-- the other is not, and TessFilter checks it after the policy's. Over a
-- view with security_barrier, the clause that is not leakproof stays
-- above the view's scan.
CREATE FUNCTION leaky(int) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS
    $$ BEGIN RETURN $1 % 3 = 0; END $$;
-- Not leakproof either, and fails on a row of the policy's.
CREATE FUNCTION peek(int) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    IF $1 >= 5 THEN
        RAISE EXCEPTION 'peek saw a row the policy hides';
    END IF;
    RETURN true;
END $$;
CREATE TABLE ft_parted (a int4, b int4) PARTITION BY RANGE (a);
CREATE FOREIGN TABLE ft_parted_far PARTITION OF ft_parted FOR VALUES FROM (1) TO (1001)
    SERVER scan_server OPTIONS (rows '1000', exact 'a,b');
ALTER FOREIGN TABLE ft_parted_far ALTER COLUMN b OPTIONS (ADD mod '10');
CREATE TABLE ft_parted_near PARTITION OF ft_parted FOR VALUES FROM (1001) TO (2001);
INSERT INTO ft_parted_near SELECT i, i % 10 FROM generate_series(1001, 2000) AS i;
CREATE TABLE ht_parted (a int4, b int4) PARTITION BY RANGE (a);
CREATE TABLE ht_parted_far PARTITION OF ht_parted FOR VALUES FROM (1) TO (1001);
CREATE TABLE ht_parted_near PARTITION OF ht_parted FOR VALUES FROM (1001) TO (2001);
INSERT INTO ht_parted SELECT i, i % 10 FROM generate_series(1, 2000) AS i;
ANALYZE ft_parted_near, ht_parted;
CREATE ROLE regress_source_rls;
ALTER TABLE ft_parted ENABLE ROW LEVEL SECURITY;
ALTER TABLE ht_parted ENABLE ROW LEVEL SECURITY;
CREATE POLICY ft_parted_visible ON ft_parted USING (b < 5);
CREATE POLICY ht_parted_visible ON ht_parted USING (b < 5);
CREATE VIEW ft_barrier WITH (security_barrier) AS SELECT a, b FROM ft_t WHERE b < 5;
CREATE VIEW ht_barrier WITH (security_barrier) AS SELECT a, b FROM ht_t WHERE b < 5;
GRANT SELECT ON ft_parted, ht_parted, ft_barrier, ht_barrier TO regress_source_rls;
SET ROLE regress_source_rls;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ft_parted WHERE leaky(a) AND a > 10;
SELECT source_same('SELECT a FROM ft_parted WHERE leaky(a) AND a > 10');
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM ft_barrier WHERE leaky(a) AND a > 10;
SELECT source_same('SELECT a FROM ft_barrier WHERE leaky(a) AND a > 10');
-- The source leaves the policy's clause to TessFilter, which checks it
-- before a clause that is not leakproof and fails on any row the policy
-- hides.
RESET ROLE;
ALTER FOREIGN TABLE ft_parted_far OPTIONS (SET exact 'a');
SET ROLE regress_source_rls;
EXPLAIN (COSTS OFF) SELECT a FROM ft_parted WHERE peek(b) AND a > 10;
SELECT source_same('SELECT a FROM ft_parted WHERE peek(b) AND a > 10');
RESET ROLE;

-- spec: source-scan/A source's own check reveals nothing
-- The source compares by code of its own, exactly and to skip groups, a
-- leakproof clause over the rows the policy hides, and one the core
-- pushes into the view over the rows the view's clause hides: no error,
-- no message.
ALTER FOREIGN TABLE ft_parted_far OPTIONS (SET exact 'a', ADD prune 'b', ADD own 'on');
ALTER FOREIGN TABLE ft_t OPTIONS (ADD exact 'a', ADD prune 'b', ADD own 'on');
SET ROLE regress_source_rls;
EXPLAIN (COSTS OFF) SELECT a FROM ft_parted WHERE a > 10 AND b > 2;
SELECT source_same('SELECT a FROM ft_parted WHERE a > 10 AND b > 2');
EXPLAIN (COSTS OFF) SELECT a FROM ft_barrier WHERE a > 10 AND b > 2;
SELECT source_same('SELECT a FROM ft_barrier WHERE a > 10 AND b > 2');
RESET ROLE;
ALTER FOREIGN TABLE ft_t OPTIONS (DROP exact, DROP prune, DROP own);

-- spec: source-scan/A volatile clause and a subquery in a clause
-- A volatile clause in a full scan without a limit, called as many
-- times as over the heap table; and a NOT IN over a subquery whose
-- column may be NULL, which stays in the clause: neither is usable.
CREATE SEQUENCE source_calls;
CREATE FUNCTION counted(int) RETURNS boolean LANGUAGE plpgsql VOLATILE AS
    $$ BEGIN RETURN nextval('source_calls') > 0 AND $1 % 2 = 0; END $$;
CREATE TABLE maybe_null AS SELECT CASE WHEN i = 5 THEN NULL ELSE i * 10 END AS x
    FROM generate_series(1, 5) AS i;
ALTER FOREIGN TABLE ft_t OPTIONS (ADD exact 'a');
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM ft_t WHERE counted(a);
SELECT setval('source_calls', 1, false) IS NOT NULL AS reset;
SELECT count(*) FROM ft_t WHERE counted(a);
SELECT nextval('source_calls') - 1 AS calls_foreign;
SELECT setval('source_calls', 1, false) IS NOT NULL AS reset;
SELECT count(*) FROM ht_t WHERE counted(a);
SELECT nextval('source_calls') - 1 AS calls_heap;
SELECT explain_neutral('SELECT count(*) FROM ft_t WHERE a NOT IN (SELECT x FROM maybe_null)',
    'VERBOSE, COSTS OFF');
SELECT source_same('SELECT count(*) FROM ft_t WHERE a NOT IN (SELECT x FROM maybe_null)');

-- spec: source-scan/Answers that are not allowed
-- An exact and a pruning answer for a clause not usable, a value none of
-- the three, an exact and a pruning answer for a clause the path does
-- not carry; under TessFilter, a plan with qualifiers and a plan
-- without a clause it answered for. Beyond the scenario, an internal
-- check: a clause handed to the filter that its path did not record.
SELECT tessera_test_scan_sources(ARRAY['faulty']);
SELECT fault, source_error('EXPLAIN SELECT count(*) FROM ft_t WHERE counted(b) AND a > 10 AND b < 5')
FROM unnest(ARRAY['unusable', 'unusable_prune', 'bad_value', 'not_carried',
    'not_carried_prune', 'plan_qual', 'plan_missing', 'extra_clause']) AS fault,
    LATERAL tessera_test_scan_source_fault(fault);
SELECT tessera_test_scan_source_fault(NULL);

-- spec: source-scan/The first source that reads the relation
-- The first answers that it checks the clause and declines, the second
-- reads the table and leaves the answers, the third, cheaper, is not
-- asked.
ALTER FOREIGN TABLE ft_t OPTIONS (DROP exact);
SELECT tessera_test_scan_sources(ARRAY['decliner', 'reader', 'cheaper']);
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 900;
SELECT tessera_test_scan_source_calls();
SELECT source_same('SELECT d FROM ft_t WHERE a > 900');

-- spec: source-scan/A source not safe in parallel
-- A table read by the wrapper's serial path alone, whose source's path
-- may run in a worker, then is not parallel-safe: in a parallel join
-- with a heap table the source's node and TessFilter over it run inside
-- the workers, then neither does.
SELECT tessera_test_scan_sources(ARRAY['reader']);
CREATE TABLE big_rows AS SELECT i AS y, i % 1000 AS a FROM generate_series(1, 200000) AS i;
ANALYZE big_rows;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET tessera.scan_parallel_setup_cost = 0;
SET min_parallel_table_scan_size = 0;
EXPLAIN (COSTS OFF)
    SELECT count(*) FROM big_rows AS g JOIN ft_t AS f ON f.a = g.a WHERE f.b < 5;
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_unsafe 'on');
EXPLAIN (COSTS OFF)
    SELECT count(*) FROM big_rows AS g JOIN ft_t AS f ON f.a = g.a WHERE f.b < 5;
SELECT source_same('SELECT count(*) FROM big_rows AS g JOIN ft_t AS f ON f.a = g.a WHERE f.b < 5');
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_unsafe);
RESET tessera.scan_parallel_setup_cost;
RESET min_parallel_table_scan_size;

-- spec: source-scan/Parallel rows through a source
-- A clause checked exactly when the serial path is offered and left when
-- the partial one is: TessGather over TessFilter over the source's
-- partial node, with the partial path's rows, and the core's Gather
-- there with tessera.batch_gather off.
ALTER FOREIGN TABLE ft_par OPTIONS (ADD exact 'b', ADD partial_exact '', ADD source_rows '7');
EXPLAIN (COSTS OFF) SELECT a, b FROM ft_par WHERE b < 5;
SELECT source_same('SELECT a, b FROM ft_par WHERE b < 5');
SELECT plan_rows('SELECT a, b FROM ft_par WHERE b < 5', 'TessFilter') AS filter_rows;
SET tessera.batch_gather = off;
EXPLAIN (COSTS OFF) SELECT a, b FROM ft_par WHERE b < 5;
SELECT source_same('SELECT a, b FROM ft_par WHERE b < 5');
RESET tessera.batch_gather;
SET tessera.enable = off;
SELECT plan_rows('SELECT a, b FROM ft_par WHERE b < 5', 'Foreign Scan') AS partial_rows;
RESET tessera.enable;

ALTER FOREIGN TABLE ft_par OPTIONS (DROP source_rows);

-- spec: source-scan/A parallel count
-- A partial TessAgg over TessFilter over the source's partial node, then
-- over the partial node alone, every clause checked by the source.
EXPLAIN (COSTS OFF) SELECT count(*) FROM ft_par WHERE b < 5;
SELECT source_same('SELECT count(*) FROM ft_par WHERE b < 5');
ALTER FOREIGN TABLE ft_par OPTIONS (SET partial_exact 'b');
EXPLAIN (COSTS OFF) SELECT count(*) FROM ft_par WHERE b < 5;
SELECT source_same('SELECT count(*) FROM ft_par WHERE b < 5');

-- spec: source-scan/A parallel read by a subquery's value
-- A clause on an uncorrelated subquery's value, checked exactly by the
-- partial node alone, then used to skip groups under TessFilter: every
-- worker uses the value the gather sends.
ALTER FOREIGN TABLE ft_par OPTIONS (DROP exact, SET partial_exact 'a');
SELECT explain_neutral('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
SELECT source_same('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
SET tessera.batch_gather = off;
SELECT source_same('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
RESET tessera.batch_gather;
ALTER FOREIGN TABLE ft_par OPTIONS (DROP partial_exact, ADD prune 'a');
SELECT explain_neutral('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
SELECT source_same('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
SET tessera.batch_gather = off;
SELECT source_same('SELECT a, b FROM ft_par WHERE a < (SELECT max(x) * 10 FROM outer_rows)');
RESET tessera.batch_gather;
ALTER FOREIGN TABLE ft_par OPTIONS (DROP prune);
RESET parallel_setup_cost;
RESET parallel_tuple_cost;
SELECT tessera_test_scan_sources(ARRAY[]::text[]);

-- spec: source-scan/A source that costs less than its wrapper
-- The source's path, and TessFilter over it with the cost of the clause
-- it leaves, cost less than the wrapper's at the start and in total: the
-- plan reads through the source with tessera.scan_cost_factor at its
-- least and at its greatest, and TessFilter has the rows of the path
-- offered, whatever the source estimates.
SELECT tessera_test_scan_sources(ARRAY['reader']);
SET tessera.scan_cost_factor = 0;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 900;
SET tessera.scan_cost_factor = 10;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 900;
RESET tessera.scan_cost_factor;
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_rows '7');
SELECT plan_rows('SELECT d FROM ft_t WHERE a > 900', 'TessFilter') AS filter_rows;
SET tessera.enable = off;
SELECT plan_rows('SELECT d FROM ft_t WHERE a > 900', 'Foreign Scan') AS wrapper_rows;
RESET tessera.enable;
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_rows);

-- spec: source-scan/A source that costs more than its wrapper
-- With clauses and without, under a limit and under a grouping: the
-- core's ForeignScan, packed under the batch grouping.
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_startup '2000', ADD source_total '3000');
EXPLAIN (COSTS OFF) SELECT d FROM ft_t;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 900;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE a > 900 LIMIT 3;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_t WHERE a > 900 GROUP BY b;
SELECT source_same('SELECT b, count(*) FROM ft_t WHERE a > 900 GROUP BY b');
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_startup, DROP source_total);

-- spec: source-scan/The clauses left decide
-- The source's path alone costs less than the wrapper's, but a costly
-- clause it leaves, for each row, makes TessFilter cost more; and under
-- a LIMIT, the hashed subquery of a NOT IN it leaves, built before the
-- first row, makes TessFilter start later than the wrapper's scan,
-- which TessFilter costs less than without the LIMIT.
CREATE FUNCTION costly(int) RETURNS boolean LANGUAGE plpgsql IMMUTABLE COST 100000 AS
    $$ BEGIN RETURN $1 > 0; END $$;
-- A column with no NULL that may hold one keeps the NOT IN a subquery.
CREATE TABLE many_null AS SELECT i * 2 AS x FROM generate_series(1, 20000) AS i;
ANALYZE many_null;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t;
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE costly(a);
-- The cost for a row counts the rows the source returns, not the rows
-- of the path offered: one row of the source's keeps TessFilter, over a
-- wrapper that offers 333.
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_rows '1');
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE costly(a);
SELECT source_same('SELECT d FROM ft_t WHERE costly(a)');
-- The filter's terms, from its plan and its source's: the clauses'
-- start cost, a hashed subquery's building, counted in the start and in
-- the total alike, and their cost for a row, which grows with the rows
-- the source returns, ten times for ten times the rows.
CREATE FUNCTION filter_terms(query text) RETURNS TABLE (startup numeric, per_rows numeric)
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
    filter jsonb;
    source jsonb;
BEGIN
    EXECUTE format('EXPLAIN (FORMAT JSON) %s', query) INTO plan;
    filter := jsonb_path_query_first(plan, 'strict $.**?(@."Custom Plan Provider" == "TessFilter")');
    source := jsonb_path_query_first(plan, 'strict $.**?(@."Custom Plan Provider" == "TessTestSource")');
    startup := (filter->>'Startup Cost')::numeric - (source->>'Startup Cost')::numeric;
    per_rows := (filter->>'Total Cost')::numeric - (source->>'Total Cost')::numeric - startup;
    RETURN NEXT;
END $$;
ALTER FOREIGN TABLE ft_t OPTIONS (ADD source_startup '0', ADD source_total '10',
    SET source_rows '3');
SELECT startup, round(per_rows / 250) AS source_rows FROM filter_terms('SELECT d FROM ft_t WHERE costly(a)');
ALTER FOREIGN TABLE ft_t OPTIONS (SET source_rows '700');
CREATE TEMP TABLE terms AS
    SELECT * FROM filter_terms('SELECT d FROM ft_t WHERE a NOT IN (SELECT x FROM many_null)');
ALTER FOREIGN TABLE ft_t OPTIONS (SET source_rows '7000');
INSERT INTO terms
    SELECT * FROM filter_terms('SELECT d FROM ft_t WHERE a NOT IN (SELECT x FROM many_null)');
SELECT min(startup) > 0 AS start_counted, min(startup) = max(startup) AS same_start,
    round(max(per_rows) / min(per_rows)) AS rows_ratio FROM terms;
DROP TABLE terms;
DROP FUNCTION filter_terms(text);
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_rows);
SELECT explain_neutral('SELECT d FROM ft_t WHERE a NOT IN (SELECT x FROM many_null)');
SELECT explain_neutral('SELECT d FROM ft_t WHERE a NOT IN (SELECT x FROM many_null) LIMIT 1');
SELECT source_same('SELECT d FROM ft_t WHERE a NOT IN (SELECT x FROM many_null)');
ALTER FOREIGN TABLE ft_t OPTIONS (DROP source_startup, DROP source_total);
SELECT tessera_test_scan_sources(ARRAY[]::text[]);
DROP TABLE many_null;
DROP FUNCTION costly(int);

-- spec: source-scan/Tessera off
-- No source is asked, and the plan has the core's ForeignScan alone.
SELECT tessera_test_scan_sources(ARRAY['reader']);
SET tessera.enable = off;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_t WHERE a > 900 GROUP BY b;
SELECT tessera_test_scan_source_calls();
RESET tessera.enable;

-- spec: source-scan/A statement that locks or writes rows
-- A SELECT FOR UPDATE, with an ORDER BY a batch sort takes too; an
-- INSERT of the rows of a SELECT the planner merges into it; an UPDATE
-- FROM the table: the core's ForeignScan, under LockRows in the first.
-- An INSERT of a grouping, planned as a query of its own: the grouping
-- reads through the source's node.
CREATE TABLE sink (a int4, b int4);
EXPLAIN (COSTS OFF) SELECT a FROM ft_t FOR UPDATE;
EXPLAIN (COSTS OFF) SELECT a FROM ft_t ORDER BY b FOR UPDATE;
EXPLAIN (COSTS OFF) INSERT INTO sink SELECT a, b FROM ft_t;
EXPLAIN (COSTS OFF) UPDATE sink SET b = f.b FROM ft_t AS f WHERE sink.a = f.a;
SELECT explain_neutral('INSERT INTO sink SELECT b, count(*) FROM ft_t GROUP BY b');
INSERT INTO sink SELECT b, count(*) FROM ft_t GROUP BY b;
SELECT array_agg(a ORDER BY a) = (SELECT array_agg(DISTINCT b ORDER BY b) FROM ht_t) AS groups,
    sum(b) = 1000 AS rows FROM sink;
DROP TABLE sink;

-- spec: source-scan/Relations and paths not offered
-- A lateral reference, a pseudoconstant clause, foreign paths with path
-- keys only, the inner side of a nested loop through the wrapper's
-- parameterized path, and in parallel a partial path not parallel-aware:
-- no source is offered those paths.
SELECT tessera_test_scan_sources(ARRAY['reader']);
EXPLAIN (COSTS OFF)
    SELECT o.x, s.y, s.a FROM outer_rows AS o
    LEFT JOIN LATERAL (SELECT o.x AS y, f.a FROM ft_t AS f) AS s ON s.a = o.x;
SELECT source_same('SELECT o.x, s.y, s.a FROM outer_rows AS o LEFT JOIN LATERAL (SELECT o.x AS y, f.a FROM ft_t AS f) AS s ON s.a = o.x');
EXPLAIN (COSTS OFF) SELECT d FROM ft_t WHERE current_setting('tessera.enable') = 'on';
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_sorted GROUP BY b;
SELECT tessera_test_scan_source_calls();
EXPLAIN (COSTS OFF)
    SELECT o.x, f.b FROM outer_rows AS o JOIN ft_join AS f ON f.a <= o.x AND f.a >= o.x;
SELECT tessera_test_scan_sources(ARRAY['reader']);
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_unaware GROUP BY b;
SELECT tessera_test_scan_source_calls();
SELECT source_same('SELECT b, count(*) FROM ft_unaware GROUP BY b');
RESET parallel_setup_cost;
RESET parallel_tuple_cost;

-- spec: source-scan/A foreign partition
-- A grouping over a partitioned table of a heap and a foreign
-- partition, over a heap table whose child of inheritance is a foreign
-- table, and over a UNION ALL of a heap table and a foreign one: the
-- heap side through the heap scan node, the foreign side through the
-- source's node, under TessAppend.
ALTER FOREIGN TABLE ft_parted_far OPTIONS (DROP exact, DROP prune, DROP own);
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a) FROM ft_parted GROUP BY b;
SELECT source_same('SELECT b, count(*), sum(a) FROM ft_parted GROUP BY b');
CREATE TABLE ft_inherited (a int4, b int4);
INSERT INTO ft_inherited SELECT i, i % 10 FROM generate_series(1001, 1500) AS i;
CREATE FOREIGN TABLE ft_inherited_child () INHERITS (ft_inherited) SERVER scan_server
    OPTIONS (rows '1000');
ALTER FOREIGN TABLE ft_inherited_child ALTER COLUMN b OPTIONS (ADD mod '10');
CREATE TABLE ht_inherited (a int4, b int4);
INSERT INTO ht_inherited SELECT i, i % 10 FROM generate_series(1001, 1500) AS i;
CREATE TABLE ht_inherited_child () INHERITS (ht_inherited);
INSERT INTO ht_inherited_child SELECT i, i % 10 FROM generate_series(1, 1000) AS i;
ANALYZE ONLY ft_inherited;
ANALYZE ht_inherited, ht_inherited_child;
EXPLAIN (COSTS OFF) SELECT b, count(*), sum(a) FROM ft_inherited GROUP BY b;
SELECT source_same('SELECT b, count(*), sum(a) FROM ft_inherited GROUP BY b');
SELECT explain_neutral('SELECT b, count(*), sum(a) FROM (SELECT a, b FROM ht_join UNION ALL SELECT a, b FROM ft_t) AS u GROUP BY b');
SELECT source_same('SELECT b, count(*), sum(a) FROM (SELECT a, b FROM ht_join UNION ALL SELECT a, b FROM ft_t) AS u GROUP BY b');

-- spec: source-scan/Heap tables beside a source
-- A source that would take any path it is offered, registered while
-- queries read heap tables: it is never asked, so the plans are those
-- without it.
SELECT tessera_test_scan_sources(ARRAY['any']);
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ht_t WHERE a > 100 GROUP BY b;
EXPLAIN (COSTS OFF) SELECT o.x, h.d FROM outer_rows AS o JOIN ht_t AS h ON h.a = o.x;
SELECT tessera_test_scan_source_calls();
SELECT source_same('SELECT b, count(*) FROM ht_t WHERE a > 100 GROUP BY b');

-- spec: source-scan/A source of the least size
-- A source of the least size, without scan_rows, one whose scan_rows is
-- NULL and one past the current structure, whose scan_rows declines,
-- beside the reader: all are found and listed, the first two are offered
-- nothing, the third is asked, and the reader reads the table.
SELECT tessera_test_scan_sources(ARRAY['least', 'null_callback', 'past', 'reader']);
SELECT tessera_test_scan_source_listed();
EXPLAIN (COSTS OFF) SELECT b, count(*) FROM ft_t GROUP BY b;
SELECT tessera_test_scan_source_calls();
SELECT tessera_test_scan_sources(ARRAY[]::text[]);

-- spec: string-bytes/A column offered as bytes
-- spec: string-bytes/Batches over one dictionary
-- The test source's bytes form, which later consumers read: text columns
-- given plain, through a dictionary that changes every three batches,
-- and through one that holds each value at two indexes, beside a text
-- column of no bounded values, given without a dictionary, with NULL rows,
-- empty strings and strings of up to 10 000 bytes, over a rescan too. The
-- source reads each batch it makes as a consumer would (check_bytes): the
-- layout; each view's arrays and bytes as given, and those of the row's
-- Datum, after asks of every column in both forms; the dictionary's count
-- and each row's index against the generator's, one index's bytes the same
-- under its identity, the identities of two columns apart and each kept
-- within its batches and changed at the next and after a rescan; a plain
-- column's fields written over a result of a dictionary; a structure of an
-- older size refused and left as it was; and a projection over the source
-- passes the bytes of the columns it passes on and no others. TessAgg
-- reads a grouping's text key in the bytes form, asking no Datum value of
-- it, where the batches offer it: the asks of either form are counted,
-- and the batches checked are shown.
SELECT tessera_test_scan_sources(ARRAY['reader']);
CREATE FOREIGN TABLE ft_bytes (a int4, s text OPTIONS (mod '50', width '10000'),
    n text OPTIONS (mod '13', nulls '7'), p text)
    SERVER scan_server
    OPTIONS (rows '1000', bytes 'dictionary', dictionary_batches '3', check_bytes 'on');
CREATE TABLE ht_bytes AS
    SELECT i AS a,
        CASE WHEN i % 50 = 0 THEN ''
             ELSE (i % 50)::text || repeat('x', (i % 50) * 7919 % 10000) END AS s,
        CASE WHEN i % 7 = 0 THEN NULL ELSE (i % 13)::text END AS n,
        i::text AS p
    FROM generate_series(1, 1000) AS i;
SELECT tessera_test_scan_source_asks();
SELECT source_same('SELECT a, length(s), md5(s), n, p FROM ft_bytes');
SELECT tessera_test_scan_source_asks();
SELECT source_same('SELECT s, count(*), count(n) FROM ft_bytes GROUP BY s');
SELECT source_same('SELECT g, (SELECT count(n) FROM ft_bytes WHERE a > g)
    FROM generate_series(1, 3) AS g');
SELECT tessera_test_scan_source_asks();
-- The ungrouped COUNT(DISTINCT) of s and of n reads both as bytes, beside
-- a sum of the column a before them: Datums of a alone are asked, twice
-- a batch, by the read of the columns in their order and by the sum.
SELECT source_same('SELECT count(DISTINCT s), count(DISTINCT n), sum(a) FROM ft_bytes');
SELECT tessera_test_scan_source_asks();
-- Under a FILTER of a, which numbers some of the rows whose bytes the
-- batch gave, 5 values of s, 3 of n and none: Datums of a alone, for the
-- read in order and each filter.
SELECT source_same('SELECT count(DISTINCT s) FILTER (WHERE a % 50 < 5),
                           count(DISTINCT n) FILTER (WHERE a % 13 < 3),
                           count(DISTINCT s) FILTER (WHERE a < 0) FROM ft_bytes');
SELECT count(DISTINCT s) FILTER (WHERE a % 50 < 5) AS s5,
       count(DISTINCT n) FILTER (WHERE a % 13 < 3) AS n3,
       count(DISTINCT s) FILTER (WHERE a < 0) AS none FROM ft_bytes;
SELECT tessera_test_scan_source_asks();
-- Through the source's projection, which passes s on and computes the
-- others: the bytes of s at the source's addresses, and no bytes of the
-- computed columns.
EXPLAIN (COSTS OFF, VERBOSE) SELECT s, lower(n), p || 'y' FROM ft_bytes;
SELECT source_same('SELECT s, lower(n), p || ''y'' FROM ft_bytes');
SELECT tessera_test_scan_source_asks();
ALTER FOREIGN TABLE ft_bytes OPTIONS (SET bytes 'twice');
SELECT source_same('SELECT n, count(*) FROM ft_bytes GROUP BY n');
SELECT tessera_test_scan_source_asks();
ALTER FOREIGN TABLE ft_bytes OPTIONS (SET bytes 'plain');
SELECT source_same('SELECT a, md5(s), n FROM ft_bytes WHERE a > 900');
SELECT tessera_test_scan_source_asks();
-- Without the bridge's identities, as an older bridge has none, every
-- column is given plain.
ALTER FOREIGN TABLE ft_bytes OPTIONS (SET bytes 'dictionary', ADD bridge_identities 'off');
SELECT source_same('SELECT n, count(*) FROM ft_bytes GROUP BY n');
SELECT tessera_test_scan_source_asks();
ALTER FOREIGN TABLE ft_bytes OPTIONS (DROP bridge_identities);
-- Values bounded past what an index holds: given plain, in either form of
-- a dictionary; and a scan of one row in the form of two indexes a value,
-- whose one row stands at an even place.
CREATE FOREIGN TABLE ft_bytes_wide (a int4, s text OPTIONS (mod '2147483648'))
    SERVER scan_server OPTIONS (rows '2', bytes 'twice', check_bytes 'on');
SELECT count(s) FROM ft_bytes_wide;
SELECT tessera_test_scan_source_asks();
CREATE FOREIGN TABLE ft_bytes_one (a int4, s text OPTIONS (mod '13'))
    SERVER scan_server OPTIONS (rows '1', bytes 'twice', check_bytes 'on');
SELECT count(s) FROM ft_bytes_one;
SELECT tessera_test_scan_source_asks();
DROP FOREIGN TABLE ft_bytes_wide, ft_bytes_one;
-- Keys of both forms in one dictionary: the batches given as Datum values
-- and in the bytes form in turn, the long values compressed, with NULL
-- rows and empty strings, so that a grouping's value enters from either
-- form and is found from the other, a compressed one made plain to be
-- compared: the groups of the heap; and, of 3000 values under a small
-- work_mem, the lookups of a frozen table and the rows it writes to disk.
CREATE FOREIGN TABLE ft_bytes_mixed (a int4,
    s text OPTIONS (mod '50', width '10000', compress 'on', nulls '7'),
    m text OPTIONS (mod '3000', width '300', compress 'on'))
    SERVER scan_server OPTIONS (rows '4000', bytes 'plain', bytes_batches 'alternate');
-- The source's check of its own batches, which reads each batch's plain
-- values in both forms, is refused with these options.
ALTER FOREIGN TABLE ft_bytes_mixed OPTIONS (ADD check_bytes 'on');
SELECT count(*) FROM ft_bytes_mixed;
ALTER FOREIGN TABLE ft_bytes_mixed OPTIONS (DROP check_bytes);
CREATE TABLE ht_bytes_mixed AS
    SELECT i AS a,
        CASE WHEN i % 7 = 0 THEN NULL WHEN i % 50 = 0 THEN ''
             ELSE (i % 50)::text || repeat('x', (i % 50) * 7919 % 10000) END AS s,
        CASE WHEN i % 3000 = 0 THEN ''
             ELSE (i % 3000)::text || repeat('x', (i % 3000) * 7919 % 300) END AS m
    FROM generate_series(1, 4000) AS i;
SELECT source_same('SELECT s, count(*) FROM ft_bytes_mixed GROUP BY s');
SELECT source_same('SELECT md5(s), count(*) FROM ft_bytes_mixed GROUP BY s');
SET work_mem = '64kB';
SELECT source_same('SELECT m, count(*) FROM ft_bytes_mixed GROUP BY m');
RESET work_mem;
SELECT source_same('SELECT count(DISTINCT s), count(DISTINCT m) FROM ft_bytes_mixed');
SELECT tessera_test_scan_source_asks();
DROP FOREIGN TABLE ft_bytes_mixed;
DROP TABLE ht_bytes_mixed;
-- spec: string-bytes/Two indexes of one value
-- A source's index to a key's number: a row of an index met before costs
-- no hash and no lookup. The source gives an even row of a batch its
-- value as its index and an odd row the value plus 2047, so that of 2047
-- values, each read four times, twice at either parity, every value
-- stands at two indexes, each met twice: the rows of both fall into one
-- group. Under a work_mem of 64kB a key keeps the numbers of the first
-- 2048 indexes, its share of the grouping's memory: the rows of the
-- indexes 0 to 2047 find them there, the last at the array's end, and
-- those of 2048 to 4093, past it, are looked up by their bytes; under the
-- default work_mem every index is kept. A rescan for each outer row, the
-- source keeping its dictionary's identity, drops the numbers with the
-- key dictionary it resets. The heap's groups each time; an assert build
-- checks each number found by an index against the row's bytes.
CREATE FOREIGN TABLE ft_bytes_index (a int4, s text OPTIONS (mod '2047'))
    SERVER scan_server OPTIONS (rows '8192', bytes 'twice', rescan_identities 'keep');
CREATE TABLE ht_bytes_index AS SELECT i AS a, (i % 2047)::text AS s
    FROM generate_series(1, 8192) AS i;
SET work_mem = '64kB';
SELECT source_same('SELECT s, count(*) FROM ft_bytes_index GROUP BY s');
SELECT source_same('SELECT count(DISTINCT s) FROM ft_bytes_index');
RESET work_mem;
SELECT source_same('SELECT s, count(*) FROM ft_bytes_index GROUP BY s');
SELECT source_same('SELECT g, (SELECT count(*) FROM (SELECT s FROM ft_bytes_index
                                                    WHERE a > g * 1000 GROUP BY s) q)
                    FROM generate_series(1, 3) AS g');
SELECT source_same('SELECT g, (SELECT count(DISTINCT s) FROM ft_bytes_index WHERE a > g * 1000)
                    FROM generate_series(1, 3) AS g');
SELECT tessera_test_scan_source_asks();
DROP FOREIGN TABLE ft_bytes_index;
DROP TABLE ht_bytes_index;
-- Large sparse indexes: of a dictionary of two million values, the key
-- keeps no more numbers than its share of the default work_mem, an
-- eighth of hash_mem, which the grouping counts with its groups: the
-- table writes nothing to disk, where an array of every value, 16 MB,
-- would pass hash_mem and freeze it. An odd row's index, a million past
-- its value, lies far past the array and is looked up by its bytes. The
-- heap's groups.
CREATE FUNCTION agg_spilled(query text) RETURNS bool LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, '
                   'COSTS OFF) %s', query) INTO plan;
    RETURN jsonb_path_exists(plan,
        '$.** ? (@."Custom Plan Provider" == "TessAgg")."Disk Usage"');
END $$;
CREATE FOREIGN TABLE ft_bytes_sparse (a int4, s text OPTIONS (mod '1000000'))
    SERVER scan_server OPTIONS (rows '600', bytes 'twice');
CREATE TABLE ht_bytes_sparse AS SELECT i AS a, (i % 1000000)::text AS s
    FROM generate_series(1, 600) AS i;
SELECT source_same('SELECT s, count(*) FROM ft_bytes_sparse GROUP BY s');
SELECT NOT agg_spilled('SELECT s, count(*) FROM ft_bytes_sparse GROUP BY s') AS within_hash_mem;
DROP FOREIGN TABLE ft_bytes_sparse;
DROP TABLE ht_bytes_sparse;
DROP FUNCTION agg_spilled(text);
-- spec: string-bytes/With and without the flag
-- Two batch implementations of one function of a text argument, the
-- bytes of each row, the first registered with TESS_FUNCTION_BYTES and
-- the second without, over the test source's bytes form: in an
-- aggregate's argument and in a filter's value, the first gets the column
-- in the bytes form and the second Datums, a call a batch each, and both
-- give the core's results. Called at the size a call had before its field
-- bytes, as by a consumer built before it, the first reads Datums.
CREATE FUNCTION tessera_test_octets_bytes(text) RETURNS int4
AS :'scan_source', 'tessera_test_octets' LANGUAGE C STRICT IMMUTABLE;
CREATE FUNCTION tessera_test_octets_datums(text) RETURNS int4
AS :'scan_source', 'tessera_test_octets' LANGUAGE C STRICT IMMUTABLE;
CREATE FUNCTION tessera_test_scan_register_octets(regprocedure, regprocedure) RETURNS void
AS :'scan_source', 'tessera_test_scan_register_octets' LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_scan_octets_calls() RETURNS text
AS :'scan_source', 'tessera_test_scan_octets_calls' LANGUAGE C STRICT;
CREATE FUNCTION tessera_test_scan_call_before_bytes(regprocedure, text[]) RETURNS int4[]
AS :'scan_source', 'tessera_test_scan_call_before_bytes' LANGUAGE C STRICT;
SELECT tessera_test_scan_register_octets('tessera_test_octets_bytes(text)',
                                         'tessera_test_octets_datums(text)');
SELECT tessera_test_scan_octets_calls();
SELECT source_same('SELECT sum(tessera_test_octets_bytes(s)), sum(tessera_test_octets_datums(s)),
                           sum(tessera_test_octets_bytes(n)) FROM ft_bytes');
SELECT tessera_test_scan_octets_calls();
EXPLAIN (COSTS OFF) SELECT a FROM ft_bytes
    WHERE tessera_test_octets_bytes(s) > 5000 AND tessera_test_octets_datums(n) = 1;
SELECT source_same('SELECT a FROM ft_bytes
    WHERE tessera_test_octets_bytes(s) > 5000 AND tessera_test_octets_datums(n) = 1');
SELECT tessera_test_scan_octets_calls();
SELECT tessera_test_scan_call_before_bytes('tessera_test_octets_bytes(text)',
                                           ARRAY['', 'a', 'héllo', repeat('x', 3000)]);
SELECT tessera_test_scan_octets_calls();
DROP FUNCTION tessera_test_scan_call_before_bytes(regprocedure, text[]);
DROP FUNCTION tessera_test_scan_octets_calls();
DROP FUNCTION tessera_test_scan_register_octets(regprocedure, regprocedure);
DROP FUNCTION tessera_test_octets_datums(text), tessera_test_octets_bytes(text);
-- The string kernels over the test source's bytes form, whose filters a
-- batch filter computes: each function of a value, of a piece and each
-- predicate, of a column with NULL rows and of one with empty strings
-- and long ones, in a grouping's aggregate, in a filter and in an output
-- column computed for a sort, a predicate's through a CASE, and two
-- columns compared, give the core's rows. The patterns with _, with an
-- escape and of more pieces than the kernels' matcher holds go to the
-- core's function a row, with values made from the bytes. IN lists with
-- a NULL element, under IS UNKNOWN and NOT, take their NULL rows from
-- the form.
SELECT kind, e AS expression,
       source_same(format(CASE kind
           WHEN 'value' THEN 'SELECT a %% 4 AS g, sum(%s) FROM ft_bytes GROUP BY 1'
           WHEN 'piece' THEN 'SELECT a %% 4 AS g, sum(octet_length(%s)) FROM ft_bytes GROUP BY 1'
           ELSE 'SELECT a %% 4 AS g, count(*) FILTER (WHERE %s) FROM ft_bytes GROUP BY 1' END,
         e)) AS in_aggregate,
       source_same(format(CASE kind
           WHEN 'value' THEN 'SELECT a FROM ft_bytes WHERE %s > 1'
           WHEN 'piece' THEN 'SELECT a FROM ft_bytes WHERE %s <> ''1'''
           ELSE 'SELECT a FROM ft_bytes WHERE %s' END, e)) AS in_filter,
       source_same(format(CASE kind
           WHEN 'predicate' THEN 'SELECT * FROM (SELECT a, CASE WHEN %s THEN 1 ELSE 0 END AS x
                                  FROM ft_bytes WHERE a > 10 ORDER BY a) q'
           ELSE 'SELECT * FROM (SELECT a, %s AS x FROM ft_bytes WHERE a > 10 ORDER BY a) q' END,
         e)) AS in_output
FROM (VALUES ('value', 'length(s)'), ('value', 'char_length(n)'), ('value', 'octet_length(s)'),
             ('piece', 'substr(s, 2, 3)'), ('piece', 'substr(n, 2)'), ('piece', 'left(s, 2)'),
             ('piece', 'right(n, 1)'), ('piece', 'ltrim(n)'), ('piece', 'rtrim(s)'),
             ('piece', 'btrim(n)'),
             ('predicate', 'n LIKE ''1%'''), ('predicate', 's NOT LIKE ''%1x%'''),
             ('predicate', 'starts_with(s, ''2'')'), ('predicate', 'n = ''3'''),
             ('predicate', 'n <> ''3'''), ('predicate', 'n IN (''1'', ''12'', '''')'),
             ('predicate', '(n IN (''1'', NULL, '''')) IS UNKNOWN'),
             ('predicate', '(n NOT IN (''1'', NULL, ''12'')) IS UNKNOWN'),
             ('predicate', 'NOT (n IN (''1'', ''12''))'),
             ('predicate', 'n = p'), ('predicate', 's <> n'),
             ('predicate', 'n LIKE ''1_'''), ('predicate', 'n LIKE ''\1%'''),
             ('predicate', 's LIKE ''1\x%'''),
             ('predicate', 's NOT LIKE repeat(''%x'', 33)')) x(kind, e)
ORDER BY kind, e;
-- A count under conditions of strings read as bytes, under OR, NOT and
-- IS UNKNOWN, which ask each value's NULL flag: no Datum is asked.
SELECT tessera_test_scan_source_asks() IS NOT NULL AS asks_reset;
SELECT source_same('SELECT count(*) FROM ft_bytes
    WHERE (s LIKE ''2%'' AND n <> ''3'') OR NOT (n IN (''1'', ''12'')) OR (n = ''5'') IS UNKNOWN');
SELECT tessera_test_scan_source_asks();
-- A grouping by n with the sum of the length of s, a column before it:
-- the read of the columns in their order leaves s out, which the length
-- reads as bytes, and asks no Datum of either.
SELECT source_same('SELECT n, sum(length(s)) FROM ft_bytes GROUP BY n');
SELECT tessera_test_scan_source_asks();
-- A column in the bytes form compared with one the source gives as
-- compressed Datums alone: a row with a compressed value is compared
-- again by the kernels over that row, the value detoasted and the
-- other's made from its bytes.
CREATE FOREIGN TABLE ft_bytes_packed (a int4, s text OPTIONS (mod '50', width '10000'),
    c text OPTIONS (mod '50', width '10000', nulls '3', compress 'on', bytes 'none'))
    SERVER scan_server OPTIONS (rows '1000', bytes 'plain');
CREATE TABLE ht_bytes_packed AS
    SELECT i AS a,
        CASE WHEN i % 50 = 0 THEN ''
             ELSE (i % 50)::text || repeat('x', (i % 50) * 7919 % 10000) END AS s,
        CASE WHEN i % 3 = 0 THEN NULL WHEN i % 50 = 0 THEN ''
             ELSE (i % 50)::text || repeat('x', (i % 50) * 7919 % 10000) END AS c
    FROM generate_series(1, 1000) AS i;
SELECT source_same('SELECT count(*) FILTER (WHERE s = c), count(*) FILTER (WHERE c <> s)
                    FROM ft_bytes_packed');
SELECT source_same('SELECT a FROM ft_bytes_packed WHERE s = c');
SELECT tessera_test_scan_source_asks();
DROP FOREIGN TABLE ft_bytes_packed;
DROP TABLE ht_bytes_packed;
SELECT tessera_test_scan_sources(ARRAY[]::text[]);
DROP FOREIGN TABLE ft_bytes;
DROP TABLE ht_bytes;

DROP FOREIGN TABLE ft_inherited_child;
DROP TABLE ht_inherited_child, ft_inherited, ht_inherited;

DROP VIEW ft_barrier, ht_barrier;
DROP TABLE ft_parted, ht_parted, maybe_null, big_rows;
DROP ROLE regress_source_rls;
DROP FUNCTION counted(int), leaky(int), peek(int), noisy_max(), plan_rows(text, text);
DROP SEQUENCE source_calls;
DROP FUNCTION source_error(text);
DROP TABLE ht_t, ht_two, ht_sorted, ht_join, outer_rows, ht_par, ht_unaware;
DROP FOREIGN TABLE ft_t, ft_two, ft_sorted, ft_join, ft_par, ft_unaware;
DROP SERVER scan_server;
DROP FOREIGN DATA WRAPPER tessera_test_scan;
DROP FUNCTION source_same(text);
DROP FUNCTION tessera_test_scan_source_asks();
DROP FUNCTION tessera_test_scan_source_fault(text);
DROP FUNCTION tessera_test_scan_source_listed();
DROP FUNCTION tessera_test_scan_source_calls();
DROP FUNCTION tessera_test_scan_sources(text[]);
DROP FUNCTION tessera_test_scan_fdw_handler();
DROP FUNCTION explain_neutral(text, text), core_neutral(text);
DROP EXTENSION tessera;
