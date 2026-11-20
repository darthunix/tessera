CREATE EXTENSION tessera;
-- EXPLAIN in the same words on every core: see the file.
\getenv abs_srcdir PG_ABS_SRCDIR
\set core_neutral :abs_srcdir '/sql/include/core_neutral.sql'
\set ECHO none
\i :core_neutral
\set ECHO all
LOAD 'tessera_nodes';
LOAD 'tessera_kernels';

-- PostgreSQL 19 and 18 estimate a UNION's rows as all its branches' rows,
-- where master sums their distinct groups: there the workers' grouping
-- seems to save nothing and is not chosen. In two statements 18 and 19
-- choose the other way between the core's UNION of sorted branches
-- (Unique over Merge Append) and the node's grouping. The rows are the
-- same: union_parallel_1.out is 19's output, union_parallel_2.out 18's.
-- The core, so that each expected file matches one alone.
SELECT CASE WHEN current_setting('server_version_num')::int >= 200000 THEN 'master'
            ELSE (current_setting('server_version_num')::int / 10000)::text END AS core;

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

CREATE TABLE union_b (a int, b bigint, t text);
INSERT INTO union_b
SELECT CASE WHEN i % 5 = 0 THEN NULL ELSE i END, i * 100, 'b' || i
FROM generate_series(1, 700) AS i;
ANALYZE union_b;
SET max_parallel_workers_per_gather = 0;

-- Under a Gather: the node's partial grouping of every participant's rows
-- of the branches over its parallel Append, TessGather, and its grouping
-- of their groups above; at the default costs over 200000 rows whose keys
-- the planner knows few, read in parallel whatever their size. NULL a
-- value of its own, two columns, a count above, the workers alone; a
-- branch without a partial path leaves the UNION serial, as the core's.
CREATE TABLE union_keys AS
SELECT i % 10 AS b, CASE WHEN i % 11 = 0 THEN NULL ELSE i % 13 END AS c,
       CASE WHEN i % 11 = 0 THEN NULL ELSE 'v' || (i % 13) END AS t
FROM generate_series(1, 200000) AS i;
ANALYZE union_keys;
SET max_parallel_workers_per_gather = 2;
SET min_parallel_table_scan_size = 0;
-- The node's model of a partial scan would keep so small a table serial.
SET tessera.scan_parallel_setup_cost = 0;
SET tessera.scan_worker_page_cost = 0;
SELECT explain_neutral($$SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3$$, 'VERBOSE, COSTS OFF');
SELECT union_same($$SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3$$);
SELECT union_same($$SELECT count(*), sum(x) FROM (SELECT c AS x FROM union_keys UNION SELECT b FROM union_keys WHERE c IS NULL) AS s$$);
-- Text through dictionaries, each participant's own.
SELECT explain_neutral($$SELECT t, b FROM union_keys WHERE b < 5 UNION SELECT t, b FROM union_keys WHERE b > 3$$);
SELECT union_same($$SELECT t, b FROM union_keys WHERE b < 5 UNION SELECT t, b FROM union_keys WHERE b > 3$$);
-- Their tables sent up early, and none written to disk. On 19 and 18 no
-- participant groups (see the start): early is NULL there, and no_disk
-- holds with nothing to show.
CREATE FUNCTION union_partial(query text, name text) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE
    plan jsonb;
BEGIN
    EXECUTE format('EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON, TIMING OFF, SUMMARY OFF, BUFFERS OFF, COSTS OFF) %s', query)
        INTO plan;
    RETURN jsonb_path_query_first(plan,
        format('$[0]."Plan".** ? (@."Custom Plan Provider" == "TessAgg" && @."Partial Mode" == "Partial").%I',
               name)::jsonpath)::text;
END $$;
SET work_mem = '64kB';
SELECT union_same($$SELECT t, b FROM union_keys WHERE b < 5 UNION SELECT t, b FROM union_keys WHERE b > 3$$);
SELECT union_partial($$SELECT t, b FROM union_keys WHERE b < 5 UNION SELECT t, b FROM union_keys WHERE b > 3$$, 'Early Emits')::int > 0 AS early,
       union_partial($$SELECT t, b FROM union_keys WHERE b < 5 UNION SELECT t, b FROM union_keys WHERE b > 3$$, 'Disk Usage') IS NULL AS no_disk;
RESET work_mem;
DROP FUNCTION union_partial;
-- The same as the left side of an EXCEPT.
SELECT explain_neutral($$
(SELECT c FROM union_keys WHERE b < 5 UNION SELECT c FROM union_keys WHERE b > 3) EXCEPT SELECT b FROM union_keys WHERE b = 1$$, 'VERBOSE, COSTS OFF');
SELECT union_same($$(SELECT c FROM union_keys WHERE b < 5 UNION SELECT c FROM union_keys WHERE b > 3)
                   EXCEPT SELECT b FROM union_keys WHERE b = 1$$);
SET parallel_leader_participation = off;
SELECT union_same($$SELECT c, b FROM union_keys WHERE b < 5 UNION SELECT c, b FROM union_keys WHERE b > 3$$);
RESET parallel_leader_participation;
ALTER TABLE union_b SET (parallel_workers = 0);
SELECT explain_neutral($$SELECT c FROM union_keys UNION SELECT a FROM union_b$$, 'VERBOSE, COSTS OFF');
ALTER TABLE union_b RESET (parallel_workers);
SET max_parallel_workers_per_gather = 0;
RESET min_parallel_table_scan_size;
RESET tessera.scan_parallel_setup_cost;
RESET tessera.scan_worker_page_cost;
DROP TABLE union_keys;

DROP TABLE union_b;
DROP FUNCTION union_same(text);
RESET max_parallel_workers_per_gather;
DROP FUNCTION explain_neutral(text, text), core_neutral(text);
DROP EXTENSION tessera;
