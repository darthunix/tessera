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

DROP FUNCTION plan_rows(text, text);
DROP FUNCTION source_error(text);
DROP TABLE ht_t, ht_two, ht_sorted, ht_join, outer_rows, ht_par, ht_unaware;
DROP FOREIGN TABLE ft_t, ft_two, ft_sorted, ft_join, ft_par, ft_unaware;
DROP SERVER scan_server;
DROP FOREIGN DATA WRAPPER tessera_test_scan;
DROP FUNCTION source_same(text);
DROP FUNCTION tessera_test_scan_source_fault(text);
DROP FUNCTION tessera_test_scan_source_calls();
DROP FUNCTION tessera_test_scan_sources(text[]);
DROP FUNCTION tessera_test_scan_fdw_handler();
DROP FUNCTION explain_neutral(text, text), core_neutral(text);
DROP EXTENSION tessera;
